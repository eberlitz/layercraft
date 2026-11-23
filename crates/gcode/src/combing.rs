//! Intra-island travel combing to keep rapid moves inside printed regions when possible.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use layercraft_clip::{JoinType, offset, simplify};
use layercraft_geom::{ExPolygon, Point2, Polygon, mm_to_nm};

#[derive(Debug, Clone)]
struct CombingIsland {
    raw: ExPolygon,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    boundary_loops_mm: Vec<Vec<(f64, f64)>>,
    safe_loops_mm: Vec<Vec<(f64, f64)>>,
}

/// Precomputed per-layer polygon visibility graphs for perimeter-avoiding travel moves.
#[derive(Debug, Clone, Default)]
pub struct CombingLayer {
    islands: Vec<CombingIsland>,
    snap_margin_mm: f64,
}

impl CombingLayer {
    /// Build combing islands for one sliced layer.
    pub fn from_regions(regions: &[ExPolygon], outer_wall_width_mm: f64) -> Self {
        let inset_mm = (0.5 * outer_wall_width_mm).clamp(0.12, 0.35);
        let simplify_eps_mm = (0.15 * inset_mm).clamp(0.008, 0.03);
        let snap_margin_mm = (0.85 * outer_wall_width_mm).max(0.25);
        let mut islands = Vec::with_capacity(regions.len());

        for ex in regions {
            let Some(aabb) = ex.aabb() else {
                continue;
            };
            let (min_x, min_y) = aabb.min.to_mm();
            let (max_x, max_y) = aabb.max.to_mm();

            let mut boundary_loops_mm = Vec::with_capacity(1 + ex.holes.len());
            boundary_loops_mm.push(poly_to_mm_capped(&ex.outer, 192));
            for hole in &ex.holes {
                boundary_loops_mm.push(poly_to_mm_capped(hole, 128));
            }

            let mut safe_loops_mm = Vec::new();
            let single = std::slice::from_ref(ex);
            let inset_res = mm_to_nm(-inset_mm)
                .ok()
                .and_then(|delta| offset(single, delta, JoinType::Miter { limit: 2.0 }).ok());
            if let Some(shrunk) = inset_res
                && !shrunk.is_empty()
            {
                let eps_nm = mm_to_nm(simplify_eps_mm).unwrap_or(20_000);
                for s_ex in &shrunk {
                    let s_outer = simplify(&s_ex.outer, eps_nm);
                    let pts_o = poly_to_mm_capped(&s_outer, 96);
                    if pts_o.len() >= 3 {
                        safe_loops_mm.push(pts_o);
                    }
                    for h in &s_ex.holes {
                        let s_hole = simplify(h, eps_nm);
                        let pts_h = poly_to_mm_capped(&s_hole, 96);
                        if pts_h.len() >= 3 {
                            safe_loops_mm.push(pts_h);
                        }
                    }
                }
            }
            if safe_loops_mm.is_empty() {
                for b_loop in &boundary_loops_mm {
                    if b_loop.len() >= 3 {
                        safe_loops_mm.push(cap_ring(b_loop, 96));
                    }
                }
            }

            islands.push(CombingIsland {
                raw: ex.clone(),
                min_x,
                min_y,
                max_x,
                max_y,
                boundary_loops_mm,
                safe_loops_mm,
            });
        }

        Self {
            islands,
            snap_margin_mm,
        }
    }

    /// Plan an interior travel polyline from `from_mm` to `to_mm`.
    ///
    /// Returns `None` if endpoints do not belong to the same island, if a direct line is already
    /// cleanly inside the island, if no valid interior route exists, or if `max_detour_mm > 0`
    /// is exceeded. Returned points exclude `from_mm` and always end at `to_mm`.
    pub fn plan_travel(
        &self,
        from_mm: (f64, f64),
        to_mm: (f64, f64),
        max_detour_mm: f64,
    ) -> Option<Vec<(f64, f64)>> {
        let direct_len = hypot_mm(from_mm, to_mm);
        if direct_len <= 1e-4 {
            return None;
        }

        let island = self.islands.iter().find(|isl| {
            isl.contains_near(from_mm, self.snap_margin_mm)
                && isl.contains_near(to_mm, self.snap_margin_mm)
        })?;

        // Fast check: if the straight move stays inside the island, no detour is needed.
        if island.segment_stays_inside(from_mm, to_mm) {
            return None;
        }

        let a_anchor = if island.contains_strict(from_mm) {
            from_mm
        } else {
            island.nearest_safe_point(from_mm).unwrap_or(from_mm)
        };
        let b_anchor = if island.contains_strict(to_mm) {
            to_mm
        } else {
            island.nearest_safe_point(to_mm).unwrap_or(to_mm)
        };

        let mut nodes = Vec::new();
        nodes.push(a_anchor); // node 0
        nodes.push(b_anchor); // node 1
        for lp in &island.safe_loops_mm {
            nodes.extend_from_slice(lp);
        }

        let route = dijkstra_interior_path(island, &nodes)?;
        let mut out = Vec::with_capacity(route.len() + 1);
        for pt in route {
            if out.last().is_none_or(|&prev| hypot_mm(prev, pt) > 1e-4)
                && hypot_mm(from_mm, pt) > 1e-4
            {
                out.push(pt);
            }
        }
        if out.last().is_none_or(|&last| hypot_mm(last, to_mm) > 1e-4) {
            out.push(to_mm);
        }
        if out.len() <= 1 {
            return None;
        }

        let mut combed_len = 0.0;
        let mut prev = from_mm;
        for &pt in &out {
            combed_len += hypot_mm(prev, pt);
            prev = pt;
        }
        if max_detour_mm > 0.0 && combed_len - direct_len > max_detour_mm {
            return None;
        }

        Some(out)
    }
}

impl CombingIsland {
    fn contains_strict(&self, p: (f64, f64)) -> bool {
        if p.0 < self.min_x - 1e-4
            || p.0 > self.max_x + 1e-4
            || p.1 < self.min_y - 1e-4
            || p.1 > self.max_y + 1e-4
        {
            return false;
        }
        Point2::from_mm(p.0, p.1)
            .map(|pt| self.raw.contains_point(pt))
            .unwrap_or(false)
    }

    fn contains_near(&self, p: (f64, f64), margin: f64) -> bool {
        if p.0 < self.min_x - margin
            || p.0 > self.max_x + margin
            || p.1 < self.min_y - margin
            || p.1 > self.max_y + margin
        {
            return false;
        }
        if self.contains_strict(p) {
            return true;
        }
        min_dist_to_loops(p, &self.boundary_loops_mm) <= margin
    }

    fn nearest_safe_point(&self, p: (f64, f64)) -> Option<(f64, f64)> {
        let mut best: Option<(f64, (f64, f64))> = None;
        for lp in &self.safe_loops_mm {
            for &v in lp {
                let d = hypot_mm(p, v);
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, v));
                }
            }
        }
        best.map(|(_, pt)| pt)
    }

    fn segment_stays_inside(&self, a: (f64, f64), b: (f64, f64)) -> bool {
        let len = hypot_mm(a, b);
        if len <= 1e-6 {
            return true;
        }
        for &t in &[0.125, 0.25, 0.375, 0.5, 0.625, 0.75, 0.875] {
            let mx = a.0 + t * (b.0 - a.0);
            let my = a.1 + t * (b.1 - a.1);
            if !self.contains_strict((mx, my)) {
                return false;
            }
        }
        for lp in &self.boundary_loops_mm {
            let n = lp.len();
            if n < 2 {
                continue;
            }
            for i in 0..n {
                let (Some(&e0), Some(&e1)) = (lp.get(i), lp.get((i + 1) % n)) else {
                    continue;
                };
                if segments_properly_intersect(a, b, e0, e1) {
                    return false;
                }
            }
        }
        true
    }
}

#[derive(Copy, Clone, Debug)]
struct HeapItem {
    dist: f64,
    idx: usize,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.idx == other.idx && self.dist.to_bits() == other.dist.to_bits()
    }
}

impl Eq for HeapItem {}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .dist
            .total_cmp(&self.dist)
            .then_with(|| self.idx.cmp(&other.idx))
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn dijkstra_interior_path(island: &CombingIsland, nodes: &[(f64, f64)]) -> Option<Vec<(f64, f64)>> {
    let n = nodes.len();
    if n < 2 {
        return None;
    }

    let mut dist = vec![f64::INFINITY; n];
    let mut prev: Vec<Option<usize>> = vec![None; n];
    let mut visited = vec![false; n];
    let mut heap = BinaryHeap::new();

    if let Some(d0) = dist.get_mut(0) {
        *d0 = 0.0;
    }
    heap.push(HeapItem { dist: 0.0, idx: 0 });

    while let Some(HeapItem {
        dist: cur_d,
        idx: u,
    }) = heap.pop()
    {
        if visited.get(u).copied().unwrap_or(true) {
            continue;
        }
        if let Some(vis) = visited.get_mut(u) {
            *vis = true;
        }
        if u == 1 {
            let mut chain = Vec::new();
            let mut trace = Some(1usize);
            let mut guard = 0usize;
            while let Some(cur) = trace {
                guard += 1;
                if guard > n + 2 {
                    return None;
                }
                if let Some(&pt) = nodes.get(cur) {
                    chain.push(pt);
                }
                trace = prev.get(cur).copied().flatten();
            }
            chain.reverse();
            return Some(chain);
        }

        let Some(&pu) = nodes.get(u) else {
            continue;
        };
        for (v, &pv) in nodes.iter().enumerate() {
            if u == v || visited.get(v).copied().unwrap_or(true) {
                continue;
            }
            let edge_len = hypot_mm(pu, pv);
            let cand = cur_d + edge_len;
            if cand >= dist.get(v).copied().unwrap_or(f64::INFINITY) {
                continue;
            }
            if !island.segment_stays_inside(pu, pv) {
                continue;
            }
            if let Some(dv) = dist.get_mut(v) {
                *dv = cand;
            }
            if let Some(pv_slot) = prev.get_mut(v) {
                *pv_slot = Some(u);
            }
            heap.push(HeapItem { dist: cand, idx: v });
        }
    }

    None
}

fn hypot_mm(a: (f64, f64), b: (f64, f64)) -> f64 {
    (b.0 - a.0).hypot(b.1 - a.1)
}

fn orient2d(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

fn segments_properly_intersect(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    if a.0.max(b.0) < c.0.min(d.0) - 1e-6
        || c.0.max(d.0) < a.0.min(b.0) - 1e-6
        || a.1.max(b.1) < c.1.min(d.1) - 1e-6
        || c.1.max(d.1) < a.1.min(b.1) - 1e-6
    {
        return false;
    }
    let o1 = orient2d(a, b, c);
    let o2 = orient2d(a, b, d);
    let o3 = orient2d(c, d, a);
    let o4 = orient2d(c, d, b);
    let eps = 1e-7;
    ((o1 > eps && o2 < -eps) || (o1 < -eps && o2 > eps))
        && ((o3 > eps && o4 < -eps) || (o3 < -eps && o4 > eps))
}

fn point_to_segment_dist(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let len2 = dx * dx + dy * dy;
    if len2 <= 1e-12 {
        return hypot_mm(p, a);
    }
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0);
    hypot_mm(p, (a.0 + t * dx, a.1 + t * dy))
}

fn min_dist_to_loops(p: (f64, f64), loops: &[Vec<(f64, f64)>]) -> f64 {
    let mut best = f64::INFINITY;
    for lp in loops {
        let n = lp.len();
        if n == 0 {
            continue;
        }
        for i in 0..n {
            if let (Some(&a), Some(&b)) = (lp.get(i), lp.get((i + 1) % n)) {
                best = best.min(point_to_segment_dist(p, a, b));
            }
        }
    }
    best
}

fn poly_to_mm_capped(poly: &Polygon, max_pts: usize) -> Vec<(f64, f64)> {
    let raw: Vec<(f64, f64)> = poly.points.iter().map(|p| p.to_mm()).collect();
    cap_ring(&raw, max_pts)
}

fn cap_ring(pts: &[(f64, f64)], max_pts: usize) -> Vec<(f64, f64)> {
    if pts.len() <= max_pts || max_pts < 3 {
        return pts.to_vec();
    }
    let step = (pts.len() as f64) / (max_pts as f64);
    let mut out = Vec::with_capacity(max_pts);
    for i in 0..max_pts {
        let idx = ((i as f64) * step).floor() as usize;
        if let Some(&pt) = pts.get(idx.min(pts.len() - 1)) {
            out.push(pt);
        }
    }
    out
}
