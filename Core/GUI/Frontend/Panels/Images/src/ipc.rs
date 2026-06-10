//! Per-panel IPC helpers for the Images panel.
//!
//! The image-gen surface ships through `wylde-gateway` rather than the
//! harness — the Gateway's pipe server happens to serve the same
//! `/api/images/*` HTTP shape its HTTP front door does (see
//! `Gateway/pipe.py::start`: when the FastAPI `app` is passed in, the
//! pipe routes by `method` + `path` exactly like the HTTP layer).  So
//! every helper here goes `wylde_gui_pipe::call("wylde-gateway", verb,
//! "/api/images/…", body)` and projects the JSON reply into a strongly-
//! typed struct.
//!
//! Routes used:
//!
//!   * `GET /api/images/library` — list `{id, filename, size_bytes,
//!     created_at, metadata}`.
//!   * `GET /api/images/library/{id}` — fetch one image inline, `data_b64`
//!     is base64 of the raw PNG/JPEG/WebP bytes.
//!   * `DELETE /api/images/library/{id}` — drop every file matching
//!     `<id>.*` from the library directory.
//!   * `POST /api/images/generate` — proxy to the underlying ComfyUI
//!     service.  Gateway timeout is 600 s (set in the Rust gateway port).
//!   * `GET /api/images/models` — proxy to ComfyUI `/list_models`.
//!   * `GET /api/images/loras` — proxy to ComfyUI `/list_loras`.
//!
//! Errors carry the raw transport string verbatim so the panel's
//! degraded-state branch can show it inline.

use base64::Engine as _;
use serde_json::Value;

pub const SVC_GATEWAY: &str = "wylde-gateway";

async fn get(path: &str) -> Result<Value, String> {
    wylde_gui_pipe::call(SVC_GATEWAY, "GET", path, None).await
}

async fn delete(path: &str) -> Result<Value, String> {
    wylde_gui_pipe::call(SVC_GATEWAY, "DELETE", path, None).await
}

async fn post(path: &str, body: Value) -> Result<Value, String> {
    wylde_gui_pipe::call(SVC_GATEWAY, "POST", path, Some(body)).await
}

// ── Library list ────────────────────────────────────────────────────

/// One row in the gallery grid.  `created_at` is the file mtime in Unix
/// seconds as a float (matches the Python + Rust gateways).  `size_bytes`
/// is the on-disk byte count.  `metadata` is the sidecar JSON the
/// image-gen service writes next to each PNG — shape varies by service
/// version, so we keep it as a `Value` and surface known fields lazily
/// in the metadata pane.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageEntry {
    pub id: String,
    pub filename: String,
    pub size_bytes: u64,
    pub created_at: f64,
    pub metadata: Value,
}

impl ImageEntry {
    pub fn from_value(v: &Value) -> Self {
        Self {
            id: v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            filename: v
                .get("filename")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            size_bytes: v.get("size_bytes").and_then(|x| x.as_u64()).unwrap_or(0),
            created_at: v.get("created_at").and_then(|x| x.as_f64()).unwrap_or(0.0),
            metadata: v.get("metadata").cloned().unwrap_or(Value::Null),
        }
    }

    /// Best-effort prompt projection out of the sidecar metadata.
    pub fn prompt(&self) -> Option<&str> {
        self.metadata
            .get("prompt")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    }

    /// Best-effort model name out of the sidecar metadata.
    pub fn model(&self) -> Option<&str> {
        self.metadata.get("model").and_then(|v| v.as_str())
    }

    /// Best-effort seed.  ComfyUI writes this as an integer, but we
    /// also accept a string (older Wylde image services did).
    pub fn seed(&self) -> Option<String> {
        if let Some(n) = self.metadata.get("seed").and_then(|v| v.as_i64()) {
            return Some(n.to_string());
        }
        self.metadata
            .get("seed")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned())
    }

    pub fn width(&self) -> Option<u64> {
        self.metadata.get("width").and_then(|v| v.as_u64())
    }

    pub fn height(&self) -> Option<u64> {
        self.metadata.get("height").and_then(|v| v.as_u64())
    }

    pub fn workspace_id(&self) -> Option<&str> {
        self.metadata.get("workspace_id").and_then(|v| v.as_str())
    }

    /// Source classifier: `generated` / `imported` / `tool` / `unknown`.
    /// Falls back to "generated" when nothing is recorded, since that's
    /// what the image-gen service writes by default.
    pub fn source(&self) -> &str {
        self.metadata
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("generated")
    }
}

/// Parse the `/library` reply, which is `{"images": [ … ]}` shaped the
/// same in both the Python and Rust gateway implementations.
pub fn parse_library(v: &Value) -> Vec<ImageEntry> {
    let Some(arr) = v.get("images").and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    arr.iter().map(ImageEntry::from_value).collect()
}

pub async fn read_library() -> Result<Vec<ImageEntry>, String> {
    let v = get("/api/images/library").await?;
    Ok(parse_library(&v))
}

// ── Single-image fetch (raw bytes inline) ───────────────────────────

/// Decoded image payload — base64 already unwrapped, mime preserved so
/// the renderer can pick the right `gpui::ImageFormat`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageBytes {
    pub id: String,
    pub filename: String,
    pub mime: String,
    pub bytes: Vec<u8>,
    pub metadata: Value,
}

impl ImageBytes {
    pub fn extension(&self) -> &str {
        // mime is "image/png" / "image/jpeg" / "image/webp" / "image/gif"
        // — caller-friendly suffix is just whatever follows the slash.
        match self.mime.split_once('/') {
            Some((_, ext)) => ext,
            None => "",
        }
    }
}

pub async fn read_image(id: &str) -> Result<ImageBytes, String> {
    let path = format!("/api/images/library/{id}");
    let v = get(&path).await?;
    parse_image_bytes(id, &v)
}

/// Project a `/library/{id}` reply into the typed shape.  Public for
/// unit testing.
pub fn parse_image_bytes(id: &str, v: &Value) -> Result<ImageBytes, String> {
    let data_b64 = v
        .get("data_b64")
        .and_then(|x| x.as_str())
        .ok_or_else(|| format!("malformed image reply for {id}: no data_b64"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64.as_bytes())
        .map_err(|e| format!("decode base64 for {id}: {e}"))?;
    Ok(ImageBytes {
        id: v
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or(id)
            .to_owned(),
        filename: v
            .get("filename")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_owned(),
        mime: v
            .get("mime")
            .and_then(|x| x.as_str())
            .unwrap_or("image/png")
            .to_owned(),
        bytes,
        metadata: v.get("metadata").cloned().unwrap_or(Value::Null),
    })
}

// ── Delete ──────────────────────────────────────────────────────────

/// Drop every file matching `<id>.*` from the library.  Returns the
/// list of filenames the server removed.
pub async fn delete_image(id: &str) -> Result<Vec<String>, String> {
    let path = format!("/api/images/library/{id}");
    let v = delete(&path).await?;
    let removed = v
        .get("deleted")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_owned()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(removed)
}

// ── Generate ────────────────────────────────────────────────────────

/// Minimal generate-request envelope.  Mirrors the ComfyUI proxy: the
/// underlying service is free-form so we only fix the prompt key here
/// and pass everything else through verbatim via `extra`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GenerateRequest {
    pub prompt: String,
    pub model: Option<String>,
    pub workspace_id: Option<String>,
}

impl GenerateRequest {
    pub fn to_value(&self) -> Value {
        let mut body = serde_json::Map::new();
        body.insert("prompt".into(), Value::from(self.prompt.clone()));
        if let Some(m) = &self.model {
            body.insert("model".into(), Value::from(m.clone()));
        }
        if let Some(w) = &self.workspace_id {
            body.insert("workspace_id".into(), Value::from(w.clone()));
        }
        Value::Object(body)
    }
}

/// Generate result — minimum projection.  The image-gen service is
/// free-form so we surface whatever id-shaped field we can recognise
/// and keep the raw body for diagnostics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GenerateOutcome {
    pub id: Option<String>,
    pub filename: Option<String>,
    pub raw: Value,
}

impl GenerateOutcome {
    pub fn from_value(v: &Value) -> Self {
        let id = v
            .get("id")
            .or_else(|| v.get("image_id"))
            .or_else(|| v.get("img_id"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_owned());
        let filename = v
            .get("filename")
            .and_then(|x| x.as_str())
            .map(|s| s.to_owned());
        Self {
            id,
            filename,
            raw: v.clone(),
        }
    }
}

/// One-shot generate.  Returns once the gateway proxy replies — the
/// caller-visible flow uses a tokio task so the panel doesn't block the
/// UI thread on the 600 s timeout.
pub async fn generate(req: GenerateRequest) -> Result<GenerateOutcome, String> {
    if req.prompt.trim().is_empty() {
        return Err("bad_request: prompt is empty".into());
    }
    let body = req.to_value();
    let v = post("/api/images/generate", body).await?;
    Ok(GenerateOutcome::from_value(&v))
}

// ── Available models + LoRAs ────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageModel {
    pub name: String,
    pub kind: String,
}

impl ImageModel {
    pub fn from_value(v: &Value) -> Self {
        // Two shapes seen in the wild: a bare string ("model.safetensors")
        // or `{name, kind}`.  Normalise both.
        if let Some(name) = v.as_str() {
            return Self {
                name: name.to_owned(),
                kind: "checkpoint".into(),
            };
        }
        Self {
            name: v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            kind: v
                .get("kind")
                .and_then(|x| x.as_str())
                .unwrap_or("checkpoint")
                .to_owned(),
        }
    }
}

pub fn parse_models(v: &Value) -> Vec<ImageModel> {
    // ComfyUI's `/list_models` typically returns `{"models": [...]}` but
    // also occasionally bare arrays; tolerate both shapes.
    let arr_opt = v
        .get("models")
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned());
    let Some(arr) = arr_opt else {
        return Vec::new();
    };
    arr.iter().map(ImageModel::from_value).collect()
}

pub async fn read_models() -> Result<Vec<ImageModel>, String> {
    let v = get("/api/images/models").await?;
    Ok(parse_models(&v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_library_handles_empty_envelope() {
        assert!(parse_library(&json!({})).is_empty());
        assert!(parse_library(&json!({"images": []})).is_empty());
    }

    #[test]
    fn parse_library_projects_full_row() {
        let v = json!({"images": [
            {
                "id": "abc",
                "filename": "abc.png",
                "size_bytes": 12345,
                "created_at": 1_780_000_000.5,
                "metadata": {
                    "prompt": "a sunset",
                    "model": "sdxl",
                    "seed": 42,
                    "width": 1024,
                    "height": 1024,
                    "workspace_id": "ws-1",
                    "source": "generated"
                }
            }
        ]});
        let rows = parse_library(&v);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.id, "abc");
        assert_eq!(r.size_bytes, 12345);
        assert_eq!(r.prompt(), Some("a sunset"));
        assert_eq!(r.model(), Some("sdxl"));
        assert_eq!(r.seed().as_deref(), Some("42"));
        assert_eq!(r.width(), Some(1024));
        assert_eq!(r.workspace_id(), Some("ws-1"));
        assert_eq!(r.source(), "generated");
    }

    #[test]
    fn image_entry_falls_back_when_metadata_missing() {
        let v = json!({"id": "x", "filename": "x.png", "size_bytes": 1, "created_at": 0.0});
        let e = ImageEntry::from_value(&v);
        assert!(e.prompt().is_none());
        assert!(e.model().is_none());
        assert!(e.seed().is_none());
        // source defaults to "generated" when nothing is recorded.
        assert_eq!(e.source(), "generated");
    }

    #[test]
    fn image_entry_seed_string_shape_is_supported() {
        let v = json!({
            "id": "x", "filename": "x.png", "size_bytes": 0, "created_at": 0.0,
            "metadata": {"seed": "deadbeef"}
        });
        assert_eq!(
            ImageEntry::from_value(&v).seed().as_deref(),
            Some("deadbeef")
        );
    }

    #[test]
    fn parse_image_bytes_decodes_base64() {
        // "hello" base64 = "aGVsbG8="
        let v = json!({
            "id": "h",
            "filename": "h.png",
            "mime": "image/png",
            "data_b64": "aGVsbG8=",
            "metadata": {}
        });
        let out = parse_image_bytes("h", &v).unwrap();
        assert_eq!(out.id, "h");
        assert_eq!(out.filename, "h.png");
        assert_eq!(out.mime, "image/png");
        assert_eq!(out.bytes, b"hello");
        assert_eq!(out.extension(), "png");
    }

    #[test]
    fn parse_image_bytes_rejects_missing_payload() {
        let v = json!({"id": "h", "filename": "h.png", "mime": "image/png"});
        let err = parse_image_bytes("h", &v).unwrap_err();
        assert!(err.contains("no data_b64"));
    }

    #[test]
    fn parse_image_bytes_rejects_malformed_base64() {
        let v = json!({
            "id": "h", "filename": "h.png", "mime": "image/png",
            "data_b64": "not~~~valid~~~base64",
            "metadata": {}
        });
        assert!(parse_image_bytes("h", &v).is_err());
    }

    #[test]
    fn parse_image_bytes_defaults_mime_when_missing() {
        let v = json!({"id": "h", "filename": "h.png", "data_b64": "aGVsbG8="});
        let out = parse_image_bytes("h", &v).unwrap();
        assert_eq!(out.mime, "image/png");
        assert_eq!(out.extension(), "png");
    }

    #[test]
    fn parse_models_accepts_object_and_bare_array() {
        let v_obj = json!({"models": ["m1", {"name": "m2", "kind": "lora"}]});
        let v_arr = json!(["m3"]);
        let mods_obj = parse_models(&v_obj);
        assert_eq!(mods_obj.len(), 2);
        assert_eq!(mods_obj[0].name, "m1");
        assert_eq!(mods_obj[0].kind, "checkpoint");
        assert_eq!(mods_obj[1].name, "m2");
        assert_eq!(mods_obj[1].kind, "lora");
        let mods_arr = parse_models(&v_arr);
        assert_eq!(mods_arr.len(), 1);
        assert_eq!(mods_arr[0].name, "m3");
    }

    #[test]
    fn generate_request_serialises_known_keys() {
        let req = GenerateRequest {
            prompt: "fox".into(),
            model: Some("sdxl".into()),
            workspace_id: Some("ws".into()),
        };
        let v = req.to_value();
        assert_eq!(v["prompt"], "fox");
        assert_eq!(v["model"], "sdxl");
        assert_eq!(v["workspace_id"], "ws");
    }

    #[test]
    fn generate_request_skips_none_keys() {
        let req = GenerateRequest {
            prompt: "fox".into(),
            ..GenerateRequest::default()
        };
        let v = req.to_value();
        assert!(v.get("model").is_none());
        assert!(v.get("workspace_id").is_none());
    }

    #[test]
    fn generate_outcome_picks_known_id_keys() {
        let v = json!({"image_id": "abc", "filename": "abc.png"});
        let g = GenerateOutcome::from_value(&v);
        assert_eq!(g.id.as_deref(), Some("abc"));
        assert_eq!(g.filename.as_deref(), Some("abc.png"));
    }

    #[tokio::test]
    async fn generate_rejects_empty_prompt() {
        let err = generate(GenerateRequest::default()).await.unwrap_err();
        assert!(err.contains("bad_request"));
    }
}
