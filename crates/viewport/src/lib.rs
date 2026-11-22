//! 3D camera, scene mesh/toolpath geometry generation, and headless CPU PNG renderer.

mod render;

use std::collections::HashMap;
use std::ops::Range;

use glam::{Mat4, Vec3, Vec4};
use layercraft_config::PrintConfig;
use layercraft_geom::{Aabb3, Point3, TriangleMesh};
use layercraft_slice::{ExtrusionPath, ExtrusionRole, SlicedPlate};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use render::{RenderMode, RenderRequest, render_png};

/// Viewport lighting & bed-plate palette theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewportTheme {
    #[default]
    Dark,
    Light,
}

/// Errors returned by viewport geometry builders or software PNG rendering.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ViewportError {
    #[error("invalid render dimensions {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("PNG encoding failed: {0}")]
    PngEncode(String),
}

/// Standard camera presets oriented around the build plate (Z-up).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CameraPreset {
    #[default]
    Isometric,
    Top,
    Front,
    Right,
}

/// Toolpath color mapping modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ColorMode {
    #[default]
    Feature,
    Speed,
    Layer,
    LayerHeight,
}

/// Interactive Z-up orbit camera looking at a target point on the build plate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OrbitCamera {
    pub target: [f32; 3],
    pub yaw_rad: f32,
    pub pitch_rad: f32,
    pub distance: f32,
    pub fov_y_rad: f32,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        Self::from_preset(CameraPreset::Isometric, 250.0, 250.0, 20.0)
    }
}

impl OrbitCamera {
    /// Construct a camera from a preset centered on a bed of size `(bed_w, bed_d)` and `part_h`.
    pub fn from_preset(preset: CameraPreset, bed_w: f32, bed_d: f32, part_h: f32) -> Self {
        let target = [0.5 * bed_w, 0.5 * bed_d, (0.4 * part_h).max(5.0)];
        let base_dist = (bed_w.max(bed_d) * 0.65).max(50.0);
        let (yaw_rad, pitch_rad, distance) = match preset {
            CameraPreset::Isometric => (-0.78_f32, 0.62_f32, base_dist),
            CameraPreset::Top => (-std::f32::consts::FRAC_PI_2, 1.54_f32, base_dist * 1.15),
            CameraPreset::Front => (-std::f32::consts::FRAC_PI_2, 0.12_f32, base_dist),
            CameraPreset::Right => (0.0_f32, 0.12_f32, base_dist),
        };
        Self {
            target,
            yaw_rad,
            pitch_rad,
            distance,
            fov_y_rad: 45.0_f32.to_radians(),
        }
    }

    /// Construct a closer camera framed tightly around a part AABB.
    pub fn framed_on_aabb(preset: CameraPreset, aabb: Aabb3, bed_w: f32, bed_d: f32) -> Self {
        let c = aabb.center();
        let sz = aabb.size();
        let diag = ((sz.x * sz.x + sz.y * sz.y + sz.z * sz.z).sqrt() as f32).max(15.0);
        let base = Self::from_preset(preset, bed_w, bed_d, sz.z as f32);
        Self {
            target: [c.x as f32, c.y as f32, c.z as f32],
            distance: (diag * 1.85).max(28.0),
            ..base
        }
    }

    /// Orbit camera around target by pointer delta in pixels.
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw_rad -= dx * 0.008;
        self.pitch_rad = (self.pitch_rad + dy * 0.008).clamp(-1.54, 1.54);
    }

    /// Pan target along the camera's local right and up vectors.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        let t = Vec3::from_array(self.target);
        let eye = self.eye_pos();
        let forward = (t - eye).normalize_or_zero();
        let right = forward.cross(Vec3::Z).normalize_or_zero();
        let up = right.cross(forward).normalize_or_zero();
        let scale = self.distance * 0.0015;
        let next = t - right * (dx * scale) + up * (dy * scale);
        self.target = next.to_array();
    }

    /// Zoom camera distance multiplicatively.
    pub fn zoom(&mut self, scroll_delta: f32) {
        let factor = (-scroll_delta * 0.002).exp();
        self.distance = (self.distance * factor).clamp(8.0, 2000.0);
    }

    /// World-space eye position in millimeters.
    pub fn eye_pos(&self) -> Vec3 {
        let t = Vec3::from_array(self.target);
        let cp = self.pitch_rad.cos();
        let sp = self.pitch_rad.sin();
        let cy = self.yaw_rad.cos();
        let sy = self.yaw_rad.sin();
        t + Vec3::new(cp * cy, cp * sy, sp) * self.distance
    }

    /// World-space eye position as `[f32; 3]`.
    pub fn eye_pos_array(&self) -> [f32; 3] {
        self.eye_pos().to_array()
    }

    /// Right-handed Z-up view matrix.
    pub fn view_matrix(&self) -> Mat4 {
        let eye = self.eye_pos();
        let center = Vec3::from_array(self.target);
        let f = (center - eye).normalize_or_zero();
        let s = f.cross(Vec3::Z).normalize_or_zero();
        let u = s.cross(f);
        Mat4::from_cols(
            Vec4::new(s.x, u.x, -f.x, 0.0),
            Vec4::new(s.y, u.y, -f.y, 0.0),
            Vec4::new(s.z, u.z, -f.z, 0.0),
            Vec4::new(-s.dot(eye), -u.dot(eye), f.dot(eye), 1.0),
        )
    }

    /// Perspective projection matrix targeting `[0, 1]` NDC depth (wgpu convention).
    pub fn proj_matrix(&self, aspect: f32) -> Mat4 {
        let safe_aspect = if aspect.is_finite() && aspect > 0.01 {
            aspect
        } else {
            1.0
        };
        let near = (self.distance * 0.01).clamp(0.1, 2.0);
        let far = (self.distance * 25.0).max(2000.0);
        let f = 1.0 / (0.5 * self.fov_y_rad).tan();
        let nf = 1.0 / (near - far);
        Mat4::from_cols(
            Vec4::new(f / safe_aspect, 0.0, 0.0, 0.0),
            Vec4::new(0.0, f, 0.0, 0.0),
            Vec4::new(0.0, 0.0, far * nf, -1.0),
            Vec4::new(0.0, 0.0, near * far * nf, 0.0),
        )
    }

    /// Combined view-projection matrix.
    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        self.proj_matrix(aspect) * self.view_matrix()
    }
}

/// Lit 3D triangle vertex (position, outward normal, RGBA color).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneVertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub color: [f32; 4],
}

impl SceneVertex {
    /// Pack into a flat `[f32; 10]` layout compatible with `bytemuck::cast_slice` without `unsafe`.
    #[inline]
    pub const fn to_packed(self) -> [f32; 10] {
        [
            self.pos[0],
            self.pos[1],
            self.pos[2],
            self.normal[0],
            self.normal[1],
            self.normal[2],
            self.color[0],
            self.color[1],
            self.color[2],
            self.color[3],
        ]
    }
}

/// Unlit 3D line vertex (position, RGBA color).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineVertex {
    pub pos: [f32; 3],
    pub color: [f32; 4],
}

impl LineVertex {
    /// Pack into `[f32; 7]` without `unsafe`.
    #[inline]
    pub const fn to_packed(self) -> [f32; 7] {
        [
            self.pos[0],
            self.pos[1],
            self.pos[2],
            self.color[0],
            self.color[1],
            self.color[2],
            self.color[3],
        ]
    }
}

/// Return high-contrast RGBA color `[0..1]` for an extrusion feature role.
pub fn role_color(role: ExtrusionRole) -> [f32; 4] {
    match role {
        ExtrusionRole::OuterWall => [0.96, 0.45, 0.16, 1.0],
        ExtrusionRole::InnerWall => [0.95, 0.77, 0.22, 1.0],
        ExtrusionRole::OverhangWall => [0.98, 0.22, 0.62, 1.0],
        ExtrusionRole::TopSurface => [0.92, 0.28, 0.30, 1.0],
        ExtrusionRole::Ironing => [0.84, 0.88, 0.96, 1.0],
        ExtrusionRole::BottomSurface => [0.22, 0.72, 0.88, 1.0],
        ExtrusionRole::InternalSolidInfill => [0.62, 0.36, 0.85, 1.0],
        ExtrusionRole::Bridge => [0.20, 0.56, 0.98, 1.0],
        ExtrusionRole::SparseInfill => [0.26, 0.78, 0.44, 1.0],
        ExtrusionRole::Support => [0.22, 0.80, 0.62, 1.0],
        ExtrusionRole::SupportInterface => [0.12, 0.56, 0.44, 1.0],
        ExtrusionRole::Skirt => [0.35, 0.84, 0.74, 1.0],
        ExtrusionRole::Brim => [0.40, 0.65, 0.96, 1.0],
    }
}

fn lerp_color(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 4] {
    let u = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * u,
        a[1] + (b[1] - a[1]) * u,
        a[2] + (b[2] - a[2]) * u,
        1.0,
    ]
}

/// Compute RGBA color for an extrusion path according to `mode`.
pub fn path_color(
    path: &ExtrusionPath,
    mode: ColorMode,
    layer_idx: usize,
    total_layers: usize,
) -> [f32; 4] {
    path_color_with_height_range(path, mode, layer_idx, total_layers, (0.04, 0.32))
}

fn path_color_with_height_range(
    path: &ExtrusionPath,
    mode: ColorMode,
    layer_idx: usize,
    total_layers: usize,
    height_range_mm: (f64, f64),
) -> [f32; 4] {
    match mode {
        ColorMode::Feature => role_color(path.role),
        ColorMode::Speed => {
            let t = ((path.speed_mm_s as f32) - 15.0) / 110.0;
            lerp_color([0.18, 0.42, 0.94], [0.96, 0.30, 0.18], t)
        }
        ColorMode::Layer => {
            let denom = total_layers.saturating_sub(1).max(1) as f32;
            let t = (layer_idx as f32) / denom;
            lerp_color([0.16, 0.76, 0.78], [0.95, 0.42, 0.24], t)
        }
        ColorMode::LayerHeight => {
            let (min_h, max_h) = height_range_mm;
            let span = (max_h - min_h).max(1e-4);
            let t = ((path.height_mm - min_h) / span) as f32;
            lerp_color([0.16, 0.72, 0.94], [0.95, 0.36, 0.22], t)
        }
    }
}

/// Build 10 mm grid lines, build-plate border, and origin XYZ axis lines using the Dark theme.
pub fn build_bed_grid(config: &PrintConfig) -> Vec<LineVertex> {
    build_bed_grid_themed(config, ViewportTheme::Dark)
}

/// Build theme-matched 10 mm grid lines, build-plate border, front accent, and origin XYZ triad.
pub fn build_bed_grid_themed(config: &PrintConfig, theme: ViewportTheme) -> Vec<LineVertex> {
    let w = config.machine.bed_width_mm as f32;
    let d = config.machine.bed_depth_mm as f32;
    let mut lines = Vec::new();
    let z = 0.04_f32;

    let (minor, major, border, front_col) = match theme {
        ViewportTheme::Dark => (
            [0.22, 0.25, 0.31, 0.70],
            [0.34, 0.40, 0.49, 0.92],
            [0.46, 0.54, 0.66, 1.0],
            [0.08, 0.72, 0.65, 1.0],
        ),
        ViewportTheme::Light => (
            [0.73, 0.77, 0.83, 0.80],
            [0.55, 0.61, 0.69, 0.95],
            [0.38, 0.44, 0.53, 1.0],
            [0.05, 0.58, 0.53, 1.0],
        ),
    };

    let steps_x = (w / 10.0).floor() as i32;
    for ix in 1..steps_x {
        let x = (ix as f32) * 10.0;
        let col = if ix % 5 == 0 { major } else { minor };
        lines.push(LineVertex {
            pos: [x, 0.0, z],
            color: col,
        });
        lines.push(LineVertex {
            pos: [x, d, z],
            color: col,
        });
    }

    let steps_y = (d / 10.0).floor() as i32;
    for iy in 1..steps_y {
        let y = (iy as f32) * 10.0;
        let col = if iy % 5 == 0 { major } else { minor };
        lines.push(LineVertex {
            pos: [0.0, y, z],
            color: col,
        });
        lines.push(LineVertex {
            pos: [w, y, z],
            color: col,
        });
    }

    for (a, b, col) in [
        ([0.0, 0.0, z], [w, 0.0, z], front_col),
        ([w, 0.0, z], [w, d, z], border),
        ([w, d, z], [0.0, d, z], border),
        ([0.0, d, z], [0.0, 0.0, z], border),
    ] {
        lines.push(LineVertex { pos: a, color: col });
        lines.push(LineVertex { pos: b, color: col });
    }

    // Origin triad (X=red, Y=green, Z=blue) raised slightly above grid lines to prevent Z fighting.
    let ax = 26.0_f32;
    let z_ax = 0.08_f32;
    for (end, col) in [
        ([ax, 0.0, z_ax], [0.95, 0.25, 0.25, 1.0]),
        ([0.0, ax, z_ax], [0.25, 0.90, 0.35, 1.0]),
        ([0.0, 0.0, ax], [0.28, 0.55, 0.98, 1.0]),
    ] {
        lines.push(LineVertex {
            pos: [0.0, 0.0, z_ax],
            color: col,
        });
        lines.push(LineVertex {
            pos: end,
            color: col,
        });
    }

    lines
}

/// Build a 3.2 mm thick matte build-plate slab with a teal front-edge indicator.
pub fn build_bed_plate_mesh(config: &PrintConfig, theme: ViewportTheme) -> Vec<SceneVertex> {
    let w = config.machine.bed_width_mm as f32;
    let d = config.machine.bed_depth_mm as f32;
    let z_top = -0.02_f32;
    let z_bot = -3.2_f32;

    let (top_col, bevel_col, front_accent) = match theme {
        ViewportTheme::Dark => (
            [0.145, 0.165, 0.200, 1.0],
            [0.10, 0.115, 0.14, 1.0],
            [0.08, 0.72, 0.65, 1.0],
        ),
        ViewportTheme::Light => (
            [0.875, 0.895, 0.925, 1.0],
            [0.75, 0.78, 0.83, 1.0],
            [0.05, 0.58, 0.53, 1.0],
        ),
    };

    let mut verts = Vec::with_capacity(42);
    // Top face
    push_quad(
        &mut verts,
        [0.0, 0.0, z_top],
        [w, 0.0, z_top],
        [w, d, z_top],
        [0.0, d, z_top],
        [0.0, 0.0, 1.0],
        top_col,
    );
    // Bottom face
    push_quad(
        &mut verts,
        [0.0, d, z_bot],
        [w, d, z_bot],
        [w, 0.0, z_bot],
        [0.0, 0.0, z_bot],
        [0.0, 0.0, -1.0],
        bevel_col,
    );
    // 4 vertical perimeter bevel walls
    push_quad(
        &mut verts,
        [0.0, 0.0, z_bot],
        [w, 0.0, z_bot],
        [w, 0.0, z_top],
        [0.0, 0.0, z_top],
        [0.0, -1.0, 0.0],
        bevel_col,
    );
    push_quad(
        &mut verts,
        [w, d, z_bot],
        [0.0, d, z_bot],
        [0.0, d, z_top],
        [w, d, z_top],
        [0.0, 1.0, 0.0],
        bevel_col,
    );
    push_quad(
        &mut verts,
        [0.0, d, z_bot],
        [0.0, 0.0, z_bot],
        [0.0, 0.0, z_top],
        [0.0, d, z_top],
        [-1.0, 0.0, 0.0],
        bevel_col,
    );
    push_quad(
        &mut verts,
        [w, 0.0, z_bot],
        [w, d, z_bot],
        [w, d, z_top],
        [w, 0.0, z_top],
        [1.0, 0.0, 0.0],
        bevel_col,
    );
    // Front-edge accent bar
    push_quad(
        &mut verts,
        [w * 0.20, -0.05, -2.0],
        [w * 0.80, -0.05, -2.0],
        [w * 0.80, -0.05, -0.35],
        [w * 0.20, -0.05, -0.35],
        [0.0, -1.0, 0.0],
        front_accent,
    );

    verts
}

fn quantize_pos(p: Point3) -> [i64; 3] {
    [
        (p.x * 10_000.0).round() as i64,
        (p.y * 10_000.0).round() as i64,
        (p.z * 10_000.0).round() as i64,
    ]
}

/// Convert a prepared `TriangleMesh` into lit triangles with crease-angle (`38°`) smooth vertex normals.
pub fn build_mesh_vertices(mesh: &TriangleMesh, color: [f32; 4]) -> Vec<SceneVertex> {
    const CREASE_COS_THRESHOLD: f32 = 0.788; // ~cos(38 deg)

    let mut incident: HashMap<[i64; 3], Vec<Vec3>> = HashMap::new();
    let mut face_normals = Vec::with_capacity(mesh.triangles.len());

    for tri in &mesh.triangles {
        let n_d = tri.double_area_vec().normalize_or_zero();
        let fn_vec = Vec3::new(n_d.x as f32, n_d.y as f32, n_d.z as f32).normalize_or_zero();
        face_normals.push(fn_vec);
        for p in [tri.a, tri.b, tri.c] {
            incident.entry(quantize_pos(p)).or_default().push(fn_vec);
        }
    }

    let mut verts = Vec::with_capacity(mesh.triangles.len() * 3);
    for (tri, face_n) in mesh.triangles.iter().zip(face_normals.iter().copied()) {
        for p in [tri.a, tri.b, tri.c] {
            let mut sum = Vec3::ZERO;
            if let Some(list) = incident.get(&quantize_pos(p)) {
                for &cand in list {
                    if cand.dot(face_n) >= CREASE_COS_THRESHOLD {
                        sum += cand;
                    }
                }
            }
            let smooth = if sum.length_squared() > 1e-8 {
                sum.normalize()
            } else {
                face_n
            };
            verts.push(SceneVertex {
                pos: [p.x as f32, p.y as f32, p.z as f32],
                normal: smooth.to_array(),
                color,
            });
        }
    }
    verts
}

/// 3D extruded bead ribbon geometry indexed by layer range for O(1) layer scrubbing.
#[derive(Debug, Clone, Default)]
pub struct ToolpathGeometry {
    pub vertices: Vec<SceneVertex>,
    pub layer_ranges: Vec<Range<u32>>,
}

/// Build 3D extruded bead ribbons (top cap + two side walls) for every layer in `plate`.
pub fn build_toolpath_mesh(plate: &SlicedPlate, mode: ColorMode) -> ToolpathGeometry {
    let total_layers = plate.layers.len();
    let height_range_mm = if mode == ColorMode::LayerHeight && !plate.layers.is_empty() {
        let mut min_h = f64::INFINITY;
        let mut max_h = 0.0_f64;
        for l in &plate.layers {
            min_h = min_h.min(l.height_mm);
            max_h = max_h.max(l.height_mm);
        }
        if max_h - min_h > 1e-4 {
            (min_h, max_h)
        } else {
            (0.04, 0.32)
        }
    } else {
        (0.04, 0.32)
    };
    let mut vertices = Vec::new();
    let mut layer_ranges = Vec::with_capacity(total_layers);

    for (layer_idx, layer) in plate.layers.iter().enumerate() {
        let start = vertices.len() as u32;
        let z_top = layer.z_top_mm as f32;
        let z_bot = (layer.z_top_mm - layer.height_mm) as f32;

        for path in &layer.paths {
            let n = path.points.len();
            if n < 2 {
                continue;
            }
            let color =
                path_color_with_height_range(path, mode, layer_idx, total_layers, height_range_mm);
            let seg_count = if path.closed { n } else { n - 1 };

            for i in 0..seg_count {
                let Some(&pa) = path.points.get(i) else {
                    continue;
                };
                let next_idx = if path.closed {
                    (i + 1) % n
                } else {
                    (i + 1).min(n - 1)
                };
                let Some(&pb) = path.points.get(next_idx) else {
                    continue;
                };
                let hwa = (0.5 * path.vertex_width_mm(i)) as f32;
                let hwb = (0.5 * path.vertex_width_mm(next_idx)) as f32;
                let (ax, ay) = pa.to_mm();
                let (bx, by) = pb.to_mm();
                let ax = ax as f32;
                let ay = ay as f32;
                let bx = bx as f32;
                let by = by as f32;
                let dx = bx - ax;
                let dy = by - ay;
                let len = dx.hypot(dy);
                if len < 1e-4 {
                    continue;
                }
                let nx = -dy / len;
                let ny = dx / len;
                let oxa = nx * hwa;
                let oya = ny * hwa;
                let oxb = nx * hwb;
                let oyb = ny * hwb;

                let al_t = [ax + oxa, ay + oya, z_top];
                let ar_t = [ax - oxa, ay - oya, z_top];
                let bl_t = [bx + oxb, by + oyb, z_top];
                let br_t = [bx - oxb, by - oyb, z_top];

                let al_b = [ax + oxa, ay + oya, z_bot];
                let ar_b = [ax - oxa, ay - oya, z_bot];
                let bl_b = [bx + oxb, by + oyb, z_bot];
                let br_b = [bx - oxb, by - oyb, z_bot];

                // Top face (+Z)
                push_tri(&mut vertices, ar_t, br_t, bl_t, [0.0, 0.0, 1.0], color);
                push_tri(&mut vertices, ar_t, bl_t, al_t, [0.0, 0.0, 1.0], color);

                // Left side wall (+n)
                push_tri(&mut vertices, al_b, bl_b, bl_t, [nx, ny, 0.0], color);
                push_tri(&mut vertices, al_b, bl_t, al_t, [nx, ny, 0.0], color);

                // Right side wall (-n)
                push_tri(&mut vertices, br_b, ar_b, ar_t, [-nx, -ny, 0.0], color);
                push_tri(&mut vertices, br_b, ar_t, br_t, [-nx, -ny, 0.0], color);
            }
        }
        let end = vertices.len() as u32;
        layer_ranges.push(start..end);
    }

    ToolpathGeometry {
        vertices,
        layer_ranges,
    }
}

fn push_tri(
    out: &mut Vec<SceneVertex>,
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    normal: [f32; 3],
    color: [f32; 4],
) {
    out.push(SceneVertex {
        pos: a,
        normal,
        color,
    });
    out.push(SceneVertex {
        pos: b,
        normal,
        color,
    });
    out.push(SceneVertex {
        pos: c,
        normal,
        color,
    });
}

fn push_quad(
    out: &mut Vec<SceneVertex>,
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    d: [f32; 3],
    normal: [f32; 3],
    color: [f32; 4],
) {
    push_tri(out, a, b, c, normal, color);
    push_tri(out, a, c, d, normal, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use layercraft_geom::{Point3, Triangle};
    use layercraft_slice::slice_single_mesh;

    fn sample_cube() -> TriangleMesh {
        let s = 20.0;
        let tris = [
            ((0.0, 0.0, 0.0), (0.0, s, 0.0), (s, s, 0.0)),
            ((0.0, 0.0, 0.0), (s, s, 0.0), (s, 0.0, 0.0)),
            ((0.0, 0.0, s), (s, 0.0, s), (s, s, s)),
            ((0.0, 0.0, s), (s, s, s), (0.0, s, s)),
            ((0.0, 0.0, 0.0), (s, 0.0, 0.0), (s, 0.0, s)),
            ((0.0, 0.0, 0.0), (s, 0.0, s), (0.0, 0.0, s)),
            ((0.0, s, 0.0), (0.0, s, s), (s, s, s)),
            ((0.0, s, 0.0), (s, s, s), (s, s, 0.0)),
            ((0.0, 0.0, 0.0), (0.0, 0.0, s), (0.0, s, s)),
            ((0.0, 0.0, 0.0), (0.0, s, s), (0.0, s, 0.0)),
            ((s, 0.0, 0.0), (s, s, 0.0), (s, s, s)),
            ((s, 0.0, 0.0), (s, s, s), (s, 0.0, s)),
        ]
        .into_iter()
        .map(|(a, b, c)| {
            Triangle::new(
                Point3::new(a.0, a.1, a.2),
                Point3::new(b.0, b.1, b.2),
                Point3::new(c.0, c.1, c.2),
            )
        })
        .collect();
        let mut m = TriangleMesh::new(tris).unwrap();
        m.ground_and_center(125.0, 125.0).unwrap();
        m
    }

    #[test]
    fn renders_valid_non_blank_pngs_across_all_modes() {
        let cfg = PrintConfig::default();
        let mesh = sample_cube();
        let plate = slice_single_mesh(&mesh, &cfg).unwrap();

        for mode in [
            RenderMode::Plate,
            RenderMode::Preview,
            RenderMode::LayerTopDown,
        ] {
            let png = render_png(
                Some(&mesh),
                Some(&plate),
                &cfg,
                &RenderRequest {
                    width: 320,
                    height: 240,
                    mode,
                    camera_preset: CameraPreset::Isometric,
                    max_layer: Some(50),
                    single_layer_only: false,
                    color_mode: ColorMode::Feature,
                },
            )
            .unwrap();
            assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
            assert!(png.len() > 1200, "unexpectedly tiny png for {mode:?}");
        }
    }

    #[test]
    fn bed_plate_and_themed_grid_build_for_dark_and_light() {
        let cfg = PrintConfig::default();
        for theme in [ViewportTheme::Dark, ViewportTheme::Light] {
            let plate = build_bed_plate_mesh(&cfg, theme);
            let grid = build_bed_grid_themed(&cfg, theme);
            assert_eq!(plate.len(), 42);
            assert!(grid.len() > 40);
        }
        let cube = sample_cube();
        let verts = build_mesh_vertices(&cube, [0.5, 0.6, 0.7, 1.0]);
        assert_eq!(verts.len(), 36);
        // 90-degree cube corners stay sharp (unit axis-aligned normals).
        for v in &verts {
            let max_comp = v.normal.iter().map(|c| c.abs()).fold(0.0_f32, f32::max);
            assert!(
                (max_comp - 1.0).abs() < 1e-4,
                "unexpected bevel {:?}",
                v.normal
            );
        }
    }
}
