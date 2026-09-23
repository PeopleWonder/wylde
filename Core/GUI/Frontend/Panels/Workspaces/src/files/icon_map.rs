//! File-tree icon resolution (visual-polish F1).
//!
//! Maps a directory [`Entry`](super::ipc::Entry) to the icon it should render
//! with, through a 4-level override config (most-specific wins):
//!
//!   1. `by_file[name]`     — exact filename (e.g. `Cargo.toml`)
//!   2. `by_extension[ext]` — file extension, no dot (e.g. `rs`)
//!   3. nearest-ancestor `by_folder` rule with `applies_to_children` — a file
//!      inside `target/` inherits the `target` rule, closest ancestor wins
//!   4. `default.file` / `default.folder`
//!
//! Directories resolve against `by_file` then `by_folder` (their own rule, e.g.
//! `.git`) then `default.folder` (`open_icon` when expanded).
//!
//! The config is a typed struct with a rich built-in [`Default`] (the single
//! source of truth, no I/O), optionally **overlaid** by a `file_tree_icons:`
//! block in the Visual Style YAML — sparse overrides win over the defaults
//! (see [`IconConfig::load`]). gpui paints `svg()` as a mask tinted by
//! `text_color`, so the per-type colour is a config concern, not the asset:
//! each rule may carry a `color` (a `$theme_token` or `#hex`); `None` means
//! "inherit the row's text colour" — which keeps icons in step with the
//! white-font hierarchy and the ignored/secondary dimming.

use std::collections::HashMap;
use std::sync::OnceLock;

use gpui::Rgba;
use serde::Deserialize;

use super::ipc::{Entry, Kind};

/// The process-wide icon config, parsed once from the embedded Visual Style
/// YAML (defaults overlaid with any `file_tree_icons:` overrides). The render
/// path calls this per row, so the parse must happen exactly once.
pub fn config() -> &'static IconConfig {
    static CFG: OnceLock<IconConfig> = OnceLock::new();
    CFG.get_or_init(IconConfig::load)
}

/// A resolved icon to render: the asset key for `svg().path(...)` and an
/// optional explicit tint (`None` → inherit the row's text colour).
#[derive(Clone, Debug, PartialEq)]
pub struct IconSpec {
    /// Short icon name, e.g. `"code"`. Use [`IconSpec::asset_path`] for the
    /// `svg().path(...)` key.
    pub icon: String,
    /// Resolved tint, or `None` to inherit the row's text colour.
    pub tint: Option<Rgba>,
}

impl IconSpec {
    /// The AssetSource key the embedded bundle resolves (`F0`).
    pub fn asset_path(&self) -> String {
        format!("icons/file-tree/{}.svg", self.icon)
    }
}

/// One icon rule: which glyph, and an optional colour spec.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct IconRule {
    pub icon: String,
    #[serde(default)]
    pub color: Option<String>,
}

impl IconRule {
    fn new(icon: &str) -> Self {
        Self {
            icon: icon.to_owned(),
            color: None,
        }
    }
    fn colored(icon: &str, color: &str) -> Self {
        Self {
            icon: icon.to_owned(),
            color: Some(color.to_owned()),
        }
    }
}

/// A folder rule: like [`IconRule`] but with an `open_icon` for the expanded
/// state and `applies_to_children` so a container folder (e.g. `target`) tints
/// its descendants too.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct FolderRule {
    pub icon: String,
    #[serde(default)]
    pub open_icon: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    /// When true, descendant files with no more-specific rule inherit this one.
    #[serde(default)]
    pub applies_to_children: bool,
}

impl FolderRule {
    fn folder() -> Self {
        Self {
            icon: "folder".to_owned(),
            open_icon: Some("folder-open".to_owned()),
            color: None,
            applies_to_children: false,
        }
    }
    /// A container folder that also tints its children (e.g. `target`).
    fn container(icon: &str) -> Self {
        Self {
            icon: icon.to_owned(),
            open_icon: None,
            color: None,
            applies_to_children: true,
        }
    }
    fn named(icon: &str) -> Self {
        Self {
            icon: icon.to_owned(),
            open_icon: None,
            color: None,
            applies_to_children: false,
        }
    }
}

/// The default file/folder fallbacks.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Defaults {
    pub file: IconRule,
    pub folder: FolderRule,
}

/// The full icon config. Built-in [`Default`] is the authoritative source;
/// [`IconConfig::load`] overlays optional YAML overrides on top.
#[derive(Clone, Debug, PartialEq)]
pub struct IconConfig {
    pub default: Defaults,
    pub by_extension: HashMap<String, IconRule>,
    pub by_folder: HashMap<String, FolderRule>,
    pub by_file: HashMap<String, IconRule>,
}

impl Default for IconConfig {
    fn default() -> Self {
        // Two upstream glyph sets back these names (see the assets `ATTRIBUTION`
        // and `Core/GUI/Shell/src/assets.rs::FILE_TREE_ICONS`):
        //   • Lucide (ISC)  — category / UI / generic: file, folder,
        //     folder-open, code, doc, config, data, image, lock, git, package,
        //     book.
        //   • Seti-UI (MIT) — per-language: rust, python, typescript, react,
        //     javascript, go, c, cpp, c-sharp, java, kotlin, swift, ruby, php,
        //     scala, clojure, elixir, haskell, lua, dart, r, vue, svelte, html,
        //     css, sass, shell, powershell, json, yaml, markdown, xml, zig,
        //     perl, ocaml, julia, nim, elm, graphql, terraform, docker,
        //     makefile.
        // Extensions resolve to a per-language glyph where Seti ships one, and
        // fall through to a Lucide category otherwise; `code` is the generic
        // source-file fallback.
        let mut by_extension: HashMap<String, IconRule> = HashMap::new();

        // ── Per-language (Seti) — one glyph per language family ──
        let lang: &[(&str, &[&str])] = &[
            ("rust", &["rs"]),
            ("python", &["py", "pyi", "pyw"]),
            ("typescript", &["ts", "mts", "cts"]),
            ("react", &["tsx", "jsx"]),
            ("javascript", &["js", "mjs", "cjs"]),
            ("go", &["go"]),
            ("c", &["c", "h"]),
            ("cpp", &["cc", "cpp", "cxx", "hpp", "hh", "hxx"]),
            ("c-sharp", &["cs"]),
            ("java", &["java"]),
            ("kotlin", &["kt", "kts"]),
            ("swift", &["swift"]),
            ("ruby", &["rb"]),
            ("php", &["php"]),
            ("scala", &["scala", "sc", "sbt"]),
            ("clojure", &["clj", "cljs", "cljc", "edn"]),
            ("elixir", &["ex", "exs"]),
            ("haskell", &["hs", "lhs"]),
            ("lua", &["lua"]),
            ("dart", &["dart"]),
            ("r", &["r"]),
            ("vue", &["vue"]),
            ("svelte", &["svelte"]),
            ("html", &["html", "htm", "xhtml"]),
            ("css", &["css"]),
            ("sass", &["scss", "sass"]),
            ("shell", &["sh", "bash", "zsh", "fish"]),
            ("powershell", &["ps1", "psm1", "psd1"]),
            ("json", &["json", "jsonc", "ndjson", "json5"]),
            ("yaml", &["yaml", "yml"]),
            ("markdown", &["md", "markdown", "mdx"]),
            ("xml", &["xml", "xsd", "xsl", "xslt"]),
            ("zig", &["zig"]),
            ("perl", &["pl", "pm", "perl"]),
            ("ocaml", &["ml", "mli"]),
            ("julia", &["jl"]),
            ("nim", &["nim", "nims"]),
            ("elm", &["elm"]),
            ("graphql", &["graphql", "gql"]),
            ("terraform", &["tf", "tfvars", "hcl"]),
        ];
        for (icon, exts) in lang {
            for e in *exts {
                by_extension.insert((*e).to_owned(), IconRule::new(icon));
            }
        }

        // ── Category fallbacks (Lucide) for everything without a language glyph ──
        for e in [
            "txt", "text", "rst", "adoc", "asciidoc", "org", "rtf", "log",
        ] {
            by_extension.insert(e.to_owned(), IconRule::new("doc"));
        }
        for e in [
            "toml",
            "ini",
            "cfg",
            "conf",
            "config",
            "env",
            "properties",
            "editorconfig",
        ] {
            by_extension.insert(e.to_owned(), IconRule::new("config"));
        }
        for e in [
            "csv", "tsv", "parquet", "sql", "db", "sqlite", "sqlite3", "arrow",
        ] {
            by_extension.insert(e.to_owned(), IconRule::new("data"));
        }
        for e in [
            "png", "jpg", "jpeg", "gif", "svg", "webp", "ico", "bmp", "avif", "tiff",
        ] {
            by_extension.insert(e.to_owned(), IconRule::new("image"));
        }
        // Remaining source-ish extensions with no dedicated Seti glyph → generic `code`.
        for e in [
            "vala", "groovy", "gradle", "cmake", "asm", "s", "v", "sv", "vhdl", "coffee", "d",
            "f90", "f95", "fs", "fsx", "pas", "rkt", "scm", "lisp", "el", "tcl", "awk",
        ] {
            by_extension.insert(e.to_owned(), IconRule::new("code"));
        }
        by_extension.insert("lock".to_owned(), IconRule::new("lock"));

        let mut by_folder: HashMap<String, FolderRule> = HashMap::new();
        by_folder.insert(".git".to_owned(), FolderRule::named("git"));
        for f in [
            "target",
            "node_modules",
            "dist",
            "build",
            ".venv",
            "venv",
            "__pycache__",
            "vendor",
            ".cargo",
            "out",
        ] {
            by_folder.insert(f.to_owned(), FolderRule::container("package"));
        }

        let mut by_file: HashMap<String, IconRule> = HashMap::new();
        for f in [
            "Cargo.lock",
            "package-lock.json",
            "yarn.lock",
            "poetry.lock",
            "Pipfile.lock",
            "pnpm-lock.yaml",
            "Gemfile.lock",
            "composer.lock",
        ] {
            by_file.insert(f.to_owned(), IconRule::new("lock"));
        }
        for f in ["README.md", "README", "README.txt", "README.rst"] {
            by_file.insert(f.to_owned(), IconRule::colored("book", "$brand_light"));
        }
        for f in [
            "Cargo.toml",
            "package.json",
            "pyproject.toml",
            "Gemfile",
            "go.mod",
            "pom.xml",
        ] {
            by_file.insert(f.to_owned(), IconRule::new("package"));
        }
        // Exact-name tool files that carry their own per-tool Seti glyph.
        for f in ["Dockerfile", ".dockerignore"] {
            by_file.insert(f.to_owned(), IconRule::new("docker"));
        }
        for f in ["Makefile", "makefile", "GNUmakefile"] {
            by_file.insert(f.to_owned(), IconRule::new("makefile"));
        }

        IconConfig {
            default: Defaults {
                file: IconRule::new("file"),
                folder: FolderRule::folder(),
            },
            by_extension,
            by_folder,
            by_file,
        }
    }
}

impl IconConfig {
    /// The active config: built-in [`Default`] overlaid with any
    /// `file_tree_icons:` overrides parsed from the embedded Visual Style YAML.
    /// A missing/empty block (or a parse failure) leaves the rich defaults
    /// untouched — the file tree always has icons.
    pub fn load() -> Self {
        let mut cfg = Self::default();
        if let Some(ov) = parse_overrides(VISUAL_STYLE_V1_YAML) {
            cfg.merge(ov);
        }
        cfg
    }

    /// Overlay sparse overrides: map entries are inserted (overriding on key
    /// conflict); `default.file` / `default.folder` are replaced when present.
    fn merge(&mut self, ov: IconConfigOverride) {
        if let Some(d) = ov.default {
            if let Some(f) = d.file {
                self.default.file = f;
            }
            if let Some(f) = d.folder {
                self.default.folder = f;
            }
        }
        self.by_extension.extend(ov.by_extension);
        self.by_folder.extend(ov.by_folder);
        self.by_file.extend(ov.by_file);
    }

    /// Resolve the icon for `entry`. `is_open` only matters for directories
    /// (chooses `open_icon`).
    pub fn resolve(&self, entry: &Entry, is_open: bool) -> IconSpec {
        // 1. Exact filename always wins.
        if let Some(rule) = self.by_file.get(&entry.name) {
            return self.spec(&rule.icon, rule.color.as_deref());
        }

        let is_dir = matches!(entry.kind, Kind::Dir);
        if is_dir {
            // 2. A folder's own by_folder rule (e.g. `.git`, `target`).
            if let Some(rule) = self.by_folder.get(&entry.name) {
                let icon = if is_open {
                    rule.open_icon.as_deref().unwrap_or(&rule.icon)
                } else {
                    &rule.icon
                };
                return self.spec(icon, rule.color.as_deref());
            }
            // 4a. Default folder.
            let d = &self.default.folder;
            let icon = if is_open {
                d.open_icon.as_deref().unwrap_or(&d.icon)
            } else {
                &d.icon
            };
            return self.spec(icon, d.color.as_deref());
        }

        // 2. File extension.
        if let Some(ext) = extension(&entry.name) {
            if let Some(rule) = self.by_extension.get(&ext) {
                return self.spec(&rule.icon, rule.color.as_deref());
            }
        }

        // 3. Nearest-ancestor folder rule with applies_to_children.
        if let Some(rule) = self.ancestor_rule(&entry.rel_path) {
            return self.spec(&rule.icon, rule.color.as_deref());
        }

        // 4b. Default file.
        self.spec(&self.default.file.icon, self.default.file.color.as_deref())
    }

    /// Closest ancestor folder (walking `rel_path` from the file toward the
    /// root) carrying a `applies_to_children` rule.
    fn ancestor_rule(&self, rel_path: &str) -> Option<&FolderRule> {
        let mut parts: Vec<&str> = rel_path.split('/').collect();
        parts.pop(); // drop the file name itself
                     // Closest first.
        for folder in parts.iter().rev() {
            if let Some(rule) = self.by_folder.get(*folder) {
                if rule.applies_to_children {
                    return Some(rule);
                }
            }
        }
        None
    }

    fn spec(&self, icon: &str, color: Option<&str>) -> IconSpec {
        IconSpec {
            icon: icon.to_owned(),
            tint: color.and_then(resolve_color),
        }
    }
}

/// The lowercase extension (no dot) of a filename, or `None`. A leading dot
/// (dotfile) is not an extension (`.gitignore` → `None`).
fn extension(name: &str) -> Option<String> {
    let dot = name.rfind('.')?;
    if dot == 0 {
        return None; // dotfile, not an extension
    }
    let ext = &name[dot + 1..];
    if ext.is_empty() {
        None
    } else {
        Some(ext.to_lowercase())
    }
}

/// Resolve a colour spec to a [`Rgba`]: a `$theme_token` (against the white-font
/// text scale + brand) or a `#hex`. Unknown → `None` (inherit row colour).
pub fn resolve_color(spec: &str) -> Option<Rgba> {
    use wylde_theme::colors as c;
    let spec = spec.trim();
    if let Some(token) = spec.strip_prefix('$') {
        return Some(match token {
            "text_primary" => c::TEXT_PRIMARY,
            "text_secondary" => c::TEXT_SECONDARY,
            "text_muted" => c::TEXT_MUTED,
            "text_dim" => c::TEXT_DIM,
            "brand" => c::BRAND,
            "brand_light" => c::BRAND_LIGHT,
            _ => return None,
        });
    }
    if let Some(hex) = spec.strip_prefix('#') {
        return parse_hex_rgba(hex);
    }
    None
}

fn parse_hex_rgba(hex: &str) -> Option<Rgba> {
    let h = hex.trim();
    if h.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()? as f32 / 255.0;
    let g = u8::from_str_radix(&h[2..4], 16).ok()? as f32 / 255.0;
    let b = u8::from_str_radix(&h[4..6], 16).ok()? as f32 / 255.0;
    Some(Rgba { r, g, b, a: 1.0 })
}

// ── YAML override shape ──────────────────────────────────────────────────

const VISUAL_STYLE_V1_YAML: &str = include_str!("../../assets/visual_style_v1.yaml");

#[derive(Debug, Clone, Default, Deserialize)]
struct DefaultsOverride {
    #[serde(default)]
    file: Option<IconRule>,
    #[serde(default)]
    folder: Option<FolderRule>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct IconConfigOverride {
    #[serde(default)]
    default: Option<DefaultsOverride>,
    #[serde(default)]
    by_extension: HashMap<String, IconRule>,
    #[serde(default)]
    by_folder: HashMap<String, FolderRule>,
    #[serde(default)]
    by_file: HashMap<String, IconRule>,
}

/// Pull the `file_tree_icons:` block out of the Visual Style YAML. `None` when
/// the block is absent or the YAML doesn't parse — callers keep the defaults.
fn parse_overrides(yaml: &str) -> Option<IconConfigOverride> {
    #[derive(Deserialize)]
    struct Root {
        #[serde(default)]
        file_tree_icons: Option<IconConfigOverride>,
    }
    serde_yaml::from_str::<Root>(yaml).ok()?.file_tree_icons
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: Kind, rel_path: &str) -> Entry {
        Entry {
            name: name.to_owned(),
            kind,
            rel_path: rel_path.to_owned(),
            ignored: false,
        }
    }

    #[test]
    fn extension_skips_dotfiles_and_lowercases() {
        assert_eq!(extension("main.RS").as_deref(), Some("rs"));
        assert_eq!(extension(".gitignore"), None);
        assert_eq!(extension("Makefile"), None);
        assert_eq!(extension("archive.tar.gz").as_deref(), Some("gz"));
    }

    #[test]
    fn by_file_beats_by_extension() {
        let cfg = IconConfig::default();
        // Cargo.toml is `package` by exact name, not `config` by .toml ext.
        let spec = cfg.resolve(&entry("Cargo.toml", Kind::File, "Cargo.toml"), false);
        assert_eq!(spec.icon, "package");
        // A plain .toml falls through to the extension rule.
        let other = cfg.resolve(&entry("ruff.toml", Kind::File, "ruff.toml"), false);
        assert_eq!(other.icon, "config");
    }

    #[test]
    fn extension_maps_to_per_language_glyph() {
        let cfg = IconConfig::default();
        // Languages with a dedicated Seti glyph resolve to it (not the generic
        // `code` category).
        assert_eq!(
            cfg.resolve(&entry("main.rs", Kind::File, "src/main.rs"), false)
                .icon,
            "rust"
        );
        assert_eq!(
            cfg.resolve(&entry("app.py", Kind::File, "app.py"), false)
                .icon,
            "python"
        );
        assert_eq!(
            cfg.resolve(&entry("index.ts", Kind::File, "index.ts"), false)
                .icon,
            "typescript"
        );
        assert_eq!(
            cfg.resolve(&entry("App.tsx", Kind::File, "App.tsx"), false)
                .icon,
            "react"
        );
        assert_eq!(
            cfg.resolve(&entry("main.go", Kind::File, "main.go"), false)
                .icon,
            "go"
        );
        assert_eq!(
            cfg.resolve(&entry("notes.md", Kind::File, "notes.md"), false)
                .icon,
            "markdown"
        );
        assert_eq!(
            cfg.resolve(&entry("data.json", Kind::File, "data.json"), false)
                .icon,
            "json"
        );
        assert_eq!(
            cfg.resolve(&entry("Lib.hs", Kind::File, "Lib.hs"), false)
                .icon,
            "haskell"
        );
    }

    #[test]
    fn extension_maps_to_lucide_category_when_no_language_glyph() {
        let cfg = IconConfig::default();
        // No per-language Seti glyph → fall through to a Lucide category.
        assert_eq!(
            cfg.resolve(&entry("logo.png", Kind::File, "logo.png"), false)
                .icon,
            "image"
        );
        assert_eq!(
            cfg.resolve(&entry("notes.txt", Kind::File, "notes.txt"), false)
                .icon,
            "doc"
        );
        assert_eq!(
            cfg.resolve(&entry("ruff.toml", Kind::File, "ruff.toml"), false)
                .icon,
            "config"
        );
        assert_eq!(
            cfg.resolve(&entry("rows.csv", Kind::File, "rows.csv"), false)
                .icon,
            "data"
        );
        assert_eq!(
            cfg.resolve(&entry("q.sql", Kind::File, "q.sql"), false)
                .icon,
            "data"
        );
        // A source language with no dedicated glyph → generic `code`.
        assert_eq!(
            cfg.resolve(&entry("build.gradle", Kind::File, "build.gradle"), false)
                .icon,
            "code"
        );
    }

    #[test]
    fn unknown_extension_falls_back_to_default_file() {
        let cfg = IconConfig::default();
        let spec = cfg.resolve(&entry("mystery.zzz", Kind::File, "mystery.zzz"), false);
        assert_eq!(spec.icon, "file");
    }

    #[test]
    fn directories_use_folder_and_open_variant() {
        let cfg = IconConfig::default();
        let closed = cfg.resolve(&entry("src", Kind::Dir, "src"), false);
        assert_eq!(closed.icon, "folder");
        let open = cfg.resolve(&entry("src", Kind::Dir, "src"), true);
        assert_eq!(open.icon, "folder-open");
    }

    #[test]
    fn folder_rules_override_default_folder() {
        let cfg = IconConfig::default();
        assert_eq!(
            cfg.resolve(&entry(".git", Kind::Dir, ".git"), false).icon,
            "git"
        );
        assert_eq!(
            cfg.resolve(&entry("target", Kind::Dir, "target"), false)
                .icon,
            "package"
        );
    }

    #[test]
    fn files_inherit_nearest_ancestor_container_rule() {
        let cfg = IconConfig::default();
        // A file with no extension rule, inside target/ → inherits `package`.
        let spec = cfg.resolve(&entry("blob", Kind::File, "target/debug/blob"), false);
        assert_eq!(spec.icon, "package");
        // But a recognised extension still wins over the ancestor rule.
        let rs = cfg.resolve(&entry("build.rs", Kind::File, "target/build.rs"), false);
        assert_eq!(rs.icon, "rust");
    }

    #[test]
    fn exact_name_tool_files_get_their_glyph() {
        let cfg = IconConfig::default();
        assert_eq!(
            cfg.resolve(&entry("Dockerfile", Kind::File, "Dockerfile"), false)
                .icon,
            "docker"
        );
        assert_eq!(
            cfg.resolve(&entry("Makefile", Kind::File, "Makefile"), false)
                .icon,
            "makefile"
        );
        // Manifests stay on `package`; the exact-name rule beats the extension.
        assert_eq!(
            cfg.resolve(&entry("go.mod", Kind::File, "go.mod"), false)
                .icon,
            "package"
        );
    }

    #[test]
    fn every_mapped_icon_name_has_an_svg_asset() {
        // The config must never name a glyph with no `.svg` on disk (that would
        // render a blank icon and — once added to the embed list — fail the
        // `include_bytes!` build). Cross-check every name the defaults can emit
        // against the asset directory. The Shell crate's `FILE_TREE_ICONS`
        // embed list is separately guarded at build time by `include_bytes!`.
        let icons_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../assets/icons/file-tree");
        let cfg = IconConfig::default();
        let mut names: Vec<String> = vec![
            cfg.default.file.icon.clone(),
            cfg.default.folder.icon.clone(),
        ];
        if let Some(o) = &cfg.default.folder.open_icon {
            names.push(o.clone());
        }
        names.extend(cfg.by_extension.values().map(|r| r.icon.clone()));
        names.extend(cfg.by_file.values().map(|r| r.icon.clone()));
        for r in cfg.by_folder.values() {
            names.push(r.icon.clone());
            if let Some(o) = &r.open_icon {
                names.push(o.clone());
            }
        }
        for n in names {
            let p = icons_dir.join(format!("{n}.svg"));
            assert!(
                p.exists(),
                "mapped icon {n:?} has no asset at {}",
                p.display()
            );
        }
    }

    #[test]
    fn readme_carries_a_brand_tint() {
        let cfg = IconConfig::default();
        let spec = cfg.resolve(&entry("README.md", Kind::File, "README.md"), false);
        assert_eq!(spec.icon, "book");
        assert_eq!(spec.tint, Some(wylde_theme::colors::BRAND_LIGHT));
    }

    #[test]
    fn icon_specs_without_color_inherit_row_colour() {
        let cfg = IconConfig::default();
        let spec = cfg.resolve(&entry("main.rs", Kind::File, "main.rs"), false);
        assert_eq!(spec.tint, None, "no explicit colour → inherit");
    }

    #[test]
    fn asset_path_uses_the_bundle_namespace() {
        let spec = IconSpec {
            icon: "code".to_owned(),
            tint: None,
        };
        assert_eq!(spec.asset_path(), "icons/file-tree/code.svg");
    }

    #[test]
    fn color_resolution_tokens_and_hex() {
        assert_eq!(
            resolve_color("$text_primary"),
            Some(wylde_theme::colors::TEXT_PRIMARY)
        );
        assert_eq!(
            resolve_color("$brand_light"),
            Some(wylde_theme::colors::BRAND_LIGHT)
        );
        assert_eq!(resolve_color("$nope"), None);
        let hex = resolve_color("#dea584").unwrap();
        assert!((hex.r - 0xde as f32 / 255.0).abs() < 1e-6);
        assert!((hex.g - 0xa5 as f32 / 255.0).abs() < 1e-6);
        assert!((hex.b - 0x84 as f32 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn load_merges_yaml_overrides_over_defaults() {
        // load() must not panic on the real embedded YAML and must keep the
        // built-in defaults intact (the YAML block is sparse/optional).
        let cfg = IconConfig::load();
        assert_eq!(cfg.default.file.icon, "file");
        assert_eq!(
            cfg.resolve(&entry("main.rs", Kind::File, "main.rs"), false)
                .icon,
            "rust"
        );
    }

    #[test]
    fn yaml_override_overlays_a_single_extension() {
        let yaml = r##"
file_tree_icons:
  by_extension:
    rs: { icon: "package", color: "#dea584" }
"##;
        let ov = parse_overrides(yaml).expect("block parses");
        let mut cfg = IconConfig::default();
        cfg.merge(ov);
        // The override wins; other defaults are untouched.
        let rs = cfg.resolve(&entry("main.rs", Kind::File, "main.rs"), false);
        assert_eq!(rs.icon, "package");
        assert_eq!(
            cfg.resolve(&entry("a.py", Kind::File, "a.py"), false).icon,
            "python"
        );
    }
}
