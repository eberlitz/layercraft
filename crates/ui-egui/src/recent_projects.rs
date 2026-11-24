//! Persistent recent projects metadata (`recent_projects.json` on native, `localStorage` + IndexedDB on WASM)
//! and crisp 240x136 3D plate thumbnails encoded as PNG base64.

use base64::Engine as _;
use layercraft_config::PrintConfig;
use layercraft_geom::TriangleMesh;
use layercraft_slice::SlicedPlate;
use layercraft_viewport::{CameraPreset, ColorMode, RenderMode, RenderRequest, render_png};
use serde::{Deserialize, Serialize};

/// Maximum number of recent projects retained across restarts.
pub const MAX_RECENT_PROJECTS: usize = 12;

/// Browser `localStorage` key storing [`RecentProjectsStore`] metadata & thumbnails.
pub const BROWSER_RECENT_PROJECTS_STORAGE_KEY: &str = "layercraft.recent_projects.v1";

/// Maximum browser IndexedDB payload size per saved project (16 MiB).
pub const MAX_BROWSER_PROJECT_BLOB_BYTES: usize = 16 * 1024 * 1024;

/// Rendered thumbnail width in pixels.
pub const RECENT_THUMB_WIDTH: u32 = 240;

/// Rendered thumbnail height in pixels.
pub const RECENT_THUMB_HEIGHT: u32 = 136;

/// Storage backend for a recent project entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecentProjectSource {
    /// Local `.3mf` or `.stl` file path on disk (desktop builds).
    #[default]
    NativePath,
    /// Full `.3mf` bytes stored in browser IndexedDB (WASM builds).
    BrowserIndexedDb,
}

/// Summary card entry shown on the Studio Home page.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RecentProjectEntry {
    /// Stable identifier (`native:<path>` or `web:<id>`).
    pub id: String,
    /// Human-readable project title shown on the Home card.
    pub name: String,
    /// Lowercase source format (`"3mf"`, `"stl"`, etc.).
    pub format: String,
    /// Where the project bytes can be reloaded from.
    pub source: RecentProjectSource,
    /// Filesystem path when `source == RecentProjectSource::NativePath`.
    pub path: Option<String>,
    /// IndexedDB object key when `source == RecentProjectSource::BrowserIndexedDb`.
    pub storage_key: Option<String>,
    /// Unix epoch timestamp in seconds when last opened or saved.
    pub last_opened_unix_secs: u64,
    /// Total object count across plates.
    pub object_count: usize,
    /// Total plate count.
    pub plate_count: usize,
    /// Active machine profile name at the time of recording.
    pub printer_name: Option<String>,
    /// Formatted estimated print time if already sliced.
    pub estimated_time: Option<String>,
    /// Base64-encoded PNG thumbnail (`240x136`) of the active plate.
    pub thumbnail_png_b64: Option<String>,
}

/// Collection of recently opened or saved projects, ordered most recent first.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RecentProjectsStore {
    /// Ordered list of recent entries (most recent at index 0).
    pub entries: Vec<RecentProjectEntry>,
}

impl RecentProjectsStore {
    /// Insert or update an entry at index 0, deduplicating by ID or native path,
    /// and cap the list at [`MAX_RECENT_PROJECTS`].
    pub fn upsert(&mut self, mut entry: RecentProjectEntry) {
        let trimmed_path = entry
            .path
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned);

        let mut preserved_thumb = None;
        let mut preserved_time = None;
        let mut preserved_printer = None;

        self.entries.retain(|existing| {
            let same_id = existing.id == entry.id;
            let same_path = trimmed_path.as_deref().is_some_and(|p| {
                existing.source == RecentProjectSource::NativePath
                    && existing.path.as_deref().map(str::trim) == Some(p)
            });
            if same_id || same_path {
                if preserved_thumb.is_none() {
                    preserved_thumb = existing.thumbnail_png_b64.clone();
                }
                if preserved_time.is_none() {
                    preserved_time = existing.estimated_time.clone();
                }
                if preserved_printer.is_none() {
                    preserved_printer = existing.printer_name.clone();
                }
                false
            } else {
                true
            }
        });

        if entry.thumbnail_png_b64.is_none() {
            entry.thumbnail_png_b64 = preserved_thumb;
        }
        if entry.estimated_time.is_none() {
            entry.estimated_time = preserved_time;
        }
        if entry.printer_name.is_none() {
            entry.printer_name = preserved_printer;
        }

        self.entries.insert(0, entry);
        self.entries.truncate(MAX_RECENT_PROJECTS);
    }

    /// Remove a single entry by ID and return it if present.
    pub fn remove(&mut self, id: &str) -> Option<RecentProjectEntry> {
        let pos = self.entries.iter().position(|e| e.id == id)?;
        Some(self.entries.remove(pos))
    }

    /// Remove all recent entries.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Import legacy filesystem paths from [`crate::prefs::AppPreferences::recent_files`]
    /// if they are not already tracked in `self.entries`. Returns `true` if modified.
    pub fn migrate_legacy_recent_files(&mut self, legacy_paths: &[String]) -> bool {
        let mut changed = false;
        for raw in legacy_paths {
            if self.entries.len() >= MAX_RECENT_PROJECTS {
                break;
            }
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let already_present = self.entries.iter().any(|e| {
                e.source == RecentProjectSource::NativePath
                    && e.path.as_deref().map(str::trim) == Some(trimmed)
            });
            if already_present {
                continue;
            }
            let path = std::path::Path::new(trimmed);
            self.entries.push(RecentProjectEntry {
                id: native_entry_id(path),
                name: display_title_from_name(trimmed),
                format: infer_file_format(trimmed),
                source: RecentProjectSource::NativePath,
                path: Some(trimmed.to_owned()),
                storage_key: None,
                last_opened_unix_secs: 0,
                object_count: 1,
                plate_count: 1,
                printer_name: None,
                estimated_time: None,
                thumbnail_png_b64: None,
            });
            changed = true;
        }
        changed
    }

    /// Parse [`RecentProjectsStore`] from JSON text.
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        serde_json::from_str::<Self>(raw).map_err(|e| format!("invalid recent projects JSON: {e}"))
    }

    /// Serialize [`RecentProjectsStore`] to formatted JSON.
    pub fn to_json_pretty(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| format!("failed serializing recent projects: {e}"))
    }

    /// Default native store path (`$XDG_CONFIG_HOME/layercraft/recent_projects.json` or `~/.config/layercraft/recent_projects.json`).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn default_store_path() -> Option<std::path::PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            return Some(
                std::path::PathBuf::from(xdg)
                    .join("layercraft")
                    .join("recent_projects.json"),
            );
        }
        std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| {
            std::path::PathBuf::from(h)
                .join(".config")
                .join("layercraft")
                .join("recent_projects.json")
        })
    }

    /// Load store from an explicit native JSON file path (returns empty store if missing).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load_from_path(path: &std::path::Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed reading `{}`: {e}", path.display()))?;
        Self::from_json_str(&raw)
    }

    /// Save store to an explicit native JSON file path, creating parent directories if needed.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn save_to_path(&self, path: &std::path::Path) -> Result<(), String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed creating `{}`: {e}", parent.display()))?;
        }
        let json = self.to_json_pretty()?;
        std::fs::write(path, json).map_err(|e| format!("failed writing `{}`: {e}", path.display()))
    }

    /// Load recent projects from the default native path, falling back cleanly on error.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load_from_disk() -> Self {
        let Some(path) = Self::default_store_path() else {
            return Self::default();
        };
        Self::load_from_path(&path).unwrap_or_default()
    }

    /// Load recent projects from browser `localStorage` (falling back to empty store).
    #[cfg(target_arch = "wasm32")]
    pub fn load_from_disk() -> Self {
        let Some(storage) = browser_local_storage() else {
            return Self::default();
        };
        let Ok(Some(raw)) = storage.get_item(BROWSER_RECENT_PROJECTS_STORAGE_KEY) else {
            return Self::default();
        };
        Self::from_json_str(&raw).unwrap_or_default()
    }

    /// Persist recent projects to the default native path.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn save_to_disk(&self) -> Result<(), String> {
        let Some(path) = Self::default_store_path() else {
            return Ok(());
        };
        self.save_to_path(&path)
    }

    /// Persist recent projects to browser `localStorage`, trimming oldest thumbnails if quota is tight.
    #[cfg(target_arch = "wasm32")]
    pub fn save_to_disk(&self) -> Result<(), String> {
        let Some(storage) = browser_local_storage() else {
            return Ok(());
        };
        let json = self.to_json_pretty()?;
        if storage
            .set_item(BROWSER_RECENT_PROJECTS_STORAGE_KEY, &json)
            .is_ok()
        {
            return Ok(());
        }
        let mut trimmed = self.clone();
        for idx in (0..trimmed.entries.len()).rev() {
            if let Some(entry) = trimmed.entries.get_mut(idx) {
                entry.thumbnail_png_b64 = None;
            }
            let retry_json = trimmed.to_json_pretty()?;
            if storage
                .set_item(BROWSER_RECENT_PROJECTS_STORAGE_KEY, &retry_json)
                .is_ok()
            {
                return Ok(());
            }
        }
        Err("failed saving recent projects to localStorage (quota exceeded)".to_owned())
    }
}

/// Current Unix epoch time in seconds (works on both native and `wasm32`).
pub fn now_unix_secs() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format an entry timestamp relative to `now_unix_secs`.
pub fn format_relative_time(last_opened_unix_secs: u64, now_unix_secs: u64) -> String {
    if last_opened_unix_secs == 0 {
        return "Previously".to_owned();
    }
    let diff = now_unix_secs.saturating_sub(last_opened_unix_secs);
    if diff < 60 {
        "Just now".to_owned()
    } else if diff < 3_600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86_400 {
        format!("{}h ago", diff / 3_600)
    } else if diff < 7 * 86_400 {
        format!("{}d ago", diff / 86_400)
    } else if diff < 30 * 86_400 {
        format!("{}w ago", diff / (7 * 86_400))
    } else {
        format!("{}mo ago", (diff / (30 * 86_400)).max(1))
    }
}

/// Build a canonical-when-possible ID for a native project file path.
pub fn native_entry_id(path: &std::path::Path) -> String {
    #[cfg(not(target_arch = "wasm32"))]
    let normalized = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    #[cfg(target_arch = "wasm32")]
    let normalized = path.to_path_buf();
    format!("native:{}", normalized.to_string_lossy())
}

/// Extract lowercase format label (`"3mf"`, `"stl"`, etc.) from a file name or path.
pub fn infer_file_format(name_or_path: &str) -> String {
    std::path::Path::new(name_or_path.trim())
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .filter(|ext| !ext.is_empty())
        .unwrap_or_else(|| "project".to_owned())
}

/// Derive a clean card title (without path directory or `.3mf`/`.stl` suffix when a stem exists).
pub fn display_title_from_name(name_or_path: &str) -> String {
    let trimmed = name_or_path.trim();
    if trimmed.is_empty() {
        return "Untitled Project".to_owned();
    }
    let path = std::path::Path::new(trimmed);
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        && !stem.is_empty()
    {
        return stem.to_owned();
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .unwrap_or(trimmed)
        .to_owned()
}

/// Render a 240x136 isometric 3D PNG thumbnail encoded as standard base64.
pub fn render_plate_thumbnail_b64(
    mesh: Option<&TriangleMesh>,
    plate: Option<&SlicedPlate>,
    config: &PrintConfig,
) -> Option<String> {
    if mesh.is_none() && plate.is_none() {
        return None;
    }
    let req = RenderRequest {
        width: RECENT_THUMB_WIDTH,
        height: RECENT_THUMB_HEIGHT,
        mode: if mesh.is_some() {
            RenderMode::Plate
        } else {
            RenderMode::Preview
        },
        camera_preset: CameraPreset::Isometric,
        max_layer: None,
        single_layer_only: false,
        color_mode: ColorMode::Feature,
    };
    let png_bytes = render_png(mesh, plate, config, &req).ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(&png_bytes))
}

/// Decode a base64 PNG thumbnail into an [`egui::ColorImage`] for lazy GPU texture upload.
pub fn decode_thumbnail_color_image(b64: &str) -> Result<egui::ColorImage, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| format!("invalid thumbnail base64: {e}"))?;
    let mut decoder = png::Decoder::new(std::io::Cursor::new(&bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("invalid thumbnail PNG header: {e}"))?;
    let buf_size = reader
        .output_buffer_size()
        .ok_or_else(|| "thumbnail PNG buffer size overflow".to_owned())?;
    let mut raw = vec![0u8; buf_size];
    let info = reader
        .next_frame(&mut raw)
        .map_err(|e| format!("failed decoding thumbnail PNG frame: {e}"))?;
    let data = raw
        .get(..info.buffer_size())
        .ok_or_else(|| "truncated thumbnail PNG frame buffer".to_owned())?;
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => data
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[g, a]| [g, g, g, a])
            .collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        other => {
            return Err(format!("unsupported thumbnail PNG color type: {other:?}"));
        }
    };
    let w = usize::try_from(info.width).map_err(|_| "thumbnail width exceeds usize".to_owned())?;
    let h =
        usize::try_from(info.height).map_err(|_| "thumbnail height exceeds usize".to_owned())?;
    if w == 0 || h == 0 || rgba.len() != w.saturating_mul(h).saturating_mul(4) {
        return Err("thumbnail PNG pixel dimensions mismatch".to_owned());
    }
    Ok(egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba))
}

#[cfg(target_arch = "wasm32")]
fn browser_local_storage() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|w| w.local_storage().ok().flatten())
}

#[cfg(target_arch = "wasm32")]
mod idb {
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::JsValue;
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen_futures::JsFuture;

    const DB_NAME: &str = "layercraft_studio_v1";
    const DB_VERSION: u32 = 1;
    const STORE_NAME: &str = "project_blobs";

    fn js_err(context: &str, err: JsValue) -> String {
        format!("{context}: {err:?}")
    }

    async fn await_request(req: &web_sys::IdbRequest, context: &str) -> Result<JsValue, String> {
        let promise = js_sys::Promise::new(&mut |resolve, reject| {
            let req_ok = req.clone();
            let reject_ok = reject.clone();
            let ctx_ok = context.to_owned();
            let on_ok = Closure::once_into_js(move |_ev: web_sys::Event| {
                req_ok.set_onsuccess(None);
                req_ok.set_onerror(None);
                match req_ok.result() {
                    Ok(val) => {
                        let _ = resolve.call1(&JsValue::NULL, &val);
                    }
                    Err(e) => {
                        let _ = reject_ok
                            .call1(&JsValue::NULL, &JsValue::from_str(&js_err(&ctx_ok, e)));
                    }
                }
            });

            let req_err = req.clone();
            let ctx_err = context.to_owned();
            let on_err = Closure::once_into_js(move |_ev: web_sys::Event| {
                req_err.set_onsuccess(None);
                req_err.set_onerror(None);
                let msg = format!("{ctx_err} request failed");
                let _ = reject.call1(&JsValue::NULL, &JsValue::from_str(&msg));
            });

            req.set_onsuccess(Some(on_ok.unchecked_ref()));
            req.set_onerror(Some(on_err.unchecked_ref()));
        });
        JsFuture::from(promise)
            .await
            .map_err(|e| js_err(context, e))
    }

    async fn await_transaction(tx: &web_sys::IdbTransaction, context: &str) -> Result<(), String> {
        let promise = js_sys::Promise::new(&mut |resolve, reject| {
            let tx_ok = tx.clone();
            let on_complete = Closure::once_into_js(move |_ev: web_sys::Event| {
                tx_ok.set_oncomplete(None);
                tx_ok.set_onerror(None);
                tx_ok.set_onabort(None);
                let _ = resolve.call0(&JsValue::NULL);
            });

            let tx_err = tx.clone();
            let reject_err = reject.clone();
            let ctx_err = context.to_owned();
            let on_err = Closure::once_into_js(move |_ev: web_sys::Event| {
                tx_err.set_oncomplete(None);
                tx_err.set_onerror(None);
                tx_err.set_onabort(None);
                let msg = format!("{ctx_err} transaction error");
                let _ = reject_err.call1(&JsValue::NULL, &JsValue::from_str(&msg));
            });

            let tx_abort = tx.clone();
            let ctx_abort = context.to_owned();
            let on_abort = Closure::once_into_js(move |_ev: web_sys::Event| {
                tx_abort.set_oncomplete(None);
                tx_abort.set_onerror(None);
                tx_abort.set_onabort(None);
                let msg = format!("{ctx_abort} transaction aborted");
                let _ = reject.call1(&JsValue::NULL, &JsValue::from_str(&msg));
            });

            tx.set_oncomplete(Some(on_complete.unchecked_ref()));
            tx.set_onerror(Some(on_err.unchecked_ref()));
            tx.set_onabort(Some(on_abort.unchecked_ref()));
        });
        JsFuture::from(promise)
            .await
            .map(|_| ())
            .map_err(|e| js_err(context, e))
    }

    async fn open_db() -> Result<web_sys::IdbDatabase, String> {
        let window = web_sys::window().ok_or_else(|| "missing browser window".to_owned())?;
        let factory = window
            .indexed_db()
            .map_err(|e| js_err("window.indexedDB", e))?
            .ok_or_else(|| "IndexedDB is unavailable in this browser context".to_owned())?;
        let open_req = factory
            .open_with_u32(DB_NAME, DB_VERSION)
            .map_err(|e| js_err("indexedDB.open", e))?;

        let open_req_upgrade = open_req.clone();
        let on_upgrade = Closure::once_into_js(move |_ev: web_sys::IdbVersionChangeEvent| {
            open_req_upgrade.set_onupgradeneeded(None);
            if let Ok(val) = open_req_upgrade.result()
                && let Ok(db) = val.dyn_into::<web_sys::IdbDatabase>()
                && !db.object_store_names().contains(STORE_NAME)
            {
                let _ = db.create_object_store(STORE_NAME);
            }
        });
        open_req.set_onupgradeneeded(Some(on_upgrade.unchecked_ref()));

        let req_ref: &web_sys::IdbRequest = &open_req;
        let result = await_request(req_ref, "indexedDB.open").await?;
        open_req.set_onupgradeneeded(None);
        result
            .dyn_into::<web_sys::IdbDatabase>()
            .map_err(|e| js_err("indexedDB.open cast", e))
    }

    pub async fn put_project_bytes(key: &str, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > super::MAX_BROWSER_PROJECT_BLOB_BYTES {
            return Err(format!(
                "project size ({} bytes) exceeds {} byte browser storage cap",
                bytes.len(),
                super::MAX_BROWSER_PROJECT_BLOB_BYTES
            ));
        }
        let db = open_db().await?;
        let tx = db
            .transaction_with_str_and_mode(STORE_NAME, web_sys::IdbTransactionMode::Readwrite)
            .map_err(|e| js_err("indexedDB readwrite tx", e))?;
        let store = tx
            .object_store(STORE_NAME)
            .map_err(|e| js_err("indexedDB object_store", e))?;
        let array = js_sys::Uint8Array::from(bytes);
        let req = store
            .put_with_key(array.as_ref(), &JsValue::from_str(key))
            .map_err(|e| js_err("indexedDB put", e))?;
        await_request(&req, "indexedDB.put").await?;
        await_transaction(&tx, "indexedDB.put").await?;
        db.close();
        Ok(())
    }

    pub async fn get_project_bytes(key: &str) -> Result<Option<Vec<u8>>, String> {
        let db = open_db().await?;
        let tx = db
            .transaction_with_str_and_mode(STORE_NAME, web_sys::IdbTransactionMode::Readonly)
            .map_err(|e| js_err("indexedDB readonly tx", e))?;
        let store = tx
            .object_store(STORE_NAME)
            .map_err(|e| js_err("indexedDB object_store", e))?;
        let req = store
            .get(&JsValue::from_str(key))
            .map_err(|e| js_err("indexedDB get", e))?;
        let val = await_request(&req, "indexedDB.get").await?;
        db.close();
        if val.is_undefined() || val.is_null() {
            return Ok(None);
        }
        if val.is_instance_of::<js_sys::Uint8Array>() || val.is_instance_of::<js_sys::ArrayBuffer>()
        {
            return Ok(Some(js_sys::Uint8Array::new(&val).to_vec()));
        }
        Err("unexpected value type stored in IndexedDB project cache".to_owned())
    }

    pub async fn delete_project_bytes(key: &str) -> Result<(), String> {
        let db = open_db().await?;
        let tx = db
            .transaction_with_str_and_mode(STORE_NAME, web_sys::IdbTransactionMode::Readwrite)
            .map_err(|e| js_err("indexedDB delete tx", e))?;
        let store = tx
            .object_store(STORE_NAME)
            .map_err(|e| js_err("indexedDB object_store", e))?;
        let req = store
            .delete(&JsValue::from_str(key))
            .map_err(|e| js_err("indexedDB delete", e))?;
        await_request(&req, "indexedDB.delete").await?;
        await_transaction(&tx, "indexedDB.delete").await?;
        db.close();
        Ok(())
    }

    pub async fn clear_project_bytes() -> Result<(), String> {
        let db = open_db().await?;
        let tx = db
            .transaction_with_str_and_mode(STORE_NAME, web_sys::IdbTransactionMode::Readwrite)
            .map_err(|e| js_err("indexedDB clear tx", e))?;
        let store = tx
            .object_store(STORE_NAME)
            .map_err(|e| js_err("indexedDB object_store", e))?;
        let req = store.clear().map_err(|e| js_err("indexedDB clear", e))?;
        await_request(&req, "indexedDB.clear").await?;
        await_transaction(&tx, "indexedDB.clear").await?;
        db.close();
        Ok(())
    }
}

/// Store serialized project bytes under `key` in browser IndexedDB.
#[cfg(target_arch = "wasm32")]
pub async fn idb_put_project_bytes(key: &str, bytes: &[u8]) -> Result<(), String> {
    idb::put_project_bytes(key, bytes).await
}

/// Load serialized project bytes for `key` from browser IndexedDB (`None` if absent).
#[cfg(target_arch = "wasm32")]
pub async fn idb_get_project_bytes(key: &str) -> Result<Option<Vec<u8>>, String> {
    idb::get_project_bytes(key).await
}

/// Delete stored project bytes for `key` from browser IndexedDB.
#[cfg(target_arch = "wasm32")]
pub async fn idb_delete_project_bytes(key: &str) -> Result<(), String> {
    idb::delete_project_bytes(key).await
}

/// Clear all stored project bytes from browser IndexedDB.
#[cfg(target_arch = "wasm32")]
pub async fn idb_clear_project_bytes() -> Result<(), String> {
    idb::clear_project_bytes().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use layercraft_geom::{Point3, Triangle};

    fn test_cube_mesh() -> TriangleMesh {
        let (x0, x1, y0, y1, z0, z1) = (115.0, 135.0, 115.0, 135.0, 0.0, 20.0);
        let p000 = Point3::new(x0, y0, z0);
        let p100 = Point3::new(x1, y0, z0);
        let p110 = Point3::new(x1, y1, z0);
        let p010 = Point3::new(x0, y1, z0);
        let p001 = Point3::new(x0, y0, z1);
        let p101 = Point3::new(x1, y0, z1);
        let p111 = Point3::new(x1, y1, z1);
        let p011 = Point3::new(x0, y1, z1);
        TriangleMesh::new(vec![
            Triangle::new(p000, p110, p100),
            Triangle::new(p000, p010, p110),
            Triangle::new(p001, p101, p111),
            Triangle::new(p001, p111, p011),
            Triangle::new(p000, p100, p101),
            Triangle::new(p000, p101, p001),
            Triangle::new(p010, p111, p110),
            Triangle::new(p010, p011, p111),
            Triangle::new(p000, p001, p011),
            Triangle::new(p000, p011, p010),
            Triangle::new(p100, p110, p111),
            Triangle::new(p100, p111, p101),
        ])
        .unwrap()
    }

    fn make_entry(id: &str, name: &str, path: Option<&str>, ts: u64) -> RecentProjectEntry {
        RecentProjectEntry {
            id: id.to_owned(),
            name: name.to_owned(),
            format: "3mf".to_owned(),
            source: RecentProjectSource::NativePath,
            path: path.map(str::to_owned),
            storage_key: None,
            last_opened_unix_secs: ts,
            object_count: 1,
            plate_count: 1,
            printer_name: Some("Prusa CORE One".to_owned()),
            estimated_time: Some("18m".to_owned()),
            thumbnail_png_b64: None,
        }
    }

    #[test]
    fn recent_projects_upsert_dedup_eviction_and_json_roundtrip() {
        let mut store = RecentProjectsStore::default();
        for idx in 0..(MAX_RECENT_PROJECTS + 3) {
            let id = format!("native:/tmp/p{idx}.3mf");
            let path = format!("/tmp/p{idx}.3mf");
            store.upsert(make_entry(
                &id,
                &format!("Part {idx}"),
                Some(&path),
                idx as u64 + 10,
            ));
        }
        assert_eq!(store.entries.len(), MAX_RECENT_PROJECTS);
        assert_eq!(
            store.entries[0].id,
            format!("native:/tmp/p{}.3mf", MAX_RECENT_PROJECTS + 2)
        );

        // Upserting an existing entry by path moves it to index 0 without creating a duplicate,
        // and preserves any existing thumbnail when the update omits it.
        store.entries[4].thumbnail_png_b64 = Some("stub_thumb".to_owned());
        let existing_path = store.entries[4].path.clone();
        store.upsert(make_entry(
            "alt-id-same-path",
            "Updated Part",
            existing_path.as_deref(),
            999,
        ));
        assert_eq!(store.entries.len(), MAX_RECENT_PROJECTS);
        assert_eq!(store.entries[0].name, "Updated Part");
        assert_eq!(
            store.entries[0].thumbnail_png_b64.as_deref(),
            Some("stub_thumb")
        );

        let json = store.to_json_pretty().unwrap();
        let parsed = RecentProjectsStore::from_json_str(&json).unwrap();
        assert_eq!(parsed, store);

        assert!(store.remove("alt-id-same-path").is_some());
        assert_eq!(store.entries.len(), MAX_RECENT_PROJECTS - 1);

        let migrated = store.migrate_legacy_recent_files(&[
            "/tmp/legacy_a.stl".to_owned(),
            existing_path.unwrap_or_default(),
        ]);
        assert!(migrated);
        assert!(
            store
                .entries
                .iter()
                .any(|e| e.name == "legacy_a" && e.format == "stl")
        );
    }

    #[test]
    fn thumbnail_render_and_decode_roundtrip() {
        let mesh = test_cube_mesh();
        let cfg = PrintConfig::default();
        let b64 = render_plate_thumbnail_b64(Some(&mesh), None, &cfg)
            .expect("thumbnail PNG renders cleanly");
        let img = decode_thumbnail_color_image(&b64).expect("thumbnail PNG decodes cleanly");
        assert_eq!(
            img.size,
            [RECENT_THUMB_WIDTH as usize, RECENT_THUMB_HEIGHT as usize]
        );
        assert_eq!(
            img.pixels.len(),
            (RECENT_THUMB_WIDTH as usize) * (RECENT_THUMB_HEIGHT as usize)
        );
    }

    #[test]
    fn relative_time_formatting_works() {
        let now = 1_700_000_000;
        assert_eq!(format_relative_time(0, now), "Previously");
        assert_eq!(format_relative_time(now - 10, now), "Just now");
        assert_eq!(format_relative_time(now - 180, now), "3m ago");
        assert_eq!(format_relative_time(now - 7_200, now), "2h ago");
        assert_eq!(format_relative_time(now - 3 * 86_400, now), "3d ago");
        assert_eq!(format_relative_time(now - 14 * 86_400, now), "2w ago");
        assert_eq!(format_relative_time(now - 65 * 86_400, now), "2mo ago");
    }
}
