//! Shared desktop and WebAssembly `egui` + `wgpu` slicer application (`LayerCraftApp`).

mod icons;
mod sidebar;
mod theme;
mod toolbar;
mod ui_kit;
mod viewport_hud;

use std::sync::Arc;

use egui::{Align, Margin, RichText, Stroke, pos2};
use layercraft_config::{
    ModifierOverrides, ModifierRole, ProfileApplyResult, ProfileKind, ProfileSummary,
};
use layercraft_engine::{Session, execute};
use layercraft_mesh::{ModifierPrimitive, SampleShape};
use layercraft_viewport::{
    CameraPreset, ColorMode, OrbitCamera, build_bed_grid, build_mesh_vertices, build_toolpath_mesh,
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
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) last_slice_ms: Option<f64>,
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
            #[cfg(not(target_arch = "wasm32"))]
            last_slice_ms: None,
            #[cfg(target_arch = "wasm32")]
            pending_drops: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        };
        if let Some(sample) = initial_sample {
            app.load_sample(sample);
        } else {
            app.rebuild_scene();
        }
        app
    }

    /// Set the active color theme mode (`Dark`, `Light`, or `System`).
    pub fn set_theme_mode(&mut self, mode: ThemeMode) {
        self.theme_mode = mode;
    }

    /// Load a mesh from raw STL or 3MF bytes via `layercraft-engine` commands.
    pub fn load_mesh_bytes(&mut self, name: &str, bytes: Vec<u8>) {
        let is_3mf = name.to_ascii_lowercase().ends_with(".3mf");
        let cmd = if is_3mf {
            "plate.add_3mf_bytes"
        } else {
            "plate.add_stl_bytes"
        };
        match execute(
            &mut self.session,
            cmd,
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
                self.last_error = None;
                self.view_mode = ViewMode::Prepare;
                self.rebuild_scene();
            }
            Err(e) => self.last_error = Some(e.to_string()),
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
        self.max_layer = layers.saturating_sub(1);
        self.view_mode = ViewMode::Preview;
        self.last_error = None;
        self.status_message = format!("Sliced {layers} layers in {elapsed_ms:.1} ms.");
        self.rebuild_scene();
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
        let bed = if self.show_bed_grid {
            build_bed_grid(&self.session.config)
        } else {
            Vec::new()
        };
        let mut mesh = self
            .session
            .prepared_mesh
            .as_ref()
            .map(|m| build_mesh_vertices(m, [0.34, 0.65, 0.94, 1.0]))
            .unwrap_or_default();
        for m in &self.session.modifiers {
            let col = match m.role {
                ModifierRole::Modifier => [0.95, 0.72, 0.18, 0.35],
                ModifierRole::SupportBlocker => [0.92, 0.26, 0.26, 0.35],
                ModifierRole::SupportEnforcer => [0.20, 0.85, 0.45, 0.35],
            };
            mesh.extend(build_mesh_vertices(&m.prepared_mesh, col));
        }
        let ghost = self
            .session
            .prepared_mesh
            .as_ref()
            .map(|m| build_mesh_vertices(m, [0.65, 0.72, 0.82, 0.24]))
            .unwrap_or_default();

        let tp = self
            .session
            .sliced
            .as_ref()
            .map(|s| build_toolpath_mesh(s, self.color_mode))
            .unwrap_or_default();

        self.scene = Arc::new(GpuSceneSnapshot::from_geometry(
            self.scene_revision,
            &bed,
            &mesh,
            &ghost,
            &tp.vertices,
            tp.layer_ranges,
        ));
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
        let Some(g) = &self.session.last_gcode else {
            return;
        };

        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Export G-code")
                .set_file_name("layercraft_out.gcode")
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
            match trigger_browser_download("layercraft_out.gcode", &g.gcode) {
                Ok(()) => {
                    self.last_error = None;
                    self.status_message =
                        "Triggered browser download `layercraft_out.gcode`.".to_owned();
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

                let (draw_mesh, draw_ghost, toolpath_range) = match self.view_mode {
                    ViewMode::Prepare => (true, false, 0..0),
                    ViewMode::Preview => {
                        let n = self.scene.layer_ranges.len();
                        if n == 0 {
                            (true, false, 0..0)
                        } else {
                            let top = self.max_layer.min(n - 1);
                            let start = if self.single_layer_only { top } else { 0 };
                            let v_start = self
                                .scene
                                .layer_ranges
                                .get(start)
                                .map(|r| r.start)
                                .unwrap_or(0);
                            let v_end =
                                self.scene.layer_ranges.get(top).map(|r| r.end).unwrap_or(0);
                            (false, self.show_ghost_mesh, v_start..v_end)
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
                        viewport_size_px,
                        draw_mesh,
                        draw_ghost,
                        toolpath_range,
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
        if !self.styles_installed {
            theme::install_styles(ui.ctx());
            self.styles_installed = true;
        }
        ui.ctx().set_theme(self.theme_mode.to_preference());
        let palette = theme::active_palette(ui.ctx());

        self.handle_dropped_files(ui.ctx());

        if !ui.ctx().egui_wants_keyboard_input() && ui.input(|i| i.key_pressed(egui::Key::F)) {
            self.frame_camera(CameraPreset::Isometric);
        }

        toolbar::draw_header_bar(self, ui, &palette);
        toolbar::draw_ribbon_bar(self, ui, &palette);
        self.draw_status_bar(ui, &palette);
        sidebar::draw_sidebar(self, ui, &palette);
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
    use wasm_bindgen::JsCast as _;

    let win = web_sys::window().ok_or_else(|| "missing browser window".to_owned())?;
    let doc = win
        .document()
        .ok_or_else(|| "missing browser document".to_owned())?;
    let body = doc
        .body()
        .ok_or_else(|| "missing document body".to_owned())?;

    let bag = web_sys::BlobPropertyBag::new();
    bag.set_type("text/plain;charset=utf-8");
    let parts = js_sys::Array::of1(&wasm_bindgen::JsValue::from_str(contents));
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &bag)
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
}
