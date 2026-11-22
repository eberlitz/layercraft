//! Headless Z-buffered CPU rasterizer and PNG encoder for CLI and MCP screenshots.

use glam::{Mat4, Vec3, Vec4};
use layercraft_config::PrintConfig;
use layercraft_geom::TriangleMesh;
use layercraft_slice::SlicedPlate;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    CameraPreset, ColorMode, LineVertex, OrbitCamera, SceneVertex, ViewportError, build_bed_grid,
    build_mesh_vertices, build_toolpath_mesh, path_color,
};

/// Render target style for [`render_png`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RenderMode {
    Plate,
    #[default]
    Preview,
    #[serde(alias = "layer_topdown")]
    LayerTopDown,
}

/// Parameters controlling [`render_png`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RenderRequest {
    pub width: u32,
    pub height: u32,
    pub mode: RenderMode,
    pub camera_preset: CameraPreset,
    pub max_layer: Option<usize>,
    pub single_layer_only: bool,
    pub color_mode: ColorMode,
}

impl Default for RenderRequest {
    fn default() -> Self {
        Self {
            width: 800,
            height: 600,
            mode: RenderMode::Preview,
            camera_preset: CameraPreset::Isometric,
            max_layer: None,
            single_layer_only: false,
            color_mode: ColorMode::Feature,
        }
    }
}

/// Render the current build plate, mesh, or sliced toolpaths into an RGB8 PNG byte vector.
pub fn render_png(
    mesh: Option<&TriangleMesh>,
    plate: Option<&SlicedPlate>,
    config: &PrintConfig,
    req: &RenderRequest,
) -> Result<Vec<u8>, ViewportError> {
    if req.width == 0 || req.height == 0 {
        return Err(ViewportError::InvalidDimensions {
            width: req.width,
            height: req.height,
        });
    }
    let w = req.width.clamp(64, 1920) as usize;
    let h = req.height.clamp(64, 1920) as usize;

    let mut color_buf = vec![0u8; w * h * 3];
    // Vertical background gradient matching dark slicer studio aesthetic.
    for y in 0..h {
        let t = (y as f32) / ((h.saturating_sub(1)).max(1) as f32);
        let r = (24.0 + t * 10.0) as u8;
        let g = (27.0 + t * 12.0) as u8;
        let b = (34.0 + t * 16.0) as u8;
        for x in 0..w {
            let idx = (y * w + x) * 3;
            if let Some(px) = color_buf.get_mut(idx..idx + 3) {
                px.copy_from_slice(&[r, g, b]);
            }
        }
    }

    if req.mode == RenderMode::LayerTopDown {
        if let Some(sliced) = plate {
            render_layer_topdown(sliced, config, req, w, h, &mut color_buf);
        }
        return encode_png(w as u32, h as u32, &color_buf);
    }

    let mut depth_buf = vec![1.0_f32; w * h];
    let bed_w = config.machine.bed_width_mm as f32;
    let bed_d = config.machine.bed_depth_mm as f32;
    let cam = match mesh.and_then(TriangleMesh::aabb) {
        Some(aabb) => OrbitCamera::framed_on_aabb(req.camera_preset, aabb, bed_w, bed_d),
        None => OrbitCamera::from_preset(req.camera_preset, bed_w, bed_d, 20.0),
    };
    let vp = cam.view_proj((w as f32) / (h as f32));

    let bed_lines = build_bed_grid(config);
    rasterize_lines(&bed_lines, vp, w, h, &mut color_buf, &mut depth_buf);

    match req.mode {
        RenderMode::Plate => {
            if let Some(m) = mesh {
                let verts = build_mesh_vertices(m, [0.35, 0.64, 0.92, 1.0]);
                rasterize_triangles(&verts, vp, w, h, &mut color_buf, &mut depth_buf);
            }
        }
        RenderMode::Preview => {
            if let Some(sliced) = plate {
                let geom = build_toolpath_mesh(sliced, req.color_mode);
                let total = geom.layer_ranges.len();
                if total > 0 {
                    let top_idx = req.max_layer.unwrap_or(total - 1).min(total - 1);
                    let start_idx = if req.single_layer_only { top_idx } else { 0 };
                    let start_v = geom
                        .layer_ranges
                        .get(start_idx)
                        .map(|r| r.start as usize)
                        .unwrap_or(0);
                    let end_v = geom
                        .layer_ranges
                        .get(top_idx)
                        .map(|r| r.end as usize)
                        .unwrap_or(0);
                    if let Some(slice) = geom.vertices.get(start_v..end_v) {
                        rasterize_triangles(slice, vp, w, h, &mut color_buf, &mut depth_buf);
                    }
                }
            } else if let Some(m) = mesh {
                let verts = build_mesh_vertices(m, [0.35, 0.64, 0.92, 1.0]);
                rasterize_triangles(&verts, vp, w, h, &mut color_buf, &mut depth_buf);
            }
        }
        RenderMode::LayerTopDown => {}
    }

    encode_png(w as u32, h as u32, &color_buf)
}

fn render_layer_topdown(
    plate: &SlicedPlate,
    config: &PrintConfig,
    req: &RenderRequest,
    w: usize,
    h: usize,
    color_buf: &mut [u8],
) {
    let total = plate.layers.len();
    if total == 0 {
        return;
    }
    let layer_idx = req.max_layer.unwrap_or(total - 1).min(total - 1);
    let Some(layer) = plate.layers.get(layer_idx) else {
        return;
    };

    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for path in &layer.paths {
        for pt in &path.points {
            let (x, y) = pt.to_mm();
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if !min_x.is_finite() {
        min_x = 0.0;
        min_y = 0.0;
        max_x = config.machine.bed_width_mm;
        max_y = config.machine.bed_depth_mm;
    }

    let pad = 2.0;
    let span_x = (max_x - min_x + 2.0 * pad).max(1.0);
    let span_y = (max_y - min_y + 2.0 * pad).max(1.0);
    let scale = ((w as f64 - 24.0) / span_x).min((h as f64 - 24.0) / span_y);
    let cx = 0.5 * (min_x + max_x);
    let cy = 0.5 * (min_y + max_y);

    let to_px = |x: f64, y: f64| -> (i32, i32) {
        let px = ((w as f64) * 0.5 + (x - cx) * scale).round() as i32;
        let py = ((h as f64) * 0.5 - (y - cy) * scale).round() as i32;
        (px, py)
    };

    for path in &layer.paths {
        let n = path.points.len();
        if n < 2 {
            continue;
        }
        let col = path_color(path, req.color_mode, layer_idx, total);
        let rgb = [
            (col[0] * 255.0) as u8,
            (col[1] * 255.0) as u8,
            (col[2] * 255.0) as u8,
        ];
        let segs = if path.closed { n } else { n - 1 };
        for i in 0..segs {
            let (Some(&pa), Some(&pb)) = (path.points.get(i), path.points.get((i + 1) % n)) else {
                continue;
            };
            let radius = ((0.45 * path.segment_width_mm(i) * scale).round() as i32).clamp(1, 8);
            let (ax, ay) = pa.to_mm();
            let (bx, by) = pb.to_mm();
            let (x0, y0) = to_px(ax, ay);
            let (x1, y1) = to_px(bx, by);
            draw_thick_line_2d(w, h, color_buf, x0, y0, x1, y1, radius, rgb);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_thick_line_2d(
    w: usize,
    h: usize,
    buf: &mut [u8],
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    radius: i32,
    rgb: [u8; 3],
) {
    let steps = (x1 - x0).abs().max((y1 - y0).abs()).max(1);
    for s in 0..=steps {
        let t = (s as f32) / (steps as f32);
        let cx = (x0 as f32 + (x1 - x0) as f32 * t).round() as i32;
        let cy = (y0 as f32 + (y1 - y0) as f32 * t).round() as i32;
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx * dx + dy * dy > radius * radius {
                    continue;
                }
                let px = cx + dx;
                let py = cy + dy;
                if px >= 0 && py >= 0 && (px as usize) < w && (py as usize) < h {
                    let idx = ((py as usize) * w + (px as usize)) * 3;
                    if let Some(dst) = buf.get_mut(idx..idx + 3) {
                        dst.copy_from_slice(&rgb);
                    }
                }
            }
        }
    }
}

fn project(vp: Mat4, p: [f32; 3], w: usize, h: usize) -> Option<[f32; 3]> {
    let clip: Vec4 = vp * Vec4::new(p[0], p[1], p[2], 1.0);
    if clip.w <= 1e-4 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    if ndc.z < 0.0 || ndc.z > 1.0 {
        return None;
    }
    let sx = (ndc.x * 0.5 + 0.5) * (w as f32);
    let sy = (1.0 - (ndc.y * 0.5 + 0.5)) * (h as f32);
    Some([sx, sy, ndc.z])
}

fn rasterize_lines(
    lines: &[LineVertex],
    vp: Mat4,
    w: usize,
    h: usize,
    color_buf: &mut [u8],
    depth_buf: &mut [f32],
) {
    let (pairs, _) = lines.as_chunks::<2>();
    for &[a, b] in pairs {
        let (Some(pa), Some(pb)) = (project(vp, a.pos, w, h), project(vp, b.pos, w, h)) else {
            continue;
        };
        let dx = pb[0] - pa[0];
        let dy = pb[1] - pa[1];
        let steps = dx.abs().max(dy.abs()).ceil().clamp(1.0, 2000.0) as usize;
        let rgb = [
            (a.color[0].clamp(0.0, 1.0) * 255.0) as u8,
            (a.color[1].clamp(0.0, 1.0) * 255.0) as u8,
            (a.color[2].clamp(0.0, 1.0) * 255.0) as u8,
        ];
        for s in 0..=steps {
            let t = (s as f32) / (steps as f32);
            let px = (pa[0] + dx * t).round() as i32;
            let py = (pa[1] + dy * t).round() as i32;
            let pz = pa[2] + (pb[2] - pa[2]) * t;
            if px >= 0 && py >= 0 && (px as usize) < w && (py as usize) < h {
                let p_idx = (py as usize) * w + (px as usize);
                if let Some(d) = depth_buf.get_mut(p_idx)
                    && pz <= *d + 1e-4
                {
                    *d = pz;
                    if let Some(dst) = color_buf.get_mut(p_idx * 3..p_idx * 3 + 3) {
                        dst.copy_from_slice(&rgb);
                    }
                }
            }
        }
    }
}

fn edge_fn(ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32) -> f32 {
    (px - ax) * (by - ay) - (py - ay) * (bx - ax)
}

fn rasterize_triangles(
    verts: &[SceneVertex],
    vp: Mat4,
    w: usize,
    h: usize,
    color_buf: &mut [u8],
    depth_buf: &mut [f32],
) {
    let light_dir = Vec3::new(0.45, -0.35, 0.82).normalize();
    let (tris, _) = verts.as_chunks::<3>();
    for &[v0, v1, v2] in tris {
        let (Some(p0), Some(p1), Some(p2)) = (
            project(vp, v0.pos, w, h),
            project(vp, v1.pos, w, h),
            project(vp, v2.pos, w, h),
        ) else {
            continue;
        };

        let area = edge_fn(p0[0], p0[1], p1[0], p1[1], p2[0], p2[1]);
        if area.abs() < 1e-4 {
            continue;
        }

        let min_x = p0[0].min(p1[0]).min(p2[0]).floor().max(0.0) as i32;
        let max_x = p0[0].max(p1[0]).max(p2[0]).ceil().min((w - 1) as f32) as i32;
        let min_y = p0[1].min(p1[1]).min(p2[1]).floor().max(0.0) as i32;
        let max_y = p0[1].max(p1[1]).max(p2[1]).ceil().min((h - 1) as f32) as i32;

        let n = Vec3::from_array(v0.normal).normalize_or_zero();
        let diff = n.dot(light_dir).abs();
        let hemi = 0.5 + 0.5 * n.z;
        let shade = (0.32 + 0.50 * diff + 0.18 * hemi).clamp(0.15, 1.0);
        let rgb = [
            (v0.color[0] * shade * 255.0).clamp(0.0, 255.0) as u8,
            (v0.color[1] * shade * 255.0).clamp(0.0, 255.0) as u8,
            (v0.color[2] * shade * 255.0).clamp(0.0, 255.0) as u8,
        ];

        for py in min_y..=max_y {
            let fy = (py as f32) + 0.5;
            for px in min_x..=max_x {
                let fx = (px as f32) + 0.5;
                let w0 = edge_fn(p1[0], p1[1], p2[0], p2[1], fx, fy) / area;
                let w1 = edge_fn(p2[0], p2[1], p0[0], p0[1], fx, fy) / area;
                let w2 = edge_fn(p0[0], p0[1], p1[0], p1[1], fx, fy) / area;
                if w0 >= -1e-4 && w1 >= -1e-4 && w2 >= -1e-4 {
                    let z = w0 * p0[2] + w1 * p1[2] + w2 * p2[2];
                    let p_idx = (py as usize) * w + (px as usize);
                    if let Some(d) = depth_buf.get_mut(p_idx)
                        && z < *d
                    {
                        *d = z;
                        if let Some(dst) = color_buf.get_mut(p_idx * 3..p_idx * 3 + 3) {
                            dst.copy_from_slice(&rgb);
                        }
                    }
                }
            }
        }
    }
}

fn encode_png(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>, ViewportError> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| ViewportError::PngEncode(e.to_string()))?;
        writer
            .write_image_data(rgb)
            .map_err(|e| ViewportError::PngEncode(e.to_string()))?;
    }
    Ok(out)
}
