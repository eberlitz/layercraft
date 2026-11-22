//! Triangle mesh inspection, bed placement, and exact Z-plane slicing.

use std::collections::HashMap;
use std::f64::consts::TAU;

use layercraft_clip::{ClipError, union_polygons};
use layercraft_geom::{
    Aabb3, ExPolygon, GeomError, Point2, Point3, Polygon, Triangle, TriangleMesh, mm_to_nm,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors produced during mesh preparation or slicing.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum MeshError {
    /// Geometry conversion or validation failure.
    #[error(transparent)]
    Geom(#[from] GeomError),
    /// 2D polygon kernel failure while reconstructing slice regions.
    #[error(transparent)]
    Clip(#[from] ClipError),
    /// Invalid layer schedule parameters.
    #[error("invalid slicing parameters: {0}")]
    InvalidSchedule(&'static str),
}

/// Summary properties computed from a triangle mesh.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MeshInspection {
    pub triangle_count: usize,
    pub aabb: Option<Aabb3>,
    pub volume_mm3: f64,
    pub surface_area_mm2: f64,
    pub degenerate_triangles: usize,
    pub non_manifold_edges: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct QuantPt3(i64, i64, i64);

fn quantize_pt3(p: Point3) -> Result<QuantPt3, GeomError> {
    // 100 nm tolerance bucket for edge manifoldness counting.
    let qx = (mm_to_nm(p.x)? as f64 / 100.0).round() as i64;
    let qy = (mm_to_nm(p.y)? as f64 / 100.0).round() as i64;
    let qz = (mm_to_nm(p.z)? as f64 / 100.0).round() as i64;
    Ok(QuantPt3(qx, qy, qz))
}

fn edge_key(a: QuantPt3, b: QuantPt3) -> (QuantPt3, QuantPt3) {
    if (a.0, a.1, a.2) <= (b.0, b.1, b.2) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Inspect a triangle mesh for bounding box, volume, surface area, and topology anomalies.
pub fn inspect_mesh(mesh: &TriangleMesh) -> MeshInspection {
    let mut degenerate_triangles = 0usize;
    let mut edge_counts: HashMap<(QuantPt3, QuantPt3), usize> = HashMap::new();

    for tri in &mesh.triangles {
        if tri.area_mm2() <= 1e-12 {
            degenerate_triangles += 1;
            continue;
        }
        if let (Ok(qa), Ok(qb), Ok(qc)) = (
            quantize_pt3(tri.a),
            quantize_pt3(tri.b),
            quantize_pt3(tri.c),
        ) {
            for (u, v) in [(qa, qb), (qb, qc), (qc, qa)] {
                if u != v {
                    let count = edge_counts.entry(edge_key(u, v)).or_insert(0);
                    *count += 1;
                }
            }
        }
    }

    let non_manifold_edges = edge_counts.values().filter(|&&c| c != 2).count();

    MeshInspection {
        triangle_count: mesh.triangles.len(),
        aabb: mesh.aabb(),
        volume_mm3: mesh.signed_volume_mm3(),
        surface_area_mm2: mesh.surface_area_mm2(),
        degenerate_triangles,
        non_manifold_edges,
    }
}

/// Drop zero-area degenerate triangles and ground + center the mesh onto the build plate.
pub fn prepare_for_plate(
    mesh: &TriangleMesh,
    bed_width_mm: f64,
    bed_depth_mm: f64,
) -> Result<TriangleMesh, MeshError> {
    if !bed_width_mm.is_finite()
        || !bed_depth_mm.is_finite()
        || bed_width_mm <= 0.0
        || bed_depth_mm <= 0.0
    {
        return Err(MeshError::InvalidSchedule(
            "bed dimensions must be finite and positive",
        ));
    }

    let valid: Vec<Triangle> = mesh
        .triangles
        .iter()
        .copied()
        .filter(|t| t.is_finite() && t.area_mm2() > 1e-12)
        .collect();
    let mut clean = TriangleMesh::new(valid)?;
    clean.ground_and_center(0.5 * bed_width_mm, 0.5 * bed_depth_mm)?;
    Ok(clean)
}

/// A single layer Z slice definition.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LayerPlane {
    pub index: usize,
    pub z_top_mm: f64,
    pub z_slice_mm: f64,
    pub height_mm: f64,
}

/// Build the sequence of layer planes covering `[0, max_z_mm]`.
pub fn build_layer_schedule(
    max_z_mm: f64,
    first_layer_height_mm: f64,
    layer_height_mm: f64,
) -> Result<Vec<LayerPlane>, MeshError> {
    if !max_z_mm.is_finite() || max_z_mm <= 0.0 {
        return Err(MeshError::InvalidSchedule("max_z must be positive"));
    }
    if !first_layer_height_mm.is_finite() || first_layer_height_mm <= 0.0 {
        return Err(MeshError::InvalidSchedule(
            "first_layer_height must be positive",
        ));
    }
    if !layer_height_mm.is_finite() || layer_height_mm <= 0.0 {
        return Err(MeshError::InvalidSchedule("layer_height must be positive"));
    }

    let mut schedule = Vec::new();
    let mut z_prev = 0.0;
    let eps = 1e-4;

    if max_z_mm > eps {
        let z_top = first_layer_height_mm;
        schedule.push(LayerPlane {
            index: 0,
            z_top_mm: z_top,
            z_slice_mm: 0.5 * z_top,
            height_mm: first_layer_height_mm,
        });
        z_prev = z_top;
    }

    let mut idx = 1usize;
    while z_prev + eps < max_z_mm {
        let z_top = first_layer_height_mm + (idx as f64) * layer_height_mm;
        let z_slice = z_prev + 0.5 * layer_height_mm;
        if z_slice >= max_z_mm {
            break;
        }
        schedule.push(LayerPlane {
            index: idx,
            z_top_mm: z_top,
            z_slice_mm: z_slice,
            height_mm: layer_height_mm,
        });
        z_prev = z_top;
        idx += 1;
        if idx > 1_000_000 {
            return Err(MeshError::InvalidSchedule("excessive layer count"));
        }
    }

    Ok(schedule)
}

/// Raw 2D slice regions produced at one Z plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RawSliceLayer {
    pub plane: LayerPlane,
    pub regions: Vec<ExPolygon>,
}

/// Slice a prepared mesh (`min_z == 0`) along the configured layer schedule.
pub fn slice_mesh(
    mesh: &TriangleMesh,
    first_layer_height_mm: f64,
    layer_height_mm: f64,
) -> Result<Vec<RawSliceLayer>, MeshError> {
    let aabb = mesh.aabb().ok_or(GeomError::EmptyMesh)?;
    let max_z = aabb.max.z - aabb.min.z;
    let schedule = build_layer_schedule(max_z, first_layer_height_mm, layer_height_mm)?;

    // Precompute per-triangle Z bounds for fast per-layer filtering.
    let tri_z: Vec<(f64, f64, &Triangle)> = mesh
        .triangles
        .iter()
        .map(|t| {
            let zmin = t.a.z.min(t.b.z).min(t.c.z);
            let zmax = t.a.z.max(t.b.z).max(t.c.z);
            (zmin, zmax, t)
        })
        .collect();

    let mut layers = Vec::with_capacity(schedule.len());
    for plane in schedule {
        let z = plane.z_slice_mm;
        let mut segments: Vec<(Point2, Point2)> = Vec::new();

        for &(zmin, zmax, tri) in &tri_z {
            if zmin > z || zmax <= z {
                continue;
            }
            if let Some(seg) = intersect_triangle_at_z(tri, z)?
                && seg.0 != seg.1
            {
                segments.push(seg);
            }
        }

        let loops = stitch_segments_into_loops(&segments);
        let regions = union_polygons(&loops)?;
        layers.push(RawSliceLayer { plane, regions });
    }

    Ok(layers)
}

fn lerp_edge_xy(p0: Point3, p1: Point3, z: f64) -> Result<Point2, GeomError> {
    let dz = p1.z - p0.z;
    let t = if dz.abs() < 1e-15 {
        0.5
    } else {
        ((z - p0.z) / dz).clamp(0.0, 1.0)
    };
    let x = p0.x + t * (p1.x - p0.x);
    let y = p0.y + t * (p1.y - p0.y);
    Point2::from_mm(x, y)
}

/// Intersect a single triangle with horizontal plane `z` using half-open edge intervals
/// `[min_z, max_z)` so shared vertices on `z` never double-count or drop segments.
///
/// Edge orientation is chosen so that when viewed from `+Z`, the outward mesh normal
/// lies to the right of the directed segment `start -> end` (CCW outer contours).
fn intersect_triangle_at_z(tri: &Triangle, z: f64) -> Result<Option<(Point2, Point2)>, GeomError> {
    let edges = [(tri.a, tri.b), (tri.b, tri.c), (tri.c, tri.a)];
    let mut enter: Option<Point2> = None;
    let mut exit: Option<Point2> = None;

    for (p0, p1) in edges {
        let upward = p0.z <= z && p1.z > z;
        let downward = p1.z <= z && p0.z > z;
        if upward {
            enter = Some(lerp_edge_xy(p0, p1, z)?);
        } else if downward {
            exit = Some(lerp_edge_xy(p0, p1, z)?);
        }
    }

    match (enter, exit) {
        // An edge entering z upwards followed by an edge exiting z downwards gives
        // right-hand outward normal => CCW outer boundary when traversing enter -> exit
        // or exit -> enter; verify via cube front face (y=0, normal (0,-1,0)):
        // (0,0,0)->(20,0,0)->(20,0,20): upward on (20,0,0)->(20,0,20) gives x=20,y=0;
        // downward on (0,0,20)->(0,0,0) gives x=0,y=0. On front face y=0, CCW square goes
        // (0,0) -> (20,0), which is exit -> enter!
        (Some(p_up), Some(p_down)) => Ok(Some((p_down, p_up))),
        _ => Ok(None),
    }
}

fn stitch_segments_into_loops(segments: &[(Point2, Point2)]) -> Vec<Polygon> {
    if segments.is_empty() {
        return Vec::new();
    }

    let mut start_map: HashMap<Point2, Vec<usize>> = HashMap::with_capacity(segments.len());
    for (idx, &(start, _)) in segments.iter().enumerate() {
        start_map.entry(start).or_default().push(idx);
    }

    let mut used = vec![false; segments.len()];
    let mut loops = Vec::new();

    for start_idx in 0..segments.len() {
        if used.get(start_idx).copied().unwrap_or(true) {
            continue;
        }
        let Some(&(first_pt, mut cur_end)) = segments.get(start_idx) else {
            continue;
        };
        if let Some(u) = used.get_mut(start_idx) {
            *u = true;
        }

        let mut pts = vec![first_pt, cur_end];
        let mut guard = 0usize;
        while cur_end != first_pt && guard <= segments.len() {
            guard += 1;
            let next_idx = start_map.get(&cur_end).and_then(|cands| {
                cands
                    .iter()
                    .copied()
                    .find(|&idx| !used.get(idx).copied().unwrap_or(true))
            });

            if let Some(idx) = next_idx {
                if let Some(u) = used.get_mut(idx) {
                    *u = true;
                }
                if let Some(&(_, end_pt)) = segments.get(idx) {
                    cur_end = end_pt;
                    if cur_end != first_pt {
                        pts.push(cur_end);
                    }
                }
            } else {
                // Tolerance fallback for slightly cracked STL meshes (within 10 um = 10_000 nm).
                let max_snap_sq: i128 = 10_000i128 * 10_000i128;
                let mut best: Option<(usize, i128)> = None;
                for (idx, &(cand_start, _)) in segments.iter().enumerate() {
                    if used.get(idx).copied().unwrap_or(true) {
                        continue;
                    }
                    let d2 = cur_end.distance_sq_nm(cand_start);
                    if d2 <= max_snap_sq && best.is_none_or(|(_, bd)| d2 < bd) {
                        best = Some((idx, d2));
                    }
                }
                if let Some((idx, _)) = best {
                    if let Some(u) = used.get_mut(idx) {
                        *u = true;
                    }
                    if let Some(&(_, end_pt)) = segments.get(idx) {
                        cur_end = end_pt;
                        if cur_end != first_pt {
                            pts.push(cur_end);
                        }
                        continue;
                    }
                }
                break;
            }
        }

        // Only emit closed (or nearly closed) contours with >= 3 distinct vertices.
        if pts.len() >= 3
            && (cur_end == first_pt || cur_end.distance_sq_nm(first_pt) <= 10_000i128 * 10_000i128)
        {
            loops.push(Polygon::new(pts));
        }
    }

    loops
}

/// Built-in watertight sample geometries available in headless CLI, desktop UI, web, and MCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SampleShape {
    #[default]
    Cube20,
    Cylinder,
    HollowTube,
    SteppedBlock,
    ThinRib,
}

impl SampleShape {
    /// Human-friendly display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Cube20 => "Cube 20 mm",
            Self::Cylinder => "Cylinder R10 H20",
            Self::HollowTube => "Hollow Tube R15/R8",
            Self::SteppedBlock => "Stepped Block",
            Self::ThinRib => "Thin Rib 30x1.6",
        }
    }

    /// All built-in sample variants.
    pub const ALL: [Self; 5] = [
        Self::Cube20,
        Self::Cylinder,
        Self::HollowTube,
        Self::SteppedBlock,
        Self::ThinRib,
    ];
}

fn push_sample_quad(tris: &mut Vec<Triangle>, a: Point3, b: Point3, c: Point3, d: Point3) {
    tris.push(Triangle::new(a, b, c));
    tris.push(Triangle::new(a, c, d));
}

fn sample_box(w: f64, d: f64, h: f64, ox: f64, oy: f64, oz: f64) -> Vec<Triangle> {
    let p000 = Point3::new(ox, oy, oz);
    let p100 = Point3::new(ox + w, oy, oz);
    let p110 = Point3::new(ox + w, oy + d, oz);
    let p010 = Point3::new(ox, oy + d, oz);
    let p001 = Point3::new(ox, oy, oz + h);
    let p101 = Point3::new(ox + w, oy, oz + h);
    let p111 = Point3::new(ox + w, oy + d, oz + h);
    let p011 = Point3::new(ox, oy + d, oz + h);

    let mut tris = Vec::with_capacity(12);
    push_sample_quad(&mut tris, p000, p010, p110, p100);
    push_sample_quad(&mut tris, p001, p101, p111, p011);
    push_sample_quad(&mut tris, p000, p100, p101, p001);
    push_sample_quad(&mut tris, p010, p011, p111, p110);
    push_sample_quad(&mut tris, p000, p001, p011, p010);
    push_sample_quad(&mut tris, p100, p110, p111, p101);
    tris
}

/// Build one of LayerCraft's built-in sample primitives.
pub fn build_sample_shape(shape: SampleShape) -> Result<TriangleMesh, MeshError> {
    let tris = match shape {
        SampleShape::Cube20 => sample_box(20.0, 20.0, 20.0, 0.0, 0.0, 0.0),
        SampleShape::ThinRib => sample_box(30.0, 1.6, 10.0, 0.0, 0.0, 0.0),
        SampleShape::SteppedBlock => {
            let mut t = sample_box(20.0, 20.0, 10.0, 0.0, 0.0, 0.0);
            t.extend(sample_box(10.0, 10.0, 10.0, 5.0, 5.0, 10.0));
            t
        }
        SampleShape::Cylinder => {
            let n = 48usize;
            let r = 10.0;
            let h = 20.0;
            let mut t = Vec::with_capacity(n * 4);
            let cb = Point3::new(0.0, 0.0, 0.0);
            let ct = Point3::new(0.0, 0.0, h);
            for i in 0..n {
                let a0 = TAU * (i as f64) / (n as f64);
                let a1 = TAU * (((i + 1) % n) as f64) / (n as f64);
                let b0 = Point3::new(r * a0.cos(), r * a0.sin(), 0.0);
                let b1 = Point3::new(r * a1.cos(), r * a1.sin(), 0.0);
                let t0 = Point3::new(r * a0.cos(), r * a0.sin(), h);
                let t1 = Point3::new(r * a1.cos(), r * a1.sin(), h);
                t.push(Triangle::new(cb, b1, b0));
                t.push(Triangle::new(ct, t0, t1));
                push_sample_quad(&mut t, b0, b1, t1, t0);
            }
            t
        }
        SampleShape::HollowTube => {
            let n = 48usize;
            let orad = 15.0;
            let irad = 8.0;
            let h = 10.0;
            let mut t = Vec::with_capacity(n * 8);
            for i in 0..n {
                let a0 = TAU * (i as f64) / (n as f64);
                let a1 = TAU * (((i + 1) % n) as f64) / (n as f64);
                let ob0 = Point3::new(orad * a0.cos(), orad * a0.sin(), 0.0);
                let ob1 = Point3::new(orad * a1.cos(), orad * a1.sin(), 0.0);
                let ot0 = Point3::new(orad * a0.cos(), orad * a0.sin(), h);
                let ot1 = Point3::new(orad * a1.cos(), orad * a1.sin(), h);
                let ib0 = Point3::new(irad * a0.cos(), irad * a0.sin(), 0.0);
                let ib1 = Point3::new(irad * a1.cos(), irad * a1.sin(), 0.0);
                let it0 = Point3::new(irad * a0.cos(), irad * a0.sin(), h);
                let it1 = Point3::new(irad * a1.cos(), irad * a1.sin(), h);
                push_sample_quad(&mut t, ob0, ob1, ot1, ot0);
                push_sample_quad(&mut t, ib1, ib0, it0, it1);
                push_sample_quad(&mut t, ib0, ib1, ob1, ob0);
                push_sample_quad(&mut t, ot0, ot1, it1, it0);
            }
            t
        }
    };
    Ok(TriangleMesh::new(tris)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_cube(size: f64) -> TriangleMesh {
        let s = size;
        let tris = [
            // bottom z=0
            ((0.0, 0.0, 0.0), (0.0, s, 0.0), (s, s, 0.0)),
            ((0.0, 0.0, 0.0), (s, s, 0.0), (s, 0.0, 0.0)),
            // top z=s
            ((0.0, 0.0, s), (s, 0.0, s), (s, s, s)),
            ((0.0, 0.0, s), (s, s, s), (0.0, s, s)),
            // front y=0
            ((0.0, 0.0, 0.0), (s, 0.0, 0.0), (s, 0.0, s)),
            ((0.0, 0.0, 0.0), (s, 0.0, s), (0.0, 0.0, s)),
            // back y=s
            ((0.0, s, 0.0), (0.0, s, s), (s, s, s)),
            ((0.0, s, 0.0), (s, s, s), (s, s, 0.0)),
            // left x=0
            ((0.0, 0.0, 0.0), (0.0, 0.0, s), (0.0, s, s)),
            ((0.0, 0.0, 0.0), (0.0, s, s), (0.0, s, 0.0)),
            // right x=s
            ((s, 0.0, 0.0), (s, s, 0.0), (s, s, s)),
            ((s, 0.0, 0.0), (s, s, s), (s, 0.0, s)),
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
    fn cube_slices_cleanly_to_exact_layers_and_area() {
        let cube = prepare_for_plate(&unit_cube(20.0), 250.0, 250.0).unwrap();
        let insp = inspect_mesh(&cube);
        assert_eq!(insp.non_manifold_edges, 0);
        assert!((insp.volume_mm3 - 8000.0).abs() < 1e-4);

        let layers = slice_mesh(&cube, 0.2, 0.2).unwrap();
        assert_eq!(layers.len(), 100);
        for layer in &layers {
            assert_eq!(layer.regions.len(), 1);
            assert!((layer.regions[0].area_mm2() - 400.0).abs() < 1e-4);
            assert!(layer.regions[0].outer.is_ccw());
        }
    }
}
