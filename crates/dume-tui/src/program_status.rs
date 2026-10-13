use base64::{Engine, engine::general_purpose::STANDARD};
use std::io::{self, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramStatus {
    Idle,
    Working,
    BlockedAuth,
    BlockedQuestion,
    Done,
    Error,
}

impl ProgramStatus {
    fn fields(self) -> (&'static str, &'static str) {
        match self {
            Self::Idle => ("idle", "대기 중"),
            Self::Working => ("working", "작업 중"),
            Self::BlockedAuth => ("blocked:kind=auth", "인증이 필요합니다"),
            Self::BlockedQuestion => ("blocked:kind=question", "응답을 기다리는 중"),
            Self::Done => ("done", "작업 완료"),
            Self::Error => ("error", "오류 발생"),
        }
    }
}

pub struct ProgramStatusReporter {
    enabled: bool,
    previous: Option<ProgramStatus>,
}

impl ProgramStatusReporter {
    pub fn new(is_tty: bool) -> Self {
        Self::with_setting(is_tty, std::env::var("DUME_PROGRAM_STATUS").ok().as_deref())
    }

    fn with_setting(is_tty: bool, setting: Option<&str>) -> Self {
        let enabled = is_tty
            && match setting {
                Some("1") => true,
                Some("0") => false,
                Some("auto") | None => true,
                Some(_) => false,
            };
        Self {
            enabled,
            previous: None,
        }
    }

    pub fn emit(&mut self, writer: &mut impl Write, status: ProgramStatus) -> io::Result<()> {
        if !self.enabled || self.previous == Some(status) {
            return Ok(());
        }
        let (name, message) = status.fields();
        let encoded = STANDARD.encode(message.as_bytes());
        write!(
            writer,
            "\x1b]7501;state={name}:app=dume:msg={encoded}\x1b\\"
        )?;
        self.previous = Some(status);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_fixed_osc_and_suppresses_repeated_state() {
        let mut reporter = ProgramStatusReporter {
            enabled: true,
            previous: None,
        };
        let mut bytes = Vec::new();
        reporter.emit(&mut bytes, ProgramStatus::Working).unwrap();
        reporter.emit(&mut bytes, ProgramStatus::Working).unwrap();
        assert_eq!(
            bytes,
            b"\x1b]7501;state=working:app=dume:msg=7J6R7JeFIOykkQ==\x1b\\"
        );
        reporter.emit(&mut bytes, ProgramStatus::Done).unwrap();
        assert!(
            std::str::from_utf8(&bytes)
                .unwrap()
                .ends_with("\x1b]7501;state=done:app=dume:msg=7J6R7JeFIOyZhOujjA==\x1b\\")
        );
    }

    #[test]
    fn blocked_states_have_exact_wire_syntax() {
        let mut reporter = ProgramStatusReporter {
            enabled: true,
            previous: None,
        };
        let mut bytes = Vec::new();
        reporter
            .emit(&mut bytes, ProgramStatus::BlockedAuth)
            .unwrap();
        assert_eq!(bytes, b"\x1b]7501;state=blocked:kind=auth:app=dume:msg=7J247Kad7J20IO2VhOyalO2VqeuLiOuLpA==\x1b\\");
        bytes.clear();
        reporter
            .emit(&mut bytes, ProgramStatus::BlockedQuestion)
            .unwrap();
        assert_eq!(bytes, b"\x1b]7501;state=blocked:kind=question:app=dume:msg=7J2R64u17J2EIOq4sOuLpOumrOuKlCDspJE=\x1b\\");
    }

    #[test]
    fn setting_is_tty_gated() {
        assert!(ProgramStatusReporter::with_setting(true, Some("1")).enabled);
        assert!(!ProgramStatusReporter::with_setting(false, Some("1")).enabled);
        assert!(!ProgramStatusReporter::with_setting(true, Some("0")).enabled);
        assert!(!ProgramStatusReporter::with_setting(false, Some("auto")).enabled);
        assert!(ProgramStatusReporter::with_setting(true, Some("auto")).enabled);
        assert!(!ProgramStatusReporter::with_setting(true, Some("invalid")).enabled);
    }

    #[test]
    fn disabled_reporter_does_not_write() {
        let mut reporter = ProgramStatusReporter {
            enabled: false,
            previous: None,
        };
        let mut bytes = Vec::new();
        reporter.emit(&mut bytes, ProgramStatus::Error).unwrap();
        assert!(bytes.is_empty());
    }

    #[test]
    fn status_payload_is_fixed_and_contains_no_controls() {
        for state in [
            ProgramStatus::Idle,
            ProgramStatus::Working,
            ProgramStatus::BlockedAuth,
            ProgramStatus::BlockedQuestion,
            ProgramStatus::Done,
            ProgramStatus::Error,
        ] {
            let (_, message) = state.fields();
            assert!(!message.chars().any(char::is_control));
        }
    }
}
