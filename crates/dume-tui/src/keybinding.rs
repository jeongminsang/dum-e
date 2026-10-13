use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    SubmitInput,
    SubmitFollowUp,
    InsertChar(char),
    InsertNewline,
    DeleteChar,
    DeleteForward,
    DeleteToBeginning,
    DeleteToEnd,
    DeleteWordBackward,
    DeleteWordForward,
    CursorLeft,
    CursorRight,
    CursorWordLeft,
    CursorWordRight,
    CursorHome,
    CursorEnd,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    Clear,
    OpenRoutingSettings,
    Tab,
    Escape,
    None,
}

#[derive(Debug, Clone)]
pub struct KeyBindings {
    bindings: HashMap<String, Action>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    bindings: HashMap<String, String>,
}

impl KeyBindings {
    pub fn load_default() -> Result<Self> {
        let mut bindings = defaults();
        let Some(home) = std::env::var_os("HOME") else {
            return Ok(Self { bindings });
        };
        let path = PathBuf::from(home).join(".dume/agent/keybindings.json");
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self { bindings });
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let config: Config = serde_json::from_str(&content)
            .with_context(|| format!("parsing {}", path.display()))?;
        apply_config(&mut bindings, config)?;
        Ok(Self { bindings })
    }

    pub fn action(&self, key: KeyEvent) -> Action {
        self.bindings
            .get(&format_key(key))
            .cloned()
            .unwrap_or_else(|| match key.code {
                KeyCode::Char(c)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    Action::InsertChar(c)
                }
                _ => Action::None,
            })
    }
}

fn apply_config(bindings: &mut HashMap<String, Action>, config: Config) -> Result<()> {
    let mut assigned = HashMap::new();
    for (key, name) in config.bindings {
        let stroke = normalize_key(&key).with_context(|| format!("invalid key binding {key:?}"))?;
        let action = parse_action(&name).with_context(|| format!("invalid action {name:?}"))?;
        if let Some(previous) = assigned.insert(stroke.clone(), name.clone()) {
            bail!("key binding conflict: {key:?} maps to both {previous:?} and {name:?}");
        }
        bindings.insert(stroke, action);
    }
    Ok(())
}

fn defaults() -> HashMap<String, Action> {
    [
        ("Ctrl+c", Action::Quit),
        ("Ctrl+d", Action::Quit),
        ("Ctrl+l", Action::Clear),
        ("Ctrl+r", Action::OpenRoutingSettings),
        ("Enter", Action::SubmitInput),
        ("Ctrl+q", Action::SubmitFollowUp),
        ("Tab", Action::Tab),
        ("Esc", Action::Escape),
        ("Backspace", Action::DeleteChar),
        ("Ctrl+h", Action::DeleteChar),
        ("Delete", Action::DeleteForward),
        ("Left", Action::CursorLeft),
        ("Ctrl+b", Action::CursorLeft),
        ("Right", Action::CursorRight),
        ("Ctrl+f", Action::CursorRight),
        ("Home", Action::CursorHome),
        ("Ctrl+a", Action::CursorHome),
        ("End", Action::CursorEnd),
        ("Ctrl+e", Action::CursorEnd),
        ("Up", Action::ScrollUp),
        ("Down", Action::ScrollDown),
        ("PageUp", Action::PageUp),
        ("PageDown", Action::PageDown),
        ("Shift+Enter", Action::InsertNewline),
        ("Alt+Enter", Action::InsertNewline),
        ("Ctrl+Enter", Action::InsertNewline),
        ("Ctrl+j", Action::InsertNewline),
        ("Ctrl+u", Action::DeleteToBeginning),
        ("Ctrl+k", Action::DeleteToEnd),
        ("Ctrl+w", Action::DeleteWordBackward),
        ("Alt+Backspace", Action::DeleteWordBackward),
        ("Alt+d", Action::DeleteWordForward),
        ("Alt+Delete", Action::DeleteWordForward),
        ("Alt+b", Action::CursorWordLeft),
        ("Alt+Left", Action::CursorWordLeft),
        ("Alt+f", Action::CursorWordRight),
        ("Alt+Right", Action::CursorWordRight),
    ]
    .into_iter()
    .map(|(key, action)| (key.to_ascii_lowercase(), action))
    .collect()
}

fn parse_action(name: &str) -> Result<Action> {
    Ok(match name {
        "Quit" => Action::Quit,
        "SubmitInput" => Action::SubmitInput,
        "SubmitFollowUp" => Action::SubmitFollowUp,
        "InsertNewline" => Action::InsertNewline,
        "Clear" => Action::Clear,
        "OpenRoutingSettings" => Action::OpenRoutingSettings,
        "Tab" => Action::Tab,
        "Escape" => Action::Escape,
        "DeleteChar" => Action::DeleteChar,
        "DeleteForward" => Action::DeleteForward,
        "DeleteToBeginning" => Action::DeleteToBeginning,
        "DeleteToEnd" => Action::DeleteToEnd,
        "DeleteWordBackward" => Action::DeleteWordBackward,
        "DeleteWordForward" => Action::DeleteWordForward,
        "CursorLeft" => Action::CursorLeft,
        "CursorRight" => Action::CursorRight,
        "CursorWordLeft" => Action::CursorWordLeft,
        "CursorWordRight" => Action::CursorWordRight,
        "CursorHome" => Action::CursorHome,
        "CursorEnd" => Action::CursorEnd,
        "ScrollUp" => Action::ScrollUp,
        "ScrollDown" => Action::ScrollDown,
        "PageUp" => Action::PageUp,
        "PageDown" => Action::PageDown,
        _ => bail!("unsupported action"),
    })
}

fn normalize_key(key: &str) -> Result<String> {
    let mut parts = key.split('+').collect::<Vec<_>>();
    let code = parts.pop().context("missing key")?;
    if parts.iter().any(|part| {
        !part.eq_ignore_ascii_case("ctrl")
            && !part.eq_ignore_ascii_case("alt")
            && !part.eq_ignore_ascii_case("shift")
    }) {
        bail!("unsupported modifier");
    }
    let mut mods = parts
        .iter()
        .map(|p| p.to_ascii_lowercase())
        .collect::<Vec<_>>();
    mods.sort();
    let code = code.to_ascii_lowercase();
    if code.is_empty() || mods.windows(2).any(|w| w[0] == w[1]) {
        bail!("invalid key");
    }
    mods.push(code);
    Ok(mods.join("+"))
}

fn format_key(key: KeyEvent) -> String {
    let mut parts = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl".to_string());
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt".to_string());
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift".to_string());
    }
    let code = match key.code {
        KeyCode::Char(c) => c.to_string().to_ascii_lowercase(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        _ => return String::new(),
    };
    parts.sort();
    parts.push(code);
    parts.join("+")
}

pub fn handle_key_event(key: KeyEvent) -> Action {
    KeyBindings {
        bindings: defaults(),
    }
    .action(key)
}

#[cfg(test)]
mod routing_binding_tests {
    use super::*;

    #[test]
    fn forward_delete_default_and_remap_are_action_dispatched() {
        let default_bindings = KeyBindings {
            bindings: defaults(),
        };
        assert_eq!(
            default_bindings.action(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
            Action::DeleteForward
        );
        let config: Config =
            serde_json::from_str(r#"{"bindings":{"Ctrl+Alt+x":"DeleteForward"}}"#).unwrap();
        let mut bindings = defaults();
        apply_config(&mut bindings, config).unwrap();
        assert_eq!(
            KeyBindings { bindings }.action(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
            Action::DeleteForward
        );
    }

    #[test]
    fn routing_settings_binding_is_remappable_without_ctrl_r_special_casing() {
        assert_eq!(
            KeyBindings {
                bindings: defaults()
            }
            .action(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Action::OpenRoutingSettings,
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keybindings.json");
        fs::write(
            &path,
            r#"{"bindings":{"Ctrl+x":"OpenRoutingSettings","Ctrl+r":"Clear"}}"#,
        )
        .unwrap();
        let config: Config = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let mut bindings = defaults();
        apply_config(&mut bindings, config).unwrap();
        let bindings = KeyBindings { bindings };
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            Action::OpenRoutingSettings
        );
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Action::Clear
        );
    }

    #[test]
    fn ctrl_alt_remaps_match_actual_events_and_defaults_can_be_overridden() {
        let config: Config = serde_json::from_str(
            r#"{"bindings":{"Ctrl+Alt+x":"OpenRoutingSettings","Ctrl+r":"Clear"}}"#,
        )
        .unwrap();
        let mut bindings = defaults();
        apply_config(&mut bindings, config).unwrap();
        let bindings = KeyBindings { bindings };
        assert_eq!(
            bindings.action(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
            Action::OpenRoutingSettings,
        );
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Action::Clear,
        );
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            Action::Clear,
        );
        assert_eq!(
            normalize_key("Ctrl+Alt+x").unwrap(),
            format_key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ))
        );
        let duplicate: Config = serde_json::from_str(
            r#"{"bindings":{"Ctrl+Alt+x":"Clear","Alt+Ctrl+x":"OpenRoutingSettings"}}"#,
        )
        .unwrap();
        let mut defaults_copy = defaults();
        assert!(apply_config(&mut defaults_copy, duplicate).is_err());
    }

    #[test]
    fn shift_enter_and_newline_combinations_dispatch_insert_newline() {
        let bindings = KeyBindings {
            bindings: defaults(),
        };
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            Action::InsertNewline
        );
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            Action::InsertNewline
        );
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
            Action::InsertNewline
        );
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            Action::InsertNewline
        );
    }

    #[test]
    fn command_backspace_alias_ctrl_u_and_editing_keys_dispatch_proper_actions() {
        let bindings = KeyBindings {
            bindings: defaults(),
        };
        // Ctrl+u (Cmd+Backspace on macOS terminal) must delete to beginning, NEVER insert 'u'
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            Action::DeleteToBeginning
        );
        // Ctrl+w / Alt+Backspace must delete word backward
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            Action::DeleteWordBackward
        );
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)),
            Action::DeleteWordBackward
        );
        // Ctrl+k must delete to end
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            Action::DeleteToEnd
        );
        // Unmapped Ctrl or Alt key must NEVER fall back to inserting characters
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL)),
            Action::None
        );
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)),
            Action::None
        );
        // Plain char or Shift+char inserts character
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Action::InsertChar('a')
        );
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            Action::InsertChar('A')
        );
    }
}
