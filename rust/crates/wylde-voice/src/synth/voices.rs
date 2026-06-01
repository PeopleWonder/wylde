//! `voices.npz` loader.
//!
//! NumPy's `np.savez` produces an uncompressed ZIP (PKZIP "stored")
//! container with one `.npy` member per array — that's exactly what
//! `Voice/download_models.py:213` writes after assembling the per-voice
//! `.bin` files into a single bundle. Each entry is a `[510, 1, 256]`
//! float32 style-vector table; for synthesis we slice it on axis 0 with
//! `len(tokens)` to get the `[1, 256]` style the model expects.
//!
//! We parse the container in-house (no `zip` / `ndarray-npy` dep) for
//! three reasons:
//!
//! 1. `np.savez` defaults to "stored" — no inflate, just byte copies.
//! 2. The `.npy` header is ~20 lines of parser; pulling in the
//!    `ndarray-npy` crate (and its transitive `zip` + `miniz_oxide`
//!    deps) would dwarf the volume of code here.
//! 3. Keeps `cargo build --release -p wylde-voice` binary size on the
//!    same ground it sits on today (5.6 MB after Slice 11.A+).
//!
//! Validation strategy: each `.npy` is parsed by header → shape → raw
//! bytes; size is checked against the declared shape so a corrupt or
//! double-compressed file fails fast at load with a clean error.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Per-voice style-vector dimensions, mirroring
/// `Voice/download_models.py:204` (`reshape(-1, 1, 256)`).
pub const VOICE_STYLE_LENGTHS: usize = 510;
pub const VOICE_STYLE_INNER: usize = 1;
pub const VOICE_STYLE_DIM: usize = 256;
pub const VOICE_STYLE_TOTAL_F32: usize =
    VOICE_STYLE_LENGTHS * VOICE_STYLE_INNER * VOICE_STYLE_DIM;

#[derive(Debug, Error)]
pub enum VoicesLoadError {
    #[error("voices.npz not found at {0}")]
    NotFound(PathBuf),

    #[error("voices.npz I/O failed: {0}")]
    Io(String),

    #[error("voices.npz format: {0}")]
    Format(String),

    #[error("voice {voice}: {detail}")]
    VoiceEntry { voice: String, detail: String },
}

/// One Kokoro voice. `data` is the flat float32 buffer of shape
/// `[VOICE_STYLE_LENGTHS, 1, VOICE_STYLE_DIM]` — same row-major layout
/// numpy uses by default.
#[derive(Debug, Clone)]
pub struct VoiceStyle {
    pub data: Vec<f32>,
}

impl VoiceStyle {
    /// Slice the style table at row index `len_tokens`. Mirrors
    /// `voice = voice[len(tokens)]` in
    /// `kokoro_onnx.Kokoro._create_audio`.
    ///
    /// Returns `None` if the requested length is out of range (caller
    /// should treat as "phoneme sequence too long for this voice").
    pub fn style_for_token_len(&self, len_tokens: usize) -> Option<&[f32]> {
        if len_tokens >= VOICE_STYLE_LENGTHS {
            return None;
        }
        let stride = VOICE_STYLE_INNER * VOICE_STYLE_DIM;
        let start = len_tokens * stride;
        self.data.get(start..start + stride)
    }
}

/// Loaded Kokoro voices bundle. Cheap to clone (`Arc` the whole
/// `Voices` struct from the call site if needed).
#[derive(Debug, Clone)]
pub struct Voices {
    by_name: HashMap<String, VoiceStyle>,
}

impl Voices {
    /// Load and parse a `voices.npz` file.
    pub fn load(path: &Path) -> Result<Self, VoicesLoadError> {
        if !path.exists() {
            return Err(VoicesLoadError::NotFound(path.to_path_buf()));
        }
        let mut f = File::open(path).map_err(|e| VoicesLoadError::Io(e.to_string()))?;
        let entries = read_zip_central_directory(&mut f)?;

        let mut by_name = HashMap::new();
        for entry in entries {
            // np.savez names members "<key>.npy". Skip anything that
            // doesn't fit so a stray manifest file wouldn't break load.
            let Some(name) = entry.name.strip_suffix(".npy") else {
                continue;
            };
            let raw_npy = read_zip_member(&mut f, &entry)?;
            let parsed = parse_npy(&raw_npy).map_err(|detail| VoicesLoadError::VoiceEntry {
                voice: name.to_owned(),
                detail,
            })?;
            by_name.insert(name.to_owned(), parsed);
        }

        if by_name.is_empty() {
            return Err(VoicesLoadError::Format(
                "voices.npz contained no .npy members".to_owned(),
            ));
        }

        Ok(Self { by_name })
    }

    /// Resolve a voice by name. None when missing.
    pub fn get(&self, voice: &str) -> Option<&VoiceStyle> {
        self.by_name.get(voice)
    }

    /// All voice names, sorted alphabetically for stable enumeration.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.by_name.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

// --------------------------------------------------------------------- //
// Minimal PKZIP "stored"-only reader.                                   //
// --------------------------------------------------------------------- //

/// One entry in the central directory we care about.
struct ZipEntry {
    name: String,
    /// Offset of the local file header in the archive.
    local_header_offset: u64,
    /// Uncompressed size in bytes. We refuse compressed entries.
    uncompressed_size: u32,
}

/// End-of-Central-Directory signature.
const EOCD_SIGNATURE: u32 = 0x0605_4b50;
/// Central-Directory File-Header signature.
const CD_SIGNATURE: u32 = 0x0201_4b50;
/// Local File-Header signature.
const LOCAL_SIGNATURE: u32 = 0x0403_4b50;

fn read_zip_central_directory(f: &mut File) -> Result<Vec<ZipEntry>, VoicesLoadError> {
    let file_len = f
        .seek(SeekFrom::End(0))
        .map_err(|e| VoicesLoadError::Io(e.to_string()))?;
    // ZIP EOCD is at most ~22 bytes for an empty-comment archive, but
    // can be up to 22 + 65535 if a comment is present. np.savez writes
    // no comment, so a 64 KiB tail read is more than enough.
    let scan_len = file_len.min(65_557);
    let scan_start = file_len - scan_len;
    f.seek(SeekFrom::Start(scan_start))
        .map_err(|e| VoicesLoadError::Io(e.to_string()))?;
    let mut buf = vec![0_u8; scan_len as usize];
    f.read_exact(&mut buf)
        .map_err(|e| VoicesLoadError::Io(e.to_string()))?;

    // Walk backwards for the EOCD magic.
    let mut eocd_local = None;
    if buf.len() >= 4 {
        for i in (0..=buf.len() - 4).rev() {
            if u32::from_le_bytes(buf[i..i + 4].try_into().unwrap()) == EOCD_SIGNATURE {
                eocd_local = Some(i);
                break;
            }
        }
    }
    let eocd_at = eocd_local.ok_or_else(|| {
        VoicesLoadError::Format("EOCD record not found — not a valid .npz".to_owned())
    })?;
    let eocd = &buf[eocd_at..];
    if eocd.len() < 22 {
        return Err(VoicesLoadError::Format("EOCD record truncated".to_owned()));
    }
    let total_entries = u16::from_le_bytes(eocd[10..12].try_into().unwrap()) as usize;
    let cd_size = u32::from_le_bytes(eocd[12..16].try_into().unwrap()) as u64;
    let cd_offset = u32::from_le_bytes(eocd[16..20].try_into().unwrap()) as u64;

    f.seek(SeekFrom::Start(cd_offset))
        .map_err(|e| VoicesLoadError::Io(e.to_string()))?;
    let mut cd = vec![0_u8; cd_size as usize];
    f.read_exact(&mut cd)
        .map_err(|e| VoicesLoadError::Io(e.to_string()))?;

    let mut entries = Vec::with_capacity(total_entries);
    let mut cur = 0_usize;
    while cur + 46 <= cd.len() && entries.len() < total_entries {
        if u32::from_le_bytes(cd[cur..cur + 4].try_into().unwrap()) != CD_SIGNATURE {
            return Err(VoicesLoadError::Format(format!(
                "central-directory header at offset {cur} has wrong magic"
            )));
        }
        let compression = u16::from_le_bytes(cd[cur + 10..cur + 12].try_into().unwrap());
        if compression != 0 {
            return Err(VoicesLoadError::Format(format!(
                "compressed .npz entry (method={compression}) not supported — \
                 voices.npz must be uncompressed (np.savez default)"
            )));
        }
        let compressed_size =
            u32::from_le_bytes(cd[cur + 20..cur + 24].try_into().unwrap());
        let uncompressed_size =
            u32::from_le_bytes(cd[cur + 24..cur + 28].try_into().unwrap());
        if compressed_size != uncompressed_size {
            return Err(VoicesLoadError::Format(format!(
                "compressed/uncompressed sizes disagree ({compressed_size} vs \
                 {uncompressed_size}) — entry is not 'stored'"
            )));
        }
        let name_len = u16::from_le_bytes(cd[cur + 28..cur + 30].try_into().unwrap()) as usize;
        let extra_len = u16::from_le_bytes(cd[cur + 30..cur + 32].try_into().unwrap()) as usize;
        let comment_len = u16::from_le_bytes(cd[cur + 32..cur + 34].try_into().unwrap()) as usize;
        let local_offset = u32::from_le_bytes(cd[cur + 42..cur + 46].try_into().unwrap()) as u64;
        let name_start = cur + 46;
        let name_end = name_start + name_len;
        if name_end > cd.len() {
            return Err(VoicesLoadError::Format(
                "central-directory name overflows record".to_owned(),
            ));
        }
        let name = String::from_utf8(cd[name_start..name_end].to_vec())
            .map_err(|e| VoicesLoadError::Format(format!("entry name not UTF-8: {e}")))?;
        entries.push(ZipEntry {
            name,
            local_header_offset: local_offset,
            uncompressed_size,
        });
        cur = name_end + extra_len + comment_len;
    }
    Ok(entries)
}

fn read_zip_member(f: &mut File, entry: &ZipEntry) -> Result<Vec<u8>, VoicesLoadError> {
    f.seek(SeekFrom::Start(entry.local_header_offset))
        .map_err(|e| VoicesLoadError::Io(e.to_string()))?;
    let mut header = [0_u8; 30];
    f.read_exact(&mut header)
        .map_err(|e| VoicesLoadError::Io(e.to_string()))?;
    if u32::from_le_bytes(header[0..4].try_into().unwrap()) != LOCAL_SIGNATURE {
        return Err(VoicesLoadError::Format(format!(
            "local file header for {} has wrong magic",
            entry.name
        )));
    }
    let name_len = u16::from_le_bytes(header[26..28].try_into().unwrap()) as i64;
    let extra_len = u16::from_le_bytes(header[28..30].try_into().unwrap()) as i64;
    f.seek(SeekFrom::Current(name_len + extra_len))
        .map_err(|e| VoicesLoadError::Io(e.to_string()))?;
    let mut data = vec![0_u8; entry.uncompressed_size as usize];
    f.read_exact(&mut data)
        .map_err(|e| VoicesLoadError::Io(e.to_string()))?;
    Ok(data)
}

// --------------------------------------------------------------------- //
// Minimal .npy parser (float32, C-order, shape [N, 1, 256] only).       //
// --------------------------------------------------------------------- //

fn parse_npy(bytes: &[u8]) -> Result<VoiceStyle, String> {
    const MAGIC: &[u8; 6] = b"\x93NUMPY";
    if bytes.len() < 10 || &bytes[0..6] != MAGIC {
        return Err("missing \\x93NUMPY magic".to_owned());
    }
    let major = bytes[6];
    let minor = bytes[7];
    let header_len: usize = match (major, minor) {
        (1, 0) => u16::from_le_bytes([bytes[8], bytes[9]]) as usize,
        (2, 0) | (3, 0) => {
            if bytes.len() < 12 {
                return Err("v2 header truncated".to_owned());
            }
            u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize
        }
        _ => return Err(format!("unsupported .npy version {major}.{minor}")),
    };
    let header_start = if major == 1 { 10 } else { 12 };
    let header_end = header_start + header_len;
    if header_end > bytes.len() {
        return Err("header length overflows file".to_owned());
    }
    let header = std::str::from_utf8(&bytes[header_start..header_end])
        .map_err(|e| format!("header not UTF-8: {e}"))?;

    let descr = extract_dict_str(header, "descr")?;
    let descr_trim = descr.trim_matches('\'');
    if descr_trim != "<f4" && descr_trim != "|f4" && descr_trim != "f4" {
        return Err(format!(
            "expected float32 little-endian dtype, got {descr_trim:?}"
        ));
    }
    let fortran = extract_dict_value(header, "fortran_order")?;
    if fortran.trim() == "True" {
        return Err("Fortran-order arrays not supported".to_owned());
    }
    let shape = extract_shape(header)?;
    if shape != [VOICE_STYLE_LENGTHS, VOICE_STYLE_INNER, VOICE_STYLE_DIM] {
        return Err(format!(
            "expected shape [{VOICE_STYLE_LENGTHS}, {VOICE_STYLE_INNER}, {VOICE_STYLE_DIM}], got {shape:?}"
        ));
    }

    let body = &bytes[header_end..];
    if body.len() != VOICE_STYLE_TOTAL_F32 * 4 {
        return Err(format!(
            "payload size {} doesn't match expected {} bytes",
            body.len(),
            VOICE_STYLE_TOTAL_F32 * 4
        ));
    }
    let mut data = Vec::with_capacity(VOICE_STYLE_TOTAL_F32);
    for chunk in body.chunks_exact(4) {
        data.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(VoiceStyle { data })
}

/// Pull a quoted-string value out of the Python-literal `.npy` header
/// dict — e.g. `descr: '<f4'`. We tolerate single or double quotes
/// because numpy 1.x writes singles and 2.x writes doubles.
fn extract_dict_str(header: &str, key: &str) -> Result<String, String> {
    let raw = extract_dict_value(header, key)?;
    let trimmed = raw.trim();
    // Strip one layer of quotes.
    let stripped = trimmed
        .strip_prefix('\'')
        .or_else(|| trimmed.strip_prefix('"'))
        .ok_or_else(|| format!(".npy header key {key} value not a quoted string: {raw:?}"))?;
    let final_str = stripped
        .strip_suffix('\'')
        .or_else(|| stripped.strip_suffix('"'))
        .ok_or_else(|| format!(".npy header key {key} value missing closing quote: {raw:?}"))?;
    Ok(final_str.to_owned())
}

/// Pull the raw textual value for `key` out of the `.npy` header.
/// Stops at the next `,` or `}` outside any nested parens. Brittle but
/// sufficient for numpy's tightly-formatted output.
fn extract_dict_value(header: &str, key: &str) -> Result<String, String> {
    let needle = format!("'{key}'");
    let alt_needle = format!("\"{key}\"");
    let key_at = header
        .find(&needle)
        .or_else(|| header.find(&alt_needle))
        .ok_or_else(|| format!(".npy header missing key {key}"))?;
    let after_key = &header[key_at..];
    let colon_at = after_key
        .find(':')
        .ok_or_else(|| format!("no ':' after key {key} in header"))?;
    let after_colon = &after_key[colon_at + 1..];
    let mut depth_paren = 0_i32;
    let mut value_end = after_colon.len();
    for (i, ch) in after_colon.char_indices() {
        match ch {
            '(' | '[' | '{' => depth_paren += 1,
            ')' | ']' | '}' => {
                if depth_paren == 0 {
                    value_end = i;
                    break;
                }
                depth_paren -= 1;
            }
            ',' if depth_paren == 0 => {
                value_end = i;
                break;
            }
            _ => {}
        }
    }
    Ok(after_colon[..value_end].trim().to_owned())
}

fn extract_shape(header: &str) -> Result<[usize; 3], String> {
    let raw = extract_dict_value(header, "shape")?;
    let trimmed = raw
        .trim()
        .strip_prefix('(')
        .ok_or_else(|| format!("shape missing '(' prefix: {raw:?}"))?
        .trim_end_matches(',')
        .trim_end_matches(')');
    let parts: Vec<&str> = trimmed.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if parts.len() != 3 {
        return Err(format!("expected 3-d shape, got {parts:?}"));
    }
    let mut out = [0_usize; 3];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p
            .parse::<usize>()
            .map_err(|_| format!("shape axis {i} not an integer: {p:?}"))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal `.npy` v1.0 byte buffer for shape [510,1,256]
    /// filled with a deterministic ramp — round-trips through
    /// `parse_npy` so we don't need the real model on disk.
    fn synthetic_npy(value: f32) -> Vec<u8> {
        let header = format!(
            "{{'descr': '<f4', 'fortran_order': False, 'shape': ({L}, {I}, {D}), }}",
            L = VOICE_STYLE_LENGTHS,
            I = VOICE_STYLE_INNER,
            D = VOICE_STYLE_DIM,
        );
        // Pad header so total prefix length is multiple of 64 (numpy convention).
        let prefix_len_unpadded = 10 + header.len() + 1;
        let pad = (64 - prefix_len_unpadded % 64) % 64;
        let mut padded_header = header.into_bytes();
        padded_header.extend(std::iter::repeat_n(b' ', pad));
        padded_header.push(b'\n');
        let header_len = u16::try_from(padded_header.len()).unwrap();

        let mut out = Vec::new();
        out.extend_from_slice(b"\x93NUMPY");
        out.push(1);
        out.push(0);
        out.extend_from_slice(&header_len.to_le_bytes());
        out.extend_from_slice(&padded_header);
        for _ in 0..VOICE_STYLE_TOTAL_F32 {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out
    }

    #[test]
    fn parse_npy_accepts_correct_shape() {
        let raw = synthetic_npy(0.5);
        let parsed = parse_npy(&raw).expect("synthetic npy parses");
        assert_eq!(parsed.data.len(), VOICE_STYLE_TOTAL_F32);
        assert!((parsed.data[0] - 0.5).abs() < 1e-6);
        assert!((parsed.data[VOICE_STYLE_TOTAL_F32 - 1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn parse_npy_rejects_missing_magic() {
        let mut raw = synthetic_npy(0.0);
        raw[0] = b'X';
        let err = parse_npy(&raw).unwrap_err();
        assert!(err.contains("NUMPY"), "{err}");
    }

    #[test]
    fn voice_style_for_token_len_slices_correctly() {
        let mut data = vec![0_f32; VOICE_STYLE_TOTAL_F32];
        // Mark each row with its index so the slice test is unambiguous.
        for row in 0..VOICE_STYLE_LENGTHS {
            for col in 0..VOICE_STYLE_DIM {
                data[row * VOICE_STYLE_DIM + col] = row as f32;
            }
        }
        let style = VoiceStyle { data };
        let row5 = style.style_for_token_len(5).expect("len 5 in range");
        assert_eq!(row5.len(), VOICE_STYLE_DIM);
        assert!((row5[0] - 5.0).abs() < 1e-6);
        assert!((row5[VOICE_STYLE_DIM - 1] - 5.0).abs() < 1e-6);
    }

    #[test]
    fn voice_style_returns_none_for_out_of_range_len() {
        let style = VoiceStyle {
            data: vec![0_f32; VOICE_STYLE_TOTAL_F32],
        };
        assert!(style.style_for_token_len(VOICE_STYLE_LENGTHS).is_none());
        assert!(style.style_for_token_len(10_000).is_none());
    }

    #[test]
    fn voices_load_missing_file_returns_not_found() {
        let err = Voices::load(Path::new("/no/such/voices.npz")).unwrap_err();
        assert!(matches!(err, VoicesLoadError::NotFound(_)));
    }

    #[test]
    fn synthetic_npz_round_trips_through_voices_load() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let npz_path = dir.path().join("voices.npz");

        // Write a 2-voice "stored" zip by hand.
        let voice_a = synthetic_npy(0.1);
        let voice_b = synthetic_npy(0.2);
        let mut bytes: Vec<u8> = Vec::new();
        let mut entries: Vec<(String, u32, u64)> = Vec::new();
        for (name, payload) in [("alpha.npy", &voice_a), ("beta.npy", &voice_b)] {
            let local_offset = bytes.len() as u64;
            let mut local = Vec::new();
            local.extend_from_slice(&LOCAL_SIGNATURE.to_le_bytes());
            local.extend_from_slice(&20_u16.to_le_bytes());   // version needed
            local.extend_from_slice(&0_u16.to_le_bytes());    // flags
            local.extend_from_slice(&0_u16.to_le_bytes());    // method=stored
            local.extend_from_slice(&0_u16.to_le_bytes());    // mod time
            local.extend_from_slice(&0_u16.to_le_bytes());    // mod date
            local.extend_from_slice(&0_u32.to_le_bytes());    // crc32 (we don't verify)
            let size = payload.len() as u32;
            local.extend_from_slice(&size.to_le_bytes());
            local.extend_from_slice(&size.to_le_bytes());
            local.extend_from_slice(&(name.len() as u16).to_le_bytes());
            local.extend_from_slice(&0_u16.to_le_bytes());    // extra len
            local.extend_from_slice(name.as_bytes());
            bytes.extend(local);
            bytes.extend_from_slice(payload);
            entries.push((name.to_owned(), size, local_offset));
        }
        let cd_offset = bytes.len() as u64;
        for (name, size, local_offset) in &entries {
            let mut cd_hdr = Vec::new();
            cd_hdr.extend_from_slice(&CD_SIGNATURE.to_le_bytes());
            cd_hdr.extend_from_slice(&20_u16.to_le_bytes()); // version made by
            cd_hdr.extend_from_slice(&20_u16.to_le_bytes()); // version needed
            cd_hdr.extend_from_slice(&0_u16.to_le_bytes());
            cd_hdr.extend_from_slice(&0_u16.to_le_bytes());
            cd_hdr.extend_from_slice(&0_u16.to_le_bytes());
            cd_hdr.extend_from_slice(&0_u16.to_le_bytes());
            cd_hdr.extend_from_slice(&0_u32.to_le_bytes()); // crc32
            cd_hdr.extend_from_slice(&size.to_le_bytes());
            cd_hdr.extend_from_slice(&size.to_le_bytes());
            cd_hdr.extend_from_slice(&(name.len() as u16).to_le_bytes());
            cd_hdr.extend_from_slice(&0_u16.to_le_bytes()); // extra len
            cd_hdr.extend_from_slice(&0_u16.to_le_bytes()); // comment len
            cd_hdr.extend_from_slice(&0_u16.to_le_bytes()); // disk num start
            cd_hdr.extend_from_slice(&0_u16.to_le_bytes()); // internal attrs
            cd_hdr.extend_from_slice(&0_u32.to_le_bytes()); // external attrs
            cd_hdr.extend_from_slice(&(*local_offset as u32).to_le_bytes());
            cd_hdr.extend_from_slice(name.as_bytes());
            bytes.extend(cd_hdr);
        }
        let cd_size = (bytes.len() as u64) - cd_offset;
        let mut eocd = Vec::new();
        eocd.extend_from_slice(&EOCD_SIGNATURE.to_le_bytes());
        eocd.extend_from_slice(&0_u16.to_le_bytes()); // disk num
        eocd.extend_from_slice(&0_u16.to_le_bytes()); // cd disk
        eocd.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        eocd.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        eocd.extend_from_slice(&(cd_size as u32).to_le_bytes());
        eocd.extend_from_slice(&(cd_offset as u32).to_le_bytes());
        eocd.extend_from_slice(&0_u16.to_le_bytes()); // comment len
        bytes.extend(eocd);

        let mut f = std::fs::File::create(&npz_path).unwrap();
        f.write_all(&bytes).unwrap();
        drop(f);

        let voices = Voices::load(&npz_path).expect("synthetic .npz loads");
        assert_eq!(voices.len(), 2);
        assert!(voices.get("alpha").is_some());
        assert!(voices.get("beta").is_some());
        assert_eq!(voices.names(), vec!["alpha", "beta"]);
        let alpha_row = voices.get("alpha").unwrap().style_for_token_len(0).unwrap();
        assert!((alpha_row[0] - 0.1).abs() < 1e-6);
    }
}
