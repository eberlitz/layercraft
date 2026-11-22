//! Deterministic FDM slicing pipeline: slice -> perimeters -> skins -> infill -> adhesion.

use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI, SQRT_2, TAU};

use layercraft_clip::{
    ClipError, JoinType, clip_lines, clip_polylines, clip_polylines_difference, difference,
    intersection, offset, union,
};
use layercraft_config::{InfillPattern, PrintConfig};
use layercraft_geom::{Aabb2, ExPolygon, GeomError, Point2, Polygon, TriangleMesh, mm_to_nm};
use layercraft_mesh::{MeshError, RawSliceLayer, prepare_for_plate, slice_mesh};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors produced by the slicing pipeline.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum SliceError {
    #[error(transparent)]
    Geom(#[from] GeomError),
    #[error(transparent)]
    Clip(#[from] ClipError),
    #[error(transparent)]
    Mesh(#[from] MeshError),
    #[error("configuration error: {0}")]
    Config(#[from] layercraft_config::ConfigError),
}

/// Classification of an extruded toolpath segment.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum ExtrusionRole {
    OuterWall,
    InnerWall,
    OverhangWall,
    BottomSurface,
    TopSurface,
    InternalSolidInfill,
    Bridge,
    SparseInfill,
    Skirt,
    Brim,
}

impl ExtrusionRole {
    /// Human-readable feature tag matching standard slicer G-code `;TYPE:` conventions.
    pub fn label(self) -> &'static str {
        match self {
            Self::OuterWall => "Outer wall",
            Self::InnerWall => "Inner wall",
            Self::OverhangWall => "Overhang wall",
            Self::BottomSurface => "Bottom surface",
            Self::TopSurface => "Top surface",
            Self::InternalSolidInfill => "Internal solid infill",
            Self::Bridge => "Bridge",
            Self::SparseInfill => "Sparse infill",
            Self::Skirt => "Skirt",
            Self::Brim => "Brim",
        }
    }
}

fn default_flow_ratio() -> f64 {
    1.0
}

/// A 2D toolpath polyline at a single layer height.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExtrusionPath {
    pub role: ExtrusionRole,
    pub width_mm: f64,
    pub height_mm: f64,
    pub speed_mm_s: f64,
    #[serde(default = "default_flow_ratio")]
    pub flow_ratio: f64,
    pub points: Vec<Point2>,
    pub closed: bool,
}

/// One sliced layer with cross-section regions and generated extrusion paths.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SlicedLayer {
    pub index: usize,
    pub z_top_mm: f64,
    pub height_mm: f64,
    pub slice_regions: Vec<ExPolygon>,
    pub paths: Vec<ExtrusionPath>,
    #[serde(default)]
    pub fan_percent: u8,
    #[serde(default)]
    pub estimated_time_s: f64,
}

/// Output of slicing a single plate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SlicedPlate {
    pub layers: Vec<SlicedLayer>,
}

/// Stadium cross-sectional area (`mm^2`) of an extrusion bead of width `w` and height `h`.
#[inline]
pub fn bead_area_mm2(width_mm: f64, height_mm: f64) -> f64 {
    if width_mm >= height_mm {
        (width_mm - height_mm) * height_mm + PI * (0.5 * height_mm) * (0.5 * height_mm)
    } else {
        PI * (0.5 * width_mm) * (0.5 * width_mm)
    }
}

/// Stadium cross-section line spacing between adjacent beads of width `w` and height `h`.
#[inline]
pub fn stadium_spacing_mm(width_mm: f64, height_mm: f64) -> f64 {
    let overlap = height_mm * (1.0 - FRAC_PI_4);
    (width_mm - overlap).max(0.05 * width_mm)
}

/// Run the full slicing pipeline for a single mesh onto the configured bed.
pub fn slice_single_mesh(
    mesh: &TriangleMesh,
    config: &PrintConfig,
) -> Result<SlicedPlate, SliceError> {
    config.validate()?;
    let prepared = prepare_for_plate(
        mesh,
        config.machine.bed_width_mm,
        config.machine.bed_depth_mm,
    )?;
    let raw_layers = slice_mesh(
        &prepared,
        config.process.first_layer_height_mm,
        config.process.layer_height_mm,
    )?;
    slice_from_raw_layers(&raw_layers, config)
}

/// Execute perimeters, skin detection, infill, and adhesion from pre-sliced Z layers.
pub fn slice_from_raw_layers(
    raw_layers: &[RawSliceLayer],
    config: &PrintConfig,
) -> Result<SlicedPlate, SliceError> {
    config.validate()?;
    let n = raw_layers.len();

    let mut per_layer_walls: Vec<Vec<ExtrusionPath>> = Vec::with_capacity(n);
    let mut per_layer_infill_areas: Vec<Vec<ExPolygon>> = Vec::with_capacity(n);

    // Stage 2: Perimeters & infill boundary extraction.
    for raw in raw_layers {
        let h = raw.plane.height_mm;
        let (walls, infill_area) = build_perimeters(&raw.regions, raw.plane.index, h, config)?;
        per_layer_walls.push(walls);
        per_layer_infill_areas.push(infill_area);
    }

    // Stage 3: Top / bottom exposed surface detection & shell propagation.
    let mut bottom_exposed: Vec<Vec<ExPolygon>> = Vec::with_capacity(n);
    let mut top_exposed: Vec<Vec<ExPolygon>> = Vec::with_capacity(n);
    for idx in 0..n {
        let Some(infill_here) = per_layer_infill_areas.get(idx) else {
            continue;
        };
        let below = idx
            .checked_sub(1)
            .and_then(|j| raw_layers.get(j))
            .map(|l| l.regions.as_slice())
            .unwrap_or(&[]);
        let above = raw_layers
            .get(idx + 1)
            .map(|l| l.regions.as_slice())
            .unwrap_or(&[]);

        bottom_exposed.push(difference(infill_here, below)?);
        top_exposed.push(difference(infill_here, above)?);
    }

    let bottom_shells = config.process.bottom_shell_layers as usize;
    let top_shells = config.process.top_shell_layers as usize;

    let mut layers = Vec::with_capacity(n);
    for idx in 0..n {
        let Some(raw) = raw_layers.get(idx) else {
            continue;
        };
        let Some(infill_here) = per_layer_infill_areas.get(idx) else {
            continue;
        };
        let h = raw.plane.height_mm;
        let is_first = idx == 0;

        let mut paths = Vec::new();

        // Optional Stage 5 adhesion on layer 0 before part walls.
        if is_first {
            append_adhesion_paths(&raw.regions, h, config, &mut paths)?;
        }

        // Emit walls (already ordered inner -> outer), splitting unsupported outer walls into overhangs.
        if let Some(wpaths) = per_layer_walls.get(idx) {
            if idx > 0 && config.process.detect_overhangs {
                let below = raw_layers
                    .get(idx - 1)
                    .map(|l| l.regions.as_slice())
                    .unwrap_or(&[]);
                let split_walls = split_overhang_wall_paths(wpaths, below, h, config)?;
                paths.extend(split_walls);
            } else {
                paths.extend(wpaths.iter().cloned());
            }
        }

        // Gather solid shell regions from nearby bottom/top exposed layers.
        let mut shell_sources: Vec<ExPolygon> = Vec::new();
        if bottom_shells > 0 {
            let min_j = idx.saturating_sub(bottom_shells - 1);
            for j in min_j..=idx {
                if let Some(exp) = bottom_exposed.get(j) {
                    shell_sources.extend(exp.iter().cloned());
                }
            }
        }
        if top_shells > 0 {
            let max_j = (idx + top_shells - 1).min(n.saturating_sub(1));
            for j in idx..=max_j {
                if let Some(exp) = top_exposed.get(j) {
                    shell_sources.extend(exp.iter().cloned());
                }
            }
        }

        let solid_union = if shell_sources.is_empty() {
            Vec::new()
        } else {
            intersection(infill_here, &union(&shell_sources)?)?
        };

        let top_here = if top_shells > 0 {
            top_exposed.get(idx).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut bottom_here = if bottom_shells > 0 {
            let b = bottom_exposed.get(idx).cloned().unwrap_or_default();
            difference(&b, &top_here)?
        } else {
            Vec::new()
        };
        let exposed_here = union(&[top_here.as_slice(), bottom_here.as_slice()].concat())?;
        let mut internal_solid = difference(&solid_union, &exposed_here)?;
        let mut sparse_region = difference(infill_here, &solid_union)?;

        let solid_speed = if is_first {
            config.process.effective_first_layer_infill_speed_mm_s()
        } else {
            config.process.effective_solid_infill_speed_mm_s()
        };
        let top_speed = if is_first {
            config.process.effective_first_layer_infill_speed_mm_s()
        } else {
            config.process.effective_top_surface_speed_mm_s()
        };
        let solid_w = config.process.solid_infill_width_mm;
        let solid_spacing = stadium_spacing_mm(solid_w, h);
        let solid_angle = if idx % 2 == 0 { FRAC_PI_4 } else { -FRAC_PI_4 };

        // Detect unsupported bottom areas above layer 0 and anchor them into supporting material below.
        let mut bridge_paths = Vec::new();
        if idx > 0 && config.process.detect_bridges && !bottom_here.is_empty() {
            let below = raw_layers
                .get(idx - 1)
                .map(|l| l.regions.as_slice())
                .unwrap_or(&[]);
            let bridge_raw = std::mem::take(&mut bottom_here);
            let anchor_mm = 2.0 * solid_w;
            let expanded = offset(
                &bridge_raw,
                mm_to_nm(anchor_mm)?,
                JoinType::Miter { limit: 2.0 },
            )?;
            let anchor_ext = intersection(&expanded, below)?;
            let bridge_union = union(&[bridge_raw.as_slice(), anchor_ext.as_slice()].concat())?;
            let anchored_bridges = intersection(infill_here, &bridge_union)?;

            internal_solid = difference(&internal_solid, &anchored_bridges)?;
            sparse_region = difference(&sparse_region, &anchored_bridges)?;

            for bridge_poly in &anchored_bridges {
                let bridge_angle = compute_bridge_angle(bridge_poly, below)?;
                let hatched = generate_rectilinear_hatch(
                    std::slice::from_ref(bridge_poly),
                    bridge_angle,
                    solid_spacing,
                    0.0,
                    false,
                    ExtrusionRole::Bridge,
                    solid_w,
                    h,
                    config.process.bridge_speed_mm_s,
                    config.process.bridge_flow_ratio,
                )?;
                bridge_paths.extend(hatched);
            }
        }

        // Emit solid infill & bridge categories.
        let mono = config.process.monotonic_solid_infill;
        for (region, role, speed, monotonic) in [
            (
                &bottom_here,
                ExtrusionRole::BottomSurface,
                solid_speed,
                mono,
            ),
            (&top_here, ExtrusionRole::TopSurface, top_speed, mono),
            (
                &internal_solid,
                ExtrusionRole::InternalSolidInfill,
                solid_speed,
                false,
            ),
        ] {
            if !region.is_empty() {
                let hatched = generate_rectilinear_hatch(
                    region,
                    solid_angle,
                    solid_spacing,
                    0.0,
                    monotonic,
                    role,
                    solid_w,
                    h,
                    speed,
                    1.0,
                )?;
                paths.extend(hatched);
            }
        }
        paths.extend(bridge_paths);

        // Stage 4: Sparse infill.
        let density = config.process.sparse_infill_density;
        if !sparse_region.is_empty() && density > 1e-6 {
            let sparse_w = config.process.sparse_infill_width_mm;
            let sparse_speed = if is_first {
                config.process.effective_first_layer_infill_speed_mm_s()
            } else {
                config.process.infill_speed_mm_s
            };
            match config.process.sparse_infill_pattern {
                InfillPattern::Rectilinear => {
                    let pitch = (sparse_w / density).max(sparse_w);
                    let angle = if idx % 2 == 0 { FRAC_PI_4 } else { -FRAC_PI_4 };
                    let hatched = generate_rectilinear_hatch(
                        &sparse_region,
                        angle,
                        pitch,
                        0.0,
                        false,
                        ExtrusionRole::SparseInfill,
                        sparse_w,
                        h,
                        sparse_speed,
                        1.0,
                    )?;
                    paths.extend(hatched);
                }
                InfillPattern::Grid => {
                    // Two orthogonal hatch families at pitch = 2 * width / density yield effective
                    // area fill density = density.
                    let pitch = (2.0 * sparse_w / density).max(sparse_w);
                    for angle in [FRAC_PI_4, -FRAC_PI_4] {
                        let hatched = generate_rectilinear_hatch(
                            &sparse_region,
                            angle,
                            pitch,
                            0.0,
                            false,
                            ExtrusionRole::SparseInfill,
                            sparse_w,
                            h,
                            sparse_speed,
                            1.0,
                        )?;
                        paths.extend(hatched);
                    }
                }
                InfillPattern::Cubic => {
                    let pitch = (3.0 * sparse_w / density).max(sparse_w);
                    let phase = -raw.plane.z_top_mm / SQRT_2;
                    for j in 0..3 {
                        let theta = (j as f64) * (2.0 * PI / 3.0);
                        let hatch_angle = theta - FRAC_PI_2;
                        let hatched = generate_rectilinear_hatch(
                            &sparse_region,
                            hatch_angle,
                            pitch,
                            phase,
                            false,
                            ExtrusionRole::SparseInfill,
                            sparse_w,
                            h,
                            sparse_speed,
                            1.0,
                        )?;
                        paths.extend(hatched);
                    }
                }
                InfillPattern::Gyroid => {
                    let gyroid = generate_gyroid_infill(
                        &sparse_region,
                        raw.plane.z_top_mm,
                        density,
                        sparse_w,
                        h,
                        sparse_speed,
                    )?;
                    paths.extend(gyroid);
                }
            }
        }

        let (fan_percent, estimated_time_s) = apply_flow_and_cooling(idx, &mut paths, config);

        layers.push(SlicedLayer {
            index: raw.plane.index,
            z_top_mm: raw.plane.z_top_mm,
            height_mm: h,
            slice_regions: raw.regions.clone(),
            paths,
            fan_percent,
            estimated_time_s,
        });
    }

    Ok(SlicedPlate { layers })
}

fn path_length_mm(path: &ExtrusionPath) -> f64 {
    let mut len = 0.0;
    for pair in path.points.windows(2) {
        if let (Some(&a), Some(&b)) = (pair.first(), pair.get(1)) {
            len += a.distance_mm(b);
        }
    }
    if path.closed
        && let (Some(&first), Some(&last)) = (path.points.first(), path.points.last())
    {
        len += last.distance_mm(first);
    }
    len
}

fn apply_flow_and_cooling(
    layer_idx: usize,
    paths: &mut [ExtrusionPath],
    config: &PrintConfig,
) -> (u8, f64) {
    let max_vol_flow = config.filament.max_volumetric_speed_mm3_s;
    let travel_speed = config.machine.travel_speed_mm_s.max(1e-6);

    let mut path_lengths = Vec::with_capacity(paths.len());
    let mut extrusion_time_s = 0.0;
    let mut travel_time_s = 0.0;
    let mut prev_end: Option<Point2> = None;
    let mut has_bridge_or_overhang = false;

    for path in paths.iter_mut() {
        if matches!(
            path.role,
            ExtrusionRole::Bridge | ExtrusionRole::OverhangWall
        ) {
            has_bridge_or_overhang = true;
        }
        let eff_area = bead_area_mm2(path.width_mm, path.height_mm) * path.flow_ratio.max(0.01);
        if eff_area > 1e-9 && max_vol_flow.is_finite() && max_vol_flow > 0.0 {
            path.speed_mm_s = path.speed_mm_s.min(max_vol_flow / eff_area);
        }
        let len = path_length_mm(path);
        path_lengths.push(len);
        extrusion_time_s += len / path.speed_mm_s.max(1e-6);

        if let Some(&start) = path.points.first() {
            if let Some(prev) = prev_end {
                travel_time_s += prev.distance_mm(start) / travel_speed;
            }
            prev_end = if path.closed {
                Some(start)
            } else {
                path.points.last().copied()
            };
        }
    }

    let raw_time_s = extrusion_time_s + travel_time_s;
    let disable_first = config.filament.disable_fan_first_layers as usize;
    let full_layer = config.filament.full_fan_speed_layer as usize;
    let min_fan = config.filament.fan_min_percent as f64;
    let max_fan = config.filament.fan_max_percent as f64;

    let base_fan_pct = if layer_idx < disable_first {
        0.0
    } else if full_layer <= disable_first || layer_idx >= full_layer {
        min_fan
    } else {
        let span = (full_layer - disable_first + 1) as f64;
        let step = (layer_idx - disable_first + 1) as f64;
        min_fan * (step / span)
    };

    let mut fan_pct = base_fan_pct;
    let target = config.filament.slow_down_layer_time_s;
    if layer_idx >= disable_first && target > 0.0 && raw_time_s > 1e-6 && raw_time_s < target {
        let t = ((target - raw_time_s) / target).clamp(0.0, 1.0);
        fan_pct = base_fan_pct + t * (max_fan - base_fan_pct).max(0.0);

        let needed_ext = (target - travel_time_s).max(extrusion_time_s);
        if needed_ext > extrusion_time_s + 1e-9 {
            let scale = (extrusion_time_s / needed_ext).clamp(0.0, 1.0);
            for path in paths.iter_mut() {
                let floor_speed = config
                    .filament
                    .slow_down_min_speed_mm_s
                    .min(path.speed_mm_s);
                path.speed_mm_s = (path.speed_mm_s * scale).max(floor_speed);
            }
        }
    }

    if layer_idx >= disable_first && has_bridge_or_overhang {
        fan_pct = fan_pct.max(config.filament.bridge_fan_percent as f64);
    }

    let final_ext_time_s: f64 = paths
        .iter()
        .zip(path_lengths.iter())
        .map(|(p, &len)| len / p.speed_mm_s.max(1e-6))
        .sum();
    let fan_u8 = fan_pct.round().clamp(0.0, 100.0) as u8;
    (fan_u8, final_ext_time_s + travel_time_s)
}

fn split_overhang_wall_paths(
    walls: &[ExtrusionPath],
    below: &[ExPolygon],
    layer_height_mm: f64,
    config: &PrintConfig,
) -> Result<Vec<ExtrusionPath>, SliceError> {
    let margin_mm = layer_height_mm * config.process.overhang_threshold_deg.to_radians().tan()
        - 0.5 * config.process.outer_wall_width_mm
        + 0.015;
    let support_mask = if margin_mm.abs() <= 1e-4 {
        below.to_vec()
    } else {
        offset(below, mm_to_nm(margin_mm)?, JoinType::Miter { limit: 2.0 })?
    };

    let mut out = Vec::with_capacity(walls.len());
    for path in walls {
        if path.role != ExtrusionRole::OuterWall || !path.closed || path.points.len() < 3 {
            out.push(path.clone());
            continue;
        }

        let mut closed_pts = Vec::with_capacity(path.points.len() + 1);
        closed_pts.extend_from_slice(&path.points);
        if let Some(&first) = path.points.first() {
            closed_pts.push(first);
        }

        let unsupported = clip_polylines_difference(&[closed_pts], &support_mask)?;
        if unsupported.is_empty() {
            out.push(path.clone());
            continue;
        }

        let n = path.points.len();
        let mut active_role: Option<(ExtrusionRole, f64)> = None;
        let mut active_pts: Vec<Point2> = Vec::new();

        for i in 0..n {
            let (Some(&a), Some(&b)) = (path.points.get(i), path.points.get((i + 1) % n)) else {
                continue;
            };
            if a == b {
                continue;
            }
            let edge = vec![a, b];
            let (ax, ay) = a.to_mm();
            let (bx, by) = b.to_mm();
            let edx = bx - ax;
            let edy = by - ay;
            let elen2 = edx * edx + edy * edy;
            if elen2 <= 1e-12 {
                continue;
            }

            let supported_segs = clip_polylines(std::slice::from_ref(&edge), &support_mask)?;
            let overhang_segs = clip_polylines_difference(&[edge], &support_mask)?;

            let mut edge_pieces: Vec<(f64, ExtrusionRole, f64, Point2, Point2)> = Vec::new();
            for (segs, role, speed) in [
                (supported_segs, ExtrusionRole::OuterWall, path.speed_mm_s),
                (
                    overhang_segs,
                    ExtrusionRole::OverhangWall,
                    config.process.overhang_speed_mm_s,
                ),
            ] {
                for seg in segs {
                    let (Some(&sp0), Some(&sp1)) = (seg.first(), seg.last()) else {
                        continue;
                    };
                    if sp0.distance_mm(sp1) <= 1e-6 {
                        continue;
                    }
                    let (p0x, p0y) = sp0.to_mm();
                    let (p1x, p1y) = sp1.to_mm();
                    let t0 = ((p0x - ax) * edx + (p0y - ay) * edy) / elen2;
                    let t1 = ((p1x - ax) * edx + (p1y - ay) * edy) / elen2;
                    let (ta, pa, pb) = if t1 < t0 {
                        (t1, sp1, sp0)
                    } else {
                        (t0, sp0, sp1)
                    };
                    edge_pieces.push((ta, role, speed, pa, pb));
                }
            }
            edge_pieces.sort_by(|x, y| x.0.total_cmp(&y.0));

            for (_, role, speed, p0, p1) in edge_pieces {
                let can_extend = active_role.is_some_and(|(r, _)| r == role)
                    && active_pts
                        .last()
                        .is_some_and(|&lp| lp.distance_mm(p0) <= 1e-3);
                if can_extend {
                    if active_pts
                        .last()
                        .is_none_or(|&lp| lp.distance_mm(p0) > 1e-6)
                    {
                        active_pts.push(p0);
                    }
                    active_pts.push(p1);
                } else {
                    if let Some((prev_role, prev_speed)) = active_role
                        && active_pts.len() >= 2
                    {
                        out.push(ExtrusionPath {
                            role: prev_role,
                            width_mm: path.width_mm,
                            height_mm: path.height_mm,
                            speed_mm_s: prev_speed,
                            flow_ratio: 1.0,
                            points: std::mem::take(&mut active_pts),
                            closed: false,
                        });
                    }
                    active_role = Some((role, speed));
                    active_pts = vec![p0, p1];
                }
            }
        }

        if let Some((prev_role, prev_speed)) = active_role
            && active_pts.len() >= 2
        {
            out.push(ExtrusionPath {
                role: prev_role,
                width_mm: path.width_mm,
                height_mm: path.height_mm,
                speed_mm_s: prev_speed,
                flow_ratio: 1.0,
                points: active_pts,
                closed: false,
            });
        }
    }

    Ok(out)
}

fn polygon_centroid(poly: &Polygon) -> (f64, f64) {
    let n = poly.points.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    let mut area2 = 0.0;
    let mut cx = 0.0;
    let mut cy = 0.0;
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    for i in 0..n {
        let (Some(&p0), Some(&p1)) = (poly.points.get(i), poly.points.get((i + 1) % n)) else {
            continue;
        };
        let (x0, y0) = p0.to_mm();
        let (x1, y1) = p1.to_mm();
        sum_x += x0;
        sum_y += y0;
        let cross = x0 * y1 - x1 * y0;
        area2 += cross;
        cx += (x0 + x1) * cross;
        cy += (y0 + y1) * cross;
    }
    if area2.abs() > 1e-9 {
        (cx / (3.0 * area2), cy / (3.0 * area2))
    } else {
        let nf = n as f64;
        (sum_x / nf, sum_y / nf)
    }
}

fn compute_bridge_angle(bridge: &ExPolygon, below: &[ExPolygon]) -> Result<f64, SliceError> {
    let expanded = offset(
        std::slice::from_ref(bridge),
        mm_to_nm(1.0)?,
        JoinType::Miter { limit: 2.0 },
    )?;
    let mut anchors = intersection(&expanded, below)?;
    if anchors.len() >= 2 {
        anchors.sort_by(|a, b| b.area_mm2().total_cmp(&a.area_mm2()));
        if let (Some(a0), Some(a1)) = (anchors.first(), anchors.get(1)) {
            let c0 = polygon_centroid(&a0.outer);
            let c1 = polygon_centroid(&a1.outer);
            let dx = c1.0 - c0.0;
            let dy = c1.1 - c0.1;
            if dx.hypot(dy) > 1e-6 {
                return Ok(dy.atan2(dx));
            }
        }
    }

    let mut best_angle = 0.0;
    let mut best_span = f64::INFINITY;
    for step in 0..12 {
        let angle = (step as f64) * (PI / 12.0);
        let ca = angle.cos();
        let sa = angle.sin();
        let mut u_min = f64::INFINITY;
        let mut u_max = f64::NEG_INFINITY;
        for pt in &bridge.outer.points {
            let (px, py) = pt.to_mm();
            let u = px * ca + py * sa;
            u_min = u_min.min(u);
            u_max = u_max.max(u);
        }
        let span = u_max - u_min;
        if span < best_span {
            best_span = span;
            best_angle = angle;
        }
    }
    Ok(best_angle)
}

fn region_aabb(region: &[ExPolygon]) -> Option<Aabb2> {
    let mut bounds: Option<Aabb2> = None;
    for ex in region {
        if let Some(bb) = ex.aabb() {
            bounds = Some(match bounds {
                Some(cur) => cur.union(bb),
                None => bb,
            });
        }
    }
    bounds
}

fn generate_gyroid_infill(
    region: &[ExPolygon],
    z_mm: f64,
    density: f64,
    width_mm: f64,
    height_mm: f64,
    speed_mm_s: f64,
) -> Result<Vec<ExtrusionPath>, SliceError> {
    if region.is_empty() || density <= 1e-6 {
        return Ok(Vec::new());
    }
    let Some(aabb) = region_aabb(region) else {
        return Ok(Vec::new());
    };
    let (min_x, min_y) = aabb.min.to_mm();
    let (max_x, max_y) = aabb.max.to_mm();

    let l = (2.44 * width_mm / density).max(2.0 * width_mm);
    let k = TAU / l;
    let kz = k * z_mm;
    let cos_z = kz.cos();
    let sin_z = kz.sin();

    let x0 = min_x - l;
    let x1 = max_x + l;
    let y0 = min_y - l;
    let y1 = max_y + l;

    let ds = (l / 16.0).clamp(0.25, 1.2);
    let mut raw_waves: Vec<Vec<Point2>> = Vec::new();

    if cos_z.abs() >= sin_z.abs() {
        let sign = if cos_z >= 0.0 { 1.0 } else { -1.0 };
        let a = sign * cos_z;
        let samples = (((x1 - x0) / ds).ceil() as usize + 1).clamp(2, 2000);
        let dx = (x1 - x0) / ((samples - 1) as f64);

        let c_start = ((k * y0) / TAU).floor() as i64 - 1;
        let c_end = ((k * y1) / TAU).ceil() as i64 + 1;
        let cell_count = c_end.saturating_sub(c_start).clamp(0, 1000);

        for c_off in 0..=cell_count {
            let cell = c_start + c_off;
            for odd in [false, true] {
                let mut wave = Vec::with_capacity(samples);
                for s in 0..samples {
                    let x = x0 + (s as f64) * dx;
                    let kx = k * x;
                    let b = sign * kx.sin();
                    let c = -sign * sin_z * kx.cos();
                    let r = a.hypot(b);
                    let alpha = b.atan2(a);
                    let beta = (c / r).clamp(-1.0, 1.0).asin();
                    let ky =
                        TAU * (cell as f64) + if odd { PI - beta - alpha } else { beta - alpha };
                    wave.push(Point2::from_mm(x, ky / k)?);
                }
                raw_waves.push(wave);
            }
        }
    } else {
        let sign = if sin_z >= 0.0 { 1.0 } else { -1.0 };
        let a = sign * sin_z;
        let samples = (((y1 - y0) / ds).ceil() as usize + 1).clamp(2, 2000);
        let dy = (y1 - y0) / ((samples - 1) as f64);

        let c_start = ((k * x0) / TAU).floor() as i64 - 1;
        let c_end = ((k * x1) / TAU).ceil() as i64 + 1;
        let cell_count = c_end.saturating_sub(c_start).clamp(0, 1000);

        for c_off in 0..=cell_count {
            let cell = c_start + c_off;
            for odd in [false, true] {
                let mut wave = Vec::with_capacity(samples);
                for s in 0..samples {
                    let y = y0 + (s as f64) * dy;
                    let ky = k * y;
                    let b = sign * ky.cos();
                    let c = -sign * cos_z * ky.sin();
                    let r = a.hypot(b);
                    let gamma = b.atan2(a);
                    let delta = (c / r).clamp(-1.0, 1.0).acos();
                    let kx = TAU * (cell as f64) + if odd { gamma - delta } else { gamma + delta };
                    wave.push(Point2::from_mm(kx / k, y)?);
                }
                raw_waves.push(wave);
            }
        }
    }

    let clipped = clip_polylines(&raw_waves, region)?;
    let mut paths = Vec::with_capacity(clipped.len());
    for (i, mut pts) in clipped.into_iter().enumerate() {
        if pts.len() < 2 {
            continue;
        }
        if i % 2 == 1 {
            pts.reverse();
        }
        paths.push(ExtrusionPath {
            role: ExtrusionRole::SparseInfill,
            width_mm,
            height_mm,
            speed_mm_s,
            flow_ratio: 1.0,
            points: pts,
            closed: false,
        });
    }
    Ok(paths)
}

fn build_perimeters(
    regions: &[ExPolygon],
    layer_idx: usize,
    layer_height_mm: f64,
    config: &PrintConfig,
) -> Result<(Vec<ExtrusionPath>, Vec<ExPolygon>), SliceError> {
    if regions.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let join = JoinType::Miter { limit: 2.0 };
    let is_first = layer_idx == 0;
    let outer_w = config.process.outer_wall_width_mm;
    let inner_w = config.process.inner_wall_width_mm;
    let outer_speed = if is_first {
        config.process.first_layer_speed_mm_s
    } else {
        config.process.outer_wall_speed_mm_s
    };
    let inner_speed = if is_first {
        config.process.first_layer_speed_mm_s
    } else {
        config.process.inner_wall_speed_mm_s
    };

    let wall_loops = config.process.wall_loops;
    if wall_loops == 0 {
        return Ok((Vec::new(), regions.to_vec()));
    }

    let mut rings: Vec<(ExtrusionRole, f64, f64, Vec<ExPolygon>)> =
        Vec::with_capacity(wall_loops as usize);

    // Outer wall centerline sits 0.5 * outer_w inward from the model surface.
    let mut cumulative_inset_mm = 0.5 * outer_w;
    let outer_polys = offset(regions, -mm_to_nm(cumulative_inset_mm)?, join)?;
    if outer_polys.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    rings.push((ExtrusionRole::OuterWall, outer_w, outer_speed, outer_polys));

    // Each inner wall steps inward by the stadium pitch between adjacent beads.
    let mut prev_w = outer_w;
    for _ in 1..wall_loops {
        let overlap = layer_height_mm * (1.0 - FRAC_PI_4);
        let pitch = (0.5 * prev_w + 0.5 * inner_w - overlap).max(0.05 * inner_w);
        cumulative_inset_mm += pitch;
        let inner_polys = offset(regions, -mm_to_nm(cumulative_inset_mm)?, join)?;
        if inner_polys.is_empty() {
            break;
        }
        rings.push((ExtrusionRole::InnerWall, inner_w, inner_speed, inner_polys));
        prev_w = inner_w;
    }

    // Compute the actual un-overlapped void left inside the innermost wall; if thin geometry
    // already fills the cross-section completely, do not synthesize an overlap sliver of infill.
    let inner_edge_inset_mm = cumulative_inset_mm + 0.5 * prev_w;
    let true_interior = offset(regions, -mm_to_nm(inner_edge_inset_mm)?, join)?;
    let infill_area = if true_interior.is_empty() {
        Vec::new()
    } else {
        let infill_overlap = 0.15 * config.process.solid_infill_width_mm;
        offset(&true_interior, mm_to_nm(infill_overlap)?, join)?
    };

    // Emit inner walls first, then outer wall (standard inside-out ordering).
    let mut paths = Vec::new();
    for (role, width_mm, speed_mm_s, poly_group) in rings.into_iter().rev() {
        for ex in poly_group {
            push_closed_poly(
                &ex.outer,
                role,
                width_mm,
                layer_height_mm,
                speed_mm_s,
                &mut paths,
            );
            for hole in ex.holes {
                push_closed_poly(
                    &hole,
                    role,
                    width_mm,
                    layer_height_mm,
                    speed_mm_s,
                    &mut paths,
                );
            }
        }
    }

    Ok((paths, infill_area))
}

fn push_closed_poly(
    poly: &Polygon,
    role: ExtrusionRole,
    width_mm: f64,
    height_mm: f64,
    speed_mm_s: f64,
    out: &mut Vec<ExtrusionPath>,
) {
    if poly.points.len() >= 3 {
        out.push(ExtrusionPath {
            role,
            width_mm,
            height_mm,
            speed_mm_s,
            flow_ratio: 1.0,
            points: poly.points.clone(),
            closed: true,
        });
    }
}

fn append_adhesion_paths(
    layer0: &[ExPolygon],
    height_mm: f64,
    config: &PrintConfig,
    out: &mut Vec<ExtrusionPath>,
) -> Result<(), SliceError> {
    if layer0.is_empty() {
        return Ok(());
    }
    let join = JoinType::Round {
        arc_tolerance_nm: 25_000,
    };
    let w = config.process.outer_wall_width_mm;
    let speed = config.process.first_layer_speed_mm_s;

    if config.process.brim_width_mm > 0.0 {
        let spacing = stadium_spacing_mm(w, height_mm);
        let loops = ((config.process.brim_width_mm / spacing).ceil() as usize).max(1);
        for i in (0..loops).rev() {
            let delta = 0.5 * w + (i as f64) * spacing;
            let brim_polys = offset(layer0, mm_to_nm(delta)?, join)?;
            for ex in brim_polys {
                push_closed_poly(&ex.outer, ExtrusionRole::Brim, w, height_mm, speed, out);
            }
        }
    }

    if config.process.skirt_loops > 0 {
        let spacing = stadium_spacing_mm(w, height_mm);
        let base_gap = config.process.brim_width_mm + 4.0;
        for i in 0..config.process.skirt_loops {
            let delta = base_gap + (i as f64) * spacing;
            let skirt_polys = offset(layer0, mm_to_nm(delta)?, join)?;
            for ex in skirt_polys {
                push_closed_poly(&ex.outer, ExtrusionRole::Skirt, w, height_mm, speed, out);
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn flush_hatch_pts(
    pts: &mut Vec<Point2>,
    role: ExtrusionRole,
    width_mm: f64,
    height_mm: f64,
    speed_mm_s: f64,
    flow_ratio: f64,
    paths: &mut Vec<ExtrusionPath>,
) {
    if pts.len() >= 2 {
        paths.push(ExtrusionPath {
            role,
            width_mm,
            height_mm,
            speed_mm_s,
            flow_ratio,
            points: std::mem::take(pts),
            closed: false,
        });
    } else {
        pts.clear();
    }
}

fn try_chain_hatch_segment(
    current_pts: &mut Vec<Point2>,
    mut a: Point2,
    mut b: Point2,
    max_link_mm: f64,
    region: &[ExPolygon],
    reorient_end: bool,
) -> Option<(Point2, Point2)> {
    let Some(&last_pt) = current_pts.last() else {
        return Some((a, b));
    };
    if reorient_end && last_pt.distance_mm(b) < last_pt.distance_mm(a) {
        std::mem::swap(&mut a, &mut b);
    }
    let dist = last_pt.distance_mm(a);
    let mid = Point2::new((last_pt.x + a.x) / 2, (last_pt.y + a.y) / 2);
    let inside = region.iter().any(|ex| ex.contains_point(mid));
    if dist <= max_link_mm && inside {
        if a != last_pt {
            current_pts.push(a);
        }
        current_pts.push(b);
        None
    } else {
        Some((a, b))
    }
}

#[allow(clippy::too_many_arguments)]
fn generate_rectilinear_hatch(
    region: &[ExPolygon],
    angle_rad: f64,
    spacing_mm: f64,
    phase_offset_mm: f64,
    monotonic: bool,
    role: ExtrusionRole,
    width_mm: f64,
    height_mm: f64,
    speed_mm_s: f64,
    flow_ratio: f64,
) -> Result<Vec<ExtrusionPath>, SliceError> {
    if region.is_empty() || spacing_mm <= 0.0 {
        return Ok(Vec::new());
    }
    let Some(aabb) = region_aabb(region) else {
        return Ok(Vec::new());
    };

    let (min_x, min_y) = aabb.min.to_mm();
    let (max_x, max_y) = aabb.max.to_mm();
    let cos_a = angle_rad.cos();
    let sin_a = angle_rad.sin();

    // Project AABB corners into rotated hatch space:
    // u =  x * cos + y * sin
    // v = -x * sin + y * cos
    let mut u_min = f64::INFINITY;
    let mut u_max = f64::NEG_INFINITY;
    let mut v_min = f64::INFINITY;
    let mut v_max = f64::NEG_INFINITY;
    for (cx, cy) in [
        (min_x, min_y),
        (max_x, min_y),
        (max_x, max_y),
        (min_x, max_y),
    ] {
        let u = cx * cos_a + cy * sin_a;
        let v = -cx * sin_a + cy * cos_a;
        u_min = u_min.min(u);
        u_max = u_max.max(u);
        v_min = v_min.min(v);
        v_max = v_max.max(v);
    }

    let u0 = u_min - 1.0;
    let u1 = u_max + 1.0;

    let k_start = ((v_min - phase_offset_mm) / spacing_mm).floor() as i64;
    let k_end = ((v_max - phase_offset_mm) / spacing_mm).ceil() as i64;
    let mut raw_lines: Vec<(Point2, Point2)> = Vec::new();

    let count = k_end.saturating_sub(k_start).clamp(0, 100_000);
    for step in 0..=count {
        let v = ((k_start + step) as f64) * spacing_mm + phase_offset_mm;
        if v < v_min - 1e-4 || v > v_max + 1e-4 {
            continue;
        }
        let (ua, ub) = if step % 2 == 0 { (u0, u1) } else { (u1, u0) };
        let ax = ua * cos_a - v * sin_a;
        let ay = ua * sin_a + v * cos_a;
        let bx = ub * cos_a - v * sin_a;
        let by = ub * sin_a + v * cos_a;
        raw_lines.push((Point2::from_mm(ax, ay)?, Point2::from_mm(bx, by)?));
    }

    let clipped = clip_lines(&raw_lines, region)?;
    if clipped.is_empty() {
        return Ok(Vec::new());
    }

    let mut tagged: Vec<(i64, f64, Point2, Point2)> = Vec::with_capacity(clipped.len());
    for seg in clipped {
        let (Some(&p0), Some(&p1)) = (seg.first(), seg.last()) else {
            continue;
        };
        if p0 == p1 {
            continue;
        }
        let (x0, y0) = p0.to_mm();
        let (x1, y1) = p1.to_mm();
        let v_mid = -0.5 * (x0 + x1) * sin_a + 0.5 * (y0 + y1) * cos_a;
        let u_mid = 0.5 * (x0 + x1) * cos_a + 0.5 * (y0 + y1) * sin_a;
        let row = ((v_mid - phase_offset_mm) / spacing_mm).round() as i64;

        let u0_p = x0 * cos_a + y0 * sin_a;
        let u1_p = x1 * cos_a + y1 * sin_a;
        let (pa, pb) = if u1_p < u0_p { (p1, p0) } else { (p0, p1) };
        tagged.push((row, u_mid, pa, pb));
    }
    tagged.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));

    let max_link_mm = (2.5 * spacing_mm).max(2.0 * width_mm);
    let mut paths: Vec<ExtrusionPath> = Vec::new();
    let mut current_pts: Vec<Point2> = Vec::new();
    let mut flip_dir = false;

    if monotonic {
        for row_segs in tagged.chunk_by(|a, b| a.0 == b.0) {
            flip_dir = !flip_dir;
            if row_segs.len() > 1 {
                flush_hatch_pts(
                    &mut current_pts,
                    role,
                    width_mm,
                    height_mm,
                    speed_mm_s,
                    flow_ratio,
                    &mut paths,
                );
                let mut ordered: Vec<(Point2, Point2)> =
                    row_segs.iter().map(|t| (t.2, t.3)).collect();
                if flip_dir {
                    ordered.reverse();
                }
                for (mut a, mut b) in ordered {
                    if flip_dir {
                        std::mem::swap(&mut a, &mut b);
                    }
                    let mut single = vec![a, b];
                    flush_hatch_pts(
                        &mut single,
                        role,
                        width_mm,
                        height_mm,
                        speed_mm_s,
                        flow_ratio,
                        &mut paths,
                    );
                }
            } else if let Some(&(_, _, mut a, mut b)) = row_segs.first() {
                if flip_dir {
                    std::mem::swap(&mut a, &mut b);
                }
                if let Some((na, nb)) =
                    try_chain_hatch_segment(&mut current_pts, a, b, max_link_mm, region, false)
                {
                    flush_hatch_pts(
                        &mut current_pts,
                        role,
                        width_mm,
                        height_mm,
                        speed_mm_s,
                        flow_ratio,
                        &mut paths,
                    );
                    current_pts = vec![na, nb];
                }
            }
        }
    } else {
        for (i, &(_, _, mut a, mut b)) in tagged.iter().enumerate() {
            let prev_row = i.checked_sub(1).and_then(|j| tagged.get(j)).map(|t| t.0);
            if prev_row != Some(tagged.get(i).map(|t| t.0).unwrap_or(0)) {
                flip_dir = !flip_dir;
            }
            if flip_dir {
                std::mem::swap(&mut a, &mut b);
            }
            if let Some((na, nb)) =
                try_chain_hatch_segment(&mut current_pts, a, b, max_link_mm, region, true)
            {
                flush_hatch_pts(
                    &mut current_pts,
                    role,
                    width_mm,
                    height_mm,
                    speed_mm_s,
                    flow_ratio,
                    &mut paths,
                );
                current_pts = vec![na, nb];
            }
        }
    }

    flush_hatch_pts(
        &mut current_pts,
        role,
        width_mm,
        height_mm,
        speed_mm_s,
        flow_ratio,
        &mut paths,
    );

    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use layercraft_geom::{Point3, Triangle};

    fn make_box_mesh(w: f64, d: f64, h: f64) -> TriangleMesh {
        let tris = [
            ((0.0, 0.0, 0.0), (0.0, d, 0.0), (w, d, 0.0)),
            ((0.0, 0.0, 0.0), (w, d, 0.0), (w, 0.0, 0.0)),
            ((0.0, 0.0, h), (w, 0.0, h), (w, d, h)),
            ((0.0, 0.0, h), (w, d, h), (0.0, d, h)),
            ((0.0, 0.0, 0.0), (w, 0.0, 0.0), (w, 0.0, h)),
            ((0.0, 0.0, 0.0), (w, 0.0, h), (0.0, 0.0, h)),
            ((0.0, d, 0.0), (0.0, d, h), (w, d, h)),
            ((0.0, d, 0.0), (w, d, h), (w, d, 0.0)),
            ((0.0, 0.0, 0.0), (0.0, 0.0, h), (0.0, d, h)),
            ((0.0, 0.0, 0.0), (0.0, d, h), (0.0, d, 0.0)),
            ((w, 0.0, 0.0), (w, d, 0.0), (w, d, h)),
            ((w, 0.0, 0.0), (w, d, h), (w, 0.0, h)),
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
        TriangleMesh::new(tris).unwrap()
    }

    #[test]
    fn cube_slice_has_bottom_sparse_and_top_layers() {
        let mesh = make_box_mesh(20.0, 20.0, 20.0);
        let cfg = PrintConfig::default();
        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        assert_eq!(plate.layers.len(), 100);

        let l0 = &plate.layers[0];
        assert!(l0.paths.iter().any(|p| p.role == ExtrusionRole::OuterWall));
        assert!(l0.paths.iter().any(|p| p.role == ExtrusionRole::InnerWall));
        assert!(
            l0.paths
                .iter()
                .any(|p| p.role == ExtrusionRole::BottomSurface)
        );

        let mid = &plate.layers[50];
        assert!(
            mid.paths
                .iter()
                .any(|p| p.role == ExtrusionRole::SparseInfill)
        );
        assert!(
            !mid.paths
                .iter()
                .any(|p| p.role == ExtrusionRole::TopSurface)
        );

        let top = &plate.layers[99];
        assert!(
            top.paths
                .iter()
                .any(|p| p.role == ExtrusionRole::TopSurface)
        );
    }

    #[test]
    fn volumetric_flow_cap_clamps_path_speeds() {
        let mesh = make_box_mesh(20.0, 20.0, 2.0);
        let mut cfg = PrintConfig::default();
        cfg.process.infill_speed_mm_s = 300.0;
        cfg.process.outer_wall_speed_mm_s = 220.0;
        cfg.filament.max_volumetric_speed_mm3_s = 6.0;
        cfg.filament.slow_down_layer_time_s = 0.0;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        for layer in &plate.layers {
            for p in &layer.paths {
                let flow = bead_area_mm2(p.width_mm, p.height_mm) * p.speed_mm_s;
                assert!(
                    flow <= cfg.filament.max_volumetric_speed_mm3_s + 1e-6,
                    "flow {flow} exceeded cap on layer {}",
                    layer.index
                );
            }
        }
    }

    #[test]
    fn small_tower_triggers_fan_boost_and_cooling_slowdown() {
        let mesh = make_box_mesh(6.0, 6.0, 4.0);
        let mut cfg = PrintConfig::default();
        cfg.filament.disable_fan_first_layers = 1;
        cfg.filament.full_fan_speed_layer = 3;
        cfg.filament.fan_min_percent = 40;
        cfg.filament.fan_max_percent = 100;
        cfg.filament.slow_down_layer_time_s = 8.0;
        cfg.filament.slow_down_min_speed_mm_s = 12.0;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        assert_eq!(plate.layers[0].fan_percent, 0);
        assert!(plate.layers[1].fan_percent > 0);
        let top_upper = &plate.layers[plate.layers.len() - 2];
        assert!(top_upper.fan_percent >= 80);
        assert!(
            top_upper
                .paths
                .iter()
                .all(|p| p.speed_mm_s < cfg.process.outer_wall_speed_mm_s)
        );
    }

    #[test]
    fn top_surface_speed_override_applies() {
        let mesh = make_box_mesh(30.0, 30.0, 2.0);
        let mut cfg = PrintConfig::default();
        cfg.process.top_shell_layers = 2;
        cfg.process.bottom_shell_layers = 1;
        cfg.process.top_surface_speed_mm_s = Some(28.0);
        cfg.filament.slow_down_layer_time_s = 0.0;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let top_layer = plate.layers.last().unwrap();
        let top_paths: Vec<_> = top_layer
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::TopSurface)
            .collect();
        assert!(!top_paths.is_empty());
        for p in top_paths {
            assert!((p.speed_mm_s - 28.0).abs() < 1e-6);
        }
    }

    fn make_bridge_arch_mesh(
        pillar_w: f64,
        span: f64,
        depth: f64,
        pillar_h: f64,
        deck_h: f64,
    ) -> TriangleMesh {
        let mut tris = Vec::new();
        let right_x = pillar_w + span;
        let total_w = 2.0 * pillar_w + span;
        for (ox, oy, oz, w, d, h) in [
            (0.0, 0.0, 0.0, pillar_w, depth, pillar_h),
            (right_x, 0.0, 0.0, pillar_w, depth, pillar_h),
            (0.0, 0.0, pillar_h, total_w, depth, deck_h),
        ] {
            let mut m = make_box_mesh(w, d, h);
            m.translate(ox, oy, oz).unwrap();
            tris.extend(m.triangles);
        }
        TriangleMesh::new(tris).unwrap()
    }

    fn make_overhang_ramp_mesh(
        base_w: f64,
        depth: f64,
        height: f64,
        overhang_deg: f64,
    ) -> TriangleMesh {
        let dx = height * overhang_deg.to_radians().tan();
        let top_w = base_w + dx;
        let push_q = |tris: &mut Vec<Triangle>, a: Point3, b: Point3, c: Point3, d: Point3| {
            tris.push(Triangle::new(a, b, c));
            tris.push(Triangle::new(a, c, d));
        };
        let p000 = Point3::new(0.0, 0.0, 0.0);
        let p100 = Point3::new(base_w, 0.0, 0.0);
        let p110 = Point3::new(base_w, depth, 0.0);
        let p010 = Point3::new(0.0, depth, 0.0);
        let p001 = Point3::new(0.0, 0.0, height);
        let p101 = Point3::new(top_w, 0.0, height);
        let p111 = Point3::new(top_w, depth, height);
        let p011 = Point3::new(0.0, depth, height);

        let mut tris = Vec::with_capacity(12);
        push_q(&mut tris, p000, p010, p110, p100);
        push_q(&mut tris, p001, p101, p111, p011);
        push_q(&mut tris, p000, p100, p101, p001);
        push_q(&mut tris, p010, p011, p111, p110);
        push_q(&mut tris, p000, p001, p011, p010);
        push_q(&mut tris, p100, p110, p111, p101);
        TriangleMesh::new(tris).unwrap()
    }

    #[test]
    fn bridge_arch_emits_x_aligned_bridges_and_boosts_fan() {
        let mesh = make_bridge_arch_mesh(6.0, 18.0, 10.0, 4.0, 2.0);
        let mut cfg = PrintConfig::default();
        cfg.filament.fan_min_percent = 30;
        cfg.filament.bridge_fan_percent = 100;
        cfg.filament.slow_down_layer_time_s = 0.0;
        cfg.process.bridge_speed_mm_s = 28.0;
        cfg.process.bridge_flow_ratio = 0.90;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let bridge_layer = &plate.layers[20];
        let bridges: Vec<_> = bridge_layer
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::Bridge)
            .collect();
        assert!(!bridges.is_empty(), "expected Bridge paths on layer 20");
        assert_eq!(bridge_layer.fan_percent, 100);

        let mut x_span_len = 0.0;
        let mut total_len = 0.0;
        for b in bridges {
            assert!((b.speed_mm_s - 28.0).abs() < 1e-6);
            assert!((b.flow_ratio - 0.90).abs() < 1e-6);
            for w in b.points.windows(2) {
                let (x0, y0) = w[0].to_mm();
                let (x1, y1) = w[1].to_mm();
                let dx = (x1 - x0).abs();
                let dy = (y1 - y0).abs();
                let len = dx.hypot(dy);
                total_len += len;
                if len > 1.0 {
                    assert!(
                        dy < dx * 0.05 + 1e-4,
                        "expected X-aligned bridge span, got ({x0},{y0}) -> ({x1},{y1})"
                    );
                    x_span_len += len;
                }
            }
        }
        assert!(x_span_len > 0.90 * total_len);
    }

    #[test]
    fn overhang_detection_triggers_at_60deg_and_skips_30deg() {
        let mut cfg = PrintConfig::default();
        cfg.process.overhang_threshold_deg = 45.0;
        cfg.process.overhang_speed_mm_s = 22.0;
        cfg.filament.slow_down_layer_time_s = 0.0;

        let steep = make_overhang_ramp_mesh(12.0, 12.0, 4.0, 60.0);
        let steep_plate = slice_single_mesh(&steep, &cfg).unwrap();
        let overhang_paths: Vec<_> = steep_plate
            .layers
            .iter()
            .flat_map(|l| l.paths.iter())
            .filter(|p| p.role == ExtrusionRole::OverhangWall)
            .collect();
        assert!(
            !overhang_paths.is_empty(),
            "60 deg overhang should produce OverhangWall"
        );
        for p in overhang_paths {
            assert!((p.speed_mm_s - 22.0).abs() < 1e-6);
        }

        let shallow = make_overhang_ramp_mesh(12.0, 12.0, 4.0, 30.0);
        let shallow_plate = slice_single_mesh(&shallow, &cfg).unwrap();
        let shallow_overhangs = shallow_plate
            .layers
            .iter()
            .flat_map(|l| l.paths.iter())
            .filter(|p| p.role == ExtrusionRole::OverhangWall)
            .count();
        assert_eq!(shallow_overhangs, 0);
    }

    #[test]
    fn gyroid_and_cubic_infill_produce_3d_varying_paths() {
        let mesh = make_box_mesh(24.0, 24.0, 6.0);

        let mut cfg_g = PrintConfig::default();
        cfg_g.process.sparse_infill_pattern = InfillPattern::Gyroid;
        cfg_g.process.sparse_infill_density = 0.25;
        let plate_g = slice_single_mesh(&mesh, &cfg_g).unwrap();
        let l10_g: Vec<_> = plate_g.layers[10]
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::SparseInfill)
            .collect();
        let l15_g: Vec<_> = plate_g.layers[15]
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::SparseInfill)
            .collect();
        assert!(!l10_g.is_empty() && !l15_g.is_empty());
        assert!(l10_g.iter().any(|p| p.points.len() >= 5));
        assert_ne!(l10_g[0].points, l15_g[0].points);

        let mut cfg_c = PrintConfig::default();
        cfg_c.process.sparse_infill_pattern = InfillPattern::Cubic;
        cfg_c.process.sparse_infill_density = 0.25;
        let plate_c = slice_single_mesh(&mesh, &cfg_c).unwrap();
        let l10_c: Vec<_> = plate_c.layers[10]
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::SparseInfill)
            .collect();
        let l15_c: Vec<_> = plate_c.layers[15]
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::SparseInfill)
            .collect();
        assert!(!l10_c.is_empty() && !l15_c.is_empty());
        assert_ne!(l10_c[0].points, l15_c[0].points);
    }

    #[test]
    fn monotonic_top_surface_orders_holed_rows_non_decreasingly() {
        let outer = Polygon::new(vec![
            Point2::from_mm(0.0, 0.0).unwrap(),
            Point2::from_mm(20.0, 0.0).unwrap(),
            Point2::from_mm(20.0, 20.0).unwrap(),
            Point2::from_mm(0.0, 20.0).unwrap(),
        ]);
        let mut hole = Polygon::new(vec![
            Point2::from_mm(7.0, 7.0).unwrap(),
            Point2::from_mm(13.0, 7.0).unwrap(),
            Point2::from_mm(13.0, 13.0).unwrap(),
            Point2::from_mm(7.0, 13.0).unwrap(),
        ]);
        hole.ensure_cw();
        let region = vec![ExPolygon::new(outer, vec![hole])];

        let angle = FRAC_PI_4;
        let spacing = stadium_spacing_mm(0.42, 0.2);
        let paths = generate_rectilinear_hatch(
            &region,
            angle,
            spacing,
            0.0,
            true,
            ExtrusionRole::TopSurface,
            0.42,
            0.2,
            60.0,
            1.0,
        )
        .unwrap();
        assert!(paths.len() > 2);

        let sin_a = angle.sin();
        let cos_a = angle.cos();
        let mut last_row = i64::MIN;
        for p in &paths {
            for seg in p.points.windows(2) {
                let (x0, y0) = seg[0].to_mm();
                let (x1, y1) = seg[1].to_mm();
                let u_diff = ((x1 - x0) * cos_a + (y1 - y0) * sin_a).abs();
                let v_diff = (-(x1 - x0) * sin_a + (y1 - y0) * cos_a).abs();
                if u_diff > v_diff {
                    let v_mid = -0.5 * (x0 + x1) * sin_a + 0.5 * (y0 + y1) * cos_a;
                    let row = (v_mid / spacing).round() as i64;
                    assert!(
                        row >= last_row,
                        "monotonic order regressed from row {last_row} to {row}"
                    );
                    last_row = row;
                }
            }
        }
    }
}
