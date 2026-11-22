//! 2D Voronoi medial-axis extraction and local thickness queries.
//!
//! Confined to `layercraft-clip` so the underlying `boostvoronoi` kernel never
//! leaks into higher crates; all public types use `layercraft-geom` primitives
//! and millimeter scalars.

use boostvoronoi::prelude::{Builder, Cell, Line, Point, SourceCategory};
use layercraft_geom::{ExPolygon, NM_PER_MM, Point2, Polygon};

use crate::ClipError;

/// Internal Voronoi quantization step in nanometers (`1_000 nm = 1 µm`).
const VORONOI_SCALE_NM: i64 = 1_000;
const VORONOI_UNIT_MM: f64 = (VORONOI_SCALE_NM as f64) / (NM_PER_MM as f64);

/// Maximum chord deviation step (mm) when sampling parabolic Voronoi edges.
const PARABOLA_STEP_MM: f64 = 0.10;

/// Minimum clearance radius (mm) required for a Voronoi vertex anchor.
const MIN_ANCHOR_RADIUS_MM: f64 = 0.005;

/// Corner-bisector slope threshold (`|dr| / chord_len > 0.50` removes bisectors
/// of boundary corners with opening angle `> 60°` whose endpoint lies on ∂R).
const CORNER_BISECTOR_SLOPE_LIMIT: f64 = 0.50;

/// A sampled point on the medial axis with its exact boundary clearance radius.
#[derive(Debug, Clone, PartialEq)]
pub struct MedialSample {
    pub point: Point2,
    pub radius_mm: f64,
}

/// A pruned interior medial-axis edge represented as a sampled polyline.
#[derive(Debug, Clone, PartialEq)]
pub struct MedialEdge {
    pub samples: Vec<MedialSample>,
}

/// Pruned interior medial-axis skeleton and vertex thickness anchors for one `ExPolygon`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MedialAxis {
    pub edges: Vec<MedialEdge>,
    pub anchors: Vec<MedialSample>,
}

#[derive(Debug, Clone, Copy)]
struct SegMm {
    ax: f64,
    ay: f64,
    bx: f64,
    by: f64,
}

impl SegMm {
    fn start(self) -> (f64, f64) {
        (self.ax, self.ay)
    }

    fn end(self) -> (f64, f64) {
        (self.bx, self.by)
    }
}

impl MedialAxis {
    /// Query the local wall thickness (`2 * radius_mm`, in mm) and nearest tangent
    /// medial-axis partner `(mx, my)` for a point `(px, py)` lying at inward offset
    /// `inset_mm` from the boundary of the source polygon.
    pub fn query_thickness_at(
        &self,
        px: f64,
        py: f64,
        inset_mm: f64,
        min_radius_mm: f64,
    ) -> Option<(f64, (f64, f64))> {
        let mut best_score = f64::INFINITY;
        let mut best: Option<(f64, (f64, f64))> = None;

        for a in &self.anchors {
            if a.radius_mm < min_radius_mm {
                continue;
            }
            let (mx, my) = a.point.to_mm();
            let dist = (px - mx).hypot(py - my);
            let target = (a.radius_mm - inset_mm).max(0.0);
            let score = (dist - target).abs() + 1e-4 * dist;
            if score < best_score {
                best_score = score;
                best = Some((2.0 * a.radius_mm, (mx, my)));
            }
        }

        for edge in &self.edges {
            for pair in edge.samples.windows(2) {
                let (Some(sa), Some(sb)) = (pair.first(), pair.get(1)) else {
                    continue;
                };
                if sa.radius_mm < min_radius_mm && sb.radius_mm < min_radius_mm {
                    continue;
                }
                let (ax, ay) = sa.point.to_mm();
                let (bx, by) = sb.point.to_mm();
                let r_max = sa.radius_mm.max(sb.radius_mm);
                let min_box_dx = if px < ax.min(bx) {
                    ax.min(bx) - px
                } else if px > ax.max(bx) {
                    px - ax.max(bx)
                } else {
                    0.0
                };
                let min_box_dy = if py < ay.min(by) {
                    ay.min(by) - py
                } else if py > ay.max(by) {
                    py - ay.max(by)
                } else {
                    0.0
                };
                let min_box_dist = min_box_dx.hypot(min_box_dy);
                if min_box_dist - (r_max - inset_mm).max(0.0) > best_score {
                    continue;
                }

                let dx = bx - ax;
                let dy = by - ay;
                let len2 = dx * dx + dy * dy;
                let t = if len2 > 1e-12 {
                    (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
                } else {
                    0.0
                };

                for cand_t in [0.0, t, 1.0] {
                    let r = sa.radius_mm + cand_t * (sb.radius_mm - sa.radius_mm);
                    if r < min_radius_mm {
                        continue;
                    }
                    let mx = ax + cand_t * dx;
                    let my = ay + cand_t * dy;
                    let dist = (px - mx).hypot(py - my);
                    let target = (r - inset_mm).max(0.0);
                    let score = (dist - target).abs() + 1e-4 * dist;
                    if score < best_score {
                        best_score = score;
                        best = Some((2.0 * r, (mx, my)));
                    }
                }
            }
        }

        best
    }
}

/// Compute the pruned interior Voronoi medial axis and vertex anchors for `region`.
pub fn medial_axis(region: &ExPolygon) -> Result<MedialAxis, ClipError> {
    let mut simplified_outer = crate::simplify(&region.outer, 2_000);
    simplified_outer.ensure_ccw();
    let mut contours: Vec<Polygon> = Vec::with_capacity(1 + region.holes.len());
    contours.push(simplified_outer);
    for hole in &region.holes {
        let mut sh = crate::simplify(hole, 2_000);
        sh.ensure_cw();
        contours.push(sh);
    }

    let mut min_qx = i64::MAX;
    let mut min_qy = i64::MAX;
    let mut any_point = false;
    for poly in &contours {
        for p in &poly.points {
            let qx = p.x.div_euclid(VORONOI_SCALE_NM);
            let qy = p.y.div_euclid(VORONOI_SCALE_NM);
            min_qx = min_qx.min(qx);
            min_qy = min_qy.min(qy);
            any_point = true;
        }
    }
    if !any_point {
        return Ok(MedialAxis::default());
    }

    let mut segs_i32: Vec<Line<i32>> = Vec::new();
    let mut segs_mm: Vec<SegMm> = Vec::new();

    for poly in &contours {
        let mut qpts: Vec<(i32, i32)> = Vec::with_capacity(poly.points.len());
        for p in &poly.points {
            let qx64 = ((p.x as f64) / (VORONOI_SCALE_NM as f64)).round() as i64 - min_qx;
            let qy64 = ((p.y as f64) / (VORONOI_SCALE_NM as f64)).round() as i64 - min_qy;
            let qx = i32::try_from(qx64)
                .map_err(|_| ClipError::InvalidParameter("coordinates out of i32 Voronoi range"))?;
            let qy = i32::try_from(qy64)
                .map_err(|_| ClipError::InvalidParameter("coordinates out of i32 Voronoi range"))?;
            if qpts.last().copied() != Some((qx, qy)) {
                qpts.push((qx, qy));
            }
        }
        clean_quantized_contour(&mut qpts);
        if qpts.len() < 3 {
            continue;
        }
        let n = qpts.len();
        for i in 0..n {
            let (Some(&a), Some(&b)) = (qpts.get(i), qpts.get((i + 1) % n)) else {
                continue;
            };
            if a == b {
                continue;
            }
            segs_i32.push(Line::new(
                Point { x: a.0, y: a.1 },
                Point { x: b.0, y: b.1 },
            ));
            segs_mm.push(SegMm {
                ax: (a.0 as i64 + min_qx) as f64 * VORONOI_UNIT_MM,
                ay: (a.1 as i64 + min_qy) as f64 * VORONOI_UNIT_MM,
                bx: (b.0 as i64 + min_qx) as f64 * VORONOI_UNIT_MM,
                by: (b.1 as i64 + min_qy) as f64 * VORONOI_UNIT_MM,
            });
        }
    }

    if segs_i32.len() < 3 {
        return Ok(MedialAxis::default());
    }

    let diagram = Builder::<i32>::default()
        .with_segments(segs_i32.iter())
        .map_err(|_| ClipError::KernelFailure(-1))?
        .build()
        .map_err(|_| ClipError::KernelFailure(-1))?;

    let to_world = |vx: f64, vy: f64| -> (f64, f64) {
        (
            (vx + min_qx as f64) * VORONOI_UNIT_MM,
            (vy + min_qy as f64) * VORONOI_UNIT_MM,
        )
    };

    let mut anchors = Vec::new();
    for v in diagram.vertices() {
        if v.is_site_point() {
            continue;
        }
        let (wx, wy) = to_world(v.x(), v.y());
        if !wx.is_finite() || !wy.is_finite() || !point_in_expolygon_mm(wx, wy, region) {
            continue;
        }
        let Ok(inc_edge) = v.get_incident_edge() else {
            continue;
        };
        let Ok(edge) = diagram.edge(inc_edge) else {
            continue;
        };
        let Ok(cell_id) = edge.cell() else {
            continue;
        };
        let Ok(cell) = diagram.cell(cell_id) else {
            continue;
        };
        let Some(r) = clearance_radius_mm(wx, wy, cell, &segs_mm) else {
            continue;
        };
        if r >= MIN_ANCHOR_RADIUS_MM
            && let Ok(pt) = Point2::from_mm(wx, wy)
        {
            anchors.push(MedialSample {
                point: pt,
                radius_mm: r,
            });
        }
    }

    let mut edges = Vec::new();
    for edge in diagram.edges() {
        if !edge.is_primary() {
            continue;
        }
        let Ok(twin_id) = edge.twin() else {
            continue;
        };
        if edge.id().usize() >= twin_id.usize() {
            continue;
        }
        let Some(v0_id) = edge.vertex0() else {
            continue;
        };
        let Ok(Some(v1_id)) = diagram.edge_get_vertex1(edge.id()) else {
            continue;
        };
        let (Ok(v0), Ok(v1)) = (diagram.vertex(v0_id), diagram.vertex(v1_id)) else {
            continue;
        };
        let p0 = to_world(v0.x(), v0.y());
        let p1 = to_world(v1.x(), v1.y());
        if !p0.0.is_finite() || !p0.1.is_finite() || !p1.0.is_finite() || !p1.1.is_finite() {
            continue;
        }
        let chord_len = (p1.0 - p0.0).hypot(p1.1 - p0.1);
        if chord_len < 1e-5 {
            continue;
        }

        let Ok(cell_a) = edge.cell().and_then(|cid| diagram.cell(cid)) else {
            continue;
        };
        let Ok(cell_b) = diagram
            .edge(twin_id)
            .and_then(|tw| tw.cell())
            .and_then(|cid| diagram.cell(cid))
        else {
            continue;
        };

        let pts_mm = if edge.is_curved() {
            discretize_curved_pair(p0, p1, cell_a, cell_b, &segs_mm, PARABOLA_STEP_MM)
        } else {
            vec![p0, p1]
        };
        if pts_mm.len() < 2 {
            continue;
        }

        let mid_idx = pts_mm.len() / 2;
        let mid_pt = if pts_mm.len() == 2 {
            (0.5 * (p0.0 + p1.0), 0.5 * (p0.1 + p1.1))
        } else if let Some(&mp) = pts_mm.get(mid_idx) {
            mp
        } else {
            continue;
        };
        if !point_in_expolygon_mm(mid_pt.0, mid_pt.1, region) {
            continue;
        }

        let mut samples = Vec::with_capacity(pts_mm.len());
        for (x, y) in pts_mm {
            let ra = clearance_radius_mm(x, y, cell_a, &segs_mm);
            let rb = clearance_radius_mm(x, y, cell_b, &segs_mm);
            let Some(r) = (match (ra, rb) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            }) else {
                samples.clear();
                break;
            };
            let Ok(pt) = Point2::from_mm(x, y) else {
                samples.clear();
                break;
            };
            samples.push(MedialSample {
                point: pt,
                radius_mm: r,
            });
        }
        if samples.len() < 2 {
            continue;
        }

        if let (Some(first), Some(last)) = (samples.first(), samples.last()) {
            let r_min = first.radius_mm.min(last.radius_mm);
            let dr = (last.radius_mm - first.radius_mm).abs();
            if r_min < MIN_ANCHOR_RADIUS_MM && (dr / chord_len) > CORNER_BISECTOR_SLOPE_LIMIT {
                continue;
            }
        }

        edges.push(MedialEdge { samples });
    }

    Ok(MedialAxis { edges, anchors })
}

fn clean_quantized_contour(pts: &mut Vec<(i32, i32)>) {
    loop {
        if pts.len() >= 2 && pts.first() == pts.last() {
            pts.pop();
        }
        let n = pts.len();
        if n < 3 {
            break;
        }
        let mut spike_idx = None;
        for i in 0..n {
            let prev = pts.get((i + n - 1) % n);
            let next = pts.get((i + 1) % n);
            if prev.is_some() && prev == next {
                spike_idx = Some(i);
                break;
            }
        }
        if let Some(idx) = spike_idx {
            pts.remove(idx);
            continue;
        }
        break;
    }
}

fn site_point_mm(cell: &Cell, segs_mm: &[SegMm]) -> Option<(f64, f64)> {
    let seg = segs_mm.get(cell.source_index().usize())?;
    match cell.source_category() {
        SourceCategory::SegmentStart | SourceCategory::SinglePoint => Some(seg.start()),
        SourceCategory::SegmentEnd => Some(seg.end()),
        SourceCategory::Segment => None,
    }
}

fn clearance_radius_mm(x: f64, y: f64, cell: &Cell, segs_mm: &[SegMm]) -> Option<f64> {
    let seg = segs_mm.get(cell.source_index().usize())?;
    let r = match cell.source_category() {
        SourceCategory::SegmentStart | SourceCategory::SinglePoint => {
            (x - seg.ax).hypot(y - seg.ay)
        }
        SourceCategory::SegmentEnd => (x - seg.bx).hypot(y - seg.by),
        SourceCategory::Segment => point_to_segment_dist_mm(x, y, *seg),
    };
    r.is_finite().then_some(r)
}

fn point_to_segment_dist_mm(px: f64, py: f64, seg: SegMm) -> f64 {
    let dx = seg.bx - seg.ax;
    let dy = seg.by - seg.ay;
    let len2 = dx * dx + dy * dy;
    if len2 <= 1e-18 {
        return (px - seg.ax).hypot(py - seg.ay);
    }
    let t = (((px - seg.ax) * dx + (py - seg.ay) * dy) / len2).clamp(0.0, 1.0);
    let qx = seg.ax + t * dx;
    let qy = seg.ay + t * dy;
    (px - qx).hypot(py - qy)
}

fn discretize_curved_pair(
    v0: (f64, f64),
    v1: (f64, f64),
    cell_a: &Cell,
    cell_b: &Cell,
    segs_mm: &[SegMm],
    max_step_mm: f64,
) -> Vec<(f64, f64)> {
    let (focus_opt, seg_opt) = match (cell_a.source_category(), cell_b.source_category()) {
        (SourceCategory::Segment, _) => (
            site_point_mm(cell_b, segs_mm),
            segs_mm.get(cell_a.source_index().usize()).copied(),
        ),
        (_, SourceCategory::Segment) => (
            site_point_mm(cell_a, segs_mm),
            segs_mm.get(cell_b.source_index().usize()).copied(),
        ),
        _ => (None, None),
    };

    let (Some(focus), Some(seg)) = (focus_opt, seg_opt) else {
        return vec![v0, v1];
    };

    let ux = seg.bx - seg.ax;
    let uy = seg.by - seg.ay;
    let seg_len = ux.hypot(uy);
    if seg_len <= 1e-12 {
        return vec![v0, v1];
    }
    let tx = ux / seg_len;
    let ty = uy / seg_len;
    let nx = -ty;
    let ny = tx;

    let to_local = |p: (f64, f64)| -> (f64, f64) {
        let rx = p.0 - seg.ax;
        let ry = p.1 - seg.ay;
        (rx * tx + ry * ty, rx * nx + ry * ny)
    };
    let to_world = |lx: f64, ly: f64| -> (f64, f64) {
        (seg.ax + lx * tx + ly * nx, seg.ay + lx * ty + ly * ny)
    };

    let (lfx, lfy) = to_local(focus);
    if lfy.abs() <= 1e-9 {
        return vec![v0, v1];
    }
    let (lx0, _) = to_local(v0);
    let (lx1, _) = to_local(v1);
    let span_x = (lx1 - lx0).abs();
    let steps = ((span_x / max_step_mm.max(0.01)).ceil() as usize).clamp(2, 48);

    let mut out = Vec::with_capacity(steps + 1);
    out.push(v0);
    for k in 1..steps {
        let u = (k as f64) / (steps as f64);
        let lx = lx0 + u * (lx1 - lx0);
        let dx = lx - lfx;
        let ly = (dx * dx + lfy * lfy) / (2.0 * lfy);
        out.push(to_world(lx, ly));
    }
    out.push(v1);
    out
}

fn point_in_polygon_mm(x: f64, y: f64, poly: &Polygon) -> bool {
    let n = poly.points.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (Some(&pi), Some(&pj)) = (poly.points.get(i), poly.points.get(j)) else {
            j = i;
            continue;
        };
        let (xi, yi) = pi.to_mm();
        let (xj, yj) = pj.to_mm();
        if (yi > y) != (yj > y) {
            let denom = yj - yi;
            if denom.abs() > 1e-18 {
                let x_int = xi + (y - yi) * (xj - xi) / denom;
                if x < x_int {
                    inside = !inside;
                }
            }
        }
        j = i;
    }
    inside
}

fn point_in_expolygon_mm(x: f64, y: f64, ex: &ExPolygon) -> bool {
    if !point_in_polygon_mm(x, y, &ex.outer) {
        return false;
    }
    for hole in &ex.holes {
        if point_in_polygon_mm(x, y, hole) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Polygon {
        Polygon::new(vec![
            Point2::from_mm(x0, y0).unwrap(),
            Point2::from_mm(x1, y0).unwrap(),
            Point2::from_mm(x1, y1).unwrap(),
            Point2::from_mm(x0, y1).unwrap(),
        ])
    }

    fn circle_poly(cx: f64, cy: f64, r: f64, n: usize, cw: bool) -> Polygon {
        let mut pts = Vec::with_capacity(n);
        for i in 0..n {
            let idx = if cw { (n - i) % n } else { i };
            let ang = TAU * (idx as f64) / (n as f64);
            pts.push(Point2::from_mm(cx + r * ang.cos(), cy + r * ang.sin()).unwrap());
        }
        Polygon::new(pts)
    }

    #[test]
    fn square_20mm_has_center_anchor_radius_10() {
        let sq = ExPolygon::new(rect(0.0, 0.0, 20.0, 20.0), vec![]);
        let ma = medial_axis(&sq).unwrap();
        assert!(
            ma.anchors.iter().any(|a| {
                let (ax, ay) = a.point.to_mm();
                (ax - 10.0).abs() < 0.05
                    && (ay - 10.0).abs() < 0.05
                    && (a.radius_mm - 10.0).abs() < 0.05
            }),
            "expected center anchor with r ≈ 10.0 mm, got {:?}",
            ma.anchors
        );
        let (t, _) = ma.query_thickness_at(10.0, 0.21, 0.21, 0.05).unwrap();
        assert!((t - 20.0).abs() < 0.1, "thickness = {t}");
    }

    #[test]
    fn thin_rib_0_5mm_has_clean_spine() {
        let rib = ExPolygon::new(rect(0.0, 0.0, 20.0, 0.50), vec![]);
        let ma = medial_axis(&rib).unwrap();
        assert!(!ma.edges.is_empty(), "expected non-empty spine edges");
        let spine_len: f64 = ma
            .edges
            .iter()
            .map(|e| {
                e.samples
                    .windows(2)
                    .map(|w| w[0].point.distance_mm(w[1].point))
                    .sum::<f64>()
            })
            .sum();
        assert!(
            (spine_len - 19.5).abs() < 0.2,
            "expected spine length ~19.5 mm, got {spine_len:.3}"
        );
        for e in &ma.edges {
            for s in &e.samples {
                assert!(
                    (s.radius_mm - 0.25).abs() < 0.02,
                    "unexpected radius {:.4}",
                    s.radius_mm
                );
            }
        }
    }

    #[test]
    fn hollow_annulus_has_circular_medial_ring() {
        let outer = circle_poly(0.0, 0.0, 10.0, 48, false);
        let hole = circle_poly(0.0, 0.0, 9.4, 48, true);
        let ring = ExPolygon::new(outer, vec![hole]);
        let ma = medial_axis(&ring).unwrap();
        assert!(!ma.edges.is_empty());
        let total_len: f64 = ma
            .edges
            .iter()
            .map(|e| {
                e.samples
                    .windows(2)
                    .map(|w| w[0].point.distance_mm(w[1].point))
                    .sum::<f64>()
            })
            .sum();
        let expected_circ = TAU * 9.7;
        assert!(
            (total_len - expected_circ).abs() < 1.5,
            "ring len {total_len:.2} vs expected {expected_circ:.2}"
        );
        for e in &ma.edges {
            for s in &e.samples {
                assert!(
                    (s.radius_mm - 0.30).abs() < 0.04,
                    "annular radius {:.4} != 0.30",
                    s.radius_mm
                );
            }
        }

        // Check degree of every endpoint vertex in 64-gon at (125, 125)
        let o64 = circle_poly(125.0, 125.0, 10.0, 64, false);
        let h64 = circle_poly(125.0, 125.0, 9.4, 64, true);
        let ma64 = medial_axis(&ExPolygon::new(o64, vec![h64])).unwrap();
        let mut endpoints: Vec<(f64, f64)> = Vec::new();
        for e in &ma64.edges {
            endpoints.push(e.samples.first().unwrap().point.to_mm());
            endpoints.push(e.samples.last().unwrap().point.to_mm());
        }
        let mut odd_deg = Vec::new();
        for (i, &p) in endpoints.iter().enumerate() {
            let deg = endpoints
                .iter()
                .enumerate()
                .filter(|&(j, &q)| j != i && (p.0 - q.0).hypot(p.1 - q.1) < 1e-4)
                .count();
            if deg != 1 {
                odd_deg.push((p, deg));
            }
        }
        assert!(
            odd_deg.is_empty(),
            "medial ring at (125,125) has open endpoints: {odd_deg:?} (total edges {})",
            ma64.edges.len()
        );
    }

    #[test]
    fn tapered_wedge_spine_tapers_smoothly() {
        let wedge_poly = Polygon::new(vec![
            Point2::from_mm(0.0, -2.0).unwrap(),
            Point2::from_mm(20.0, -0.10).unwrap(),
            Point2::from_mm(20.0, 0.10).unwrap(),
            Point2::from_mm(0.0, 2.0).unwrap(),
        ]);
        let wedge = ExPolygon::new(wedge_poly, vec![]);
        let ma = medial_axis(&wedge).unwrap();
        assert!(!ma.edges.is_empty());
        let mut r_min = f64::INFINITY;
        let mut r_max = 0.0f64;
        for e in &ma.edges {
            for s in &e.samples {
                r_min = r_min.min(s.radius_mm);
                r_max = r_max.max(s.radius_mm);
            }
        }
        assert!(r_min < 0.20, "expected r_min < 0.20, got {r_min:.3}");
        assert!(r_max > 1.50, "expected r_max > 1.50, got {r_max:.3}");
    }
}
