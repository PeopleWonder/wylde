//! `wylde-release` — sign + publish Wylde GUI builds for the Phase 12.5
//! self-updater (slice 3b).
//!
//! A small developer tool that runs on Aaron's release machine only. It
//! wraps two external CLIs:
//!
//!   * **`rsign`** (rsign2) — minisign keypair generation + detached
//!     signing. The shipped `wylde-updater` verifies these signatures
//!     against the one embedded public key, fail-closed.
//!   * **`gh`** — uploads the binary + its `.minisig` to GitHub Releases
//!     on `PeopleWonder/wylde`, the public repo the in-app updater polls.
//!
//! Nothing here is user-facing, so the ergonomics stay deliberately
//! plain: clap subcommands, `anyhow` error chains, and thin wrappers over
//! `std::process::Command`. The signing/publish *decisions* (path
//! resolution, the prerelease flag, signature-filename derivation) live
//! in pure helpers so they're unit-tested without a key or network.
//!
//! Subcommands:
//!   * `generate-key` — mint the release keypair (or report the existing
//!     one for embedding).
//!   * `sign <binary>` — produce `<binary>.minisig`.
//!   * `publish --version … --channel … --binary …` — sign-if-needed, then
//!     `gh release create`.
//!   * `verify-public-key <key>` — sanity-check a key against the one baked
//!     into `wylde-updater`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

mod bench;
mod host;
mod preflight;
mod receipt;
mod smoke;

/// Default location of the release **private** signing key, relative to
/// the repo root. Never committed (`keys/.gitignore`); the public half is
/// what gets embedded into `wylde-updater::PUBLIC_KEY`.
const DEFAULT_KEY_PATH: &str = "rust/crates/wylde-updater/keys/wylde-signing.key";

/// Env var an operator can set instead of passing `--key` every time.
const KEY_PATH_ENV: &str = "RSIGN_KEY_PATH";

/// Env var that turns on passwordless signing without the `-W` flag (set to
/// `1`/`true`/`yes`). The CLI flag still wins when explicitly given.
const PASSWORDLESS_ENV: &str = "RSIGN_PASSWORDLESS";

/// The public repo the in-app updater polls; `publish` uploads here.
const DEFAULT_REPO: &str = "PeopleWonder/wylde";

#[derive(Parser, Debug)]
#[command(
    name = "wylde-release",
    about = "Sign and publish Wylde GUI builds for the self-updater (dev-machine tool).",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Generate the minisign release keypair via `rsign generate`.
    ///
    /// If a key already exists at the target path this is informational:
    /// it prints the public key for embedding rather than clobbering the
    /// private key (regenerating would invalidate every shipped build).
    GenerateKey {
        /// Where to write the private key (public key is the sibling
        /// `.pub`). Defaults to the in-repo `keys/` path.
        #[arg(long)]
        key: Option<PathBuf>,
        /// Comment baked into the generated key files.
        #[arg(long, default_value = "Wylde release signing key")]
        comment: String,
    },

    /// Sign `<binary>`, writing the detached `<binary>.minisig`.
    Sign {
        /// Path to the binary to sign.
        binary: PathBuf,
        /// Private key path. Falls back to `$RSIGN_KEY_PATH`, then the
        /// default in-repo `keys/` path.
        #[arg(long)]
        key: Option<PathBuf>,
        /// The signing key is passwordless (generated with `rsign generate
        /// -W`). Passes `-W` to `rsign sign` so it never prompts on the
        /// console — required for the project's production key and for
        /// unattended release runs. `RSIGN_PASSWORDLESS=1` sets it too.
        #[arg(short = 'W', long)]
        passwordless: bool,
    },

    /// Sign (if needed) and publish a release to GitHub Releases.
    ///
    /// Requires `gh` authenticated against the target repo. Beta-channel
    /// releases are published as GitHub pre-releases so the updater's
    /// stable channel skips them.
    Publish {
        /// Release tag, e.g. `v0.1.0-alpha.1`. Used verbatim as the tag.
        #[arg(long)]
        version: String,
        /// Release channel. `beta` ⇒ GitHub pre-release.
        #[arg(long, value_enum)]
        channel: Channel,
        /// The primary binary asset to upload (the bare signed
        /// `wylde-gui-<target>.exe` the self-updater consumes). Its
        /// `.minisig` sibling is uploaded alongside (created via `sign`
        /// first if missing).
        #[arg(long)]
        binary: PathBuf,
        /// Additional asset(s) to attach to the same release, e.g. the NSIS
        /// installer (`WyldeSetup-<version>.exe`). Repeatable. Each one's
        /// `.minisig` sibling is uploaded alongside it too (signed first if
        /// missing), so a single `publish` can carry both the updater binary
        /// and the human-install installer.
        #[arg(long = "extra-asset")]
        extra_asset: Vec<PathBuf>,
        /// Private key path for the sign-if-missing step. Falls back to
        /// `$RSIGN_KEY_PATH`, then the default in-repo `keys/` path.
        #[arg(long)]
        key: Option<PathBuf>,
        /// The signing key is passwordless — passes `-W` to any
        /// sign-if-missing step (see `sign --passwordless`).
        #[arg(short = 'W', long)]
        passwordless: bool,
        /// Target `owner/repo`. Defaults to the public updater repo.
        #[arg(long, default_value = DEFAULT_REPO)]
        repo: String,
        /// Release notes body. Defaults to a one-line auto message.
        /// Ignored when `--notes-file` is given.
        #[arg(long)]
        notes: Option<String>,
        /// Read the release notes body from a markdown file (wins over
        /// `--notes`). Lets a multi-section changelog be published without
        /// cramming it onto the command line.
        #[arg(long = "notes-file")]
        notes_file: Option<PathBuf>,
        /// Print the `rsign`/`gh` commands that would run, without
        /// executing them. Use this to rehearse a release safely.
        #[arg(long)]
        dry_run: bool,
        /// **Deliberate escape hatch.** Skip the preflight-receipt gate. The
        /// gate exists to stop a build shipping unverified (the exact "shipped
        /// broken" failure), so this prints a loud warning and should be used
        /// only when you know precisely why. `--dry-run` never checks the
        /// receipt (it's a rehearsal).
        #[arg(long)]
        no_preflight_receipt: bool,
        /// Repo root to look for `preflight-receipt.json` in. Defaults to the
        /// git top-level of the current directory.
        #[arg(long)]
        repo_root: Option<PathBuf>,
    },

    /// Compare a candidate public key against the one embedded in
    /// `wylde-updater`. Accepts either the base64 key line directly or a
    /// path to an `rsign` `.pub` file.
    VerifyPublicKey {
        /// The base64 public-key line, or a path to a `.pub` file.
        pubkey: String,
    },

    /// Run the benchmark suite, compare against the committed baseline, and
    /// **fail on a regression past the per-metric threshold**. This is the
    /// standalone regression gate; `preflight` runs it as one of its steps.
    ///
    /// The suite drives live Ollama (reasoning arms) and reads the live index
    /// (lexical eval), so it runs on the release machine, not CI.
    Bench(preflight::BenchArgs),

    /// Run the full local preflight (version-consistency G7 + the benchmark
    /// gate, plus an optional artifact build) and write a **receipt** bound to
    /// the current commit. `publish` refuses without a green, current receipt.
    Preflight(preflight::PreflightArgs),

    /// Run the **L2 cold-start smoke + L3 service-health + L5 shipped-config**
    /// launch-and-verify gate on its own (without the benchmark/receipt
    /// machinery). Launches the shipped daemon + GUI from a neutral cwd, then
    /// asserts the assembled system is actually functional — services
    /// discovered, VRAM broker up, Ollama has the models, **Memgraph has real
    /// data**, the **reasoning tier is shipped OFF**, RAG answers, a chat turn
    /// completes, a memory round-trips. Prints each check's verdict and exits
    /// non-zero on any failure. `preflight --launch` runs the same checks and
    /// folds them into the receipt.
    Smoke(preflight::SmokeArgs),
}

/// Release channel mirror of `wylde_updater::Channel`. Local copy so the
/// CLI's `--channel` parsing doesn't leak the updater's enum into the arg
/// layer; the only behavioural bit we need is "does this map to a GitHub
/// pre-release".
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Channel {
    Stable,
    Beta,
}

impl Channel {
    /// `true` when releases on this channel must be marked GitHub
    /// pre-releases (so the updater's stable channel skips them).
    fn is_prerelease(self) -> bool {
        matches!(self, Channel::Beta)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::GenerateKey { key, comment } => generate_key(key, &comment),
        Cmd::Sign {
            binary,
            key,
            passwordless,
        } => {
            let key = resolve_key_path(key);
            let sig = sign(&binary, &key, resolve_passwordless(passwordless))?;
            println!("signed: {} -> {}", binary.display(), sig.display());
            Ok(())
        }
        Cmd::Publish {
            version,
            channel,
            binary,
            extra_asset,
            key,
            passwordless,
            repo,
            notes,
            notes_file,
            dry_run,
            no_preflight_receipt,
            repo_root,
        } => publish(
            &version,
            channel,
            &binary,
            &extra_asset,
            key,
            resolve_passwordless(passwordless),
            &repo,
            notes.as_deref(),
            notes_file.as_deref(),
            dry_run,
            no_preflight_receipt,
            repo_root,
        ),
        Cmd::VerifyPublicKey { pubkey } => verify_public_key(&pubkey),
        Cmd::Bench(args) => preflight::run_bench(args),
        Cmd::Preflight(args) => preflight::run_preflight(args),
        Cmd::Smoke(args) => preflight::run_smoke(args),
    }
}

// ── Pure helpers (unit-tested) ───────────────────────────────────────

/// Resolve the private-key path: explicit `--key` wins, then
/// `$RSIGN_KEY_PATH`, then the in-repo default.
fn resolve_key_path(flag: Option<PathBuf>) -> PathBuf {
    if let Some(p) = flag {
        return p;
    }
    if let Ok(env) = std::env::var(KEY_PATH_ENV) {
        if !env.trim().is_empty() {
            return PathBuf::from(env);
        }
    }
    PathBuf::from(DEFAULT_KEY_PATH)
}

/// The detached-signature path for a binary: `<binary>.minisig`.
///
/// `rsign`'s own default, made explicit so the `publish` step knows
/// exactly which file to look for and upload.
fn sig_path_for(binary: &Path) -> PathBuf {
    let mut name = binary.as_os_str().to_owned();
    name.push(".minisig");
    PathBuf::from(name)
}

/// The public-key path that pairs with a private-key path: same stem with
/// a `.pub` extension (`rsign`'s convention).
fn pub_path_for(key: &Path) -> PathBuf {
    key.with_extension("pub")
}

/// Extract the base64 key line from raw key material.
///
/// An `rsign` `.pub` file is two lines — an `untrusted comment:` header
/// then the base64 key. `wylde-updater` embeds only that second line, so
/// when given a multi-line blob (or a file's contents) we take the last
/// non-empty, non-comment line. A bare single line passes through trimmed.
fn extract_pubkey(raw: &str) -> Option<String> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .rfind(|l| !l.to_ascii_lowercase().starts_with("untrusted comment"))
        .map(str::to_owned)
}

// ── Subcommand bodies ────────────────────────────────────────────────

fn generate_key(key: Option<PathBuf>, comment: &str) -> Result<()> {
    let key_path = resolve_key_path(key);
    let pub_path = pub_path_for(&key_path);

    if key_path.exists() {
        // Don't regenerate — a fresh keypair would orphan every already
        // shipped, already-signed build. Report the existing public key
        // for embedding instead.
        println!("A signing key already exists at {}.", key_path.display());
        match std::fs::read_to_string(&pub_path) {
            Ok(contents) => match extract_pubkey(&contents) {
                Some(line) => {
                    println!("\nPublic key (embed this in wylde-updater::PUBLIC_KEY):");
                    println!("{line}");
                }
                None => println!("(could not parse a key line out of {})", pub_path.display()),
            },
            Err(_) => println!(
                "(no matching public key at {} — pass the right --key, or \
                 re-run `rsign generate` manually)",
                pub_path.display()
            ),
        }
        println!(
            "\nRefusing to regenerate (that would invalidate every shipped build). \
             Delete the key files by hand if you really intend to rotate."
        );
        return Ok(());
    }

    // Fresh generation. rsign prompts interactively for a password — this
    // is a human-driven, dev-machine step, so that's fine.
    if let Some(parent) = key_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating key directory {}", parent.display()))?;
    }
    println!(
        "Generating a new signing keypair at {} (rsign will prompt for a password)…",
        key_path.display()
    );
    run(Command::new("rsign").args([
        "generate",
        "-s",
        &key_path.to_string_lossy(),
        "-p",
        &pub_path.to_string_lossy(),
        "-c",
        comment,
    ]))
    .context("rsign generate failed (is `rsign` installed and on PATH?)")?;

    if let Ok(contents) = std::fs::read_to_string(&pub_path) {
        if let Some(line) = extract_pubkey(&contents) {
            println!("\nPublic key (embed this in wylde-updater::PUBLIC_KEY):");
            println!("{line}");
        }
    }
    Ok(())
}

/// Resolve whether to sign passwordlessly: the `-W`/`--passwordless` flag
/// wins; otherwise honour `RSIGN_PASSWORDLESS` (`1`/`true`/`yes`).
fn resolve_passwordless(flag: bool) -> bool {
    if flag {
        return true;
    }
    matches!(
        std::env::var(PASSWORDLESS_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

/// Sign `binary` with `key`, returning the `.minisig` path written.
///
/// `passwordless` adds `-W` so `rsign` never prompts on the console — needed
/// for the project's passwordless production key and unattended runs.
fn sign(binary: &Path, key: &Path, passwordless: bool) -> Result<PathBuf> {
    if !binary.exists() {
        bail!("binary to sign does not exist: {}", binary.display());
    }
    if !key.exists() {
        bail!(
            "signing key not found at {} (set {} or pass --key)",
            key.display(),
            KEY_PATH_ENV
        );
    }
    let sig = sig_path_for(binary);
    let mut args: Vec<String> = vec!["sign".into()];
    if passwordless {
        args.push("-W".into());
    }
    args.extend([
        "-s".into(),
        key.to_string_lossy().into_owned(),
        "-x".into(),
        sig.to_string_lossy().into_owned(),
        binary.to_string_lossy().into_owned(),
    ]);
    run(Command::new("rsign").args(&args))
        .context("rsign sign failed (is `rsign` installed and on PATH?)")?;
    Ok(sig)
}

#[allow(clippy::too_many_arguments)]
fn publish(
    version: &str,
    channel: Channel,
    binary: &Path,
    extra_assets: &[PathBuf],
    key: Option<PathBuf>,
    passwordless: bool,
    repo: &str,
    notes: Option<&str>,
    notes_file: Option<&Path>,
    dry_run: bool,
    no_preflight_receipt: bool,
    repo_root: Option<PathBuf>,
) -> Result<()> {
    if !binary.exists() {
        bail!("binary to publish does not exist: {}", binary.display());
    }

    // ── The preflight-receipt gate (enforcement-matrix row 14). Refuse to
    // publish a build whose running system was never verified. Skipped for a
    // dry-run rehearsal, or via the deliberate, loud `--no-preflight-receipt`.
    if dry_run {
        println!("[dry-run] (skipping preflight-receipt gate — rehearsal only)");
    } else if no_preflight_receipt {
        eprintln!(
            "⚠️  --no-preflight-receipt: shipping WITHOUT a verified preflight receipt.\n\
             ⚠️  This is the exact gate that stops a broken build from shipping. Proceed only if\n\
             ⚠️  you know why the normal `wylde-release preflight` path was bypassed."
        );
    } else {
        preflight::enforce_receipt_for_publish(repo_root.as_deref(), version)
            .context("preflight-receipt gate")?;
        println!("✓ preflight receipt validates for {version} at HEAD.");
    }

    let key = resolve_key_path(key);

    // Every uploadable asset is the file itself plus its `.minisig`. The
    // primary binary leads; any `--extra-asset` (e.g. the installer) follows.
    // Each gets signed-if-missing so a release never ships an unsigned asset.
    let mut asset_paths: Vec<PathBuf> = Vec::new();
    for asset in std::iter::once(binary).chain(extra_assets.iter().map(PathBuf::as_path)) {
        if !asset.exists() {
            bail!("asset to publish does not exist: {}", asset.display());
        }
        let sig = sig_path_for(asset);
        // Sign-if-missing. In a dry run we only describe the signing step.
        if !sig.exists() {
            if dry_run {
                println!(
                    "[dry-run] would sign: rsign sign {}-s {} -x {} {}",
                    if passwordless { "-W " } else { "" },
                    key.display(),
                    sig.display(),
                    asset.display()
                );
            } else {
                println!("No signature at {} — signing first…", sig.display());
                sign(asset, &key, passwordless)?;
            }
        }
        asset_paths.push(asset.to_path_buf());
        asset_paths.push(sig);
    }

    let notes = resolve_notes(notes, notes_file, version, channel)?;
    // Changelog gate: a real release must carry real notes (fail-closed).
    // Exempt only for a --dry-run rehearsal.
    enforce_publishable_notes(&notes, dry_run).context("changelog gate")?;
    if !dry_run {
        println!("✓ changelog gate: release notes are present and non-placeholder.");
    }

    // gh release create <tag> <asset>… --repo … --title … --notes … [--prerelease]
    let mut args: Vec<String> = vec!["release".into(), "create".into(), version.into()];
    for asset in &asset_paths {
        args.push(asset.to_string_lossy().into_owned());
    }
    args.extend([
        "--repo".into(),
        repo.into(),
        "--title".into(),
        version.into(),
        "--notes".into(),
        notes,
    ]);
    if channel.is_prerelease() {
        args.push("--prerelease".into());
    }

    if dry_run {
        println!("[dry-run] would publish: gh {}", args.join(" "));
        return Ok(());
    }
    run(Command::new("gh").args(&args))
        .context("gh release create failed (is `gh` installed, authed, and the tag free?)")?;
    println!(
        "published {version} to {repo} ({} asset file(s))",
        asset_paths.len()
    );
    Ok(())
}

/// Prefix of the one-line rehearsal placeholder that [`resolve_notes`]
/// synthesises when no real notes are supplied. The publish gate
/// ([`enforce_publishable_notes`]) refuses any release whose notes start
/// with this — a real stable/experimental release must carry a real
/// changelog, never the auto-message.
const AUTO_NOTES_PREFIX: &str = "Automated release ";

/// Resolve the release-notes body: `--notes-file` wins (read from disk),
/// then `--notes`, then a one-line auto message. The auto message is only
/// legitimate for a `--dry-run` rehearsal; a real publish is gated by
/// [`enforce_publishable_notes`] so it can never ship.
fn resolve_notes(
    notes: Option<&str>,
    notes_file: Option<&Path>,
    version: &str,
    channel: Channel,
) -> Result<String> {
    if let Some(path) = notes_file {
        return std::fs::read_to_string(path)
            .with_context(|| format!("reading notes file {}", path.display()));
    }
    Ok(notes.map(str::to_owned).unwrap_or_else(|| {
        format!(
            "{AUTO_NOTES_PREFIX}{version} ({} channel).",
            channel_name(channel)
        )
    }))
}

/// Whether `notes` are a real changelog rather than empty or the rehearsal
/// placeholder. Pure so the publish gate is unit-tested without `gh`.
fn notes_are_publishable(notes: &str) -> bool {
    let trimmed = notes.trim();
    !trimmed.is_empty() && !trimmed.starts_with(AUTO_NOTES_PREFIX)
}

/// The changelog gate for `publish` (enforcement-matrix companion to the
/// preflight-receipt gate): a real release must carry real release notes.
/// Fails closed — an empty `--notes-file`, no notes at all (the synthesised
/// auto-message), or the placeholder is refused. A `--dry-run` rehearsal is
/// exempt (it never reaches GitHub), so the auto-message can still be
/// previewed.
fn enforce_publishable_notes(notes: &str, dry_run: bool) -> Result<()> {
    if dry_run || notes_are_publishable(notes) {
        return Ok(());
    }
    bail!(
        "refusing to publish without a real changelog: the release notes are empty or the \
         one-line auto-message. A stable or experimental release must ship real notes — pass \
         `--notes-file <path>` pointing at this version's CHANGELOG.md section (or `--notes`). \
         The auto-message is only for `--dry-run` rehearsals."
    )
}

fn channel_name(channel: Channel) -> &'static str {
    match channel {
        Channel::Stable => "stable",
        Channel::Beta => "beta",
    }
}

fn verify_public_key(input: &str) -> Result<()> {
    // Accept either a path to a `.pub` file or the key string itself.
    let raw = if Path::new(input).is_file() {
        std::fs::read_to_string(input)
            .with_context(|| format!("reading public-key file {input}"))?
    } else {
        input.to_owned()
    };
    let candidate =
        extract_pubkey(&raw).context("could not parse a base64 public-key line from the input")?;

    if !wylde_updater::has_signing_key() {
        // The shipped build still carries the placeholder, so there is
        // nothing real to compare against yet. Report rather than claim a
        // (mis)match.
        println!(
            "wylde-updater has NO production key embedded yet (still the placeholder).\n\
             Candidate key:\n{candidate}\n\n\
             Embed this line in wylde-updater::PUBLIC_KEY and rebuild, then re-run \
             to confirm the match."
        );
        return Ok(());
    }

    if candidate.trim() == wylde_updater::PUBLIC_KEY.trim() {
        println!("MATCH — the candidate key is the one embedded in wylde-updater.");
        Ok(())
    } else {
        bail!(
            "MISMATCH — candidate does not equal the embedded key.\n  candidate: {candidate}\n  embedded:  {}",
            wylde_updater::PUBLIC_KEY
        );
    }
}

// ── Process plumbing ─────────────────────────────────────────────────

/// Run a command to completion, mapping a non-zero exit (or a failure to
/// spawn) into an `anyhow` error. stdout/stderr are inherited so the
/// wrapped tool's own output (and `rsign`'s password prompt) reach the
/// terminal directly.
fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("spawning {:?}", cmd.get_program()))?;
    if !status.success() {
        bail!("{:?} exited with {}", cmd.get_program(), status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sig_path_appends_minisig() {
        assert_eq!(
            sig_path_for(Path::new("wylde-gui.exe")),
            PathBuf::from("wylde-gui.exe.minisig")
        );
        // A pathful binary keeps its directory.
        assert_eq!(
            sig_path_for(Path::new("dist/wylde-gui")),
            PathBuf::from("dist/wylde-gui.minisig")
        );
    }

    #[test]
    fn pub_path_swaps_extension() {
        assert_eq!(
            pub_path_for(Path::new("keys/wylde-signing.key")),
            PathBuf::from("keys/wylde-signing.pub")
        );
    }

    #[test]
    fn beta_channel_is_a_prerelease_stable_is_not() {
        assert!(Channel::Beta.is_prerelease());
        assert!(!Channel::Stable.is_prerelease());
    }

    #[test]
    fn resolve_key_prefers_flag_over_env_and_default() {
        let flag = PathBuf::from("/explicit/key");
        assert_eq!(resolve_key_path(Some(flag.clone())), flag);
    }

    #[test]
    fn resolve_key_falls_back_to_default_without_flag_or_env() {
        // The test process must not have the env var set for this to be
        // deterministic; the CI/dev shell doesn't set RSIGN_KEY_PATH.
        std::env::remove_var(KEY_PATH_ENV);
        assert_eq!(resolve_key_path(None), PathBuf::from(DEFAULT_KEY_PATH));
    }

    #[test]
    fn extract_pubkey_takes_the_key_line_not_the_comment() {
        // Shape of an rsign `.pub` file.
        let blob = "untrusted comment: minisign public key 1234ABCD\nRWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\n";
        assert_eq!(
            extract_pubkey(blob).as_deref(),
            Some("RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3")
        );
    }

    #[test]
    fn extract_pubkey_passes_through_a_bare_line() {
        assert_eq!(
            extract_pubkey("  RWQbarekey  ").as_deref(),
            Some("RWQbarekey")
        );
    }

    #[test]
    fn extract_pubkey_rejects_empty_input() {
        assert_eq!(extract_pubkey("\n  \n"), None);
    }

    #[test]
    fn channel_name_round_trips() {
        assert_eq!(channel_name(Channel::Stable), "stable");
        assert_eq!(channel_name(Channel::Beta), "beta");
    }

    /// The clap layer must parse — a derive typo would otherwise only
    /// surface at runtime.
    #[test]
    fn cli_parses_publish_subcommand() {
        let cli = Cli::try_parse_from([
            "wylde-release",
            "publish",
            "--version",
            "v0.1.0-alpha.1",
            "--channel",
            "beta",
            "--binary",
            "wylde-gui.exe",
            "--dry-run",
        ])
        .expect("publish args parse");
        match cli.command {
            Cmd::Publish {
                version,
                channel,
                dry_run,
                ..
            } => {
                assert_eq!(version, "v0.1.0-alpha.1");
                assert_eq!(channel, Channel::Beta);
                assert!(dry_run);
            }
            other => panic!("expected Publish, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_publish_with_extra_assets() {
        // Both the bare updater binary and the installer in one publish.
        let cli = Cli::try_parse_from([
            "wylde-release",
            "publish",
            "--version",
            "v0.1.0-alpha.1",
            "--channel",
            "beta",
            "--binary",
            "wylde-gui-x86_64-pc-windows-msvc.exe",
            "--extra-asset",
            "WyldeSetup-0.1.0-alpha.1.exe",
            "--notes-file",
            "RELEASE_NOTES.md",
            "--dry-run",
        ])
        .expect("publish args parse");
        match cli.command {
            Cmd::Publish {
                binary,
                extra_asset,
                notes_file,
                ..
            } => {
                assert_eq!(
                    binary,
                    PathBuf::from("wylde-gui-x86_64-pc-windows-msvc.exe")
                );
                assert_eq!(
                    extra_asset,
                    vec![PathBuf::from("WyldeSetup-0.1.0-alpha.1.exe")]
                );
                assert_eq!(notes_file, Some(PathBuf::from("RELEASE_NOTES.md")));
            }
            other => panic!("expected Publish, got {other:?}"),
        }
    }

    #[test]
    fn resolve_passwordless_flag_wins_then_env() {
        // Explicit flag is always honoured.
        assert!(resolve_passwordless(true));
        // Without the flag, the env var decides.
        std::env::remove_var(PASSWORDLESS_ENV);
        assert!(!resolve_passwordless(false));
        std::env::set_var(PASSWORDLESS_ENV, "1");
        assert!(resolve_passwordless(false));
        std::env::set_var(PASSWORDLESS_ENV, "no");
        assert!(!resolve_passwordless(false));
        std::env::remove_var(PASSWORDLESS_ENV);
    }

    #[test]
    fn resolve_notes_prefers_explicit_string_then_auto() {
        // Explicit --notes string passes through.
        assert_eq!(
            resolve_notes(Some("hand notes"), None, "v1.0.0", Channel::Beta).unwrap(),
            "hand notes"
        );
        // No notes at all ⇒ the one-line auto message naming the channel.
        let auto = resolve_notes(None, None, "v1.0.0", Channel::Stable).unwrap();
        assert!(auto.contains("v1.0.0"));
        assert!(auto.contains("stable"));
    }

    #[test]
    fn changelog_gate_rejects_empty_and_placeholder_notes() {
        // Real notes pass.
        assert!(notes_are_publishable(
            "## 0.2.0\n- a real, user-facing change"
        ));
        // Empty / whitespace-only is refused.
        assert!(!notes_are_publishable(""));
        assert!(!notes_are_publishable("   \n\t "));
        // The synthesised rehearsal placeholder is refused — this is the
        // silent-fallback hole being closed.
        let auto = resolve_notes(None, None, "0.2.0", Channel::Stable).unwrap();
        assert!(
            !notes_are_publishable(&auto),
            "the auto-message must never count as a real changelog"
        );
    }

    #[test]
    fn changelog_gate_fails_closed_for_a_real_publish_but_exempts_dry_run() {
        let auto = resolve_notes(None, None, "0.2.0", Channel::Beta).unwrap();
        // A real publish (dry_run = false) with no real notes is refused...
        assert!(
            enforce_publishable_notes(&auto, false).is_err(),
            "a real release without real notes must fail closed"
        );
        // ...the same rehearsal (dry_run = true) is allowed to preview it...
        assert!(enforce_publishable_notes(&auto, true).is_ok());
        // ...and a real publish WITH real notes passes.
        assert!(enforce_publishable_notes("## 0.2.0\n- real notes", false).is_ok());
    }

    #[test]
    fn resolve_notes_reads_file_over_string() {
        // No tempfile dep on this standalone crate — use the OS temp dir
        // with a pid-unique name so parallel test runs don't collide.
        let path =
            std::env::temp_dir().join(format!("wylde-release-notes-{}.md", std::process::id()));
        std::fs::write(&path, "# From file\n").unwrap();
        // notes-file wins over an also-present --notes string.
        let resolved =
            resolve_notes(Some("ignored"), Some(&path), "v1.0.0", Channel::Beta).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(resolved, "# From file\n");
    }

    #[test]
    fn cli_parses_verify_public_key() {
        let cli = Cli::try_parse_from(["wylde-release", "verify-public-key", "RWQabc"])
            .expect("verify args parse");
        assert!(matches!(cli.command, Cmd::VerifyPublicKey { .. }));
    }
}
