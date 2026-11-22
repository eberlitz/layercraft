//! Deterministic FDM slicing pipeline: slice -> perimeters -> skins -> infill -> adhesion.

use std::f64::consts::{FRAC_PI_4, PI};

use layercraft_clip::{ClipError, JoinType, clip_lines, difference, intersection, offset, union};
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
    BottomSurface,
    TopSurface,
    InternalSolidInfill,
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
            Self::BottomSurface => "Bottom surface",
            Self::TopSurface => "Top surface",
            Self::InternalSolidInfill => "Internal solid infill",
            Self::SparseInfill => "Sparse infill",
            Self::Skirt => "Skirt",
            Self::Brim => "Brim",
        }
    }
}

/// A 2D toolpath polyline at a single layer height.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExtrusionPath {
    pub role: ExtrusionRole,
    pub width_mm: f64,
    pub height_mm: f64,
    pub speed_mm_s: f64,
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

        // Emit walls (already ordered inner -> outer).
        if let Some(wpaths) = per_layer_walls.get(idx) {
            paths.extend(wpaths.iter().cloned());
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
        let bottom_here = if bottom_shells > 0 {
            let b = bottom_exposed.get(idx).cloned().unwrap_or_default();
            difference(&b, &top_here)?
        } else {
            Vec::new()
        };
        let exposed_here = union(&[top_here.as_slice(), bottom_here.as_slice()].concat())?;
        let internal_solid = difference(&solid_union, &exposed_here)?;
        let sparse_region = difference(infill_here, &solid_union)?;

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

        // Emit solid infill categories.
        for (region, role, speed) in [
            (&bottom_here, ExtrusionRole::BottomSurface, solid_speed),
            (&top_here, ExtrusionRole::TopSurface, top_speed),
            (
                &internal_solid,
                ExtrusionRole::InternalSolidInfill,
                solid_speed,
            ),
        ] {
            if !region.is_empty() {
                let hatched = generate_rectilinear_hatch(
                    region,
                    solid_angle,
                    solid_spacing,
                    role,
                    solid_w,
                    h,
                    speed,
                )?;
                paths.extend(hatched);
            }
        }

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
                        ExtrusionRole::SparseInfill,
                        sparse_w,
                        h,
                        sparse_speed,
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
                            ExtrusionRole::SparseInfill,
                            sparse_w,
                            h,
                            sparse_speed,
                        )?;
                        paths.extend(hatched);
                    }
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

    for path in paths.iter_mut() {
        let area = bead_area_mm2(path.width_mm, path.height_mm);
        if area > 1e-9 && max_vol_flow.is_finite() && max_vol_flow > 0.0 {
            path.speed_mm_s = path.speed_mm_s.min(max_vol_flow / area);
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

    let final_ext_time_s: f64 = paths
        .iter()
        .zip(path_lengths.iter())
        .map(|(p, &len)| len / p.speed_mm_s.max(1e-6))
        .sum();
    let fan_u8 = fan_pct.round().clamp(0.0, 100.0) as u8;
    (fan_u8, final_ext_time_s + travel_time_s)
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

fn generate_rectilinear_hatch(
    region: &[ExPolygon],
    angle_rad: f64,
    spacing_mm: f64,
    role: ExtrusionRole,
    width_mm: f64,
    height_mm: f64,
    speed_mm_s: f64,
) -> Result<Vec<ExtrusionPath>, SliceError> {
    if region.is_empty() || spacing_mm <= 0.0 {
        return Ok(Vec::new());
    }

    let mut bounds: Option<Aabb2> = None;
    for ex in region {
        if let Some(bb) = ex.aabb() {
            bounds = Some(match bounds {
                Some(cur) => cur.union(bb),
                None => bb,
            });
        }
    }
    let Some(aabb) = bounds else {
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

    // Snap first hatch line onto a global phase grid so adjacent regions align seamlessly.
    let k_start = (v_min / spacing_mm).floor() as i64;
    let k_end = (v_max / spacing_mm).ceil() as i64;
    let mut raw_lines: Vec<(Point2, Point2)> = Vec::new();

    let count = k_end.saturating_sub(k_start).clamp(0, 100_000);
    for step in 0..=count {
        let v = ((k_start + step) as f64) * spacing_mm;
        if v < v_min - 1e-4 || v > v_max + 1e-4 {
            continue;
        }
        // Alternate scan direction to naturally support boustrophedon traversal.
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

    // Order clipped segments by rotated (v, u) coordinate and zig-zag chain short connections
    // whose midpoint lies inside the fill region.
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
        let row = (v_mid / spacing_mm).round() as i64;
        tagged.push((row, u_mid, p0, p1));
    }
    tagged.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));

    // Build continuous zig-zag polylines where endpoints are close and midpoint is inside region.
    let max_link_mm = (2.5 * spacing_mm).max(2.0 * width_mm);
    let mut paths: Vec<ExtrusionPath> = Vec::new();
    let mut current_pts: Vec<Point2> = Vec::new();
    let mut flip_dir = false;

    for (i, &(_, _, mut a, mut b)) in tagged.iter().enumerate() {
        let prev_row = i.checked_sub(1).and_then(|j| tagged.get(j)).map(|t| t.0);
        if prev_row != Some(tagged.get(i).map(|t| t.0).unwrap_or(0)) {
            flip_dir = !flip_dir;
        }
        if flip_dir {
            std::mem::swap(&mut a, &mut b);
        }

        if let Some(&last_pt) = current_pts.last() {
            // Re-orient segment if reversing gives shorter connection.
            let d_ab = last_pt.distance_mm(a);
            let d_ba = last_pt.distance_mm(b);
            if d_ba < d_ab {
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
                continue;
            }
            // Flush current polyline and start a new one.
            if current_pts.len() >= 2 {
                paths.push(ExtrusionPath {
                    role,
                    width_mm,
                    height_mm,
                    speed_mm_s,
                    points: current_pts,
                    closed: false,
                });
            }
            current_pts = vec![a, b];
        } else {
            current_pts = vec![a, b];
        }
    }

    if current_pts.len() >= 2 {
        paths.push(ExtrusionPath {
            role,
            width_mm,
            height_mm,
            speed_mm_s,
            points: current_pts,
            closed: false,
        });
    }

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
}
