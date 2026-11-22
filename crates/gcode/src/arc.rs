//! Clean-room greedy circumcircle arc fitter for 2D XY toolpaths (`G2`/`G3`).

use std::f64::consts::TAU;

/// A 2D XY extrusion motion primitive emitted after optional arc fitting.
#[derive(Debug, Clone, PartialEq)]
pub enum MotionCmd {
    Line {
        to: (f64, f64),
        len_mm: f64,
    },
    Arc {
        cw: bool,
        to: (f64, f64),
        center: (f64, f64),
        radius: f64,
        sweep_rad: f64,
        len_mm: f64,
    },
}

const MIN_ARC_RADIUS_MM: f64 = 0.25;
const MAX_ARC_RADIUS_MM: f64 = 5000.0;
const MIN_ARC_CHORDS: usize = 3;

#[inline]
fn wrap_signed_angle(delta: f64) -> f64 {
    let mut a = delta % TAU;
    if a > std::f64::consts::PI {
        a -= TAU;
    } else if a <= -std::f64::consts::PI {
        a += TAU;
    }
    a
}

fn circumcircle(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> Option<((f64, f64), f64)> {
    let (ax, ay) = a;
    let (bx, by) = b;
    let (cx, cy) = c;
    let d = 2.0 * (ax * (by - cy) + bx * (cy - ay) + cx * (ay - by));
    if d.abs() <= 1e-9 {
        return None;
    }
    let a2 = ax * ax + ay * ay;
    let b2 = bx * bx + by * by;
    let c2 = cx * cx + cy * cy;
    let ux = (a2 * (by - cy) + b2 * (cy - ay) + c2 * (ay - by)) / d;
    let uy = (a2 * (cx - bx) + b2 * (ax - cx) + c2 * (bx - ax)) / d;
    let r = (ax - ux).hypot(ay - uy);
    if !ux.is_finite() || !uy.is_finite() || !r.is_finite() {
        return None;
    }
    Some(((ux, uy), r))
}

fn try_fit_arc(pts: &[(f64, f64)], start: usize, end: usize, tol_mm: f64) -> Option<MotionCmd> {
    if end < start + MIN_ARC_CHORDS {
        return None;
    }
    let &p_start = pts.get(start)?;
    let &p_mid = pts.get((start + end) / 2)?;
    let &p_end = pts.get(end)?;

    let (center, r) = circumcircle(p_start, p_mid, p_end)?;
    if !(MIN_ARC_RADIUS_MM..=MAX_ARC_RADIUS_MM).contains(&r) {
        return None;
    }

    let min_sweep = 10.0_f64.to_radians();
    let max_sweep = 175.0_f64.to_radians();
    let min_cos_corner = 45.0_f64.to_radians().cos();

    let mut total_sweep = 0.0;
    let mut sign: Option<f64> = None;
    let mut prev_dir: Option<(f64, f64)> = None;

    for k in start..end {
        let &p0 = pts.get(k)?;
        let &p1 = pts.get(k + 1)?;
        let dx = p1.0 - p0.0;
        let dy = p1.1 - p0.1;
        let seg_len = dx.hypot(dy);
        if seg_len <= 1e-6 {
            return None;
        }
        let dir = (dx / seg_len, dy / seg_len);
        if let Some(pd) = prev_dir {
            let dot = pd.0 * dir.0 + pd.1 * dir.1;
            if dot < min_cos_corner {
                return None;
            }
        }
        prev_dir = Some(dir);

        let a0 = (p0.1 - center.1).atan2(p0.0 - center.0);
        let a1 = (p1.1 - center.1).atan2(p1.0 - center.0);
        let da = wrap_signed_angle(a1 - a0);
        if da.abs() <= 1e-6 {
            return None;
        }
        let step_sign = da.signum();
        match sign {
            Some(s) if (s - step_sign).abs() > 0.5 => return None,
            None => sign = Some(step_sign),
            _ => {}
        }
        total_sweep += da.abs();
        if total_sweep > max_sweep {
            return None;
        }

        // Chord midpoint sagitta/radial error check against the fitted circle.
        let mx = 0.5 * (p0.0 + p1.0);
        let my = 0.5 * (p0.1 + p1.1);
        let d_mid = (mx - center.0).hypot(my - center.1);
        if (d_mid - r).abs() > tol_mm {
            return None;
        }
    }

    if total_sweep < min_sweep {
        return None;
    }

    for k in start..=end {
        let &pk = pts.get(k)?;
        let rk = (pk.0 - center.0).hypot(pk.1 - center.1);
        if (rk - r).abs() > tol_mm {
            return None;
        }
    }

    let cw = sign? < 0.0;
    let len_mm = r * total_sweep;
    if !len_mm.is_finite() || len_mm <= 1e-6 {
        return None;
    }

    Some(MotionCmd::Arc {
        cw,
        to: p_end,
        center,
        radius: r,
        sweep_rad: total_sweep,
        len_mm,
    })
}

/// Fit circular `G2`/`G3` arcs greedily across a dense XY polyline within `tol_mm`.
///
/// The first coordinate `points[0]` is treated as the starting position; returns motion
/// primitives ending at every retained vertex up to `points.last()`.
pub fn fit_polyline_arcs(points: &[(f64, f64)], tol_mm: f64) -> Vec<MotionCmd> {
    if points.len() < 2 {
        return Vec::new();
    }
    let safe_tol = tol_mm.clamp(0.001, 1.0);
    let n = points.len();
    let mut cmds = Vec::with_capacity(n);
    let mut i = 0usize;

    while i + 1 < n {
        let mut best_fit: Option<(usize, MotionCmd)> = None;
        if i + MIN_ARC_CHORDS < n {
            for end in (i + MIN_ARC_CHORDS)..n {
                if let Some(arc) = try_fit_arc(points, i, end, safe_tol) {
                    best_fit = Some((end, arc));
                } else {
                    break;
                }
            }
        }

        if let Some((end_idx, arc)) = best_fit {
            cmds.push(arc);
            i = end_idx;
        } else {
            let (Some(&a), Some(&b)) = (points.get(i), points.get(i + 1)) else {
                break;
            };
            let len_mm = (b.0 - a.0).hypot(b.1 - a.1);
            if len_mm > 1e-6 {
                cmds.push(MotionCmd::Line { to: b, len_mm });
            }
            i += 1;
        }
    }

    cmds
}

/// Sample intermediate points along an arc (excluding `from`, including endpoint).
pub fn sample_arc_points(
    from: (f64, f64),
    center: (f64, f64),
    radius: f64,
    cw: bool,
    sweep_rad: f64,
    max_step_rad: f64,
) -> Vec<(f64, f64)> {
    let steps = (sweep_rad / max_step_rad.max(1e-3)).ceil().clamp(2.0, 64.0) as usize;
    let a0 = (from.1 - center.1).atan2(from.0 - center.0);
    let dir = if cw { -1.0 } else { 1.0 };
    let mut out = Vec::with_capacity(steps);
    for s in 1..=steps {
        let t = (s as f64) / (steps as f64);
        let ang = a0 + dir * t * sweep_rad;
        out.push((center.0 + radius * ang.cos(), center.1 + radius * ang.sin()));
    }
    out
}
