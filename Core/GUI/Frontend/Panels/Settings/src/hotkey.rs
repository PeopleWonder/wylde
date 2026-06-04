//! Push-to-talk hotkey capture — the pure state machine + formatting
//! behind the Settings → Voice "Push-to-talk hotkey" widget.
//!
//! Voice Slice 6 shipped a 5-preset *cycle* (click to advance through
//! `Ctrl+Space → Alt+Space → Right Ctrl → F8 → CapsLock`).  This module
//! replaces that with live capture: the row is a focusable pill; clicking
//! it arms capture, and the next real key chord becomes the binding.
//!
//! Everything here is platform-pure and unit-testable without a gpui
//! `App` — the panel feeds each `KeyDownEvent`'s `Keystroke` to
//! [`resolve_capture`] and acts on the [`CaptureOutcome`].  The display
//! formatting matches the persisted preset style (`"Ctrl+Alt+V"`, `"F8"`,
//! `"Ctrl+Space"`) so a captured value round-trips with already-stored
//! configs and the `voice.set_config` field stays byte-compatible.

use gpui::{Keystroke, Modifiers};

/// Keys we refuse to bind as a push-to-talk chord, regardless of
/// modifiers.  Enter and Tab drive submission + focus traversal across
/// the whole UI; shadowing them with PTT would be a foot-gun.  Escape is
/// the capture-cancel key (handled before this list) so it can never
/// reach here as a candidate.  Kept deliberately minimal per the slice
/// brief ("Enter, Tab, etc. — minimal blocklist").
pub const RESERVED_KEYS: &[&str] = &["enter", "tab"];

/// User-facing note shown when a reserved key is pressed during capture.
pub const RESERVED_NOTE: &str = "Enter and Tab are reserved — try another key.";

/// The prompt shown in the pill while capture is armed.
pub const CAPTURE_PROMPT: &str = "Press any key combination…";

/// True when `key` names a modifier on its own.  gpui normally reports a
/// lone modifier press as a `ModifiersChanged` event (which never reaches
/// our key-down handler), but a platform that routes it through key-down
/// names the key like this — and `Keystroke::parse("ctrl")` yields it too.
/// Either way a modifier alone must never commit a chord.
fn is_modifier_key(key: &str) -> bool {
    matches!(
        key,
        "control"
            | "ctrl"
            | "alt"
            | "option"
            | "shift"
            | "platform"
            | "cmd"
            | "super"
            | "win"
            | "function"
            | "fn"
    )
}

/// Outcome of feeding one key-down into the armed capture widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureOutcome {
    /// Escape — leave the value unchanged and exit capture.
    Cancelled,
    /// A modifier on its own (or an empty key) — stay armed, keep waiting
    /// for the chord's terminal non-modifier key.
    Pending,
    /// A reserved key (Enter/Tab) — reject, stay armed, surface why.
    Reserved(&'static str),
    /// A committed chord, formatted in the persisted preset style.
    Committed(String),
}

/// Map one keystroke (while capture is armed) to its outcome.  Pure: the
/// panel handler turns the result into focus/persist/notify side-effects.
pub fn resolve_capture(ks: &Keystroke) -> CaptureOutcome {
    let key = ks.key.as_str();
    if key == "escape" {
        return CaptureOutcome::Cancelled;
    }
    if key.is_empty() || is_modifier_key(key) {
        return CaptureOutcome::Pending;
    }
    if RESERVED_KEYS.contains(&key) {
        return CaptureOutcome::Reserved(RESERVED_NOTE);
    }
    CaptureOutcome::Committed(format_chord(&ks.modifiers, key))
}

/// Render a chord in the `"Ctrl+Alt+V"` / `"F8"` / `"Ctrl+Space"` style the
/// presets used.  Modifier order is fixed (Ctrl, Alt, Shift, Win) so the
/// same chord always formats identically.  The `function` modifier is
/// intentionally not rendered — laptop Fn rarely reaches the app and was
/// never a preset.
pub fn format_chord(m: &Modifiers, key: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if m.control {
        parts.push("Ctrl".into());
    }
    if m.alt {
        parts.push("Alt".into());
    }
    if m.shift {
        parts.push("Shift".into());
    }
    if m.platform {
        parts.push("Win".into());
    }
    parts.push(key_label(key));
    parts.join("+")
}

/// Title-case a single key for display.  Single letters uppercase
/// (`"v"` → `"V"`), function keys keep their number (`"f8"` → `"F8"`),
/// well-known named keys get a friendly spelling, and anything else has
/// its first character upper-cased.
fn key_label(key: &str) -> String {
    // Single ASCII letter → uppercase.
    if key.len() == 1 {
        let c = key.as_bytes()[0];
        if c.is_ascii_alphabetic() {
            return key.to_ascii_uppercase();
        }
        // Digits / punctuation single chars pass through unchanged.
        return key.to_owned();
    }
    // Function keys f1..f24 → uppercase F, keep the number.
    if let Some(n) = key.strip_prefix('f') {
        if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
            return format!("F{n}");
        }
    }
    match key {
        "space" => "Space".into(),
        "backspace" => "Backspace".into(),
        "delete" => "Delete".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        "up" => "Up".into(),
        "down" => "Down".into(),
        "left" => "Left".into(),
        "right" => "Right".into(),
        "pageup" => "PageUp".into(),
        "pagedown" => "PageDown".into(),
        "insert" => "Insert".into(),
        "escape" => "Escape".into(),
        _ => {
            // Fallback: upper-case the first char, keep the rest.
            let mut chars = key.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Keystroke;

    /// `Keystroke::parse` is the test-event constructor gpui ships;
    /// `"ctrl-alt-v"` builds the same struct the platform hands key-down.
    fn ks(s: &str) -> Keystroke {
        Keystroke::parse(s).expect("valid test keystroke")
    }

    #[test]
    fn commits_modifier_plus_letter_in_preset_style() {
        // Capital V in parse becomes shift+v, so spell it `ctrl-alt-v`.
        assert_eq!(
            resolve_capture(&ks("ctrl-alt-v")),
            CaptureOutcome::Committed("Ctrl+Alt+V".into())
        );
    }

    #[test]
    fn commits_bare_function_key() {
        assert_eq!(
            resolve_capture(&ks("f8")),
            CaptureOutcome::Committed("F8".into())
        );
    }

    #[test]
    fn commits_ctrl_space_matching_legacy_preset() {
        // Back-compat with the shipped default `"Ctrl+Space"`.
        assert_eq!(
            resolve_capture(&ks("ctrl-space")),
            CaptureOutcome::Committed("Ctrl+Space".into())
        );
    }

    #[test]
    fn shift_renders_after_ctrl_alt() {
        // Order is always Ctrl, Alt, Shift, Win regardless of input order.
        assert_eq!(format_chord(&ks("shift-ctrl-x").modifiers, "x"), "Ctrl+Shift+X");
    }

    #[test]
    fn win_modifier_renders() {
        assert_eq!(format_chord(&ks("cmd-k").modifiers, "k"), "Win+K");
    }

    #[test]
    fn escape_cancels() {
        assert_eq!(resolve_capture(&ks("escape")), CaptureOutcome::Cancelled);
    }

    #[test]
    fn bare_modifier_is_pending_not_a_commit() {
        // `Keystroke::parse("ctrl")` yields key="control", no modifiers —
        // the lone-modifier shape.  Must not commit.
        assert_eq!(resolve_capture(&ks("ctrl")), CaptureOutcome::Pending);
        assert_eq!(resolve_capture(&ks("shift")), CaptureOutcome::Pending);
        assert_eq!(resolve_capture(&ks("alt")), CaptureOutcome::Pending);
    }

    #[test]
    fn reserved_keys_are_rejected() {
        assert_eq!(
            resolve_capture(&ks("enter")),
            CaptureOutcome::Reserved(RESERVED_NOTE)
        );
        assert_eq!(
            resolve_capture(&ks("tab")),
            CaptureOutcome::Reserved(RESERVED_NOTE)
        );
        // Even with a modifier, Enter/Tab stay reserved.
        assert_eq!(
            resolve_capture(&ks("ctrl-enter")),
            CaptureOutcome::Reserved(RESERVED_NOTE)
        );
    }

    #[test]
    fn space_and_named_keys_format_titlecased() {
        assert_eq!(key_label("space"), "Space");
        assert_eq!(key_label("pageup"), "PageUp");
        assert_eq!(key_label("home"), "Home");
        // Unknown named key falls back to first-char upper.
        assert_eq!(key_label("menu"), "Menu");
    }

    #[test]
    fn digit_key_passes_through() {
        assert_eq!(
            resolve_capture(&ks("ctrl-1")),
            CaptureOutcome::Committed("Ctrl+1".into())
        );
    }

    #[test]
    fn bare_letter_without_modifiers_commits() {
        // A bare key is a valid PTT binding (F8/CapsLock were presets);
        // only the reserved list blocks a commit.
        assert_eq!(
            resolve_capture(&ks("g")),
            CaptureOutcome::Committed("G".into())
        );
    }
}
