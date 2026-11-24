//! Shared desktop and WebAssembly `egui` + `wgpu` slicer application (`LayerCraftApp`).

mod icons;
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
};
use layercraft_engine::{
    HistoryActionResult, ProjectLoadResult, Session, execute, export_project_3mf_bytes,
};
use layercraft_mesh::{ModifierPrimitive, SampleShape};
use layercraft_viewport::{
    CameraPreset, ColorMode, OrbitCamera, ViewportTheme, build_bed_grid_themed,
    build_bed_plate_mesh, build_mesh_vertices, build_toolpath_mesh_with_config, ray_intersect_mesh,
};
use layercraft_viewport_gpu::{GpuSceneSnapshot, ViewportCallback};
use serde_json::Value;

use crate::sidebar::SidebarTab;
use crate::theme::ThemePalette;
pub use theme::ThemeMode;

/// Active workspace view mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Prepare,
    Preview,
}

#[cfg(not(target_arch = "wasm32"))]
struct PendingSliceOutcome {
    job_id: u64,
    elapsed_ms: f64,
    result: Result<(layercraft_slice::SlicedPlate, layercraft_gcode::GcodeOutput), String>,
}

#[cfg(not(target_arch = "wasm32"))]
struct PendingSliceJob {
    rx: mpsc::Receiver<PendingSliceOutcome>,
}

/// Frame callback invoked before UI layout to pump external controllers (e.g. Live MCP bridge).
pub type ExternalPoller = Box<dyn FnMut(&mut LayerCraftApp)>;

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
    pub(crate) sidebar_tab: SidebarTab,
    pub(crate) ribbon_collapsed: bool,
    pub(crate) show_legend_hud: bool,
    pub(crate) last_scene_dark: bool,
    pub(crate) external_poller: Option<ExternalPoller>,
    pub(crate) bridge_endpoint: Option<String>,
    pub(crate) last_mcp_activity: Option<String>,
    pub(crate) pending_config_undo: Option<PrintConfig>,
    pub(crate) drag_undo_active: bool,
    pub(crate) pointer_active: bool,
    pub(crate) plate_rename_buf: (u32, String),
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) last_slice_ms: Option<f64>,
    #[cfg(not(target_arch = "wasm32"))]
    pending_slice: Option<PendingSliceJob>,
    #[cfg(not(target_arch = "wasm32"))]
    next_slice_job_id: u64,
    #[cfg(target_arch = "wasm32")]
    pending_drops: std::rc::Rc<std::cell::RefCell<Vec<(String, Result<Vec<u8>, String>)>>>,
}

impl Default for LayerCraftApp {
    fn default() -> Self {
        Self::new(Some(SampleShape::Cube20))
    }
}

impl LayerCraftApp {
    /// Construct a new UI session, optionally preloading a sample shape.
    pub fn new(initial_sample: Option<SampleShape>) -> Self {
        let mut app = Self {
            session: Session::default(),
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
            selected_machine_profile: None,
            selected_filament_profile: None,
            selected_process_profile: None,
            profile_warning_count: 0,
            status_message: "Drop an .stl or .3mf onto the window, or load a sample part."
                .to_owned(),
            last_error: None,
            scene_revision: 0,
            scene: Arc::new(GpuSceneSnapshot::default()),
            viewport_aspect: 1.5,
            theme_mode: ThemeMode::Dark,
            styles_installed: false,
            sidebar_open: true,
            sidebar_tab: SidebarTab::Plate,
            ribbon_collapsed: false,
            show_legend_hud: true,
            last_scene_dark: true,
            external_poller: None,
            bridge_endpoint: None,
            last_mcp_activity: None,
            pending_config_undo: None,
            drag_undo_active: false,
            pointer_active: false,
            plate_rename_buf: (0, String::new()),
            #[cfg(not(target_arch = "wasm32"))]
            last_slice_ms: None,
            #[cfg(not(target_arch = "wasm32"))]
            pending_slice: None,
            #[cfg(not(target_arch = "wasm32"))]
            next_slice_job_id: 0,
            #[cfg(target_arch = "wasm32")]
            pending_drops: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        };
        if let Some(sample) = initial_sample {
            app.load_sample(sample);
            app.session.clear_history();
        } else {
            app.rebuild_scene();
        }
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

    fn default_gcode_filename(&self) -> String {
        if self.session.plate_count() <= 1 && self.session.active_plate_name() == "Plate 1" {
            return "layercraft_out.gcode".to_owned();
        }
        let mut slug: String = self
            .session
            .active_plate_name()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        while slug.contains("__") {
            slug = slug.replace("__", "_");
        }
        let trimmed = slug.trim_matches('_');
        if trimmed.is_empty() {
            format!("plate_{}.gcode", self.session.active_plate_id())
        } else {
            format!("{trimmed}.gcode")
        }
    }

    /// Load a mesh or LayerCraft `.3mf` project from raw STL or 3MF bytes via `layercraft-engine`.
    pub fn load_mesh_bytes(&mut self, name: &str, bytes: Vec<u8>) {
        self.cancel_pending_slice();
        let is_3mf = name.to_ascii_lowercase().ends_with(".3mf");
        if is_3mf {
            match execute(
                &mut self.session,
                "project.load_3mf_bytes",
                serde_json::json!({ "bytes": bytes, "name": name }),
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
                    self.selected_object_id = self.session.objects.last().map(|o| o.id);
                    self.selected_modifier_id = None;
                    self.view_mode = ViewMode::Prepare;
                    self.max_layer = 0;
                    self.layer_move_step = 0;
                    self.playback_playing = false;
                    self.frame_camera(CameraPreset::Isometric);
                    self.rebuild_scene();
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
            serde_json::json!({ "bytes": bytes, "name": name }),
        ) {
            Ok(_) => {
                self.last_error = None;
                self.selected_object_id = self.session.objects.last().map(|o| o.id);
                self.selected_modifier_id = None;
                self.view_mode = ViewMode::Prepare;
                self.status_message = format!("Loaded `{name}`.");
                self.frame_camera(CameraPreset::Isometric);
                self.rebuild_scene();
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

    pub(crate) fn auto_arrange_plate(&mut self) {
        self.cancel_pending_slice();
        match execute(
            &mut self.session,
            "plate.auto_arrange",
            serde_json::json!({ "spacing_mm": 6.0 }),
        ) {
            Ok(_) => {
                self.last_error = None;
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
                self.view_mode = ViewMode::Prepare;
                self.rebuild_scene();
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    /// Return `true` while a native background slice job is active.
    pub(crate) fn is_slicing(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.pending_slice.is_some()
        }
        #[cfg(target_arch = "wasm32")]
        {
            false
        }
    }

    pub(crate) fn cancel_pending_slice(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.next_slice_job_id = self.next_slice_job_id.wrapping_add(1);
            self.pending_slice = None;
        }
    }

    /// Trigger slicing from interactive UI controls (async background thread on native, sync on WASM).
    pub(crate) fn start_slice_action(&mut self, ctx: &egui::Context) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = ctx;
            self.run_slice();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.pending_slice.is_some() {
                return;
            }
            if self.session.prepared_mesh.is_none() && self.session.mesh.is_none() {
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
                    mesh: m.prepared_mesh.clone(),
                    overrides: m.overrides.clone(),
                })
                .collect();

            let (tx, rx) = mpsc::channel();
            let ctx_clone = ctx.clone();
            let spawned = std::thread::Builder::new()
                .name("layercraft-slice".to_owned())
                .spawn(move || {
                    let t0 = web_time::Instant::now();
                    let res = (|| -> Result<
                        (layercraft_slice::SlicedPlate, layercraft_gcode::GcodeOutput),
                        String,
                    > {
                        let plate = if let Some(prep) = prepared.as_ref() {
                            layercraft_slice::slice_prepared_mesh_with_modifiers(
                                prep, &cfg, &mod_vols,
                            )
                            .map_err(|e| e.to_string())?
                        } else if let Some(r) = raw.as_ref() {
                            layercraft_slice::slice_single_mesh_with_modifiers(r, &cfg, &mod_vols)
                                .map_err(|e| e.to_string())?
                        } else {
                            return Err("no mesh loaded on build plate".to_owned());
                        };
                        let gcode_out =
                            layercraft_gcode::emit_gcode(&plate, &cfg).map_err(|e| e.to_string())?;
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
                    self.pending_slice = Some(PendingSliceJob { rx });
                }
                Err(_) => {
                    self.run_slice();
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
                self.last_slice_ms = Some(out.elapsed_ms);
                self.view_mode = ViewMode::Preview;
                self.last_error = None;
                self.status_message =
                    format!("Sliced {layers} layers in {:.1} ms.", out.elapsed_ms);
                self.rebuild_scene();
                self.set_active_layer(layers.saturating_sub(1));
            }
            Err(err_msg) => {
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
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.last_slice_ms = Some(elapsed_ms);
        }
        let layers = self
            .session
            .sliced
            .as_ref()
            .map(|s| s.layers.len())
            .unwrap_or(0);
        self.view_mode = ViewMode::Preview;
        self.last_error = None;
        self.status_message = format!("Sliced {layers} layers in {elapsed_ms:.1} ms.");
        self.rebuild_scene();
        self.set_active_layer(layers.saturating_sub(1));
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

        let mut mesh = Vec::new();
        if self.session.objects.is_empty() {
            if let Some(m) = &self.session.prepared_mesh {
                mesh.extend(build_mesh_vertices(m, selected_col));
            }
        } else {
            for obj in &self.session.objects {
                let col = if self.selected_object_id == Some(obj.id) {
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
                build_toolpath_mesh_with_config(s, self.color_mode, Some(&self.session.config))
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
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        #[cfg(target_arch = "wasm32")]
        {
            let ready: Vec<(String, Result<Vec<u8>, String>)> =
                std::mem::take(&mut *self.pending_drops.borrow_mut());
            for (name, res) in ready {
                match res {
                    Ok(bytes) => {
                        if name.to_ascii_lowercase().ends_with(".json") {
                            self.load_profile_json_bytes(&name, bytes);
                        } else {
                            self.load_mesh_bytes(&name, bytes);
                        }
                    }
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
                queue.borrow_mut().push((name, res));
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
                    self.selected_object_id = self.session.objects.last().map(|o| o.id);
                    self.selected_modifier_id = None;
                    self.view_mode = ViewMode::Prepare;
                    self.max_layer = 0;
                    self.layer_move_step = 0;
                    self.playback_playing = false;
                    self.frame_camera(CameraPreset::Isometric);
                    self.rebuild_scene();
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
                self.selected_object_id = self.session.objects.last().map(|o| o.id);
                self.selected_modifier_id = None;
                self.view_mode = ViewMode::Prepare;
                self.status_message = format!("Loaded `{path}`.");
                self.frame_camera(CameraPreset::Isometric);
                self.rebuild_scene();
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
                    self.status_message =
                        "Triggered browser download `layercraft_project.3mf`.".to_owned();
                }
                Err(e) => {
                    self.last_error = Some(format!("Download failed: {e}"));
                }
            }
        }
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
        let filename = self.default_gcode_filename();
        let Some(g) = &self.session.last_gcode else {
            return;
        };

        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Export G-code")
                .set_file_name(&filename)
                .add_filter("G-code", &["gcode", "gco", "g"])
                .save_file()
            {
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

        #[cfg(target_arch = "wasm32")]
        {
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

    fn viewport_panel(&mut self, ui: &mut egui::Ui, p: &ThemePalette) {
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
                        ViewMode::Prepare => (true, false, 0..0, 0..0, None),
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
        self.poll_pending_slice(ui.ctx());

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

            if ui.input(|i| i.key_pressed(egui::Key::F)) {
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

        toolbar::draw_header_bar(self, ui, &palette);
        toolbar::draw_ribbon_bar(self, ui, &palette);
        toolbar::draw_plates_bar(self, ui, &palette);
        self.draw_status_bar(ui, &palette);

        let cfg_before_sidebar = self.session.config.clone();
        sidebar::draw_sidebar(self, ui, &palette);
        if self.session.config != cfg_before_sidebar && self.pending_config_undo.is_none() {
            self.pending_config_undo = Some(cfg_before_sidebar);
        }
        if !ui.ctx().egui_is_using_pointer()
            && !ui.ctx().egui_wants_keyboard_input()
            && let Some(prev_cfg) = self.pending_config_undo.take()
            && self.session.config != prev_cfg
        {
            self.session
                .push_config_undo_snapshot("Edit settings", prev_cfg);
        }

        self.viewport_panel(ui, &palette);
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
}
