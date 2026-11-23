//! Embedded PNG thumbnail renderer and base64 G-code comment block formatter.

use std::fmt::Write as _;

use base64::Engine as _;
use layercraft_config::PrintConfig;
use layercraft_slice::{ExtrusionRole, SlicedPlate};

use crate::GcodeError;

struct OrthoIsoProj {
    center: [f64; 3],
    right: [f64; 3],
    up: [f64; 3],
    forward: [f64; 3],
    scale: f64,
    half_w: f64,
    half_h: f64,
}

impl OrthoIsoProj {
    fn from_plate(plate: &SlicedPlate, config: &PrintConfig, width: u32, height: u32) -> Self {
        let yaw = -45.0_f64.to_radians();
        let pitch = 35.264_f64.to_radians();
        let cy = yaw.cos();
        let sy = yaw.sin();
        let cp = pitch.cos();
        let sp = pitch.sin();

        let right = [-sy, cy, 0.0];
        let forward = [cp * cy, cp * sy, sp];
        let up = [-sp * cy, -sp * sy, cp];

        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        let mut any = false;
        for layer in &plate.layers {
            let z = layer.z_top_mm;
            for path in &layer.paths {
                for pt in &path.points {
                    let (x, y) = pt.to_mm();
                    any = true;
                    min[0] = min[0].min(x);
                    min[1] = min[1].min(y);
                    min[2] = min[2].min(0.0).min(z);
                    max[0] = max[0].max(x);
                    max[1] = max[1].max(y);
                    max[2] = max[2].max(z);
                }
            }
        }
        if !any {
            min = [0.0, 0.0, 0.0];
            max = [
                config.machine.bed_width_mm,
                config.machine.bed_depth_mm,
                20.0,
            ];
        }

        let center = [
            0.5 * (min[0] + max[0]),
            0.5 * (min[1] + max[1]),
            0.5 * (min[2] + max[2]),
        ];

        let mut u_min = f64::INFINITY;
        let mut u_max = f64::NEG_INFINITY;
        let mut v_min = f64::INFINITY;
        let mut v_max = f64::NEG_INFINITY;
        for &x in &[min[0], max[0]] {
            for &y in &[min[1], max[1]] {
                for &z in &[min[2], max[2]] {
                    let d = [x - center[0], y - center[1], z - center[2]];
                    let px = dot3(d, right);
                    let py = dot3(d, up);
                    u_min = u_min.min(px);
                    u_max = u_max.max(px);
                    v_min = v_min.min(py);
                    v_max = v_max.max(py);
                }
            }
        }

        let span_x = (u_max - u_min).max(1.0);
        let span_y = (v_max - v_min).max(1.0);
        let w_f = f64::from(width.max(1));
        let h_f = f64::from(height.max(1));
        let scale = (0.76 * w_f / span_x).min(0.76 * h_f / span_y);

        Self {
            center,
            right,
            up,
            forward,
            scale,
            half_w: 0.5 * w_f,
            half_h: 0.5 * h_f,
        }
    }

    fn project(&self, p: [f64; 3]) -> [f64; 3] {
        let d = [
            p[0] - self.center[0],
            p[1] - self.center[1],
            p[2] - self.center[2],
        ];
        let sx = self.half_w + dot3(d, self.right) * self.scale;
        let sy = self.half_h - dot3(d, self.up) * self.scale;
        // Larger `forward` dot product means closer to the camera, so smaller depth wins Z-buffer.
        let depth = -dot3(d, self.forward);
        [sx, sy, depth]
    }
}

fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn role_rgb(role: ExtrusionRole, z_frac: f64) -> [u8; 3] {
    let base: [u8; 3] = match role {
        ExtrusionRole::OuterWall => [0x14, 0xB8, 0xA6],
        ExtrusionRole::InnerWall => [0x5B, 0x9B, 0xD5],
        ExtrusionRole::OverhangWall => [0xF5, 0x9E, 0x0B],
        ExtrusionRole::Bridge => [0x22, 0xD3, 0xEE],
        ExtrusionRole::TopSurface => [0x94, 0xA3, 0xB8],
        ExtrusionRole::BottomSurface => [0x64, 0x74, 0x8B],
        ExtrusionRole::InternalSolidInfill => [0x7C, 0x8B, 0xA1],
        ExtrusionRole::SparseInfill => [0x34, 0xD3, 0x99],
        ExtrusionRole::Support | ExtrusionRole::SupportInterface => [0x6E, 0x76, 0x81],
        ExtrusionRole::Ironing => [0xE2, 0xE8, 0xF0],
        ExtrusionRole::Skirt | ExtrusionRole::Brim => [0x0F, 0x76, 0x6E],
    };
    let shade = 0.78 + 0.22 * z_frac.clamp(0.0, 1.0);
    [
        ((f64::from(base[0]) * shade).clamp(0.0, 255.0)) as u8,
        ((f64::from(base[1]) * shade).clamp(0.0, 255.0)) as u8,
        ((f64::from(base[2]) * shade).clamp(0.0, 255.0)) as u8,
    ]
}

struct ThumbCanvas {
    w: usize,
    h: usize,
    color_buf: Vec<u8>,
    depth_buf: Vec<f64>,
}

impl ThumbCanvas {
    fn new(width: u32, height: u32) -> Self {
        let w = width as usize;
        let h = height as usize;
        let mut color_buf = vec![0u8; w * h * 3];
        for y in 0..h {
            let t = (y as f64) / ((h.saturating_sub(1)).max(1) as f64);
            let r = (24.0 + (15.0 - 24.0) * t) as u8;
            let g = (29.0 + (19.0 - 29.0) * t) as u8;
            let b = (40.0 + (28.0 - 40.0) * t) as u8;
            for x in 0..w {
                let off = (y * w + x) * 3;
                if let Some(dst) = color_buf.get_mut(off..off + 3) {
                    dst.copy_from_slice(&[r, g, b]);
                }
            }
        }
        Self {
            w,
            h,
            color_buf,
            depth_buf: vec![f64::INFINITY; w * h],
        }
    }

    fn rasterize_line(&mut self, a: [f64; 3], b: [f64; 3], rgb: [u8; 3], radius: i32) {
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let steps = dx.abs().max(dy.abs()).ceil().clamp(1.0, 2000.0) as usize;
        for s in 0..=steps {
            let t = (s as f64) / (steps as f64);
            let cx = (a[0] + dx * t).round() as i32;
            let cy = (a[1] + dy * t).round() as i32;
            let depth = a[2] + (b[2] - a[2]) * t;
            for oy in -radius..=radius {
                for ox in -radius..=radius {
                    let px = cx + ox;
                    let py = cy + oy;
                    if px >= 0 && py >= 0 && (px as usize) < self.w && (py as usize) < self.h {
                        let idx = (py as usize) * self.w + (px as usize);
                        if let Some(d_slot) = self.depth_buf.get_mut(idx)
                            && depth <= *d_slot + 0.05
                        {
                            *d_slot = depth;
                            if let Some(dst) = self.color_buf.get_mut(idx * 3..idx * 3 + 3) {
                                dst.copy_from_slice(&rgb);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Render an isometric studio RGB8 PNG thumbnail of the sliced plate toolpaths.
pub fn render_gcode_thumbnail_png(
    plate: &SlicedPlate,
    config: &PrintConfig,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, GcodeError> {
    if !(16..=640).contains(&width) || !(16..=640).contains(&height) {
        return Err(GcodeError::InvalidParameter("machine.thumbnails"));
    }

    let mut canvas = ThumbCanvas::new(width, height);
    let proj = OrthoIsoProj::from_plate(plate, config, width, height);

    // Subtle build-plate outline at Z = 0.
    let bw = config.machine.bed_width_mm;
    let bd = config.machine.bed_depth_mm;
    let bed_corners = [
        proj.project([0.0, 0.0, 0.0]),
        proj.project([bw, 0.0, 0.0]),
        proj.project([bw, bd, 0.0]),
        proj.project([0.0, bd, 0.0]),
    ];
    for i in 0..4 {
        if let (Some(&ca), Some(&cb)) = (bed_corners.get(i), bed_corners.get((i + 1) % 4)) {
            let col = if i == 0 {
                [0x14, 0xB8, 0xA6]
            } else {
                [0x33, 0x3D, 0x4F]
            };
            canvas.rasterize_line(ca, cb, col, 0);
        }
    }

    let max_z = plate
        .layers
        .last()
        .map(|l| l.z_top_mm)
        .unwrap_or(1.0)
        .max(1e-3);
    let line_radius = i32::from(width >= 200);
    for layer in &plate.layers {
        let z = layer.z_top_mm;
        let z_frac = z / max_z;
        for path in &layer.paths {
            if path.points.len() < 2 {
                continue;
            }
            let rgb = role_rgb(path.role, z_frac);
            for pair in path.points.windows(2) {
                if let (Some(&p0), Some(&p1)) = (pair.first(), pair.get(1)) {
                    let (x0, y0) = p0.to_mm();
                    let (x1, y1) = p1.to_mm();
                    canvas.rasterize_line(
                        proj.project([x0, y0, z]),
                        proj.project([x1, y1, z]),
                        rgb,
                        line_radius,
                    );
                }
            }
            if path.closed
                && let (Some(&first), Some(&last)) = (path.points.first(), path.points.last())
                && first != last
            {
                let (x0, y0) = last.to_mm();
                let (x1, y1) = first.to_mm();
                canvas.rasterize_line(
                    proj.project([x0, y0, z]),
                    proj.project([x1, y1, z]),
                    rgb,
                    line_radius,
                );
            }
        }
    }

    let mut png_out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|_| GcodeError::FormatError)?;
        writer
            .write_image_data(&canvas.color_buf)
            .map_err(|_| GcodeError::FormatError)?;
    }
    Ok(png_out)
}

/// Format raw PNG bytes into a Prusa / Klipper / Mainsail G-code thumbnail comment block.
pub fn format_thumbnail_block(
    width: u32,
    height: u32,
    png_bytes: &[u8],
) -> Result<String, GcodeError> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes);
    let mut out = String::with_capacity(b64.len() + 128);
    writeln!(out, "; thumbnail begin {width}x{height} {}", b64.len())
        .map_err(|_| GcodeError::FormatError)?;
    for chunk in b64.as_bytes().chunks(78) {
        let s = std::str::from_utf8(chunk).map_err(|_| GcodeError::FormatError)?;
        writeln!(out, "; {s}").map_err(|_| GcodeError::FormatError)?;
    }
    writeln!(out, "; thumbnail end").map_err(|_| GcodeError::FormatError)?;
    Ok(out)
}
