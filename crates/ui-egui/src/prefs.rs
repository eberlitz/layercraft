//! Persistent lightweight Studio preferences (`prefs.json` on native, no-op on WASM).

use layercraft_config::ActiveProfiles;
use serde::{Deserialize, Serialize};

/// Maximum number of recent files kept in [`AppPreferences::recent_files`].
pub const MAX_RECENT_FILES: usize = 10;

/// User UI preferences saved across native Studio restarts.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppPreferences {
    /// Theme preference (`"dark"`, `"light"`, or `"system"`).
    pub theme_mode: Option<String>,
    /// Last active Machine / Filament / Process preset names.
    pub active_profiles: ActiveProfiles,
    /// Recently opened model/project paths (most recent first, deduplicated).
    pub recent_files: Vec<String>,
    /// Saved desktop window inner size `[width, height]` in logical pixels.
    pub window_size: Option<[f32; 2]>,
    /// Optional saved sidebar panel width in logical pixels.
    pub sidebar_width: Option<f32>,
}

impl AppPreferences {
    /// Insert `path` at the front of `recent_files`, deduplicating and capping at [`MAX_RECENT_FILES`].
    pub fn push_recent_file(&mut self, path: &str) {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return;
        }
        self.recent_files.retain(|p| p != trimmed);
        self.recent_files.insert(0, trimmed.to_owned());
        self.recent_files.truncate(MAX_RECENT_FILES);
    }

    /// Parse [`AppPreferences`] from JSON text.
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        serde_json::from_str::<Self>(raw).map_err(|e| format!("invalid preferences JSON: {e}"))
    }

    /// Serialize [`AppPreferences`] to formatted JSON.
    pub fn to_json_pretty(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| format!("failed serializing preferences: {e}"))
    }

    /// Default native path (`$XDG_CONFIG_HOME/layercraft/prefs.json` or `~/.config/layercraft/prefs.json`).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn default_prefs_path() -> Option<std::path::PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            return Some(
                std::path::PathBuf::from(xdg)
                    .join("layercraft")
                    .join("prefs.json"),
            );
        }
        std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| {
            std::path::PathBuf::from(h)
                .join(".config")
                .join("layercraft")
                .join("prefs.json")
        })
    }

    /// Load preferences from an explicit JSON file path (returns defaults if missing).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load_from_path(path: &std::path::Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed reading `{}`: {e}", path.display()))?;
        Self::from_json_str(&raw)
    }

    /// Save preferences to an explicit JSON file path, creating parent directories if needed.
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

    /// Load user preferences from the default native path, falling back cleanly on error.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load_from_disk() -> Self {
        let Some(path) = Self::default_prefs_path() else {
            return Self::default();
        };
        Self::load_from_path(&path).unwrap_or_default()
    }

    /// WASM preferences stub (returns defaults).
    #[cfg(target_arch = "wasm32")]
    pub fn load_from_disk() -> Self {
        Self::default()
    }

    /// Persist user preferences to the default native path.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn save_to_disk(&self) -> Result<(), String> {
        let Some(path) = Self::default_prefs_path() else {
            return Ok(());
        };
        self.save_to_path(&path)
    }

    /// WASM preferences persistence stub (no-op).
    #[cfg(target_arch = "wasm32")]
    pub fn save_to_disk(&self) -> Result<(), String> {
        Ok(())
    }
}
