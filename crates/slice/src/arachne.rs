//! Arachne variable-width wall generation built over `layercraft-clip` medial-axis skeletons.

use std::f64::consts::FRAC_PI_4;

use layercraft_clip::{
    JoinType, MedialAxis, MedialEdge, clip_polylines, clip_polylines_difference, intersection,
    medial_axis, offset, union,
};
use layercraft_config::PrintConfig;
use layercraft_geom::{ExPolygon, Point2, Polygon, mm_to_nm};

use crate::{
    ExtrusionPath, ExtrusionRole, SliceError, build_perimeters_classic, wall_order_indices,
};

const EDGE_SUBDIVIDE_MM: f64 = 0.40;
const MIN_COLLAPSED_EDGE_MM: f64 = 0.02;
const SIMPLIFY_XY_EPS_MM: f64 = 0.005;
const SIMPLIFY_W_EPS_MM: f64 = 0.004;
const MAX_SEG_WIDTH_DELTA_MM: f64 = 0.04;
const UNIFORM_WIDTH_TOL_MM: f64 = 0.004;
const CHAIN_STITCH_TOL_MM: f64 = 0.025;
const MITER_SHIFT_LIMIT: f64 = 1.6;

type WidthPiece = (Vec<(f64, f64)>, Vec<f64>);
type StitchedChain = (Vec<(f64, f64)>, Vec<f64>, bool);

#[derive(Debug, Clone)]
pub(crate) struct BeadingParams {
    pub wall_loops: usize,
    pub outer_w: f64,
    pub inner_w: f64,
    pub overlap: f64,
    pub w_min_outer: f64,
    pub w_max_outer: f64,
    pub w_min_inner: f64,
    pub w_max_inner: f64,
    pub min_feature: f64,
    pub distribution_count: usize,
    pub t_infill: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct BeadingPlan {
    pub pair_widths: Vec<f64>,
    pub center_width: Option<f64>,
}

impl BeadingParams {
    pub(crate) fn from_config(layer_height_mm: f64, config: &PrintConfig) -> Self {
        let wall_loops = config.process.wall_loops as usize;
        let outer_w = config.process.outer_wall_width_mm;
        let inner_w = config.process.inner_wall_width_mm;
        let solid_w = config.process.solid_infill_width_mm;
        let overlap = layer_height_mm * (1.0 - FRAC_PI_4);
        let ar = &config.process.arachne;

        let w_min_outer = (ar.min_bead_width_ratio * outer_w).max(0.05);
        let w_max_outer = (ar.max_bead_width_ratio * outer_w).max(w_min_outer);
        let w_min_inner = (ar.min_bead_width_ratio * inner_w).max(0.05);
        let w_max_inner = (ar.max_bead_width_ratio * inner_w).max(w_min_inner);
        let min_feature = (ar.min_feature_size_ratio * outer_w).max(0.02);

        let mut p = Self {
            wall_loops,
            outer_w,
            inner_w,
            overlap,
            w_min_outer,
            w_max_outer,
            w_min_inner,
            w_max_inner,
            min_feature,
            distribution_count: ar.wall_distribution_count.max(1) as usize,
            t_infill: 0.0,
        };
        let n_max = 2 * wall_loops;
        p.t_infill = p.t_nom(n_max) + 0.5 * solid_w.min(inner_w);
        p
    }

    pub(crate) fn t_nom(&self, k: usize) -> f64 {
        if k == 0 {
            0.0
        } else if k == 1 {
            self.outer_w
        } else {
            2.0 * self.outer_w + ((k - 2) as f64) * self.inner_w - ((k - 1) as f64) * self.overlap
        }
    }

    pub(crate) fn thresh(&self, k: usize) -> f64 {
        if k <= 1 {
            return self.min_feature;
        }
        let t2 = (2.0 * self.w_min_outer - self.overlap).max(self.min_feature + 1e-4);
        if k == 2 {
            return t2;
        }
        let mut prev = t2;
        for j in 3..=k {
            let tj = (self.t_nom(j - 1) + self.w_min_inner - self.overlap)
                .max(prev + 0.05 * self.inner_w);
            prev = tj;
        }
        prev
    }

    pub(crate) fn nominal_inset(&self, pair_idx: usize) -> f64 {
        let step_pitch =
            (0.5 * self.outer_w + 0.5 * self.inner_w - self.overlap).max(0.05 * self.inner_w);
        let inner_pitch = (self.inner_w - self.overlap).max(0.05 * self.inner_w);
        if pair_idx == 0 {
            0.5 * self.outer_w
        } else {
            0.5 * self.outer_w + step_pitch + ((pair_idx - 1) as f64) * inner_pitch
        }
    }

    pub(crate) fn nominal_inner_edge_inset(&self) -> f64 {
        if self.wall_loops == 0 {
            0.0
        } else {
            let last_idx = self.wall_loops - 1;
            let last_w = if last_idx == 0 {
                self.outer_w
            } else {
                self.inner_w
            };
            self.nominal_inset(last_idx) + 0.5 * last_w
        }
    }
}

pub(crate) fn compute_beading(thickness_mm: f64, params: &BeadingParams) -> BeadingPlan {
    let n = params.wall_loops;
    if n == 0 || thickness_mm < params.thresh(1) {
        return BeadingPlan {
            pair_widths: Vec::new(),
            center_width: None,
        };
    }

    let nominal_pairs = || -> Vec<f64> {
        let mut pw = Vec::with_capacity(n);
        for m in 0..n {
            pw.push(if m == 0 {
                params.outer_w
            } else {
                params.inner_w
            });
        }
        pw
    };

    if thickness_mm >= params.t_infill {
        return BeadingPlan {
            pair_widths: nominal_pairs(),
            center_width: None,
        };
    }

    let max_k = 2 * n;
    let mut best_k = 1usize;
    for k in 2..=max_k {
        if thickness_mm >= params.thresh(k) {
            best_k = k;
        } else {
            break;
        }
    }

    if best_k == 1 {
        let w = thickness_mm.clamp(params.w_min_outer, params.w_max_outer);
        return BeadingPlan {
            pair_widths: Vec::new(),
            center_width: Some(w),
        };
    }

    if best_k == 2 {
        let w =
            (0.5 * (thickness_mm + params.overlap)).clamp(params.w_min_outer, params.w_max_outer);
        return BeadingPlan {
            pair_widths: vec![w],
            center_width: None,
        };
    }

    let num_layers = best_k.div_ceil(2);
    let has_center = best_k % 2 == 1;

    struct LayerSpec {
        w: f64,
        min_w: f64,
        max_w: f64,
        mult: f64,
    }

    let mut layers = Vec::with_capacity(num_layers);
    for idx in 0..num_layers {
        let is_outer = idx == 0;
        let mult = if has_center && idx + 1 == num_layers {
            1.0
        } else {
            2.0
        };
        let (w, min_w, max_w) = if is_outer {
            (params.outer_w, params.w_min_outer, params.w_max_outer)
        } else {
            (params.inner_w, params.w_min_inner, params.w_max_inner)
        };
        layers.push(LayerSpec {
            w,
            min_w,
            max_w,
            mult,
        });
    }

    let target_sum = thickness_mm + ((best_k - 1) as f64) * params.overlap;
    let init_sum: f64 = layers.iter().map(|l| l.w * l.mult).sum();
    let mut remaining = target_sum - init_sum;

    let mut active_count = params.distribution_count.min(num_layers);
    for _ in 0..=num_layers + 2 {
        if remaining.abs() <= 1e-7 {
            break;
        }
        let start_idx = num_layers.saturating_sub(active_count);
        let total_weight: f64 = layers
            .iter()
            .skip(start_idx)
            .filter(|l| {
                if remaining > 0.0 {
                    l.w < l.max_w - 1e-9
                } else {
                    l.w > l.min_w + 1e-9
                }
            })
            .map(|l| l.mult)
            .sum();

        if total_weight <= 1e-9 {
            if active_count < num_layers {
                active_count += 1;
                continue;
            }
            break;
        }

        let delta = remaining / total_weight;
        let mut applied = 0.0;
        for l in layers.iter_mut().skip(start_idx) {
            let old = l.w;
            let next = (old + delta).clamp(l.min_w, l.max_w);
            l.w = next;
            applied += (next - old) * l.mult;
        }
        remaining -= applied;
        if applied.abs() <= 1e-9 {
            if active_count < num_layers {
                active_count += 1;
            } else {
                break;
            }
        }
    }

    let pair_count = best_k / 2;
    let pair_widths: Vec<f64> = layers.iter().take(pair_count).map(|l| l.w).collect();
    let center_width = if has_center {
        layers.last().map(|l| l.w)
    } else {
        None
    };

    BeadingPlan {
        pair_widths,
        center_width,
    }
}

pub(crate) fn build_perimeters_arachne(
    regions: &[ExPolygon],
    layer_idx: usize,
    layer_height_mm: f64,
    config: &PrintConfig,
) -> Result<(Vec<ExtrusionPath>, Vec<ExPolygon>), SliceError> {
    if regions.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    if config.process.wall_loops == 0 {
        return Ok((Vec::new(), regions.to_vec()));
    }

    let params = BeadingParams::from_config(layer_height_mm, config);
    let is_first = layer_idx == 0;
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

    let mut all_paths = Vec::new();
    let mut all_infill = Vec::new();

    for region in regions {
        let single = std::slice::from_ref(region);
        let Ok(medial) = medial_axis(region) else {
            let (fallback_paths, fallback_infill) =
                build_perimeters_classic(single, layer_idx, layer_height_mm, config)?;
            all_paths.extend(fallback_paths);
            all_infill.extend(fallback_infill);
            continue;
        };

        let n = params.wall_loops;
        let support_join = JoinType::Miter { limit: 1.5 };
        let miter2 = JoinType::Miter { limit: 2.0 };

        let mut r_pair: Vec<Vec<ExPolygon>> = Vec::with_capacity(n);
        for m in 0..n {
            let hr = 0.5 * params.thresh(2 * m + 2);
            let core = offset(single, -mm_to_nm(hr)?, support_join)?;
            if core.is_empty() {
                r_pair.push(Vec::new());
            } else {
                let expanded = offset(&core, mm_to_nm(hr + 0.005)?, support_join)?;
                r_pair.push(intersection(single, &expanded)?);
            }
        }

        let mut paired_by_ring: Vec<Vec<ExtrusionPath>> = vec![Vec::new(); n];
        let mut odd_by_ring: Vec<Vec<ExtrusionPath>> = vec![Vec::new(); n];

        // Paired loops from innermost (n - 1) outward to m = 0.
        for m in (0..n).rev() {
            let Some(sup) = r_pair.get(m) else {
                continue;
            };
            if sup.is_empty() {
                continue;
            }
            let d_nom = params.nominal_inset(m);
            let base_polys = offset(sup, -mm_to_nm(d_nom)?, miter2)?;
            let role = if m == 0 {
                ExtrusionRole::OuterWall
            } else {
                ExtrusionRole::InnerWall
            };
            let speed = if m == 0 { outer_speed } else { inner_speed };
            let Some(target_dst) = paired_by_ring.get_mut(m) else {
                continue;
            };

            let min_r = (0.45 * params.thresh(2 * m + 2)).max(0.02);

            for ex in base_polys {
                emit_paired_contour(
                    &ex.outer,
                    &medial,
                    &params,
                    m,
                    d_nom,
                    min_r,
                    role,
                    layer_height_mm,
                    speed,
                    target_dst,
                );
                for hole in &ex.holes {
                    emit_paired_contour(
                        hole,
                        &medial,
                        &params,
                        m,
                        d_nom,
                        min_r,
                        role,
                        layer_height_mm,
                        speed,
                        target_dst,
                    );
                }
            }
        }

        // Odd medial center beads indexed by wall ring `m`.
        for m in 0..n {
            let t_lo = params.thresh(2 * m + 1);
            let t_hi = params.thresh(2 * m + 2) - 1e-4;
            let nom_w = if m == 0 {
                params.outer_w
            } else {
                params.inner_w
            };
            let min_w = if m == 0 {
                params.w_min_outer
            } else {
                params.w_min_inner
            };

            let d_nom = params.nominal_inset(m);
            let excl_inset = (d_nom - 0.5 * min_w).max(0.0);
            let excl_polys = match r_pair.get(m) {
                Some(r) if !r.is_empty() => {
                    if excl_inset <= 1e-4 {
                        r.clone()
                    } else {
                        let shrunk = offset(r, -mm_to_nm(excl_inset)?, support_join)?;
                        if shrunk.is_empty() { r.clone() } else { shrunk }
                    }
                }
                _ => Vec::new(),
            };

            let incl_polys = if m == 0 {
                None
            } else {
                match r_pair.get(m - 1) {
                    Some(prev) if !prev.is_empty() => Some(prev.as_slice()),
                    _ => continue,
                }
            };

            let mut raw_pieces: Vec<WidthPiece> = Vec::new();
            for edge in &medial.edges {
                let threshold_runs = extract_threshold_runs(edge, t_lo);
                if threshold_runs.is_empty() {
                    continue;
                }
                let nm_runs: Vec<Vec<Point2>> = threshold_runs
                    .iter()
                    .filter_map(|run| {
                        let pts: Vec<Point2> = run
                            .iter()
                            .filter_map(|&(x, y, _)| Point2::from_mm(x, y).ok())
                            .collect();
                        (pts.len() >= 2).then_some(pts)
                    })
                    .collect();
                if nm_runs.is_empty() {
                    continue;
                }

                let mut clipped = if excl_polys.is_empty() {
                    nm_runs
                } else {
                    clip_polylines_difference(&nm_runs, &excl_polys)?
                };
                if clipped.is_empty() {
                    continue;
                }
                if let Some(incl) = incl_polys {
                    clipped = clip_polylines(&clipped, incl)?;
                }

                for piece in clipped {
                    let sampled = subdivide_open_poly(&piece, EDGE_SUBDIVIDE_MM);
                    if sampled.len() < 2 {
                        continue;
                    }
                    let mut pts_mm = Vec::with_capacity(sampled.len());
                    let mut widths = Vec::with_capacity(sampled.len());
                    for (px, py) in sampled {
                        let t_local = project_thickness_on_edge(px, py, edge).unwrap_or(t_lo);
                        let t_clamped = t_local.clamp(t_lo, t_hi.max(t_lo));
                        let plan = compute_beading(t_clamped, &params);
                        let w = plan.center_width.unwrap_or(nom_w);
                        pts_mm.push((px, py));
                        widths.push(w);
                    }
                    raw_pieces.push((pts_mm, widths));
                }
            }

            let stitched = stitch_open_chains(raw_pieces, CHAIN_STITCH_TOL_MM);
            let role = if m == 0 {
                ExtrusionRole::OuterWall
            } else {
                ExtrusionRole::InnerWall
            };
            let speed = if m == 0 { outer_speed } else { inner_speed };
            let min_piece_len = (0.5 * min_w).max(0.15);
            let Some(dst) = odd_by_ring.get_mut(m) else {
                continue;
            };

            for (pts, widths, closed) in stitched {
                let len = polyline_len_mm(&pts, closed);
                if (!closed && len < min_piece_len) || pts.len() < 2 {
                    continue;
                }
                emit_polyline_with_widths(&pts, &widths, closed, role, layer_height_mm, speed, dst);
            }
        }

        // Emit perimeter rings in configured WallOrder (paired contour + center single-pass bead).
        for m in wall_order_indices(n, config.process.wall_order) {
            if let Some(paired) = paired_by_ring.get_mut(m) {
                all_paths.append(paired);
            }
            if let Some(odd) = odd_by_ring.get_mut(m) {
                all_paths.append(odd);
            }
        }

        // Residual infill region excluding all thickened zero-gap walls.
        let half_infill = 0.5 * params.t_infill;
        let core_infill = offset(single, -mm_to_nm(half_infill)?, support_join)?;
        if !core_infill.is_empty() {
            let inner_edge_inset = params.nominal_inner_edge_inset();
            let infill_overlap =
                config.process.infill_wall_overlap_ratio * config.process.solid_infill_width_mm;
            let expand_mm = (half_infill - inner_edge_inset).max(0.0) + infill_overlap;
            let expanded = offset(&core_infill, mm_to_nm(expand_mm)?, miter2)?;
            let max_infill_inset = (inner_edge_inset - infill_overlap).max(0.0);
            let allowed = if max_infill_inset <= 1e-4 {
                single.to_vec()
            } else {
                offset(single, -mm_to_nm(max_infill_inset)?, miter2)?
            };
            all_infill.extend(intersection(&expanded, &allowed)?);
        }
    }

    let infill_area = if all_infill.len() > 1 {
        union(&all_infill)?
    } else {
        all_infill
    };

    Ok((all_paths, infill_area))
}

#[allow(clippy::too_many_arguments)]
fn emit_paired_contour(
    poly: &Polygon,
    medial: &MedialAxis,
    params: &BeadingParams,
    pair_idx: usize,
    d_nom: f64,
    min_r: f64,
    role: ExtrusionRole,
    layer_height_mm: f64,
    speed_mm_s: f64,
    out: &mut Vec<ExtrusionPath>,
) {
    let cleaned = merge_tiny_polygon_edges(&poly.points, MIN_COLLAPSED_EDGE_MM);
    if cleaned.len() < 3 {
        return;
    }
    let sampled = subdivide_closed_poly(&cleaned, EDGE_SUBDIVIDE_MM);
    let m = sampled.len();
    if m < 3 {
        return;
    }

    let default_w = if pair_idx == 0 {
        params.w_min_outer
    } else {
        params.w_min_inner
    };

    let mut displaced = Vec::with_capacity(m);
    let mut widths = Vec::with_capacity(m);

    for (i, &(px, py)) in sampled.iter().enumerate() {
        let thickness = medial
            .query_thickness_at(px, py, d_nom, min_r)
            .map(|(t, _)| t)
            .unwrap_or(params.t_infill);
        let plan = compute_beading(thickness, params);
        let w_m = plan.pair_widths.get(pair_idx).copied().unwrap_or(default_w);

        let mut d_target = 0.0;
        for u in 0..pair_idx {
            let wu = plan.pair_widths.get(u).copied().unwrap_or(if u == 0 {
                params.w_min_outer
            } else {
                params.w_min_inner
            });
            d_target += wu - params.overlap;
        }
        d_target += 0.5 * w_m;

        let mut delta_d = d_target - d_nom;
        let max_inward = (0.45 * (0.5 * thickness - d_nom)).max(0.0);
        let max_outward = 0.45 * d_nom;
        if delta_d > 0.0 {
            delta_d = delta_d.min(max_inward);
        } else {
            delta_d = delta_d.max(-max_outward);
        }

        let (nx, ny) = inward_miter_normal(&sampled, i);
        let shift_x = nx * delta_d;
        let shift_y = ny * delta_d;
        displaced.push((px + shift_x, py + shift_y));
        widths.push(w_m);
    }

    let (simp_pts, simp_w) = simplify_varwidth_closed(&displaced, &widths);
    if simp_pts.len() >= 3 {
        emit_polyline_with_widths(
            &simp_pts,
            &simp_w,
            true,
            role,
            layer_height_mm,
            speed_mm_s,
            out,
        );
    }
}

fn refine_variable_width_polyline(
    pts_mm: &[(f64, f64)],
    widths_mm: &[f64],
    closed: bool,
) -> (Vec<(f64, f64)>, Vec<f64>) {
    let n = pts_mm.len();
    if n < 2 || n != widths_mm.len() {
        return (pts_mm.to_vec(), widths_mm.to_vec());
    }
    let segs = if closed { n } else { n - 1 };
    let mut out_pts = Vec::with_capacity(n);
    let mut out_w = Vec::with_capacity(n);

    for i in 0..segs {
        let j = (i + 1) % n;
        let (Some(&a), Some(&b), Some(&wa), Some(&wb)) = (
            pts_mm.get(i),
            pts_mm.get(j),
            widths_mm.get(i),
            widths_mm.get(j),
        ) else {
            continue;
        };
        out_pts.push(a);
        out_w.push(wa);

        let dw = (wb - wa).abs();
        let len = (b.0 - a.0).hypot(b.1 - a.1);
        if dw > MAX_SEG_WIDTH_DELTA_MM && len > 0.15 {
            let max_by_len = (len / 0.12).floor().max(1.0);
            let steps = (dw / MAX_SEG_WIDTH_DELTA_MM).ceil().min(max_by_len) as usize;
            if steps >= 2 {
                for k in 1..steps {
                    let t = (k as f64) / (steps as f64);
                    out_pts.push((a.0 + t * (b.0 - a.0), a.1 + t * (b.1 - a.1)));
                    out_w.push(wa + t * (wb - wa));
                }
            }
        }
    }

    if !closed && let (Some(&pn), Some(&wn)) = (pts_mm.last(), widths_mm.last()) {
        out_pts.push(pn);
        out_w.push(wn);
    }

    (out_pts, out_w)
}

fn emit_polyline_with_widths(
    pts_mm: &[(f64, f64)],
    widths_mm: &[f64],
    closed: bool,
    role: ExtrusionRole,
    height_mm: f64,
    speed_mm_s: f64,
    out: &mut Vec<ExtrusionPath>,
) {
    if pts_mm.len() < 2 || pts_mm.len() != widths_mm.len() {
        return;
    }
    let (pts_refined, widths_refined) = refine_variable_width_polyline(pts_mm, widths_mm, closed);
    let points: Vec<Point2> = pts_refined
        .iter()
        .filter_map(|&(x, y)| Point2::from_mm(x, y).ok())
        .collect();
    if points.len() != pts_refined.len() {
        return;
    }

    let min_w = widths_refined.iter().copied().fold(f64::INFINITY, f64::min);
    let max_w = widths_refined.iter().copied().fold(0.0, f64::max);
    let mean_w = weighted_mean_width(&pts_refined, &widths_refined, closed);

    let point_widths_mm = if max_w - min_w <= UNIFORM_WIDTH_TOL_MM {
        Vec::new()
    } else {
        widths_refined
    };

    out.push(ExtrusionPath {
        role,
        width_mm: mean_w,
        point_widths_mm,
        height_mm,
        speed_mm_s,
        flow_ratio: 1.0,
        points,
        closed,
        object_id: None,
        fan_percent: None,
    });
}

fn weighted_mean_width(pts: &[(f64, f64)], widths: &[f64], closed: bool) -> f64 {
    let n = pts.len();
    if n == 0 || widths.is_empty() {
        return 0.4;
    }
    let segs = if closed { n } else { n.saturating_sub(1) };
    let mut len_sum = 0.0;
    let mut area_w_sum = 0.0;
    for i in 0..segs {
        let j = (i + 1) % n;
        let (Some(&a), Some(&b), Some(&wa), Some(&wb)) =
            (pts.get(i), pts.get(j), widths.get(i), widths.get(j))
        else {
            continue;
        };
        let len = (b.0 - a.0).hypot(b.1 - a.1);
        if len > 1e-9 {
            len_sum += len;
            area_w_sum += len * 0.5 * (wa + wb);
        }
    }
    if len_sum > 1e-9 {
        area_w_sum / len_sum
    } else {
        widths.iter().sum::<f64>() / (widths.len() as f64)
    }
}

fn merge_tiny_polygon_edges(pts: &[Point2], min_len_mm: f64) -> Vec<Point2> {
    let mut kept: Vec<Point2> = Vec::with_capacity(pts.len());
    for &p in pts {
        if let Some(&prev) = kept.last()
            && prev.distance_mm(p) < min_len_mm
        {
            continue;
        }
        kept.push(p);
    }
    while kept.len() > 3
        && let (Some(&first), Some(&last)) = (kept.first(), kept.last())
        && first.distance_mm(last) < min_len_mm
    {
        kept.pop();
    }
    kept
}

fn subdivide_open_poly(pts: &[Point2], max_seg_mm: f64) -> Vec<(f64, f64)> {
    let n = pts.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return pts.first().map(|p| vec![p.to_mm()]).unwrap_or_default();
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n - 1 {
        let (Some(&a), Some(&b)) = (pts.get(i), pts.get(i + 1)) else {
            continue;
        };
        let (ax, ay) = a.to_mm();
        let (bx, by) = b.to_mm();
        out.push((ax, ay));
        let len = (bx - ax).hypot(by - ay);
        if len > max_seg_mm {
            let steps = (len / max_seg_mm).ceil() as usize;
            for k in 1..steps {
                let u = (k as f64) / (steps as f64);
                out.push((ax + u * (bx - ax), ay + u * (by - ay)));
            }
        }
    }
    if let Some(&last) = pts.last() {
        out.push(last.to_mm());
    }
    out
}

fn subdivide_closed_poly(pts: &[Point2], max_seg_mm: f64) -> Vec<(f64, f64)> {
    let n = pts.len();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let (Some(&a), Some(&b)) = (pts.get(i), pts.get((i + 1) % n)) else {
            continue;
        };
        let (ax, ay) = a.to_mm();
        let (bx, by) = b.to_mm();
        out.push((ax, ay));
        let len = (bx - ax).hypot(by - ay);
        if len > max_seg_mm {
            let steps = (len / max_seg_mm).ceil() as usize;
            for k in 1..steps {
                let u = (k as f64) / (steps as f64);
                out.push((ax + u * (bx - ax), ay + u * (by - ay)));
            }
        }
    }
    out
}

fn inward_miter_normal(pts: &[(f64, f64)], idx: usize) -> (f64, f64) {
    let n = pts.len();
    let (Some(&prev), Some(&curr), Some(&next)) = (
        pts.get((idx + n - 1) % n),
        pts.get(idx),
        pts.get((idx + 1) % n),
    ) else {
        return (0.0, 0.0);
    };

    let unit_left_normal = |a: (f64, f64), b: (f64, f64)| -> Option<(f64, f64)> {
        let dx = b.0 - a.0;
        let dy = b.1 - a.1;
        let len = dx.hypot(dy);
        (len > 1e-9).then_some((-dy / len, dx / len))
    };

    match (unit_left_normal(prev, curr), unit_left_normal(curr, next)) {
        (Some(n0), Some(n1)) => {
            let bx = n0.0 + n1.0;
            let by = n0.1 + n1.1;
            let blen = bx.hypot(by);
            if blen <= 1e-6 {
                return n0;
            }
            let ux = bx / blen;
            let uy = by / blen;
            let cos_half = (ux * n0.0 + uy * n0.1).abs();
            let scale = if cos_half > 0.25 {
                (1.0 / cos_half).min(MITER_SHIFT_LIMIT)
            } else {
                1.0
            };
            (ux * scale, uy * scale)
        }
        (Some(n0), None) => n0,
        (None, Some(n1)) => n1,
        (None, None) => (0.0, 0.0),
    }
}

fn is_collinear_with_width(
    a: (f64, f64),
    wa: f64,
    p: (f64, f64),
    wp: f64,
    b: (f64, f64),
    wb: f64,
) -> bool {
    if (wb - wa).abs() > MAX_SEG_WIDTH_DELTA_MM {
        return false;
    }
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let len2 = dx * dx + dy * dy;
    if len2 <= 1e-12 {
        return (p.0 - a.0).hypot(p.1 - a.1) <= SIMPLIFY_XY_EPS_MM
            && (wp - wa).abs() <= SIMPLIFY_W_EPS_MM;
    }
    let t = ((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2;
    if !(0.0..=1.0).contains(&t) {
        return false;
    }
    let proj_x = a.0 + t * dx;
    let proj_y = a.1 + t * dy;
    let xy_err = (p.0 - proj_x).hypot(p.1 - proj_y);
    let w_interp = wa + t * (wb - wa);
    xy_err <= SIMPLIFY_XY_EPS_MM && (wp - w_interp).abs() <= SIMPLIFY_W_EPS_MM
}

fn simplify_varwidth_closed(pts: &[(f64, f64)], widths: &[f64]) -> (Vec<(f64, f64)>, Vec<f64>) {
    if pts.len() <= 3 || pts.len() != widths.len() {
        return (pts.to_vec(), widths.to_vec());
    }
    let n = pts.len();
    let mut out_pts = Vec::with_capacity(n);
    let mut out_w = Vec::with_capacity(n);
    if let (Some(&p0), Some(&w0)) = (pts.first(), widths.first()) {
        out_pts.push(p0);
        out_w.push(w0);
    }
    for i in 1..n {
        let Some(&curr_p) = pts.get(i) else {
            continue;
        };
        let Some(&curr_w) = widths.get(i) else {
            continue;
        };
        let Some(&prev_p) = out_pts.last() else {
            continue;
        };
        let Some(&prev_w) = out_w.last() else {
            continue;
        };
        let (next_p, next_w) = if i + 1 < n {
            match (pts.get(i + 1), widths.get(i + 1)) {
                (Some(&np), Some(&nw)) => (np, nw),
                _ => continue,
            }
        } else {
            match (out_pts.first(), out_w.first()) {
                (Some(&np), Some(&nw)) => (np, nw),
                _ => continue,
            }
        };
        let remaining_after = n - 1 - i;
        if out_pts.len() + remaining_after >= 3
            && is_collinear_with_width(prev_p, prev_w, curr_p, curr_w, next_p, next_w)
        {
            continue;
        }
        out_pts.push(curr_p);
        out_w.push(curr_w);
    }
    if out_pts.len() > 3
        && let (Some(&last_p), Some(&first_p), Some(&second_p)) =
            (out_pts.last(), out_pts.first(), out_pts.get(1))
        && let (Some(&last_w), Some(&first_w), Some(&second_w)) =
            (out_w.last(), out_w.first(), out_w.get(1))
        && is_collinear_with_width(last_p, last_w, first_p, first_w, second_p, second_w)
    {
        out_pts.remove(0);
        out_w.remove(0);
    }
    (out_pts, out_w)
}

fn simplify_varwidth_open(pts: &[(f64, f64)], widths: &[f64]) -> (Vec<(f64, f64)>, Vec<f64>) {
    if pts.len() <= 2 || pts.len() != widths.len() {
        return (pts.to_vec(), widths.to_vec());
    }
    let mut out_pts = Vec::with_capacity(pts.len());
    let mut out_w = Vec::with_capacity(widths.len());
    if let (Some(&p0), Some(&w0)) = (pts.first(), widths.first()) {
        out_pts.push(p0);
        out_w.push(w0);
    }
    for i in 1..pts.len() - 1 {
        let (Some(&prev_p), Some(&curr_p), Some(&next_p)) =
            (out_pts.last(), pts.get(i), pts.get(i + 1))
        else {
            continue;
        };
        let (Some(&prev_w), Some(&curr_w), Some(&next_w)) =
            (out_w.last(), widths.get(i), widths.get(i + 1))
        else {
            continue;
        };
        if is_collinear_with_width(prev_p, prev_w, curr_p, curr_w, next_p, next_w) {
            continue;
        }
        out_pts.push(curr_p);
        out_w.push(curr_w);
    }
    if let (Some(&pn), Some(&wn)) = (pts.last(), widths.last())
        && out_pts
            .last()
            .is_none_or(|&prev| (prev.0 - pn.0).hypot(prev.1 - pn.1) > 1e-4)
    {
        out_pts.push(pn);
        out_w.push(wn);
    }
    (out_pts, out_w)
}

fn extract_threshold_runs(edge: &MedialEdge, t_lo: f64) -> Vec<Vec<(f64, f64, f64)>> {
    let mut runs: Vec<Vec<(f64, f64, f64)>> = Vec::new();
    let mut cur: Vec<(f64, f64, f64)> = Vec::new();

    for pair in edge.samples.windows(2) {
        let (Some(sa), Some(sb)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        let (ax, ay) = sa.point.to_mm();
        let (bx, by) = sb.point.to_mm();
        let ta = 2.0 * sa.radius_mm;
        let tb = 2.0 * sb.radius_mm;

        let a_in = ta >= t_lo;
        let b_in = tb >= t_lo;
        if !a_in && !b_in {
            if cur.len() >= 2 {
                runs.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
            continue;
        }

        if a_in && b_in {
            if cur.is_empty() {
                cur.push((ax, ay, ta));
            }
            cur.push((bx, by, tb));
        } else {
            let denom = tb - ta;
            let u = if denom.abs() > 1e-12 {
                ((t_lo - ta) / denom).clamp(0.0, 1.0)
            } else {
                0.5
            };
            let cx = ax + u * (bx - ax);
            let cy = ay + u * (by - ay);
            if a_in {
                if cur.is_empty() {
                    cur.push((ax, ay, ta));
                }
                cur.push((cx, cy, t_lo));
                if cur.len() >= 2 {
                    runs.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
            } else {
                if cur.len() >= 2 {
                    runs.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
                cur.push((cx, cy, t_lo));
                cur.push((bx, by, tb));
            }
        }
    }
    if cur.len() >= 2 {
        runs.push(cur);
    }
    runs
}

fn project_thickness_on_edge(px: f64, py: f64, edge: &MedialEdge) -> Option<f64> {
    let mut best_d2 = f64::INFINITY;
    let mut best_t = None;
    for pair in edge.samples.windows(2) {
        let (Some(sa), Some(sb)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        let (ax, ay) = sa.point.to_mm();
        let (bx, by) = sb.point.to_mm();
        let dx = bx - ax;
        let dy = by - ay;
        let len2 = dx * dx + dy * dy;
        let u = if len2 > 1e-12 {
            (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let qx = ax + u * dx;
        let qy = ay + u * dy;
        let d2 = (px - qx).powi(2) + (py - qy).powi(2);
        if d2 < best_d2 {
            best_d2 = d2;
            let r = sa.radius_mm + u * (sb.radius_mm - sa.radius_mm);
            best_t = Some(2.0 * r);
        }
    }
    best_t
}

fn stitch_open_chains(mut pieces: Vec<WidthPiece>, tol_mm: f64) -> Vec<StitchedChain> {
    let mut out = Vec::new();
    while let Some((mut cur_pts, mut cur_w)) = pieces.pop() {
        if cur_pts.len() < 2 {
            continue;
        }
        while let Some(&head) = cur_pts.first() {
            let Some(&tail) = cur_pts.last() else {
                break;
            };
            let dist = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).hypot(a.1 - b.1);
            let mut best_dist = tol_mm;
            let mut matched = None;
            for (idx, (pts, _)) in pieces.iter().enumerate() {
                let (Some(&p0), Some(&p1)) = (pts.first(), pts.last()) else {
                    continue;
                };
                for (d, prepend, reverse) in [
                    (dist(tail, p0), false, false),
                    (dist(tail, p1), false, true),
                    (dist(head, p1), true, false),
                    (dist(head, p0), true, true),
                ] {
                    if d <= best_dist {
                        best_dist = d;
                        matched = Some((idx, prepend, reverse));
                    }
                }
            }
            let Some((idx, prepend, reverse)) = matched else {
                break;
            };
            let (mut add_pts, mut add_w) = pieces.swap_remove(idx);
            if reverse {
                add_pts.reverse();
                add_w.reverse();
            }
            if prepend {
                if !cur_pts.is_empty() && !add_pts.is_empty() {
                    add_pts.pop();
                    add_w.pop();
                }
                add_pts.extend(cur_pts);
                add_w.extend(cur_w);
                cur_pts = add_pts;
                cur_w = add_w;
            } else {
                cur_pts.extend(add_pts.into_iter().skip(1));
                cur_w.extend(add_w.into_iter().skip(1));
            }
        }

        let is_closed = cur_pts.len() >= 4
            && polyline_len_mm(&cur_pts, false) >= (4.0 * tol_mm).max(0.10)
            && cur_pts
                .first()
                .zip(cur_pts.last())
                .is_some_and(|(&a, &b)| (a.0 - b.0).hypot(a.1 - b.1) <= tol_mm);
        if is_closed {
            if let Some(wl) = cur_w.pop()
                && let Some(wf) = cur_w.first_mut()
            {
                *wf = 0.5 * (*wf + wl);
            }
            cur_pts.pop();
            let (sp, sw) = simplify_varwidth_closed(&cur_pts, &cur_w);
            out.push((sp, sw, true));
        } else {
            let (sp, sw) = simplify_varwidth_open(&cur_pts, &cur_w);
            out.push((sp, sw, false));
        }
    }
    out
}

fn polyline_len_mm(pts: &[(f64, f64)], closed: bool) -> f64 {
    let n = pts.len();
    if n < 2 {
        return 0.0;
    }
    let segs = if closed { n } else { n - 1 };
    let mut total = 0.0;
    for i in 0..segs {
        if let (Some(&a), Some(&b)) = (pts.get(i), pts.get((i + 1) % n)) {
            total += (b.0 - a.0).hypot(b.1 - a.1);
        }
    }
    total
}

fn extract_range_runs(edge: &MedialEdge, min_w: f64, max_w: f64) -> Vec<WidthPiece> {
    let mut pieces = Vec::new();
    let mut pts: Vec<(f64, f64)> = Vec::new();
    let mut ws: Vec<f64> = Vec::new();

    for pair in edge.samples.windows(2) {
        let (Some(sa), Some(sb)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        let wa = 2.0 * sa.radius_mm;
        let wb = 2.0 * sb.radius_mm;
        let mid_w = 0.5 * (wa + wb);
        if mid_w >= min_w && mid_w <= max_w {
            let (ax, ay) = sa.point.to_mm();
            let (bx, by) = sb.point.to_mm();
            let w_a = wa.clamp(min_w, max_w);
            let w_b = wb.clamp(min_w, max_w);
            if pts.is_empty() {
                pts.push((ax, ay));
                ws.push(w_a);
            }
            pts.push((bx, by));
            ws.push(w_b);
        } else if pts.len() >= 2 {
            pieces.push((std::mem::take(&mut pts), std::mem::take(&mut ws)));
        } else {
            pts.clear();
            ws.clear();
        }
    }
    if pts.len() >= 2 {
        pieces.push((pts, ws));
    }
    pieces
}

fn anchor_gap_fill_endpoint(
    p_end: (f64, f64),
    p_neighbor: (f64, f64),
    w_end: f64,
    wall_coverage: &[ExPolygon],
    model_regions: &[ExPolygon],
) -> Option<(f64, f64)> {
    if wall_coverage.is_empty() || w_end <= 1e-6 {
        return None;
    }
    let dx = p_end.0 - p_neighbor.0;
    let dy = p_end.1 - p_neighbor.1;
    let len = dx.hypot(dy);
    if len <= 1e-9 {
        return None;
    }
    let ux = dx / len;
    let uy = dy / len;
    let in_any = |polys: &[ExPolygon], x: f64, y: f64| -> bool {
        Point2::from_mm(x, y).is_ok_and(|pt| polys.iter().any(|ex| ex.contains_point(pt)))
    };

    for frac in [0.85, 0.70, 0.55, 0.40, 0.25] {
        let ext = frac * w_end;
        let p_ext = (p_end.0 + ext * ux, p_end.1 + ext * uy);
        let p_mid = (p_end.0 + 0.5 * ext * ux, p_end.1 + 0.5 * ext * uy);
        let p_cap = (
            p_end.0 + (ext + 0.5 * w_end) * ux,
            p_end.1 + (ext + 0.5 * w_end) * uy,
        );
        if in_any(wall_coverage, p_ext.0, p_ext.1)
            && in_any(model_regions, p_mid.0, p_mid.1)
            && in_any(model_regions, p_cap.0, p_cap.1)
        {
            return Some(p_ext);
        }
    }
    None
}

/// Generate medial-axis variable-width gap fill paths across narrow residual channels.
pub(crate) fn extract_gap_fill_paths(
    gap_regions: &[ExPolygon],
    wall_coverage: &[ExPolygon],
    model_regions: &[ExPolygon],
    min_width_mm: f64,
    max_width_mm: f64,
    layer_height_mm: f64,
    speed_mm_s: f64,
) -> Result<Vec<ExtrusionPath>, SliceError> {
    if gap_regions.is_empty() || max_width_mm <= min_width_mm {
        return Ok(Vec::new());
    }
    let mut pieces = Vec::new();
    for region in gap_regions {
        let Ok(axis) = medial_axis(region) else {
            continue;
        };
        for edge in &axis.edges {
            pieces.extend(extract_range_runs(edge, min_width_mm, max_width_mm));
        }
    }
    if pieces.is_empty() {
        return Ok(Vec::new());
    }
    let min_piece_len = (0.5 * min_width_mm).max(0.05);
    let stitched = stitch_open_chains(pieces, CHAIN_STITCH_TOL_MM);
    let mut out = Vec::new();
    for (mut pts, mut widths, closed) in stitched {
        if pts.len() < 2 || (!closed && polyline_len_mm(&pts, false) < min_piece_len) {
            continue;
        }
        if !closed && pts.len() == widths.len() {
            if let (Some(&p0), Some(&p1), Some(&w0)) = (pts.first(), pts.get(1), widths.first())
                && let Some(ext_start) =
                    anchor_gap_fill_endpoint(p0, p1, w0, wall_coverage, model_regions)
            {
                pts.insert(0, ext_start);
                widths.insert(0, w0);
            }
            let n = pts.len();
            if n >= 2
                && let (Some(&p_last), Some(&p_prev), Some(&w_last)) =
                    (pts.get(n - 1), pts.get(n - 2), widths.last())
                && let Some(ext_end) =
                    anchor_gap_fill_endpoint(p_last, p_prev, w_last, wall_coverage, model_regions)
            {
                pts.push(ext_end);
                widths.push(w_last);
            }
        }
        emit_polyline_with_widths(
            &pts,
            &widths,
            closed,
            ExtrusionRole::GapFill,
            layer_height_mm,
            speed_mm_s,
            &mut out,
        );
    }
    Ok(out)
}
