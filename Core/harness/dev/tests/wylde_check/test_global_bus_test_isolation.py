"""Tests for rule 60 (``global_bus_test_isolation``) — mirrors prod-side
``wylde_check/rules/_global_bus_test_isolation.py``.

A ``#[cfg(test)]`` test that reaches a process-global ``broadcast::Sender``
must own its channel (injection) or serialize on a test-module ``Mutex``
guard — the ``src/``-unit-test half of the #83 self-collision class rule 56
covers for ``tests/`` binaries (#246).
"""

from __future__ import annotations

from typing import Any, List

from .conftest import _write

_CRATE_SRC = ("rust", "crates", "wylde-workspaces", "src")


def _src(root: Any, name: str, body: str) -> None:
    path = root
    for part in _CRATE_SRC:
        path = path / part
    _write(path / f"{name}.rs", body)


def _names(findings: List[Any]) -> List[str]:
    return sorted(f.context for f in findings)


# The bus definition every fixture shares: a global sender + accessor, a
# `publish` that reaches it, and a `subscribe`.
_BUS_DEF = """\
use tokio::sync::broadcast;

static SENDER: OnceLock<broadcast::Sender<Evt>> = OnceLock::new();

fn sender() -> &'static broadcast::Sender<Evt> {
    SENDER.get_or_init(|| broadcast::channel(128).0)
}

pub fn subscribe() -> broadcast::Receiver<Evt> {
    sender().subscribe()
}

pub fn publish(e: Evt) -> usize {
    sender().send(e).unwrap_or(0)
}
"""


def _file(test_mod: str, *, extra_top: str = "") -> str:
    return _BUS_DEF + extra_top + "\n#[cfg(test)]\nmod tests {\n" + test_mod + "}\n"


# ── FAIL cases (the #246 shape) ──────────────────────────────────────


def test_flags_bare_subscribe_in_a_test(isolated_tree: Any) -> None:
    """The exact #246 failure: assert on the first event off the global."""
    wc, root = isolated_tree
    _src(
        root,
        "bus",
        _file(
            """\
    use super::*;

    #[tokio::test]
    async fn delta_event_is_broadcast() {
        let mut rx = subscribe();
        publish(Evt::One);
        assert_eq!(rx.recv().await.unwrap(), Evt::One);
    }
"""
        ),
    )
    found = wc.check_global_bus_test_isolation()
    assert _names(found) == ["delta_event_is_broadcast"]
    assert found[0].severity == "error"
    assert found[0].rule == "global_bus_test_isolation"


def test_flags_a_publisher_that_never_names_the_bus(isolated_tree: Any) -> None:
    """The collider #246 taught us about.

    ``drives_the_loop`` mentions neither the bus nor ``publish`` — it calls a
    helper that runs product code which publishes. Rule 56's
    "one test can't self-collide" carve-out would have missed this entirely;
    here it is in scope.
    """
    wc, root = isolated_tree
    _src(
        root,
        "bus",
        _file(
            """\
    use super::*;

    fn spawn_loop() -> Handle {
        tokio::spawn(run_loop())
    }

    #[tokio::test]
    async fn drives_the_loop() {
        let _h = spawn_loop();
        assert!(true);
    }
""",
            extra_top="\nasync fn run_loop() {\n    publish(Evt::One);\n}\n",
        ),
    )
    assert _names(wc.check_global_bus_test_isolation()) == ["drives_the_loop"]


def test_flags_every_toucher_not_just_the_reader(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _src(
        root,
        "bus",
        _file(
            """\
    use super::*;

    #[tokio::test]
    async fn reads() {
        let mut _rx = subscribe();
    }

    #[tokio::test]
    async fn writes() {
        publish(Evt::One);
    }

    #[tokio::test]
    async fn touches_nothing() {
        assert_eq!(2 + 2, 4);
    }
"""
        ),
    )
    assert _names(wc.check_global_bus_test_isolation()) == ["reads", "writes"]


# ── PASS cases (no findings) ─────────────────────────────────────────


def test_pass_injected_channel_per_test(isolated_tree: Any) -> None:
    """The preferred fix (#246): the test owns the channel end-to-end."""
    wc, root = isolated_tree
    _src(
        root,
        "bus",
        _file(
            """\
    use super::*;

    fn spawn_loop() -> (broadcast::Receiver<Evt>, Handle) {
        let (tx, rx) = broadcast::channel(128);
        (rx, tokio::spawn(run_loop(tx)))
    }

    #[tokio::test]
    async fn delta_event_is_broadcast() {
        let (mut rx, _h) = spawn_loop();
        assert_eq!(rx.recv().await.unwrap(), Evt::One);
    }
""",
            extra_top=(
                "\nasync fn run_loop(tx: broadcast::Sender<Evt>) {\n"
                "    let _ = tx.send(Evt::One);\n}\n"
            ),
        ),
    )
    assert wc.check_global_bus_test_isolation() == []


def test_pass_serialized_on_a_test_module_guard(isolated_tree: Any) -> None:
    """The ``TEST_GUARD``/``guard()`` shape already used by the Pipe buses —
    rule 56's DB_LOCK pattern, applied to a bus."""
    wc, root = isolated_tree
    _src(
        root,
        "bus",
        _file(
            """\
    use super::*;

    static TEST_GUARD: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn subscriber_receives_change() {
        let _g = guard();
        let mut rx = subscribe();
        publish(Evt::One);
        assert!(rx.try_recv().is_ok());
    }
"""
        ),
    )
    assert wc.check_global_bus_test_isolation() == []


def test_pass_direct_lock_without_a_helper(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _src(
        root,
        "bus",
        _file(
            """\
    use super::*;

    static TEST_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn subscriber_receives_change() {
        let _g = TEST_GUARD.lock().unwrap();
        let mut rx = subscribe();
        assert!(rx.try_recv().is_err());
    }
"""
        ),
    )
    assert wc.check_global_bus_test_isolation() == []


def test_pass_file_with_no_global_bus(isolated_tree: Any) -> None:
    """A local channel is not a shared resource — out of scope."""
    wc, root = isolated_tree
    _src(
        root,
        "plain",
        """\
use tokio::sync::broadcast;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_channel_is_fine() {
        let (tx, mut rx) = broadcast::channel(4);
        let _ = tx.send(1);
        assert_eq!(rx.recv().await.unwrap(), 1);
    }
}
""",
    )
    assert wc.check_global_bus_test_isolation() == []


def test_pass_bus_with_no_unit_tests(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _src(root, "bus", _BUS_DEF)
    assert wc.check_global_bus_test_isolation() == []


def test_integration_binaries_are_rule_56s_half(isolated_tree: Any) -> None:
    """A ``tests/`` binary is out of scope here — rule 56 owns that half, and
    double-reporting one hazard under two rules helps nobody."""
    wc, root = isolated_tree
    path = root / "rust" / "crates" / "wylde-workspaces" / "tests"
    _write(
        path / "bus_integration.rs",
        _file(
            """\
    use super::*;

    #[tokio::test]
    async fn reads() {
        let mut _rx = subscribe();
    }
"""
        ),
    )
    assert wc.check_global_bus_test_isolation() == []


# ── Hygiene: comments and the opt-out ────────────────────────────────


def test_commented_out_subscribe_does_not_arm_the_rule(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _src(
        root,
        "bus",
        _file(
            """\
    use super::*;

    #[test]
    fn nothing_live_here() {
        // let mut rx = subscribe();
        assert_eq!(2 + 2, 4);
    }
"""
        ),
    )
    assert wc.check_global_bus_test_isolation() == []


def test_commented_out_guard_does_not_count_as_serialization(
    isolated_tree: Any,
) -> None:
    """The mirror case: a guard that isn't actually acquired must not
    satisfy the rule."""
    wc, root = isolated_tree
    _src(
        root,
        "bus",
        _file(
            """\
    use super::*;

    static TEST_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn reads_unguarded() {
        // let _g = TEST_GUARD.lock().unwrap();
        let mut _rx = subscribe();
    }
"""
        ),
    )
    assert _names(wc.check_global_bus_test_isolation()) == ["reads_unguarded"]


def test_opt_out_marker_suppresses_the_finding(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _src(
        root,
        "bus",
        _file(
            """\
    use super::*;

    // Deliberately asserts the global wiring. (wylde-check: global-bus-test-ok)
    #[test]
    fn asserts_the_global_wiring() {
        let mut _rx = subscribe();
    }
"""
        ),
    )
    assert wc.check_global_bus_test_isolation() == []


# ── File-backed test modules (the rule-20 split escape hatch) ─────────


def _mod_dir(root: Any):
    path = root
    for part in _CRATE_SRC:
        path = path / part
    return path / "watcher"


def test_follows_a_file_backed_test_module(isolated_tree: Any) -> None:
    """A bus file whose tests live in a sibling `mod tests;` file.

    This is not hypothetical: #246's own fix pushed `watcher/mod.rs` past
    rule 20's 700-line cap, so the tests moved to `watcher/tests.rs`. A rule
    that only understood the inline `mod tests { … }` form would have gone
    quiet at exactly the moment the file it guards got split — the #101/#116
    "gate goes quiet rather than red" decay.
    """
    wc, root = isolated_tree
    d = _mod_dir(root)
    _write(d / "mod.rs", _BUS_DEF + "\n#[cfg(test)]\nmod tests;\n")
    _write(
        d / "tests.rs",
        """\
use super::*;

#[tokio::test]
async fn reads_the_global() {
    let mut rx = subscribe();
    assert!(rx.try_recv().is_err());
}
""",
    )
    found = wc.check_global_bus_test_isolation()
    assert _names(found) == ["reads_the_global"]
    assert found[0].file.endswith("watcher/tests.rs")


def test_file_backed_module_may_isolate(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    d = _mod_dir(root)
    _write(
        d / "mod.rs",
        _BUS_DEF
        + "\nasync fn run_loop(tx: broadcast::Sender<Evt>) {\n"
        "    let _ = tx.send(Evt::One);\n}\n"
        "\n#[cfg(test)]\nmod tests;\n",
    )
    _write(
        d / "tests.rs",
        """\
use super::*;

fn spawn_loop() -> broadcast::Receiver<Evt> {
    let (tx, rx) = broadcast::channel(128);
    tokio::spawn(run_loop(tx));
    rx
}

#[tokio::test]
async fn delta_event_is_broadcast() {
    let mut rx = spawn_loop();
    assert_eq!(rx.recv().await.unwrap(), Evt::One);
}
""",
    )
    assert wc.check_global_bus_test_isolation() == []
