//! Shared desktop and WebAssembly `egui` + `wgpu` slicer application (`LayerCraftApp`).

mod home;
mod icons;
pub mod prefs;
pub mod recent_projects;
mod sidebar;
mod theme;
mod toolbar;
mod ui_kit;
mod viewport_hud;

use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc;

use egui::{Align, Margin, RichText, Stroke, pos2};
use layercraft_config::{
    ModifierOverrides, ModifierRole, PrintConfig, ProfileApplyResult, ProfileKind, ProfileSummary,
    SupportStyle,
};
#[cfg(target_arch = "wasm32")]
use layercraft_engine::worker_protocol::{SliceJobEvent, SliceJobObject, SliceJobRequest};
use layercraft_engine::{
    HistoryActionResult, PlateRepairResult, PrinterAuth, PrinterProfile, ProjectLoadResult,
    SaveUserProfileResult, Session, execute, export_project_3mf_bytes,
};
#[cfg(not(target_arch = "wasm32"))]
use layercraft_engine::{
    PrinterError, PrinterStatusReport, UploadOptions, UploadProgress, UploadResult, probe_printer,
    upload_to_printer_with_progress,
};
use layercraft_gcode::{BgcodeOptions, encode_bgcode};
use layercraft_mesh::{ModifierPrimitive, SampleShape};
use layercraft_slice::{PrintabilityReport, PrintabilityRisk};
use layercraft_viewport::{
    CameraPreset, ColorMode, OrbitCamera, ViewportTheme, build_bed_grid_themed,
    build_bed_plate_mesh, build_mesh_vertices, build_toolpath_instances_with_config,
    ray_intersect_mesh,
};
use layercraft_viewport_gpu::{GpuSceneSnapshot, ViewportCallback};
use serde_json::Value;

use crate::sidebar::SidebarTab;
use crate::theme::ThemePalette;
pub use theme::ThemeMode;

/// Active workspace view mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Home,
    Prepare,
    Preview,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct GcodeLayerIndex {
    source_ptr: usize,
    source_len: usize,
    line_ranges: Vec<(usize, usize)>,
    layer_ranges: Vec<(usize, usize)>,
    layer_move_lines: Vec<Vec<usize>>,
}

impl GcodeLayerIndex {
    pub fn build(gcode: &str) -> Self {
        let mut line_ranges = Vec::new();
        let mut layer_ranges = Vec::new();
        let mut layer_move_lines = Vec::new();
        let mut current_start: Option<usize> = None;
        let mut current_moves: Vec<usize> = Vec::new();

        let mut pos = 0_usize;
        for raw_line in gcode.split('\n') {
            let trimmed_cr = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            let is_trailing_empty = trimmed_cr.is_empty() && pos == gcode.len();
            if !is_trailing_empty {
                let start = pos;
                let end = start + trimmed_cr.len();
                let line_idx = line_ranges.len();
                line_ranges.push((start, end));

                let trimmed = trimmed_cr.trim_start();
                if trimmed == ";LAYER_CHANGE" {
                    if let Some(prev_start) = current_start.take() {
                        layer_ranges.push((prev_start, line_idx));
                        layer_move_lines.push(std::mem::take(&mut current_moves));
                    }
                    current_start = Some(line_idx);
                } else if current_start.is_some()
                    && (trimmed.starts_with("G0 ")
                        || trimmed.starts_with("G1 ")
                        || trimmed.starts_with("G2 ")
                        || trimmed.starts_with("G3 "))
                {
                    current_moves.push(line_idx);
                }
            }
            pos = pos.saturating_add(raw_line.len()).saturating_add(1);
        }

        if let Some(start) = current_start {
            layer_ranges.push((start, line_ranges.len()));
            layer_move_lines.push(current_moves);
        }

        Self {
            source_ptr: gcode.as_ptr() as usize,
            source_len: gcode.len(),
            line_ranges,
            layer_ranges,
            layer_move_lines,
        }
    }

    pub fn matches(&self, gcode: &str) -> bool {
        self.source_ptr == gcode.as_ptr() as usize && self.source_len == gcode.len()
    }

    pub fn total_lines(&self) -> usize {
        self.line_ranges.len()
    }

    pub fn line_str<'a>(&self, gcode: &'a str, idx: usize) -> &'a str {
        self.line_ranges
            .get(idx)
            .and_then(|&(s, e)| gcode.get(s..e))
            .unwrap_or("")
    }

    pub fn layer_line_range(&self, layer_idx: usize) -> Option<(usize, usize)> {
        self.layer_ranges.get(layer_idx).copied()
    }

    pub fn active_move_line(
        &self,
        layer_idx: usize,
        cur_step: usize,
        step_count: usize,
    ) -> Option<usize> {
        let moves = self.layer_move_lines.get(layer_idx)?;
        if moves.is_empty() || cur_step == 0 || step_count == 0 {
            return None;
        }
        let max_idx = moves.len().saturating_sub(1);
        let denom = step_count.saturating_sub(1).max(1);
        let step0 = cur_step.saturating_sub(1).min(denom);
        let mapped = (step0.saturating_mul(max_idx)) / denom;
        moves.get(mapped).copied()
    }
}

#[cfg(not(target_arch = "wasm32"))]
enum SliceJobError {
    Cancelled,
    Failed(String),
}

#[cfg(not(target_arch = "wasm32"))]
struct PendingSliceOutcome {
    job_id: u64,
    elapsed_ms: f64,
    result: Result<(layercraft_slice::SlicedPlate, layercraft_gcode::GcodeOutput), SliceJobError>,
}

#[cfg(not(target_arch = "wasm32"))]
struct PendingSliceJob {
    rx: mpsc::Receiver<PendingSliceOutcome>,
    progress: Arc<layercraft_slice::SliceProgress>,
}

#[cfg(not(target_arch = "wasm32"))]
enum PendingPrinterOutcome {
    Probe {
        job_id: u64,
        result: Result<PrinterStatusReport, String>,
    },
    Upload {
        job_id: u64,
        result: Result<UploadResult, PrinterError>,
    },
}

#[cfg(not(target_arch = "wasm32"))]
struct PendingPrinterJob {
    rx: mpsc::Receiver<PendingPrinterOutcome>,
    upload_progress: Option<Arc<UploadProgress>>,
}

/// Frame callback invoked before UI layout to pump external controllers (e.g. Live MCP bridge).
pub type ExternalPoller = Box<dyn FnMut(&mut LayerCraftApp)>;

fn default_printer_draft() -> PrinterProfile {
    PrinterProfile {
        auth: PrinterAuth::Digest {
            username: "maker".to_owned(),
            password: String::new(),
        },
        ..PrinterProfile::default()
    }
}

fn slugify_printer_id(name: &str) -> String {
    let mut slug: String = name
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "printer-1".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Viewport interaction tool mode (deferred 3D gizmos / face painting extend this enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewportToolMode {
    #[default]
    Select,
}

#[cfg(target_arch = "wasm32")]
type PendingWebDrops =
    std::rc::Rc<std::cell::RefCell<Vec<(WebFilePickMode, String, Result<Vec<u8>, String>)>>>;

#[cfg(target_arch = "wasm32")]
type PendingBrowserRecent =
    std::rc::Rc<std::cell::RefCell<Option<(String, String, Result<Option<Vec<u8>>, String>)>>>;

#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebFilePickMode {
    OpenReplace,
    AddToPlate,
    ImportProfileJson,
}

#[cfg(target_arch = "wasm32")]
enum WebSliceOutcome {
    Done {
        job_id: u64,
        elapsed_ms: f64,
        plate: layercraft_slice::SlicedPlate,
        gcode: layercraft_gcode::GcodeOutput,
    },
    Error {
        job_id: u64,
        message: String,
    },
}

#[cfg(target_arch = "wasm32")]
struct PendingWebSliceJob {
    worker: web_sys::Worker,
    _on_message: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_error: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::ErrorEvent)>,
    progress: std::rc::Rc<std::cell::Cell<(usize, usize)>>,
    outcome: std::rc::Rc<std::cell::RefCell<Option<WebSliceOutcome>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingDiscardAction {
    ClearPlate,
    NewEmptyProject,
    LoadSample(SampleShape),
    OpenFileDialog,
    OpenRecentId(String),
}

#[derive(Debug, Clone)]
pub(crate) struct SavePresetDialogState {
    pub kind: ProfileKind,
    pub name: String,
    pub error: Option<String>,
}

/// Shared application state driving both native `eframe` and browser `WebRunner`.
pub struct LayerCraftApp {
    pub(crate) session: Session,
    pub(crate) camera: OrbitCamera,
    pub(crate) view_mode: ViewMode,
    pub(crate) color_mode: ColorMode,
    pub(crate) max_layer: usize,
    pub(crate) single_layer_only: bool,
    pub(crate) show_ghost_mesh: bool,
    pub(crate) show_bed_grid: bool,
    pub(crate) show_travels: bool,
    pub(crate) layer_move_step: usize,
    pub(crate) playback_playing: bool,
    pub(crate) playback_speed: f32,
    pub(crate) playback_accum: f64,
    pub(crate) last_playback_time_s: Option<f64>,
    pub(crate) selected_sample: SampleShape,
    pub(crate) selected_object_id: Option<u64>,
    pub(crate) selected_modifier_id: Option<u64>,
    pub(crate) selected_machine_profile: Option<String>,
    pub(crate) selected_filament_profile: Option<String>,
    pub(crate) selected_process_profile: Option<String>,
    pub(crate) profile_warning_count: usize,
    pub(crate) status_message: String,
    pub(crate) last_error: Option<String>,
    pub(crate) scene_revision: u64,
    pub(crate) scene: Arc<GpuSceneSnapshot>,
    pub(crate) viewport_aspect: f32,
    pub(crate) theme_mode: ThemeMode,
    pub(crate) styles_installed: bool,
    pub(crate) sidebar_open: bool,
    pub(crate) narrow_layout_applied: bool,
    pub(crate) sidebar_tab: SidebarTab,
    pub(crate) last_settings_tab: SidebarTab,
    pub(crate) settings_search: String,
    pub(crate) home_search: String,
    pub(crate) show_legend_hud: bool,
    pub(crate) show_all_legend_roles: bool,
    pub show_gcode_viewer: bool,
    pub gcode_viewer_all_layers: bool,
    pub(crate) gcode_index: GcodeLayerIndex,
    pub(crate) last_scene_dark: bool,
    pub(crate) external_poller: Option<ExternalPoller>,
    pub(crate) bridge_endpoint: Option<String>,
    pub(crate) last_mcp_activity: Option<String>,
    pub(crate) pending_config_undo: Option<PrintConfig>,
    pub(crate) drag_undo_active: bool,
    pub(crate) pointer_active: bool,
    pub(crate) plate_rename_buf: (u32, String),
    pub(crate) printer_draft: PrinterProfile,
    pub tool_mode: ViewportToolMode,
    pub prefs: prefs::AppPreferences,
    pub recent_projects: recent_projects::RecentProjectsStore,
    pub(crate) recent_thumb_textures:
        std::collections::HashMap<String, Option<egui::TextureHandle>>,
    pub(crate) current_recent_id: Option<String>,
    pub(crate) current_project_title: Option<String>,
    pub printability_report: Option<PrintabilityReport>,
    pub project_dirty: bool,
    pub(crate) pending_discard_action: Option<PendingDiscardAction>,
    pub(crate) save_preset_dialog: Option<SavePresetDialogState>,
    pub(crate) last_window_inner_size: Option<[f32; 2]>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) printer_status_report: Option<PrinterStatusReport>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) show_upload_dialog: bool,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) upload_filename: String,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) upload_binary: bool,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) upload_overwrite: bool,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) upload_start_print: bool,
    pub(crate) last_slice_ms: Option<f64>,
    #[cfg(not(target_arch = "wasm32"))]
    pending_slice: Option<PendingSliceJob>,
    next_slice_job_id: u64,
    #[cfg(not(target_arch = "wasm32"))]
    pending_printer_job: Option<PendingPrinterJob>,
    #[cfg(not(target_arch = "wasm32"))]
    next_printer_job_id: u64,
    #[cfg(target_arch = "wasm32")]
    pending_web_slice: Option<PendingWebSliceJob>,
    #[cfg(target_arch = "wasm32")]
    last_ctx: Option<egui::Context>,
    #[cfg(target_arch = "wasm32")]
    file_picker_cb: Option<wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>>,
    #[cfg(target_arch = "wasm32")]
    pending_drops: PendingWebDrops,
    #[cfg(target_arch = "wasm32")]
    pending_browser_recent: PendingBrowserRecent,
}

impl Default for LayerCraftApp {
    fn default() -> Self {
        Self::new(Some(SampleShape::Cube20))
    }
}

impl LayerCraftApp {
    /// Construct a new UI session, optionally preloading a sample shape.
    pub fn new(initial_sample: Option<SampleShape>) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let mut session = {
            let mut s = Session::default();
            let _ = s.load_printers_from_disk();
            #[cfg(not(test))]
            let _ = s.load_user_profiles_from_disk();
            s
        };
        #[cfg(target_arch = "wasm32")]
        let mut session = {
            let mut s = Session::default();
            let _ = prefs::load_user_profiles_from_browser(&mut s.profiles);
            s
        };

        #[cfg(not(test))]
        let loaded_prefs = prefs::AppPreferences::load_from_disk();
        #[cfg(test)]
        let loaded_prefs = prefs::AppPreferences::default();

        #[cfg(not(test))]
        let loaded_recent = {
            let mut store = recent_projects::RecentProjectsStore::load_from_disk();
            if store.migrate_legacy_recent_files(&loaded_prefs.recent_files) {
                let _ = store.save_to_disk();
            }
            store
        };
        #[cfg(test)]
        let loaded_recent = recent_projects::RecentProjectsStore::default();

        let initial_theme = match loaded_prefs
            .theme_mode
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("light") => ThemeMode::Light,
            Some("system") => ThemeMode::System,
            _ => ThemeMode::Dark,
        };
        let initial_scene_dark = initial_theme != ThemeMode::Light;

        let valid_profile = |lib: &layercraft_config::ProfileLibrary,
                             kind: ProfileKind,
                             slot: &Option<String>|
         -> Option<String> {
            slot.as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty() && lib.get(kind, name).is_some())
                .map(str::to_owned)
        };
        let saved_machine = valid_profile(
            &session.profiles,
            ProfileKind::Machine,
            &loaded_prefs.active_profiles.machine,
        );
        let saved_filament = valid_profile(
            &session.profiles,
            ProfileKind::Filament,
            &loaded_prefs.active_profiles.filament,
        );
        let saved_process = valid_profile(
            &session.profiles,
            ProfileKind::Process,
            &loaded_prefs.active_profiles.process,
        );

        let mut initial_warnings = 0usize;
        if (saved_machine.is_some() || saved_filament.is_some() || saved_process.is_some())
            && let Ok(val) = execute(
                &mut session,
                "profile.apply",
                serde_json::json!({
                    "machine": saved_machine,
                    "filament": saved_filament,
                    "process": saved_process,
                    "reset_to_defaults": true
                }),
            )
        {
            initial_warnings = serde_json::from_value::<ProfileApplyResult>(val)
                .map(|r| r.warnings.len())
                .unwrap_or(0);
            session.clear_history();
        }
        let applied_machine = session.active_profiles.machine.clone();
        let applied_filament = session.active_profiles.filament.clone();
        let applied_process = session.active_profiles.process.clone();

        let initial_printer_draft = session
            .printers
            .first()
            .cloned()
            .unwrap_or_else(default_printer_draft);
        let mut app = Self {
            session,
            camera: OrbitCamera::default(),
            view_mode: ViewMode::Prepare,
            color_mode: ColorMode::Feature,
            max_layer: 0,
            single_layer_only: false,
            show_ghost_mesh: false,
            show_bed_grid: true,
            show_travels: false,
            layer_move_step: 0,
            playback_playing: false,
            playback_speed: 1.0,
            playback_accum: 0.0,
            last_playback_time_s: None,
            selected_sample: initial_sample.unwrap_or(SampleShape::Cube20),
            selected_object_id: None,
            selected_modifier_id: None,
            selected_machine_profile: applied_machine,
            selected_filament_profile: applied_filament,
            selected_process_profile: applied_process,
            profile_warning_count: initial_warnings,
            status_message: "Drop an .stl or .3mf onto the window, or load a sample part."
                .to_owned(),
            last_error: None,
            scene_revision: 0,
            scene: Arc::new(GpuSceneSnapshot::default()),
            viewport_aspect: 1.5,
            theme_mode: initial_theme,
            styles_installed: false,
            sidebar_open: true,
            narrow_layout_applied: false,
            sidebar_tab: SidebarTab::Plate,
            last_settings_tab: SidebarTab::Quality,
            settings_search: String::new(),
            home_search: String::new(),
            show_legend_hud: true,
            show_all_legend_roles: false,
            show_gcode_viewer: false,
            gcode_viewer_all_layers: false,
            gcode_index: GcodeLayerIndex::default(),
            last_scene_dark: initial_scene_dark,
            external_poller: None,
            bridge_endpoint: None,
            last_mcp_activity: None,
            pending_config_undo: None,
            drag_undo_active: false,
            pointer_active: false,
            plate_rename_buf: (0, String::new()),
            printer_draft: initial_printer_draft,
            tool_mode: ViewportToolMode::Select,
            prefs: loaded_prefs,
            recent_projects: loaded_recent,
            recent_thumb_textures: std::collections::HashMap::new(),
            current_recent_id: None,
            current_project_title: initial_sample.map(|s| s.label().to_owned()),
            printability_report: None,
            project_dirty: false,
            pending_discard_action: None,
            save_preset_dialog: None,
            last_window_inner_size: None,
            #[cfg(not(target_arch = "wasm32"))]
            printer_status_report: None,
            #[cfg(not(target_arch = "wasm32"))]
            show_upload_dialog: false,
            #[cfg(not(target_arch = "wasm32"))]
            upload_filename: "layercraft_out.bgcode".to_owned(),
            #[cfg(not(target_arch = "wasm32"))]
            upload_binary: true,
            #[cfg(not(target_arch = "wasm32"))]
            upload_overwrite: true,
            #[cfg(not(target_arch = "wasm32"))]
            upload_start_print: false,
            last_slice_ms: None,
            #[cfg(not(target_arch = "wasm32"))]
            pending_slice: None,
            next_slice_job_id: 0,
            #[cfg(not(target_arch = "wasm32"))]
            pending_printer_job: None,
            #[cfg(not(target_arch = "wasm32"))]
            next_printer_job_id: 0,
            #[cfg(target_arch = "wasm32")]
            pending_web_slice: None,
            #[cfg(target_arch = "wasm32")]
            last_ctx: None,
            #[cfg(target_arch = "wasm32")]
            file_picker_cb: None,
            #[cfg(target_arch = "wasm32")]
            pending_drops: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            #[cfg(target_arch = "wasm32")]
            pending_browser_recent: std::rc::Rc::new(std::cell::RefCell::new(None)),
        };
        if let Some(sample) = initial_sample {
            app.load_sample(sample);
            app.session.clear_history();
            app.project_dirty = false;
        } else {
            app.rebuild_scene();
        }
        app
    }

    /// Construct a new Studio session starting on the Home / Recent Projects hub.
    pub fn new_home() -> Self {
        let mut app = Self::new(None);
        app.view_mode = ViewMode::Home;
        app.status_message =
            "Open a recent project, drop a .3mf or .stl file, or pick a Quick Start shape."
                .to_owned();
        app
    }

    /// Set the active color theme mode (`Dark`, `Light`, or `System`).
    pub fn set_theme_mode(&mut self, mode: ThemeMode) {
        self.theme_mode = mode;
    }

    /// Install an external frame poller callback (used by native Studio to pump IPC bridge requests).
    pub fn set_external_poller(&mut self, poller: ExternalPoller) {
        self.external_poller = Some(poller);
    }

    /// Update the live MCP bridge indicator in the Studio status bar.
    pub fn set_bridge_status(&mut self, endpoint: Option<String>, last_activity: Option<String>) {
        if endpoint.is_some() {
            self.bridge_endpoint = endpoint;
        }
        if last_activity.is_some() {
            self.last_mcp_activity = last_activity;
        }
    }

    /// Immutable reference to the underlying [`Session`].
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Mutable reference to the underlying [`Session`].
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// Snapshot of the current 3D orbit camera.
    pub fn camera(&self) -> OrbitCamera {
        self.camera
    }

    /// Total move steps available inside the active layer (`0` if unsliced or empty).
    pub(crate) fn active_layer_step_count(&self) -> usize {
        self.scene
            .layer_steps
            .get(self.max_layer)
            .map(Vec::len)
            .unwrap_or(0)
    }

    /// Select active preview layer and reset intra-layer move scrubber to 100% of that layer.
    pub(crate) fn set_active_layer(&mut self, layer: usize) {
        let total = self.scene.layer_ranges.len();
        self.max_layer = if total == 0 { 0 } else { layer.min(total - 1) };
        self.layer_move_step = self.active_layer_step_count();
        self.playback_playing = false;
        self.playback_accum = 0.0;
        self.last_playback_time_s = None;
    }

    /// Synchronize UI view state from an external controller (e.g. MCP `set_view` / `screenshot`).
    pub fn apply_view_sync(
        &mut self,
        view_mode: Option<ViewMode>,
        camera: Option<CameraPreset>,
        max_layer: Option<usize>,
        single_layer_only: Option<bool>,
        color_mode: Option<ColorMode>,
    ) {
        if let Some(m) = view_mode {
            self.view_mode = m;
        }
        if let Some(c) = camera {
            self.frame_camera(c);
        }
        if let Some(cm) = color_mode
            && self.color_mode != cm
        {
            self.color_mode = cm;
            self.rebuild_scene();
        }
        if let Some(layer) = max_layer {
            self.set_active_layer(layer);
        }
        if let Some(single) = single_layer_only {
            self.single_layer_only = single;
        }
    }

    /// Synchronize sidebar selections, view mode, and 3D scene after an external engine mutation.
    pub fn sync_after_external_command(&mut self, command: &str) {
        self.cancel_pending_slice();
        self.last_error = None;
        self.selected_machine_profile = self.session.active_profiles.machine.clone();
        self.selected_filament_profile = self.session.active_profiles.filament.clone();
        self.selected_process_profile = self.session.active_profiles.process.clone();

        let obj_still_exists = self
            .selected_object_id
            .is_some_and(|id| self.session.objects.iter().any(|o| o.id == id));
        if !obj_still_exists {
            self.selected_object_id = self.session.objects.last().map(|o| o.id);
        }
        let mod_still_exists = self
            .selected_modifier_id
            .is_some_and(|id| self.session.modifiers.iter().any(|m| m.id == id));
        if !mod_still_exists {
            self.selected_modifier_id = self.session.modifiers.last().map(|m| m.id);
        }

        let sliced_just_now = (command == "slice.run" || command == "plate.analyze_printability")
            && self.session.sliced.is_some();

        if command == "plate.clear" {
            self.view_mode = ViewMode::Prepare;
            self.max_layer = 0;
            self.layer_move_step = 0;
            self.playback_playing = false;
            self.status_message = "Cleared plate via MCP.".to_owned();
        } else if command == "project.load_file" || command == "project.load_3mf_bytes" {
            self.view_mode = ViewMode::Prepare;
            self.max_layer = 0;
            self.layer_move_step = 0;
            self.playback_playing = false;
            self.frame_camera(CameraPreset::Isometric);
            self.status_message = if self.session.plate_count() > 1 {
                format!(
                    "Loaded {} plates via MCP ({command}).",
                    self.session.plate_count()
                )
            } else {
                format!("Loaded project via MCP ({command}).")
            };
        } else if command.starts_with("plates.") {
            self.max_layer = 0;
            self.layer_move_step = 0;
            self.playback_playing = false;
            if self.session.sliced.is_none() {
                self.view_mode = ViewMode::Prepare;
            }
            self.status_message = format!(
                "Plate `{}` ({} object(s)) via MCP.",
                self.session.active_plate_name(),
                self.session.objects.len()
            );
        } else if command == "session.undo" || command == "session.redo" {
            if self.session.sliced.is_none() {
                self.view_mode = ViewMode::Prepare;
            }
            self.status_message = format!("Updated session via MCP ({command}).");
        } else if command == "plate.repair_mesh" {
            if self.session.sliced.is_none() {
                self.view_mode = ViewMode::Prepare;
            }
            self.status_message = "Repaired mesh via MCP.".to_owned();
        } else if command.starts_with("plate.add_") || command == "plate.auto_arrange" {
            if self.session.sliced.is_none() {
                self.view_mode = ViewMode::Prepare;
            }
            self.frame_camera(CameraPreset::Isometric);
            self.status_message = format!("Updated plate via MCP ({command}).");
        } else if sliced_just_now {
            let layers = self
                .session
                .sliced
                .as_ref()
                .map(|s| s.layers.len())
                .unwrap_or(0);
            self.view_mode = ViewMode::Preview;
            self.status_message = format!("Sliced {layers} layers via MCP.");
        } else {
            self.status_message = format!("Updated via MCP ({command}).");
        }

        self.rebuild_scene();
        if sliced_just_now {
            self.refresh_printability_after_slice();
        } else if command == "plate.clear"
            || command == "project.load_file"
            || command == "project.load_3mf_bytes"
        {
            self.printability_report = None;
            self.project_dirty = false;
        } else if !command.starts_with("layer.")
            && !command.starts_with("session.")
            && !command.starts_with("plate.inspect")
            && !command.starts_with("plate.check_layout")
        {
            self.printability_report = None;
        }
        if sliced_just_now
            || (command == "plates.select"
                && self.session.sliced.is_some()
                && self.view_mode == ViewMode::Preview)
        {
            let layers = self.scene.layer_ranges.len();
            self.set_active_layer(layers.saturating_sub(1));
        }
    }

    fn sync_selections_after_history(&mut self) {
        self.selected_machine_profile = self.session.active_profiles.machine.clone();
        self.selected_filament_profile = self.session.active_profiles.filament.clone();
        self.selected_process_profile = self.session.active_profiles.process.clone();

        if !self
            .selected_object_id
            .is_some_and(|id| self.session.objects.iter().any(|o| o.id == id))
        {
            self.selected_object_id = self.session.objects.last().map(|o| o.id);
        }
        if !self
            .selected_modifier_id
            .is_some_and(|id| self.session.modifiers.iter().any(|m| m.id == id))
        {
            self.selected_modifier_id = self.session.modifiers.last().map(|m| m.id);
        }
        if self.session.sliced.is_none() {
            self.view_mode = ViewMode::Prepare;
            self.max_layer = 0;
            self.layer_move_step = 0;
            self.playback_playing = false;
        }
    }

    pub(crate) fn undo_action(&mut self) {
        self.cancel_pending_slice();
        if let Some(prev_cfg) = self.pending_config_undo.take()
            && self.session.config != prev_cfg
        {
            self.session
                .push_config_undo_snapshot("Edit settings", prev_cfg);
        }
        match execute(&mut self.session, "session.undo", Value::Null) {
            Ok(val) => {
                if let Ok(res) = serde_json::from_value::<HistoryActionResult>(val)
                    && res.applied
                {
                    self.last_error = None;
                    self.printability_report = None;
                    self.project_dirty = true;
                    self.sync_selections_after_history();
                    let what = res.label.unwrap_or_else(|| "change".to_owned());
                    self.status_message = format!("Undid `{what}`.");
                    self.rebuild_scene();
                }
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    pub(crate) fn redo_action(&mut self) {
        self.cancel_pending_slice();
        match execute(&mut self.session, "session.redo", Value::Null) {
            Ok(val) => {
                if let Ok(res) = serde_json::from_value::<HistoryActionResult>(val)
                    && res.applied
                {
                    self.last_error = None;
                    self.printability_report = None;
                    self.project_dirty = true;
                    self.sync_selections_after_history();
                    let what = res.label.unwrap_or_else(|| "change".to_owned());
                    self.status_message = format!("Redid `{what}`.");
                    self.rebuild_scene();
                }
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    /// Pick the closest 3D object or modifier intersecting screen ray at NDC `(ndc_x, ndc_y)`.
    pub(crate) fn pick_at_ndc(&mut self, ndc_x: f32, ndc_y: f32) -> bool {
        let ray = self.camera.screen_ray(ndc_x, ndc_y, self.viewport_aspect);
        enum PickTarget {
            Object(u64, String),
            Modifier(u64, String),
        }
        let mut best: Option<(f32, PickTarget)> = None;

        for obj in &self.session.objects {
            if let Some(t) = ray_intersect_mesh(ray, &obj.prepared_mesh)
                && best.as_ref().is_none_or(|(bt, _)| t < *bt)
            {
                best = Some((t, PickTarget::Object(obj.id, obj.name.clone())));
            }
        }
        for m in &self.session.modifiers {
            if let Some(t) = ray_intersect_mesh(ray, &m.prepared_mesh)
                && best.as_ref().is_none_or(|(bt, _)| t < *bt)
            {
                best = Some((t, PickTarget::Modifier(m.id, m.name.clone())));
            }
        }

        match best {
            Some((_, PickTarget::Object(id, name))) => {
                self.selected_object_id = Some(id);
                self.selected_modifier_id = None;
                self.sidebar_tab = SidebarTab::Plate;
                self.status_message = format!("Selected object #{id} `{name}`.");
                self.rebuild_scene();
                true
            }
            Some((_, PickTarget::Modifier(id, name))) => {
                self.selected_modifier_id = Some(id);
                self.selected_object_id = None;
                self.sidebar_tab = SidebarTab::Plate;
                self.status_message = format!("Selected modifier #{id} `{name}`.");
                self.rebuild_scene();
                true
            }
            None => {
                if self.selected_object_id.is_some() || self.selected_modifier_id.is_some() {
                    self.selected_object_id = None;
                    self.selected_modifier_id = None;
                    self.rebuild_scene();
                }
                false
            }
        }
    }

    /// Switch active build plate by zero-based index.
    pub fn select_plate(&mut self, index: usize) {
        self.cancel_pending_slice();
        match execute(
            &mut self.session,
            "plates.select",
            serde_json::json!({ "index": index }),
        ) {
            Ok(_) => {
                self.last_error = None;
                self.printability_report = None;
                self.playback_playing = false;
                self.layer_move_step = 0;
                self.selected_object_id = self.session.objects.last().map(|o| o.id);
                self.selected_modifier_id = self.session.modifiers.last().map(|m| m.id);
                if self.session.sliced.is_none() {
                    self.view_mode = ViewMode::Prepare;
                    self.max_layer = 0;
                }
                self.status_message = format!(
                    "Active plate: `{}` ({} object(s)).",
                    self.session.active_plate_name(),
                    self.session.objects.len()
                );
                self.rebuild_scene();
                if self.session.sliced.is_some() && self.view_mode == ViewMode::Preview {
                    let layers = self.scene.layer_ranges.len();
                    self.set_active_layer(layers.saturating_sub(1));
                }
            }
            Err(e) => {
                self.last_error = Some(format!("Failed selecting plate: {e}"));
            }
        }
    }

    /// Add a new empty build plate and switch to it.
    pub fn add_plate(&mut self, name: Option<&str>) {
        self.cancel_pending_slice();
        match execute(
            &mut self.session,
            "plates.add",
            serde_json::json!({ "name": name, "select": true }),
        ) {
            Ok(_) => {
                self.last_error = None;
                self.printability_report = None;
                self.project_dirty = true;
                self.selected_object_id = None;
                self.selected_modifier_id = None;
                self.view_mode = ViewMode::Prepare;
                self.max_layer = 0;
                self.layer_move_step = 0;
                self.playback_playing = false;
                self.status_message =
                    format!("Added plate `{}`.", self.session.active_plate_name());
                self.rebuild_scene();
            }
            Err(e) => {
                self.last_error = Some(format!("Failed adding plate: {e}"));
            }
        }
    }

    /// Rename a build plate by zero-based index.
    pub fn rename_plate(&mut self, index: usize, name: &str) {
        match execute(
            &mut self.session,
            "plates.rename",
            serde_json::json!({ "index": index, "name": name }),
        ) {
            Ok(_) => {
                self.last_error = None;
                self.status_message =
                    format!("Renamed plate to `{}`.", self.session.active_plate_name());
            }
            Err(e) => {
                self.last_error = Some(format!("Failed renaming plate: {e}"));
            }
        }
    }

    /// Remove a build plate by zero-based index (keeps at least one plate).
    pub fn remove_plate(&mut self, index: usize) {
        self.cancel_pending_slice();
        match execute(
            &mut self.session,
            "plates.remove",
            serde_json::json!({ "index": index }),
        ) {
            Ok(_) => {
                self.last_error = None;
                self.printability_report = None;
                self.project_dirty = true;
                self.playback_playing = false;
                self.layer_move_step = 0;
                self.selected_object_id = self.session.objects.last().map(|o| o.id);
                self.selected_modifier_id = self.session.modifiers.last().map(|m| m.id);
                if self.session.sliced.is_none() {
                    self.view_mode = ViewMode::Prepare;
                    self.max_layer = 0;
                }
                self.status_message = format!(
                    "Removed plate; active plate is `{}` ({} object(s)).",
                    self.session.active_plate_name(),
                    self.session.objects.len()
                );
                self.rebuild_scene();
                if self.session.sliced.is_some() && self.view_mode == ViewMode::Preview {
                    let layers = self.scene.layer_ranges.len();
                    self.set_active_layer(layers.saturating_sub(1));
                }
            }
            Err(e) => {
                self.last_error = Some(format!("Failed removing plate: {e}"));
            }
        }
    }

    #[cfg(test)]
    fn default_gcode_filename(&self) -> String {
        self.session
            .default_export_filename(self.session.config.machine.binary_gcode)
    }

    pub(crate) fn save_printer_draft(&mut self) {
        let mut draft = self.printer_draft.clone();
        if draft.id.trim().is_empty() {
            draft.id = slugify_printer_id(&draft.name);
        }
        if draft.name.trim().is_empty() {
            draft.name = draft.id.clone();
        }
        self.printer_draft = draft.clone();
        match execute(
            &mut self.session,
            "printer.upsert",
            serde_json::json!({ "profile": draft, "persist": true }),
        ) {
            Ok(_) => {
                self.last_error = None;
                self.status_message = format!("Saved printer `{}`.", self.printer_draft.name);
            }
            Err(e) => {
                self.last_error = Some(format!("Failed saving printer: {e}"));
            }
        }
    }

    pub(crate) fn delete_selected_printer(&mut self) {
        let target = self.printer_draft.id.clone();
        match execute(
            &mut self.session,
            "printer.remove",
            serde_json::json!({ "id": target, "persist": true }),
        ) {
            Ok(_) => {
                self.last_error = None;
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.printer_status_report = None;
                }
                self.printer_draft = self
                    .session
                    .printers
                    .first()
                    .cloned()
                    .unwrap_or_else(default_printer_draft);
                self.status_message = "Removed saved printer.".to_owned();
            }
            Err(e) => {
                self.last_error = Some(format!("Failed removing printer: {e}"));
            }
        }
    }

    pub(crate) fn select_saved_printer(&mut self, id: &str) {
        if let Some(found) = self.session.printers.iter().find(|p| p.id == id).cloned() {
            self.printer_draft = found;
            #[cfg(not(target_arch = "wasm32"))]
            {
                self.printer_status_report = None;
            }
        }
    }

    pub(crate) fn start_new_printer_draft(&mut self) {
        let idx = self.session.printers.len() + 1;
        let mut draft = default_printer_draft();
        draft.id = format!("printer-{idx}");
        draft.name = format!("Printer {idx}");
        self.printer_draft = draft;
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.printer_status_report = None;
        }
    }

    /// Load a mesh or LayerCraft `.3mf` project from raw STL or 3MF bytes via `layercraft-engine`.
    pub fn load_mesh_bytes(&mut self, name: &str, bytes: Vec<u8>) {
        self.load_mesh_bytes_with_mode(name, bytes, false);
    }

    /// Load or append a mesh or `.3mf` file from raw bytes (`append: false` replaces, `append: true` adds to active plate).
    pub fn load_mesh_bytes_with_mode(&mut self, name: &str, bytes: Vec<u8>, append: bool) {
        self.load_mesh_bytes_inner(name, bytes, append, true);
    }

    fn load_mesh_bytes_inner(
        &mut self,
        name: &str,
        bytes: Vec<u8>,
        append: bool,
        record_recent: bool,
    ) {
        self.cancel_pending_slice();
        let is_3mf = name.to_ascii_lowercase().ends_with(".3mf");
        if is_3mf {
            let cmd = if append {
                "plate.add_3mf_bytes"
            } else {
                "project.load_3mf_bytes"
            };
            let params = if append {
                serde_json::json!({ "bytes": bytes, "name": name, "append": true })
            } else {
                serde_json::json!({ "bytes": bytes, "name": name })
            };
            match execute(&mut self.session, cmd, params) {
                Ok(val) => {
                    if !append {
                        let res = serde_json::from_value::<ProjectLoadResult>(val).ok();
                        let plate_count = res
                            .as_ref()
                            .map(|r| r.plate_count)
                            .unwrap_or_else(|| self.session.plate_count());
                        if res.as_ref().is_some_and(|r| r.is_layercraft_project) {
                            self.selected_machine_profile =
                                self.session.active_profiles.machine.clone();
                            self.selected_filament_profile =
                                self.session.active_profiles.filament.clone();
                            self.selected_process_profile =
                                self.session.active_profiles.process.clone();
                            if plate_count > 1 {
                                self.status_message = format!(
                                    "Loaded project `{name}` ({plate_count} plates, active `{}`: {} object(s), {} modifier(s)).",
                                    self.session.active_plate_name(),
                                    self.session.objects.len(),
                                    self.session.modifiers.len()
                                );
                            } else {
                                self.status_message = format!(
                                    "Loaded project `{name}` ({} object(s), {} modifier(s)).",
                                    self.session.objects.len(),
                                    self.session.modifiers.len()
                                );
                            }
                        } else if plate_count > 1 {
                            self.status_message = format!(
                                "Loaded `{name}` ({plate_count} plates, active `{}`: {} object(s)).",
                                self.session.active_plate_name(),
                                self.session.objects.len()
                            );
                        } else {
                            self.status_message = format!(
                                "Loaded `{name}` ({} object(s)).",
                                self.session.objects.len()
                            );
                        }
                        self.project_dirty = !res.as_ref().is_some_and(|r| r.is_layercraft_project);
                        self.current_project_title =
                            Some(recent_projects::display_title_from_name(name));
                    } else {
                        self.status_message = format!(
                            "Added `{name}` to plate ({} object(s)).",
                            self.session.objects.len()
                        );
                        self.project_dirty = true;
                    }
                    self.last_error = None;
                    self.append_repair_warning_to_status();
                    self.printability_report = None;
                    self.selected_object_id = self.session.objects.last().map(|o| o.id);
                    self.selected_modifier_id = None;
                    self.view_mode = ViewMode::Prepare;
                    self.max_layer = 0;
                    self.layer_move_step = 0;
                    self.playback_playing = false;
                    self.frame_camera(CameraPreset::Isometric);
                    self.rebuild_scene();
                    #[cfg(target_arch = "wasm32")]
                    if !append && record_recent {
                        let fmt = recent_projects::infer_file_format(name);
                        self.record_recent_browser(name, &fmt);
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    let _ = record_recent;
                }
                Err(e) => {
                    self.last_error = Some(format!("Failed to load `{name}`: {e}"));
                }
            }
            return;
        }

        match execute(
            &mut self.session,
            "plate.add_stl_bytes",
            serde_json::json!({ "bytes": bytes, "name": name, "append": append }),
        ) {
            Ok(_) => {
                self.last_error = None;
                self.printability_report = None;
                self.project_dirty = true;
                if !append {
                    self.current_project_title =
                        Some(recent_projects::display_title_from_name(name));
                }
                self.selected_object_id = self.session.objects.last().map(|o| o.id);
                self.selected_modifier_id = None;
                self.view_mode = ViewMode::Prepare;
                self.status_message = if append {
                    format!("Added `{name}` to plate.")
                } else {
                    format!("Loaded `{name}`.")
                };
                self.append_repair_warning_to_status();
                self.frame_camera(CameraPreset::Isometric);
                self.rebuild_scene();
                #[cfg(target_arch = "wasm32")]
                if !append && record_recent {
                    let fmt = recent_projects::infer_file_format(name);
                    self.record_recent_browser(name, &fmt);
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = record_recent;
            }
            Err(e) => {
                self.last_error = Some(format!("Failed to load `{name}`: {e}"));
            }
        }
    }

    fn apply_loaded_profile_selection(&mut self, loaded: &[ProfileSummary]) {
        for summary in loaded.iter().filter(|s| s.instantiation) {
            match summary.kind {
                ProfileKind::Machine => {
                    self.selected_machine_profile = Some(summary.name.clone());
                }
                ProfileKind::Filament => {
                    self.selected_filament_profile = Some(summary.name.clone());
                }
                ProfileKind::Process => {
                    self.selected_process_profile = Some(summary.name.clone());
                }
                ProfileKind::MachineModel => {}
            }
        }
    }

    /// Load JSON profile(s) from raw UTF-8 bytes via `profile.load_json`.
    pub fn load_profile_json_bytes(&mut self, name: &str, bytes: Vec<u8>) {
        let json = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => {
                self.last_error = Some(format!("Invalid UTF-8 in profile `{name}`: {e}"));
                return;
            }
        };
        match execute(
            &mut self.session,
            "profile.load_json",
            serde_json::json!({ "json": json }),
        ) {
            Ok(val) => {
                let loaded: Vec<ProfileSummary> = serde_json::from_value(val).unwrap_or_default();
                self.apply_loaded_profile_selection(&loaded);
                #[cfg(target_arch = "wasm32")]
                {
                    let _ = prefs::save_user_profiles_to_browser(&self.session.profiles);
                }
                self.last_error = None;
                self.status_message = format!("Loaded {} profile(s) from `{name}`.", loaded.len());
            }
            Err(e) => {
                self.last_error = Some(format!("Failed loading profile `{name}`: {e}"));
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn load_native_profile_path(&mut self, path: &str) {
        let p = std::path::Path::new(path);
        if p.is_dir() {
            match execute(
                &mut self.session,
                "profile.load_dir",
                serde_json::json!({ "path": path }),
            ) {
                Ok(val) => {
                    let report: layercraft_config::ProfileDirLoadReport =
                        match serde_json::from_value(val) {
                            Ok(r) => r,
                            Err(e) => {
                                self.last_error = Some(e.to_string());
                                return;
                            }
                        };
                    self.apply_loaded_profile_selection(&report.loaded);
                    self.last_error = None;
                    self.status_message = format!(
                        "Loaded {} profile(s) from `{path}` ({} skipped).",
                        report.loaded.len(),
                        report.skipped.len()
                    );
                }
                Err(e) => {
                    self.last_error = Some(format!("Failed loading profiles `{path}`: {e}"));
                }
            }
        } else {
            match execute(
                &mut self.session,
                "profile.load_file",
                serde_json::json!({ "path": path }),
            ) {
                Ok(val) => {
                    let loaded: Vec<ProfileSummary> =
                        serde_json::from_value(val).unwrap_or_default();
                    self.apply_loaded_profile_selection(&loaded);
                    self.last_error = None;
                    self.status_message =
                        format!("Loaded {} profile(s) from `{path}`.", loaded.len());
                }
                Err(e) => {
                    self.last_error = Some(format!("Failed loading profile `{path}`: {e}"));
                }
            }
        }
    }

    pub(crate) fn apply_selected_profiles(&mut self) {
        self.cancel_pending_slice();
        match execute(
            &mut self.session,
            "profile.apply",
            serde_json::json!({
                "machine": self.selected_machine_profile,
                "filament": self.selected_filament_profile,
                "process": self.selected_process_profile,
                "reset_to_defaults": true
            }),
        ) {
            Ok(val) => {
                let res: Option<ProfileApplyResult> = serde_json::from_value(val).ok();
                self.profile_warning_count = res.as_ref().map(|r| r.warnings.len()).unwrap_or(0);
                self.last_error = None;
                self.printability_report = None;
                self.project_dirty = true;
                self.sync_and_save_prefs();
                self.status_message = if self.profile_warning_count > 0 {
                    format!(
                        "Applied profiles ({} warning(s)).",
                        self.profile_warning_count
                    )
                } else {
                    "Applied selected profiles.".to_owned()
                };
                self.frame_camera(CameraPreset::Isometric);
                self.rebuild_scene();
            }
            Err(e) => {
                self.last_error = Some(format!("Profile apply failed: {e}"));
            }
        }
    }

    /// Load a built-in sample primitive through `plate.add_sample_shape`.
    pub fn load_sample(&mut self, shape: SampleShape) {
        self.load_sample_with_mode(shape, false);
    }

    /// Load or append a built-in sample primitive on the build plate.
    pub fn load_sample_with_mode(&mut self, shape: SampleShape, append: bool) {
        self.cancel_pending_slice();
        self.selected_sample = shape;
        match execute(
            &mut self.session,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": shape, "append": append }),
        ) {
            Ok(_) => {
                self.last_error = None;
                self.printability_report = None;
                self.project_dirty = true;
                if !append {
                    self.current_recent_id = None;
                    self.current_project_title = Some(shape.label().to_owned());
                }
                self.selected_object_id = self.session.objects.last().map(|o| o.id);
                self.selected_modifier_id = None;
                self.view_mode = ViewMode::Prepare;
                let verb = if append { "Added" } else { "Loaded" };
                self.status_message = format!("{verb} sample `{}`.", shape.label());
                self.frame_camera(CameraPreset::Isometric);
                self.rebuild_scene();
            }
            Err(e) => {
                self.last_error = Some(e.to_string());
            }
        }
    }

    pub(crate) fn repair_plate_mesh(&mut self, object_id: Option<u64>) {
        self.cancel_pending_slice();
        match execute(
            &mut self.session,
            "plate.repair_mesh",
            serde_json::json!({ "object_id": object_id }),
        ) {
            Ok(val) => {
                let res: Option<PlateRepairResult> = serde_json::from_value(val).ok();
                self.last_error = None;
                self.printability_report = None;
                self.view_mode = ViewMode::Prepare;
                if let Some(r) = res {
                    if !r.changed {
                        self.status_message =
                            "Mesh already clean (no repairable defects found).".to_owned();
                    } else {
                        self.project_dirty = true;
                        let welded: usize =
                            r.objects.iter().map(|o| o.report.welded_vertices).sum();
                        let flipped: usize =
                            r.objects.iter().map(|o| o.report.flipped_triangles).sum();
                        let holes: usize = r.objects.iter().map(|o| o.report.holes_filled).sum();
                        let rem: usize = r
                            .objects
                            .iter()
                            .map(|o| o.report.removed_degenerate + o.report.removed_duplicate)
                            .sum();
                        let still_bad = r.plate_inspection.as_ref().is_some_and(|i| i.needs_repair);
                        self.status_message = if still_bad {
                            format!(
                                "Repaired mesh (welded {welded}, removed {rem}, flipped {flipped}, filled {holes} hole(s)); some non-manifold edges remain."
                            )
                        } else {
                            format!(
                                "Repaired mesh (welded {welded}, removed {rem}, flipped {flipped}, filled {holes} hole(s))."
                            )
                        };
                    }
                } else {
                    self.status_message = "Repaired mesh.".to_owned();
                }
                self.rebuild_scene();
            }
            Err(e) => {
                self.last_error = Some(format!("Mesh repair failed: {e}"));
            }
        }
    }

    fn append_repair_warning_to_status(&mut self) {
        if let Some(insp) = &self.session.plate_inspection
            && insp.needs_repair
        {
            self.status_message.push_str(&format!(
                " Topology issues detected ({} non-manifold / {} flipped edge(s)) — use Plate -> Mesh Summary -> Repair Mesh.",
                insp.non_manifold_edges, insp.inconsistent_winding_edges
            ));
        }
    }

    pub(crate) fn auto_arrange_plate(&mut self) {
        self.cancel_pending_slice();
        match execute(
            &mut self.session,
            "plate.auto_arrange",
            serde_json::json!({ "spacing_mm": 6.0 }),
        ) {
            Ok(_) => {
                self.last_error = None;
                self.printability_report = None;
                self.project_dirty = true;
                self.view_mode = ViewMode::Prepare;
                self.status_message = format!(
                    "Arranged {} object(s) on plate.",
                    self.session.objects.len()
                );
                self.frame_camera(CameraPreset::Isometric);
                self.rebuild_scene();
            }
            Err(e) => {
                self.last_error = Some(e.to_string());
            }
        }
    }

    pub(crate) fn remove_object(&mut self, id: u64) {
        self.cancel_pending_slice();
        match execute(
            &mut self.session,
            "plate.remove_object",
            serde_json::json!({ "id": id }),
        ) {
            Ok(_) => {
                self.last_error = None;
                self.printability_report = None;
                self.project_dirty = true;
                self.selected_object_id = self.session.objects.last().map(|o| o.id);
                self.view_mode = ViewMode::Prepare;
                self.rebuild_scene();
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    pub(crate) fn transform_object(
        &mut self,
        id: u64,
        pos: [f64; 2],
        rot: [f64; 3],
        scale: [f64; 3],
    ) {
        self.cancel_pending_slice();
        match execute(
            &mut self.session,
            "plate.transform_object",
            serde_json::json!({
                "id": id,
                "position_xy_mm": pos,
                "rotation_deg": rot,
                "scale": scale,
            }),
        ) {
            Ok(_) => {
                if self.pointer_active && !self.drag_undo_active {
                    self.drag_undo_active = true;
                    self.session.set_undo_suppressed(true);
                }
                self.last_error = None;
                self.printability_report = None;
                self.project_dirty = true;
                self.view_mode = ViewMode::Prepare;
                self.rebuild_scene();
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    pub(crate) fn add_modifier_primitive(
        &mut self,
        primitive: ModifierPrimitive,
        role: ModifierRole,
    ) {
        self.cancel_pending_slice();
        match execute(
            &mut self.session,
            "plate.add_modifier_primitive",
            serde_json::json!({ "primitive": primitive, "role": role }),
        ) {
            Ok(_) => {
                self.last_error = None;
                self.printability_report = None;
                self.project_dirty = true;
                self.selected_modifier_id = self.session.modifiers.last().map(|m| m.id);
                self.selected_object_id = None;
                self.sidebar_tab = SidebarTab::Plate;
                self.view_mode = ViewMode::Prepare;
                self.status_message = format!("Added {} volume.", role.label());
                self.rebuild_scene();
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    pub(crate) fn remove_modifier(&mut self, id: u64) {
        self.cancel_pending_slice();
        match execute(
            &mut self.session,
            "plate.remove_modifier",
            serde_json::json!({ "id": id }),
        ) {
            Ok(_) => {
                self.last_error = None;
                self.printability_report = None;
                self.project_dirty = true;
                self.selected_modifier_id = self.session.modifiers.last().map(|m| m.id);
                self.view_mode = ViewMode::Prepare;
                self.rebuild_scene();
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    pub(crate) fn update_modifier(
        &mut self,
        id: u64,
        role: ModifierRole,
        pos: [f64; 3],
        rot: [f64; 3],
        scale: [f64; 3],
        overrides: ModifierOverrides,
    ) {
        self.cancel_pending_slice();
        match execute(
            &mut self.session,
            "plate.update_modifier",
            serde_json::json!({
                "id": id,
                "role": role,
                "position_xyz_mm": pos,
                "rotation_deg": rot,
                "scale": scale,
                "overrides": overrides,
            }),
        ) {
            Ok(_) => {
                if self.pointer_active && !self.drag_undo_active {
                    self.drag_undo_active = true;
                    self.session.set_undo_suppressed(true);
                }
                self.last_error = None;
                self.printability_report = None;
                self.project_dirty = true;
                self.view_mode = ViewMode::Prepare;
                self.rebuild_scene();
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    pub(crate) fn set_object_overrides(&mut self, id: u64, overrides: ModifierOverrides) {
        self.cancel_pending_slice();
        match execute(
            &mut self.session,
            "plate.set_object_overrides",
            serde_json::json!({
                "id": id,
                "overrides": overrides,
            }),
        ) {
            Ok(_) => {
                if self.pointer_active && !self.drag_undo_active {
                    self.drag_undo_active = true;
                    self.session.set_undo_suppressed(true);
                }
                self.last_error = None;
                self.printability_report = None;
                self.project_dirty = true;
                self.view_mode = ViewMode::Prepare;
                self.rebuild_scene();
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    pub(crate) fn sync_gcode_index(&mut self) {
        if let Some(out) = self.session.last_gcode.as_ref() {
            if !self.gcode_index.matches(&out.gcode) {
                self.gcode_index = GcodeLayerIndex::build(&out.gcode);
            }
        } else if self.gcode_index.total_lines() > 0 {
            self.gcode_index = GcodeLayerIndex::default();
        }
    }

    /// Return `true` while a background slice job is active.
    pub(crate) fn is_slicing(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.pending_slice.is_some()
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.pending_web_slice.is_some()
        }
    }

    pub(crate) fn slice_progress_snapshot(&self) -> Option<(usize, usize)> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.pending_slice.as_ref().map(|j| j.progress.snapshot())
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.pending_web_slice.as_ref().map(|j| j.progress.get())
        }
    }

    pub(crate) fn cancel_pending_slice(&mut self) {
        self.next_slice_job_id = self.next_slice_job_id.wrapping_add(1);
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(job) = self.pending_slice.take() {
                job.progress.cancel();
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            if let Some(job) = self.pending_web_slice.take() {
                job.worker.set_onmessage(None);
                job.worker.set_onerror(None);
                job.worker.terminate();
            }
        }
    }

    pub(crate) fn cancel_slice_action(&mut self) {
        self.cancel_pending_slice();
        self.last_error = None;
        self.status_message = "Slicing cancelled.".to_owned();
    }

    /// Trigger slicing from interactive UI controls (async background thread on native, Web Worker on WASM).
    pub(crate) fn start_slice_action(&mut self, ctx: &egui::Context) {
        #[cfg(target_arch = "wasm32")]
        {
            if self.pending_web_slice.is_some() {
                return;
            }
            if self.session.objects.is_empty()
                && self.session.prepared_mesh.is_none()
                && self.session.mesh.is_none()
            {
                self.last_error = Some("No mesh loaded on build plate.".to_owned());
                return;
            }
            if let Err(e) = self.session.config.validate() {
                self.last_error = Some(e.to_string());
                return;
            }
            if let Some(prev_cfg) = self.pending_config_undo.take()
                && self.session.config != prev_cfg
            {
                self.session
                    .push_config_undo_snapshot("Edit settings", prev_cfg);
            }

            let job_id = self.next_slice_job_id.wrapping_add(1);
            self.next_slice_job_id = job_id;
            let objects: Vec<SliceJobObject> = self
                .session
                .objects
                .iter()
                .map(|o| SliceJobObject {
                    id: o.id,
                    name: o.name.clone(),
                    prepared_mesh: layercraft_geom::TriangleMesh::clone(&o.prepared_mesh),
                    overrides: o.overrides.clone(),
                })
                .collect();
            let req = SliceJobRequest {
                job_id,
                config: self.session.config.clone(),
                objects,
                prepared_mesh: if self.session.objects.is_empty() {
                    self.session.prepared_mesh.as_deref().cloned()
                } else {
                    None
                },
                modifiers: self
                    .session
                    .modifiers
                    .iter()
                    .map(|m| layercraft_slice::ModifierVolume {
                        id: m.id,
                        name: m.name.clone(),
                        role: m.role,
                        mesh: layercraft_geom::TriangleMesh::clone(&m.prepared_mesh),
                        overrides: m.overrides.clone(),
                    })
                    .collect(),
            };
            let Ok(req_json) = serde_json::to_string(&req) else {
                self.run_slice();
                return;
            };

            let Ok(worker) = web_sys::Worker::new("./slice_worker_loader.js") else {
                self.run_slice();
                return;
            };

            use wasm_bindgen::JsCast as _;
            let progress = std::rc::Rc::new(std::cell::Cell::new((0_usize, 0_usize)));
            let outcome = std::rc::Rc::new(std::cell::RefCell::new(None));
            let worker_cb = worker.clone();
            let progress_cb = std::rc::Rc::clone(&progress);
            let outcome_cb = std::rc::Rc::clone(&outcome);
            let ctx_cb = ctx.clone();
            let mut pending_payload = Some(req_json);

            let on_message =
                wasm_bindgen::closure::Closure::wrap(Box::new(move |ev: web_sys::MessageEvent| {
                    let Some(raw) = ev.data().as_string() else {
                        return;
                    };
                    match serde_json::from_str::<SliceJobEvent>(&raw) {
                        Ok(SliceJobEvent::Ready) => {
                            if let Some(payload) = pending_payload.take()
                                && let Err(err) = worker_cb
                                    .post_message(&wasm_bindgen::JsValue::from_str(&payload))
                            {
                                *outcome_cb.borrow_mut() = Some(WebSliceOutcome::Error {
                                    job_id,
                                    message: format!("failed posting slice job to worker: {err:?}"),
                                });
                                ctx_cb.request_repaint();
                            }
                        }
                        Ok(SliceJobEvent::Progress {
                            job_id: jid,
                            completed_layers,
                            total_layers,
                        }) if jid == job_id => {
                            progress_cb.set((completed_layers, total_layers));
                            ctx_cb.request_repaint();
                        }
                        Ok(SliceJobEvent::Done {
                            job_id: jid,
                            elapsed_ms,
                            plate,
                            gcode,
                        }) if jid == job_id => {
                            *outcome_cb.borrow_mut() = Some(WebSliceOutcome::Done {
                                job_id: jid,
                                elapsed_ms,
                                plate,
                                gcode,
                            });
                            ctx_cb.request_repaint();
                        }
                        Ok(SliceJobEvent::Error {
                            job_id: jid,
                            message,
                        }) if jid == job_id => {
                            *outcome_cb.borrow_mut() = Some(WebSliceOutcome::Error {
                                job_id: jid,
                                message,
                            });
                            ctx_cb.request_repaint();
                        }
                        Ok(_) => {}
                        Err(e) => {
                            *outcome_cb.borrow_mut() = Some(WebSliceOutcome::Error {
                                job_id,
                                message: format!("invalid worker message: {e}"),
                            });
                            ctx_cb.request_repaint();
                        }
                    }
                })
                    as Box<dyn FnMut(web_sys::MessageEvent)>);

            let outcome_err = std::rc::Rc::clone(&outcome);
            let ctx_err = ctx.clone();
            let on_error =
                wasm_bindgen::closure::Closure::wrap(Box::new(move |ev: web_sys::ErrorEvent| {
                    *outcome_err.borrow_mut() = Some(WebSliceOutcome::Error {
                        job_id,
                        message: format!("slice worker error: {}", ev.message()),
                    });
                    ctx_err.request_repaint();
                })
                    as Box<dyn FnMut(web_sys::ErrorEvent)>);

            worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
            worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));
            self.last_error = None;
            self.status_message = "Slicing in background worker...".to_owned();
            self.pending_web_slice = Some(PendingWebSliceJob {
                worker,
                _on_message: on_message,
                _on_error: on_error,
                progress,
                outcome,
            });
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.pending_slice.is_some() {
                return;
            }
            if self.session.objects.is_empty()
                && self.session.prepared_mesh.is_none()
                && self.session.mesh.is_none()
            {
                self.last_error = Some("No mesh loaded on build plate.".to_owned());
                return;
            }
            if let Err(e) = self.session.config.validate() {
                self.last_error = Some(e.to_string());
                return;
            }
            if let Some(prev_cfg) = self.pending_config_undo.take()
                && self.session.config != prev_cfg
            {
                self.session
                    .push_config_undo_snapshot("Edit settings", prev_cfg);
            }

            let job_id = self.next_slice_job_id.wrapping_add(1);
            self.next_slice_job_id = job_id;
            let cfg = self.session.config.clone();
            let objs_owned: Vec<(
                u64,
                String,
                Arc<layercraft_geom::TriangleMesh>,
                ModifierOverrides,
            )> = self
                .session
                .objects
                .iter()
                .map(|o| {
                    (
                        o.id,
                        o.name.clone(),
                        Arc::clone(&o.prepared_mesh),
                        o.overrides.clone(),
                    )
                })
                .collect();
            let prepared = self.session.prepared_mesh.clone();
            let raw = self.session.mesh.clone();
            let mod_vols: Vec<layercraft_slice::ModifierVolume> = self
                .session
                .modifiers
                .iter()
                .map(|m| layercraft_slice::ModifierVolume {
                    id: m.id,
                    name: m.name.clone(),
                    role: m.role,
                    mesh: layercraft_geom::TriangleMesh::clone(&m.prepared_mesh),
                    overrides: m.overrides.clone(),
                })
                .collect();

            let progress = Arc::new(layercraft_slice::SliceProgress::new());
            let progress_bg = Arc::clone(&progress);
            let (tx, rx) = mpsc::channel();
            let ctx_clone = ctx.clone();
            let spawned = std::thread::Builder::new()
                .name("layercraft-slice".to_owned())
                .spawn(move || {
                    let t0 = web_time::Instant::now();
                    let map_slice_err = |e: layercraft_slice::SliceError| match e {
                        layercraft_slice::SliceError::Cancelled => SliceJobError::Cancelled,
                        other => SliceJobError::Failed(other.to_string()),
                    };
                    let res = (|| -> Result<
                        (layercraft_slice::SlicedPlate, layercraft_gcode::GcodeOutput),
                        SliceJobError,
                    > {
                        let plate = if !objs_owned.is_empty() {
                            let plate_objs: Vec<layercraft_slice::PlateSliceObject<'_>> =
                                objs_owned
                                    .iter()
                                    .map(|(id, name, mesh, ov)| {
                                        layercraft_slice::PlateSliceObject {
                                            id: *id,
                                            name: name.as_str(),
                                            prepared_mesh: mesh.as_ref(),
                                            overrides: ov,
                                        }
                                    })
                                    .collect();
                            layercraft_slice::slice_plate_objects_with_progress(
                                &plate_objs,
                                &cfg,
                                &mod_vols,
                                Some(progress_bg.as_ref()),
                            )
                            .map_err(map_slice_err)?
                        } else if let Some(prep) = prepared.as_ref() {
                            layercraft_slice::slice_prepared_mesh_with_progress(
                                prep,
                                &cfg,
                                &mod_vols,
                                Some(progress_bg.as_ref()),
                            )
                            .map_err(map_slice_err)?
                        } else if let Some(r) = raw.as_ref() {
                            layercraft_slice::slice_single_mesh_with_modifiers(r, &cfg, &mod_vols)
                                .map_err(map_slice_err)?
                        } else {
                            return Err(SliceJobError::Failed(
                                "no mesh loaded on build plate".to_owned(),
                            ));
                        };
                        if progress_bg.is_cancelled() {
                            return Err(SliceJobError::Cancelled);
                        }
                        let gcode_out = layercraft_gcode::emit_gcode(&plate, &cfg)
                            .map_err(|e| SliceJobError::Failed(e.to_string()))?;
                        if progress_bg.is_cancelled() {
                            return Err(SliceJobError::Cancelled);
                        }
                        Ok((plate, gcode_out))
                    })();
                    let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
                    let _ = tx.send(PendingSliceOutcome {
                        job_id,
                        elapsed_ms,
                        result: res,
                    });
                    ctx_clone.request_repaint();
                });

            match spawned {
                Ok(_) => {
                    self.last_error = None;
                    self.status_message = "Slicing in background...".to_owned();
                    self.pending_slice = Some(PendingSliceJob { rx, progress });
                }
                Err(_) => {
                    self.run_slice();
                }
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn poll_pending_web_slice(&mut self, ctx: &egui::Context) {
        let out = {
            let Some(job) = self.pending_web_slice.as_ref() else {
                return;
            };
            job.outcome.borrow_mut().take()
        };
        let Some(outcome) = out else {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
            return;
        };
        if let Some(job) = self.pending_web_slice.take() {
            job.worker.set_onmessage(None);
            job.worker.set_onerror(None);
            job.worker.terminate();
        }
        match outcome {
            WebSliceOutcome::Done {
                job_id,
                elapsed_ms,
                plate,
                gcode,
            } => {
                if job_id != self.next_slice_job_id {
                    return;
                }
                let layers = plate.layers.len();
                self.session.sliced = Some(plate);
                self.session.last_gcode = Some(gcode);
                self.gcode_index = GcodeLayerIndex::default();
                self.last_slice_ms = Some(elapsed_ms);
                self.view_mode = ViewMode::Preview;
                self.last_error = None;
                self.status_message = format!("Sliced {layers} layers in {elapsed_ms:.1} ms.");
                self.refresh_printability_after_slice();
                self.rebuild_scene();
                self.set_active_layer(layers.saturating_sub(1));
                self.update_current_recent_after_slice();
            }
            WebSliceOutcome::Error { job_id, message } => {
                if job_id != self.next_slice_job_id {
                    return;
                }
                if message == "slicing cancelled" {
                    self.last_error = None;
                    self.status_message = "Slicing cancelled.".to_owned();
                } else {
                    self.last_error = Some(message);
                }
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn poll_pending_slice(&mut self, ctx: &egui::Context) {
        let outcome = match self.pending_slice.as_ref() {
            Some(job) => match job.rx.try_recv() {
                Ok(out) => Some(out),
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(50));
                    None
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending_slice = None;
                    None
                }
            },
            None => None,
        };
        let Some(out) = outcome else {
            return;
        };
        self.pending_slice = None;
        if out.job_id != self.next_slice_job_id {
            return;
        }
        match out.result {
            Ok((plate, gcode_out)) => {
                let layers = plate.layers.len();
                self.session.sliced = Some(plate);
                self.session.last_gcode = Some(gcode_out);
                self.gcode_index = GcodeLayerIndex::default();
                self.last_slice_ms = Some(out.elapsed_ms);
                self.view_mode = ViewMode::Preview;
                self.last_error = None;
                self.status_message =
                    format!("Sliced {layers} layers in {:.1} ms.", out.elapsed_ms);
                self.refresh_printability_after_slice();
                self.rebuild_scene();
                self.set_active_layer(layers.saturating_sub(1));
                self.update_current_recent_after_slice();
            }
            Err(SliceJobError::Cancelled) => {
                self.last_error = None;
                self.status_message = "Slicing cancelled.".to_owned();
            }
            Err(SliceJobError::Failed(err_msg)) => {
                self.last_error = Some(err_msg);
            }
        }
    }

    /// Slice and generate G-code via `slice.run` and `gcode.export`.
    pub fn run_slice(&mut self) {
        let t0 = web_time::Instant::now();
        let cfg = self.session.config.clone();
        if let Err(e) = execute(
            &mut self.session,
            "config.set",
            serde_json::json!({ "config": cfg }),
        ) {
            self.last_error = Some(e.to_string());
            return;
        }
        if let Err(e) = execute(&mut self.session, "slice.run", Value::Null) {
            self.last_error = Some(e.to_string());
            return;
        }
        if let Err(e) = execute(&mut self.session, "gcode.export", Value::Null) {
            self.last_error = Some(e.to_string());
            return;
        }
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.last_slice_ms = Some(elapsed_ms);
        let layers = self
            .session
            .sliced
            .as_ref()
            .map(|s| s.layers.len())
            .unwrap_or(0);
        self.gcode_index = GcodeLayerIndex::default();
        self.view_mode = ViewMode::Preview;
        self.last_error = None;
        self.status_message = format!("Sliced {layers} layers in {elapsed_ms:.1} ms.");
        self.refresh_printability_after_slice();
        self.rebuild_scene();
        self.set_active_layer(layers.saturating_sub(1));
        self.update_current_recent_after_slice();
    }

    pub(crate) fn frame_camera(&mut self, preset: CameraPreset) {
        let bw = self.session.config.machine.bed_width_mm as f32;
        let bd = self.session.config.machine.bed_depth_mm as f32;
        self.camera = match self.session.prepared_mesh.as_ref().and_then(|m| m.aabb()) {
            Some(aabb) => OrbitCamera::framed_on_aabb(preset, aabb, bw, bd),
            None => OrbitCamera::from_preset(preset, bw, bd, 20.0),
        };
    }

    pub(crate) fn rebuild_scene(&mut self) {
        self.scene_revision = self.scene_revision.wrapping_add(1);
        let vp_theme = if self.last_scene_dark {
            ViewportTheme::Dark
        } else {
            ViewportTheme::Light
        };
        let bed_plate = if self.show_bed_grid {
            build_bed_plate_mesh(&self.session.config, vp_theme)
        } else {
            Vec::new()
        };
        let bed = if self.show_bed_grid {
            build_bed_grid_themed(&self.session.config, vp_theme)
        } else {
            Vec::new()
        };

        let (selected_col, unselected_col, ghost_col) = if self.last_scene_dark {
            (
                [0.18_f32, 0.72, 0.68, 1.0],
                [0.46_f32, 0.57, 0.70, 1.0],
                [0.65_f32, 0.74, 0.84, 0.22],
            )
        } else {
            (
                [0.10_f32, 0.58, 0.55, 1.0],
                [0.38_f32, 0.49, 0.62, 1.0],
                [0.25_f32, 0.34, 0.44, 0.18],
            )
        };

        let (selected_oob_col, oob_col, selected_overlap_col, overlap_col) = if self.last_scene_dark
        {
            (
                [0.98_f32, 0.34, 0.32, 1.0],
                [0.88_f32, 0.24, 0.22, 1.0],
                [0.98_f32, 0.68, 0.24, 1.0],
                [0.92_f32, 0.56, 0.16, 1.0],
            )
        } else {
            (
                [0.86_f32, 0.20, 0.18, 1.0],
                [0.78_f32, 0.16, 0.14, 1.0],
                [0.88_f32, 0.50, 0.12, 1.0],
                [0.80_f32, 0.44, 0.08, 1.0],
            )
        };

        let mut mesh = Vec::new();
        if self.session.objects.is_empty() {
            if let Some(m) = &self.session.prepared_mesh {
                mesh.extend(build_mesh_vertices(m, selected_col));
            }
        } else {
            for obj in &self.session.objects {
                let is_selected = self.selected_object_id == Some(obj.id);
                let is_oob = self
                    .session
                    .plate_layout
                    .out_of_bounds_object_ids
                    .contains(&obj.id);
                let is_overlap = self
                    .session
                    .plate_layout
                    .colliding_object_ids
                    .contains(&obj.id);
                let col = if is_oob {
                    if is_selected {
                        selected_oob_col
                    } else {
                        oob_col
                    }
                } else if is_overlap {
                    if is_selected {
                        selected_overlap_col
                    } else {
                        overlap_col
                    }
                } else if is_selected {
                    selected_col
                } else {
                    unselected_col
                };
                mesh.extend(build_mesh_vertices(&obj.prepared_mesh, col));
            }
        }

        let mut modifiers = Vec::new();
        for m in &self.session.modifiers {
            let alpha = if self.selected_modifier_id == Some(m.id) {
                0.44_f32
            } else {
                0.28_f32
            };
            let col = match m.role {
                ModifierRole::Modifier => [0.95, 0.72, 0.18, alpha],
                ModifierRole::SupportBlocker => [0.92, 0.26, 0.26, alpha],
                ModifierRole::SupportEnforcer => [0.20, 0.85, 0.45, alpha],
                ModifierRole::NegativePart => [0.85, 0.22, 0.72, alpha],
            };
            modifiers.extend(build_mesh_vertices(&m.prepared_mesh, col));
        }

        let ghost = self
            .session
            .prepared_mesh
            .as_ref()
            .map(|m| build_mesh_vertices(m, ghost_col))
            .unwrap_or_default();

        let tp = self
            .session
            .sliced
            .as_ref()
            .map(|s| {
                build_toolpath_instances_with_config(s, self.color_mode, Some(&self.session.config))
            })
            .unwrap_or_default();

        self.scene = Arc::new(GpuSceneSnapshot::from_geometry(
            self.scene_revision,
            &bed_plate,
            &bed,
            &mesh,
            &modifiers,
            &ghost,
            &tp,
        ));
        let max_steps = self.active_layer_step_count();
        self.layer_move_step = self.layer_move_step.min(max_steps);
        self.sync_gcode_index();
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        #[cfg(target_arch = "wasm32")]
        {
            let ready: Vec<(WebFilePickMode, String, Result<Vec<u8>, String>)> =
                std::mem::take(&mut *self.pending_drops.borrow_mut());
            for (mode, name, res) in ready {
                match res {
                    Ok(bytes) => match mode {
                        WebFilePickMode::ImportProfileJson => {
                            self.load_profile_json_bytes(&name, bytes);
                        }
                        WebFilePickMode::AddToPlate => {
                            self.load_mesh_bytes_with_mode(&name, bytes, true);
                        }
                        WebFilePickMode::OpenReplace => {
                            if name.to_ascii_lowercase().ends_with(".json") {
                                self.load_profile_json_bytes(&name, bytes);
                            } else {
                                self.load_mesh_bytes_with_mode(&name, bytes, false);
                            }
                        }
                    },
                    Err(e) => self.last_error = Some(format!("Failed reading `{name}`: {e}")),
                }
            }
        }

        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        let Some(file) = dropped.into_iter().next() else {
            return;
        };
        let name = file
            .path()
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("dropped.stl")
            .to_owned();

        #[cfg(not(target_arch = "wasm32"))]
        {
            let is_json = name.to_ascii_lowercase().ends_with(".json");
            match file.bytes() {
                Ok(bytes) => {
                    if is_json {
                        self.load_profile_json_bytes(&name, bytes);
                    } else {
                        self.load_mesh_bytes(&name, bytes);
                    }
                }
                Err(_) => {
                    let p = file.path().to_string_lossy().into_owned();
                    if is_json {
                        self.load_native_profile_path(&p);
                    } else {
                        self.load_native_path_with_mode(&p, false);
                    }
                }
            }
        }

        #[cfg(target_arch = "wasm32")]
        {
            let queue = std::rc::Rc::clone(&self.pending_drops);
            let ctx_clone = ctx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let res = file.bytes_async().await;
                let mode = if name.to_ascii_lowercase().ends_with(".json") {
                    WebFilePickMode::ImportProfileJson
                } else {
                    WebFilePickMode::OpenReplace
                };
                queue.borrow_mut().push((mode, name, res));
                ctx_clone.request_repaint();
            });
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn load_native_path_with_mode(&mut self, path: &str, append: bool) {
        self.cancel_pending_slice();
        let is_3mf = path.to_ascii_lowercase().ends_with(".3mf");
        if !append && is_3mf {
            match execute(
                &mut self.session,
                "project.load_file",
                serde_json::json!({ "path": path }),
            ) {
                Ok(val) => {
                    let res = serde_json::from_value::<ProjectLoadResult>(val).ok();
                    let plate_count = res
                        .as_ref()
                        .map(|r| r.plate_count)
                        .unwrap_or_else(|| self.session.plate_count());
                    self.last_error = None;
                    if res.as_ref().is_some_and(|r| r.is_layercraft_project) {
                        self.selected_machine_profile =
                            self.session.active_profiles.machine.clone();
                        self.selected_filament_profile =
                            self.session.active_profiles.filament.clone();
                        self.selected_process_profile =
                            self.session.active_profiles.process.clone();
                        if plate_count > 1 {
                            self.status_message = format!(
                                "Loaded project `{path}` ({plate_count} plates, active `{}`: {} object(s), {} modifier(s)).",
                                self.session.active_plate_name(),
                                self.session.objects.len(),
                                self.session.modifiers.len()
                            );
                        } else {
                            self.status_message = format!(
                                "Loaded project `{path}` ({} object(s), {} modifier(s)).",
                                self.session.objects.len(),
                                self.session.modifiers.len()
                            );
                        }
                    } else if plate_count > 1 {
                        self.status_message = format!(
                            "Loaded `{path}` ({plate_count} plates, active `{}`: {} object(s)).",
                            self.session.active_plate_name(),
                            self.session.objects.len()
                        );
                    } else {
                        self.status_message = format!(
                            "Loaded `{path}` ({} object(s)).",
                            self.session.objects.len()
                        );
                    }
                    self.append_repair_warning_to_status();
                    self.printability_report = None;
                    let loaded_proj = res.as_ref().is_some_and(|r| r.is_layercraft_project);
                    self.project_dirty = !loaded_proj;
                    self.prefs.push_recent_file(path);
                    self.sync_and_save_prefs();
                    self.selected_object_id = self.session.objects.last().map(|o| o.id);
                    self.selected_modifier_id = None;
                    self.view_mode = ViewMode::Prepare;
                    self.max_layer = 0;
                    self.layer_move_step = 0;
                    self.playback_playing = false;
                    self.frame_camera(CameraPreset::Isometric);
                    self.rebuild_scene();
                    self.record_recent_native(std::path::Path::new(path));
                }
                Err(e) => {
                    self.last_error = Some(format!("Failed loading `{path}`: {e}"));
                }
            }
            return;
        }

        match execute(
            &mut self.session,
            "plate.add_mesh_file",
            serde_json::json!({ "path": path, "append": append }),
        ) {
            Ok(_) => {
                self.last_error = None;
                self.printability_report = None;
                self.project_dirty = true;
                self.selected_object_id = self.session.objects.last().map(|o| o.id);
                self.selected_modifier_id = None;
                self.view_mode = ViewMode::Prepare;
                self.status_message = format!("Loaded `{path}`.");
                self.append_repair_warning_to_status();
                self.frame_camera(CameraPreset::Isometric);
                self.rebuild_scene();
                if !append {
                    self.prefs.push_recent_file(path);
                    self.sync_and_save_prefs();
                    self.record_recent_native(std::path::Path::new(path));
                }
            }
            Err(e) => {
                self.last_error = Some(format!("Failed loading `{path}`: {e}"));
            }
        }
    }

    pub(crate) fn save_project_action(&mut self) {
        let bytes = match export_project_3mf_bytes(&self.session) {
            Ok(b) => b,
            Err(e) => {
                self.last_error = Some(format!("Project export failed: {e}"));
                return;
            }
        };

        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Save LayerCraft Project (.3mf)")
                .set_file_name("layercraft_project.3mf")
                .add_filter("3MF Project", &["3mf", "3MF"])
                .save_file()
            {
                match std::fs::write(&path, &bytes) {
                    Ok(()) => {
                        self.last_error = None;
                        self.project_dirty = false;
                        let p_str = path.to_string_lossy().into_owned();
                        self.prefs.push_recent_file(&p_str);
                        self.sync_and_save_prefs();
                        self.record_recent_native(&path);
                        self.status_message = format!("Saved project `{}`.", path.display());
                    }
                    Err(e) => {
                        self.last_error = Some(format!("Save project failed: {e}"));
                    }
                }
            }
        }

        #[cfg(target_arch = "wasm32")]
        {
            match trigger_browser_download_bytes(
                "layercraft_project.3mf",
                &bytes,
                "application/vnd.ms-package.3dmanufacturing-3dmodel+xml",
            ) {
                Ok(()) => {
                    self.last_error = None;
                    self.project_dirty = false;
                    let title = self.active_project_title();
                    self.record_recent_browser(&title, "3mf");
                    self.status_message =
                        "Triggered browser download `layercraft_project.3mf`.".to_owned();
                }
                Err(e) => {
                    self.last_error = Some(format!("Download failed: {e}"));
                }
            }
        }
    }

    /// Active project title for header pill & Home cards.
    pub(crate) fn active_project_title(&self) -> String {
        if let Some(title) = self.current_project_title.as_deref()
            && !title.trim().is_empty()
        {
            return title.to_owned();
        }
        if let Some(first) = self.session.objects.first() {
            let trimmed = recent_projects::display_title_from_name(&first.name);
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
        "Untitled Project".to_owned()
    }

    /// Clear current plate into a clean blank workspace in Prepare view.
    pub(crate) fn new_empty_project_action(&mut self) {
        self.clear_plate_action();
        self.current_recent_id = None;
        self.current_project_title = None;
        self.view_mode = ViewMode::Prepare;
        self.status_message =
            "Started new empty project — drop an .stl/.3mf file or add a calibration shape."
                .to_owned();
    }

    fn format_estimated_time(secs: f64) -> String {
        let total = secs.max(0.0).round() as u64;
        let hours = total / 3_600;
        let mins = (total % 3_600) / 60;
        let rem_secs = total % 60;
        if hours > 0 {
            format!("{hours}h {mins:02}m")
        } else if mins > 0 {
            format!("{mins}m {rem_secs:02}s")
        } else {
            format!("{rem_secs}s")
        }
    }

    fn total_project_object_count(&self) -> usize {
        let total: usize = self
            .session
            .list_plates()
            .iter()
            .map(|p| p.object_count)
            .sum();
        total.max(self.session.objects.len())
    }

    fn render_session_thumbnail_b64(&self) -> Option<String> {
        let single_or_prepared = if self.session.objects.is_empty() {
            self.session.prepared_mesh.as_deref().cloned()
        } else if self.session.objects.len() == 1 {
            self.session
                .objects
                .first()
                .map(|o| layercraft_geom::TriangleMesh::clone(&o.prepared_mesh))
        } else {
            let tris: Vec<layercraft_geom::Triangle> = self
                .session
                .objects
                .iter()
                .flat_map(|o| o.prepared_mesh.triangles.iter().copied())
                .collect();
            layercraft_geom::TriangleMesh::new(tris).ok()
        };
        recent_projects::render_plate_thumbnail_b64(
            single_or_prepared.as_ref(),
            self.session.sliced.as_ref(),
            &self.session.config,
        )
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn record_recent_native(&mut self, path: &std::path::Path) {
        let path_str = path.to_string_lossy().into_owned();
        let id = recent_projects::native_entry_id(path);
        let name = recent_projects::display_title_from_name(&path_str);
        let format = recent_projects::infer_file_format(&path_str);
        let entry = recent_projects::RecentProjectEntry {
            id: id.clone(),
            name: name.clone(),
            format,
            source: recent_projects::RecentProjectSource::NativePath,
            path: Some(path_str),
            storage_key: None,
            last_opened_unix_secs: recent_projects::now_unix_secs(),
            object_count: self.total_project_object_count(),
            plate_count: self.session.plate_count().max(1),
            printer_name: self.selected_machine_profile.clone(),
            estimated_time: self
                .session
                .last_gcode
                .as_ref()
                .map(|g| Self::format_estimated_time(g.stats.estimated_time_s)),
            thumbnail_png_b64: self.render_session_thumbnail_b64(),
        };
        self.recent_projects.upsert(entry);
        self.recent_thumb_textures.remove(&id);
        self.current_recent_id = Some(id);
        self.current_project_title = Some(name);
        #[cfg(not(test))]
        {
            let _ = self.recent_projects.save_to_disk();
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn record_recent_browser(&mut self, display_name: &str, format: &str) {
        let Ok(bytes) = export_project_3mf_bytes(&self.session) else {
            return;
        };
        if bytes.len() > recent_projects::MAX_BROWSER_PROJECT_BLOB_BYTES {
            return;
        }
        let now = recent_projects::now_unix_secs();
        let name = recent_projects::display_title_from_name(display_name);
        let key = match self.current_recent_id.as_deref() {
            Some(existing) if existing.starts_with("web:") => existing.to_owned(),
            _ => format!("web:{}-{now}", slugify_printer_id(&name)),
        };
        let entry = recent_projects::RecentProjectEntry {
            id: key.clone(),
            name: name.clone(),
            format: format.to_ascii_lowercase(),
            source: recent_projects::RecentProjectSource::BrowserIndexedDb,
            path: None,
            storage_key: Some(key.clone()),
            last_opened_unix_secs: now,
            object_count: self.total_project_object_count(),
            plate_count: self.session.plate_count().max(1),
            printer_name: self.selected_machine_profile.clone(),
            estimated_time: self
                .session
                .last_gcode
                .as_ref()
                .map(|g| Self::format_estimated_time(g.stats.estimated_time_s)),
            thumbnail_png_b64: self.render_session_thumbnail_b64(),
        };
        self.recent_projects.upsert(entry);
        self.recent_thumb_textures.remove(&key);
        self.current_recent_id = Some(key.clone());
        self.current_project_title = Some(name);
        #[cfg(not(test))]
        {
            let _ = self.recent_projects.save_to_disk();
        }
        wasm_bindgen_futures::spawn_local(async move {
            let _ = recent_projects::idb_put_project_bytes(&key, &bytes).await;
        });
    }

    fn update_current_recent_after_slice(&mut self) {
        let Some(cur_id) = self.current_recent_id.clone() else {
            return;
        };
        let est = self
            .session
            .last_gcode
            .as_ref()
            .map(|g| Self::format_estimated_time(g.stats.estimated_time_s));
        let printer_name = self.selected_machine_profile.clone();
        let object_count = self.total_project_object_count();
        let plate_count = self.session.plate_count().max(1);
        let thumb = self.render_session_thumbnail_b64();
        let mut updated = false;
        if let Some(entry) = self
            .recent_projects
            .entries
            .iter_mut()
            .find(|e| e.id == cur_id)
        {
            entry.estimated_time = est;
            entry.printer_name = printer_name;
            entry.object_count = object_count;
            entry.plate_count = plate_count;
            entry.last_opened_unix_secs = recent_projects::now_unix_secs();
            if thumb.is_some() {
                entry.thumbnail_png_b64 = thumb;
            }
            updated = true;
        }
        if updated {
            self.recent_thumb_textures.remove(&cur_id);
            #[cfg(not(test))]
            {
                let _ = self.recent_projects.save_to_disk();
            }
        }
    }

    /// Re-open a stored recent project entry by ID.
    pub(crate) fn open_recent_project(&mut self, id: &str) {
        let Some(entry) = self
            .recent_projects
            .entries
            .iter()
            .find(|e| e.id == id)
            .cloned()
        else {
            self.last_error = Some("Recent project entry no longer exists.".to_owned());
            return;
        };

        match entry.source {
            recent_projects::RecentProjectSource::NativePath => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let Some(path_str) = entry.path else {
                        self.last_error =
                            Some("Recent project entry is missing a path.".to_owned());
                        return;
                    };
                    if std::path::Path::new(&path_str).exists() {
                        self.load_native_path_with_mode(&path_str, false);
                    } else {
                        self.last_error = Some(format!(
                            "File not found on disk: `{path_str}`. You can remove it from Recent Projects."
                        ));
                    }
                }
                #[cfg(target_arch = "wasm32")]
                {
                    self.last_error = Some(
                        "Local filesystem projects cannot be reopened from the browser build."
                            .to_owned(),
                    );
                }
            }
            recent_projects::RecentProjectSource::BrowserIndexedDb => {
                #[cfg(target_arch = "wasm32")]
                {
                    let Some(key) = entry.storage_key.or_else(|| Some(entry.id.clone())) else {
                        self.last_error =
                            Some("Browser project entry is missing storage key.".to_owned());
                        return;
                    };
                    let id_clone = entry.id.clone();
                    let name_clone = entry.name.clone();
                    let pending_slot = std::rc::Rc::clone(&self.pending_browser_recent);
                    let ctx_opt = self.last_ctx.clone();
                    self.last_error = None;
                    self.status_message =
                        format!("Restoring `{}` from browser cache...", entry.name);
                    wasm_bindgen_futures::spawn_local(async move {
                        let res = recent_projects::idb_get_project_bytes(&key).await;
                        if let Ok(mut guard) = pending_slot.try_borrow_mut() {
                            *guard = Some((id_clone, name_clone, res));
                        }
                        if let Some(ctx) = ctx_opt {
                            ctx.request_repaint();
                        }
                    });
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.last_error = Some(
                        "Browser IndexedDB projects are only available in web builds.".to_owned(),
                    );
                }
            }
        }
    }

    /// Remove a single entry from Recent Projects (and IndexedDB in WASM).
    pub(crate) fn remove_recent_project(&mut self, id: &str) {
        if let Some(removed) = self.recent_projects.remove(id) {
            self.recent_thumb_textures.remove(id);
            if self.current_recent_id.as_deref() == Some(id) {
                self.current_recent_id = None;
            }
            #[cfg(not(test))]
            {
                let _ = self.recent_projects.save_to_disk();
            }
            #[cfg(target_arch = "wasm32")]
            if let Some(key) = removed.storage_key {
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = recent_projects::idb_delete_project_bytes(&key).await;
                });
            }
            #[cfg(not(target_arch = "wasm32"))]
            let _ = removed;
        }
    }

    /// Clear all Recent Projects entries (and browser IndexedDB cache in WASM).
    pub(crate) fn clear_recent_projects(&mut self) {
        self.recent_projects.clear();
        self.recent_thumb_textures.clear();
        self.current_recent_id = None;
        #[cfg(not(test))]
        {
            let _ = self.recent_projects.save_to_disk();
        }
        #[cfg(target_arch = "wasm32")]
        wasm_bindgen_futures::spawn_local(async {
            let _ = recent_projects::idb_clear_project_bytes().await;
        });
    }

    #[cfg(target_arch = "wasm32")]
    fn poll_pending_browser_recent(&mut self) {
        let ready = self
            .pending_browser_recent
            .try_borrow_mut()
            .ok()
            .and_then(|mut g| g.take());
        let Some((id, name, res)) = ready else {
            return;
        };
        match res {
            Ok(Some(bytes)) => {
                self.current_recent_id = Some(id.clone());
                self.load_mesh_bytes_inner(&format!("{name}.3mf"), bytes, false, false);
                self.current_recent_id = Some(id);
                self.current_project_title = Some(name);
                self.project_dirty = false;
            }
            Ok(None) => {
                self.last_error = Some(format!(
                    "Cached browser project data for `{name}` was not found in IndexedDB."
                ));
            }
            Err(e) => {
                self.last_error = Some(format!("Failed opening cached project `{name}`: {e}"));
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn open_file_dialog(&mut self, append: bool) {
        let mode = if append {
            WebFilePickMode::AddToPlate
        } else {
            WebFilePickMode::OpenReplace
        };
        if let Err(err) = self.trigger_browser_file_picker(".stl,.3mf,.STL,.3MF", mode) {
            self.last_error = Some(err);
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn import_profile_file_dialog(&mut self) {
        if let Err(err) =
            self.trigger_browser_file_picker(".json,.JSON", WebFilePickMode::ImportProfileJson)
        {
            self.last_error = Some(err);
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn trigger_browser_file_picker(
        &mut self,
        accept: &str,
        mode: WebFilePickMode,
    ) -> Result<(), String> {
        use wasm_bindgen::JsCast as _;

        let win = web_sys::window().ok_or_else(|| "missing browser window".to_owned())?;
        let doc = win
            .document()
            .ok_or_else(|| "missing browser document".to_owned())?;
        let body = doc
            .body()
            .ok_or_else(|| "missing document body".to_owned())?;
        if let Some(old) = doc.get_element_by_id("layercraft_hidden_file_input") {
            old.remove();
        }
        let elem = doc
            .create_element("input")
            .map_err(|e| format!("failed creating file input: {e:?}"))?;
        let input = elem
            .dyn_into::<web_sys::HtmlInputElement>()
            .map_err(|_| "element is not HtmlInputElement".to_owned())?;
        input.set_id("layercraft_hidden_file_input");
        input.set_type("file");
        input.set_accept(accept);
        let _ = input.style().set_property("display", "none");
        let _ = body.append_child(&input);

        let queue = std::rc::Rc::clone(&self.pending_drops);
        let ctx_opt = self.last_ctx.clone();
        let input_cb = input.clone();
        let cb = wasm_bindgen::closure::Closure::wrap(Box::new(move |_ev: web_sys::Event| {
            let file_opt = input_cb.files().and_then(|fl| fl.get(0));
            input_cb.remove();
            let Some(file) = file_opt else {
                return;
            };
            let name = file.name();
            let promise = file.array_buffer();
            let q = std::rc::Rc::clone(&queue);
            let c = ctx_opt.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let bytes_res = match wasm_bindgen_futures::JsFuture::from(promise).await {
                    Ok(buf) => Ok(js_sys::Uint8Array::new(&buf).to_vec()),
                    Err(e) => Err(format!("failed reading file bytes: {e:?}")),
                };
                q.borrow_mut().push((mode, name, bytes_res));
                if let Some(ctx) = &c {
                    ctx.request_repaint();
                }
            });
        }) as Box<dyn FnMut(web_sys::Event)>);

        input.set_onchange(Some(cb.as_ref().unchecked_ref()));
        self.file_picker_cb = Some(cb);
        input.click();
        Ok(())
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn open_file_dialog(&mut self, append: bool) {
        let title = if append {
            "Add 3D Model to Plate"
        } else {
            "Open 3D Model"
        };
        if let Some(path) = rfd::FileDialog::new()
            .set_title(title)
            .add_filter("3D Models", &["stl", "STL", "3mf", "3MF"])
            .pick_file()
        {
            let p = path.to_string_lossy().into_owned();
            self.load_native_path_with_mode(&p, append);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn import_profile_file_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Import Profile JSON")
            .add_filter("JSON Profiles", &["json", "JSON"])
            .pick_file()
        {
            let p = path.to_string_lossy().into_owned();
            self.load_native_profile_path(&p);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn import_profile_dir_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Import Profile Folder")
            .pick_folder()
        {
            let p = path.to_string_lossy().into_owned();
            self.load_native_profile_path(&p);
        }
    }

    pub(crate) fn export_gcode_action(&mut self) {
        self.export_gcode_with_format(self.session.config.machine.binary_gcode);
    }

    pub(crate) fn export_gcode_with_format(&mut self, binary: bool) {
        let filename = self.session.default_export_filename(binary);
        let Some(g) = &self.session.last_gcode else {
            return;
        };

        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut dialog = rfd::FileDialog::new()
                .set_title(if binary {
                    "Export Binary G-code (.bgcode)"
                } else {
                    "Export G-code"
                })
                .set_file_name(&filename);
            dialog = if binary {
                dialog
                    .add_filter("Binary G-code", &["bgcode"])
                    .add_filter("G-code", &["gcode", "gco", "g"])
            } else {
                dialog
                    .add_filter("G-code", &["gcode", "gco", "g"])
                    .add_filter("Binary G-code", &["bgcode"])
            };
            if let Some(path) = dialog.save_file() {
                let lower = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(str::to_ascii_lowercase)
                    .unwrap_or_default();
                let write_binary = match lower.as_str() {
                    "bgcode" => true,
                    "gcode" | "gco" | "g" => false,
                    _ => binary,
                };
                if write_binary {
                    match encode_bgcode(g, &self.session.config, &BgcodeOptions::default()) {
                        Ok(bytes) => match std::fs::write(&path, &bytes) {
                            Ok(()) => {
                                self.last_error = None;
                                self.status_message = format!(
                                    "Exported `{}` ({} bytes .bgcode).",
                                    path.display(),
                                    bytes.len()
                                );
                            }
                            Err(e) => {
                                self.last_error = Some(format!("Export failed: {e}"));
                            }
                        },
                        Err(e) => {
                            self.last_error = Some(format!("Failed encoding .bgcode: {e}"));
                        }
                    }
                } else {
                    match std::fs::write(&path, &g.gcode) {
                        Ok(()) => {
                            self.last_error = None;
                            self.status_message = format!("Exported `{}`.", path.display());
                        }
                        Err(e) => {
                            self.last_error = Some(format!("Export failed: {e}"));
                        }
                    }
                }
            }
        }

        #[cfg(target_arch = "wasm32")]
        {
            if binary {
                match encode_bgcode(g, &self.session.config, &BgcodeOptions::default()) {
                    Ok(bytes) => {
                        match trigger_browser_download_bytes(
                            &filename,
                            &bytes,
                            "application/octet-stream",
                        ) {
                            Ok(()) => {
                                self.last_error = None;
                                self.status_message = format!(
                                    "Triggered browser download `{filename}` ({} bytes).",
                                    bytes.len()
                                );
                            }
                            Err(e) => {
                                self.last_error = Some(format!("Download failed: {e}"));
                            }
                        }
                    }
                    Err(e) => {
                        self.last_error = Some(format!("Failed encoding .bgcode: {e}"));
                    }
                }
            } else {
                match trigger_browser_download(&filename, &g.gcode) {
                    Ok(()) => {
                        self.last_error = None;
                        self.status_message = format!("Triggered browser download `{filename}`.");
                    }
                    Err(e) => {
                        self.last_error = Some(format!("Download failed: {e}"));
                    }
                }
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn is_printer_busy(&self) -> bool {
        self.pending_printer_job.is_some()
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn upload_progress_snapshot(&self) -> Option<(usize, usize)> {
        self.pending_printer_job
            .as_ref()
            .and_then(|j| j.upload_progress.as_ref())
            .map(|p| p.snapshot())
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn cancel_printer_upload_action(&mut self) {
        self.next_printer_job_id = self.next_printer_job_id.wrapping_add(1);
        if let Some(job) = self.pending_printer_job.take()
            && let Some(p) = job.upload_progress
        {
            p.cancel();
        }
        self.last_error = None;
        self.status_message = "Printer upload cancelled.".to_owned();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn open_upload_dialog(&mut self) {
        let supports_bg = self.printer_draft.protocol.supports_bgcode();
        let binary = supports_bg
            && (self.printer_draft.prefer_bgcode || self.session.config.machine.binary_gcode);
        self.upload_binary = binary;
        self.upload_filename = self.session.default_export_filename(binary);
        self.upload_overwrite = true;
        self.upload_start_print = false;
        self.show_upload_dialog = true;
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn start_printer_probe_action(&mut self, ctx: &egui::Context) {
        if self.pending_printer_job.is_some() {
            return;
        }
        let job_id = self.next_printer_job_id.wrapping_add(1);
        self.next_printer_job_id = job_id;
        let prof = self.printer_draft.clone();
        let (tx, rx) = mpsc::channel();
        let ctx_clone = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("layercraft-printer-probe".to_owned())
            .spawn(move || {
                let res = probe_printer(&prof).map_err(|e| e.to_string());
                let _ = tx.send(PendingPrinterOutcome::Probe {
                    job_id,
                    result: res,
                });
                ctx_clone.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.last_error = None;
                self.status_message = format!("Connecting to `{}`...", self.printer_draft.base_url);
                self.pending_printer_job = Some(PendingPrinterJob {
                    rx,
                    upload_progress: None,
                });
            }
            Err(e) => {
                self.last_error = Some(format!("Failed spawning printer probe: {e}"));
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn start_printer_upload_action(&mut self, ctx: &egui::Context) {
        if self.pending_printer_job.is_some() {
            return;
        }
        let Some(gcode_out) = self.session.last_gcode.clone() else {
            self.last_error = Some("Slice the plate before uploading to a printer.".to_owned());
            return;
        };
        let job_id = self.next_printer_job_id.wrapping_add(1);
        self.next_printer_job_id = job_id;
        let prof = self.printer_draft.clone();
        let cfg = self.session.config.clone();
        let binary = self.upload_binary;
        let opts = UploadOptions {
            remote_path: self.upload_filename.trim().to_owned(),
            storage: None,
            start_print: self.upload_start_print,
            overwrite: self.upload_overwrite,
        };
        let upload_progress = Arc::new(UploadProgress::new());
        let progress_bg = Arc::clone(&upload_progress);
        let (tx, rx) = mpsc::channel();
        let ctx_clone = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("layercraft-printer-upload".to_owned())
            .spawn(move || {
                let res = (|| -> Result<UploadResult, PrinterError> {
                    if progress_bg.is_cancelled() {
                        return Err(PrinterError::Cancelled);
                    }
                    let payload = if binary {
                        encode_bgcode(&gcode_out, &cfg, &BgcodeOptions::default())
                            .map_err(|e| PrinterError::Transport(e.to_string()))?
                    } else {
                        gcode_out.gcode.into_bytes()
                    };
                    if progress_bg.is_cancelled() {
                        return Err(PrinterError::Cancelled);
                    }
                    upload_to_printer_with_progress(
                        &prof,
                        &payload,
                        &opts,
                        Some(progress_bg.as_ref()),
                    )
                })();
                let _ = tx.send(PendingPrinterOutcome::Upload {
                    job_id,
                    result: res,
                });
                ctx_clone.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.last_error = None;
                self.status_message = format!(
                    "Uploading `{}` to `{}`...",
                    self.upload_filename, self.printer_draft.name
                );
                self.pending_printer_job = Some(PendingPrinterJob {
                    rx,
                    upload_progress: Some(upload_progress),
                });
            }
            Err(e) => {
                self.last_error = Some(format!("Failed starting upload thread: {e}"));
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn poll_pending_printer_job(&mut self, ctx: &egui::Context) {
        let outcome = match self.pending_printer_job.as_ref() {
            Some(job) => match job.rx.try_recv() {
                Ok(o) => Some(o),
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(60));
                    None
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending_printer_job = None;
                    None
                }
            },
            None => None,
        };
        let Some(outcome) = outcome else {
            return;
        };
        self.pending_printer_job = None;
        match outcome {
            PendingPrinterOutcome::Probe { job_id, .. }
            | PendingPrinterOutcome::Upload { job_id, .. }
                if job_id != self.next_printer_job_id => {}
            PendingPrinterOutcome::Probe {
                result: Ok(report), ..
            } => {
                self.last_error = None;
                self.status_message = format!(
                    "Connected to `{}` ({}{}{}).",
                    report.printer_name,
                    report.state.as_deref().unwrap_or("online"),
                    report
                        .model
                        .as_deref()
                        .map(|m| format!(" · {m}"))
                        .unwrap_or_default(),
                    report
                        .firmware
                        .as_deref()
                        .map(|f| format!(" · fw {f}"))
                        .unwrap_or_default()
                );
                self.printer_status_report = Some(report);
            }
            PendingPrinterOutcome::Probe {
                result: Err(err), ..
            } => {
                self.printer_status_report = None;
                self.last_error = Some(format!("Printer probe failed: {err}"));
            }
            PendingPrinterOutcome::Upload {
                result: Ok(res), ..
            } => {
                self.last_error = None;
                self.show_upload_dialog = false;
                self.status_message = format!(
                    "{} `{}/{}` ({} bytes, HTTP {}).",
                    res.message, res.storage, res.remote_path, res.bytes_uploaded, res.http_status
                );
            }
            PendingPrinterOutcome::Upload {
                result: Err(PrinterError::Cancelled),
                ..
            } => {
                self.last_error = None;
                self.status_message = "Printer upload cancelled.".to_owned();
            }
            PendingPrinterOutcome::Upload {
                result: Err(err), ..
            } => {
                self.last_error = Some(format!("Printer upload failed: {err}"));
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn draw_printer_upload_dialog(&mut self, ctx: &egui::Context, p: &ThemePalette) {
        if !self.show_upload_dialog {
            return;
        }
        let mut open = self.show_upload_dialog;
        let busy = self.is_printer_busy();
        let mut upload_clicked = false;
        let mut cancel_clicked = false;
        let mut open_machine_tab = false;

        egui::Window::new("Send to Printer")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(360.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;

                if !self.session.printers.is_empty() {
                    let mut picked: Option<String> = None;
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Printer").color(p.text_secondary));
                        egui::ComboBox::from_id_salt("upload_printer_sel")
                            .width(220.0)
                            .selected_text(self.printer_draft.name.as_str())
                            .show_ui(ui, |ui| {
                                for pr in &self.session.printers {
                                    if ui
                                        .selectable_label(pr.id == self.printer_draft.id, &pr.name)
                                        .clicked()
                                    {
                                        picked = Some(pr.id.clone());
                                    }
                                }
                            });
                    });
                    if let Some(id) = picked {
                        self.select_saved_printer(&id);
                        let bin = self.printer_draft.protocol.supports_bgcode()
                            && (self.printer_draft.prefer_bgcode
                                || self.session.config.machine.binary_gcode);
                        self.upload_binary = bin;
                        self.upload_filename = self.session.default_export_filename(bin);
                    }
                }

                ui.label(
                    RichText::new(format!(
                        "{} · {}",
                        self.printer_draft.protocol.label(),
                        if self.printer_draft.base_url.trim().is_empty() {
                            "(no URL configured)"
                        } else {
                            self.printer_draft.base_url.as_str()
                        }
                    ))
                    .size(11.5)
                    .color(p.text_muted),
                );

                if self.printer_draft.base_url.trim().is_empty() {
                    ui.label(
                        RichText::new(
                            "Configure a printer URL & credentials in Machine -> Direct Printer Upload.",
                        )
                        .size(11.5)
                        .color(p.warning),
                    );
                    if ui_kit::secondary_button(ui, p, None, "Open Machine Settings").clicked() {
                        open_machine_tab = true;
                    }
                }

                ui.horizontal(|ui| {
                    ui.label(RichText::new("Format").color(p.text_secondary));
                    if ui.selectable_label(self.upload_binary, ".bgcode (Binary)").clicked()
                        && !self.upload_binary
                    {
                        self.upload_binary = true;
                        if self.upload_filename.to_ascii_lowercase().ends_with(".gcode") {
                            let stem_len = self.upload_filename.len().saturating_sub(6);
                            let stem = self.upload_filename.get(..stem_len).unwrap_or("print");
                            self.upload_filename = format!("{stem}.bgcode");
                        } else if !self.upload_filename.to_ascii_lowercase().ends_with(".bgcode") {
                            self.upload_filename.push_str(".bgcode");
                        }
                    }
                    if ui.selectable_label(!self.upload_binary, ".gcode (Text)").clicked()
                        && self.upload_binary
                    {
                        self.upload_binary = false;
                        if self.upload_filename.to_ascii_lowercase().ends_with(".bgcode") {
                            let stem_len = self.upload_filename.len().saturating_sub(7);
                            let stem = self.upload_filename.get(..stem_len).unwrap_or("print");
                            self.upload_filename = format!("{stem}.gcode");
                        } else if !self.upload_filename.to_ascii_lowercase().ends_with(".gcode") {
                            self.upload_filename.push_str(".gcode");
                        }
                    }
                });

                ui.horizontal(|ui| {
                    ui.label(RichText::new("Filename").color(p.text_secondary));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.upload_filename)
                            .desired_width(240.0),
                    );
                });

                ui.checkbox(&mut self.upload_overwrite, "Overwrite existing file on printer");
                ui.checkbox(
                    &mut self.upload_start_print,
                    "Start printing immediately after upload",
                );
                if self.upload_start_print {
                    ui.label(
                        RichText::new("Confirm the build plate is clear before starting.")
                            .size(11.0)
                            .color(p.warning),
                    );
                }

                if let Some((sent, total)) = self.upload_progress_snapshot() {
                    let (frac, text) = if total > 0 {
                        let f = ((sent.min(total)) as f32) / (total as f32);
                        let sent_kib = (sent as f64) / 1024.0;
                        let total_kib = (total as f64) / 1024.0;
                        (
                            f,
                            format!("{sent_kib:.1} / {total_kib:.1} KiB ({:.0}%)", f * 100.0),
                        )
                    } else {
                        (0.0, "Encoding / connecting...".to_owned())
                    };
                    ui.add(egui::ProgressBar::new(frac).text(text).animate(total == 0));
                }

                if let Some(err) = &self.last_error {
                    ui.label(RichText::new(err).size(11.5).color(p.danger));
                }

                ui.separator();
                let mut cancel_upload_clicked = false;
                let uploading = busy && self.upload_progress_snapshot().is_some();
                ui.horizontal(|ui| {
                    if uploading {
                        if ui_kit::secondary_button(ui, p, None, "Cancel Upload").clicked() {
                            cancel_upload_clicked = true;
                        }
                    } else {
                        ui.add_enabled_ui(!busy, |ui| {
                            if ui_kit::secondary_button(ui, p, None, "Cancel").clicked() {
                                cancel_clicked = true;
                            }
                        });
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let ready = !busy && !self.printer_draft.base_url.trim().is_empty();
                        ui.add_enabled_ui(ready, |ui| {
                            let label = if busy { "Uploading..." } else { "Upload" };
                            if ui_kit::primary_button(ui, p, icons::Icon::SendPrinter, label)
                                .clicked()
                            {
                                upload_clicked = true;
                            }
                        });
                    });
                });
                if cancel_upload_clicked {
                    self.cancel_printer_upload_action();
                }
            });

        self.show_upload_dialog = open && !cancel_clicked;
        if open_machine_tab {
            self.sidebar_open = true;
            self.sidebar_tab = SidebarTab::Machine;
            self.show_upload_dialog = false;
        } else if upload_clicked {
            self.start_printer_upload_action(ctx);
        }
    }

    fn draw_status_bar(&self, ui: &mut egui::Ui, p: &ThemePalette) {
        let resp = egui::Panel::bottom("layercraft_status_bar")
            .frame(
                egui::Frame::NONE
                    .fill(p.bg_chrome)
                    .inner_margin(Margin::symmetric(12, 4)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_min_height(18.0);
                    let dot_col = if self.last_error.is_some() {
                        p.danger
                    } else {
                        p.success
                    };
                    let (dot_r, _) =
                        ui.allocate_exact_size(egui::vec2(8.0, 14.0), egui::Sense::hover());
                    ui.painter().circle_filled(dot_r.center(), 3.5, dot_col);

                    if let Some(err) = &self.last_error {
                        ui.label(RichText::new(err).size(11.5).color(p.danger));
                    } else {
                        ui.label(
                            RichText::new(&self.status_message)
                                .size(11.5)
                                .color(p.text_secondary),
                        );
                    }

                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let avail_w = ui.available_width();
                        if let Some(ep) = &self.bridge_endpoint {
                            let label = match &self.last_mcp_activity {
                                Some(act) => format!("MCP: {act}"),
                                None => "MCP Live".to_owned(),
                            };
                            ui.add_space(6.0);
                            let badge = egui::Frame::new()
                                .fill(p.bg_card)
                                .stroke(Stroke::new(1.0, p.accent))
                                .corner_radius(4.0)
                                .inner_margin(Margin::symmetric(6, 1))
                                .show(ui, |ui| {
                                    ui.label(RichText::new(label).size(10.5).color(p.accent));
                                });
                            badge
                                .response
                                .on_hover_text(format!("MCP Live Studio bridge listening at {ep}"));
                        }
                        if avail_w >= 460.0 {
                            ui.label(
                                RichText::new(format!(
                                    "Bed {:.0}×{:.0}×{:.0} mm  ·  Drag: orbit · Shift+Drag: pan · Wheel: zoom · F: fit",
                                    self.session.config.machine.bed_width_mm,
                                    self.session.config.machine.bed_depth_mm,
                                    self.session.config.machine.bed_height_mm,
                                ))
                                .size(11.0)
                                .color(p.text_muted),
                            );
                        }
                    });
                });
            })
            .response;

        let r = resp.rect;
        ui.painter().line_segment(
            [pos2(r.left(), r.top()), pos2(r.right(), r.top())],
            Stroke::new(1.0, p.border_subtle),
        );
    }

    fn viewport_panel(&mut self, ui: &mut egui::Ui, p: &ThemePalette) -> egui::Rect {
        let rect = egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                let rect = ui.available_rect_before_wrap();
                let resp = ui.allocate_rect(rect, egui::Sense::click_and_drag());

                if rect.height() > 1.0 {
                    self.viewport_aspect = rect.width() / rect.height();
                }

                let shift = ui.input(|i| i.modifiers.shift);
                if resp.dragged_by(egui::PointerButton::Middle)
                    || (resp.dragged_by(egui::PointerButton::Primary) && shift)
                {
                    let d = resp.drag_delta();
                    self.camera.pan(d.x, d.y);
                } else if resp.dragged_by(egui::PointerButton::Primary)
                    || resp.dragged_by(egui::PointerButton::Secondary)
                {
                    let d = resp.drag_delta();
                    self.camera.orbit(d.x, d.y);
                }

                if resp.hovered() {
                    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                    if scroll.abs() > 0.0 {
                        self.camera.zoom(scroll);
                    }
                }

                if self.view_mode == ViewMode::Prepare
                    && resp.clicked_by(egui::PointerButton::Primary)
                    && !shift
                    && rect.width() > 1.0
                    && rect.height() > 1.0
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    let ndc_x = ((pos.x - rect.left()) / rect.width()) * 2.0 - 1.0;
                    let ndc_y = 1.0 - ((pos.y - rect.top()) / rect.height()) * 2.0;
                    self.pick_at_ndc(ndc_x, ndc_y);
                }

                if self.view_mode == ViewMode::Preview && self.playback_playing {
                    let step_count = self.active_layer_step_count();
                    let now = ui.input(|i| i.time);
                    if step_count == 0 || self.layer_move_step >= step_count {
                        self.playback_playing = false;
                        self.last_playback_time_s = None;
                    } else {
                        if let Some(prev_t) = self.last_playback_time_s {
                            let dt = (now - prev_t).clamp(0.0, 0.1);
                            let rate = ((step_count as f64) / 6.0).clamp(12.0, 140.0)
                                * f64::from(self.playback_speed);
                            self.playback_accum += dt * rate;
                            let advance = self.playback_accum.floor() as usize;
                            if advance > 0 {
                                self.playback_accum -= advance as f64;
                                self.layer_move_step =
                                    (self.layer_move_step + advance).min(step_count);
                            }
                        }
                        self.last_playback_time_s = Some(now);
                        if self.layer_move_step >= step_count {
                            self.playback_playing = false;
                            self.last_playback_time_s = None;
                        } else {
                            ui.ctx().request_repaint();
                        }
                    }
                }

                let (draw_mesh, draw_ghost, toolpath_range, travel_range, nozzle_pos) =
                    match self.view_mode {
                        ViewMode::Home | ViewMode::Prepare => (true, false, 0..0, 0..0, None),
                        ViewMode::Preview => {
                            let n = self.scene.layer_ranges.len();
                            if n == 0 {
                                (true, false, 0..0, 0..0, None)
                            } else {
                                let top = self.max_layer.min(n - 1);
                                let start_layer = if self.single_layer_only { top } else { 0 };
                                let v_start = self
                                    .scene
                                    .layer_ranges
                                    .get(start_layer)
                                    .map(|r| r.start)
                                    .unwrap_or(0);
                                let top_layer_v_start = self
                                    .scene
                                    .layer_ranges
                                    .get(top)
                                    .map(|r| r.start)
                                    .unwrap_or(0);
                                let steps = self
                                    .scene
                                    .layer_steps
                                    .get(top)
                                    .map(Vec::as_slice)
                                    .unwrap_or(&[]);
                                let step_count = steps.len();
                                let cur_step = self.layer_move_step.min(step_count);

                                let v_end = if step_count == 0 {
                                    self.scene.layer_ranges.get(top).map(|r| r.end).unwrap_or(0)
                                } else if cur_step == 0 {
                                    top_layer_v_start
                                } else {
                                    steps
                                        .get(cur_step - 1)
                                        .map(|s| s.toolpath_vertex_end)
                                        .unwrap_or(top_layer_v_start)
                                };

                                let t_range = if self.show_travels && step_count > 0 {
                                    let t_start = self
                                        .scene
                                        .travel_layer_ranges
                                        .get(top)
                                        .map(|r| r.start)
                                        .unwrap_or(0);
                                    let t_end = if cur_step == 0 {
                                        t_start
                                    } else {
                                        steps
                                            .get(cur_step - 1)
                                            .map(|s| s.travel_vertex_end)
                                            .unwrap_or(t_start)
                                    };
                                    t_start..t_end
                                } else {
                                    0..0
                                };

                                let show_nozzle = step_count > 0
                                    && (cur_step < step_count
                                        || self.playback_playing
                                        || self.show_travels);
                                let n_pos = if show_nozzle {
                                    if cur_step == 0 {
                                        steps.first().map(|s| s.nozzle_pos)
                                    } else {
                                        steps.get(cur_step - 1).map(|s| s.nozzle_pos)
                                    }
                                } else {
                                    None
                                };

                                (false, self.show_ghost_mesh, v_start..v_end, t_range, n_pos)
                            }
                        }
                    };

                let ppp = ui.ctx().pixels_per_point();
                let viewport_size_px = [
                    (rect.width() * ppp).round().clamp(1.0, 4096.0) as u32,
                    (rect.height() * ppp).round().clamp(1.0, 4096.0) as u32,
                ];
                let cb = egui_wgpu::Callback::new_paint_callback(
                    rect,
                    ViewportCallback {
                        scene: Arc::clone(&self.scene),
                        view_proj: self.camera.view_proj(self.viewport_aspect),
                        eye_pos: self.camera.eye_pos_array(),
                        dark_mode: p.dark,
                        viewport_size_px,
                        draw_mesh,
                        draw_ghost,
                        toolpath_range,
                        travel_range,
                        nozzle_pos,
                    },
                );
                ui.painter().add(cb);
                rect
            })
            .inner;

        viewport_hud::draw_viewport_hud(self, ui.ctx(), rect, p);
        rect
    }

    /// Run one frame of the LayerCraft UI inside `ui` (shared between live `eframe` and headless captures).
    pub fn render_ui(&mut self, ui: &mut egui::Ui) {
        if let Some(mut poller) = self.external_poller.take() {
            poller(self);
            if self.external_poller.is_none() {
                self.external_poller = Some(poller);
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.poll_pending_slice(ui.ctx());
            self.poll_pending_printer_job(ui.ctx());
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.last_ctx = Some(ui.ctx().clone());
            self.poll_pending_web_slice(ui.ctx());
            self.poll_pending_browser_recent();
        }

        self.pointer_active = ui.ctx().egui_is_using_pointer();
        if !self.pointer_active {
            self.drag_undo_active = false;
            self.session.set_undo_suppressed(false);
        }

        if !self.styles_installed {
            theme::install_styles(ui.ctx());
            self.styles_installed = true;
        }
        ui.ctx().set_theme(self.theme_mode.to_preference());
        let palette = theme::active_palette(ui.ctx());
        if palette.dark != self.last_scene_dark {
            self.last_scene_dark = palette.dark;
            self.rebuild_scene();
        }

        self.handle_dropped_files(ui.ctx());

        if !ui.ctx().egui_wants_keyboard_input() {
            let undo_pressed = ui.input(|i| {
                i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::Z)
            });
            let redo_pressed = ui.input(|i| {
                i.modifiers.command
                    && ((i.modifiers.shift && i.key_pressed(egui::Key::Z))
                        || i.key_pressed(egui::Key::Y))
            });
            let save_pressed = ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S));

            if undo_pressed {
                self.undo_action();
            } else if redo_pressed {
                self.redo_action();
            } else if save_pressed {
                self.save_project_action();
            }

            if self.view_mode != ViewMode::Home && ui.input(|i| i.key_pressed(egui::Key::F)) {
                self.frame_camera(CameraPreset::Isometric);
            }
            if self.view_mode == ViewMode::Preview && !self.scene.layer_ranges.is_empty() {
                if ui.input(|i| i.key_pressed(egui::Key::T)) {
                    self.show_travels = !self.show_travels;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Space)) {
                    let max_steps = self.active_layer_step_count();
                    if max_steps > 0 {
                        if self.playback_playing {
                            self.playback_playing = false;
                            self.last_playback_time_s = None;
                        } else {
                            if self.layer_move_step >= max_steps {
                                self.layer_move_step = 0;
                            }
                            self.playback_playing = true;
                            self.playback_accum = 0.0;
                            self.last_playback_time_s = None;
                        }
                    }
                }
                if ui.input(|i| i.key_pressed(egui::Key::ArrowLeft)) && self.layer_move_step > 0 {
                    self.layer_move_step -= 1;
                    self.playback_playing = false;
                    self.last_playback_time_s = None;
                }
                if ui.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                    let max_steps = self.active_layer_step_count();
                    if self.layer_move_step < max_steps {
                        self.layer_move_step += 1;
                    }
                    self.playback_playing = false;
                    self.last_playback_time_s = None;
                }
                if ui.input(|i| i.key_pressed(egui::Key::ArrowUp))
                    && self.max_layer + 1 < self.scene.layer_ranges.len()
                {
                    let next = self.max_layer + 1;
                    self.set_active_layer(next);
                }
                if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) && self.max_layer > 0 {
                    let prev = self.max_layer - 1;
                    self.set_active_layer(prev);
                }
            }
        }

        let screen_w = ui.available_width();
        if screen_w < 760.0 && !self.narrow_layout_applied {
            self.sidebar_open = false;
            self.narrow_layout_applied = true;
        } else if screen_w >= 820.0 && self.narrow_layout_applied {
            self.narrow_layout_applied = false;
        }

        toolbar::draw_header_bar(self, ui, &palette);
        self.draw_status_bar(ui, &palette);

        if self.view_mode == ViewMode::Home {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(palette.bg_panel))
                .show(ui, |ui| {
                    home::draw_home_page(self, ui, &palette);
                });
        } else {
            let cfg_before_sidebar = self.session.config.clone();
            if screen_w < 760.0 {
                let vp_rect = self.viewport_panel(ui, &palette);
                sidebar::draw_sidebar_overlay(self, ui.ctx(), vp_rect, &palette);
            } else {
                sidebar::draw_sidebar(self, ui, &palette);
                let _ = self.viewport_panel(ui, &palette);
            }
            if self.session.config != cfg_before_sidebar {
                self.printability_report = None;
                self.project_dirty = true;
                if self.pending_config_undo.is_none() {
                    self.pending_config_undo = Some(cfg_before_sidebar);
                }
            }
            if !ui.ctx().egui_is_using_pointer()
                && !ui.ctx().egui_wants_keyboard_input()
                && let Some(prev_cfg) = self.pending_config_undo.take()
                && self.session.config != prev_cfg
            {
                self.session
                    .push_config_undo_snapshot("Edit settings", prev_cfg);
            }
        }

        if let Some(rect) = ui.ctx().input(|i| i.viewport().inner_rect) {
            let sz = [rect.width(), rect.height()];
            if sz[0] >= 320.0 && sz[1] >= 240.0 {
                self.last_window_inner_size = Some(sz);
                if !ui.ctx().egui_is_using_pointer() {
                    let changed = self.prefs.window_size.is_none_or(|prev| {
                        (prev[0] - sz[0]).abs() > 2.0 || (prev[1] - sz[1]).abs() > 2.0
                    });
                    if changed {
                        self.sync_and_save_prefs();
                    }
                }
            }
        }

        #[cfg(not(target_arch = "wasm32"))]
        self.draw_printer_upload_dialog(ui.ctx(), &palette);
        self.draw_save_preset_dialog(ui.ctx(), &palette);
        self.draw_unsaved_changes_dialog(ui.ctx(), &palette);
    }

    pub(crate) fn sync_and_save_prefs(&mut self) {
        self.prefs.theme_mode = Some(
            match self.theme_mode {
                ThemeMode::Dark => "dark",
                ThemeMode::Light => "light",
                ThemeMode::System => "system",
            }
            .to_owned(),
        );
        self.prefs.active_profiles = self.session.active_profiles.clone();
        if let Some(sz) = self.last_window_inner_size {
            self.prefs.window_size = Some(sz);
        }
        #[cfg(not(test))]
        {
            let _ = self.prefs.save_to_disk();
        }
    }

    pub(crate) fn cycle_theme_mode(&mut self) {
        self.theme_mode = self.theme_mode.cycle();
        self.sync_and_save_prefs();
    }

    fn refresh_printability_after_slice(&mut self) {
        let Some(mesh) = self
            .session
            .prepared_mesh
            .as_deref()
            .or(self.session.mesh.as_deref())
        else {
            self.printability_report = None;
            return;
        };
        let report = layercraft_slice::analyze_printability(
            mesh,
            self.session.sliced.as_ref(),
            &self.session.config,
        );
        self.printability_report = Some(report);
    }

    /// Run Printability / DFM diagnostics without blocking on a full slice when pre-slice checks suffice.
    pub fn analyze_printability_action(&mut self) {
        match execute(
            &mut self.session,
            "plate.analyze_printability",
            serde_json::json!({ "auto_slice": false }),
        ) {
            Ok(val) => {
                self.last_error = None;
                match serde_json::from_value::<PrintabilityReport>(val) {
                    Ok(report) => {
                        let summary = format!(
                            "DFM: {} ({} issue(s))",
                            match report.overall_risk {
                                PrintabilityRisk::Low => "Low risk",
                                PrintabilityRisk::Medium => "Moderate risk",
                                PrintabilityRisk::High => "High risk",
                            },
                            report.issues.len()
                        );
                        self.printability_report = Some(report);
                        self.status_message = summary;
                    }
                    Err(e) => {
                        self.last_error = Some(format!("Failed decoding DFM report: {e}"));
                    }
                }
            }
            Err(e) => {
                self.last_error = Some(format!("DFM analysis failed: {e}"));
            }
        }
    }

    /// Enable automatic Tree Supports (undoable) and refresh DFM analysis.
    pub fn apply_quick_fix_tree_supports(&mut self) {
        self.cancel_pending_slice();
        let prev_cfg = self.session.config.clone();
        self.session.config.process.support.enabled = true;
        self.session.config.process.support.style = SupportStyle::Tree;
        if self.session.config != prev_cfg {
            self.session
                .push_config_undo_snapshot("Enable tree supports", prev_cfg);
        }
        self.session.sliced = None;
        self.session.last_gcode = None;
        self.project_dirty = true;
        self.view_mode = ViewMode::Prepare;
        self.rebuild_scene();
        self.analyze_printability_action();
        self.status_message = "Enabled automatic Tree supports.".to_owned();
    }

    /// Add a 5 mm outer brim (undoable) to improve first-layer bed adhesion and refresh DFM analysis.
    pub fn apply_quick_fix_brim(&mut self) {
        self.cancel_pending_slice();
        let prev_cfg = self.session.config.clone();
        if self.session.config.process.brim_width_mm < 5.0 {
            self.session.config.process.brim_width_mm = 5.0;
        }
        if self.session.config != prev_cfg {
            self.session
                .push_config_undo_snapshot("Add 5mm brim", prev_cfg);
        }
        self.session.sliced = None;
        self.session.last_gcode = None;
        self.project_dirty = true;
        self.view_mode = ViewMode::Prepare;
        self.rebuild_scene();
        self.analyze_printability_action();
        self.status_message = "Enabled 5 mm outer brim.".to_owned();
    }

    pub(crate) fn open_save_preset_dialog(&mut self, kind: ProfileKind) {
        let base_name = match kind {
            ProfileKind::Machine => self
                .selected_machine_profile
                .as_deref()
                .unwrap_or("Printer"),
            ProfileKind::Filament => self
                .selected_filament_profile
                .as_deref()
                .unwrap_or("Filament"),
            ProfileKind::Process => self
                .selected_process_profile
                .as_deref()
                .unwrap_or("Process"),
            ProfileKind::MachineModel => "Preset",
        };
        let suggested = if self.session.profiles.is_builtin(kind, base_name) {
            format!("Custom {base_name}")
        } else {
            base_name.to_owned()
        };
        self.save_preset_dialog = Some(SavePresetDialogState {
            kind,
            name: suggested,
            error: None,
        });
    }

    pub(crate) fn save_user_preset(
        &mut self,
        kind: ProfileKind,
        name: &str,
    ) -> Result<String, String> {
        let persist = !cfg!(any(target_arch = "wasm32", test));
        let val = execute(
            &mut self.session,
            "profile.save_user",
            serde_json::json!({
                "kind": kind,
                "name": name,
                "activate": true,
                "persist": persist,
            }),
        )
        .map_err(|e| e.to_string())?;
        let res: SaveUserProfileResult = serde_json::from_value(val).map_err(|e| e.to_string())?;
        match kind {
            ProfileKind::Machine => {
                self.selected_machine_profile = Some(res.summary.name.clone());
            }
            ProfileKind::Filament => {
                self.selected_filament_profile = Some(res.summary.name.clone());
            }
            ProfileKind::Process => {
                self.selected_process_profile = Some(res.summary.name.clone());
            }
            ProfileKind::MachineModel => {}
        }
        self.last_error = None;
        #[cfg(target_arch = "wasm32")]
        {
            let _ = prefs::save_user_profiles_to_browser(&self.session.profiles);
        }
        self.sync_and_save_prefs();
        self.status_message = format!("Saved custom preset `{}`.", res.summary.name);
        Ok(res.summary.name)
    }

    pub fn delete_user_preset_action(&mut self, kind: ProfileKind, name: &str) {
        let persist = !cfg!(any(target_arch = "wasm32", test));
        match execute(
            &mut self.session,
            "profile.delete_user",
            serde_json::json!({
                "kind": kind,
                "name": name,
                "persist": persist,
            }),
        ) {
            Ok(_) => {
                match kind {
                    ProfileKind::Machine => {
                        if self.selected_machine_profile.as_deref() == Some(name) {
                            self.selected_machine_profile = None;
                        }
                    }
                    ProfileKind::Filament => {
                        if self.selected_filament_profile.as_deref() == Some(name) {
                            self.selected_filament_profile = None;
                        }
                    }
                    ProfileKind::Process => {
                        if self.selected_process_profile.as_deref() == Some(name) {
                            self.selected_process_profile = None;
                        }
                    }
                    ProfileKind::MachineModel => {}
                }
                self.last_error = None;
                #[cfg(target_arch = "wasm32")]
                {
                    let _ = prefs::save_user_profiles_to_browser(&self.session.profiles);
                }
                self.sync_and_save_prefs();
                self.status_message = format!("Deleted custom preset `{name}`.");
            }
            Err(e) => {
                self.last_error = Some(format!("Delete preset failed: {e}"));
            }
        }
    }

    pub fn clear_plate_action(&mut self) {
        self.cancel_pending_slice();
        match execute(&mut self.session, "plate.clear", Value::Null) {
            Ok(_) => {
                self.last_error = None;
                self.printability_report = None;
                self.project_dirty = false;
                self.selected_object_id = None;
                self.selected_modifier_id = None;
                self.view_mode = ViewMode::Prepare;
                self.max_layer = 0;
                self.layer_move_step = 0;
                self.playback_playing = false;
                self.status_message = "Cleared build plate.".to_owned();
                self.rebuild_scene();
            }
            Err(e) => self.last_error = Some(format!("Failed clearing plate: {e}")),
        }
    }

    pub(crate) fn request_discard_or_run(&mut self, action: PendingDiscardAction) {
        if self.project_dirty && !self.session.objects.is_empty() {
            self.pending_discard_action = Some(action);
        } else {
            self.execute_discard_action(action);
        }
    }

    fn execute_discard_action(&mut self, action: PendingDiscardAction) {
        match action {
            PendingDiscardAction::ClearPlate => self.clear_plate_action(),
            PendingDiscardAction::NewEmptyProject => self.new_empty_project_action(),
            PendingDiscardAction::LoadSample(shape) => self.load_sample_with_mode(shape, false),
            PendingDiscardAction::OpenFileDialog => self.open_file_dialog(false),
            PendingDiscardAction::OpenRecentId(id) => self.open_recent_project(&id),
        }
    }

    fn draw_save_preset_dialog(&mut self, ctx: &egui::Context, p: &ThemePalette) {
        let Some(mut state) = self.save_preset_dialog.take() else {
            return;
        };
        let mut open = true;
        let mut save_clicked = false;
        let mut cancel_clicked = false;
        let kind_label = match state.kind {
            ProfileKind::Machine => "Printer",
            ProfileKind::Filament => "Filament",
            ProfileKind::Process => "Process",
            ProfileKind::MachineModel => "Preset",
        };

        egui::Window::new(format!("Save Custom {kind_label} Preset"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("Snapshots the current settings as a reusable custom preset.")
                        .size(11.5)
                        .color(p.text_secondary),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut state.name)
                            .desired_width(220.0)
                            .hint_text("My Preset"),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        save_clicked = true;
                    }
                });
                if let Some(err) = &state.error {
                    ui.add_space(4.0);
                    ui.label(RichText::new(err).size(11.0).color(p.danger));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui_kit::primary_button(ui, p, icons::Icon::SaveProject, "Save Preset")
                        .clicked()
                    {
                        save_clicked = true;
                    }
                    if ui_kit::secondary_button(ui, p, None, "Cancel").clicked() {
                        cancel_clicked = true;
                    }
                });
            });

        if cancel_clicked || !open {
            return;
        }
        if save_clicked {
            match self.save_user_preset(state.kind, &state.name) {
                Ok(_) => return,
                Err(err) => {
                    state.error = Some(err);
                }
            }
        }
        self.save_preset_dialog = Some(state);
    }

    fn draw_unsaved_changes_dialog(&mut self, ctx: &egui::Context, p: &ThemePalette) {
        let Some(action) = self.pending_discard_action.clone() else {
            return;
        };
        let mut open = true;
        let mut save_and_continue = false;
        let mut discard_clicked = false;
        let mut cancel_clicked = false;

        egui::Window::new("Unsaved Project Changes")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(340.0)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "You have unsaved plate or settings changes. Save as .3mf before continuing, or discard them?",
                    )
                    .size(12.0)
                    .color(p.text_primary),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui_kit::primary_button(ui, p, icons::Icon::SaveProject, "Save .3mf")
                        .clicked()
                    {
                        save_and_continue = true;
                    }
                    if ui_kit::secondary_button(ui, p, None, "Discard Changes").clicked() {
                        discard_clicked = true;
                    }
                    if ui_kit::secondary_button(ui, p, None, "Cancel").clicked() {
                        cancel_clicked = true;
                    }
                });
            });

        if cancel_clicked || !open {
            self.pending_discard_action = None;
        } else if discard_clicked {
            self.pending_discard_action = None;
            self.execute_discard_action(action);
        } else if save_and_continue {
            self.save_project_action();
            if !self.project_dirty {
                self.pending_discard_action = None;
                self.execute_discard_action(action);
            }
        }
    }
}

impl eframe::App for LayerCraftApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.render_ui(ui);
    }
}

#[cfg(target_arch = "wasm32")]
fn trigger_browser_download(filename: &str, contents: &str) -> Result<(), String> {
    trigger_browser_download_bytes(filename, contents.as_bytes(), "text/plain;charset=utf-8")
}

#[cfg(target_arch = "wasm32")]
fn trigger_browser_download_bytes(
    filename: &str,
    bytes: &[u8],
    mime_type: &str,
) -> Result<(), String> {
    use wasm_bindgen::JsCast as _;

    let win = web_sys::window().ok_or_else(|| "missing browser window".to_owned())?;
    let doc = win
        .document()
        .ok_or_else(|| "missing browser document".to_owned())?;
    let body = doc
        .body()
        .ok_or_else(|| "missing document body".to_owned())?;

    let bag = web_sys::BlobPropertyBag::new();
    bag.set_type(mime_type);
    let array = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::of1(&wasm_bindgen::JsValue::from(array));
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &bag)
        .map_err(|e| format!("failed creating Blob: {e:?}"))?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)
        .map_err(|e| format!("failed creating object URL: {e:?}"))?;

    let elem = doc
        .create_element("a")
        .map_err(|e| format!("failed creating anchor element: {e:?}"))?;
    let anchor = elem
        .dyn_into::<web_sys::HtmlAnchorElement>()
        .map_err(|_| "element is not HtmlAnchorElement".to_owned())?;
    anchor.set_href(&url);
    anchor.set_download(filename);
    let _ = anchor.style().set_property("display", "none");
    let _ = body.append_child(&anchor);
    anchor.click();
    let _ = body.remove_child(&anchor);
    let _ = web_sys::Url::revoke_object_url(&url);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_palettes_and_styles_build_cleanly() {
        let dark = ThemePalette::dark_studio();
        let light = ThemePalette::light_drafting();
        assert!(dark.dark);
        assert!(!light.dark);
        assert_eq!(dark.accent, egui::Color32::from_rgb(20, 184, 166));
        assert_eq!(light.accent, egui::Color32::from_rgb(13, 148, 136));

        let dark_style = theme::build_egui_style(&dark, true);
        let light_style = theme::build_egui_style(&light, false);
        assert!(dark_style.visuals.dark_mode);
        assert!(!light_style.visuals.dark_mode);
        assert_eq!(ThemeMode::Dark.cycle().cycle().cycle(), ThemeMode::Dark);
    }

    #[test]
    fn app_loads_slices_and_switches_modes_cleanly() {
        let mut app = LayerCraftApp::new(Some(SampleShape::Cube20));
        assert_eq!(app.view_mode, ViewMode::Prepare);
        assert_eq!(app.session.objects.len(), 1);
        assert!(app.last_error.is_none());

        app.run_slice();
        assert_eq!(app.view_mode, ViewMode::Preview);
        assert!(app.session.sliced.is_some());
        assert!(app.session.last_gcode.is_some());
        assert!(app.last_error.is_none());
    }

    #[test]
    fn layer_scrubber_travel_preview_and_playback_work_cleanly() {
        let mut app = LayerCraftApp::new(Some(SampleShape::Cube20));
        app.run_slice();
        let total_layers = app.scene.layer_ranges.len();
        assert!(total_layers >= 10);
        let top_steps = app.active_layer_step_count();
        assert!(top_steps > 5);
        assert_eq!(app.layer_move_step, top_steps);

        app.show_travels = true;
        app.layer_move_step = top_steps / 2;
        assert_eq!(app.layer_move_step, top_steps / 2);

        // Switching layers resets move step to 100% of the newly selected layer.
        app.set_active_layer(3);
        assert_eq!(app.max_layer, 3);
        assert_eq!(app.layer_move_step, app.active_layer_step_count());
        assert!(!app.playback_playing);
    }

    #[test]
    fn pick_at_ndc_selects_objects_modifiers_and_clears_on_miss() {
        let mut app = LayerCraftApp::new(Some(SampleShape::Cube20));
        app.load_sample_with_mode(SampleShape::Cylinder, true);
        app.auto_arrange_plate();
        assert_eq!(app.session.objects.len(), 2);

        app.viewport_aspect = 1.6;
        app.frame_camera(CameraPreset::Top);
        app.camera.pitch_rad = 1.54;

        let project_ndc = |cam: &OrbitCamera, aspect: f32, pos: [f64; 3]| -> (f32, f32) {
            let m = cam.view_proj(aspect).to_cols_array_2d();
            let (x, y, z) = (pos[0] as f32, pos[1] as f32, pos[2] as f32);
            let cx = m[0][0] * x + m[1][0] * y + m[2][0] * z + m[3][0];
            let cy = m[0][1] * x + m[1][1] * y + m[2][1] * z + m[3][1];
            let cw = m[0][3] * x + m[1][3] * y + m[2][3] * z + m[3][3];
            (cx / cw, cy / cw)
        };

        let obj0_id = app.session.objects[0].id;
        let obj1_id = app.session.objects[1].id;
        let c0 = app.session.objects[0]
            .prepared_mesh
            .aabb()
            .unwrap()
            .center();
        let c1 = app.session.objects[1]
            .prepared_mesh
            .aabb()
            .unwrap()
            .center();

        let (x0, y0) = project_ndc(&app.camera, app.viewport_aspect, [c0.x, c0.y, c0.z]);
        assert!(app.pick_at_ndc(x0, y0));
        assert_eq!(app.selected_object_id, Some(obj0_id));
        assert_eq!(app.selected_modifier_id, None);

        let (x1, y1) = project_ndc(&app.camera, app.viewport_aspect, [c1.x, c1.y, c1.z]);
        assert!(app.pick_at_ndc(x1, y1));
        assert_eq!(app.selected_object_id, Some(obj1_id));

        // Add modifier offset cleanly away from objects, then pick it.
        app.add_modifier_primitive(
            ModifierPrimitive::Box {
                width_mm: 14.0,
                depth_mm: 14.0,
                height_mm: 14.0,
            },
            ModifierRole::SupportBlocker,
        );
        let mod_id = app.session.modifiers[0].id;
        let mod_role = app.session.modifiers[0].role;
        let mod_ov = app.session.modifiers[0].overrides.clone();
        app.update_modifier(
            mod_id,
            mod_role,
            [60.0, 60.0, 7.0],
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            mod_ov,
        );
        app.frame_camera(CameraPreset::Top);
        app.camera.target = [125.0, 125.0, 5.0];
        app.camera.distance = 220.0;
        let (xm, ym) = project_ndc(&app.camera, app.viewport_aspect, [60.0, 60.0, 7.0]);
        assert!(app.pick_at_ndc(xm, ym));
        assert_eq!(app.selected_modifier_id, Some(mod_id));
        assert_eq!(app.selected_object_id, None);

        // Far corner miss clears selection.
        assert!(!app.pick_at_ndc(0.98, 0.98));
        assert_eq!(app.selected_object_id, None);
        assert_eq!(app.selected_modifier_id, None);
    }

    #[test]
    fn undo_redo_and_project_3mf_roundtrip_in_app() {
        let mut app = LayerCraftApp::new(Some(SampleShape::Cube20));
        // Startup sample does not pollute undo history.
        assert!(!app.session.history_status().can_undo);

        let id = app.session.objects[0].id;
        app.transform_object(id, [85.0, 115.0], [0.0, 0.0, 20.0], [1.2, 1.2, 1.2]);
        app.add_modifier_primitive(
            ModifierPrimitive::Box {
                width_mm: 10.0,
                depth_mm: 10.0,
                height_mm: 8.0,
            },
            ModifierRole::Modifier,
        );
        app.session.config.process.sparse_infill_density = 0.44;
        app.session
            .push_config_undo_snapshot("Edit settings", PrintConfig::default());

        let exported_bytes = export_project_3mf_bytes(&app.session).unwrap();

        // Undo settings edit -> reverts infill density
        app.undo_action();
        assert!(
            (app.session.config.process.sparse_infill_density
                - PrintConfig::default().process.sparse_infill_density)
                .abs()
                < 1e-6
        );

        // Undo modifier add -> removes modifier
        app.undo_action();
        assert!(app.session.modifiers.is_empty());

        // Redo modifier add -> brings modifier back
        app.redo_action();
        assert_eq!(app.session.modifiers.len(), 1);

        // Load saved project .3mf into a fresh app via load_mesh_bytes
        let mut app2 = LayerCraftApp::new(None);
        app2.load_mesh_bytes("saved_project.3mf", exported_bytes);
        assert!(app2.last_error.is_none());
        assert_eq!(app2.session.objects.len(), 1);
        assert_eq!(app2.session.modifiers.len(), 1);
        assert!((app2.session.objects[0].transform.position_xy_mm[0] - 85.0).abs() < 1e-6);
        assert!((app2.session.config.process.sparse_infill_density - 0.44).abs() < 1e-6);
    }

    #[test]
    fn multiplate_app_add_select_and_remove_plates() {
        let mut app = LayerCraftApp::new(Some(SampleShape::Cube20));
        assert_eq!(app.session.plate_count(), 1);
        assert_eq!(app.session.active_plate_name(), "Plate 1");

        app.add_plate(Some("Walls"));
        assert!(app.last_error.is_none());
        assert_eq!(app.session.plate_count(), 2);
        assert_eq!(app.session.active_plate_index(), 1);
        assert_eq!(app.session.active_plate_name(), "Walls");
        assert!(app.session.objects.is_empty());

        app.load_sample(SampleShape::Cylinder);
        assert_eq!(app.session.objects.len(), 1);
        assert_eq!(app.default_gcode_filename(), "Walls.gcode");

        app.rename_plate(1, "Outer Walls");
        assert_eq!(app.session.active_plate_name(), "Outer Walls");
        assert_eq!(app.default_gcode_filename(), "Outer_Walls.gcode");

        app.select_plate(0);
        assert_eq!(app.session.active_plate_index(), 0);
        assert_eq!(app.session.active_plate_name(), "Plate 1");
        assert_eq!(app.session.objects.len(), 1);

        app.remove_plate(1);
        assert_eq!(app.session.plate_count(), 1);
        assert!(app.last_error.is_none());
    }

    #[test]
    fn binary_gcode_toggle_printer_draft_and_upload_dialog_render_cleanly() {
        let mut app = LayerCraftApp::new(Some(SampleShape::Cube20));
        assert_eq!(app.default_gcode_filename(), "layercraft_out.gcode");
        app.session.config.machine.binary_gcode = true;
        assert_eq!(app.default_gcode_filename(), "layercraft_out.bgcode");

        app.run_slice();
        app.printer_draft = PrinterProfile {
            id: "lab-c1".to_owned(),
            name: "Lab CORE One".to_owned(),
            base_url: "192.168.1.40".to_owned(),
            auth: PrinterAuth::ApiKey {
                api_key: "key-123".to_owned(),
            },
            ..PrinterProfile::default()
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            app.open_upload_dialog();
            assert!(app.show_upload_dialog);
            assert!(!app.upload_start_print);
        }

        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            app.render_ui(ui);
        });
        out.textures_delta.clear();
    }

    #[test]
    fn ui_repair_plate_mesh_fixes_defective_stl() {
        let mut tris = layercraft_mesh::build_sample_shape(SampleShape::Cube20)
            .unwrap()
            .triangles;
        tris.pop();
        if let Some(first) = tris.first_mut() {
            *first = first.flipped();
        }
        let mut stl = vec![0u8; 80];
        stl.extend_from_slice(&(tris.len() as u32).to_le_bytes());
        for t in &tris {
            stl.extend_from_slice(&[0u8; 12]);
            for p in [t.a, t.b, t.c] {
                stl.extend_from_slice(&(p.x as f32).to_le_bytes());
                stl.extend_from_slice(&(p.y as f32).to_le_bytes());
                stl.extend_from_slice(&(p.z as f32).to_le_bytes());
            }
            stl.extend_from_slice(&0u16.to_le_bytes());
        }

        let mut app = LayerCraftApp::new(None);
        app.load_mesh_bytes("broken_cube.stl", stl);
        assert!(app.session.plate_inspection.as_ref().unwrap().needs_repair);
        assert!(app.status_message.contains("Topology issues detected"));

        app.repair_plate_mesh(None);
        assert!(app.last_error.is_none());
        assert!(!app.session.plate_inspection.as_ref().unwrap().needs_repair);
        assert!(app.status_message.contains("Repaired mesh"));
    }

    #[test]
    fn ui_prefs_roundtrip_and_recent_files_dedup_cap() {
        let mut p = prefs::AppPreferences::default();
        for idx in 0..15 {
            p.push_recent_file(&format!("/tmp/part_{idx}.stl"));
        }
        assert_eq!(p.recent_files.len(), prefs::MAX_RECENT_FILES);
        assert_eq!(p.recent_files[0], "/tmp/part_14.stl");

        // Re-pushing existing path moves it back to index 0 without duplicating.
        p.push_recent_file("/tmp/part_10.stl");
        assert_eq!(p.recent_files.len(), prefs::MAX_RECENT_FILES);
        assert_eq!(p.recent_files[0], "/tmp/part_10.stl");
        assert_eq!(
            p.recent_files
                .iter()
                .filter(|x| *x == "/tmp/part_10.stl")
                .count(),
            1
        );

        p.theme_mode = Some("light".to_owned());
        p.window_size = Some([1440.0, 900.0]);
        p.active_profiles.machine = Some("LayerCraft CORE One+ HF 0.4mm".to_owned());

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("layercraft_ui_prefs_{unique}.json"));
        p.save_to_path(&path).unwrap();
        let loaded = prefs::AppPreferences::load_from_path(&path).unwrap();
        assert_eq!(loaded, p);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ui_plate_safety_colors_and_dfm_quick_fixes_work() {
        let mut app = LayerCraftApp::new(Some(SampleShape::Cube20));
        assert!(!app.project_dirty);
        let a_id = app.session.objects[0].id;

        // Move off bed -> OOB highlighted red in scene mesh vertices.
        app.transform_object(a_id, [5.0, 125.0], [0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        assert!(app.project_dirty);
        assert_eq!(
            app.session.plate_layout.out_of_bounds_object_ids,
            vec![a_id]
        );
        assert!(!app.scene.mesh_verts.is_empty());
        assert!(app.scene.mesh_verts[0][6] > 0.75);
        assert!(app.scene.mesh_verts[0][7] < 0.40);

        // Add second cube and overlap -> colliding highlighted amber.
        app.transform_object(a_id, [125.0, 125.0], [0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        app.load_sample_with_mode(SampleShape::Cube20, true);
        let b_id = app.session.objects[1].id;
        app.transform_object(b_id, [125.0, 125.0], [0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        assert!(app.session.plate_layout.has_issues);

        app.auto_arrange_plate();
        assert!(!app.session.plate_layout.has_issues);

        // Pre-slice DFM analysis and quick-fix toggles work cleanly.
        app.analyze_printability_action();
        assert!(app.printability_report.is_some());

        app.apply_quick_fix_tree_supports();
        assert!(app.session.config.process.support.enabled);
        assert_eq!(app.session.config.process.support.style, SupportStyle::Tree);

        app.apply_quick_fix_brim();
        assert!(app.session.config.process.brim_width_mm >= 5.0);
    }

    #[test]
    fn ui_custom_preset_save_delete_and_dirty_guard() {
        let mut app = LayerCraftApp::new(Some(SampleShape::Cube20));
        assert!(!app.project_dirty);

        // Built-in cannot be overwritten via save_user_preset.
        assert!(
            app.save_user_preset(ProfileKind::Machine, "LayerCraft CORE One+ HF 0.4mm")
                .is_err()
        );

        let saved = app
            .save_user_preset(ProfileKind::Filament, "UI Custom Filament")
            .unwrap();
        assert_eq!(saved, "UI Custom Filament");
        assert_eq!(
            app.selected_filament_profile.as_deref(),
            Some("UI Custom Filament")
        );

        app.delete_user_preset_action(ProfileKind::Filament, "UI Custom Filament");
        assert!(app.last_error.is_none());
        assert!(app.selected_filament_profile.is_none());

        // Dirty plate intercepts destructive actions cleanly via non-blocking guard.
        app.load_sample_with_mode(SampleShape::Cylinder, true);
        assert!(app.project_dirty);
        app.request_discard_or_run(PendingDiscardAction::LoadSample(SampleShape::Dome));
        assert!(app.pending_discard_action.is_some());
        assert_eq!(app.session.objects.len(), 2);

        if let Some(pending) = app.pending_discard_action.take() {
            app.execute_discard_action(pending);
        }
        assert_eq!(app.session.objects.len(), 1);
    }

    #[test]
    fn ui_object_overrides_and_negative_part_scene_work() {
        let mut app = LayerCraftApp::new(Some(SampleShape::Cube20));
        let obj_id = app.session.objects[0].id;
        app.set_object_overrides(
            obj_id,
            ModifierOverrides {
                wall_loops: Some(4),
                ..ModifierOverrides::default()
            },
        );
        assert!(app.project_dirty);
        assert_eq!(app.session.objects[0].overrides.wall_loops, Some(4));

        app.undo_action();
        assert_eq!(app.session.objects[0].overrides.wall_loops, None);
        app.redo_action();
        assert_eq!(app.session.objects[0].overrides.wall_loops, Some(4));

        app.add_modifier_primitive(
            ModifierPrimitive::Box {
                width_mm: 8.0,
                depth_mm: 8.0,
                height_mm: 10.0,
            },
            ModifierRole::NegativePart,
        );
        assert_eq!(app.session.modifiers.len(), 1);
        assert_eq!(app.session.modifiers[0].role, ModifierRole::NegativePart);
        assert!(!app.scene.modifier_verts.is_empty());
    }

    #[test]
    fn ui_preview_color_modes_and_gcode_viewer_render_cleanly() {
        let mut app = LayerCraftApp::new(Some(SampleShape::Cube20));
        app.run_slice();
        assert_eq!(app.view_mode, ViewMode::Preview);

        let plate = app.session.sliced.as_ref().unwrap().clone();
        for mode in ColorMode::ALL {
            app.color_mode = mode;
            app.rebuild_scene();
            assert!(!app.scene.toolpath_beads.is_empty());
            let (lo, hi) = layercraft_viewport::compute_color_mode_range(
                &plate,
                mode,
                Some(&app.session.config),
            );
            assert!(lo.is_finite() && hi.is_finite() && hi >= lo);
        }

        app.show_gcode_viewer = true;
        assert!(app.gcode_index.total_lines() > 10);
        let r0 = app.gcode_index.layer_line_range(0).unwrap();
        assert!(r0.1 > r0.0);
        let gcode = &app.session.last_gcode.as_ref().unwrap().gcode;
        assert_eq!(
            app.gcode_index.line_str(gcode, r0.0).trim(),
            ";LAYER_CHANGE"
        );
        assert!(app.gcode_index.active_move_line(0, 1, 10).is_some());
    }

    #[test]
    fn ui_home_page_and_recent_projects_workflow() {
        let mut app = LayerCraftApp::new_home();
        assert_eq!(app.view_mode, ViewMode::Home);

        // Seed a recent project entry with an actual rendered 3D plate thumbnail
        let sample_mesh = layercraft_mesh::build_sample_shape(SampleShape::Dome).unwrap();
        let thumb_b64 = recent_projects::render_plate_thumbnail_b64(
            Some(&sample_mesh),
            None,
            &app.session.config,
        );
        assert!(thumb_b64.is_some());

        app.recent_projects
            .upsert(recent_projects::RecentProjectEntry {
                id: "native:/tmp/dome_test.3mf".to_owned(),
                name: "dome_test".to_owned(),
                format: "3mf".to_owned(),
                source: recent_projects::RecentProjectSource::NativePath,
                path: Some("/tmp/dome_test.3mf".to_owned()),
                storage_key: None,
                last_opened_unix_secs: recent_projects::now_unix_secs(),
                object_count: 1,
                plate_count: 1,
                printer_name: Some("Prusa CORE One".to_owned()),
                estimated_time: Some("24m 10s".to_owned()),
                thumbnail_png_b64: thumb_b64,
            });

        let ctx = egui::Context::default();
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            ..Default::default()
        };

        // Render Home in Dark mode & Light mode + search filter
        app.set_theme_mode(ThemeMode::Dark);
        let mut out = ctx.run_ui(raw_input.clone(), |ui| app.render_ui(ui));
        out.textures_delta.clear();
        assert!(
            app.recent_thumb_textures
                .contains_key("native:/tmp/dome_test.3mf")
        );

        app.home_search = "dome".to_owned();
        app.set_theme_mode(ThemeMode::Light);
        let mut out = ctx.run_ui(raw_input.clone(), |ui| app.render_ui(ui));
        out.textures_delta.clear();

        // Transition to Prepare with sample + Settings live search + Preview frame
        app.load_sample(SampleShape::Dome);
        assert_eq!(app.view_mode, ViewMode::Prepare);
        app.sidebar_tab = SidebarTab::Quality;
        app.settings_search = "arachne".to_owned();
        let mut out = ctx.run_ui(raw_input.clone(), |ui| app.render_ui(ui));
        out.textures_delta.clear();

        app.run_slice();
        assert_eq!(app.view_mode, ViewMode::Preview);
        let mut out = ctx.run_ui(raw_input.clone(), |ui| app.render_ui(ui));
        out.textures_delta.clear();

        // Phone-width (412x840) responsive pass: Home, Prepare (collapsed + overlay drawer), Preview
        let phone_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(412.0, 840.0),
            )),
            ..Default::default()
        };
        app.view_mode = ViewMode::Home;
        let mut out = ctx.run_ui(phone_input.clone(), |ui| app.render_ui(ui));
        out.textures_delta.clear();
        assert!(app.narrow_layout_applied);
        assert!(!app.sidebar_open);

        app.view_mode = ViewMode::Prepare;
        let mut out = ctx.run_ui(phone_input.clone(), |ui| app.render_ui(ui));
        out.textures_delta.clear();
        assert!(!app.sidebar_open);

        app.sidebar_open = true;
        let mut out = ctx.run_ui(phone_input.clone(), |ui| app.render_ui(ui));
        out.textures_delta.clear();
        assert!(app.sidebar_open);

        app.sidebar_open = false;
        app.view_mode = ViewMode::Preview;
        let mut out = ctx.run_ui(phone_input, |ui| app.render_ui(ui));
        out.textures_delta.clear();

        let mut out = ctx.run_ui(raw_input, |ui| app.render_ui(ui));
        out.textures_delta.clear();
        assert!(!app.narrow_layout_applied);
    }
}
