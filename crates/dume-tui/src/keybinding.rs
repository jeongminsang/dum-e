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
    DeleteChar,
    CursorLeft,
    CursorRight,
    CursorHome,
    CursorEnd,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    Clear,
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
        let mut assigned = HashMap::new();
        for (key, name) in config.bindings {
            let stroke =
                normalize_key(&key).with_context(|| format!("invalid key binding {key:?}"))?;
            let action = parse_action(&name).with_context(|| format!("invalid action {name:?}"))?;
            if let Some(previous) = assigned.insert(stroke.clone(), name.clone()) {
                bail!("key binding conflict: {key:?} maps to both {previous:?} and {name:?}");
            }
            bindings.insert(stroke, action);
        }
        Ok(Self { bindings })
    }

    pub fn action(&self, key: KeyEvent) -> Action {
        self.bindings
            .get(&format_key(key))
            .cloned()
            .unwrap_or_else(|| match key.code {
                KeyCode::Char(c) => Action::InsertChar(c),
                _ => Action::None,
            })
    }
}

fn defaults() -> HashMap<String, Action> {
    [
        ("Ctrl+c", Action::Quit),
        ("Ctrl+d", Action::Quit),
        ("Ctrl+l", Action::Clear),
        ("Enter", Action::SubmitInput),
        ("Ctrl+q", Action::SubmitFollowUp),
        ("Tab", Action::Tab),
        ("Esc", Action::Escape),
        ("Backspace", Action::DeleteChar),
        ("Left", Action::CursorLeft),
        ("Right", Action::CursorRight),
        ("Home", Action::CursorHome),
        ("End", Action::CursorEnd),
        ("Up", Action::ScrollUp),
        ("Down", Action::ScrollDown),
        ("PageUp", Action::PageUp),
        ("PageDown", Action::PageDown),
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
        "Clear" => Action::Clear,
        "Tab" => Action::Tab,
        "Escape" => Action::Escape,
        "DeleteChar" => Action::DeleteChar,
        "CursorLeft" => Action::CursorLeft,
        "CursorRight" => Action::CursorRight,
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
    parts.push(code);
    parts.join("+")
}

pub fn handle_key_event(key: KeyEvent) -> Action {
    KeyBindings {
        bindings: defaults(),
    }
    .action(key)
}
