//! Clean-room deterministic triangle mesh repair.
//!
//! Pipeline (all steps deterministic: hash maps are only used for lookups, never iterated
//! to make ordering decisions):
//! 1. 27-neighbour spatial-hash vertex welding.
//! 2. Degenerate and duplicate triangle removal.
//! 3. Open-boundary crack snapping and boundary T-junction splitting.
//! 4. Manifold-edge BFS winding consistency.
//! 5. Boundary-loop hole filling (Newell-plane ear clipping, centroid-fan fallback).
//! 6. Shell outward-volume enforcement that preserves enclosed void cavities using
//!    ray-cast containment parity.

use std::collections::{HashMap, HashSet, VecDeque};

use layercraft_geom::{GeomError, Point3, Triangle, TriangleMesh};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{MeshError, MeshInspection, inspect_mesh};

/// Tunable parameters for [`repair_mesh`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MeshRepairConfig {
    /// Vertices closer than this distance (mm) are welded into one vertex.
    pub weld_tolerance_mm: f64,
    /// Open boundary vertices (and boundary T-junctions) within this distance (mm) are snapped
    /// together to close cracks.
    pub crack_snap_mm: f64,
    /// Remove duplicate triangles (same vertex set, either winding).
    pub remove_duplicates: bool,
    /// Split boundary edges at T-junction vertices lying on them.
    pub split_t_junctions: bool,
    /// Make triangle winding consistent per shell and orient shells outward.
    pub fix_winding: bool,
    /// Fill open boundary loops with new triangles.
    pub fill_holes: bool,
    /// Holes with more boundary edges than this are left open.
    pub max_hole_edges: usize,
}

impl Default for MeshRepairConfig {
    fn default() -> Self {
        Self {
            weld_tolerance_mm: 0.005,
            crack_snap_mm: 0.05,
            remove_duplicates: true,
            split_t_junctions: true,
            fix_winding: true,
            fill_holes: true,
            max_hole_edges: 256,
        }
    }
}

/// Statistics describing what [`repair_mesh`] changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MeshRepairReport {
    pub input_triangles: usize,
    pub output_triangles: usize,
    /// Distinct input vertex positions merged into another vertex by welding.
    pub welded_vertices: usize,
    /// Zero-area, collapsed, or non-finite triangles removed.
    pub removed_degenerate: usize,
    /// Duplicate triangles removed.
    pub removed_duplicate: usize,
    /// Vertices inserted into boundary edges to resolve T-junctions.
    pub t_junctions_split: usize,
    /// Open-boundary vertex pairs snapped together.
    pub cracks_snapped: usize,
    /// Input triangles whose winding was reversed.
    pub flipped_triangles: usize,
    /// Connected shells (via manifold edges) in the output.
    pub shells: usize,
    /// Inside-out top-level shells that were turned outward.
    pub inverted_shells_fixed: usize,
    /// Enclosed inward-facing shells kept as internal voids.
    pub internal_cavities_preserved: usize,
    pub holes_filled: usize,
    pub hole_triangles_added: usize,
    /// Boundary loops left open (too large, or not a closed loop).
    pub holes_unfilled: usize,
    /// `true` when the output differs from the input.
    pub changed: bool,
    pub before: MeshInspection,
    pub after: MeshInspection,
}

/// A repaired mesh together with its repair report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RepairedMesh {
    pub mesh: TriangleMesh,
    pub report: MeshRepairReport,
}

// ---------------------------------------------------------------------------------------------
// Small vector helpers.
// ---------------------------------------------------------------------------------------------

type V3 = [f64; 3];

#[inline]
fn v3(p: Point3) -> V3 {
    [p.x, p.y, p.z]
}

#[inline]
fn p3(v: V3) -> Point3 {
    let [x, y, z] = v;
    Point3::new(x, y, z)
}

#[inline]
fn sub(a: V3, b: V3) -> V3 {
    let ([ax, ay, az], [bx, by, bz]) = (a, b);
    [ax - bx, ay - by, az - bz]
}

#[inline]
fn add(a: V3, b: V3) -> V3 {
    let ([ax, ay, az], [bx, by, bz]) = (a, b);
    [ax + bx, ay + by, az + bz]
}

#[inline]
fn scale(a: V3, s: f64) -> V3 {
    let [ax, ay, az] = a;
    [ax * s, ay * s, az * s]
}

#[inline]
fn dot(a: V3, b: V3) -> f64 {
    let ([ax, ay, az], [bx, by, bz]) = (a, b);
    ax * bx + ay * by + az * bz
}

#[inline]
fn cross(a: V3, b: V3) -> V3 {
    let ([ax, ay, az], [bx, by, bz]) = (a, b);
    [ay * bz - az * by, az * bx - ax * bz, ax * by - ay * bx]
}

#[inline]
fn norm(a: V3) -> f64 {
    dot(a, a).sqrt()
}

#[inline]
fn dist2(a: V3, b: V3) -> f64 {
    let d = sub(a, b);
    dot(d, d)
}

#[inline]
fn finite3(a: V3) -> bool {
    a.iter().all(|c| c.is_finite())
}

/// Twice-area threshold matching the `area_mm2() <= 1e-12` degeneracy rule used elsewhere.
const DEGENERATE_DOUBLE_AREA: f64 = 2e-12;

// ---------------------------------------------------------------------------------------------
// Indexed mesh + edge topology.
// ---------------------------------------------------------------------------------------------

/// Indexed triangle mesh used internally by repair and inspection.
#[derive(Debug, Clone, Default)]
pub(crate) struct Indexed {
    pub(crate) verts: Vec<V3>,
    pub(crate) tris: Vec<[usize; 3]>,
}

impl Indexed {
    fn pts(&self, t: [usize; 3]) -> Option<[V3; 3]> {
        let [a, b, c] = t;
        Some([
            *self.verts.get(a)?,
            *self.verts.get(b)?,
            *self.verts.get(c)?,
        ])
    }

    fn double_area(&self, t: [usize; 3]) -> f64 {
        self.pts(t)
            .map(|[a, b, c]| norm(cross(sub(b, a), sub(c, a))))
            .unwrap_or(0.0)
    }
}

/// Usage record of one undirected edge: total count and the first two triangles using it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EdgeUse {
    pub(crate) count: u32,
    t0: usize,
    t1: usize,
}

pub(crate) type EdgeMap = HashMap<(usize, usize), EdgeUse>;

#[inline]
fn ukey(a: usize, b: usize) -> (usize, usize) {
    if a <= b { (a, b) } else { (b, a) }
}

#[inline]
fn directed_edges(t: [usize; 3]) -> [(usize, usize); 3] {
    let [a, b, c] = t;
    [(a, b), (b, c), (c, a)]
}

/// `true` when triangle `t` traverses the directed edge `a -> b`.
#[inline]
fn has_directed(t: [usize; 3], a: usize, b: usize) -> bool {
    directed_edges(t).contains(&(a, b))
}

#[inline]
fn flip_tri(t: [usize; 3]) -> [usize; 3] {
    let [a, b, c] = t;
    [a, c, b]
}

pub(crate) fn build_edges(tris: &[[usize; 3]]) -> EdgeMap {
    let mut edges: EdgeMap = HashMap::with_capacity(tris.len().saturating_mul(3) / 2 + 1);
    for (ti, &t) in tris.iter().enumerate() {
        for (a, b) in directed_edges(t) {
            if a == b {
                continue;
            }
            let e = edges.entry(ukey(a, b)).or_insert(EdgeUse {
                count: 0,
                t0: ti,
                t1: usize::MAX,
            });
            if e.count == 1 {
                e.t1 = ti;
            }
            e.count = e.count.saturating_add(1);
        }
    }
    edges
}

fn manifold_neighbor(edges: &EdgeMap, a: usize, b: usize, ti: usize) -> Option<usize> {
    let e = edges.get(&ukey(a, b))?;
    if e.count != 2 {
        return None;
    }
    if e.t0 == ti {
        Some(e.t1)
    } else if e.t1 == ti {
        Some(e.t0)
    } else {
        None
    }
}

/// Boundary half-edges `(a, b, triangle)` in deterministic triangle order.
fn boundary_half_edges(tris: &[[usize; 3]], edges: &EdgeMap) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    for (ti, &t) in tris.iter().enumerate() {
        for (a, b) in directed_edges(t) {
            if a != b && edges.get(&ukey(a, b)).is_some_and(|e| e.count == 1) {
                out.push((a, b, ti));
            }
        }
    }
    out
}

/// Connected components over manifold edges and the per-triangle flips that make each
/// component consistently wound. Each component keeps the orientation of its majority.
pub(crate) struct Components {
    pub(crate) comp: Vec<usize>,
    pub(crate) count: usize,
    flip: Vec<bool>,
}

pub(crate) fn orient_components(tris: &[[usize; 3]], edges: &EdgeMap) -> Components {
    let n = tris.len();
    let mut comp = vec![usize::MAX; n];
    let mut flip = vec![false; n];
    let mut count = 0usize;
    let mut queue = VecDeque::new();
    let mut members = Vec::new();
    for seed in 0..n {
        if comp.get(seed).copied() != Some(usize::MAX) {
            continue;
        }
        let id = count;
        count += 1;
        if let Some(c) = comp.get_mut(seed) {
            *c = id;
        }
        members.clear();
        queue.clear();
        queue.push_back(seed);
        while let Some(ti) = queue.pop_front() {
            members.push(ti);
            let Some(&orig) = tris.get(ti) else {
                continue;
            };
            let eff = if flip.get(ti).copied().unwrap_or(false) {
                flip_tri(orig)
            } else {
                orig
            };
            for (a, b) in directed_edges(eff) {
                let Some(other) = manifold_neighbor(edges, a, b, ti) else {
                    continue;
                };
                if comp.get(other).copied() != Some(usize::MAX) {
                    continue;
                }
                let Some(&other_t) = tris.get(other) else {
                    continue;
                };
                // The neighbour must traverse the shared edge as b -> a.
                let need_flip = has_directed(other_t, a, b);
                if let Some(f) = flip.get_mut(other) {
                    *f = need_flip;
                }
                if let Some(c) = comp.get_mut(other) {
                    *c = id;
                }
                queue.push_back(other);
            }
        }
        let flipped = members
            .iter()
            .filter(|&&m| flip.get(m).copied().unwrap_or(false))
            .count();
        if flipped * 2 > members.len() {
            for &m in &members {
                if let Some(f) = flip.get_mut(m) {
                    *f = !*f;
                }
            }
        }
    }
    Components { comp, count, flip }
}

// ---------------------------------------------------------------------------------------------
// Shell analysis (volume + containment).
// ---------------------------------------------------------------------------------------------

pub(crate) struct ShellInfo {
    pub(crate) tris: Vec<usize>,
    pub(crate) volume: f64,
    /// `true` when every edge is shared by exactly two consistently wound shell triangles.
    pub(crate) closed_consistent: bool,
    /// Indices of other shells that contain this shell (odd ray-cast parity).
    pub(crate) containers: Vec<usize>,
}

/// Fixed, slightly skewed ray direction to avoid axis-aligned edge/vertex hits.
const RAY_DIR: V3 = [0.577_350_269_2, 0.581_234_567_9, 0.573_456_789_1];

fn ray_hits_triangle(o: V3, d: V3, a: V3, b: V3, c: V3) -> bool {
    let e1 = sub(b, a);
    let e2 = sub(c, a);
    let p = cross(d, e2);
    let det = dot(e1, p);
    if !det.is_finite() || det.abs() < 1e-24 {
        return false;
    }
    let inv = 1.0 / det;
    let s = sub(o, a);
    let u = dot(s, p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return false;
    }
    let q = cross(s, e1);
    let v = dot(d, q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return false;
    }
    dot(e2, q) * inv > 1e-9
}

fn aabb_of(ix: &Indexed, tris: &[usize]) -> Option<(V3, V3)> {
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    let mut any = false;
    for &ti in tris {
        let Some(&t) = ix.tris.get(ti) else {
            continue;
        };
        let Some(pts) = ix.pts(t) else {
            continue;
        };
        for p in pts {
            for k in 0..3 {
                if let (Some(l), Some(h), Some(&v)) = (lo.get_mut(k), hi.get_mut(k), p.get(k)) {
                    *l = l.min(v);
                    *h = h.max(v);
                }
            }
            any = true;
        }
    }
    any.then_some((lo, hi))
}

fn aabb_inside(inner: (V3, V3), outer: (V3, V3)) -> bool {
    let ((ilo, ihi), (olo, ohi)) = (inner, outer);
    (0..3).all(|k| {
        let (Some(&a), Some(&b), Some(&c), Some(&d)) =
            (ilo.get(k), ihi.get(k), olo.get(k), ohi.get(k))
        else {
            return false;
        };
        a >= c && b <= d
    })
}

/// Analyze the shells of an indexed mesh whose triangle windings are taken as-is.
pub(crate) fn analyze_shells(ix: &Indexed, edges: &EdgeMap, comps: &Components) -> Vec<ShellInfo> {
    let mut shells: Vec<ShellInfo> = (0..comps.count)
        .map(|_| ShellInfo {
            tris: Vec::new(),
            volume: 0.0,
            closed_consistent: true,
            containers: Vec::new(),
        })
        .collect();
    for (ti, &t) in ix.tris.iter().enumerate() {
        let Some(shell) = comps.comp.get(ti).and_then(|&c| shells.get_mut(c)) else {
            continue;
        };
        shell.tris.push(ti);
        if let Some([a, b, c]) = ix.pts(t) {
            shell.volume += dot(a, cross(b, c)) / 6.0;
        }
        for (a, b) in directed_edges(t) {
            match manifold_neighbor(edges, a, b, ti).and_then(|o| ix.tris.get(o)) {
                Some(&other) if has_directed(other, b, a) => {}
                _ => shell.closed_consistent = false,
            }
        }
    }

    let boxes: Vec<Option<(V3, V3)>> = shells.iter().map(|s| aabb_of(ix, &s.tris)).collect();
    // Representative point: centroid of the largest triangle of each shell.
    let reps: Vec<Option<V3>> = shells
        .iter()
        .map(|s| {
            let mut best: Option<(f64, usize)> = None;
            for &ti in &s.tris {
                let Some(&t) = ix.tris.get(ti) else {
                    continue;
                };
                let a2 = ix.double_area(t);
                if best.is_none_or(|(ba, _)| a2 > ba) {
                    best = Some((a2, ti));
                }
            }
            let (_, ti) = best?;
            let [a, b, c] = ix.pts(*ix.tris.get(ti)?)?;
            Some(scale(add(add(a, b), c), 1.0 / 3.0))
        })
        .collect();

    let dir = scale(RAY_DIR, 1.0 / norm(RAY_DIR));
    let n = shells.len();
    for s in 0..n {
        let (Some(Some(sbox)), Some(Some(origin))) = (boxes.get(s), reps.get(s)) else {
            continue;
        };
        let mut containers = Vec::new();
        for t in 0..n {
            if t == s {
                continue;
            }
            let (Some(Some(tbox)), Some(shell_t)) = (boxes.get(t), shells.get(t)) else {
                continue;
            };
            if !shell_t.closed_consistent || !aabb_inside(*sbox, *tbox) {
                continue;
            }
            let mut hits = 0usize;
            for &ti in &shell_t.tris {
                if let Some([a, b, c]) = ix.tris.get(ti).and_then(|&tt| ix.pts(tt))
                    && ray_hits_triangle(*origin, dir, a, b, c)
                {
                    hits += 1;
                }
            }
            if hits % 2 == 1 {
                containers.push(t);
            }
        }
        if let Some(shell) = shells.get_mut(s) {
            shell.containers = containers;
        }
    }
    shells
}

// ---------------------------------------------------------------------------------------------
// Inspection support.
// ---------------------------------------------------------------------------------------------

/// Topology statistics used by [`inspect_mesh`].
pub(crate) struct TopologyStats {
    pub(crate) duplicate_triangles: usize,
    pub(crate) inconsistent_winding_edges: usize,
    pub(crate) inverted_shells: usize,
}

/// Compute duplicate / winding / inverted-shell statistics for an indexed mesh.
pub(crate) fn topology_stats(ix: &Indexed) -> TopologyStats {
    let mut seen: HashSet<[usize; 3]> = HashSet::with_capacity(ix.tris.len());
    let mut duplicate_triangles = 0usize;
    for &t in &ix.tris {
        let mut k = t;
        k.sort_unstable();
        if !seen.insert(k) {
            duplicate_triangles += 1;
        }
    }
    let edges = build_edges(&ix.tris);
    let mut inconsistent_winding_edges = 0usize;
    for (&(a, b), e) in &edges {
        if e.count != 2 {
            continue;
        }
        if let (Some(&t0), Some(&t1)) = (ix.tris.get(e.t0), ix.tris.get(e.t1))
            && has_directed(t0, a, b) == has_directed(t1, a, b)
        {
            inconsistent_winding_edges += 1;
        }
    }
    let comps = orient_components(&ix.tris, &edges);
    let shells = analyze_shells(ix, &edges, &comps);
    let inverted_shells = shells
        .iter()
        .filter(|s| s.closed_consistent && s.containers.is_empty() && s.volume < -1e-9)
        .count();
    TopologyStats {
        duplicate_triangles,
        inconsistent_winding_edges,
        inverted_shells,
    }
}

// ---------------------------------------------------------------------------------------------
// Repair steps.
// ---------------------------------------------------------------------------------------------

type CellKey = (i64, i64, i64);

#[inline]
fn cell_of(p: V3, cell: f64) -> CellKey {
    let [x, y, z] = p;
    (
        (x / cell).floor() as i64,
        (y / cell).floor() as i64,
        (z / cell).floor() as i64,
    )
}

fn neighbor_cells(k: CellKey) -> impl Iterator<Item = CellKey> {
    let (x, y, z) = k;
    (-1i64..=1).flat_map(move |dx| {
        (-1i64..=1).flat_map(move |dy| {
            (-1i64..=1).map(move |dz| {
                (
                    x.saturating_add(dx),
                    y.saturating_add(dy),
                    z.saturating_add(dz),
                )
            })
        })
    })
}

#[inline]
fn bits_key(p: V3) -> [u64; 3] {
    // Normalise -0.0 to +0.0 so they weld exactly.
    p.map(|c| (c + 0.0).to_bits())
}

/// Weld vertices within `tol` using a 27-neighbour spatial hash. Returns the indexed mesh,
/// the number of non-finite triangles dropped and the number of distinct positions welded.
fn weld(mesh: &TriangleMesh, tol: f64) -> (Indexed, usize, usize) {
    let mut ix = Indexed::default();
    let mut nonfinite = 0usize;
    let mut exact: HashMap<[u64; 3], usize> = HashMap::new();
    let mut grid: HashMap<CellKey, Vec<usize>> = HashMap::new();
    let mut distinct_positions = 0usize;
    let tol2 = tol * tol;
    for tri in &mesh.triangles {
        if !tri.is_finite() {
            nonfinite += 1;
            continue;
        }
        let mut idx = [0usize; 3];
        for (slot, p) in idx.iter_mut().zip([tri.a, tri.b, tri.c]) {
            let pv = v3(p);
            let key = bits_key(pv);
            if let Some(&i) = exact.get(&key) {
                *slot = i;
                continue;
            }
            distinct_positions += 1;
            let mut found: Option<usize> = None;
            if tol > 0.0 {
                for nk in neighbor_cells(cell_of(pv, tol)) {
                    let Some(list) = grid.get(&nk) else {
                        continue;
                    };
                    for &cand in list {
                        if found.is_some_and(|f| f <= cand) {
                            continue;
                        }
                        if ix.verts.get(cand).is_some_and(|&q| dist2(pv, q) <= tol2) {
                            found = Some(cand);
                        }
                    }
                }
            }
            let i = match found {
                Some(i) => i,
                None => {
                    let i = ix.verts.len();
                    ix.verts.push(pv);
                    if tol > 0.0 {
                        grid.entry(cell_of(pv, tol)).or_default().push(i);
                    }
                    i
                }
            };
            exact.insert(key, i);
            *slot = i;
        }
        ix.tris.push(idx);
    }
    let welded = distinct_positions.saturating_sub(ix.verts.len());
    (ix, nonfinite, welded)
}

/// Remove collapsed / zero-area triangles and (optionally) duplicates. Returns `(degenerate, duplicate)`.
fn cleanup(ix: &mut Indexed, remove_duplicates: bool) -> (usize, usize) {
    let mut degenerate = 0usize;
    let mut duplicate = 0usize;
    let mut seen: HashSet<[usize; 3]> = HashSet::with_capacity(ix.tris.len());
    let mut kept = Vec::with_capacity(ix.tris.len());
    for &t in &ix.tris {
        let [a, b, c] = t;
        if a == b || b == c || c == a || ix.double_area(t) <= DEGENERATE_DOUBLE_AREA {
            degenerate += 1;
            continue;
        }
        if remove_duplicates {
            let mut k = t;
            k.sort_unstable();
            if !seen.insert(k) {
                duplicate += 1;
                continue;
            }
        }
        kept.push(t);
    }
    ix.tris = kept;
    (degenerate, duplicate)
}

fn sorted_boundary_vertices(half: &[(usize, usize, usize)]) -> Vec<usize> {
    let mut v: Vec<usize> = half.iter().flat_map(|&(a, b, _)| [a, b]).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Snap pairs of nearby, non-adjacent open-boundary vertices together (one merge per vertex).
fn snap_cracks(ix: &mut Indexed, snap: f64) -> usize {
    let edges = build_edges(&ix.tris);
    let half = boundary_half_edges(&ix.tris, &edges);
    if half.is_empty() {
        return 0;
    }
    let bverts = sorted_boundary_vertices(&half);
    let is_boundary: HashSet<usize> = bverts.iter().copied().collect();

    // 1-ring neighbourhoods of boundary vertices.
    let mut ring: HashMap<usize, Vec<usize>> = HashMap::new();
    for &t in &ix.tris {
        for (a, b) in directed_edges(t) {
            if is_boundary.contains(&a) {
                ring.entry(a).or_default().push(b);
            }
            if is_boundary.contains(&b) {
                ring.entry(b).or_default().push(a);
            }
        }
    }
    for list in ring.values_mut() {
        list.sort_unstable();
        list.dedup();
    }
    let near_in_topology = |a: usize, b: usize| -> bool {
        let (Some(ra), Some(rb)) = (ring.get(&a), ring.get(&b)) else {
            return false;
        };
        if ra.binary_search(&b).is_ok() {
            return true;
        }
        ra.iter().any(|x| rb.binary_search(x).is_ok())
    };

    let mut grid: HashMap<CellKey, Vec<usize>> = HashMap::new();
    for &v in &bverts {
        if let Some(&p) = ix.verts.get(v) {
            grid.entry(cell_of(p, snap)).or_default().push(v);
        }
    }
    let snap2 = snap * snap;
    let mut taken: HashSet<usize> = HashSet::new();
    let mut remap: HashMap<usize, usize> = HashMap::new();
    let mut snapped = 0usize;
    for &v in &bverts {
        if taken.contains(&v) {
            continue;
        }
        let Some(&pv) = ix.verts.get(v) else {
            continue;
        };
        let mut best: Option<(f64, usize)> = None;
        for nk in neighbor_cells(cell_of(pv, snap)) {
            let Some(list) = grid.get(&nk) else {
                continue;
            };
            for &w in list {
                if w == v || taken.contains(&w) {
                    continue;
                }
                let Some(&pw) = ix.verts.get(w) else {
                    continue;
                };
                let d2 = dist2(pv, pw);
                if d2 > snap2 {
                    continue;
                }
                let better = match best {
                    None => true,
                    Some((bd, bw)) => d2 < bd || (d2 == bd && w < bw),
                };
                if better && !near_in_topology(v, w) {
                    best = Some((d2, w));
                }
            }
        }
        if let Some((_, w)) = best {
            let (keep, drop) = if v < w { (v, w) } else { (w, v) };
            if let (Some(&pk), Some(&pd)) = (ix.verts.get(keep), ix.verts.get(drop))
                && let Some(slot) = ix.verts.get_mut(keep)
            {
                *slot = scale(add(pk, pd), 0.5);
            }
            remap.insert(drop, keep);
            taken.insert(v);
            taken.insert(w);
            snapped += 1;
        }
    }
    if snapped > 0 {
        for t in &mut ix.tris {
            for i in t.iter_mut() {
                if let Some(&r) = remap.get(i) {
                    *i = r;
                }
            }
        }
    }
    snapped
}

/// Split boundary edges at boundary vertices lying on their interior (within `tol`).
fn split_t_junctions(ix: &mut Indexed, tol: f64) -> usize {
    let mut total = 0usize;
    let tol2 = tol * tol;
    for _pass in 0..16 {
        let edges = build_edges(&ix.tris);
        let half = boundary_half_edges(&ix.tris, &edges);
        if half.is_empty() {
            break;
        }
        let mut by_x: Vec<(f64, usize)> = sorted_boundary_vertices(&half)
            .into_iter()
            .filter_map(|v| {
                ix.verts
                    .get(v)
                    .map(|p| (p.first().copied().unwrap_or(0.0), v))
            })
            .collect();
        by_x.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));

        let mut scheduled = vec![false; ix.tris.len()];
        // (triangle, a, b, c, sorted interior vertices on a->b)
        let mut splits: Vec<(usize, usize, usize, usize, Vec<usize>)> = Vec::new();
        for &(a, b, ti) in &half {
            if scheduled.get(ti).copied().unwrap_or(true) {
                continue;
            }
            let (Some(&pa), Some(&pb), Some(&t)) =
                (ix.verts.get(a), ix.verts.get(b), ix.tris.get(ti))
            else {
                continue;
            };
            let c = t
                .into_iter()
                .find(|&x| x != a && x != b)
                .unwrap_or(usize::MAX);
            let d = sub(pb, pa);
            let len2 = dot(d, d);
            if !len2.is_finite() || len2 <= 4.0 * tol2 {
                continue;
            }
            let [ax, _, _] = pa;
            let [bx, _, _] = pb;
            let lo = ax.min(bx) - tol;
            let hi = ax.max(bx) + tol;
            let start = by_x.partition_point(|&(x, _)| x < lo);
            let mut on_edge: Vec<(f64, usize)> = Vec::new();
            for &(x, w) in by_x.iter().skip(start) {
                if x > hi {
                    break;
                }
                if w == a || w == b || w == c {
                    continue;
                }
                let Some(&pw) = ix.verts.get(w) else {
                    continue;
                };
                let s = dot(sub(pw, pa), d) / len2;
                if !(s > 0.0 && s < 1.0) {
                    continue;
                }
                let foot = add(pa, scale(d, s));
                if dist2(pw, foot) > tol2 || dist2(pw, pa) <= tol2 || dist2(pw, pb) <= tol2 {
                    continue;
                }
                on_edge.push((s, w));
            }
            if on_edge.is_empty() {
                continue;
            }
            on_edge.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
            let mut ws: Vec<usize> = on_edge.into_iter().map(|(_, w)| w).collect();
            ws.dedup();
            if let Some(s) = scheduled.get_mut(ti) {
                *s = true;
            }
            splits.push((ti, a, b, c, ws));
        }
        if splits.is_empty() {
            break;
        }
        splits.sort_by_key(|s| s.0);
        for (ti, a, b, c, ws) in splits {
            total += ws.len();
            let mut chain = Vec::with_capacity(ws.len() + 2);
            chain.push(a);
            chain.extend(ws);
            chain.push(b);
            let mut first = true;
            for pair in chain.windows(2) {
                let (Some(&p), Some(&q)) = (pair.first(), pair.get(1)) else {
                    continue;
                };
                let nt = [p, q, c];
                if first {
                    if let Some(slot) = ix.tris.get_mut(ti) {
                        *slot = nt;
                    }
                    first = false;
                } else {
                    ix.tris.push(nt);
                }
            }
        }
    }
    total
}

/// Triangulate one hole loop (vertex order already matches the required fill winding).
fn triangulate_loop(verts: &mut Vec<V3>, lp: &[usize]) -> Vec<[usize; 3]> {
    if let [a, b, c] = *lp {
        return vec![[a, b, c]];
    }
    let pts: Vec<V3> = lp.iter().filter_map(|&i| verts.get(i).copied()).collect();
    if pts.len() != lp.len() || pts.len() < 3 {
        return Vec::new();
    }
    let n = pts.len();
    // Newell normal.
    let mut nrm = [0.0; 3];
    for i in 0..n {
        let (Some(&p), Some(&q)) = (pts.get(i), pts.get((i + 1) % n)) else {
            continue;
        };
        let ([px, py, pz], [qx, qy, qz]) = (p, q);
        nrm = add(
            nrm,
            [
                (py - qy) * (pz + qz),
                (pz - qz) * (px + qx),
                (px - qx) * (py + qy),
            ],
        );
    }
    let nlen = norm(nrm);
    let ear = if nlen.is_finite() && nlen > 1e-18 {
        let nh = scale(nrm, 1.0 / nlen);
        let [nx, ny, nz] = nh;
        let axis = if nx.abs() <= ny.abs() && nx.abs() <= nz.abs() {
            [1.0, 0.0, 0.0]
        } else if ny.abs() <= nz.abs() {
            [0.0, 1.0, 0.0]
        } else {
            [0.0, 0.0, 1.0]
        };
        let u = cross(axis, nh);
        let u = scale(u, 1.0 / norm(u).max(1e-300));
        let w = cross(nh, u);
        let p2: Vec<[f64; 2]> = pts.iter().map(|&p| [dot(p, u), dot(p, w)]).collect();
        ear_clip(&p2)
    } else {
        None
    };
    match ear {
        Some(local) => local
            .into_iter()
            .filter_map(|[i, j, k]| Some([*lp.get(i)?, *lp.get(j)?, *lp.get(k)?]))
            .collect(),
        None => {
            // Centroid-fan fallback for twisted / non-planar holes.
            let centroid = scale(
                pts.iter().fold([0.0; 3], |acc, &p| add(acc, p)),
                1.0 / n as f64,
            );
            if !finite3(centroid) {
                return Vec::new();
            }
            let ci = verts.len();
            verts.push(centroid);
            (0..n)
                .filter_map(|i| Some([*lp.get(i)?, *lp.get((i + 1) % n)?, ci]))
                .collect()
        }
    }
}

#[inline]
fn cross2(o: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let ([ox, oy], [ax, ay], [bx, by]) = (o, a, b);
    (ax - ox) * (by - oy) - (ay - oy) * (bx - ox)
}

/// Ear-clip a CCW simple polygon; returns local index triangles or `None` when it gets stuck.
fn ear_clip(p: &[[f64; 2]]) -> Option<Vec<[usize; 3]>> {
    let n = p.len();
    let mut area2 = 0.0;
    for i in 0..n {
        area2 += cross2([0.0, 0.0], *p.get(i)?, *p.get((i + 1) % n)?);
    }
    if !area2.is_finite() || area2 <= 0.0 {
        return None;
    }
    let scale_len = p
        .iter()
        .flat_map(|q| q.iter())
        .fold(0.0_f64, |m, v| m.max(v.abs()))
        .max(1e-9);
    let eps = 1e-14 * scale_len * scale_len;
    let mut idx: Vec<usize> = (0..n).collect();
    let mut out = Vec::with_capacity(n.saturating_sub(2));
    let mut guard = 0usize;
    while idx.len() > 3 {
        guard += 1;
        if guard > n * n + 8 {
            return None;
        }
        let m = idx.len();
        let mut clipped = false;
        for k in 0..m {
            let (Some(&ip), Some(&ic), Some(&inx)) =
                (idx.get((k + m - 1) % m), idx.get(k), idx.get((k + 1) % m))
            else {
                return None;
            };
            let (a, b, c) = (*p.get(ip)?, *p.get(ic)?, *p.get(inx)?);
            if cross2(a, b, c) <= eps {
                continue;
            }
            let blocked = idx.iter().any(|&j| {
                if j == ip || j == ic || j == inx {
                    return false;
                }
                let Some(&q) = p.get(j) else {
                    return false;
                };
                if q == a || q == b || q == c {
                    return false;
                }
                cross2(a, b, q) >= 0.0 && cross2(b, c, q) >= 0.0 && cross2(c, a, q) >= 0.0
            });
            if blocked {
                continue;
            }
            out.push([ip, ic, inx]);
            idx.remove(k);
            clipped = true;
            break;
        }
        if !clipped {
            return None;
        }
    }
    if let [a, b, c] = *idx.as_slice() {
        out.push([a, b, c]);
    }
    Some(out)
}

/// Fill boundary loops up to `max_edges` edges. Returns `(filled, triangles_added, unfilled)`.
fn fill_holes(ix: &mut Indexed, max_edges: usize) -> (usize, usize, usize) {
    let edges = build_edges(&ix.tris);
    let half = boundary_half_edges(&ix.tris, &edges);
    if half.is_empty() {
        return (0, 0, 0);
    }
    // Hole half-edges run opposite to the boundary half-edges.
    let hole: Vec<(usize, usize)> = half.iter().map(|&(a, b, _)| (b, a)).collect();
    let mut out_of: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, &(s, _)) in hole.iter().enumerate() {
        out_of.entry(s).or_default().push(i);
    }
    let mut used = vec![false; hole.len()];
    let (mut filled, mut added, mut unfilled) = (0usize, 0usize, 0usize);
    let mut new_tris = Vec::new();
    for start in 0..hole.len() {
        if used.get(start).copied().unwrap_or(true) {
            continue;
        }
        let Some(&(s0, mut end)) = hole.get(start) else {
            continue;
        };
        if let Some(u) = used.get_mut(start) {
            *u = true;
        }
        let mut lp = vec![s0];
        let mut closed = false;
        let mut guard = 0usize;
        while guard <= hole.len() {
            guard += 1;
            if end == s0 {
                closed = true;
                break;
            }
            lp.push(end);
            let next = out_of.get(&end).and_then(|c| {
                c.iter()
                    .copied()
                    .find(|&i| !used.get(i).copied().unwrap_or(true))
            });
            let Some(ni) = next else {
                break;
            };
            if let Some(u) = used.get_mut(ni) {
                *u = true;
            }
            end = hole.get(ni).map(|&(_, e)| e).unwrap_or(s0);
        }
        if !closed || lp.len() < 3 || lp.len() > max_edges {
            unfilled += 1;
            continue;
        }
        let tris = triangulate_loop(&mut ix.verts, &lp);
        if tris.is_empty() {
            unfilled += 1;
            continue;
        }
        filled += 1;
        added += tris.len();
        new_tris.extend(tris);
    }
    ix.tris.extend(new_tris);
    (filled, added, unfilled)
}

fn validate(config: &MeshRepairConfig) -> Result<(), MeshError> {
    if !config.weld_tolerance_mm.is_finite() || config.weld_tolerance_mm < 0.0 {
        return Err(MeshError::InvalidRepairConfig(
            "weld_tolerance_mm must be finite and >= 0",
        ));
    }
    if config.weld_tolerance_mm > 1.0 {
        return Err(MeshError::InvalidRepairConfig(
            "weld_tolerance_mm must be <= 1.0 mm",
        ));
    }
    if !config.crack_snap_mm.is_finite() || config.crack_snap_mm < 0.0 {
        return Err(MeshError::InvalidRepairConfig(
            "crack_snap_mm must be finite and >= 0",
        ));
    }
    if config.crack_snap_mm > 5.0 {
        return Err(MeshError::InvalidRepairConfig(
            "crack_snap_mm must be <= 5.0 mm",
        ));
    }
    Ok(())
}

/// Repair a triangle mesh deterministically. Never panics on hostile input.
///
/// Watertight, consistently wound meshes without near-coincident vertices are returned unchanged.
pub fn repair_mesh(
    mesh: &TriangleMesh,
    config: &MeshRepairConfig,
) -> Result<RepairedMesh, MeshError> {
    validate(config)?;
    if mesh.triangles.is_empty() {
        return Err(GeomError::EmptyMesh.into());
    }
    let before = inspect_mesh(mesh);

    let (mut ix, nonfinite, welded_vertices) = weld(mesh, config.weld_tolerance_mm);
    let (mut removed_degenerate, mut removed_duplicate) =
        cleanup(&mut ix, config.remove_duplicates);
    removed_degenerate += nonfinite;

    let mut cracks_snapped = 0usize;
    if config.crack_snap_mm > 0.0 {
        cracks_snapped = snap_cracks(&mut ix, config.crack_snap_mm);
        if cracks_snapped > 0 {
            let (d, u) = cleanup(&mut ix, config.remove_duplicates);
            removed_degenerate += d;
            removed_duplicate += u;
        }
    }
    let mut t_junctions_split = 0usize;
    if config.split_t_junctions {
        let tol = config.crack_snap_mm.max(config.weld_tolerance_mm).max(1e-6);
        t_junctions_split = split_t_junctions(&mut ix, tol);
        if t_junctions_split > 0 {
            let (d, u) = cleanup(&mut ix, config.remove_duplicates);
            removed_degenerate += d;
            removed_duplicate += u;
        }
    }

    let base_count = ix.tris.len();
    let mut flip_state = vec![false; base_count];
    let apply_flips = |ix: &mut Indexed, flips: &[bool], state: &mut [bool]| {
        for (ti, t) in ix.tris.iter_mut().enumerate() {
            if flips.get(ti).copied().unwrap_or(false) {
                *t = flip_tri(*t);
                if let Some(s) = state.get_mut(ti) {
                    *s = !*s;
                }
            }
        }
    };

    if config.fix_winding {
        let edges = build_edges(&ix.tris);
        let comps = orient_components(&ix.tris, &edges);
        apply_flips(&mut ix, &comps.flip, &mut flip_state);
    }

    let (mut holes_filled, mut hole_triangles_added, mut holes_unfilled) = (0, 0, 0);
    if config.fill_holes {
        (holes_filled, hole_triangles_added, holes_unfilled) =
            fill_holes(&mut ix, config.max_hole_edges);
    }

    let edges = build_edges(&ix.tris);
    let comps = orient_components(&ix.tris, &edges);
    let shells_count = comps.count;
    let mut inverted_shells_fixed = 0usize;
    let mut internal_cavities_preserved = 0usize;
    if config.fix_winding {
        apply_flips(&mut ix, &comps.flip, &mut flip_state);
        let edges = build_edges(&ix.tris);
        let comps = orient_components(&ix.tris, &edges);
        let shells = analyze_shells(&ix, &edges, &comps);
        // Flip inside-out top-level shells together with everything they enclose, so
        // genuine cavities (opposite orientation to their container) stay voids.
        let mut shell_flip = vec![false; shells.len()];
        for (r, shell) in shells.iter().enumerate() {
            if shell.containers.is_empty() && shell.volume < -1e-9 {
                inverted_shells_fixed += 1;
                if let Some(f) = shell_flip.get_mut(r) {
                    *f = !*f;
                }
                for (s, other) in shells.iter().enumerate() {
                    if other.containers.contains(&r)
                        && let Some(f) = shell_flip.get_mut(s)
                    {
                        *f = !*f;
                    }
                }
            }
        }
        for (s, shell) in shells.iter().enumerate() {
            let flipped = shell_flip.get(s).copied().unwrap_or(false);
            let vol = if flipped { -shell.volume } else { shell.volume };
            if !shell.containers.is_empty() && vol < -1e-9 {
                internal_cavities_preserved += 1;
            }
        }
        let tri_flips: Vec<bool> = (0..ix.tris.len())
            .map(|ti| {
                comps
                    .comp
                    .get(ti)
                    .and_then(|&c| shell_flip.get(c))
                    .copied()
                    .unwrap_or(false)
            })
            .collect();
        apply_flips(&mut ix, &tri_flips, &mut flip_state);
    }
    let flipped_triangles = flip_state.iter().filter(|&&f| f).count();

    let mut out = Vec::with_capacity(ix.tris.len());
    for &t in &ix.tris {
        if ix.double_area(t) <= DEGENERATE_DOUBLE_AREA {
            continue;
        }
        if let Some([a, b, c]) = ix.pts(t) {
            out.push(Triangle::new(p3(a), p3(b), p3(c)));
        }
    }
    let repaired = TriangleMesh::new(out)?;
    let after = inspect_mesh(&repaired);
    let changed = repaired.triangles != mesh.triangles;
    Ok(RepairedMesh {
        report: MeshRepairReport {
            input_triangles: mesh.triangles.len(),
            output_triangles: repaired.triangles.len(),
            welded_vertices,
            removed_degenerate,
            removed_duplicate,
            t_junctions_split,
            cracks_snapped,
            flipped_triangles,
            shells: shells_count,
            inverted_shells_fixed,
            internal_cavities_preserved,
            holes_filled,
            hole_triangles_added,
            holes_unfilled,
            changed,
            before,
            after,
        },
        mesh: repaired,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_tris(ox: f64, oy: f64, oz: f64, sx: f64, sy: f64, sz: f64) -> Vec<Triangle> {
        let p = |x, y, z| Point3::new(x, y, z);
        let (x0, y0, z0, x1, y1, z1) = (ox, oy, oz, ox + sx, oy + sy, oz + sz);
        [
            ((x0, y0, z0), (x0, y1, z0), (x1, y1, z0)),
            ((x0, y0, z0), (x1, y1, z0), (x1, y0, z0)),
            ((x0, y0, z1), (x1, y0, z1), (x1, y1, z1)),
            ((x0, y0, z1), (x1, y1, z1), (x0, y1, z1)),
            ((x0, y0, z0), (x1, y0, z0), (x1, y0, z1)),
            ((x0, y0, z0), (x1, y0, z1), (x0, y0, z1)),
            ((x0, y1, z0), (x0, y1, z1), (x1, y1, z1)),
            ((x0, y1, z0), (x1, y1, z1), (x1, y1, z0)),
            ((x0, y0, z0), (x0, y0, z1), (x0, y1, z1)),
            ((x0, y0, z0), (x0, y1, z1), (x0, y1, z0)),
            ((x1, y0, z0), (x1, y1, z0), (x1, y1, z1)),
            ((x1, y0, z0), (x1, y1, z1), (x1, y0, z1)),
        ]
        .into_iter()
        .map(|(a, b, c)| Triangle::new(p(a.0, a.1, a.2), p(b.0, b.1, b.2), p(c.0, c.1, c.2)))
        .collect()
    }

    #[test]
    fn clean_cube_is_unchanged_and_inspection_is_clean() {
        let cube = TriangleMesh::new(box_tris(0.0, 0.0, 0.0, 20.0, 20.0, 20.0)).unwrap();
        let insp = inspect_mesh(&cube);
        assert_eq!(insp.boundary_edges, 0);
        assert_eq!(insp.duplicate_triangles, 0);
        assert_eq!(insp.inconsistent_winding_edges, 0);
        assert_eq!(insp.inverted_shells, 0);
        assert!(!insp.needs_repair);

        let res = repair_mesh(&cube, &MeshRepairConfig::default()).unwrap();
        assert!(!res.report.changed);
        assert!(!res.report.after.needs_repair);
        assert_eq!(res.mesh.triangles.len(), 12);
    }

    #[test]
    fn repairs_welding_cracks_duplicates_degenerate_and_tjunctions() {
        let mut tris = box_tris(0.0, 0.0, 0.0, 20.0, 20.0, 20.0);
        // 1. Add duplicate & degenerate triangles.
        tris.push(tris[0]);
        tris.push(tris[1].flipped());
        let deg_p = Point3::new(5.0, 5.0, 5.0);
        tris.push(Triangle::new(deg_p, deg_p, Point3::new(6.0, 5.0, 5.0)));

        // 2. Introduce a 0.02 mm vertex jitter / crack on one shared corner of face 0.
        tris[2].b.x += 0.002; // welded by 0.005 mm weld tolerance
        tris[4].c.z -= 0.03; // snapped by 0.05 mm crack snap

        let messy = TriangleMesh::new(tris).unwrap();
        let before = inspect_mesh(&messy);
        assert!(before.needs_repair);
        assert!(before.degenerate_triangles >= 1);
        assert!(before.duplicate_triangles >= 1);

        let res = repair_mesh(&messy, &MeshRepairConfig::default()).unwrap();
        assert!(res.report.changed);
        assert!(res.report.removed_degenerate >= 1);
        assert!(res.report.removed_duplicate >= 2);
        assert!(!res.report.after.needs_repair, "{:?}", res.report.after);
        assert_eq!(res.report.after.non_manifold_edges, 0);
        assert!((res.report.after.volume_mm3 - 8000.0).abs() < 15.0);

        // T-junction test: split one triangle at midpoint of a neighbor's long edge.
        let mut tj = box_tris(0.0, 0.0, 0.0, 20.0, 20.0, 20.0);
        let t = tj.remove(2); // top face tri 0: (0,0,20) -> (20,0,20) -> (20,20,20)
        let mid = Point3::new(10.0, 10.0, 20.0); // midpoint on diagonal (0,0,20)-(20,20,20) shared with tri 1
        tj.push(Triangle::new(t.a, t.b, mid));
        tj.push(Triangle::new(t.b, t.c, mid));
        let tj_mesh = TriangleMesh::new(tj).unwrap();
        assert!(inspect_mesh(&tj_mesh).boundary_edges > 0);
        let tj_rep = repair_mesh(&tj_mesh, &MeshRepairConfig::default()).unwrap();
        assert!(tj_rep.report.t_junctions_split >= 1);
        assert!(
            !tj_rep.report.after.needs_repair,
            "{:?}",
            tj_rep.report.after
        );
        assert_eq!(tj_rep.report.after.boundary_edges, 0);
    }

    #[test]
    fn repairs_flipped_triangle_inverted_shell_hole_and_preserves_internal_cavity() {
        // 1. Flipped triangle + missing face (2 tris -> 4-edge hole).
        let mut holey = box_tris(0.0, 0.0, 0.0, 20.0, 20.0, 20.0);
        holey.pop();
        holey.pop();
        holey[0] = holey[0].flipped();
        let holey_mesh = TriangleMesh::new(holey).unwrap();
        let before = inspect_mesh(&holey_mesh);
        assert!(before.needs_repair);
        assert!(before.boundary_edges >= 4);
        assert!(before.inconsistent_winding_edges >= 1);

        let fixed = repair_mesh(&holey_mesh, &MeshRepairConfig::default()).unwrap();
        assert!(fixed.report.holes_filled >= 1);
        assert!(fixed.report.hole_triangles_added >= 2);
        assert!(!fixed.report.after.needs_repair, "{:?}", fixed.report.after);
        assert!((fixed.report.after.volume_mm3 - 8000.0).abs() < 1e-4);

        // 2. Inverted shell (all triangles reversed).
        let inv_tris: Vec<Triangle> = box_tris(0.0, 0.0, 0.0, 10.0, 10.0, 10.0)
            .into_iter()
            .map(Triangle::flipped)
            .collect();
        let inv_mesh = TriangleMesh::new(inv_tris).unwrap();
        let inv_insp = inspect_mesh(&inv_mesh);
        assert_eq!(inv_insp.inverted_shells, 1);
        assert!(inv_insp.needs_repair);
        let inv_fix = repair_mesh(&inv_mesh, &MeshRepairConfig::default()).unwrap();
        assert_eq!(inv_fix.report.inverted_shells_fixed, 1);
        assert!(!inv_fix.report.after.needs_repair);
        assert!((inv_fix.report.after.volume_mm3 - 1000.0).abs() < 1e-4);

        // 3. Enclosed internal void cavity preserved inside an inverted outer shell!
        // Outer 20mm cube (volume 8000) inverted, enclosing inner 10mm cube cavity (-1000).
        // When both are inverted initially -> outer gets flipped (+8000) and inner cavity flips with it (-1000) => net 7000!
        let mut hollow_tris: Vec<Triangle> = box_tris(0.0, 0.0, 0.0, 20.0, 20.0, 20.0)
            .into_iter()
            .map(Triangle::flipped)
            .collect();
        // Genuine cavity relative to inverted outer would have positive volume initially, so flipping outer makes cavity negative:
        hollow_tris.extend(box_tris(5.0, 5.0, 5.0, 10.0, 10.0, 10.0));
        let hollow_mesh = TriangleMesh::new(hollow_tris).unwrap();
        let hollow_fix = repair_mesh(&hollow_mesh, &MeshRepairConfig::default()).unwrap();
        assert_eq!(hollow_fix.report.shells, 2);
        assert_eq!(hollow_fix.report.internal_cavities_preserved, 1);
        assert!((hollow_fix.report.after.volume_mm3 - 7000.0).abs() < 1e-3);
        assert!(!hollow_fix.report.after.needs_repair);
    }

    #[test]
    fn hostile_repair_inputs_fail_cleanly_without_panic() {
        let cube = TriangleMesh::new(box_tris(0.0, 0.0, 0.0, 10.0, 10.0, 10.0)).unwrap();
        for bad in [
            MeshRepairConfig {
                weld_tolerance_mm: -0.1,
                ..Default::default()
            },
            MeshRepairConfig {
                weld_tolerance_mm: f64::NAN,
                ..Default::default()
            },
            MeshRepairConfig {
                weld_tolerance_mm: 2.0,
                ..Default::default()
            },
            MeshRepairConfig {
                crack_snap_mm: -1.0,
                ..Default::default()
            },
            MeshRepairConfig {
                crack_snap_mm: 10.0,
                ..Default::default()
            },
        ] {
            assert!(matches!(
                repair_mesh(&cube, &bad),
                Err(MeshError::InvalidRepairConfig(_))
            ));
        }
        let empty = TriangleMesh {
            triangles: Vec::new(),
        };
        assert!(repair_mesh(&empty, &MeshRepairConfig::default()).is_err());

        let nan_tri = Triangle::new(
            Point3::new(f64::NAN, 0.0, 0.0),
            Point3::new(1.0, 0.0, 0.0),
            Point3::new(0.0, 1.0, 0.0),
        );
        let nan_mesh = TriangleMesh {
            triangles: vec![nan_tri],
        };
        assert!(repair_mesh(&nan_mesh, &MeshRepairConfig::default()).is_err());
    }
}
