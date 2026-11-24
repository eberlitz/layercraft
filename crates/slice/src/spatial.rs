//! Spatial slab index for fast point-in-polygon and segment-vs-mask overhang clipping.

use layercraft_geom::{Aabb2, ExPolygon, Point2, Polygon};

const NUM_Y_SLABS: usize = 64;

#[derive(Debug, Clone, Copy)]
struct IndexedEdge {
    ax: i64,
    ay: i64,
    bx: i64,
    by: i64,
    min_x: i64,
    max_x: i64,
    min_y: i64,
    max_y: i64,
}

#[derive(Debug, Clone)]
struct IndexedRing {
    aabb: Aabb2,
    edges: Vec<IndexedEdge>,
    y_min: i64,
    y_span: f64,
    y_slabs: Vec<Vec<usize>>,
}

impl IndexedRing {
    fn from_polygon(poly: &Polygon) -> Option<Self> {
        let aabb = poly.aabb()?;
        let n = poly.points.len();
        if n < 3 {
            return None;
        }
        let y_min = aabb.min.y;
        let y_span = (aabb.max.y - aabb.min.y).max(1) as f64;
        let mut edges = Vec::with_capacity(n);
        let mut y_slabs: Vec<Vec<usize>> = (0..NUM_Y_SLABS).map(|_| Vec::new()).collect();

        for i in 0..n {
            let (Some(&a), Some(&b)) = (poly.points.get(i), poly.points.get((i + 1) % n)) else {
                continue;
            };
            if a == b {
                continue;
            }
            let min_x = a.x.min(b.x);
            let max_x = a.x.max(b.x);
            let min_y = a.y.min(b.y);
            let max_y = a.y.max(b.y);
            let edge_idx = edges.len();
            edges.push(IndexedEdge {
                ax: a.x,
                ay: a.y,
                bx: b.x,
                by: b.y,
                min_x,
                max_x,
                min_y,
                max_y,
            });
            let s0 = slab_index(min_y, y_min, y_span);
            let s1 = slab_index(max_y, y_min, y_span);
            for slab in y_slabs.get_mut(s0..=s1).into_iter().flatten() {
                slab.push(edge_idx);
            }
        }
        if edges.is_empty() {
            return None;
        }
        Some(Self {
            aabb,
            edges,
            y_min,
            y_span,
            y_slabs,
        })
    }

    fn contains_point(&self, p: Point2) -> bool {
        if p.x < self.aabb.min.x - 2
            || p.x > self.aabb.max.x + 2
            || p.y < self.aabb.min.y - 2
            || p.y > self.aabb.max.y + 2
        {
            return false;
        }
        let slab = slab_index(p.y, self.y_min, self.y_span);
        let Some(cands) = self.y_slabs.get(slab) else {
            return false;
        };

        let mut wn: i32 = 0;
        for &edge_idx in cands {
            let Some(&e) = self.edges.get(edge_idx) else {
                continue;
            };
            if e.max_y < p.y - 2 || e.min_y > p.y + 2 {
                continue;
            }
            if p.x >= e.min_x - 2 && p.x <= e.max_x + 2 && p.y >= e.min_y - 2 && p.y <= e.max_y + 2
            {
                let dx = (e.bx - e.ax) as i128;
                let dy = (e.by - e.ay) as i128;
                let px = (p.x - e.ax) as i128;
                let py = (p.y - e.ay) as i128;
                let cross = dx * py - dy * px;
                let manhattan_len = dx.abs() + dy.abs();
                if cross.abs() <= manhattan_len * 2 {
                    return true;
                }
            }
            if e.max_x < p.x {
                continue;
            }
            if e.ay <= p.y {
                if e.by > p.y {
                    let cross = (e.bx - e.ax) as i128 * (p.y - e.ay) as i128
                        - (e.by - e.ay) as i128 * (p.x - e.ax) as i128;
                    if cross > 0 {
                        wn += 1;
                    }
                }
            } else if e.by <= p.y {
                let cross = (e.bx - e.ax) as i128 * (p.y - e.ay) as i128
                    - (e.by - e.ay) as i128 * (p.x - e.ax) as i128;
                if cross < 0 {
                    wn -= 1;
                }
            }
        }
        wn != 0
    }

    fn collect_edge_intersections(&self, a: Point2, b: Point2, ts: &mut Vec<f64>) {
        let s_min_x = a.x.min(b.x) - 2;
        let s_max_x = a.x.max(b.x) + 2;
        let s_min_y = a.y.min(b.y) - 2;
        let s_max_y = a.y.max(b.y) + 2;
        if self.aabb.max.x < s_min_x
            || self.aabb.min.x > s_max_x
            || self.aabb.max.y < s_min_y
            || self.aabb.min.y > s_max_y
        {
            return;
        }
        let slab_lo = slab_index(s_min_y, self.y_min, self.y_span);
        let slab_hi = slab_index(s_max_y, self.y_min, self.y_span);
        if slab_lo == slab_hi {
            if let Some(cands) = self.y_slabs.get(slab_lo) {
                for &edge_idx in cands {
                    if let Some(&e) = self.edges.get(edge_idx) {
                        intersect_seg_with_edge(a, b, s_min_x, s_max_x, s_min_y, s_max_y, e, ts);
                    }
                }
            }
            return;
        }
        let mut unique_edges = Vec::new();
        for slab in self.y_slabs.get(slab_lo..=slab_hi).into_iter().flatten() {
            unique_edges.extend_from_slice(slab);
        }
        unique_edges.sort_unstable();
        unique_edges.dedup();
        for edge_idx in unique_edges {
            if let Some(&e) = self.edges.get(edge_idx) {
                intersect_seg_with_edge(a, b, s_min_x, s_max_x, s_min_y, s_max_y, e, ts);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn intersect_seg_with_edge(
    a: Point2,
    b: Point2,
    s_min_x: i64,
    s_max_x: i64,
    s_min_y: i64,
    s_max_y: i64,
    e: IndexedEdge,
    ts: &mut Vec<f64>,
) {
    if e.max_x < s_min_x || e.min_x > s_max_x || e.max_y < s_min_y || e.min_y > s_max_y {
        return;
    }
    let rx = (b.x - a.x) as i128;
    let ry = (b.y - a.y) as i128;
    let sx = (e.bx - e.ax) as i128;
    let sy = (e.by - e.ay) as i128;
    let denom = rx * sy - ry * sx;
    let qpx = (e.ax - a.x) as i128;
    let qpy = (e.ay - a.y) as i128;

    if denom != 0 {
        let denom_f = denom as f64;
        let t = ((qpx * sy - qpy * sx) as f64) / denom_f;
        let u = ((qpx * ry - qpy * rx) as f64) / denom_f;
        if (-1e-6..=1.0 + 1e-6).contains(&t) && (-1e-6..=1.0 + 1e-6).contains(&u) {
            ts.push(t.clamp(0.0, 1.0));
        }
    } else {
        let len2 = (rx * rx + ry * ry) as f64;
        if len2 <= 0.0 {
            return;
        }
        let cross = (qpx * ry - qpy * rx).abs();
        let manhattan = rx.abs() + ry.abs();
        if cross <= manhattan * 2 {
            let tc = ((qpx * rx + qpy * ry) as f64) / len2;
            let q2x = (e.bx - a.x) as i128;
            let q2y = (e.by - a.y) as i128;
            let td = ((q2x * rx + q2y * ry) as f64) / len2;
            if (0.0..=1.0).contains(&tc) {
                ts.push(tc);
            }
            if (0.0..=1.0).contains(&td) {
                ts.push(td);
            }
        }
    }
}

#[inline]
fn slab_index(y: i64, y_min: i64, y_span: f64) -> usize {
    let norm = ((y.saturating_sub(y_min)) as f64 / y_span).clamp(0.0, 0.999_999);
    ((norm * (NUM_Y_SLABS as f64)) as usize).min(NUM_Y_SLABS - 1)
}

#[derive(Debug, Clone)]
struct IndexedPoly {
    outer: IndexedRing,
    holes: Vec<IndexedRing>,
}

impl IndexedPoly {
    fn from_expolygon(ex: &ExPolygon) -> Option<Self> {
        let outer = IndexedRing::from_polygon(&ex.outer)?;
        let holes = ex
            .holes
            .iter()
            .filter_map(IndexedRing::from_polygon)
            .collect();
        Some(Self { outer, holes })
    }

    fn contains_point(&self, p: Point2) -> bool {
        if !self.outer.contains_point(p) {
            return false;
        }
        !self.holes.iter().any(|h| h.contains_point(p))
    }

    fn collect_edge_intersections(&self, a: Point2, b: Point2, ts: &mut Vec<f64>) {
        let prev_len = ts.len();
        self.outer.collect_edge_intersections(a, b, ts);
        let outer_aabb = self.outer.aabb;
        let s_min_x = a.x.min(b.x) - 2;
        let s_max_x = a.x.max(b.x) + 2;
        let s_min_y = a.y.min(b.y) - 2;
        let s_max_y = a.y.max(b.y) + 2;
        if outer_aabb.max.x < s_min_x
            || outer_aabb.min.x > s_max_x
            || outer_aabb.max.y < s_min_y
            || outer_aabb.min.y > s_max_y
        {
            return;
        }
        let _ = prev_len;
        for hole in &self.holes {
            hole.collect_edge_intersections(a, b, ts);
        }
    }
}

/// Pre-indexed 2D polygon region accelerator supporting fast point queries and exact segment splitting.
#[derive(Debug, Clone, Default)]
pub(crate) struct MaskClipIndex {
    polys: Vec<IndexedPoly>,
}

impl MaskClipIndex {
    pub(crate) fn from_regions(regions: &[ExPolygon]) -> Self {
        let polys = regions
            .iter()
            .filter_map(IndexedPoly::from_expolygon)
            .collect();
        Self { polys }
    }

    pub(crate) fn contains_point(&self, p: Point2) -> bool {
        self.polys.iter().any(|poly| poly.contains_point(p))
    }

    pub(crate) fn segment_inside_same_region(
        &self,
        a: Point2,
        b: Point2,
        check_endpoints: bool,
    ) -> bool {
        if a == b {
            return true;
        }
        let s_min_x = a.x.min(b.x);
        let s_max_x = a.x.max(b.x);
        let s_min_y = a.y.min(b.y);
        let s_max_y = a.y.max(b.y);
        let ax = a.x as f64;
        let ay = a.y as f64;
        let dx = (b.x as f64) - ax;
        let dy = (b.y as f64) - ay;
        let (start_step, end_step) = if check_endpoints { (0, 6) } else { (1, 5) };

        for poly in &self.polys {
            let bb = poly.outer.aabb;
            if s_min_x < bb.min.x - 2
                || s_max_x > bb.max.x + 2
                || s_min_y < bb.min.y - 2
                || s_max_y > bb.max.y + 2
            {
                continue;
            }
            let mut all_in = true;
            for step in start_step..=end_step {
                let t = (step as f64) / 6.0;
                let px = (ax + t * dx).round() as i64;
                let py = (ay + t * dy).round() as i64;
                if !poly.contains_point(Point2::new(px, py)) {
                    all_in = false;
                    break;
                }
            }
            if all_in {
                return true;
            }
        }
        false
    }

    /// Split segment `a -> b` against the mask into ordered `(p0, p1, is_supported)` pieces.
    pub(crate) fn split_segment(&self, a: Point2, b: Point2) -> Vec<(Point2, Point2, bool)> {
        if a == b {
            return Vec::new();
        }
        if self.polys.is_empty() {
            return vec![(a, b, false)];
        }
        let mut ts = Vec::with_capacity(8);
        ts.push(0.0);
        ts.push(1.0);
        for poly in &self.polys {
            poly.collect_edge_intersections(a, b, &mut ts);
        }
        ts.sort_by(f64::total_cmp);
        let mut dedup_ts: Vec<f64> = Vec::with_capacity(ts.len());
        for t in ts {
            if dedup_ts.last().is_none_or(|&prev| (t - prev) > 1e-6) {
                dedup_ts.push(t);
            }
        }
        if dedup_ts.len() < 2 {
            dedup_ts.clear();
            dedup_ts.push(0.0);
            dedup_ts.push(1.0);
        }

        let ax = a.x as f64;
        let ay = a.y as f64;
        let dx = (b.x - a.x) as f64;
        let dy = (b.y - a.y) as f64;
        let interp = |t: f64| -> Point2 {
            if t <= 1e-9 {
                a
            } else if t >= 1.0 - 1e-9 {
                b
            } else {
                Point2::new((ax + t * dx).round() as i64, (ay + t * dy).round() as i64)
            }
        };

        let mut merged: Vec<(Point2, Point2, bool)> = Vec::new();
        for win in dedup_ts.windows(2) {
            let (Some(&t0), Some(&t1)) = (win.first(), win.get(1)) else {
                continue;
            };
            let p0 = interp(t0);
            let p1 = interp(t1);
            if p0 == p1 {
                continue;
            }
            let mid = Point2::new(p0.x + (p1.x - p0.x) / 2, p0.y + (p1.y - p0.y) / 2);
            let supported = self.contains_point(mid);
            if let Some(last) = merged.last_mut()
                && last.2 == supported
                && last.1 == p0
            {
                last.1 = p1;
            } else {
                merged.push((p0, p1, supported));
            }
        }
        merged
    }
}
