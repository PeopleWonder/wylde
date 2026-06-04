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

/// Default location of the release **private** signing key, relative to
/// the repo root. Never committed (`keys/.gitignore`); the public half is
/// what gets embedded into `wylde-updater::PUBLIC_KEY`.
const DEFAULT_KEY_PATH: &str = "rust/crates/wylde-updater/keys/wylde-signing.key";

/// Env var an operator can set instead of passing `--key` every time.
const KEY_PATH_ENV: &str = "RSIGN_KEY_PATH";

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
        /// The binary asset to upload. Its `.minisig` sibling is uploaded
        /// alongside (created via `sign` first if missing).
        #[arg(long)]
        binary: PathBuf,
        /// Private key path for the sign-if-missing step. Falls back to
        /// `$RSIGN_KEY_PATH`, then the default in-repo `keys/` path.
        #[arg(long)]
        key: Option<PathBuf>,
        /// Target `owner/repo`. Defaults to the public updater repo.
        #[arg(long, default_value = DEFAULT_REPO)]
        repo: String,
        /// Release notes body. Defaults to a one-line auto message.
        #[arg(long)]
        notes: Option<String>,
        /// Print the `rsign`/`gh` commands that would run, without
        /// executing them. Use this to rehearse a release safely.
        #[arg(long)]
        dry_run: bool,
    },

    /// Compare a candidate public key against the one embedded in
    /// `wylde-updater`. Accepts either the base64 key line directly or a
    /// path to an `rsign` `.pub` file.
    VerifyPublicKey {
        /// The base64 public-key line, or a path to a `.pub` file.
        pubkey: String,
    },
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
        Cmd::Sign { binary, key } => {
            let key = resolve_key_path(key);
            let sig = sign(&binary, &key)?;
            println!("signed: {} -> {}", binary.display(), sig.display());
            Ok(())
        }
        Cmd::Publish {
            version,
            channel,
            binary,
            key,
            repo,
            notes,
            dry_run,
        } => publish(&version, channel, &binary, key, &repo, notes.as_deref(), dry_run),
        Cmd::VerifyPublicKey { pubkey } => verify_public_key(&pubkey),
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
        println!(
            "A signing key already exists at {}.",
            key_path.display()
        );
        match std::fs::read_to_string(&pub_path) {
            Ok(contents) => match extract_pubkey(&contents) {
                Some(line) => {
                    println!("\nPublic key (embed this in wylde-updater::PUBLIC_KEY):");
                    println!("{line}");
                }
                None => println!(
                    "(could not parse a key line out of {})",
                    pub_path.display()
                ),
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

/// Sign `binary` with `key`, returning the `.minisig` path written.
fn sign(binary: &Path, key: &Path) -> Result<PathBuf> {
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
    run(Command::new("rsign").args([
        "sign",
        "-s",
        &key.to_string_lossy(),
        "-x",
        &sig.to_string_lossy(),
        &binary.to_string_lossy(),
    ]))
    .context("rsign sign failed (is `rsign` installed and on PATH?)")?;
    Ok(sig)
}

fn publish(
    version: &str,
    channel: Channel,
    binary: &Path,
    key: Option<PathBuf>,
    repo: &str,
    notes: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    if !binary.exists() {
        bail!("binary to publish does not exist: {}", binary.display());
    }
    let key = resolve_key_path(key);
    let sig = sig_path_for(binary);

    // Sign-if-missing. In a dry run we only describe the signing step.
    if !sig.exists() {
        if dry_run {
            println!(
                "[dry-run] would sign: rsign sign -s {} -x {} {}",
                key.display(),
                sig.display(),
                binary.display()
            );
        } else {
            println!("No signature at {} — signing first…", sig.display());
            sign(binary, &key)?;
        }
    }

    let notes = notes
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Automated release {version} ({} channel).", channel_name(channel)));

    // gh release create <tag> <binary> <sig> --repo … --title … --notes …
    // [--prerelease]
    let mut args: Vec<String> = vec![
        "release".into(),
        "create".into(),
        version.into(),
        binary.to_string_lossy().into_owned(),
        sig.to_string_lossy().into_owned(),
        "--repo".into(),
        repo.into(),
        "--title".into(),
        version.into(),
        "--notes".into(),
        notes,
    ];
    if channel.is_prerelease() {
        args.push("--prerelease".into());
    }

    if dry_run {
        println!("[dry-run] would publish: gh {}", args.join(" "));
        return Ok(());
    }
    run(Command::new("gh").args(&args))
        .context("gh release create failed (is `gh` installed, authed, and the tag free?)")?;
    println!("published {version} to {repo}");
    Ok(())
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
    let candidate = extract_pubkey(&raw)
        .context("could not parse a base64 public-key line from the input")?;

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
    fn cli_parses_verify_public_key() {
        let cli = Cli::try_parse_from(["wylde-release", "verify-public-key", "RWQabc"])
            .expect("verify args parse");
        assert!(matches!(cli.command, Cmd::VerifyPublicKey { .. }));
    }
}
