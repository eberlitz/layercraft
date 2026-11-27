//! 3D camera, scene mesh/toolpath geometry generation, and headless CPU PNG renderer.

mod render;

use std::collections::HashMap;
use std::ops::Range;

use glam::{Mat4, Vec3, Vec4};
use layercraft_config::PrintConfig;
use layercraft_gcode::combing::CombingLayer;
use layercraft_geom::{Aabb3, Point3, TriangleMesh};
use layercraft_slice::{ExtrusionPath, ExtrusionRole, SlicedPlate, bead_area_mm2};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use render::{
    PngDiffResult, RenderMode, RenderRequest, compare_png, render_png, render_png_with_camera,
};

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
    #[error("PNG decoding failed: {0}")]
    PngDecode(String),
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
    VolumetricFlow,
    FanSpeed,
    LineWidth,
    Layer,
    LayerHeight,
}

impl ColorMode {
    /// All preview color modes in Studio ribbon display order.
    pub const ALL: [Self; 7] = [
        Self::Feature,
        Self::Speed,
        Self::VolumetricFlow,
        Self::FanSpeed,
        Self::LineWidth,
        Self::LayerHeight,
        Self::Layer,
    ];

    /// Short human-readable label for ribbon buttons and HUD.
    pub fn label(self) -> &'static str {
        match self {
            Self::Feature => "Feature",
            Self::Speed => "Speed",
            Self::VolumetricFlow => "Flow",
            Self::FanSpeed => "Fan",
            Self::LineWidth => "Width",
            Self::LayerHeight => "Layer Height",
            Self::Layer => "Layer Z",
        }
    }
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

    /// Scale camera distance multiplicatively (`< 1.0` zooms in, `> 1.0` zooms out).
    pub fn zoom_by_factor(&mut self, distance_scale: f32) {
        if distance_scale.is_finite() && distance_scale > 0.0 {
            self.distance = (self.distance * distance_scale).clamp(8.0, 2000.0);
        }
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

    /// Construct a world-space picking ray through NDC coordinates `(-1..=1, -1..=1)` (+Y up).
    pub fn screen_ray(&self, ndc_x: f32, ndc_y: f32, aspect: f32) -> Ray3 {
        let inv = self.view_proj(aspect).inverse();
        let unproject = |ndc_z: f32| -> Option<Vec3> {
            let v = inv * Vec4::new(ndc_x, ndc_y, ndc_z, 1.0);
            if v.w.abs() > 1e-7 && v.is_finite() {
                Some(v.truncate() / v.w)
            } else {
                None
            }
        };
        let origin_v = self.eye_pos();
        let dir_v = match (unproject(0.0), unproject(1.0)) {
            (Some(near_p), Some(far_p)) => (far_p - near_p).normalize_or_zero(),
            _ => (Vec3::from_array(self.target) - origin_v).normalize_or_zero(),
        };
        Ray3 {
            origin: origin_v.to_array(),
            dir: dir_v.to_array(),
        }
    }
}

/// 3D picking ray in world-space millimeters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ray3 {
    pub origin: [f32; 3],
    pub dir: [f32; 3],
}

/// Intersect `ray` against an axis-aligned bounding box, returning entry distance `t >= 0` on hit.
pub fn ray_intersect_aabb(ray: Ray3, aabb: Aabb3) -> Option<f32> {
    let [ox, oy, oz] = ray.origin;
    let [dx, dy, dz] = ray.dir;
    let mut tmin = 0.0_f64;
    let mut tmax = f64::INFINITY;

    for (o, d, min_b, max_b) in [
        (f64::from(ox), f64::from(dx), aabb.min.x, aabb.max.x),
        (f64::from(oy), f64::from(dy), aabb.min.y, aabb.max.y),
        (f64::from(oz), f64::from(dz), aabb.min.z, aabb.max.z),
    ] {
        if d.abs() < 1e-8 {
            if o < min_b || o > max_b {
                return None;
            }
        } else {
            let inv_d = 1.0 / d;
            let mut t1 = (min_b - o) * inv_d;
            let mut t2 = (max_b - o) * inv_d;
            if t1 > t2 {
                std::mem::swap(&mut t1, &mut t2);
            }
            tmin = tmin.max(t1);
            tmax = tmax.min(t2);
            if tmax < tmin {
                return None;
            }
        }
    }

    (tmax >= tmin.max(0.0)).then_some(tmin.max(0.0) as f32)
}

/// Exact double-sided Möller–Trumbore ray-triangle picking with AABB pre-check.
pub fn ray_intersect_mesh(ray: Ray3, mesh: &TriangleMesh) -> Option<f32> {
    let aabb = mesh.aabb()?;
    let _ = ray_intersect_aabb(ray, aabb)?;

    let [ox, oy, oz] = ray.origin;
    let [dx, dy, dz] = ray.dir;
    let orig = glam::DVec3::new(f64::from(ox), f64::from(oy), f64::from(oz));
    let dir = glam::DVec3::new(f64::from(dx), f64::from(dy), f64::from(dz));
    let mut best_t: Option<f32> = None;

    for tri in &mesh.triangles {
        let a = tri.a.to_vec3();
        let e1 = tri.b.to_vec3() - a;
        let e2 = tri.c.to_vec3() - a;
        let pvec = dir.cross(e2);
        let det = e1.dot(pvec);
        if det.abs() <= 1e-9 {
            continue;
        }
        let inv_det = 1.0 / det;
        let tvec = orig - a;
        let u = tvec.dot(pvec) * inv_det;
        if !(0.0..=1.0).contains(&u) {
            continue;
        }
        let qvec = tvec.cross(e1);
        let v = dir.dot(qvec) * inv_det;
        if v < 0.0 || u + v > 1.0 {
            continue;
        }
        let t = e2.dot(qvec) * inv_det;
        if t > 1e-4 {
            let tf = t as f32;
            best_t = Some(best_t.map_or(tf, |prev| prev.min(tf)));
        }
    }

    best_t
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
        ExtrusionRole::GapFill => [0.98, 0.90, 0.25, 1.0],
        ExtrusionRole::TopSurface => [0.92, 0.28, 0.30, 1.0],
        ExtrusionRole::Ironing => [0.84, 0.88, 0.96, 1.0],
        ExtrusionRole::BottomSurface => [0.22, 0.72, 0.88, 1.0],
        ExtrusionRole::InternalSolidInfill => [0.62, 0.36, 0.85, 1.0],
        ExtrusionRole::Bridge => [0.20, 0.56, 0.98, 1.0],
        ExtrusionRole::InternalBridge => [0.18, 0.72, 0.92, 1.0],
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

fn lerp3_color(a: [f32; 3], mid: [f32; 3], b: [f32; 3], t: f32) -> [f32; 4] {
    let u = t.clamp(0.0, 1.0);
    if u <= 0.5 {
        lerp_color(a, mid, u * 2.0)
    } else {
        lerp_color(mid, b, (u - 0.5) * 2.0)
    }
}

/// Sample the normalized `[0..=1]` RGBA gradient ramp used for `mode` in both 3D toolpaths and HUD legends.
pub fn color_ramp_sample(mode: ColorMode, t: f32) -> [f32; 4] {
    match mode {
        ColorMode::Feature | ColorMode::Layer | ColorMode::LayerHeight => {
            lerp_color([0.16, 0.74, 0.86], [0.95, 0.38, 0.22], t)
        }
        ColorMode::Speed => lerp3_color(
            [0.18, 0.42, 0.94],
            [0.16, 0.78, 0.56],
            [0.96, 0.30, 0.18],
            t,
        ),
        ColorMode::VolumetricFlow => lerp3_color(
            [0.20, 0.36, 0.92],
            [0.96, 0.72, 0.16],
            [0.95, 0.18, 0.16],
            t,
        ),
        ColorMode::FanSpeed => lerp3_color(
            [0.42, 0.44, 0.50],
            [0.12, 0.68, 0.76],
            [0.14, 0.88, 1.00],
            t,
        ),
        ColorMode::LineWidth => lerp3_color(
            [0.32, 0.38, 0.92],
            [0.14, 0.80, 0.70],
            [0.98, 0.54, 0.14],
            t,
        ),
    }
}

/// Compute numeric `(min, max)` legend bounds for a preview [`ColorMode`] over `plate`.
pub fn compute_color_mode_range(
    plate: &SlicedPlate,
    mode: ColorMode,
    config: Option<&PrintConfig>,
) -> (f64, f64) {
    match mode {
        ColorMode::Feature => (0.0, 1.0),
        ColorMode::FanSpeed => (0.0, 100.0),
        ColorMode::Layer => {
            let max_z = plate
                .layers
                .iter()
                .map(|l| l.z_top_mm)
                .fold(0.0_f64, f64::max)
                .max(1.0);
            (0.0, max_z)
        }
        ColorMode::LayerHeight => {
            let mut min_h = f64::INFINITY;
            let mut max_h = 0.0_f64;
            for l in &plate.layers {
                min_h = min_h.min(l.height_mm);
                max_h = max_h.max(l.height_mm);
            }
            if min_h.is_finite() && max_h - min_h > 1e-4 {
                (min_h, max_h)
            } else {
                (0.04, 0.32)
            }
        }
        ColorMode::Speed => {
            let mut min_s = f64::INFINITY;
            let mut max_s = 0.0_f64;
            for l in &plate.layers {
                for p in &l.paths {
                    min_s = min_s.min(p.speed_mm_s);
                    max_s = max_s.max(p.speed_mm_s);
                }
            }
            if min_s.is_finite() && max_s - min_s > 1e-4 {
                (min_s, max_s)
            } else {
                (15.0, 125.0)
            }
        }
        ColorMode::VolumetricFlow => {
            let fil_flow = config
                .map(|c| c.filament.flow_ratio.max(0.001))
                .unwrap_or(1.0);
            let mut max_q = 0.0_f64;
            for l in &plate.layers {
                for p in &l.paths {
                    let q = bead_area_mm2(p.max_width_mm(), p.height_mm)
                        * p.flow_ratio.max(0.001)
                        * fil_flow
                        * p.speed_mm_s;
                    if q.is_finite() {
                        max_q = max_q.max(q);
                    }
                }
            }
            let cfg_cap = config
                .map(|c| c.filament.max_volumetric_speed_mm3_s)
                .unwrap_or(15.0);
            (0.0, max_q.max(cfg_cap).max(1.0))
        }
        ColorMode::LineWidth => {
            let mut min_w = f64::INFINITY;
            let mut max_w = 0.0_f64;
            for l in &plate.layers {
                for p in &l.paths {
                    min_w = min_w.min(p.min_width_mm());
                    max_w = max_w.max(p.max_width_mm());
                }
            }
            if min_w.is_finite() && max_w - min_w > 1e-4 {
                (min_w, max_w)
            } else {
                (0.20, 0.85)
            }
        }
    }
}

/// Compute RGBA color for an extrusion path according to `mode`.
pub fn path_color(
    path: &ExtrusionPath,
    mode: ColorMode,
    layer_idx: usize,
    total_layers: usize,
) -> [f32; 4] {
    let default_range = match mode {
        ColorMode::Speed => (15.0, 125.0),
        ColorMode::VolumetricFlow => (0.0, 15.0),
        ColorMode::FanSpeed => (0.0, 100.0),
        ColorMode::LineWidth => (0.20, 0.85),
        ColorMode::LayerHeight => (0.04, 0.32),
        ColorMode::Layer | ColorMode::Feature => (0.0, 1.0),
    };
    let area = bead_area_mm2(path.width_mm, path.height_mm);
    let flow = area * path.flow_ratio.max(0.001) * path.speed_mm_s;
    segment_color(
        path,
        mode,
        layer_idx,
        total_layers,
        path.width_mm,
        flow,
        path.fan_percent.unwrap_or(0),
        default_range,
    )
}

#[allow(clippy::too_many_arguments)]
fn segment_color(
    path: &ExtrusionPath,
    mode: ColorMode,
    layer_idx: usize,
    total_layers: usize,
    seg_width_mm: f64,
    flow_mm3_s: f64,
    fan_percent: u8,
    scale_range: (f64, f64),
) -> [f32; 4] {
    let (lo, hi) = scale_range;
    let span = (hi - lo).max(1e-4);
    match mode {
        ColorMode::Feature => role_color(path.role),
        ColorMode::Speed => {
            let t = ((path.speed_mm_s - lo) / span) as f32;
            color_ramp_sample(mode, t)
        }
        ColorMode::VolumetricFlow => {
            let t = ((flow_mm3_s - lo) / span) as f32;
            color_ramp_sample(mode, t)
        }
        ColorMode::FanSpeed => {
            let t = ((f64::from(fan_percent) - lo) / span) as f32;
            color_ramp_sample(mode, t)
        }
        ColorMode::LineWidth => {
            let t = ((seg_width_mm - lo) / span) as f32;
            color_ramp_sample(mode, t)
        }
        ColorMode::Layer => {
            let denom = total_layers.saturating_sub(1).max(1) as f32;
            let t = (layer_idx as f32) / denom;
            color_ramp_sample(mode, t)
        }
        ColorMode::LayerHeight => {
            let t = ((path.height_mm - lo) / span) as f32;
            color_ramp_sample(mode, t)
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
    let z = -0.01_f32;

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
    let z_ax = 0.01_f32;
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
    let z_top = -0.04_f32;
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

/// Move classification stored per step inside a sliced layer for travel/scrubbing preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum MoveKind {
    Extrusion(ExtrusionRole),
    Travel,
    CombedTravel,
    ZHopTravel,
}

impl MoveKind {
    /// Human-readable label displayed in the move scrubber readout and legend.
    pub fn label(self) -> &'static str {
        match self {
            Self::Extrusion(role) => role.label(),
            Self::Travel => "Travel",
            Self::CombedTravel => "Combed travel",
            Self::ZHopTravel => "Z-hop travel",
        }
    }

    /// Representative RGBA color matching the 3D viewport rendering.
    pub fn color(self) -> [f32; 4] {
        match self {
            Self::Extrusion(role) => role_color(role),
            Self::Travel => [0.28, 0.72, 0.98, 0.88],
            Self::CombedTravel => [0.12, 0.92, 0.78, 0.94],
            Self::ZHopTravel => [0.76, 0.44, 0.98, 0.94],
        }
    }
}

/// Flag bit on [`BeadInstance::flags`] enabling a domed start end-cap.
pub const BEAD_FLAG_START_CAP: u32 = 1;
/// Flag bit on [`BeadInstance::flags`] enabling a domed end cap.
pub const BEAD_FLAG_END_CAP: u32 = 2;
/// Flag bit on [`BeadInstance::flags`] selecting smooth radially interpolated normals at the start ring.
pub const BEAD_FLAG_START_SMOOTH: u32 = 4;
/// Flag bit on [`BeadInstance::flags`] selecting smooth radially interpolated normals at the end ring.
pub const BEAD_FLAG_END_SMOOTH: u32 = 8;

/// Static template vertex for GPU-instanced rounded stadium bead rendering.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeadTemplateVertex {
    pub uv_t_cap: [f32; 4],
    pub norm_ao: [f32; 3],
}

impl BeadTemplateVertex {
    /// Pack into `[u, v, t, cap_push, nu, nz, ao]` (`7 × f32 = 28 bytes`).
    pub fn to_packed(self) -> [f32; 7] {
        [
            self.uv_t_cap[0],
            self.uv_t_cap[1],
            self.uv_t_cap[2],
            self.uv_t_cap[3],
            self.norm_ao[0],
            self.norm_ao[1],
            self.norm_ao[2],
        ]
    }
}

/// Build the reusable unit stadium-capsule template mesh (`18` vertices, `96` indices) for GPU instancing.
pub fn build_bead_template_mesh() -> (Vec<BeadTemplateVertex>, Vec<u16>) {
    let n = PROFILE_8.len();
    let mut verts = Vec::with_capacity(n * 2 + 2);
    for t in [0.0_f32, 1.0_f32] {
        for s in PROFILE_8 {
            verts.push(BeadTemplateVertex {
                uv_t_cap: [s.u, s.v, t, 0.0],
                norm_ao: [s.nu, s.nz, s.ao],
            });
        }
    }
    // Start cap apex (index 2*n) and end cap apex (index 2*n + 1).
    verts.push(BeadTemplateVertex {
        uv_t_cap: [0.0, 0.0, 0.0, -1.0],
        norm_ao: [0.0, 0.0, 0.90],
    });
    verts.push(BeadTemplateVertex {
        uv_t_cap: [0.0, 0.0, 1.0, 1.0],
        norm_ao: [0.0, 0.0, 0.90],
    });

    let mut indices = Vec::with_capacity(n * 12);
    let start_apex = (n * 2) as u16;
    let end_apex = (n * 2 + 1) as u16;
    for k in 0..n {
        let k_next = (k + 1) % n;
        let a0 = k as u16;
        let a1 = k_next as u16;
        let b0 = (n + k) as u16;
        let b1 = (n + k_next) as u16;

        // Body quad (a0, b0, b1), (a0, b1, a1)
        indices.extend_from_slice(&[a0, b0, b1, a0, b1, a1]);
        // Start cap fan (start_apex, a0, a1)
        indices.extend_from_slice(&[start_apex, a0, a1]);
        // End cap fan (end_apex, b1, b0)
        indices.extend_from_slice(&[end_apex, b1, b0]);
    }
    (verts, indices)
}

/// Compact per-segment extruded filament bead instance (`48` bytes packed on GPU vs `1.44 KB` unindexed CPU tube).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeadInstance {
    pub pa: [f32; 2],
    pub pb: [f32; 2],
    pub off_a: [f32; 2],
    pub off_b: [f32; 2],
    pub z_mid: f32,
    pub hz: f32,
    pub flags: u32,
    pub color: [f32; 4],
}

impl BeadInstance {
    /// Pack into `12 × f32` (`48 bytes`) with 24-bit RGB stored exactly in the trailing IEEE-754 float.
    pub fn to_packed(self) -> [f32; 12] {
        let r8 = ((self.color[0].clamp(0.0, 1.0) * 255.0).round() as u32).min(255);
        let g8 = ((self.color[1].clamp(0.0, 1.0) * 255.0).round() as u32).min(255);
        let b8 = ((self.color[2].clamp(0.0, 1.0) * 255.0).round() as u32).min(255);
        let packed_rgb = ((r8 << 16) | (g8 << 8) | b8) as f32;
        [
            self.pa[0],
            self.pa[1],
            self.pb[0],
            self.pb[1],
            self.off_a[0],
            self.off_a[1],
            self.off_b[0],
            self.off_b[1],
            self.z_mid,
            self.hz,
            (self.flags & 0xF) as f32,
            packed_rgb,
        ]
    }
}

/// One sequential motion step within a layer, indexing into extruded bead (vertices or instances) and travel buffers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerMoveStep {
    pub toolpath_vertex_end: u32,
    pub travel_vertex_end: u32,
    pub nozzle_pos: [f32; 3],
    pub kind: MoveKind,
    pub speed_mm_s: f32,
    pub width_mm: f32,
    pub fan_percent: u8,
    pub flow_mm3_s: f32,
}

/// Compact GPU-instanced toolpath data indexed by layer and intra-layer move step.
#[derive(Debug, Clone, Default)]
pub struct ToolpathInstances {
    pub beads: Vec<BeadInstance>,
    pub layer_ranges: Vec<Range<u32>>,
    pub travel_lines: Vec<LineVertex>,
    pub travel_layer_ranges: Vec<Range<u32>>,
    pub layer_steps: Vec<Vec<LayerMoveStep>>,
}

/// 3D extruded bead ribbon and travel-line geometry indexed by layer and intra-layer step.
#[derive(Debug, Clone, Default)]
pub struct ToolpathGeometry {
    pub vertices: Vec<SceneVertex>,
    pub layer_ranges: Vec<Range<u32>>,
    pub travel_lines: Vec<LineVertex>,
    pub travel_layer_ranges: Vec<Range<u32>>,
    pub layer_steps: Vec<Vec<LayerMoveStep>>,
}

/// Build compact GPU-instanced bead ribbons (without combing travel hints) for every layer in `plate`.
pub fn build_toolpath_instances(plate: &SlicedPlate, mode: ColorMode) -> ToolpathInstances {
    build_toolpath_instances_with_config(plate, mode, None)
}

/// Build 3D extruded bead ribbons (without combing travel hints) for every layer in `plate`.
pub fn build_toolpath_mesh(plate: &SlicedPlate, mode: ColorMode) -> ToolpathGeometry {
    build_toolpath_mesh_with_config(plate, mode, None)
}

fn push_travel_segment(
    travel_lines: &mut Vec<LineVertex>,
    steps: &mut Vec<LayerMoveStep>,
    toolpath_vertex_end: u32,
    endpoints: [[f32; 3]; 2],
    nozzle_pos: [f32; 3],
    kind: MoveKind,
    speed_mm_s: f32,
) {
    let color = kind.color();
    travel_lines.push(LineVertex {
        pos: endpoints[0],
        color,
    });
    travel_lines.push(LineVertex {
        pos: endpoints[1],
        color,
    });
    steps.push(LayerMoveStep {
        toolpath_vertex_end,
        travel_vertex_end: travel_lines.len() as u32,
        nozzle_pos,
        kind,
        speed_mm_s,
        width_mm: 0.0,
        fan_percent: 0,
        flow_mm3_s: 0.0,
    });
}

#[derive(Debug, Clone, Copy)]
struct ProfileSample {
    u: f32,
    v: f32,
    nu: f32,
    nz: f32,
    ao: f32,
}

const PROFILE_8: &[ProfileSample] = &[
    // Top crown
    ProfileSample {
        u: 0.00,
        v: 1.000,
        nu: 0.000,
        nz: 1.000,
        ao: 1.03,
    },
    // Top-left shoulder
    ProfileSample {
        u: 0.66,
        v: 0.943,
        nu: 0.550,
        nz: 0.835,
        ao: 0.96,
    },
    // Mid-left flank (bulges cleanly out to define bead seam & layer lines)
    ProfileSample {
        u: 1.00,
        v: 0.000,
        nu: 1.000,
        nz: 0.000,
        ao: 0.88,
    },
    // Bottom-left shoulder
    ProfileSample {
        u: 0.66,
        v: -0.943,
        nu: 0.550,
        nz: -0.835,
        ao: 0.64,
    },
    // Bottom keel
    ProfileSample {
        u: 0.00,
        v: -1.000,
        nu: 0.000,
        nz: -1.000,
        ao: 0.56,
    },
    // Bottom-right shoulder
    ProfileSample {
        u: -0.66,
        v: -0.943,
        nu: -0.550,
        nz: -0.835,
        ao: 0.64,
    },
    // Mid-right flank
    ProfileSample {
        u: -1.00,
        v: 0.000,
        nu: -1.000,
        nz: 0.000,
        ao: 0.88,
    },
    // Top-right shoulder
    ProfileSample {
        u: -0.66,
        v: 0.943,
        nu: -0.550,
        nz: 0.835,
        ao: 0.96,
    },
];

const PROFILE_6: &[ProfileSample] = &[
    ProfileSample {
        u: 0.00,
        v: 1.000,
        nu: 0.000,
        nz: 1.000,
        ao: 1.03,
    },
    ProfileSample {
        u: 1.00,
        v: 0.150,
        nu: 0.940,
        nz: 0.340,
        ao: 0.90,
    },
    ProfileSample {
        u: 0.68,
        v: -0.943,
        nu: 0.580,
        nz: -0.810,
        ao: 0.64,
    },
    ProfileSample {
        u: 0.00,
        v: -1.000,
        nu: 0.000,
        nz: -1.000,
        ao: 0.56,
    },
    ProfileSample {
        u: -0.68,
        v: -0.943,
        nu: -0.580,
        nz: -0.810,
        ao: 0.64,
    },
    ProfileSample {
        u: -1.00,
        v: 0.150,
        nu: -0.940,
        nz: 0.340,
        ao: 0.90,
    },
];

/// Small deterministic Z offset separating coplanar layer features cleanly in the depth buffer.
#[inline]
pub fn role_z_bias_mm(role: ExtrusionRole) -> f32 {
    match role {
        ExtrusionRole::Ironing => 0.002,
        ExtrusionRole::OuterWall
        | ExtrusionRole::OverhangWall
        | ExtrusionRole::Skirt
        | ExtrusionRole::Brim => 0.000,
        ExtrusionRole::InnerWall => -0.004,
        ExtrusionRole::SupportInterface => -0.005,
        ExtrusionRole::GapFill => -0.006,
        ExtrusionRole::TopSurface | ExtrusionRole::BottomSurface => -0.007,
        ExtrusionRole::Support => -0.008,
        ExtrusionRole::Bridge | ExtrusionRole::InternalBridge => -0.009,
        ExtrusionRole::InternalSolidInfill => -0.011,
        ExtrusionRole::SparseInfill => -0.014,
    }
}

#[derive(Debug, Clone, Copy)]
struct PathSeg {
    pa: (f32, f32),
    pb: (f32, f32),
    dir: (f32, f32),
    left: (f32, f32),
    len: f32,
    hwa: f32,
    hwb: f32,
    seg_width_mm: f32,
}

fn corner_join(seg_a: &PathSeg, seg_b: &PathSeg) -> Option<((f32, f32), bool)> {
    let gap = (seg_b.pa.0 - seg_a.pb.0).hypot(seg_b.pa.1 - seg_a.pb.1);
    if gap > 1e-3 {
        return None;
    }
    let dot = seg_a.dir.0 * seg_b.dir.0 + seg_a.dir.1 * seg_b.dir.1;
    if dot <= -0.5 {
        return None;
    }
    let lx = seg_a.left.0 + seg_b.left.0;
    let ly = seg_a.left.1 + seg_b.left.1;
    let denom = (1.0 + dot).max(0.25);
    let mut mx = lx / denom;
    let mut my = ly / denom;
    let m_len = mx.hypot(my);
    let max_miter = if seg_a.len.min(seg_b.len) < 1.2 * seg_a.hwb.max(seg_b.hwa) {
        1.35
    } else {
        1.65
    };
    if m_len > max_miter && m_len > 1e-5 {
        let scale = max_miter / m_len;
        mx *= scale;
        my *= scale;
    }
    let is_smooth_curve = dot > 0.80;
    Some(((mx, my), is_smooth_curve))
}

#[inline]
fn shade_bead_color(base: [f32; 4], ao: f32) -> [f32; 4] {
    [
        (base[0] * ao).clamp(0.0, 1.0),
        (base[1] * ao).clamp(0.0, 1.0),
        (base[2] * ao).clamp(0.0, 1.0),
        base[3],
    ]
}

fn build_ring_vertices(
    profile: &[ProfileSample],
    center: [f32; 3],
    radii: (f32, f32),
    offset_vec: (f32, f32),
    norm_left: (f32, f32),
    base_color: [f32; 4],
) -> Vec<SceneVertex> {
    let [cx, cy, z_mid] = center;
    let (hw, hz) = radii;
    let (ox, oy) = offset_vec;
    let (nlx, nly) = norm_left;
    let mut ring = Vec::with_capacity(profile.len());
    for s in profile {
        let px = cx + ox * (hw * s.u);
        let py = cy + oy * (hw * s.u);
        let pz = z_mid + hz * s.v;
        let nx = nlx * s.nu;
        let ny = nly * s.nu;
        let nz = s.nz;
        let n_len = (nx * nx + ny * ny + nz * nz).sqrt().max(1e-5);
        ring.push(SceneVertex {
            pos: [px, py, pz],
            normal: [nx / n_len, ny / n_len, nz / n_len],
            color: shade_bead_color(base_color, s.ao),
        });
    }
    ring
}

fn push_smooth_quad(
    out: &mut Vec<SceneVertex>,
    a: SceneVertex,
    b: SceneVertex,
    c: SceneVertex,
    d: SceneVertex,
) {
    out.push(a);
    out.push(b);
    out.push(c);
    out.push(a);
    out.push(c);
    out.push(d);
}

fn push_domed_cap(
    out: &mut Vec<SceneVertex>,
    ring: &[SceneVertex],
    center: [f32; 3],
    dir: (f32, f32),
    hw: f32,
    base_color: [f32; 4],
    is_start: bool,
) {
    let n = ring.len();
    if n < 3 {
        return;
    }
    let cap_push = (0.35 * hw).min(0.25);
    let sign = if is_start { -1.0_f32 } else { 1.0_f32 };
    let apex = SceneVertex {
        pos: [
            center[0] + sign * dir.0 * cap_push,
            center[1] + sign * dir.1 * cap_push,
            center[2],
        ],
        normal: [sign * dir.0, sign * dir.1, 0.0],
        color: shade_bead_color(base_color, 0.90),
    };
    for k in 0..n {
        let (Some(&v0), Some(&v1)) = (ring.get(k), ring.get((k + 1) % n)) else {
            continue;
        };
        out.push(apex);
        if is_start {
            out.push(v0);
            out.push(v1);
        } else {
            out.push(v1);
            out.push(v0);
        }
    }
}

fn expand_bead_into(bead: BeadInstance, profile: &[ProfileSample], out: &mut Vec<SceneVertex>) {
    let n_prof = profile.len();
    let [ax, ay] = bead.pa;
    let [bx, by] = bead.pb;
    let dx = bx - ax;
    let dy = by - ay;
    let len = dx.hypot(dy).max(1e-5);
    let dir = (dx / len, dy / len);
    let left = (-dir.1, dir.0);

    let norm_from_offset = |off: [f32; 2], smooth: bool| -> (f32, f32) {
        if smooth {
            let l = off[0].hypot(off[1]);
            if l > 1e-5 {
                (off[0] / l, off[1] / l)
            } else {
                left
            }
        } else {
            left
        }
    };

    let start_norm = norm_from_offset(bead.off_a, (bead.flags & BEAD_FLAG_START_SMOOTH) != 0);
    let end_norm = norm_from_offset(bead.off_b, (bead.flags & BEAD_FLAG_END_SMOOTH) != 0);

    let start_ring = build_ring_vertices(
        profile,
        [ax, ay, bead.z_mid],
        (1.0, bead.hz),
        (bead.off_a[0], bead.off_a[1]),
        start_norm,
        bead.color,
    );
    let end_ring = build_ring_vertices(
        profile,
        [bx, by, bead.z_mid],
        (1.0, bead.hz),
        (bead.off_b[0], bead.off_b[1]),
        end_norm,
        bead.color,
    );

    for k in 0..n_prof {
        let k_next = (k + 1) % n_prof;
        if let (Some(&a0), Some(&a1), Some(&b0), Some(&b1)) = (
            start_ring.get(k),
            start_ring.get(k_next),
            end_ring.get(k),
            end_ring.get(k_next),
        ) {
            push_smooth_quad(out, a0, b0, b1, a1);
        }
    }

    if (bead.flags & BEAD_FLAG_START_CAP) != 0 {
        let hw_a = bead.off_a[0].hypot(bead.off_a[1]);
        push_domed_cap(
            out,
            &start_ring,
            [ax, ay, bead.z_mid],
            dir,
            hw_a,
            bead.color,
            true,
        );
    }
    if (bead.flags & BEAD_FLAG_END_CAP) != 0 {
        let hw_b = bead.off_b[0].hypot(bead.off_b[1]);
        push_domed_cap(
            out,
            &end_ring,
            [bx, by, bead.z_mid],
            dir,
            hw_b,
            bead.color,
            false,
        );
    }
}

/// Expand a slice of compact [`BeadInstance`] records into world-space CPU [`SceneVertex`] triangles.
pub fn expand_bead_instances_to_vertices(beads: &[BeadInstance]) -> Vec<SceneVertex> {
    let profile = if beads.len() > 60_000 {
        PROFILE_6
    } else {
        PROFILE_8
    };
    let mut out = Vec::with_capacity(beads.len() * profile.len() * 6);
    for &bead in beads {
        expand_bead_into(bead, profile, &mut out);
    }
    out
}

/// Build compact GPU-instanced rounded bead descriptors, combed/Z-hop travel lines, and per-step scrubbing indices.
pub fn build_toolpath_instances_with_config(
    plate: &SlicedPlate,
    mode: ColorMode,
    config: Option<&PrintConfig>,
) -> ToolpathInstances {
    let total_layers = plate.layers.len();
    let scale_range = compute_color_mode_range(plate, mode, config);
    let fil_flow = config
        .map(|c| c.filament.flow_ratio.max(0.001))
        .unwrap_or(1.0);

    let travel_speed = config
        .map(|c| c.machine.travel_speed_mm_s as f32)
        .unwrap_or(180.0);
    let min_retract_travel = config
        .map(|c| c.machine.min_travel_for_retract_mm)
        .unwrap_or(1.5);
    let z_hop_mm = config.map(|c| c.machine.z_hop_mm).unwrap_or(0.0);
    let enable_combing = config.is_some_and(|c| c.process.avoid_crossing_perimeters);
    let outer_wall_w = config
        .map(|c| c.process.outer_wall_width_mm)
        .unwrap_or(0.42);
    let max_detour_mm = config
        .map(|c| c.process.avoid_crossing_perimeters_max_detour_mm)
        .unwrap_or(50.0);

    let mut beads = Vec::new();
    let mut layer_ranges = Vec::with_capacity(total_layers);
    let mut travel_lines = Vec::new();
    let mut travel_layer_ranges = Vec::with_capacity(total_layers);
    let mut layer_steps = Vec::with_capacity(total_layers);
    let mut prev_xy: Option<(f64, f64)> = None;

    for (layer_idx, layer) in plate.layers.iter().enumerate() {
        let start = beads.len() as u32;
        let travel_start = travel_lines.len() as u32;
        let mut steps = Vec::new();
        let z_top = layer.z_top_mm as f32;
        let layer_h = (layer.height_mm as f32).max(0.02);
        let z_center_base = z_top - 0.5 * layer_h;
        let hz = 0.53 * layer_h;
        let z_line = z_top + 0.03;

        let combing = if enable_combing && !layer.slice_regions.is_empty() {
            Some(CombingLayer::from_regions(
                &layer.slice_regions,
                outer_wall_w,
            ))
        } else {
            None
        };

        for path in &layer.paths {
            let n = path.points.len();
            if n < 2 {
                continue;
            }
            let Some(&first_pt) = path.points.first() else {
                continue;
            };
            let (sx_f64, sy_f64) = first_pt.to_mm();

            if let Some((px, py)) = prev_xy {
                let dist = (sx_f64 - px).hypot(sy_f64 - py);
                if dist > 1e-3 {
                    let cur_v_end = beads.len() as u32;
                    let combed_pts = combing
                        .as_ref()
                        .and_then(|c| c.plan_travel((px, py), (sx_f64, sy_f64), max_detour_mm));
                    let is_combed = combed_pts.is_some();
                    let use_z_hop = z_hop_mm > 0.0 && dist > min_retract_travel;

                    if use_z_hop && !is_combed {
                        let z_hop_top = z_top + (z_hop_mm as f32);
                        let p3 = [px as f32, py as f32, z_line];
                        let p_up = [px as f32, py as f32, z_hop_top];
                        let s_up = [sx_f64 as f32, sy_f64 as f32, z_hop_top];
                        let s3 = [sx_f64 as f32, sy_f64 as f32, z_line];
                        push_travel_segment(
                            &mut travel_lines,
                            &mut steps,
                            cur_v_end,
                            [p3, p_up],
                            p_up,
                            MoveKind::ZHopTravel,
                            travel_speed,
                        );
                        push_travel_segment(
                            &mut travel_lines,
                            &mut steps,
                            cur_v_end,
                            [p_up, s_up],
                            s_up,
                            MoveKind::ZHopTravel,
                            travel_speed,
                        );
                        push_travel_segment(
                            &mut travel_lines,
                            &mut steps,
                            cur_v_end,
                            [s_up, s3],
                            [sx_f64 as f32, sy_f64 as f32, z_top],
                            MoveKind::ZHopTravel,
                            travel_speed,
                        );
                    } else if let Some(waypoints) = combed_pts {
                        let mut wx_prev = px as f32;
                        let mut wy_prev = py as f32;
                        for (wx, wy) in waypoints {
                            let wx_f = wx as f32;
                            let wy_f = wy as f32;
                            if (wx_f - wx_prev).hypot(wy_f - wy_prev) > 1e-3 {
                                push_travel_segment(
                                    &mut travel_lines,
                                    &mut steps,
                                    cur_v_end,
                                    [[wx_prev, wy_prev, z_line], [wx_f, wy_f, z_line]],
                                    [wx_f, wy_f, z_top],
                                    MoveKind::CombedTravel,
                                    travel_speed,
                                );
                                wx_prev = wx_f;
                                wy_prev = wy_f;
                            }
                        }
                        let sx_f = sx_f64 as f32;
                        let sy_f = sy_f64 as f32;
                        if (sx_f - wx_prev).hypot(sy_f - wy_prev) > 1e-3 {
                            push_travel_segment(
                                &mut travel_lines,
                                &mut steps,
                                cur_v_end,
                                [[wx_prev, wy_prev, z_line], [sx_f, sy_f, z_line]],
                                [sx_f, sy_f, z_top],
                                MoveKind::CombedTravel,
                                travel_speed,
                            );
                        }
                    } else {
                        push_travel_segment(
                            &mut travel_lines,
                            &mut steps,
                            cur_v_end,
                            [
                                [px as f32, py as f32, z_line],
                                [sx_f64 as f32, sy_f64 as f32, z_line],
                            ],
                            [sx_f64 as f32, sy_f64 as f32, z_top],
                            MoveKind::Travel,
                            travel_speed,
                        );
                    }
                }
            }

            let seg_count = if path.closed { n } else { n - 1 };
            let mut segs: Vec<PathSeg> = Vec::with_capacity(seg_count);
            let mut best_len = 0.0_f32;
            let mut best_dir = (1.0_f32, 0.0_f32);

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
                let (ax, ay) = pa.to_mm();
                let (bx, by) = pb.to_mm();
                let (ax, ay, bx, by) = (ax as f32, ay as f32, bx as f32, by as f32);
                let dx = bx - ax;
                let dy = by - ay;
                let len = dx.hypot(dy);
                if len < 1e-4 {
                    continue;
                }
                let dir = (dx / len, dy / len);
                let left = (-dir.1, dir.0);
                if len > best_len {
                    best_len = len;
                    best_dir = dir;
                }
                let hwa = 0.985 * (0.5 * path.vertex_width_mm(i)) as f32;
                let hwb = 0.985 * (0.5 * path.vertex_width_mm(next_idx)) as f32;
                segs.push(PathSeg {
                    pa: (ax, ay),
                    pb: (bx, by),
                    dir,
                    left,
                    len,
                    hwa,
                    hwb,
                    seg_width_mm: path.segment_width_mm(i) as f32,
                });
            }

            if !segs.is_empty() {
                let mut z_bias = role_z_bias_mm(path.role);
                if matches!(
                    path.role,
                    ExtrusionRole::SparseInfill
                        | ExtrusionRole::Support
                        | ExtrusionRole::SupportInterface
                ) {
                    let angle_norm = best_dir
                        .1
                        .atan2(best_dir.0)
                        .rem_euclid(std::f32::consts::PI)
                        / std::f32::consts::PI;
                    z_bias += angle_norm * 0.0025;
                }
                let z_mid = z_center_base + z_bias;
                let eff_fan = path.fan_percent.unwrap_or(layer.fan_percent).min(100);
                let num_segs = segs.len();

                for s_idx in 0..num_segs {
                    let Some(&cur) = segs.get(s_idx) else {
                        continue;
                    };
                    let seg_w = f64::from(cur.seg_width_mm);
                    let flow_mm3_s = bead_area_mm2(seg_w, path.height_mm)
                        * path.flow_ratio.max(0.001)
                        * fil_flow
                        * path.speed_mm_s;
                    let color = segment_color(
                        path,
                        mode,
                        layer_idx,
                        total_layers,
                        seg_w,
                        flow_mm3_s,
                        eff_fan,
                        scale_range,
                    );
                    let prev_opt = if s_idx > 0 {
                        segs.get(s_idx - 1)
                    } else if path.closed && num_segs > 1 {
                        segs.last()
                    } else {
                        None
                    };
                    let next_opt = if s_idx + 1 < num_segs {
                        segs.get(s_idx + 1)
                    } else if path.closed && num_segs > 1 {
                        segs.first()
                    } else {
                        None
                    };

                    let prev_join = prev_opt.and_then(|prev| corner_join(prev, &cur));
                    let next_join = next_opt.and_then(|next| corner_join(&cur, next));

                    let (start_offset, start_joined, start_smooth) = match prev_join {
                        Some((miter, smooth)) => (miter, true, smooth),
                        None => (cur.left, false, false),
                    };
                    let (end_offset, end_joined, end_smooth) = match next_join {
                        Some((miter, smooth)) => (miter, true, smooth),
                        None => (cur.left, false, false),
                    };

                    let mut flags = 0u32;
                    if !start_joined {
                        flags |= BEAD_FLAG_START_CAP;
                    }
                    if !end_joined {
                        flags |= BEAD_FLAG_END_CAP;
                    }
                    if start_smooth {
                        flags |= BEAD_FLAG_START_SMOOTH;
                    }
                    if end_smooth {
                        flags |= BEAD_FLAG_END_SMOOTH;
                    }

                    beads.push(BeadInstance {
                        pa: [cur.pa.0, cur.pa.1],
                        pb: [cur.pb.0, cur.pb.1],
                        off_a: [start_offset.0 * cur.hwa, start_offset.1 * cur.hwa],
                        off_b: [end_offset.0 * cur.hwb, end_offset.1 * cur.hwb],
                        z_mid,
                        hz,
                        flags,
                        color,
                    });

                    steps.push(LayerMoveStep {
                        toolpath_vertex_end: beads.len() as u32,
                        travel_vertex_end: travel_lines.len() as u32,
                        nozzle_pos: [cur.pb.0, cur.pb.1, z_top],
                        kind: MoveKind::Extrusion(path.role),
                        speed_mm_s: path.speed_mm_s as f32,
                        width_mm: cur.seg_width_mm,
                        fan_percent: eff_fan,
                        flow_mm3_s: flow_mm3_s as f32,
                    });
                }
            }

            let end_pt = if path.closed {
                first_pt
            } else {
                path.points.last().copied().unwrap_or(first_pt)
            };
            prev_xy = Some(end_pt.to_mm());
        }

        let end = beads.len() as u32;
        let travel_end = travel_lines.len() as u32;
        layer_ranges.push(start..end);
        travel_layer_ranges.push(travel_start..travel_end);
        layer_steps.push(steps);
    }

    ToolpathInstances {
        beads,
        layer_ranges,
        travel_lines,
        travel_layer_ranges,
        layer_steps,
    }
}

/// Build realistic rounded stadium-profile filament beads, combed/Z-hop travel lines, and per-step scrubbing indices.
pub fn build_toolpath_mesh_with_config(
    plate: &SlicedPlate,
    mode: ColorMode,
    config: Option<&PrintConfig>,
) -> ToolpathGeometry {
    let insts = build_toolpath_instances_with_config(plate, mode, config);
    let profile = if insts.beads.len() > 60_000 {
        PROFILE_6
    } else {
        PROFILE_8
    };
    let mut vertices = Vec::with_capacity(insts.beads.len() * profile.len() * 6);
    let mut inst_end_to_vert_end = Vec::with_capacity(insts.beads.len() + 1);
    inst_end_to_vert_end.push(0u32);
    for &bead in &insts.beads {
        expand_bead_into(bead, profile, &mut vertices);
        inst_end_to_vert_end.push(vertices.len() as u32);
    }
    let map_end = |inst_idx: u32| -> u32 {
        inst_end_to_vert_end
            .get(inst_idx as usize)
            .copied()
            .unwrap_or(vertices.len() as u32)
    };
    let layer_ranges = insts
        .layer_ranges
        .into_iter()
        .map(|r| map_end(r.start)..map_end(r.end))
        .collect();
    let layer_steps = insts
        .layer_steps
        .into_iter()
        .map(|steps| {
            steps
                .into_iter()
                .map(|mut step| {
                    step.toolpath_vertex_end = map_end(step.toolpath_vertex_end);
                    step
                })
                .collect()
        })
        .collect();

    ToolpathGeometry {
        vertices,
        layer_ranges,
        travel_lines: insts.travel_lines,
        travel_layer_ranges: insts.travel_layer_ranges,
        layer_steps,
    }
}

/// Build a 3D metallic nozzle head marker mesh pointing at `tip_pos`.
pub fn build_nozzle_marker_mesh(tip_pos: [f32; 3], dark_mode: bool) -> Vec<SceneVertex> {
    const SIDES: usize = 12;
    let [tx, ty, base_z] = tip_pos;
    let tz = base_z + 0.06;
    let brass_col = if dark_mode {
        [0.94, 0.74, 0.28, 1.0]
    } else {
        [0.84, 0.62, 0.18, 1.0]
    };
    let tip_col = [0.08, 0.84, 0.76, 1.0];
    let collar_col = if dark_mode {
        [0.52, 0.58, 0.68, 1.0]
    } else {
        [0.34, 0.39, 0.48, 1.0]
    };

    let mut out = Vec::with_capacity(SIDES * 18);
    let tip = [tx, ty, tz];
    let apex = [tx, ty, tz + 8.6];

    let ring = |idx: usize, radius: f32, z: f32| -> ([f32; 3], [f32; 3]) {
        let a = (idx as f32) * std::f32::consts::TAU / (SIDES as f32);
        let (sa, ca) = a.sin_cos();
        ([tx + ca * radius, ty + sa * radius, z], [ca, sa, 0.0])
    };

    for i in 0..SIDES {
        let j = (i + 1) % SIDES;
        // 1. Teal tip cone: tz -> tz + 1.5
        let (p0, n0) = ring(i, 0.75, tz + 1.5);
        let (p1, n1) = ring(j, 0.75, tz + 1.5);
        let cn = [0.5 * (n0[0] + n1[0]), 0.5 * (n0[1] + n1[1]), -0.35];
        push_tri(&mut out, tip, p1, p0, cn, tip_col);

        // 2. Brass nozzle taper: tz + 1.5 (r=0.75) -> tz + 3.8 (r=2.1)
        let (b0, _) = ring(i, 2.1, tz + 3.8);
        let (b1, _) = ring(j, 2.1, tz + 3.8);
        push_quad(&mut out, p0, p1, b1, b0, cn, brass_col);

        // 3. Heater collar cylinder: tz + 3.8 -> tz + 7.6 (r=2.1)
        let (u0, _) = ring(i, 2.1, tz + 7.6);
        let (u1, _) = ring(j, 2.1, tz + 7.6);
        let wall_n = [0.5 * (n0[0] + n1[0]), 0.5 * (n0[1] + n1[1]), 0.0];
        push_quad(&mut out, b0, b1, u1, u0, wall_n, collar_col);

        // 4. Top bevel cap: tz + 7.6 -> apex
        push_tri(&mut out, u0, u1, apex, [0.0, 0.0, 1.0], collar_col);
    }

    out
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

    #[test]
    fn toolpath_mesh_builds_travels_combing_steps_and_nozzle_marker() {
        let mut cfg = PrintConfig::default();
        cfg.process.avoid_crossing_perimeters = true;
        cfg.machine.z_hop_mm = 0.4;
        let mesh = sample_cube();
        let plate = slice_single_mesh(&mesh, &cfg).unwrap();

        let geom = build_toolpath_mesh_with_config(&plate, ColorMode::Feature, Some(&cfg));
        assert_eq!(geom.layer_ranges.len(), plate.layers.len());
        assert_eq!(geom.travel_layer_ranges.len(), plate.layers.len());
        assert_eq!(geom.layer_steps.len(), plate.layers.len());
        assert!(!geom.travel_lines.is_empty());

        let mut prev_v = 0u32;
        let mut prev_t = 0u32;
        let mut seen_combed_or_zhop = false;
        for steps in &geom.layer_steps {
            assert!(!steps.is_empty());
            for step in steps {
                assert!(step.toolpath_vertex_end >= prev_v);
                assert!(step.travel_vertex_end >= prev_t);
                prev_v = step.toolpath_vertex_end;
                prev_t = step.travel_vertex_end;
                if matches!(step.kind, MoveKind::CombedTravel | MoveKind::ZHopTravel) {
                    seen_combed_or_zhop = true;
                }
            }
        }
        assert!(seen_combed_or_zhop);

        let nozzle = build_nozzle_marker_mesh([125.0, 125.0, 10.0], true);
        assert_eq!(nozzle.len(), 12 * 18);
    }

    #[test]
    fn screen_ray_picks_centered_cube_and_misses_corner() {
        let front_cube = sample_cube();
        let mut back_cube = sample_cube();
        back_cube.translate(0.0, 60.0, 0.0).unwrap();

        let aabb = front_cube.aabb().unwrap();
        let mut cam = OrbitCamera::framed_on_aabb(CameraPreset::Front, aabb, 250.0, 250.0);
        // Frame slightly elevated from -Y looking towards +Y at front_cube first, back_cube behind it.
        cam.pitch_rad = 0.0;
        let aspect = 16.0 / 9.0;

        let center_ray = cam.screen_ray(0.0, 0.0, aspect);
        let t_front =
            ray_intersect_mesh(center_ray, &front_cube).expect("center ray hits front cube");
        let t_back = ray_intersect_mesh(center_ray, &back_cube).expect("center ray hits back cube");
        assert!(t_front < t_back, "expected front {t_front} < back {t_back}");

        let corner_ray = cam.screen_ray(0.95, 0.95, aspect);
        assert!(ray_intersect_mesh(corner_ray, &front_cube).is_none());
    }

    #[test]
    fn toolpath_beads_have_rounded_profile_layer_normals_and_role_z_bias() {
        assert!(
            role_z_bias_mm(ExtrusionRole::OuterWall) > role_z_bias_mm(ExtrusionRole::InnerWall)
        );
        assert!(
            role_z_bias_mm(ExtrusionRole::InnerWall) > role_z_bias_mm(ExtrusionRole::TopSurface)
        );
        assert!(role_z_bias_mm(ExtrusionRole::TopSurface) > role_z_bias_mm(ExtrusionRole::Bridge));
        assert!(
            role_z_bias_mm(ExtrusionRole::Bridge) > role_z_bias_mm(ExtrusionRole::SparseInfill)
        );

        let cfg = PrintConfig::default();
        let mesh = sample_cube();
        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let geom = build_toolpath_mesh_with_config(&plate, ColorMode::Feature, Some(&cfg));
        assert!(!geom.vertices.is_empty());

        let has_top = geom.vertices.iter().any(|v| v.normal[2] > 0.7);
        let has_flank = geom.vertices.iter().any(|v| v.normal[2].abs() < 0.15);
        let has_bottom = geom.vertices.iter().any(|v| v.normal[2] < -0.7);
        assert!(has_top && has_flank && has_bottom);

        let segs = [
            PathSeg {
                pa: (0.0, 0.0),
                pb: (20.0, 0.0),
                dir: (1.0, 0.0),
                left: (0.0, 1.0),
                len: 20.0,
                hwa: 0.21,
                hwb: 0.21,
                seg_width_mm: 0.42,
            },
            PathSeg {
                pa: (20.0, 0.0),
                pb: (20.0, 20.0),
                dir: (0.0, 1.0),
                left: (-1.0, 0.0),
                len: 20.0,
                hwa: 0.21,
                hwb: 0.21,
                seg_width_mm: 0.42,
            },
        ];
        let (miter, smooth) = corner_join(&segs[0], &segs[1]).unwrap();
        assert!(!smooth);
        assert!((miter.0 - (-1.0)).abs() < 1e-5);
        assert!((miter.1 - 1.0).abs() < 1e-5);

        // Build a closed 20x20 path and check consecutive segments share exact mitered corner vertices.
        let path_plate = SlicedPlate {
            layers: vec![layercraft_slice::SlicedLayer {
                index: 0,
                z_top_mm: 0.2,
                height_mm: 0.2,
                fan_percent: 0,
                estimated_time_s: 1.0,
                object_id: None,
                slice_regions: Vec::new(),
                paths: vec![ExtrusionPath {
                    role: ExtrusionRole::OuterWall,
                    points: vec![
                        layercraft_geom::Point2::from_mm(0.0, 0.0).unwrap(),
                        layercraft_geom::Point2::from_mm(20.0, 0.0).unwrap(),
                        layercraft_geom::Point2::from_mm(20.0, 20.0).unwrap(),
                        layercraft_geom::Point2::from_mm(0.0, 20.0).unwrap(),
                    ],
                    point_widths_mm: Vec::new(),
                    closed: true,
                    width_mm: 0.42,
                    height_mm: 0.2,
                    speed_mm_s: 40.0,
                    flow_ratio: 1.0,
                    object_id: None,
                    fan_percent: None,
                }],
            }],
            objects: Vec::new(),
            print_sequence: Default::default(),
        };
        let path_geom = build_toolpath_mesh(&path_plate, ColorMode::Feature);
        // 4 segments * 8 quads * 6 vertices = 192 vertices (no open caps on closed path).
        assert_eq!(path_geom.vertices.len(), 192);
        // Seg 0 end vertex b0 (vertex 1) must match Seg 1 start vertex a0 (vertex 48).
        assert_eq!(path_geom.vertices[1].pos, path_geom.vertices[48].pos);
    }

    #[test]
    fn toolpath_instances_pack_template_and_match_cpu_beads() {
        let (tpl_verts, tpl_indices) = build_bead_template_mesh();
        assert_eq!(tpl_verts.len(), 18);
        assert_eq!(tpl_indices.len(), 96);
        assert_eq!(tpl_verts[16].uv_t_cap[3], -1.0);
        assert_eq!(tpl_verts[17].uv_t_cap[3], 1.0);

        let cfg = PrintConfig::default();
        let mesh = sample_cube();
        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let insts = build_toolpath_instances_with_config(&plate, ColorMode::Feature, Some(&cfg));
        assert_eq!(insts.layer_ranges.len(), plate.layers.len());
        assert!(!insts.beads.is_empty());

        let expanded = expand_bead_instances_to_vertices(&insts.beads);
        let cpu_geom = build_toolpath_mesh_with_config(&plate, ColorMode::Feature, Some(&cfg));
        assert_eq!(expanded.len(), cpu_geom.vertices.len());

        // Verify RGB24 exact roundtrip in packed IEEE-754 float.
        let sample_bead = BeadInstance {
            pa: [10.0, 20.0],
            pb: [30.0, 40.0],
            off_a: [0.0, 0.21],
            off_b: [-0.21, 0.21],
            z_mid: 0.1,
            hz: 0.106,
            flags: BEAD_FLAG_START_CAP | BEAD_FLAG_END_SMOOTH,
            color: [0.95, 0.52, 0.14, 1.0],
        };
        let packed = sample_bead.to_packed();
        assert_eq!(
            packed[10] as u32,
            BEAD_FLAG_START_CAP | BEAD_FLAG_END_SMOOTH
        );
        let rgb = packed[11].round() as u32;
        assert_eq!((rgb >> 16) & 0xFF, (0.95_f32 * 255.0).round() as u32);
        assert_eq!((rgb >> 8) & 0xFF, (0.52_f32 * 255.0).round() as u32);
        assert_eq!(rgb & 0xFF, (0.14_f32 * 255.0).round() as u32);
    }

    #[test]
    fn color_modes_volumetric_flow_fan_and_width_differentiate_segments() {
        let mut cfg = PrintConfig::default();
        cfg.filament.max_volumetric_speed_mm3_s = 20.0;

        let plate = SlicedPlate {
            layers: vec![layercraft_slice::SlicedLayer {
                index: 1,
                z_top_mm: 0.4,
                height_mm: 0.2,
                fan_percent: 30,
                estimated_time_s: 1.0,
                object_id: None,
                slice_regions: Vec::new(),
                paths: vec![
                    ExtrusionPath {
                        role: ExtrusionRole::OuterWall,
                        width_mm: 0.30,
                        point_widths_mm: vec![0.30, 0.80, 0.80],
                        height_mm: 0.2,
                        speed_mm_s: 40.0,
                        flow_ratio: 1.0,
                        points: vec![
                            layercraft_geom::Point2::from_mm(10.0, 10.0).unwrap(),
                            layercraft_geom::Point2::from_mm(30.0, 10.0).unwrap(),
                            layercraft_geom::Point2::from_mm(50.0, 10.0).unwrap(),
                        ],
                        closed: false,
                        object_id: None,
                        fan_percent: None,
                    },
                    ExtrusionPath {
                        role: ExtrusionRole::Bridge,
                        width_mm: 0.45,
                        point_widths_mm: Vec::new(),
                        height_mm: 0.2,
                        speed_mm_s: 120.0,
                        flow_ratio: 1.0,
                        points: vec![
                            layercraft_geom::Point2::from_mm(50.0, 20.0).unwrap(),
                            layercraft_geom::Point2::from_mm(80.0, 20.0).unwrap(),
                        ],
                        closed: false,
                        object_id: None,
                        fan_percent: Some(100),
                    },
                ],
            }],
            objects: Vec::new(),
            print_sequence: Default::default(),
        };

        let flow_inst =
            build_toolpath_instances_with_config(&plate, ColorMode::VolumetricFlow, Some(&cfg));
        assert_eq!(flow_inst.beads.len(), 3);
        assert_ne!(flow_inst.beads[0].color, flow_inst.beads[2].color);

        let fan_inst =
            build_toolpath_instances_with_config(&plate, ColorMode::FanSpeed, Some(&cfg));
        assert_ne!(fan_inst.beads[0].color, fan_inst.beads[2].color);
        let ext_steps: Vec<_> = fan_inst.layer_steps[0]
            .iter()
            .filter(|s| matches!(s.kind, MoveKind::Extrusion(_)))
            .collect();
        assert_eq!(ext_steps[0].fan_percent, 30);
        assert_eq!(ext_steps[2].fan_percent, 100);
        assert!(ext_steps[2].flow_mm3_s > ext_steps[0].flow_mm3_s);

        let width_inst =
            build_toolpath_instances_with_config(&plate, ColorMode::LineWidth, Some(&cfg));
        assert_ne!(width_inst.beads[0].color, width_inst.beads[1].color);

        let (lo, hi) = compute_color_mode_range(&plate, ColorMode::VolumetricFlow, Some(&cfg));
        assert!(lo == 0.0 && hi >= 20.0);
    }

    #[test]
    fn orbit_camera_zoom_by_factor_scales_clamps_and_ignores_invalid() {
        let mut cam = OrbitCamera::default();
        let d0 = cam.distance;
        cam.zoom_by_factor(0.5);
        assert!((cam.distance - d0 * 0.5).abs() < 1e-3);
        let d1 = cam.distance;
        cam.zoom_by_factor(f32::NAN);
        cam.zoom_by_factor(-2.0);
        cam.zoom_by_factor(0.0);
        assert_eq!(cam.distance, d1);
        cam.zoom_by_factor(1000.0);
        assert_eq!(cam.distance, 2000.0);
        cam.zoom_by_factor(1e-6);
        assert_eq!(cam.distance, 8.0);
    }
}
