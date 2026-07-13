//! Inline `<think>…</think>` stream splitter (agentic reasoning tier P1a).
//!
//! Reasoner models (DeepSeek-R1 and friends) emit their reasoning trace
//! **inline** in `message.content`, wrapped in `<think>…</think>`, rather
//! than on the separate `message.thinking` field that the native
//! think-API models use (that path is handled by
//! [`super::actions::extract_chunk_thinking`]). Left untouched, the
//! `<think>` block would leak into the user-visible answer.
//!
//! [`ThinkSplitter`] consumes the streamed content piece-by-piece and
//! splits each piece into an **answer** delta (the visible reply) and a
//! **thinking** delta (routed to [`crate::events::TurnEvent::Thinking`],
//! which the chat-processing-indicator dropdown already renders,
//! collapsed by default). The tag markers can straddle chunk boundaries,
//! so a possible partial-marker tail is held back until the next piece
//! resolves it.
//!
//! **Identity invariant (the reasoning-tier gate, fast path).** Today's
//! fast model emits no `<think>`. When the full stream contains no
//! `<think>` marker the splitter is a pure pass-through: the concatenation
//! of every `answer` delta plus [`ThinkSplitter::finish`] equals the input
//! byte-for-byte, and no `thinking` is ever produced. So a fast turn is
//! byte-identical to the pre-P1 path — the splitter only diverges once a
//! reasoner actually thinks.

/// Opening reasoning marker.
const OPEN: &str = "<think>";
/// Closing reasoning marker.
const CLOSE: &str = "</think>";

/// One split step: the visible-answer text and the reasoning text peeled
/// out of a single input piece. Either (or both) may be empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThinkDelta {
    /// Text to append to the user-visible answer.
    pub answer: String,
    /// Reasoning text to emit as a [`crate::events::TurnEvent::Thinking`] delta.
    pub thinking: String,
}

impl ThinkDelta {
    /// True when this delta carries no answer and no thinking bytes.
    pub fn is_empty(&self) -> bool {
        self.answer.is_empty() && self.thinking.is_empty()
    }
}

/// Stateful splitter that peels inline `<think>…</think>` reasoning out of
/// a streamed content sequence. Drive it with [`ThinkSplitter::push`] per
/// chunk and [`ThinkSplitter::finish`] once the stream closes.
#[derive(Debug, Clone, Default)]
pub struct ThinkSplitter {
    /// Are we currently inside an open `<think>` block?
    in_think: bool,
    /// Bytes held back because they *might* be the leading fragment of a
    /// marker split across a chunk boundary (e.g. a piece ending `"<thi"`).
    hold: String,
}

impl ThinkSplitter {
    /// Fresh splitter, outside any think block.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one streamed content piece; return the answer / thinking split.
    pub fn push(&mut self, piece: &str) -> ThinkDelta {
        let mut buf = std::mem::take(&mut self.hold);
        buf.push_str(piece);

        let mut delta = ThinkDelta::default();
        let mut rest = buf.as_str();

        loop {
            let marker = if self.in_think { CLOSE } else { OPEN };
            match rest.find(marker) {
                Some(idx) => {
                    // Everything before the marker belongs to the current sink;
                    // then the marker flips the state and is itself dropped.
                    self.route(&rest[..idx], &mut delta);
                    self.in_think = !self.in_think;
                    rest = &rest[idx + marker.len()..];
                }
                None => {
                    // No full marker. Hold back a trailing fragment that could
                    // be the *start* of this marker on the next piece; emit the
                    // rest to the current sink.
                    let keep = partial_marker_len(rest, marker);
                    let split = rest.len() - keep;
                    self.route(&rest[..split], &mut delta);
                    self.hold.push_str(&rest[split..]);
                    break;
                }
            }
        }
        delta
    }

    /// Close the stream: flush any held-back tail. A dangling partial marker
    /// or unterminated `<think>` is treated as literal content of whichever
    /// sink was active — nothing is ever dropped.
    pub fn finish(&mut self) -> ThinkDelta {
        let tail = std::mem::take(&mut self.hold);
        let mut delta = ThinkDelta::default();
        self.route(&tail, &mut delta);
        delta
    }

    /// Append `text` to the answer or thinking sink per the current state.
    fn route(&self, text: &str, delta: &mut ThinkDelta) {
        if text.is_empty() {
            return;
        }
        if self.in_think {
            delta.thinking.push_str(text);
        } else {
            delta.answer.push_str(text);
        }
    }
}

/// Length of the longest suffix of `s` that is a *proper prefix* of
/// `marker` — i.e. the tail we must hold back because it could grow into
/// `marker` on the next piece. Returns `0` when no such overlap exists.
fn partial_marker_len(s: &str, marker: &str) -> usize {
    // The longest possible dangling fragment is `marker.len() - 1` bytes.
    let max = marker.len().saturating_sub(1).min(s.len());
    for keep in (1..=max).rev() {
        let at = s.len() - keep;
        // The markers are ASCII, so a real overlap always starts on a char
        // boundary; a non-boundary slice can't match and would panic — skip.
        if !s.is_char_boundary(at) {
            continue;
        }
        let tail = &s[at..];
        if marker.as_bytes().starts_with(tail.as_bytes()) {
            return keep;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a whole content string through the splitter one piece at a
    /// time and collect the concatenated (answer, thinking) result.
    fn run_pieces(pieces: &[&str]) -> (String, String) {
        let mut s = ThinkSplitter::new();
        let mut answer = String::new();
        let mut thinking = String::new();
        for p in pieces {
            let d = s.push(p);
            answer.push_str(&d.answer);
            thinking.push_str(&d.thinking);
        }
        let d = s.finish();
        answer.push_str(&d.answer);
        thinking.push_str(&d.thinking);
        (answer, thinking)
    }

    #[test]
    fn identity_passthrough_when_no_think_marker() {
        // THE gate-off invariant: no `<think>` anywhere ⇒ answer is the
        // input verbatim, thinking is empty, regardless of chunking.
        let cases: &[&[&str]] = &[
            &["hello world"],
            &["hel", "lo ", "world"],
            &["a", "b", "c", "d"],
            &["angle < bracket > prose"],
            &["1 < 2 and 3 > 2"],
            &[""],
            &["", "x", ""],
        ];
        for pieces in cases {
            let joined: String = pieces.concat();
            let (answer, thinking) = run_pieces(pieces);
            assert_eq!(answer, joined, "answer must equal input for {pieces:?}");
            assert!(thinking.is_empty(), "no thinking for {pieces:?}");
        }
    }

    #[test]
    fn golden_single_think_block() {
        // Canned `<think>…</think>answer` ⇒ one clean answer + the trace.
        let (answer, thinking) =
            run_pieces(&["<think>let me reason about it</think>The answer is 42."]);
        assert_eq!(answer, "The answer is 42.");
        assert_eq!(thinking, "let me reason about it");
    }

    #[test]
    fn think_block_with_leading_and_trailing_answer() {
        let (answer, thinking) = run_pieces(&["Sure. <think>hmm</think> Done."]);
        assert_eq!(answer, "Sure.  Done.");
        assert_eq!(thinking, "hmm");
    }

    #[test]
    fn marker_split_across_chunk_boundary() {
        // `<think>` and `</think>` each straddle a chunk seam.
        let (answer, thinking) = run_pieces(&["<thi", "nk>rea", "son</thin", "k>ans"]);
        assert_eq!(answer, "ans");
        assert_eq!(thinking, "reason");
    }

    #[test]
    fn open_marker_split_one_byte_at_a_time() {
        let pieces: Vec<&str> = "<think>x</think>y".split("").collect();
        let (answer, thinking) = run_pieces(&pieces);
        assert_eq!(answer, "y");
        assert_eq!(thinking, "x");
    }

    #[test]
    fn multiple_think_blocks() {
        let (answer, thinking) =
            run_pieces(&["a<think>one</think>b<think>two</think>c"]);
        assert_eq!(answer, "abc");
        assert_eq!(thinking, "onetwo");
    }

    #[test]
    fn unterminated_think_is_all_reasoning() {
        // A `<think>` with no close ⇒ the remainder is reasoning, never
        // leaked to the answer; finish() flushes it.
        let (answer, thinking) = run_pieces(&["ok <think>still thinking"]);
        assert_eq!(answer, "ok ");
        assert_eq!(thinking, "still thinking");
    }

    #[test]
    fn dangling_partial_open_marker_flushes_as_answer() {
        // Stream ends mid-`<think` fragment that never completes ⇒ the
        // fragment is literal answer text (identity preserved).
        let (answer, thinking) = run_pieces(&["value <thi"]);
        assert_eq!(answer, "value <thi");
        assert!(thinking.is_empty());
    }

    #[test]
    fn lone_angle_bracket_is_not_held_forever() {
        let (answer, thinking) = run_pieces(&["a < b", " and c"]);
        assert_eq!(answer, "a < b and c");
        assert!(thinking.is_empty());
    }

    #[test]
    fn partial_marker_len_finds_prefix_overlap() {
        assert_eq!(partial_marker_len("foo<thi", OPEN), 4); // "<thi"
        assert_eq!(partial_marker_len("foo<", OPEN), 1); // "<"
        assert_eq!(partial_marker_len("foobar", OPEN), 0);
        assert_eq!(partial_marker_len("x</thin", CLOSE), 6); // "</thin"
        // A complete marker is not a *proper* prefix — find() handles those.
        assert_eq!(partial_marker_len("<think>", OPEN), 0);
    }
}
