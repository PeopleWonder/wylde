# build-watcher — self-hosted verification loop for agent sessions

Agent sessions (Cowork / Claude Code in a sandbox) can read and write this
repo but cannot run `cargo` — no Rust toolchain is reachable from the
sandbox. The watcher closes that loop on the machine that already builds
Wylde, with no cloud CI and no public exposure of work-in-progress
branches.

## How it works

```
agent                              your machine (watcher)
─────                              ───────────────────────
writes outputs/build-requests/     polls every 2s
  <id>.request  ──────────────────▶ runs the listed cargo targets
                                    writes outputs/build-results/
reads  ◀──────────────────────────    <id>.result.txt (+ exit codes)
fixes, repeats
```

Start it with **`start-build-watcher.bat`** (double-click; leave the
window open). Heartbeat at `outputs/build-watcher.alive`; log at
`outputs/build-watcher.log`. No admin rights needed.

## Request format

One target per line in `<id>.request`:

| Target | Runs |
|---|---|
| `backend` / `gui` | `cargo test` on the whole rust/ or Core/GUI workspace |
| `test:<crate>` `check:<crate>` `clippy:<crate>` | that verb, `-p <crate>`, rust/ workspace |
| `gui-test:<crate>` `gui-check:<crate>` `gui-clippy:<crate>` | same, Core/GUI workspace |

## Security model

The watcher executes a **fixed menu** of cargo invocations only. Request
files cannot smuggle commands: crate names are validated against
`^[a-zA-Z0-9_-]+$` and passed as separate argv elements; anything else is
skipped and logged. Invocation is `powershell -File` (no inline
`-Command`), per the repo's Defender-friendly tooling rule.
