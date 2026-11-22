//! Shared desktop and WebAssembly `egui` + `wgpu` slicer application (`LayerCraftApp`).

use std::sync::Arc;

use layercraft_config::{GcodeFlavor, InfillPattern, SeamPosition};
use layercraft_engine::{Session, execute};
use layercraft_mesh::{SampleShape, inspect_mesh};
use layercraft_slice::ExtrusionRole;
use layercraft_viewport::{
    CameraPreset, ColorMode, OrbitCamera, build_bed_grid, build_mesh_vertices, build_toolpath_mesh,
    role_color,
};
use layercraft_viewport_gpu::{GpuSceneSnapshot, ViewportCallback};
use serde_json::Value;

/// Active workspace view mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Prepare,
    Preview,
}

/// Shared application state driving both native `eframe` and browser `WebRunner`.
pub struct LayerCraftApp {
    session: Session,
    camera: OrbitCamera,
    view_mode: ViewMode,
    color_mode: ColorMode,
    max_layer: usize,
    single_layer_only: bool,
    show_ghost_mesh: bool,
    selected_sample: SampleShape,
    #[cfg(not(target_arch = "wasm32"))]
    file_path_input: String,
    #[cfg(not(target_arch = "wasm32"))]
    export_path_input: String,
    status_message: String,
    last_error: Option<String>,
    scene_revision: u64,
    scene: Arc<GpuSceneSnapshot>,
    viewport_aspect: f32,
    #[cfg(not(target_arch = "wasm32"))]
    last_slice_ms: Option<f64>,
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
            selected_sample: initial_sample.unwrap_or(SampleShape::Cube20),
            #[cfg(not(target_arch = "wasm32"))]
            file_path_input: String::new(),
            #[cfg(not(target_arch = "wasm32"))]
            export_path_input: "layercraft_out.gcode".to_owned(),
            status_message: "Drop an .stl or .3mf onto the window, or load a sample part."
                .to_owned(),
            last_error: None,
            scene_revision: 0,
            scene: Arc::new(GpuSceneSnapshot::default()),
            viewport_aspect: 1.5,
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
            serde_json::json!({ "bytes": bytes }),
        ) {
            Ok(_) => {
                self.last_error = None;
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

    /// Load a built-in sample primitive through `plate.add_sample_shape`.
    pub fn load_sample(&mut self, shape: SampleShape) {
        self.selected_sample = shape;
        match execute(
            &mut self.session,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": shape }),
        ) {
            Ok(_) => {
                self.last_error = None;
                self.view_mode = ViewMode::Prepare;
                self.status_message = format!("Loaded sample `{}`.", shape.label());
                self.frame_camera(CameraPreset::Isometric);
                self.rebuild_scene();
            }
            Err(e) => {
                self.last_error = Some(e.to_string());
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
        self.max_layer = layers.saturating_sub(1);
        self.view_mode = ViewMode::Preview;
        self.last_error = None;
        self.status_message = format!("Sliced {layers} layers in {elapsed_ms:.1} ms.");
        self.rebuild_scene();
    }

    fn frame_camera(&mut self, preset: CameraPreset) {
        let bw = self.session.config.machine.bed_width_mm as f32;
        let bd = self.session.config.machine.bed_depth_mm as f32;
        self.camera = match self.session.prepared_mesh.as_ref().and_then(|m| m.aabb()) {
            Some(aabb) => OrbitCamera::framed_on_aabb(preset, aabb, bw, bd),
            None => OrbitCamera::from_preset(preset, bw, bd, 20.0),
        };
    }

    fn rebuild_scene(&mut self) {
        self.scene_revision = self.scene_revision.wrapping_add(1);
        let bed = build_bed_grid(&self.session.config);
        let mesh = self
            .session
            .prepared_mesh
            .as_ref()
            .map(|m| build_mesh_vertices(m, [0.34, 0.65, 0.94, 1.0]))
            .unwrap_or_default();
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
                    Ok(bytes) => self.load_mesh_bytes(&name, bytes),
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
            match file.bytes() {
                Ok(bytes) => self.load_mesh_bytes(&name, bytes),
                Err(_) => {
                    let p = file.path().to_string_lossy().into_owned();
                    self.load_native_path(&p);
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
    fn load_native_path(&mut self, path: &str) {
        match execute(
            &mut self.session,
            "plate.add_mesh_file",
            serde_json::json!({ "path": path }),
        ) {
            Ok(_) => {
                self.last_error = None;
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

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("layercraft_top_bar").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong("LayerCraft");
                ui.separator();

                egui::ComboBox::from_id_salt("sample_combo")
                    .selected_text(self.selected_sample.label())
                    .show_ui(ui, |ui| {
                        for shape in SampleShape::ALL {
                            if ui
                                .selectable_value(&mut self.selected_sample, shape, shape.label())
                                .clicked()
                            {
                                self.load_sample(shape);
                            }
                        }
                    });
                if ui.button("Load Sample").clicked() {
                    self.load_sample(self.selected_sample);
                }

                #[cfg(not(target_arch = "wasm32"))]
                {
                    ui.separator();
                    ui.add(
                        egui::TextEdit::singleline(&mut self.file_path_input)
                            .hint_text("path/to/model.stl or .3mf")
                            .desired_width(170.0),
                    );
                    if ui.button("Open").clicked() && !self.file_path_input.trim().is_empty() {
                        let p = self.file_path_input.trim().to_owned();
                        self.load_native_path(&p);
                    }
                }

                ui.separator();
                ui.selectable_value(&mut self.view_mode, ViewMode::Prepare, "Prepare");
                ui.selectable_value(&mut self.view_mode, ViewMode::Preview, "Preview");

                ui.separator();
                if ui.button("Iso").clicked() {
                    self.frame_camera(CameraPreset::Isometric);
                }
                if ui.button("Top").clicked() {
                    self.frame_camera(CameraPreset::Top);
                }
                if ui.button("Front").clicked() {
                    self.frame_camera(CameraPreset::Front);
                }
                if ui.button("Right").clicked() {
                    self.frame_camera(CameraPreset::Right);
                }

                ui.separator();
                if ui
                    .button(egui::RichText::new("Slice Plate").strong())
                    .clicked()
                {
                    self.run_slice();
                }

                #[cfg(not(target_arch = "wasm32"))]
                if let Some(g) = &self.session.last_gcode {
                    ui.separator();
                    ui.add(
                        egui::TextEdit::singleline(&mut self.export_path_input)
                            .desired_width(140.0),
                    );
                    if ui.button("Export G-code").clicked() {
                        let path = self.export_path_input.trim().to_owned();
                        match std::fs::write(&path, &g.gcode) {
                            Ok(()) => {
                                self.last_error = None;
                                self.status_message = format!("Exported `{path}`.");
                            }
                            Err(e) => {
                                self.last_error = Some(format!("Export failed: {e}"));
                            }
                        }
                    }
                }
            });
        });
    }

    fn left_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("layercraft_settings_panel")
            .default_size(275.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("Process Settings");
                    ui.add_space(4.0);

                    let p = &mut self.session.config.process;
                    egui::Grid::new("process_grid")
                        .num_columns(2)
                        .spacing([8.0, 4.0])
                        .show(ui, |ui| {
                            ui.label("Layer height (mm)");
                            ui.add(
                                egui::DragValue::new(&mut p.layer_height_mm)
                                    .speed(0.01)
                                    .range(0.05..=0.45),
                            );
                            ui.end_row();

                            ui.label("First layer (mm)");
                            ui.add(
                                egui::DragValue::new(&mut p.first_layer_height_mm)
                                    .speed(0.01)
                                    .range(0.08..=0.45),
                            );
                            ui.end_row();

                            ui.label("Wall loops");
                            ui.add(egui::DragValue::new(&mut p.wall_loops).range(0..=12));
                            ui.end_row();

                            ui.label("Top shell layers");
                            ui.add(egui::DragValue::new(&mut p.top_shell_layers).range(0..=20));
                            ui.end_row();

                            ui.label("Bottom shell layers");
                            ui.add(egui::DragValue::new(&mut p.bottom_shell_layers).range(0..=20));
                            ui.end_row();

                            let mut pct = p.sparse_infill_density * 100.0;
                            ui.label("Infill density (%)");
                            if ui
                                .add(egui::DragValue::new(&mut pct).speed(1.0).range(0.0..=100.0))
                                .changed()
                            {
                                p.sparse_infill_density = (pct / 100.0).clamp(0.0, 1.0);
                            }
                            ui.end_row();

                            ui.label("Infill pattern");
                            egui::ComboBox::from_id_salt("infill_pat")
                                .selected_text(match p.sparse_infill_pattern {
                                    InfillPattern::Grid => "Grid",
                                    InfillPattern::Rectilinear => "Rectilinear",
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut p.sparse_infill_pattern,
                                        InfillPattern::Grid,
                                        "Grid",
                                    );
                                    ui.selectable_value(
                                        &mut p.sparse_infill_pattern,
                                        InfillPattern::Rectilinear,
                                        "Rectilinear",
                                    );
                                });
                            ui.end_row();

                            ui.label("Outer wall (mm)");
                            ui.add(
                                egui::DragValue::new(&mut p.outer_wall_width_mm)
                                    .speed(0.01)
                                    .range(0.2..=1.0),
                            );
                            ui.end_row();

                            ui.label("Inner wall (mm)");
                            ui.add(
                                egui::DragValue::new(&mut p.inner_wall_width_mm)
                                    .speed(0.01)
                                    .range(0.2..=1.0),
                            );
                            ui.end_row();

                            ui.label("Skirt loops");
                            ui.add(egui::DragValue::new(&mut p.skirt_loops).range(0..=10));
                            ui.end_row();

                            ui.label("Brim width (mm)");
                            ui.add(
                                egui::DragValue::new(&mut p.brim_width_mm)
                                    .speed(0.2)
                                    .range(0.0..=20.0),
                            );
                            ui.end_row();

                            ui.label("Seam position");
                            egui::ComboBox::from_id_salt("seam_pos")
                                .selected_text(p.seam_position.label())
                                .show_ui(ui, |ui| {
                                    for pos in SeamPosition::ALL {
                                        ui.selectable_value(&mut p.seam_position, pos, pos.label());
                                    }
                                });
                            ui.end_row();
                        });

                    ui.add_space(6.0);
                    ui.heading("Machine & Filament");
                    let m = &mut self.session.config.machine;
                    let f = &mut self.session.config.filament;
                    egui::Grid::new("machine_grid")
                        .num_columns(2)
                        .spacing([8.0, 4.0])
                        .show(ui, |ui| {
                            ui.label("Flavor");
                            egui::ComboBox::from_id_salt("flavor_sel")
                                .selected_text(match m.flavor {
                                    GcodeFlavor::Klipper => "Klipper",
                                    GcodeFlavor::Marlin => "Marlin",
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut m.flavor,
                                        GcodeFlavor::Klipper,
                                        "Klipper",
                                    );
                                    ui.selectable_value(
                                        &mut m.flavor,
                                        GcodeFlavor::Marlin,
                                        "Marlin",
                                    );
                                });
                            ui.end_row();

                            ui.label("Nozzle temp (C)");
                            ui.add(egui::DragValue::new(&mut f.nozzle_temp_c).range(150..=320));
                            ui.end_row();

                            ui.label("Bed temp (C)");
                            ui.add(egui::DragValue::new(&mut f.bed_temp_c).range(0..=120));
                            ui.end_row();
                        });

                    ui.add_space(4.0);
                    egui::CollapsingHeader::new("Cooling & Flow")
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::Grid::new("cooling_flow_grid")
                                .num_columns(2)
                                .spacing([8.0, 4.0])
                                .show(ui, |ui| {
                                    ui.label("Max flow (mm³/s)");
                                    ui.add(
                                        egui::DragValue::new(&mut f.max_volumetric_speed_mm3_s)
                                            .speed(0.2)
                                            .range(0.5..=60.0),
                                    );
                                    ui.end_row();

                                    ui.label("Fan min (%)");
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut f.fan_min_percent)
                                                .range(0..=100),
                                        )
                                        .changed()
                                        && f.fan_min_percent > f.fan_max_percent
                                    {
                                        f.fan_max_percent = f.fan_min_percent;
                                    }
                                    ui.end_row();

                                    ui.label("Fan max (%)");
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut f.fan_max_percent)
                                                .range(0..=100),
                                        )
                                        .changed()
                                        && f.fan_max_percent < f.fan_min_percent
                                    {
                                        f.fan_min_percent = f.fan_max_percent;
                                    }
                                    ui.end_row();

                                    ui.label("Disable fan layers");
                                    ui.add(
                                        egui::DragValue::new(&mut f.disable_fan_first_layers)
                                            .range(0..=20),
                                    );
                                    ui.end_row();

                                    ui.label("Slowdown layer (s)");
                                    ui.add(
                                        egui::DragValue::new(&mut f.slow_down_layer_time_s)
                                            .speed(0.2)
                                            .range(0.0..=60.0),
                                    );
                                    ui.end_row();

                                    ui.label("Min speed (mm/s)");
                                    ui.add(
                                        egui::DragValue::new(&mut f.slow_down_min_speed_mm_s)
                                            .speed(0.5)
                                            .range(1.0..=100.0),
                                    );
                                    ui.end_row();
                                });
                        });

                    egui::CollapsingHeader::new("Retraction & Travel")
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::Grid::new("retract_travel_grid")
                                .num_columns(2)
                                .spacing([8.0, 4.0])
                                .show(ui, |ui| {
                                    ui.label("Retract len (mm)");
                                    ui.add(
                                        egui::DragValue::new(&mut m.retraction_length_mm)
                                            .speed(0.05)
                                            .range(0.0..=15.0),
                                    );
                                    ui.end_row();

                                    ui.label("Z-hop (mm)");
                                    ui.add(
                                        egui::DragValue::new(&mut m.z_hop_mm)
                                            .speed(0.05)
                                            .range(0.0..=5.0),
                                    );
                                    ui.end_row();

                                    ui.label("Wipe on retract");
                                    ui.checkbox(&mut m.wipe_on_retract, "");
                                    ui.end_row();

                                    ui.label("Wipe dist (mm)");
                                    ui.add(
                                        egui::DragValue::new(&mut m.wipe_distance_mm)
                                            .speed(0.1)
                                            .range(0.0..=10.0),
                                    );
                                    ui.end_row();
                                });
                        });

                    if let Some(mesh) = &self.session.prepared_mesh {
                        ui.separator();
                        ui.heading("Mesh Summary");
                        let insp = inspect_mesh(mesh);
                        ui.label(format!("Triangles: {}", insp.triangle_count));
                        ui.label(format!("Volume: {:.1} mm³", insp.volume_mm3));
                        if let Some(bb) = insp.aabb {
                            let s = bb.size();
                            ui.label(format!("Size: {:.1} × {:.1} × {:.1} mm", s.x, s.y, s.z));
                        }
                        ui.label(format!("Non-manifold edges: {}", insp.non_manifold_edges));
                    }

                    if let Some(g) = &self.session.last_gcode {
                        ui.separator();
                        ui.heading("Print Estimates");
                        ui.label(format!("Layers: {}", g.stats.layer_count));
                        ui.label(format!("Height: {:.2} mm", g.stats.max_z_mm));
                        ui.label(format!(
                            "Filament: {:.1} mm ({:.2} cm³, {:.2} g)",
                            g.stats.filament_length_mm,
                            g.stats.filament_volume_mm3 / 1000.0,
                            g.stats.filament_weight_g
                        ));
                        let mins = (g.stats.estimated_time_s / 60.0).floor() as u64;
                        let secs = (g.stats.estimated_time_s % 60.0).round() as u64;
                        ui.label(format!("Est. time: {mins}m {secs:02}s"));

                        ui.add_space(4.0);
                        for (feat, fs) in &g.stats.features {
                            ui.label(format!("{feat}: {:.1} mm³", fs.volume_mm3));
                        }
                    }
                });
            });
    }

    fn right_panel(&mut self, ui: &mut egui::Ui) {
        if self.view_mode != ViewMode::Preview {
            return;
        }
        let Some(sliced) = &self.session.sliced else {
            return;
        };
        let total = sliced.layers.len();
        if total == 0 {
            return;
        }
        if self.max_layer >= total {
            self.max_layer = total - 1;
        }
        let layer_info = sliced
            .layers
            .get(self.max_layer)
            .map(|l| (l.z_top_mm, l.paths.len(), l.fan_percent, l.estimated_time_s));

        egui::Panel::right("layercraft_preview_panel")
            .default_size(195.0)
            .show(ui, |ui| {
                ui.heading("Layer Preview");
                let mut layer_1based = self.max_layer + 1;
                if ui
                    .add(
                        egui::Slider::new(&mut layer_1based, 1..=total)
                            .vertical()
                            .text("Layer"),
                    )
                    .changed()
                {
                    self.max_layer = layer_1based.saturating_sub(1);
                }

                if let Some((z_top_mm, path_count, fan_percent, est_time_s)) = layer_info {
                    ui.label(format!("Z = {z_top_mm:.2} mm"));
                    ui.label(format!("Paths: {path_count}"));
                    ui.label(format!("Fan: {fan_percent}%"));
                    ui.label(format!("Est. layer time: {est_time_s:.1} s"));
                }

                ui.checkbox(&mut self.single_layer_only, "Single layer only");
                ui.checkbox(&mut self.show_ghost_mesh, "Ghost mesh");

                ui.separator();
                ui.label("Color scheme");
                let mut changed = false;
                changed |= ui
                    .radio_value(&mut self.color_mode, ColorMode::Feature, "Feature type")
                    .changed();
                changed |= ui
                    .radio_value(&mut self.color_mode, ColorMode::Speed, "Speed")
                    .changed();
                changed |= ui
                    .radio_value(&mut self.color_mode, ColorMode::Layer, "Layer height")
                    .changed();
                if changed {
                    self.rebuild_scene();
                }

                if self.color_mode == ColorMode::Feature {
                    ui.separator();
                    ui.label("Legend");
                    for role in [
                        ExtrusionRole::OuterWall,
                        ExtrusionRole::InnerWall,
                        ExtrusionRole::TopSurface,
                        ExtrusionRole::BottomSurface,
                        ExtrusionRole::InternalSolidInfill,
                        ExtrusionRole::SparseInfill,
                        ExtrusionRole::Skirt,
                        ExtrusionRole::Brim,
                    ] {
                        let c = role_color(role);
                        let col = egui::Color32::from_rgb(
                            (c[0] * 255.0) as u8,
                            (c[1] * 255.0) as u8,
                            (c[2] * 255.0) as u8,
                        );
                        ui.horizontal(|ui| {
                            let (rect, _) = ui
                                .allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                            ui.painter().rect_filled(rect, 2.0, col);
                            ui.label(role.label());
                        });
                    }
                }
            });
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        egui::Panel::bottom("layercraft_status_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(err) = &self.last_error {
                    ui.colored_label(egui::Color32::from_rgb(245, 105, 95), err);
                } else {
                    ui.label(&self.status_message);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak("Drag: orbit | Shift+Drag / Middle: pan | Wheel: zoom");
                });
            });
        });
    }

    fn viewport_panel(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default()
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
            });
    }
}

impl eframe::App for LayerCraftApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_dropped_files(ui.ctx());
        self.top_bar(ui);
        self.status_bar(ui);
        self.left_panel(ui);
        self.right_panel(ui);
        self.viewport_panel(ui);
    }
}
