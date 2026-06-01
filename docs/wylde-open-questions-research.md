# Open Questions Research — Q-V2 (Voice NPU) and Q-E1 (MCP SDK pin)

Date: 2026-05-23
Author: research task (Phase 4 + Phase 10-11 gate)
Status: decision-ready

## Q-V2: Voice NPU via `ort` + OpenVINO EP

### Context

Phase 10 of the Rust migration is the Voice spike that decides whether
Phase 11a (full Rust port of `Voice/`) or Phase 11b (Rust shim around the
existing Python subprocess) ships. The NPU question is the one open variable:
today's `Voice/transcribe.py:142` reaches NPU through `optimum.intel` +
direct OpenVINO runtime with a static-shape encoder rebuild
([transcribe.py:258](../Voice/transcribe.py)). If Rust can't reach the
same NPU path, latency on the Wylde user's Intel CPU regresses and Phase 11a's
value drops.

### Investigation

**`ort` crate state.** `ort` 2.0.0-rc.12 is the current release (Mar 2026,
[crates.io/ort](https://crates.io/crates/ort), [ort.pyke.io](https://ort.pyke.io/)).
The 2.x line is the recommended one for new projects but still
RC — not GA. It exposes the OpenVINO Execution Provider via the `openvino`
Cargo feature, mirroring upstream ONNX Runtime's OpenVINO EP.

**Does it surface NPU?** Yes. ONNX Runtime's OpenVINO EP accepts
`device_type` strings `"CPU" | "GPU" | "GPU.0" | "GPU.1" | "NPU"` plus
the multi-device forms `"AUTO:GPU,NPU,CPU"`, `"HETERO:NPU,CPU"`,
`"MULTI:..."` ([ORT OpenVINO EP docs](https://onnxruntime.ai/docs/execution-providers/OpenVINO-ExecutionProvider.html)).
The `"HETERO:NPU,CPU"` string is identical to the Wylde user's
`transcribe.py:152` device chain. Compatibility table: ORT 1.24.1 ↔
OpenVINO 2025.4.1, ORT 1.23.0 ↔ OpenVINO 2025.3. `ort` 2.0-rc.12 wraps
the 1.2x ORT line.

**Whisper-on-NPU corpus.** Documented and improving:
- [openvino_notebooks#2691](https://github.com/openvinotoolkit/openvino_notebooks/issues/2691)
  is the canonical Whisper-on-NPU thread. The exact error the Wylde user worked
  around (`Channels count … != 80`) is the same VPUX dynamic-shape abort
  cited there.
- OpenVINO 2025.x release notes: "Exporting stateful Whisper models is
  now supported on NPU out of the box; `--disable-stateful` is no longer
  required." NPU plugin now reshapes batch size 1 internally and manages
  concurrent inference requests.
- Microsoft's "ONNX and NPU Acceleration for Speech on ARM" published a
  working Whisper-NPU pipeline through ONNX Runtime's OpenVINO EP — same
  code path the `ort` crate exposes, just from C++/Python instead of Rust.

**Static-shape constraint.** The ORT OpenVINO EP exposes a
`reshape_input` provider option specifically for NPU shape bounds:
`reshape_input="input_features[1..1,80..80,3000..3000]"` reproduces the Wylde user's
Python `enc_model.reshape({"input_features": [1, 80, 3000]})`. The
constraint is **surface-level, not model-export-deep** — the same `.onnx`
weights work; NPU compilation just needs the shape bounds passed at
session creation. the Wylde user's current pipeline pre-bakes the static encoder
to `.xml`; the ORT path would do it JIT via provider option. Both routes
end up with a statically-shaped NPU-loaded encoder.

**Risks not yet retired by research alone:**
1. ORT's OpenVINO EP loads `.onnx`, not OpenVINO IR (`.xml`/`.bin`).
   the Wylde user's existing IR cache (`~/.cache/huggingface/hub/ov-export/…-npu/`)
   isn't directly reusable. Either re-export to ONNX, or use
   [`openvino-rs`](https://github.com/intel/openvino-rs) (direct Rust
   bindings to the OpenVINO runtime) which *does* accept IR. The
   `openvino-rs` route most closely mirrors today's Python.
2. `ort` 2.0 is still RC; one breaking change between RC and GA is
   plausible.
3. The HETERO fallback (decoder → CPU because dynamic) needs the same
   plumbing in Rust — `optimum.intel`'s HF `pipeline()` wrapper hides a
   lot of generation-loop machinery (chunk_length_s=30, stride=5,
   timestamp handling). That's not NPU work but is Phase 11a work.

### Recommendation

**(b) Spike before committing.** All three reachability conditions are
met on paper: `ort` exposes OpenVINO EP, OpenVINO EP supports NPU and
HETERO, the static-shape constraint has a documented per-session
workaround. But "paper-true on a generic Intel NPU" is not the same as
"works on the Wylde user's Lunar/Meteor/Arrow-Lake NPU with whisper-small.en at
the latency budget." The spike retires the residual risk cheaply
(~2 sessions) before Phase 11a commits 14–22 sessions.

### Spike outline

Scope: minimal Rust binary, not yet a workspace member.

```
rust/spikes/voice-npu/  (out-of-workspace)
  Cargo.toml          # ort = "2.0.0-rc.12", features = ["openvino"]
  src/main.rs         # load whisper-small.en encoder, NPU device,
                      # reshape_input, run on a 30s clip
```

Success criteria:
1. Session compiles and loads on `device_type="NPU"` without the
   `Channels count` VPUX abort.
2. Single 30s clip transcribes to a WER ≤ 1pp delta from
   `faster-whisper` CPU baseline.
3. NPU is actually engaged (verified via `openvino-rs`' device probe or
   Task Manager NPU utilization).
4. p50 latency within 1.3× of the existing Python NPU path.

If (1) fails, fall through to `openvino-rs` (load IR directly, exactly
mirroring the Python path). If `openvino-rs` also fails, outcome (c) —
Phase 11b ships, NPU stays in Python.

Spike should re-use the existing `ov-export/.../...-npu/openvino_encoder_model.xml`
artifact (re-converted to ONNX or loaded via `openvino-rs`), not
re-export from scratch — keeps the spike tight.

---

## Q-E1: MCP SDK version pin

### Context

Phase 4 redesigns Extensions as MCP servers spoken to by a Rust
extension host. The pin question matters because (a) MCP is mid-flight:
the 2025-11-25 spec is current but the 2026-07-28 release candidate
introduces breaking changes (stateless core, removed init handshake,
deprecated Roots/Sampling/Logging, error code shifts), and (b) the
official Rust SDK (`rmcp`) is **Tier 2**, not Tier 1, which has
real implications for upgrade cadence.

### Investigation

**Spec state.** Current stable: `2025-11-25`
([modelcontextprotocol.io/specification/2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25)).
RC for next version: `2026-07-28` (final ships July 28, 2026); the
10-week validation window runs May 21 → July 28.

**Rust SDK (`rmcp`).** Latest published: **1.7.0** (May 13, 2026)
([docs.rs/rmcp/latest](https://docs.rs/crate/rmcp/latest)). Official
SDK under `modelcontextprotocol/rust-sdk`. Implements spec `2025-11-25`.
Features: `server`, `client`, `macros`, `schemars`, `auth`,
`elicitation`. Pluggable transports (stdio, async R/W, HTTP).

**SDK tiers** ([modelcontextprotocol.io/docs/sdk](https://modelcontextprotocol.io/docs/sdk)):
- **Tier 1** (ships new spec within the 10-week window): TypeScript,
  Python, C#, Go.
- **Tier 2** (no SLA, lags by weeks-to-months): **Rust**, Java.
- Tier 3: Swift, Ruby, PHP. TBD: Kotlin.

**Practical implication for Wylde:** when 2026-07-28 ships, `rmcp` will
likely lag. Wylde's host will be on `2025-11-25` for a non-trivial
window. Extension authors who pick up Python SDK 1.27+ (Tier 1) may
ship MCP servers ahead of the host's spec window. The deprecation
policy gives 12 months between deprecation and removal — comfortable
buffer for that lag.

### Recommendation

**Pin both, separately, in two places:**

```toml
# rust/Cargo.toml [workspace.dependencies]
rmcp = { version = "1.7", features = ["server", "client", "macros", "schemars"] }
```

```rust
// rust/crates/wylde-extension-host/src/spec.rs
pub const MCP_SPEC_VERSION: &str = "2025-11-25";
```

`rmcp`'s semver minor bumps within `1.x` track non-breaking SDK
improvements; the spec constant is what's reported in the MCP
`initialize` response and gates compatibility with extensions.

**Upgrade procedure** (in `docs/mcp-upgrade.md`, to be authored as part
of Phase 4):
1. Bump `rmcp` minor → run `wylde-extension-host` against the two
   bundled test MCP servers (`Extensions/_test_stdio_mcp/`,
   `Extensions/_test_http_mcp/` per Phase 4 DoD) — must pass.
2. Bump `MCP_SPEC_VERSION` only when (a) `rmcp` exposes the new spec
   version *and* (b) all bundled extensions have been verified against
   it. The two bumps are independent; never bump the spec constant
   ahead of the SDK.
3. The Python-MCP shim (`wylde-mcp-py-shim`, Phase 4) bumps its `mcp`
   Python dep in lockstep with the Rust host's spec constant.

### Per-extension version compat policy

**Policy: host advertises one spec version; accepts extensions on N or
N−1; rejects N+1 with a clear log line.**

Rationale: the MCP `initialize` handshake (still present in 2025-11-25)
exchanges spec versions. Most extension authors will track the
ecosystem; lagging one version is the realistic max. Accepting N+1
would require host-side forward-compat logic for unreleased spec
features — not worth it.

Concretely:
- Host pins `MCP_SPEC_VERSION = "2025-11-25"` (one version).
- During `initialize`, if extension advertises `2025-11-25` or
  `2025-06-18` (the N−1 stable), host proceeds.
- If extension advertises anything else, host logs
  `extension <name> requires spec X, host supports 2025-11-25 (and N-1
  2025-06-18); install a compatible extension build` and refuses to
  register the extension.

When the host bumps to 2026-07-28, the window becomes `{2026-07-28,
2025-11-25}` for one cycle, then drops 2025-11-25 at the next host
bump. This guarantees extension authors a minimum 6-month grace window
matching Anthropic's own 12-month deprecation policy with margin.

---

## Summary punchlist for the Wylde user

- [ ] **Q-V2 outcome: (b) — run a tiny `rust/spikes/voice-npu/` before
  Phase 11a commits.** Theory checks out (ort + OpenVINO EP + NPU +
  reshape_input all exist); spike validates on actual hardware.
  Success criteria are listed above. ~2 sessions, out-of-workspace so
  no Cargo.toml dependency churn.
- [ ] **Q-E1 pin: `rmcp = "1.7"`, `MCP_SPEC_VERSION = "2025-11-25"`.**
  Rust SDK is Tier 2 → expect lag when 2026-07-28 lands. Per-extension
  compat: accept N and N−1, reject N+1, log clearly.
- [ ] **Phase 4 spec adjustment:** the master plan should reference the
  SDK Tier 2 status for Rust. If the 2026-07-28 deprecation of Roots /
  Sampling / Logging matters to any Wylde extension (Webcrawler,
  Wylde_Study) it's worth a one-line audit during Phase 4.
- [ ] **Phase 11a spec adjustment:** if the Q-V2 spike uses
  `openvino-rs` rather than `ort` for the NPU path, the library map in
  master-plan §11a (`ort` for both Kokoro and Whisper NPU) needs to
  split: `ort` for Kokoro, `openvino-rs` for Whisper NPU, `whisper-rs`
  for the CPU fallback. Three crates is still acceptable.
