//! Composer input glue (Slice F, Build Order §5 "the text input widget
//! itself").
//!
//! The widget IS the shared [`wylde_gpui_input::TextInput`] the Chat panel
//! already mounts (no fork — one input component across the app, per the
//! Input crate's charter). What this file owns is the composer-side input
//! policy around it: the **debounce model** for recognition scans (typing
//! shouldn't fire a pipe lookup per keystroke) and the palette's
//! **insert-reference** edit.

use wylde_gpui_input::TextInput;

/// How long the text must sit still before a recognition scan fires.
/// Comfortably under "I paused to think", comfortably over "I'm mid-word".
pub const SCAN_DEBOUNCE_MS: u64 = 300;

/// Insert an `@symbol ` reference at the cursor (the Ctrl+P palette's
/// accept action — OQ5's reference syntax). A trailing space ends the token
/// so the tokenizer treats it as complete; surrounding whitespace is added
/// only when needed.
pub fn insert_reference(input: &mut TextInput, name: &str) {
    let buf = input.buffer_mut();
    let needs_leading_space = {
        let text = buf.text();
        let cur = buf.cursor();
        cur > 0
            && text[..cur]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace())
    };
    let mut s = String::new();
    if needs_leading_space {
        s.push(' ');
    }
    s.push('@');
    s.push_str(name);
    s.push(' ');
    buf.push_snapshot();
    buf.insert_str(&s);
}

#[cfg(test)]
mod tests {
    // `TextInput` needs a gpui entity context even for buffer-level edits;
    // exercise the pure string mechanics through a raw TextBuffer instead
    // (`insert_reference` is the same policy applied via `buffer_mut`).
    use wylde_gpui_input::TextBuffer;

    fn insert_into(buf: &mut TextBuffer, name: &str) {
        // Mirror of `insert_reference`'s policy, applied straight to a
        // buffer (the entity wrapper only adds notify plumbing).
        let needs_leading_space = {
            let text = buf.text();
            let cur = buf.cursor();
            cur > 0
                && text[..cur]
                    .chars()
                    .next_back()
                    .is_some_and(|c| !c.is_whitespace())
        };
        let mut s = String::new();
        if needs_leading_space {
            s.push(' ');
        }
        s.push('@');
        s.push_str(name);
        s.push(' ');
        buf.insert_str(&s);
    }

    #[test]
    fn inserts_at_cursor_with_spacing() {
        let mut buf = TextBuffer::with_text("explain please", false);
        buf.set_cursor(7, false); // after "explain"
        insert_into(&mut buf, "set_active");
        assert_eq!(buf.text(), "explain @set_active  please");
    }

    #[test]
    fn no_double_space_after_whitespace_or_at_start() {
        let mut buf = TextBuffer::with_text("", false);
        insert_into(&mut buf, "GraphView");
        assert_eq!(buf.text(), "@GraphView ");

        let mut buf = TextBuffer::with_text("look at ", false);
        buf.set_cursor(8, false);
        insert_into(&mut buf, "turn");
        assert_eq!(buf.text(), "look at @turn ");
    }

    #[allow(dead_code)]
    fn signature_targets_the_shared_input(input: &mut super::TextInput) {
        // Compile-time witness that the public helper targets the shared
        // TextInput type (the panel calls it inside `input.update(...)`).
        super::insert_reference(input, "x");
    }
}
