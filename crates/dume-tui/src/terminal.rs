use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{SynchronizedUpdate, execute};
use futures_util::{Stream, StreamExt};
use ratatui::{Frame, Terminal, backend::Backend};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

type PanicHook = dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static;

struct TerminalModes {
    active: AtomicBool,
    keyboard_pushed: AtomicBool,
}

impl TerminalModes {
    fn restore(&self) {
        if !self.active.swap(false, Ordering::SeqCst) {
            return;
        }
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), EndSynchronizedUpdate);
        if self.keyboard_pushed.load(Ordering::SeqCst) {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableFocusChange,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

pub(crate) struct TerminalSession {
    modes: Arc<TerminalModes>,
    previous_hook: Arc<PanicHook>,
}

impl TerminalSession {
    pub(crate) fn enter() -> io::Result<Self> {
        let session = Self {
            modes: Arc::new(TerminalModes {
                active: AtomicBool::new(false),
                keyboard_pushed: AtomicBool::new(false),
            }),
            previous_hook: Arc::from(std::panic::take_hook()),
        };
        let modes = session.modes.clone();
        let previous_hook = session.previous_hook.clone();
        std::panic::set_hook(Box::new(move |info| {
            modes.restore();
            previous_hook(info);
        }));
        enable_raw_mode()?;
        session.modes.active.store(true, Ordering::SeqCst);
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            EnableFocusChange
        )?;
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        session.modes.keyboard_pushed.store(true, Ordering::SeqCst);
        Ok(session)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.modes.restore();
        if !std::thread::panicking() {
            let previous_hook = self.previous_hook.clone();
            std::panic::set_hook(Box::new(move |info| previous_hook(info)));
        }
    }
}

pub(crate) struct ScreenRecovery {
    last_frame: SystemTime,
    repaint: bool,
}

impl ScreenRecovery {
    pub(crate) fn new() -> Self {
        Self {
            last_frame: SystemTime::now(),
            repaint: false,
        }
    }

    pub(crate) fn request_repaint(&mut self) {
        self.repaint = true;
    }

    fn observe_frame(&mut self, now: SystemTime) {
        // Instant does not reliably count macOS sleep; a clock correction only causes a repaint.
        if now
            .duration_since(self.last_frame)
            .is_ok_and(|elapsed| elapsed >= Duration::from_secs(2))
        {
            self.request_repaint();
        }
        self.last_frame = now;
    }

    fn prepare<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> io::Result<()> {
        if self.repaint {
            terminal.autoresize()?;
            terminal.clear()?;
            self.repaint = false;
        }
        Ok(())
    }

    pub(crate) fn draw<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        render: impl FnOnce(&mut Frame),
    ) -> io::Result<()> {
        self.observe_frame(SystemTime::now());
        io::stdout().sync_update(|_| {
            self.prepare(terminal)?;
            terminal.draw(render)?;
            Ok(())
        })?
    }
}

pub(crate) async fn read_events(
    mut reader: impl Stream<Item = io::Result<Event>> + Unpin,
    tx: mpsc::UnboundedSender<io::Result<Event>>,
    cancellation: CancellationToken,
) {
    loop {
        let event = tokio::select! {
            _ = cancellation.cancelled() => break,
            next = reader.next() => match next {
                Some(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
                Some(event) => event,
                None => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "terminal input closed")),
            },
        };
        let failed = event.is_err();
        if tx.send(event).is_err() || failed {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::widgets::Paragraph;

    fn render(terminal: &mut Terminal<TestBackend>) {
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("draft 안녕"), frame.area()))
            .unwrap();
    }

    #[test]
    fn recovery_overwrites_stale_cells_without_a_resize() {
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        render(&mut terminal);
        let mut stale = ratatui::buffer::Cell::default();
        stale.set_symbol("X");
        terminal
            .backend_mut()
            .draw([(0, 0, &stale), (19, 2, &stale)].into_iter())
            .unwrap();
        render(&mut terminal);
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "X");
        let mut recovery = ScreenRecovery::new();
        recovery.request_repaint();
        recovery.prepare(&mut terminal).unwrap();
        render(&mut terminal);
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "d");
        assert_eq!(terminal.backend().buffer()[(19, 2)].symbol(), " ");
        assert!(!recovery.repaint);
    }

    #[test]
    fn long_wall_clock_gap_recovers_once_without_focus() {
        let mut recovery = ScreenRecovery::new();
        let start = recovery.last_frame;
        recovery.observe_frame(start + Duration::from_millis(80));
        assert!(!recovery.repaint);
        recovery.observe_frame(start + Duration::from_secs(60));
        assert!(recovery.repaint);
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        recovery.prepare(&mut terminal).unwrap();
        recovery.observe_frame(start + Duration::from_secs(60) + Duration::from_millis(80));
        assert!(!recovery.repaint);
    }

    #[test]
    fn backwards_clock_adjustment_does_not_disable_later_recovery() {
        let mut recovery = ScreenRecovery::new();
        let earlier = recovery.last_frame - Duration::from_secs(60);
        recovery.observe_frame(earlier);
        assert!(!recovery.repaint);
        recovery.observe_frame(earlier + Duration::from_secs(3));
        assert!(recovery.repaint);
    }

    #[tokio::test]
    async fn interrupted_input_keeps_keys_and_reports_eof() {
        let key = Event::Key(KeyEvent::new(KeyCode::Char('한'), KeyModifiers::NONE));
        let input = futures_util::stream::iter([
            Err(io::Error::from(io::ErrorKind::Interrupted)),
            Ok(Event::FocusGained),
            Ok(key.clone()),
        ]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        read_events(input, tx, CancellationToken::new()).await;
        assert_eq!(rx.recv().await.unwrap().unwrap(), Event::FocusGained);
        assert_eq!(rx.recv().await.unwrap().unwrap(), key);
        assert_eq!(
            rx.recv().await.unwrap().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn fatal_input_error_is_reported_once_then_reader_stops() {
        let input = futures_util::stream::iter([
            Err(io::Error::from(io::ErrorKind::BrokenPipe)),
            Ok(Event::FocusGained),
        ]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        read_events(input, tx, CancellationToken::new()).await;
        assert_eq!(
            rx.recv().await.unwrap().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn cancellation_stops_a_pending_reader() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(read_events(
            futures_util::stream::pending(),
            tx,
            cancellation.clone(),
        ));
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(rx.recv().await.is_none());
    }
}
