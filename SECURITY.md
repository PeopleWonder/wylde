# Security Policy

Wylde is a local-first personal AI system that stores private, sometimes sensitive
data on the user's own machine — long-term memory, a personal knowledge graph, workspace
contents, and (in adjacent tooling) health-adjacent records. We take security reports
seriously and appreciate responsible disclosure.

## Reporting a vulnerability

**Please do not open a public issue for a security vulnerability.** Public disclosure before
a fix is available puts users at risk.

Instead, report it privately through **GitHub's private vulnerability reporting**:

> **[Report a vulnerability](https://github.com/PeopleWonder/wylde/security/advisories/new)**
> — repository **Security** tab → **Report a vulnerability**.

This opens a private advisory visible only to you and the maintainers. If you cannot use that
channel, open a minimal public issue that says only "security issue — requesting a private
contact channel" (no details), and a maintainer will follow up.

Please include, as far as you can:

- The affected component (service, GUI panel, installer/updater, gateway, VPN, etc.) and version.
- A description of the vulnerability and its impact.
- Steps to reproduce or a proof of concept.
- Any suggested remediation.

## Scope

Wylde runs on the user's machine and is **not yet hardened for untrusted multi-user or
internet-exposed deployment** (see the README security model). Reports are most valuable when they
concern the boundaries that *are* meant to hold, for example:

- The **Gateway** egress boundary (allowlist, kill switch, SSRF guard) and the inbound
  mobile-over-VPN trust tier.
- **Device pairing / device-gate** bearer tokens and permission tiers.
- **Encryption at rest** for sensitive stores (e.g. DPAPI-protected profile/secret data).
- The **self-updater** trust chain — release signature verification, asset/signature pairing, and
  channel routing (a malicious or spoofed release must not be installable).
- Local **IPC** (named pipes) trust assumptions, and any path where remote or untrusted input
  reaches a privileged action.
- Sandbox/jail escapes in the workspace `fs.*` verbs and tool execution.

Out of scope (known and documented, not vulnerabilities): rough alpha edges, the pre-1.0 "may break
between builds" contract, and the explicit not-yet-hardened-for-internet-exposure posture.

## Supported versions

Wylde is pre-1.0 and ships from a single stable line served by the updater's **Stable** channel.
Security fixes land on the current stable line (`0.2.x` and later) and the experimental line
(`develop` / Beta). Older builds are not separately maintained — the fix is "update to current."

| Version line | Supported |
|---|---|
| Current stable (`main`, latest `0.2.x`+) | ✅ |
| Experimental (`develop` / Beta channel) | ✅ (fixes land here first) |
| Older tagged builds | ❌ — update to current |

## Disclosure process

1. You report privately (above).
2. We confirm receipt and begin investigating; we'll keep you updated on progress.
3. We develop and verify a fix on the experimental line, then promote it through the release gate.
4. We publish the fixed release and, once users have had a reasonable window to update, disclose the
   advisory with credit to you (unless you prefer to remain anonymous).

Thank you for helping keep Wylde and its users safe.
