#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::Result;
#[cfg(any(not(target_os = "macos"), test))]
use base64::{Engine, engine::general_purpose::STANDARD};
use crossterm::event::MouseEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget, Wrap};
#[cfg(any(not(target_os = "macos"), test))]
use std::io::{self, Write};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Point {
    row: u16,
    column: u16,
}

enum Source {
    Transcript(Vec<Line<'static>>),
    Input(Buffer),
}

struct Range {
    anchor: Point,
    head: Point,
    dragging: bool,
    moved: bool,
    source: Source,
}

#[derive(Default)]
pub(crate) struct TextSelection {
    transcript: Rect,
    input: Rect,
    scroll: u16,
    screen: Option<Buffer>,
    range: Option<Range>,
}

impl TextSelection {
    pub fn clear(&mut self) {
        self.range = None;
    }

    pub fn is_dragging(&self) -> bool {
        self.range.as_ref().is_some_and(|range| range.dragging)
    }

    pub fn selects_transcript(&self) -> bool {
        self.range
            .as_ref()
            .is_some_and(|range| matches!(range.source, Source::Transcript(_)))
    }

    pub fn refresh_transcript(&mut self, lines: Vec<Line<'static>>) {
        if let Some(Range {
            source: Source::Transcript(current),
            ..
        }) = &mut self.range
        {
            *current = lines;
        }
    }

    pub fn update_viewport(&mut self, buffer: &Buffer, transcript: Rect, input: Rect, scroll: u16) {
        if self.transcript.width != transcript.width || self.input != input {
            self.clear();
        }
        self.transcript = transcript;
        self.input = input;
        self.scroll = scroll;
        self.screen = Some(buffer.clone());
    }

    pub fn start(&mut self, mouse: MouseEvent, lines: Vec<Line<'static>>) -> bool {
        self.clear();
        let position = Position::new(mouse.column, mouse.row);
        let source = if self.transcript.contains(position) {
            Source::Transcript(lines)
        } else if self.input.contains(position) {
            let Some(buffer) = self.screen.as_ref() else {
                return false;
            };
            Source::Input(buffer.clone())
        } else {
            return false;
        };
        let anchor = self.point(mouse, &source);
        self.range = Some(Range {
            anchor,
            head: anchor,
            dragging: true,
            moved: false,
            source,
        });
        true
    }

    fn viewport(&self, source: &Source) -> (Rect, u16) {
        match source {
            Source::Transcript(_) => (self.transcript, self.scroll),
            Source::Input(_) => (self.input, 0),
        }
    }

    fn point(&self, mouse: MouseEvent, source: &Source) -> Point {
        let (area, scroll) = self.viewport(source);
        Point {
            row: (mouse.row.clamp(area.y, area.bottom() - 1) - area.y)
                .saturating_add(scroll)
                .min(u16::MAX - 1),
            column: mouse.column.clamp(area.x, area.right() - 1) - area.x,
        }
    }

    pub fn drag(&mut self, mouse: MouseEvent) {
        let Some(range) = self.range.as_ref().filter(|range| range.dragging) else {
            return;
        };
        let point = self.point(mouse, &range.source);
        let range = self.range.as_mut().unwrap();
        range.head = point;
        range.moved |= point != range.anchor;
    }

    pub fn release(&mut self, mouse: MouseEvent) -> Option<String> {
        if !self.is_dragging() {
            return None;
        }
        self.drag(mouse);
        self.range.as_mut().unwrap().dragging = false;
        self.selected_text().filter(|text| !text.trim().is_empty())
    }

    fn bounds(&self) -> Option<(Point, Point)> {
        let range = self.range.as_ref().filter(|range| range.moved)?;
        Some((range.anchor.min(range.head), range.anchor.max(range.head)))
    }

    pub fn highlight(&self, buffer: &mut Buffer, style: Style) {
        let Some((start, end)) = self.bounds() else {
            return;
        };
        let (area, scroll) = self.viewport(&self.range.as_ref().unwrap().source);
        for y in area.y..area.bottom() {
            let row = (y - area.y).saturating_add(scroll);
            let mut x = area.x;
            while x < area.right() {
                let width = UnicodeWidthStr::width(buffer[(x, y)].symbol()).max(1) as u16;
                let first = Point {
                    row,
                    column: x - area.x,
                };
                let last = Point {
                    row,
                    column: first.column.saturating_add(width - 1),
                };
                if first <= end && last >= start {
                    for column in x..x.saturating_add(width).min(area.right()) {
                        buffer[(column, y)].set_style(style);
                    }
                }
                x = x.saturating_add(width);
            }
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.bounds()?;
        let range = self.range.as_ref()?;
        let (buffer, area, first_row) = match &range.source {
            Source::Transcript(lines) => {
                let area = Rect::new(0, 0, self.transcript.width, end.row - start.row + 1);
                let mut buffer = Buffer::empty(area);
                Paragraph::new(lines.clone())
                    .wrap(Wrap { trim: false })
                    .scroll((start.row, 0))
                    .render(area, &mut buffer);
                (buffer, area, start.row)
            }
            Source::Input(buffer) => (buffer.clone(), self.input, 0),
        };
        let mut rows = Vec::new();
        for row in start.row..=end.row {
            let y = area.y + row - first_row;
            let mut text = String::new();
            let mut x = area.x;
            while x < area.right() {
                let symbol = buffer[(x, y)].symbol();
                let width = UnicodeWidthStr::width(symbol).max(1) as u16;
                let first = Point {
                    row,
                    column: x - area.x,
                };
                let last = Point {
                    row,
                    column: first.column.saturating_add(width - 1),
                };
                if first <= end && last >= start {
                    text.push_str(symbol);
                }
                x = x.saturating_add(width);
            }
            rows.push(text.trim_end().to_string());
        }
        Some(rows.join("\n"))
    }
}

pub(crate) async fn copy_to_clipboard(text: String) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;
        use tokio::process::Command;

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut child = Command::new("/usr/bin/pbcopy")
                .env("LC_ALL", "en_US.UTF-8")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .context("starting clipboard copy")?;
            child
                .stdin
                .take()
                .context("opening clipboard input")?
                .write_all(text.as_bytes())
                .await?;
            let status = child.wait().await?;
            anyhow::ensure!(status.success(), "clipboard copy failed: {status}");
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("clipboard copy timed out")?
    }
    #[cfg(not(target_os = "macos"))]
    {
        write_osc52(&mut io::stdout(), &text)?;
        Ok(())
    }
}

#[cfg(any(not(target_os = "macos"), test))]
fn write_osc52(writer: &mut impl Write, text: &str) -> io::Result<()> {
    write!(
        writer,
        "\x1b]52;c;{}\x1b\\",
        STANDARD.encode(text.as_bytes())
    )?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::style::Color;

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn viewport(selection: &mut TextSelection, lines: &[Line<'static>], scroll: u16) -> Buffer {
        let area = Rect::new(2, 1, 12, 3);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 8));
        Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .render(area, &mut buffer);
        selection.update_viewport(&buffer, area, Rect::new(1, 5, 14, 2), scroll);
        buffer
    }

    #[test]
    fn drag_highlights_only_selected_cells_and_copies_in_both_directions() {
        let lines = vec![Line::from("hello world")];
        for reverse in [false, true] {
            let mut selection = TextSelection::default();
            let mut buffer = viewport(&mut selection, &lines, 0);
            let (start, end) = if reverse { (6, 2) } else { (2, 6) };
            assert!(selection.start(
                mouse(MouseEventKind::Down(MouseButton::Left), start, 1),
                lines.clone()
            ));
            selection.drag(mouse(MouseEventKind::Drag(MouseButton::Left), end, 1));
            selection.highlight(&mut buffer, Style::default().bg(Color::Blue));
            for x in 2..=6 {
                assert_eq!(buffer[(x, 1)].bg, Color::Blue);
            }
            assert_eq!(buffer[(7, 1)].bg, Color::Reset);
            assert_eq!(
                selection
                    .release(mouse(MouseEventKind::Up(MouseButton::Left), end, 1))
                    .as_deref(),
                Some("hello")
            );
            assert!(!selection.is_dragging());
            assert_eq!(
                selection.release(mouse(MouseEventKind::Up(MouseButton::Left), end, 1)),
                None
            );
        }
    }

    #[test]
    fn korean_emoji_and_combining_characters_are_copied_as_complete_graphemes() {
        let lines = vec![Line::from("가나👩‍💻e\u{301}!")];
        let mut selection = TextSelection::default();
        let mut buffer = viewport(&mut selection, &lines, 0);
        // Start in the second cell of 가, and end in the combining grapheme.
        selection.start(mouse(MouseEventKind::Down(MouseButton::Left), 3, 1), lines);
        selection.drag(mouse(MouseEventKind::Drag(MouseButton::Left), 8, 1));
        selection.highlight(&mut buffer, Style::default().bg(Color::Blue));
        assert_eq!(buffer[(2, 1)].bg, Color::Blue);
        assert_eq!(buffer[(3, 1)].bg, Color::Blue);
        assert_eq!(
            selection
                .release(mouse(MouseEventKind::Up(MouseButton::Left), 8, 1))
                .as_deref(),
            Some("가나👩‍💻e\u{301}")
        );
    }

    #[test]
    fn scrolling_keeps_document_coordinates_and_copies_rows_outside_the_viewport() {
        let lines: Vec<_> = (0..10).map(|row| Line::from(format!("row{row}"))).collect();
        let mut selection = TextSelection::default();
        viewport(&mut selection, &lines, 0);
        selection.start(
            mouse(MouseEventKind::Down(MouseButton::Left), 2, 2),
            lines.clone(),
        );
        // A large scroll skips rows, so copy must use the document, not cached visible rows.
        let mut buffer = viewport(&mut selection, &lines, 7);
        selection.drag(mouse(MouseEventKind::Drag(MouseButton::Left), 5, 2));
        selection.highlight(&mut buffer, Style::default().bg(Color::Blue));
        assert_eq!(buffer[(2, 1)].bg, Color::Blue);
        assert_eq!(buffer[(5, 2)].bg, Color::Blue);
        assert_eq!(buffer[(6, 2)].bg, Color::Reset);
        assert_eq!(
            selection
                .release(mouse(MouseEventKind::Up(MouseButton::Left), 5, 2))
                .as_deref(),
            Some("row1\nrow2\nrow3\nrow4\nrow5\nrow6\nrow7\nrow8")
        );
    }

    #[test]
    fn wrapped_rows_blank_lines_and_partial_endpoints_match_the_display() {
        let lines = vec![
            Line::from("abcdefghijklmno"),
            Line::default(),
            Line::from("last"),
        ];
        let mut selection = TextSelection::default();
        viewport(&mut selection, &lines, 0);
        selection.start(
            mouse(MouseEventKind::Down(MouseButton::Left), 5, 1),
            lines.clone(),
        );
        viewport(&mut selection, &lines, 1);
        assert_eq!(
            selection
                .release(mouse(MouseEventKind::Up(MouseButton::Left), 3, 3))
                .as_deref(),
            Some("defghijkl\nmno\n\nla")
        );
    }

    #[test]
    fn text_arriving_during_a_drag_is_included_in_the_copy() {
        let mut selection = TextSelection::default();
        let lines = vec![Line::from("first")];
        viewport(&mut selection, &lines, 0);
        selection.start(mouse(MouseEventKind::Down(MouseButton::Left), 2, 1), lines);
        let updated = vec![Line::from("first"), Line::from("second")];
        viewport(&mut selection, &updated, 0);
        selection.refresh_transcript(updated);
        assert_eq!(
            selection
                .release(mouse(MouseEventKind::Up(MouseButton::Left), 7, 2))
                .as_deref(),
            Some("first\nsecond")
        );
    }

    #[test]
    fn visible_input_copy_excludes_borders_and_preserves_wide_characters() {
        let mut selection = TextSelection::default();
        let area = Rect::new(0, 0, 16, 8);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(1, 5, "> 안녕 draft", Style::default());
        selection.update_viewport(&buffer, Rect::new(2, 1, 12, 3), Rect::new(1, 5, 14, 2), 0);
        selection.start(mouse(MouseEventKind::Down(MouseButton::Left), 4, 5), vec![]);
        assert_eq!(
            selection
                .release(mouse(MouseEventKind::Up(MouseButton::Left), 6, 5))
                .as_deref(),
            Some("안녕")
        );
    }

    #[test]
    fn clicks_without_drag_invalid_targets_and_resizes_do_not_copy() {
        let lines = vec![Line::from("hello world")];
        let mut selection = TextSelection::default();
        let mut buffer = viewport(&mut selection, &lines, 0);
        assert!(!selection.start(
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            lines.clone()
        ));
        selection.start(
            mouse(MouseEventKind::Down(MouseButton::Left), 2, 1),
            lines.clone(),
        );
        assert_eq!(
            selection.release(mouse(MouseEventKind::Up(MouseButton::Left), 2, 1)),
            None
        );
        selection.start(mouse(MouseEventKind::Down(MouseButton::Left), 2, 1), lines);
        selection.drag(mouse(MouseEventKind::Drag(MouseButton::Left), 7, 1));
        selection.update_viewport(&buffer, Rect::new(2, 1, 10, 3), Rect::new(1, 5, 14, 2), 0);
        selection.highlight(&mut buffer, Style::default().bg(Color::Blue));
        assert_eq!(buffer[(2, 1)].bg, Color::Reset);
        assert_eq!(selection.selected_text(), None);
    }

    #[test]
    fn osc52_encodes_unicode_and_control_characters_instead_of_emitting_raw_text() {
        let text = "안녕\n\x1b]52;c;malicious\x07";
        let mut bytes = Vec::new();
        write_osc52(&mut bytes, text).unwrap();
        assert_eq!(
            bytes,
            format!("\x1b]52;c;{}\x1b\\", STANDARD.encode(text)).as_bytes()
        );
        assert!(
            !bytes
                .windows("안녕".len())
                .any(|chunk| chunk == "안녕".as_bytes())
        );
    }
}
