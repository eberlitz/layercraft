//! Clean-room organic branching (tree) support generator.

use std::f64::consts::TAU;

use layercraft_clip::{JoinType, difference, intersection, offset, simplify, union};
use layercraft_config::PrintConfig;
use layercraft_geom::{ExPolygon, Point2, Polygon, mm_to_nm};
use layercraft_mesh::RawSliceLayer;

use crate::{
    ExtrusionPath, ExtrusionRole, SliceError, bed_clamp_expolygon, bottom_gap_exclusion_range,
    detect_contact_overhangs, generate_rectilinear_hatch, push_closed_poly, stadium_spacing_mm,
};

const CONE_SIMPLIFY_EPS_NM: i64 = 25_000;
const MIN_CONE_AREA_MM2: f64 = 0.02;
const MIN_DISK_AREA_MM2: f64 = 0.04;
const DISK_SEGMENTS: usize = 24;
const MAX_GRID_BINS: usize = 256;

#[derive(Debug, Clone)]
struct BranchNode {
    id: usize,
    x: f64,
    y: f64,
    radius: f64,
    age_layers: usize,
    tip_layers_left: usize,
    weight: f64,
    plate_bound: bool,
}

#[derive(Debug, Clone, Copy)]
struct LayerDisk {
    x: f64,
    y: f64,
    radius: f64,
    is_tip_cap: bool,
}

pub(crate) fn generate_tree_support_paths(
    raw_layers: &[RawSliceLayer],
    config: &PrintConfig,
    blocker_masks: &[Vec<ExPolygon>],
    enforcer_masks: &[Vec<ExPolygon>],
) -> Result<Vec<Vec<ExtrusionPath>>, SliceError> {
    let n = raw_layers.len();
    let mut per_layer = vec![Vec::new(); n];
    let s = &config.process.support;
    let has_enforcer = enforcer_masks.iter().any(|m| !m.is_empty());
    if (!s.enabled && !has_enforcer) || n < 2 {
        return Ok(per_layer);
    }

    let join = JoinType::Miter { limit: 2.0 };
    let bed_box = bed_clamp_expolygon(config)?;
    let bed_slice = std::slice::from_ref(&bed_box);

    // Pass 1: Overhang detection & top Z contact mapping.
    let contact_overhangs =
        detect_contact_overhangs(raw_layers, bed_slice, config, blocker_masks, enforcer_masks)?;
    if contact_overhangs.iter().all(Vec::is_empty) {
        return Ok(per_layer);
    }

    let tree = &s.tree;
    let tip_r = 0.5 * tree.tip_diameter_mm;
    let branch_r = 0.5 * tree.branch_diameter_mm;
    let max_r = (0.5 * tree.max_trunk_diameter_mm).max(branch_r).max(tip_r);

    // Pass 2: Bottom-up printability reachability cone.
    // A branch centerline outside `blocked[j]` can descend cleanly to the build plate at angle <= `max_branch_angle_deg`.
    let r_avoid_cone = 0.5 * s.line_width_mm + s.xy_distance_mm;
    let mut blocked: Vec<Vec<ExPolygon>> = vec![Vec::new(); n];
    if let Some(layer0) = raw_layers.first()
        && !layer0.regions.is_empty()
    {
        let obs0 = offset(&layer0.regions, mm_to_nm(r_avoid_cone)?, join)?;
        if let Some(slot) = blocked.first_mut() {
            *slot = simplify_expolygons(obs0);
        }
    }

    for idx in 1..n {
        let Some(here) = raw_layers.get(idx) else {
            continue;
        };
        let prev_cone = blocked.get(idx - 1).map(Vec::as_slice).unwrap_or(&[]);
        let erode_step = 0.92 * here.plane.height_mm * tree.max_branch_angle_deg.to_radians().tan();
        let mut combined = Vec::new();
        if !here.regions.is_empty() {
            combined.extend(offset(&here.regions, mm_to_nm(r_avoid_cone)?, join)?);
        }
        if !prev_cone.is_empty() && erode_step > 1e-4 {
            combined.extend(offset(prev_cone, -mm_to_nm(erode_step)?, join)?);
        }
        if combined.len() > 1 {
            combined = union(&combined)?;
        }
        if let Some(slot) = blocked.get_mut(idx) {
            *slot = simplify_expolygons(combined);
        }
    }

    // Pass 3: Top-down branch seeding, obstacle avoidance, and organic merging.
    let mut layer_disks: Vec<Vec<LayerDisk>> = vec![Vec::new(); n];
    let mut active: Vec<BranchNode> = Vec::new();
    let mut next_id = 0usize;
    let iface = s.interface_layers as usize;
    let min_seed_spacing = 0.75 * tree.branch_distance_mm;
    let min_seed_clear = s.xy_distance_mm + 0.5 * s.line_width_mm;

    for idx in (0..n).rev() {
        let here_regions = raw_layers
            .get(idx)
            .map(|l| l.regions.as_slice())
            .unwrap_or(&[]);
        let cone_here = blocked.get(idx).map(Vec::as_slice).unwrap_or(&[]);
        let ovhs = contact_overhangs.get(idx).map(Vec::as_slice).unwrap_or(&[]);

        for ovh in ovhs {
            let candidates = collect_seeds_in_expolygon(ovh, tree.branch_distance_mm);
            for (mut cx, mut cy) in candidates {
                let near_existing = active
                    .iter()
                    .any(|b| (b.x - cx).hypot(b.y - cy) < min_seed_spacing);
                if near_existing {
                    continue;
                }

                let mut plate_bound = true;
                if !cone_here.is_empty()
                    && let Some((sdf_c, bx, by)) = signed_dist_to_regions(cx, cy, cone_here)
                    && sdf_c < 0.0
                {
                    let nudge_dist = -sdf_c + 0.05;
                    let mut nudged_ok = false;
                    if nudge_dist <= 0.5 * tree.tip_diameter_mm {
                        let dx = bx - cx;
                        let dy = by - cy;
                        let len = dx.hypot(dy);
                        if len > 1e-9 {
                            let nx = cx + dx * (nudge_dist / len);
                            let ny = cy + dy * (nudge_dist / len);
                            if let Ok(npt) = Point2::from_mm(nx, ny)
                                && ovh.contains_point(npt)
                                && signed_dist_to_regions(nx, ny, cone_here)
                                    .is_none_or(|(s2, _, _)| s2 >= 0.0)
                            {
                                cx = nx;
                                cy = ny;
                                nudged_ok = true;
                            }
                        }
                    }
                    if !nudged_ok {
                        if s.on_build_plate_only {
                            continue;
                        }
                        plate_bound = false;
                    }
                }

                if let Some((sdf_m, _, _)) = signed_dist_to_regions(cx, cy, here_regions)
                    && sdf_m < min_seed_clear
                {
                    continue;
                }

                let (clx, cly) = clamp_to_bed(cx, cy, tip_r, config);
                active.push(BranchNode {
                    id: next_id,
                    x: clx,
                    y: cly,
                    radius: tip_r,
                    age_layers: 0,
                    tip_layers_left: iface,
                    weight: 1.0,
                    plate_bound,
                });
                next_id += 1;
            }
        }

        if let Some(slot) = layer_disks.get_mut(idx) {
            slot.reserve(active.len());
            for b in &active {
                slot.push(LayerDisk {
                    x: b.x,
                    y: b.y,
                    radius: b.radius,
                    is_tip_cap: b.tip_layers_left > 0,
                });
            }
        }

        if idx == 0 {
            break;
        }

        let next_idx = idx - 1;
        let h = raw_layers
            .get(idx)
            .map(|l| l.plane.height_mm)
            .unwrap_or(config.process.layer_height_mm);
        let max_step = (h * tree.max_branch_angle_deg.to_radians().tan()).max(1e-4);
        let taper_dr = h * tree.branch_diameter_angle_deg.to_radians().tan();
        let merge_search_range = (3.5 * tree.branch_distance_mm).max(8.0);
        let next_regions = raw_layers
            .get(next_idx)
            .map(|l| l.regions.as_slice())
            .unwrap_or(&[]);
        let next_cone = blocked.get(next_idx).map(Vec::as_slice).unwrap_or(&[]);

        let mut next_active = Vec::with_capacity(active.len());
        for b in &active {
            let mut next_r = if b.age_layers < 4 {
                let ramp =
                    tip_r + (((b.age_layers + 1) as f64) / 4.0) * (branch_r - tip_r).max(0.0);
                b.radius.max(ramp).min(max_r)
            } else {
                (b.radius + taper_dr).min(max_r)
            };

            let mut target_dx = 0.0;
            let mut target_dy = 0.0;
            let mut best_peer_dist = merge_search_range;
            for other in &active {
                if other.id == b.id || other.plate_bound != b.plate_bound {
                    continue;
                }
                let d = (other.x - b.x).hypot(other.y - b.y);
                if d > 1e-6 && d < best_peer_dist {
                    best_peer_dist = d;
                    let step = (0.65 * max_step).min(0.5 * d);
                    target_dx = (other.x - b.x) * (step / d);
                    target_dy = (other.y - b.y) * (step / d);
                }
            }

            let mut nx = b.x + target_dx;
            let mut ny = b.y + target_dy;

            if b.plate_bound && !next_cone.is_empty() {
                (nx, ny) = push_outside_cone(b.x, b.y, nx, ny, next_cone, max_step);
            }

            let mut keep = true;
            if !next_regions.is_empty() {
                let req = next_r + s.xy_distance_mm;
                if let Some((sdf, bx, by)) = signed_dist_to_regions(nx, ny, next_regions)
                    && sdf < req
                {
                    let (px, py) =
                        push_toward_clearance(b.x, b.y, nx, ny, sdf, bx, by, req + 0.02, max_step);
                    nx = px;
                    ny = py;
                    if let Some((sdf2, _, _)) = signed_dist_to_regions(nx, ny, next_regions)
                        && sdf2 < req
                    {
                        let min_keep_r = tip_r + s.xy_distance_mm;
                        if sdf2 >= min_keep_r {
                            next_r = (sdf2 - s.xy_distance_mm).clamp(tip_r, next_r);
                        } else if !b.plate_bound || sdf2 <= 0.0 {
                            keep = false;
                        } else {
                            next_r = tip_r;
                        }
                    }
                }
            }

            if !keep {
                continue;
            }

            let (clx, cly) = clamp_to_bed(nx, ny, next_r, config);
            next_active.push(BranchNode {
                id: b.id,
                x: clx,
                y: cly,
                radius: next_r,
                age_layers: b.age_layers + 1,
                tip_layers_left: b.tip_layers_left.saturating_sub(1),
                weight: b.weight,
                plate_bound: b.plate_bound,
            });
        }

        merge_close_branches(
            &mut next_active,
            max_r,
            s.xy_distance_mm,
            next_regions,
            next_cone,
        );
        active = next_active;
    }

    // Pass 4: Union branch disks per layer, clip exact model/Z-gap clearances, emit hollow tubes & interface caps.
    for idx in 0..n {
        let Some(disks) = layer_disks.get(idx) else {
            continue;
        };
        if disks.is_empty() {
            continue;
        }
        let h = raw_layers
            .get(idx)
            .map(|l| l.plane.height_mm)
            .unwrap_or(config.process.layer_height_mm);

        let mut disk_polys = Vec::with_capacity(disks.len());
        let mut tip_polys = Vec::new();
        for d in disks {
            let eff_r = if idx == 0 {
                d.radius + tree.trunk_base_flare_mm
            } else {
                d.radius
            };
            if let Ok(poly) = disk_expolygon(d.x, d.y, eff_r) {
                disk_polys.push(poly);
            }
            if d.is_tip_cap
                && let Ok(tpoly) = disk_expolygon(d.x, d.y, d.radius)
            {
                tip_polys.push(tpoly);
            }
        }
        if disk_polys.is_empty() {
            continue;
        }
        let tree_union = union(&disk_polys)?;

        let mut excl_sources = if let Some(cur_layer) = raw_layers.get(idx) {
            offset(&cur_layer.regions, mm_to_nm(s.xy_distance_mm)?, join)?
        } else {
            Vec::new()
        };
        for j in bottom_gap_exclusion_range(raw_layers, idx, s.bottom_z_distance_mm) {
            if let Some(rl) = raw_layers.get(j) {
                excl_sources.extend(rl.regions.iter().cloned());
            }
        }
        if let Some(bm) = blocker_masks.get(idx)
            && !bm.is_empty()
        {
            excl_sources.extend(bm.iter().cloned());
        }
        let excl = if excl_sources.is_empty() {
            Vec::new()
        } else {
            union(&excl_sources)?
        };

        let clipped = if excl.is_empty() {
            tree_union
        } else {
            difference(&tree_union, &excl)?
        };
        let mut tree_polys = intersection(&clipped, bed_slice)?;
        tree_polys.retain(|ex| ex.area_mm2() >= MIN_DISK_AREA_MM2);
        if tree_polys.is_empty() {
            continue;
        }

        let interface_here = if iface > 0 {
            let mut iface_sources = tip_polys;
            let top_end = (idx + iface - 1).min(n.saturating_sub(1));
            for j in idx..=top_end {
                if let Some(co) = contact_overhangs.get(j) {
                    iface_sources.extend(co.iter().cloned());
                }
            }
            if iface_sources.is_empty() {
                Vec::new()
            } else {
                let mut iu = intersection(&tree_polys, &union(&iface_sources)?)?;
                iu.retain(|ex| ex.area_mm2() >= MIN_DISK_AREA_MM2);
                iu
            }
        } else {
            Vec::new()
        };

        let base_here = if interface_here.is_empty() {
            tree_polys
        } else {
            difference(&tree_polys, &interface_here)?
        };

        let mut layer_paths = Vec::new();
        let base_speed = if idx == 0 {
            s.speed_mm_s
                .min(config.process.effective_first_layer_infill_speed_mm_s())
        } else {
            s.speed_mm_s
        };
        let iface_speed = if idx == 0 {
            s.interface_speed_mm_s
                .min(config.process.effective_first_layer_infill_speed_mm_s())
        } else {
            s.interface_speed_mm_s
        };

        if !base_here.is_empty() {
            let spacing = stadium_spacing_mm(s.line_width_mm, h);
            let wall_count = tree.branch_wall_loops.max(1) as usize;
            for w_idx in (0..wall_count).rev() {
                let inset = 0.5 * s.line_width_mm + (w_idx as f64) * spacing;
                let rings = offset(&base_here, -mm_to_nm(inset)?, join)?;
                for ex in rings {
                    push_closed_poly(
                        &ex.outer,
                        ExtrusionRole::Support,
                        s.line_width_mm,
                        h,
                        base_speed,
                        &mut layer_paths,
                    );
                    for hole in &ex.holes {
                        push_closed_poly(
                            hole,
                            ExtrusionRole::Support,
                            s.line_width_mm,
                            h,
                            base_speed,
                            &mut layer_paths,
                        );
                    }
                }
            }
            if idx == 0 {
                let floor_inset = 0.5 * s.line_width_mm + 0.5 * spacing;
                let floor_core = offset(&base_here, -mm_to_nm(floor_inset)?, join)?;
                if !floor_core.is_empty() {
                    let hatched = generate_rectilinear_hatch(
                        &floor_core,
                        0.0,
                        spacing,
                        0.0,
                        false,
                        ExtrusionRole::Support,
                        s.line_width_mm,
                        h,
                        base_speed,
                        1.0,
                    )?;
                    layer_paths.extend(hatched);
                }
            }
        }

        if !interface_here.is_empty() {
            let irings = offset(&interface_here, -mm_to_nm(0.5 * s.line_width_mm)?, join)?;
            for ex in irings {
                push_closed_poly(
                    &ex.outer,
                    ExtrusionRole::SupportInterface,
                    s.line_width_mm,
                    h,
                    iface_speed,
                    &mut layer_paths,
                );
                for hole in &ex.holes {
                    push_closed_poly(
                        hole,
                        ExtrusionRole::SupportInterface,
                        s.line_width_mm,
                        h,
                        iface_speed,
                        &mut layer_paths,
                    );
                }
            }
            let dense_floor = stadium_spacing_mm(s.line_width_mm, h);
            let iface_spacing = if s.interface_spacing_mm <= 1e-6 {
                dense_floor
            } else {
                (s.line_width_mm + s.interface_spacing_mm).max(dense_floor)
            };
            let angle = if idx % 2 == 0 {
                std::f64::consts::FRAC_PI_4
            } else {
                -std::f64::consts::FRAC_PI_4
            };
            let hatched = generate_rectilinear_hatch(
                &interface_here,
                angle,
                iface_spacing,
                0.0,
                false,
                ExtrusionRole::SupportInterface,
                s.line_width_mm,
                h,
                iface_speed,
                1.0,
            )?;
            layer_paths.extend(hatched);
        }

        if let Some(slot) = per_layer.get_mut(idx) {
            *slot = layer_paths;
        }
    }

    Ok(per_layer)
}

fn simplify_expolygons(polys: Vec<ExPolygon>) -> Vec<ExPolygon> {
    let mut out = Vec::with_capacity(polys.len());
    for ex in polys {
        let mut outer = simplify(&ex.outer, CONE_SIMPLIFY_EPS_NM);
        outer.ensure_ccw();
        if outer.points.len() < 3 || outer.area_mm2() < MIN_CONE_AREA_MM2 {
            continue;
        }
        let mut holes = Vec::with_capacity(ex.holes.len());
        for h in &ex.holes {
            let mut sh = simplify(h, CONE_SIMPLIFY_EPS_NM);
            sh.ensure_cw();
            if sh.points.len() >= 3 && sh.area_mm2() >= MIN_CONE_AREA_MM2 {
                holes.push(sh);
            }
        }
        out.push(ExPolygon::new(outer, holes));
    }
    out
}

fn disk_expolygon(cx: f64, cy: f64, r_mm: f64) -> Result<ExPolygon, SliceError> {
    let mut pts = Vec::with_capacity(DISK_SEGMENTS);
    for i in 0..DISK_SEGMENTS {
        let ang = TAU * (i as f64) / (DISK_SEGMENTS as f64);
        pts.push(Point2::from_mm(
            cx + r_mm * ang.cos(),
            cy + r_mm * ang.sin(),
        )?);
    }
    Ok(ExPolygon::new(Polygon::new(pts), Vec::new()))
}

fn clamp_to_bed(x: f64, y: f64, r: f64, config: &PrintConfig) -> (f64, f64) {
    let bw = config.machine.bed_width_mm;
    let bd = config.machine.bed_depth_mm;
    let margin = (0.5 + r).min(0.45 * bw.min(bd));
    (
        x.clamp(margin, (bw - margin).max(margin)),
        y.clamp(margin, (bd - margin).max(margin)),
    )
}

fn collect_seeds_in_expolygon(ex: &ExPolygon, spacing_mm: f64) -> Vec<(f64, f64)> {
    let Some(aabb) = ex.aabb() else {
        return Vec::new();
    };
    let (min_x, min_y) = aabb.min.to_mm();
    let (max_x, max_y) = aabb.max.to_mm();
    let step = spacing_mm.max(0.5);
    let nx = (((max_x - min_x) / step).ceil() as usize).clamp(1, MAX_GRID_BINS);
    let ny = (((max_y - min_y) / step).ceil() as usize).clamp(1, MAX_GRID_BINS);
    let dx = (max_x - min_x).max(1e-6) / (nx as f64);
    let dy = (max_y - min_y).max(1e-6) / (ny as f64);

    let mut seeds = Vec::new();
    for iy in 0..ny {
        let y0 = min_y + (iy as f64) * dy;
        let y1 = min_y + ((iy + 1) as f64) * dy;
        for ix in 0..nx {
            let x0 = min_x + (ix as f64) * dx;
            let x1 = min_x + ((ix + 1) as f64) * dx;
            if let Some(pt) = probe_cell_in_expolygon(ex, x0, y0, x1, y1) {
                seeds.push(pt);
            }
        }
    }

    if seeds.is_empty()
        && let Some(pt) = fallback_seed_point(ex)
    {
        seeds.push(pt);
    }
    seeds
}

fn probe_cell_in_expolygon(
    ex: &ExPolygon,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
) -> Option<(f64, f64)> {
    for &fy in &[0.5, 0.25, 0.75] {
        for &fx in &[0.5, 0.25, 0.75] {
            let px = x0 + fx * (x1 - x0);
            let py = y0 + fy * (y1 - y0);
            if let Ok(pt) = Point2::from_mm(px, py)
                && ex.contains_point(pt)
            {
                return Some((px, py));
            }
        }
    }

    let pts = &ex.outer.points;
    let m = pts.len();
    for i in 0..m {
        let (Some(&p_prev), Some(&p_curr), Some(&p_next)) =
            (pts.get((i + m - 1) % m), pts.get(i), pts.get((i + 1) % m))
        else {
            continue;
        };
        let (vx, vy) = p_curr.to_mm();
        if vx < x0 - 0.05 || vx > x1 + 0.05 || vy < y0 - 0.05 || vy > y1 + 0.05 {
            continue;
        }
        let (ax, ay) = p_prev.to_mm();
        let (bx, by) = p_next.to_mm();
        for (ex0, ey0, ex1, ey1) in [(vx, vy, bx, by), (ax, ay, vx, vy)] {
            let edx = ex1 - ex0;
            let edy = ey1 - ey0;
            let len = edx.hypot(edy);
            if len <= 1e-6 {
                continue;
            }
            let mx = 0.5 * (ex0 + ex1);
            let my = 0.5 * (ey0 + ey1);
            if mx < x0 - 0.05 || mx > x1 + 0.05 || my < y0 - 0.05 || my > y1 + 0.05 {
                continue;
            }
            let inx = -edy / len;
            let iny = edx / len;
            for d in [0.04, 0.12] {
                let tx = mx + inx * d;
                let ty = my + iny * d;
                if let Ok(pt) = Point2::from_mm(tx, ty)
                    && ex.contains_point(pt)
                {
                    return Some((tx, ty));
                }
            }
        }
    }
    None
}

fn fallback_seed_point(ex: &ExPolygon) -> Option<(f64, f64)> {
    let aabb = ex.aabb()?;
    let (min_x, min_y) = aabb.min.to_mm();
    let (max_x, max_y) = aabb.max.to_mm();
    for step_y in 0..=4 {
        for step_x in 0..=4 {
            let fx = (step_x as f64 + 0.5) / 5.0;
            let fy = (step_y as f64 + 0.5) / 5.0;
            let px = min_x + fx * (max_x - min_x);
            let py = min_y + fy * (max_y - min_y);
            if let Ok(pt) = Point2::from_mm(px, py)
                && ex.contains_point(pt)
            {
                return Some((px, py));
            }
        }
    }
    probe_cell_in_expolygon(ex, min_x, min_y, max_x, max_y)
}

fn signed_dist_to_regions(x: f64, y: f64, regions: &[ExPolygon]) -> Option<(f64, f64, f64)> {
    if regions.is_empty() {
        return None;
    }
    let pt = Point2::from_mm(x, y).ok()?;
    let mut inside = false;
    let mut best_d2 = f64::INFINITY;
    let mut best_qx = x;
    let mut best_qy = y;

    for ex in regions {
        if !inside && ex.contains_point(pt) {
            inside = true;
        }
        if let Some(bb) = ex.aabb() {
            let (x0, y0) = bb.min.to_mm();
            let (x1, y1) = bb.max.to_mm();
            let dx = if x < x0 {
                x0 - x
            } else if x > x1 {
                x - x1
            } else {
                0.0
            };
            let dy = if y < y0 {
                y0 - y
            } else if y > y1 {
                y - y1
            } else {
                0.0
            };
            if dx * dx + dy * dy >= best_d2 {
                continue;
            }
        }

        let mut scan_ring = |poly: &Polygon| {
            let n = poly.points.len();
            for i in 0..n {
                let (Some(&a), Some(&b)) = (poly.points.get(i), poly.points.get((i + 1) % n))
                else {
                    continue;
                };
                let (ax, ay) = a.to_mm();
                let (bx, by) = b.to_mm();
                let edx = bx - ax;
                let edy = by - ay;
                let len2 = edx * edx + edy * edy;
                let t = if len2 > 1e-18 {
                    (((x - ax) * edx + (y - ay) * edy) / len2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let qx = ax + t * edx;
                let qy = ay + t * edy;
                let d2 = (x - qx) * (x - qx) + (y - qy) * (y - qy);
                if d2 < best_d2 {
                    best_d2 = d2;
                    best_qx = qx;
                    best_qy = qy;
                }
            }
        };

        scan_ring(&ex.outer);
        for hole in &ex.holes {
            scan_ring(hole);
        }
    }

    if !best_d2.is_finite() {
        return None;
    }
    let dist = best_d2.sqrt();
    let signed = if inside { -dist } else { dist };
    Some((signed, best_qx, best_qy))
}

fn clamp_step_from_anchor(ax: f64, ay: f64, tx: f64, ty: f64, max_step: f64) -> (f64, f64) {
    let dx = tx - ax;
    let dy = ty - ay;
    let dist = dx.hypot(dy);
    if dist <= max_step || dist <= 1e-12 {
        (tx, ty)
    } else {
        (ax + dx * (max_step / dist), ay + dy * (max_step / dist))
    }
}

fn push_outside_cone(
    anchor_x: f64,
    anchor_y: f64,
    cand_x: f64,
    cand_y: f64,
    cone: &[ExPolygon],
    max_step: f64,
) -> (f64, f64) {
    let Some((sdf_c, bx, by)) = signed_dist_to_regions(cand_x, cand_y, cone) else {
        return (cand_x, cand_y);
    };
    if sdf_c >= 0.0 {
        return (cand_x, cand_y);
    }
    let dx = bx - cand_x;
    let dy = by - cand_y;
    let len = dx.hypot(dy);
    if len > 1e-9 {
        let want = len + 0.02;
        let tx = cand_x + dx * (want / len);
        let ty = cand_y + dy * (want / len);
        let (px, py) = clamp_step_from_anchor(anchor_x, anchor_y, tx, ty, max_step);
        if signed_dist_to_regions(px, py, cone).is_none_or(|(s2, _, _)| s2 >= 0.0) {
            return (px, py);
        }
    }

    if let Some((sdf_a, abx, aby)) = signed_dist_to_regions(anchor_x, anchor_y, cone)
        && sdf_a < 0.0
    {
        let adx = abx - anchor_x;
        let ady = aby - anchor_y;
        let alen = adx.hypot(ady);
        if alen > 1e-9 {
            let want = alen + 0.02;
            let tx = anchor_x + adx * (want / alen);
            let ty = anchor_y + ady * (want / alen);
            return clamp_step_from_anchor(anchor_x, anchor_y, tx, ty, max_step);
        }
    }
    (cand_x, cand_y)
}

#[allow(clippy::too_many_arguments)]
fn push_toward_clearance(
    anchor_x: f64,
    anchor_y: f64,
    cand_x: f64,
    cand_y: f64,
    sdf: f64,
    bx: f64,
    by: f64,
    target_clearance: f64,
    max_step: f64,
) -> (f64, f64) {
    let need = target_clearance - sdf;
    let (vx, vy) = if sdf < 0.0 {
        (bx - cand_x, by - cand_y)
    } else {
        (cand_x - bx, cand_y - by)
    };
    let len = vx.hypot(vy);
    if len > 1e-9 {
        let tx = cand_x + vx * (need / len);
        let ty = cand_y + vy * (need / len);
        clamp_step_from_anchor(anchor_x, anchor_y, tx, ty, max_step)
    } else {
        (cand_x, cand_y)
    }
}

fn merge_close_branches(
    nodes: &mut Vec<BranchNode>,
    max_r: f64,
    xy_distance_mm: f64,
    regions: &[ExPolygon],
    cone: &[ExPolygon],
) {
    loop {
        let m = nodes.len();
        if m < 2 {
            break;
        }
        let mut best_pair: Option<(usize, usize, f64, f64, f64)> = None;
        let mut best_dist = f64::INFINITY;

        for i in 0..m {
            for j in (i + 1)..m {
                let (Some(a), Some(b)) = (nodes.get(i), nodes.get(j)) else {
                    continue;
                };
                if a.plate_bound != b.plate_bound {
                    continue;
                }
                let dist = (a.x - b.x).hypot(a.y - b.y);
                let merge_thresh = 0.70 * (a.radius + b.radius);
                if dist > merge_thresh || dist >= best_dist {
                    continue;
                }
                let w_sum = (a.radius * a.radius + b.radius * b.radius).max(1e-9);
                let wa = (a.radius * a.radius) / w_sum;
                let wb = 1.0 - wa;
                let mx = wa * a.x + wb * b.x;
                let my = wa * a.y + wb * b.y;
                let mut mr = a.radius.hypot(b.radius).min(max_r);

                if a.plate_bound
                    && !cone.is_empty()
                    && signed_dist_to_regions(mx, my, cone).is_some_and(|(sc, _, _)| sc < 0.0)
                {
                    continue;
                }
                if !regions.is_empty()
                    && let Some((sdf, _, _)) = signed_dist_to_regions(mx, my, regions)
                    && sdf < mr + xy_distance_mm
                {
                    let avail = sdf - xy_distance_mm;
                    if avail >= a.radius.max(b.radius) {
                        mr = avail.min(max_r);
                    } else {
                        continue;
                    }
                }
                best_dist = dist;
                best_pair = Some((i, j, mx, my, mr));
            }
        }

        let Some((i, j, mx, my, mr)) = best_pair else {
            break;
        };
        let node_j = nodes.remove(j);
        if let Some(node_i) = nodes.get_mut(i) {
            node_i.x = mx;
            node_i.y = my;
            node_i.radius = mr;
            node_i.age_layers = node_i.age_layers.max(node_j.age_layers);
            node_i.tip_layers_left = 0;
            node_i.weight += node_j.weight;
        }
    }
}
