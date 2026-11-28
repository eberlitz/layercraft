//! Headless Z-buffered CPU rasterizer and PNG encoder for CLI and MCP screenshots.

use glam::{Mat4, Vec3, Vec4};
use layercraft_config::PrintConfig;
use layercraft_geom::TriangleMesh;
use layercraft_slice::SlicedPlate;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    CameraPreset, ColorMode, LineVertex, OrbitCamera, SceneVertex, ViewportError, build_bed_grid,
    build_mesh_vertices, build_toolpath_instances_with_config, expand_bead_instances_to_vertices,
    path_color,
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

/// Result of comparing two equal-dimension PNG images pixel-by-pixel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PngDiffResult {
    /// 3-panel horizontal RGB8 composite PNG (`width * 3` by `height`): A | B | Diff heatmap.
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub diff_pixels: usize,
    pub total_pixels: usize,
    pub diff_percent: f64,
    pub mean_abs_diff: f64,
}

/// Render the current build plate, mesh, or sliced toolpaths into an RGB8 PNG byte vector.
pub fn render_png(
    mesh: Option<&TriangleMesh>,
    plate: Option<&SlicedPlate>,
    config: &PrintConfig,
    req: &RenderRequest,
) -> Result<Vec<u8>, ViewportError> {
    render_png_with_camera(mesh, plate, config, req, None)
}

/// Render the plate/mesh/toolpaths using an optional explicit camera (falling back to preset).
pub fn render_png_with_camera(
    mesh: Option<&TriangleMesh>,
    plate: Option<&SlicedPlate>,
    config: &PrintConfig,
    req: &RenderRequest,
    custom_camera: Option<&OrbitCamera>,
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
    let cam = if let Some(custom) = custom_camera {
        *custom
    } else {
        match mesh.and_then(TriangleMesh::aabb) {
            Some(aabb) => OrbitCamera::framed_on_aabb(req.camera_preset, aabb, bed_w, bed_d),
            None => OrbitCamera::from_preset(req.camera_preset, bed_w, bed_d, 20.0),
        }
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
                let insts =
                    build_toolpath_instances_with_config(sliced, req.color_mode, Some(config));
                let total = insts.layer_ranges.len();
                if total > 0 {
                    let top_idx = req.max_layer.unwrap_or(total - 1).min(total - 1);
                    let start_idx = if req.single_layer_only { top_idx } else { 0 };
                    let start_i = insts
                        .layer_ranges
                        .get(start_idx)
                        .map(|r| r.start as usize)
                        .unwrap_or(0);
                    let end_i = insts
                        .layer_ranges
                        .get(top_idx)
                        .map(|r| r.end as usize)
                        .unwrap_or(0);
                    if let Some(slice) = insts.beads.get(start_i..end_i) {
                        let verts = expand_bead_instances_to_vertices(slice);
                        rasterize_triangles(&verts, vp, w, h, &mut color_buf, &mut depth_buf);
                    }
                    let s_start = insts
                        .seam_layer_ranges
                        .get(start_idx)
                        .map(|r| r.start as usize)
                        .unwrap_or(0);
                    let s_end = insts
                        .seam_layer_ranges
                        .get(top_idx)
                        .map(|r| r.end as usize)
                        .unwrap_or(0);
                    if let Some(seam_slice) = insts.seam_verts.get(s_start..s_end) {
                        rasterize_triangles(seam_slice, vp, w, h, &mut color_buf, &mut depth_buf);
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

        let n = (Vec3::from_array(v0.normal)
            + Vec3::from_array(v1.normal)
            + Vec3::from_array(v2.normal))
        .normalize_or_zero();
        let diff = n.dot(light_dir).abs();
        let hemi = 0.5 + 0.5 * n.z;
        let shade = (0.30 + 0.52 * diff + 0.18 * hemi).clamp(0.15, 1.0);
        let c0 = v0.color;
        let c1 = v1.color;
        let c2 = v2.color;
        let avg_r = (c0[0] + c1[0] + c2[0]) / 3.0;
        let avg_g = (c0[1] + c1[1] + c2[1]) / 3.0;
        let avg_b = (c0[2] + c1[2] + c2[2]) / 3.0;
        let rgb = [
            (avg_r * shade * 255.0).clamp(0.0, 255.0) as u8,
            (avg_g * shade * 255.0).clamp(0.0, 255.0) as u8,
            (avg_b * shade * 255.0).clamp(0.0, 255.0) as u8,
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

fn decode_png_rgb(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), ViewportError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| ViewportError::PngDecode(e.to_string()))?;
    let buf_size = reader
        .output_buffer_size()
        .ok_or_else(|| ViewportError::PngDecode("image buffer size overflow".to_owned()))?;
    let mut raw = vec![0u8; buf_size];
    let info = reader
        .next_frame(&mut raw)
        .map_err(|e| ViewportError::PngDecode(e.to_string()))?;
    let data = raw
        .get(..info.buffer_size())
        .ok_or_else(|| ViewportError::PngDecode("truncated frame buffer".to_owned()))?;
    let rgb = match info.color_type {
        png::ColorType::Rgb => data.to_vec(),
        png::ColorType::Rgba => data
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|&[r, g, b, _]| [r, g, b])
            .collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|&g| [g, g, g]).collect(),
        png::ColorType::GrayscaleAlpha => data
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[g, _]| [g, g, g])
            .collect(),
        other => {
            return Err(ViewportError::PngDecode(format!(
                "unsupported PNG color type: {other:?}"
            )));
        }
    };
    Ok((info.width, info.height, rgb))
}

/// Compare two equal-dimension PNGs and produce a 3-panel A | B | Diff composite PNG.
pub fn compare_png(png_a: &[u8], png_b: &[u8]) -> Result<PngDiffResult, ViewportError> {
    let (wa, ha, rgb_a) = decode_png_rgb(png_a)?;
    let (wb, hb, rgb_b) = decode_png_rgb(png_b)?;
    if wa == 0 || ha == 0 || wa != wb || ha != hb {
        return Err(ViewportError::InvalidDimensions {
            width: wb,
            height: hb,
        });
    }

    let w = wa as usize;
    let h = ha as usize;
    let total_pixels = w * h;
    let panel_bytes = total_pixels * 3;
    if rgb_a.len() < panel_bytes || rgb_b.len() < panel_bytes {
        return Err(ViewportError::PngDecode(
            "decoded RGB buffer smaller than dimensions".to_owned(),
        ));
    }

    let out_w = w * 3;
    let mut composite = vec![0u8; out_w * h * 3];
    let mut diff_pixels = 0usize;
    let mut sum_abs_diff = 0.0_f64;

    for y in 0..h {
        for x in 0..w {
            let src_idx = (y * w + x) * 3;
            let (Some(pa), Some(pb)) = (
                rgb_a.get(src_idx..src_idx + 3),
                rgb_b.get(src_idx..src_idx + 3),
            ) else {
                continue;
            };
            let ra = pa.first().copied().unwrap_or(0);
            let ga = pa.get(1).copied().unwrap_or(0);
            let ba = pa.get(2).copied().unwrap_or(0);
            let rb = pb.first().copied().unwrap_or(0);
            let gb = pb.get(1).copied().unwrap_or(0);
            let bb = pb.get(2).copied().unwrap_or(0);

            let dr = ra.abs_diff(rb);
            let dg = ga.abs_diff(gb);
            let db = ba.abs_diff(bb);
            let max_d = dr.max(dg).max(db);
            sum_abs_diff += f64::from(dr) + f64::from(dg) + f64::from(db);
            if max_d > 4 {
                diff_pixels += 1;
            }

            let left_col = if x + 1 == w {
                [62, 70, 82]
            } else {
                [ra, ga, ba]
            };
            let mid_col = if x + 1 == w {
                [62, 70, 82]
            } else {
                [rb, gb, bb]
            };

            let diff_col = if max_d > 4 {
                let t = ((f32::from(max_d)) / 64.0).clamp(0.35, 1.0);
                [
                    (40.0 + 215.0 * t) as u8,
                    (20.0 + 44.0 * (1.0 - t)) as u8,
                    (60.0 + 140.0 * t) as u8,
                ]
            } else {
                let luma = ((0.299 * f32::from(ra) + 0.587 * f32::from(ga) + 0.114 * f32::from(ba))
                    * 0.30) as u8;
                [luma, luma, luma]
            };

            let dst_a = (y * out_w + x) * 3;
            let dst_b = (y * out_w + (w + x)) * 3;
            let dst_d = (y * out_w + (w * 2 + x)) * 3;
            if let Some(s) = composite.get_mut(dst_a..dst_a + 3) {
                s.copy_from_slice(&left_col);
            }
            if let Some(s) = composite.get_mut(dst_b..dst_b + 3) {
                s.copy_from_slice(&mid_col);
            }
            if let Some(s) = composite.get_mut(dst_d..dst_d + 3) {
                s.copy_from_slice(&diff_col);
            }
        }
    }

    let png = encode_png((out_w) as u32, ha, &composite)?;
    let denom = (total_pixels * 3).max(1) as f64;
    Ok(PngDiffResult {
        png,
        width: out_w as u32,
        height: ha,
        diff_pixels,
        total_pixels,
        diff_percent: (diff_pixels as f64 / total_pixels.max(1) as f64) * 100.0,
        mean_abs_diff: sum_abs_diff / denom,
    })
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

    #[test]
    fn compare_png_reports_zero_diff_for_identical_and_detects_changes() {
        let cfg = PrintConfig::default();
        let mesh = test_cube_mesh();
        let req_iso = RenderRequest {
            width: 160,
            height: 120,
            mode: RenderMode::Plate,
            camera_preset: CameraPreset::Isometric,
            ..RenderRequest::default()
        };
        let req_top = RenderRequest {
            camera_preset: CameraPreset::Top,
            ..req_iso.clone()
        };

        let png_a = render_png(Some(&mesh), None, &cfg, &req_iso).unwrap();
        let same = compare_png(&png_a, &png_a).unwrap();
        assert_eq!(same.width, 480);
        assert_eq!(same.height, 120);
        assert_eq!(same.diff_pixels, 0);
        assert!((same.diff_percent - 0.0).abs() < 1e-9);

        let png_b = render_png(Some(&mesh), None, &cfg, &req_top).unwrap();
        let diff = compare_png(&png_a, &png_b).unwrap();
        assert!(diff.diff_pixels > 100);
        assert!(diff.diff_percent > 0.5);
        assert!(diff.png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }
}
