//! Keybinding IDs and defaults.

use std::collections::HashMap;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Logical keybinding action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Interrupt,
    Clear,
    Exit,
    Submit,
    NewLine,
    ModelSelect,
    ModelCycleForward,
    ModelCycleBackward,
    ThinkingCycle,
    ThinkingToggle,
    ToolsExpand,
    MessageCopy,
    FollowUp,
    Dequeue,
    ExternalEditor,
    SessionNew,
    SessionTree,
    SessionFork,
    SessionResume,
    // Editor navigation / editing
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveWordLeft,
    MoveWordRight,
    MoveLineStart,
    MoveLineEnd,
    DeleteBackward,
    DeleteForward,
    DeleteWordBackward,
    DeleteWordForward,
    DeleteToLineStart,
    DeleteToLineEnd,
    DeleteLine,
}

/// Keybinding configuration.
#[derive(Debug, Clone)]
pub struct Keybindings {
    map: HashMap<String, Action>,
}

impl Default for Keybindings {
    fn default() -> Self {
        let mut map = HashMap::new();
        let defaults = [
            ("escape", Action::Interrupt),
            ("ctrl+c", Action::Clear),
            ("ctrl+d", Action::Exit),
            ("enter", Action::Submit),
            ("shift+enter", Action::NewLine),
            ("ctrl+enter", Action::NewLine),
            ("ctrl+j", Action::NewLine),
            ("ctrl+l", Action::ModelSelect),
            ("ctrl+p", Action::ModelCycleForward),
            ("ctrl+shift+p", Action::ModelCycleBackward),
            ("shift+tab", Action::ThinkingCycle),
            ("ctrl+t", Action::ThinkingToggle),
            ("ctrl+o", Action::ToolsExpand),
            ("ctrl+x", Action::MessageCopy),
            ("alt+enter", Action::FollowUp),
            ("alt+up", Action::Dequeue),
            ("ctrl+g", Action::ExternalEditor),
            // Cursor movement
            ("left", Action::MoveLeft),
            ("right", Action::MoveRight),
            ("up", Action::MoveUp),
            ("down", Action::MoveDown),
            ("alt+left", Action::MoveWordLeft),
            ("alt+right", Action::MoveWordRight),
            ("ctrl+left", Action::MoveWordLeft),
            ("ctrl+right", Action::MoveWordRight),
            ("home", Action::MoveLineStart),
            ("end", Action::MoveLineEnd),
            ("ctrl+a", Action::MoveLineStart),
            ("ctrl+e", Action::MoveLineEnd),
            // Deletion
            ("backspace", Action::DeleteBackward),
            ("delete", Action::DeleteForward),
            ("ctrl+h", Action::DeleteBackward),
            ("ctrl+w", Action::DeleteWordBackward),
            ("alt+backspace", Action::DeleteWordBackward),
            ("ctrl+backspace", Action::DeleteWordBackward),
            ("alt+d", Action::DeleteWordForward),
            ("alt+delete", Action::DeleteWordForward),
            ("ctrl+u", Action::DeleteToLineStart),
            ("ctrl+k", Action::DeleteToLineEnd),
            // Cmd (super) bindings — macOS: cmd+backspace / cmd+delete
            ("super+backspace", Action::DeleteToLineStart),
            ("super+delete", Action::DeleteLine),
            ("ctrl+shift+backspace", Action::DeleteLine),
            ("ctrl+shift+u", Action::DeleteLine),
        ];
        for (k, a) in defaults {
            map.insert(k.to_string(), a);
        }
        Self { map }
    }
}

impl Keybindings {
    /// Load overrides from JSON (maps binding string → action id).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut kb = Self::default();
        if !path.exists() {
            return Ok(kb);
        }
        let raw = std::fs::read_to_string(path)?;
        if raw.trim().is_empty() {
            return Ok(kb);
        }
        // Support both { "app.interrupt": "escape" } and reverse maps.
        let value: serde_json::Value = serde_json::from_str(&raw)?;
        if let Some(obj) = value.as_object() {
            for (k, v) in obj {
                let Some(s) = v.as_str() else { continue };
                if let Some(action) = action_from_id(k) {
                    kb.map.insert(normalize_key(s), action);
                } else if let Some(action) = action_from_id(s) {
                    kb.map.insert(normalize_key(k), action);
                }
            }
        }
        Ok(kb)
    }

    /// Resolve a key event to an action.
    pub fn resolve(&self, key: KeyEvent) -> Option<Action> {
        // Prefer Shift+Enter / Ctrl+Enter as newline even when the terminal
        // reports odd modifier combinations.
        if matches!(key.code, KeyCode::Enter)
            && (key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Some(Action::NewLine);
        }
        // Some terminals emit `\n` for Shift+Enter.
        if matches!(key.code, KeyCode::Char('\n')) {
            return Some(Action::NewLine);
        }
        let s = key_to_string(key)?;
        self.map.get(&s).copied()
    }
}

fn action_from_id(id: &str) -> Option<Action> {
    Some(match id {
        "app.interrupt" | "interrupt" => Action::Interrupt,
        "app.clear" | "clear" => Action::Clear,
        "app.exit" | "exit" => Action::Exit,
        "tui.input.submit" | "submit" => Action::Submit,
        "tui.input.newLine" | "newLine" | "newline" => Action::NewLine,
        "app.model.select" | "modelSelect" => Action::ModelSelect,
        "app.model.cycleForward" | "modelCycleForward" => Action::ModelCycleForward,
        "app.model.cycleBackward" | "modelCycleBackward" => Action::ModelCycleBackward,
        "app.thinking.cycle" | "thinkingCycle" => Action::ThinkingCycle,
        "app.thinking.toggle" | "thinkingToggle" => Action::ThinkingToggle,
        "app.tools.expand" | "toolsExpand" => Action::ToolsExpand,
        "app.message.copy" | "messageCopy" => Action::MessageCopy,
        "app.message.followUp" | "followUp" => Action::FollowUp,
        "app.message.dequeue" | "dequeue" => Action::Dequeue,
        "app.editor.external" | "externalEditor" => Action::ExternalEditor,
        "app.session.new" | "sessionNew" => Action::SessionNew,
        "app.session.tree" | "sessionTree" => Action::SessionTree,
        "app.session.fork" | "sessionFork" => Action::SessionFork,
        "app.session.resume" | "sessionResume" => Action::SessionResume,
        "tui.input.moveLeft" | "moveLeft" => Action::MoveLeft,
        "tui.input.moveRight" | "moveRight" => Action::MoveRight,
        "tui.input.moveUp" | "moveUp" => Action::MoveUp,
        "tui.input.moveDown" | "moveDown" => Action::MoveDown,
        "tui.input.moveWordLeft" | "moveWordLeft" => Action::MoveWordLeft,
        "tui.input.moveWordRight" | "moveWordRight" => Action::MoveWordRight,
        "tui.input.moveLineStart" | "moveLineStart" => Action::MoveLineStart,
        "tui.input.moveLineEnd" | "moveLineEnd" => Action::MoveLineEnd,
        "tui.input.deleteBackward" | "deleteBackward" => Action::DeleteBackward,
        "tui.input.deleteForward" | "deleteForward" => Action::DeleteForward,
        "tui.input.deleteWordBackward" | "deleteWordBackward" => Action::DeleteWordBackward,
        "tui.input.deleteWordForward" | "deleteWordForward" => Action::DeleteWordForward,
        "tui.input.deleteToLineStart" | "deleteToLineStart" => Action::DeleteToLineStart,
        "tui.input.deleteToLineEnd" | "deleteToLineEnd" => Action::DeleteToLineEnd,
        "tui.input.deleteLine" | "deleteLine" => Action::DeleteLine,
        _ => return None,
    })
}

fn normalize_key(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .replace(' ', "")
        .replace("cmd+", "super+")
        .replace("command+", "super+")
        .replace("option+", "alt+")
        .replace("opt+", "alt+")
}

/// Clipboard text for the composer. `\r\n` and `\r` become `\n`, and one trailing
/// newline is dropped so a pasted paragraph does not gain a blank line.
pub fn normalize_pasted_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Paste text for a single-line field (API key, picker query).
pub fn pasted_single_line(text: &str) -> String {
    normalize_pasted_text(text).replace('\n', "")
}

/// Join a burst of ordinary keypresses when a newline is followed by more
/// pasted text. A line typed and submitted on its own stays `None` so Enter
/// still sends.
pub fn multiline_paste_text(keys: &[KeyEvent]) -> Option<String> {
    if keys.len() < 2 || !keys.iter().all(is_unbracketed_paste_key) {
        return None;
    }
    let mut buf = String::new();
    let mut saw_newline = false;
    let mut text_after_newline = false;
    for key in keys {
        let ch = paste_key_char(key);
        if saw_newline && ch != '\n' {
            text_after_newline = true;
        }
        if ch == '\n' {
            saw_newline = true;
        }
        buf.push(ch);
    }
    if !text_after_newline {
        return None;
    }
    let text = normalize_pasted_text(&buf);
    (!text.is_empty()).then_some(text)
}

/// True when this burst ends on a newline and more pasted lines may still be
/// arriving. Callers can wait briefly before treating that newline as Enter.
pub fn paste_burst_pending(keys: &[KeyEvent]) -> bool {
    if keys.len() < 2 || !keys.iter().all(is_unbracketed_paste_key) {
        return false;
    }
    matches!(keys.last().map(paste_key_char), Some('\n'))
        && keys.iter().any(|key| paste_key_char(key) != '\n')
}

fn is_unbracketed_paste_key(key: &KeyEvent) -> bool {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return false;
    }
    match key.code {
        KeyCode::Enter | KeyCode::Tab => true,
        KeyCode::Char(c) => !c.is_control() || c == '\n' || c == '\r' || c == '\t',
        _ => false,
    }
}

fn paste_key_char(key: &KeyEvent) -> char {
    match key.code {
        KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r') => '\n',
        KeyCode::Tab | KeyCode::Char('\t') => '\t',
        KeyCode::Char(c) => c,
        _ => '\0',
    }
}

fn key_to_string(key: KeyEvent) -> Option<String> {
    let mut parts = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt");
    }
    if key.modifiers.contains(KeyModifiers::SUPER) {
        parts.push("super");
    }
    let code = match key.code {
        KeyCode::Char(c) => {
            // Ctrl+char often arrives as a control code; crossterm usually gives Char.
            c.to_lowercase().to_string()
        }
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "escape".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "shift+tab".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        _ => return None,
    };
    // BackTab already includes shift semantics for our map.
    if key.code == KeyCode::BackTab {
        return Some("shift+tab".into());
    }
    parts.push(code.as_str());
    Some(parts.join("+"))
}

/// Human-readable hotkey help lines.
pub fn hotkey_help() -> Vec<(&'static str, &'static str)> {
    vec![
        ("escape", "Interrupt / abort and clear message queue"),
        ("ctrl+c", "Clear input (twice to quit)"),
        ("ctrl+d", "Exit when input empty"),
        ("enter", "Send message (queues while agent is busy)"),
        ("shift+enter / ctrl+j", "New line"),
        ("alt/ctrl+left/right", "Jump by word"),
        ("ctrl+a / ctrl+e", "Line start / end"),
        ("ctrl+u / ctrl+k", "Delete to line start / end"),
        ("ctrl+w / alt+backspace", "Delete word"),
        ("cmd/super+backspace", "Delete to line start"),
        ("cmd/super+delete", "Delete entire line"),
        ("ctrl+l", "Select model"),
        ("ctrl+p / ctrl+shift+p", "Cycle models"),
        ("shift+tab", "Cycle reasoning effort"),
        ("ctrl+t", "Toggle thinking visibility"),
        ("ctrl+o", "Expand/collapse tool output & reasoning"),
        ("↑↓", "Previous/next command; lists"),
        ("ctrl+x", "Copy last assistant message"),
        ("alt+enter", "Queue message while busy"),
        ("alt+up", "Remove last queued message"),
        ("ctrl+g", "External editor"),
        ("/", "Slash commands"),
        ("!command", "Run shell locally (not sent to the model)"),
    ]
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn chars(text: &str) -> Vec<KeyEvent> {
        text.chars()
            .map(|c| {
                if c == '\n' {
                    key(KeyCode::Enter)
                } else {
                    key(KeyCode::Char(c))
                }
            })
            .collect()
    }

    #[test]
    fn normalize_pasted_paragraph_is_one_block() {
        assert_eq!(
            normalize_pasted_text("one\r\ntwo\r\nthree\r\n"),
            "one\ntwo\nthree"
        );
        assert_eq!(
            normalize_pasted_text("one\ntwo\n\nthree\n"),
            "one\ntwo\n\nthree"
        );
        assert_eq!(pasted_single_line("sk-\r\nsecret\n"), "sk-secret");
    }

    #[test]
    fn multiline_paste_keeps_internal_newlines() {
        assert_eq!(
            multiline_paste_text(&chars("one\ntwo\nthree\n")).as_deref(),
            Some("one\ntwo\nthree")
        );
        assert_eq!(
            multiline_paste_text(&chars("one\ntwo")).as_deref(),
            Some("one\ntwo")
        );
    }

    #[test]
    fn single_line_enter_is_not_a_paste() {
        let keys = chars("hello\n");
        assert!(multiline_paste_text(&keys).is_none());
        assert!(paste_burst_pending(&keys));
    }

    #[test]
    fn modified_keys_are_not_a_paste() {
        let mut keys = chars("hello\nworld");
        keys.push(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(multiline_paste_text(&keys).is_none());
        assert!(!paste_burst_pending(&keys));
    }
}
