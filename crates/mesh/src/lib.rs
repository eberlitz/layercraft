//! Triangle mesh inspection, bed placement, and exact Z-plane slicing.

mod repair;

use std::collections::HashMap;
use std::f64::consts::{FRAC_PI_2, TAU};

use layercraft_clip::{ClipError, simplify, union_polygons};
use layercraft_geom::{
    Aabb3, ExPolygon, GeomError, Point2, Point3, Polygon, Triangle, TriangleMesh, mm_to_nm,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use repair::{MeshRepairConfig, MeshRepairReport, RepairedMesh, repair_mesh};

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
    /// Invalid mesh repair configuration.
    #[error("invalid mesh repair configuration: {0}")]
    InvalidRepairConfig(&'static str),
}

/// Summary properties computed from a triangle mesh.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MeshInspection {
    pub triangle_count: usize,
    pub aabb: Option<Aabb3>,
    pub volume_mm3: f64,
    pub surface_area_mm2: f64,
    pub degenerate_triangles: usize,
    /// Edges not shared by exactly two triangles (boundary + over-shared edges).
    pub non_manifold_edges: usize,
    /// Open edges used by exactly one triangle.
    #[serde(default)]
    pub boundary_edges: usize,
    /// Triangles that repeat another triangle's vertex set (either winding).
    #[serde(default)]
    pub duplicate_triangles: usize,
    /// Manifold edges whose two triangles traverse it in the same direction.
    #[serde(default)]
    pub inconsistent_winding_edges: usize,
    /// Closed, consistently wound shells with negative (inward) volume that are not enclosed cavities.
    #[serde(default)]
    pub inverted_shells: usize,
    /// `true` when any topology defect suggests running mesh repair.
    #[serde(default)]
    pub needs_repair: bool,
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
    let mut vert_map: HashMap<QuantPt3, usize> = HashMap::new();
    let mut ix = repair::Indexed::default();

    for tri in &mesh.triangles {
        if !tri.is_finite() || tri.area_mm2() <= 1e-12 {
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
            let mut intern = |q: QuantPt3, p: Point3| -> usize {
                if let Some(&idx) = vert_map.get(&q) {
                    idx
                } else {
                    let idx = ix.verts.len();
                    ix.verts.push([p.x, p.y, p.z]);
                    vert_map.insert(q, idx);
                    idx
                }
            };
            let ia = intern(qa, tri.a);
            let ib = intern(qb, tri.b);
            let ic = intern(qc, tri.c);
            if ia != ib && ib != ic && ic != ia {
                ix.tris.push([ia, ib, ic]);
            }
        }
    }

    let non_manifold_edges = edge_counts.values().filter(|&&c| c != 2).count();
    let boundary_edges = edge_counts.values().filter(|&&c| c == 1).count();
    let topo = repair::topology_stats(&ix);
    let needs_repair = degenerate_triangles > 0
        || non_manifold_edges > 0
        || topo.duplicate_triangles > 0
        || topo.inconsistent_winding_edges > 0
        || topo.inverted_shells > 0;

    MeshInspection {
        triangle_count: mesh.triangles.len(),
        aabb: mesh.aabb(),
        volume_mm3: mesh.signed_volume_mm3(),
        surface_area_mm2: mesh.surface_area_mm2(),
        degenerate_triangles,
        non_manifold_edges,
        boundary_edges,
        duplicate_triangles: topo.duplicate_triangles,
        inconsistent_winding_edges: topo.inconsistent_winding_edges,
        inverted_shells: topo.inverted_shells,
        needs_repair,
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

/// Placement transform for a single object on the build plate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ObjectTransform {
    /// Desired XY center position on the build plate in millimeters.
    pub position_xy_mm: [f64; 2],
    /// Euler XYZ rotation in degrees.
    pub rotation_deg: [f64; 3],
    /// Positive XYZ scale factors.
    pub scale: [f64; 3],
}

impl Default for ObjectTransform {
    fn default() -> Self {
        Self {
            position_xy_mm: [125.0, 125.0],
            rotation_deg: [0.0, 0.0, 0.0],
            scale: [1.0, 1.0, 1.0],
        }
    }
}

/// Clean degenerate triangles and apply scale -> rotation -> ground & XY placement.
pub fn prepare_transformed_object(
    mesh: &TriangleMesh,
    transform: ObjectTransform,
) -> Result<TriangleMesh, MeshError> {
    if !transform.position_xy_mm.iter().all(|v| v.is_finite())
        || !transform.rotation_deg.iter().all(|v| v.is_finite())
        || !transform.scale.iter().all(|v| v.is_finite() && *v > 0.0)
    {
        return Err(MeshError::InvalidSchedule(
            "object transform must be finite with positive scale",
        ));
    }
    let valid: Vec<Triangle> = mesh
        .triangles
        .iter()
        .copied()
        .filter(|t| t.is_finite() && t.area_mm2() > 1e-12)
        .collect();
    let mut clean = TriangleMesh::new(valid)?;
    clean.scale(transform.scale[0], transform.scale[1], transform.scale[2])?;
    clean.rotate_euler_deg(
        transform.rotation_deg[0],
        transform.rotation_deg[1],
        transform.rotation_deg[2],
    )?;
    clean.ground_and_center(transform.position_xy_mm[0], transform.position_xy_mm[1])?;
    Ok(clean)
}

/// Pack axis-aligned 2D footprints `(width_mm, depth_mm)` onto the build plate using FFDH shelf packing,
/// centered as a group on `(bed_width / 2, bed_depth / 2)`.
pub fn auto_arrange_footprints(
    sizes_xy: &[(f64, f64)],
    bed_width_mm: f64,
    bed_depth_mm: f64,
    spacing_mm: f64,
) -> Result<Vec<[f64; 2]>, MeshError> {
    if !bed_width_mm.is_finite()
        || !bed_depth_mm.is_finite()
        || bed_width_mm <= 4.0
        || bed_depth_mm <= 4.0
    {
        return Err(MeshError::InvalidSchedule(
            "bed dimensions must be finite and > 4 mm",
        ));
    }
    if !spacing_mm.is_finite() || spacing_mm < 0.0 {
        return Err(MeshError::InvalidSchedule(
            "spacing must be finite and >= 0",
        ));
    }
    if sizes_xy.is_empty() {
        return Ok(Vec::new());
    }
    for &(w, d) in sizes_xy {
        if !w.is_finite() || !d.is_finite() || w <= 0.0 || d <= 0.0 {
            return Err(MeshError::InvalidSchedule(
                "object footprint dimensions must be finite and > 0",
            ));
        }
    }

    let usable_w = bed_width_mm - 4.0;
    let mut order: Vec<usize> = (0..sizes_xy.len()).collect();
    order.sort_by(|&a, &b| {
        let (wa, da) = sizes_xy.get(a).copied().unwrap_or((0.0, 0.0));
        let (wb, db) = sizes_xy.get(b).copied().unwrap_or((0.0, 0.0));
        db.total_cmp(&da)
            .then_with(|| wb.total_cmp(&wa))
            .then_with(|| a.cmp(&b))
    });

    let mut placed = vec![[0.0, 0.0]; sizes_xy.len()];
    let mut cur_x = 0.0_f64;
    let mut cur_y = 0.0_f64;
    let mut shelf_h = 0.0_f64;
    let mut used_w = 0.0_f64;

    for idx in order {
        let (w, d) = sizes_xy.get(idx).copied().unwrap_or((0.0, 0.0));
        if cur_x > 0.0 && cur_x + w > usable_w {
            cur_y += shelf_h + spacing_mm;
            cur_x = 0.0;
            shelf_h = 0.0;
        }
        let cx = cur_x + 0.5 * w;
        let cy = cur_y + 0.5 * d;
        if let Some(slot) = placed.get_mut(idx) {
            *slot = [cx, cy];
        }
        used_w = used_w.max(cur_x + w);
        cur_x += w + spacing_mm;
        shelf_h = shelf_h.max(d);
    }

    let used_d = cur_y + shelf_h;
    if used_w > bed_width_mm - 2.0 || used_d > bed_depth_mm - 2.0 {
        return Err(MeshError::InvalidSchedule(
            "objects do not fit on build plate",
        ));
    }

    let shift_x = 0.5 * (bed_width_mm - used_w);
    let shift_y = 0.5 * (bed_depth_mm - used_d);
    for pos in &mut placed {
        pos[0] += shift_x;
        pos[1] += shift_y;
    }

    Ok(placed)
}

/// Transform for positioning a 3D modifier volume on the build plate without Z grounding.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ModifierTransform {
    /// Desired world-space XYZ center position in millimeters.
    pub position_xyz_mm: [f64; 3],
    /// Euler XYZ rotation in degrees.
    pub rotation_deg: [f64; 3],
    /// Positive XYZ scale factors.
    pub scale: [f64; 3],
}

impl Default for ModifierTransform {
    fn default() -> Self {
        Self {
            position_xyz_mm: [125.0, 125.0, 10.0],
            rotation_deg: [0.0, 0.0, 0.0],
            scale: [1.0, 1.0, 1.0],
        }
    }
}

fn default_modifier_segments() -> usize {
    32
}

/// Built-in 3D modifier primitive geometry centered around its local bounding box center.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModifierPrimitive {
    Box {
        width_mm: f64,
        depth_mm: f64,
        height_mm: f64,
    },
    Cylinder {
        radius_mm: f64,
        height_mm: f64,
        #[serde(default = "default_modifier_segments")]
        segments: usize,
    },
}

/// Construct a centered watertight primitive mesh for a modifier volume.
pub fn build_modifier_primitive(prim: ModifierPrimitive) -> Result<TriangleMesh, MeshError> {
    let tris = match prim {
        ModifierPrimitive::Box {
            width_mm,
            depth_mm,
            height_mm,
        } => {
            if !width_mm.is_finite()
                || !depth_mm.is_finite()
                || !height_mm.is_finite()
                || width_mm <= 0.0
                || depth_mm <= 0.0
                || height_mm <= 0.0
            {
                return Err(MeshError::InvalidSchedule(
                    "modifier box dimensions must be finite and positive",
                ));
            }
            sample_box(
                width_mm,
                depth_mm,
                height_mm,
                -0.5 * width_mm,
                -0.5 * depth_mm,
                -0.5 * height_mm,
            )
        }
        ModifierPrimitive::Cylinder {
            radius_mm,
            height_mm,
            segments,
        } => {
            if !radius_mm.is_finite()
                || !height_mm.is_finite()
                || radius_mm <= 0.0
                || height_mm <= 0.0
            {
                return Err(MeshError::InvalidSchedule(
                    "modifier cylinder dimensions must be finite and positive",
                ));
            }
            let n = segments.max(8);
            let half_h = 0.5 * height_mm;
            let mut t = Vec::with_capacity(n * 4);
            let cb = Point3::new(0.0, 0.0, -half_h);
            let ct = Point3::new(0.0, 0.0, half_h);
            for i in 0..n {
                let a0 = TAU * (i as f64) / (n as f64);
                let a1 = TAU * (((i + 1) % n) as f64) / (n as f64);
                let b0 = Point3::new(radius_mm * a0.cos(), radius_mm * a0.sin(), -half_h);
                let b1 = Point3::new(radius_mm * a1.cos(), radius_mm * a1.sin(), -half_h);
                let t0 = Point3::new(radius_mm * a0.cos(), radius_mm * a0.sin(), half_h);
                let t1 = Point3::new(radius_mm * a1.cos(), radius_mm * a1.sin(), half_h);
                t.push(Triangle::new(cb, b1, b0));
                t.push(Triangle::new(ct, t0, t1));
                push_sample_quad(&mut t, b0, b1, t1, t0);
            }
            t
        }
    };
    Ok(TriangleMesh::new(tris)?)
}

/// Clean degenerate triangles and apply scale -> rotation -> XYZ center placement (no Z grounding).
pub fn prepare_modifier_mesh(
    mesh: &TriangleMesh,
    transform: ModifierTransform,
) -> Result<TriangleMesh, MeshError> {
    if !transform.position_xyz_mm.iter().all(|v| v.is_finite())
        || !transform.rotation_deg.iter().all(|v| v.is_finite())
        || !transform.scale.iter().all(|v| v.is_finite() && *v > 0.0)
    {
        return Err(MeshError::InvalidSchedule(
            "modifier transform must be finite with positive scale",
        ));
    }
    let valid: Vec<Triangle> = mesh
        .triangles
        .iter()
        .copied()
        .filter(|t| t.is_finite() && t.area_mm2() > 1e-12)
        .collect();
    let mut clean = TriangleMesh::new(valid)?;
    clean.scale(transform.scale[0], transform.scale[1], transform.scale[2])?;
    clean.rotate_euler_deg(
        transform.rotation_deg[0],
        transform.rotation_deg[1],
        transform.rotation_deg[2],
    )?;
    let c = clean.aabb().ok_or(GeomError::EmptyMesh)?.center();
    clean.translate(
        transform.position_xyz_mm[0] - c.x,
        transform.position_xyz_mm[1] - c.y,
        transform.position_xyz_mm[2] - c.z,
    )?;
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

/// Explicit Z-height band forcing a target layer height over `[min_z_mm, max_z_mm]`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExplicitHeightRange {
    pub min_z_mm: f64,
    pub max_z_mm: f64,
    pub layer_height_mm: f64,
}

/// Parameters controlling adaptive and/or Z-range variable layer scheduling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VariableScheduleParams {
    pub first_layer_height_mm: f64,
    pub nominal_layer_height_mm: f64,
    pub adaptive: bool,
    pub min_layer_height_mm: f64,
    pub max_layer_height_mm: f64,
    pub quality: f64,
    pub smooth_radius_mm: f64,
    pub max_step_ratio: f64,
    pub explicit_ranges: Vec<ExplicitHeightRange>,
}

/// Build an adaptive + Z-range variable layer schedule over `[0, max_z]` of `mesh`.
pub fn build_variable_layer_schedule(
    mesh: &TriangleMesh,
    params: &VariableScheduleParams,
) -> Result<Vec<LayerPlane>, MeshError> {
    if !params.first_layer_height_mm.is_finite() || params.first_layer_height_mm <= 0.0 {
        return Err(MeshError::InvalidSchedule(
            "first_layer_height must be positive",
        ));
    }
    if !params.nominal_layer_height_mm.is_finite() || params.nominal_layer_height_mm <= 0.0 {
        return Err(MeshError::InvalidSchedule(
            "nominal_layer_height must be positive",
        ));
    }
    if !params.min_layer_height_mm.is_finite()
        || !params.max_layer_height_mm.is_finite()
        || params.min_layer_height_mm <= 0.0
        || params.min_layer_height_mm > params.max_layer_height_mm
    {
        return Err(MeshError::InvalidSchedule(
            "min_layer_height must be positive and <= max_layer_height",
        ));
    }
    if !params.quality.is_finite() || !(0.0..=1.0).contains(&params.quality) {
        return Err(MeshError::InvalidSchedule(
            "quality must be within 0.0..=1.0",
        ));
    }
    if !params.smooth_radius_mm.is_finite() || params.smooth_radius_mm < 0.0 {
        return Err(MeshError::InvalidSchedule(
            "smooth_radius_mm must be finite and >= 0",
        ));
    }
    if !params.max_step_ratio.is_finite() || params.max_step_ratio < 1.0 {
        return Err(MeshError::InvalidSchedule(
            "max_step_ratio must be finite and >= 1.0",
        ));
    }
    for r in &params.explicit_ranges {
        if !r.min_z_mm.is_finite()
            || r.min_z_mm < 0.0
            || !r.max_z_mm.is_finite()
            || r.max_z_mm <= r.min_z_mm
            || !r.layer_height_mm.is_finite()
            || r.layer_height_mm <= 0.0
        {
            return Err(MeshError::InvalidSchedule(
                "explicit height range bounds and height must be valid",
            ));
        }
    }

    let aabb = mesh.aabb().ok_or(GeomError::EmptyMesh)?;
    let max_z_mm = aabb.max.z - aabb.min.z;
    if !max_z_mm.is_finite() || max_z_mm <= 0.0 {
        return Err(MeshError::InvalidSchedule("max_z must be positive"));
    }

    if !params.adaptive && params.explicit_ranges.is_empty() {
        return build_layer_schedule(
            max_z_mm,
            params.first_layer_height_mm,
            params.nominal_layer_height_mm,
        );
    }

    let min_h = params.min_layer_height_mm;
    let max_h = params.max_layer_height_mm;
    let mut dz = (0.5 * min_h).clamp(0.02, 0.05);
    let mut bins = (max_z_mm / dz).ceil().max(1.0) as usize;
    if bins > 16_384 {
        bins = 16_384;
    }
    dz = max_z_mm / (bins as f64);

    let default_target = if params.adaptive {
        max_h
    } else {
        params.nominal_layer_height_mm
    };
    let mut bin_target = vec![default_target; bins];

    if params.adaptive {
        let delta_min = 0.35 * min_h;
        let delta_max = 1.6 * max_h;
        let delta = delta_min + (1.0 - params.quality).powf(1.5) * (delta_max - delta_min);

        for tri in &mesh.triangles {
            let da = tri.double_area_vec();
            let len = da.length();
            if 0.5 * len <= 1e-12 {
                continue;
            }
            let abs_nz = (da.z / len).abs().clamp(0.0, 1.0);
            let z0 = tri.a.z.min(tri.b.z).min(tri.c.z) - aabb.min.z;
            let z1 = tri.a.z.max(tri.b.z).max(tri.c.z) - aabb.min.z;
            if abs_nz < 1e-3 || abs_nz > 0.9995 || (z1 - z0) < 1e-4 {
                continue;
            }
            let horiz = (1.0 - abs_nz * abs_nz).max(1e-12).sqrt();
            let tan_theta = abs_nz / horiz;
            let h_tri = (delta / tan_theta).clamp(min_h, max_h);
            let b0 = ((z0 / dz).floor() as isize).clamp(0, (bins as isize) - 1) as usize;
            let b1 = ((z1 / dz).floor() as isize).clamp(0, (bins as isize) - 1) as usize;
            for slot in bin_target.iter_mut().take(b1 + 1).skip(b0) {
                *slot = (*slot).min(h_tri);
            }
        }

        if params.smooth_radius_mm > 1e-4 {
            let sigma = (0.5 * params.smooth_radius_mm).max(1e-4);
            let half_w = (params.smooth_radius_mm / dz).ceil() as isize;
            let mut weights = Vec::with_capacity((2 * half_w + 1).max(1) as usize);
            let mut w_sum = 0.0;
            for k in -half_w..=half_w {
                let d = (k as f64) * dz;
                let w = (-0.5 * (d / sigma) * (d / sigma)).exp();
                weights.push((k, w));
                w_sum += w;
            }
            if w_sum > 1e-12 {
                let mut smoothed = Vec::with_capacity(bins);
                let max_bi = (bins as isize) - 1;
                for i in 0..bins {
                    let mut acc = 0.0;
                    for &(k, w) in &weights {
                        let bi = (i as isize + k).clamp(0, max_bi) as usize;
                        acc += w * bin_target.get(bi).copied().unwrap_or(default_target);
                    }
                    smoothed.push((acc / w_sum).clamp(min_h, max_h));
                }
                bin_target = smoothed;
            }
        }
    }

    let mut bin_explicit = vec![false; bins];
    for r in &params.explicit_ranges {
        for (b, slot) in bin_target.iter_mut().enumerate() {
            let zc = ((b as f64) + 0.5) * dz;
            if zc >= r.min_z_mm - 1e-9 && zc <= r.max_z_mm + 1e-9 {
                *slot = r.layer_height_mm;
                if let Some(flag) = bin_explicit.get_mut(b) {
                    *flag = true;
                }
            }
        }
    }

    let sample_bin = |z_mm: f64| -> (f64, bool) {
        let bi = ((z_mm / dz).floor() as isize).clamp(0, (bins as isize) - 1) as usize;
        let h = bin_target.get(bi).copied().unwrap_or(default_target);
        let is_exp = bin_explicit.get(bi).copied().unwrap_or(false);
        (h, is_exp)
    };

    let mut schedule = Vec::new();
    let eps = 1e-4;
    let mut z_prev = 0.0;
    let mut prev_h = params.first_layer_height_mm;

    if max_z_mm > eps {
        let z_top = params.first_layer_height_mm.min(max_z_mm);
        schedule.push(LayerPlane {
            index: 0,
            z_top_mm: z_top,
            z_slice_mm: 0.5 * z_top,
            height_mm: z_top,
        });
        z_prev = z_top;
        prev_h = z_top;
    }

    let mut idx = 1usize;
    let base_floor = min_h.min(params.nominal_layer_height_mm);
    while z_prev + eps < max_z_mm {
        let (h0, exp0) = sample_bin(z_prev);
        let (h1, exp1) = sample_bin(z_prev + 0.5 * h0);
        let is_exp = exp1 || (exp0 && (h1 - h0).abs() < 1e-9);
        let mut h_cand = if exp1 {
            h1
        } else if exp0 {
            h0
        } else {
            h1
        };
        if params.adaptive && !is_exp {
            let step_lo = prev_h / params.max_step_ratio;
            let step_hi = prev_h * params.max_step_ratio;
            h_cand = h_cand.clamp(step_lo, step_hi).clamp(min_h, max_h);
        }

        let rem = max_z_mm - z_prev;
        let floor_h = base_floor.min(h_cand);
        let (h_step, z_top) = if rem <= h_cand + eps {
            if rem >= 0.5 * floor_h {
                (rem, max_z_mm)
            } else {
                break;
            }
        } else if rem < h_cand + floor_h {
            let half = 0.5 * rem;
            (half, z_prev + half)
        } else {
            (h_cand, z_prev + h_cand)
        };

        let z_slice = z_prev + 0.5 * h_step;
        if z_slice >= max_z_mm {
            break;
        }
        schedule.push(LayerPlane {
            index: idx,
            z_top_mm: z_top,
            z_slice_mm: z_slice,
            height_mm: h_step,
        });
        z_prev = z_top;
        prev_h = h_step;
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
    slice_mesh_with_schedule(mesh, &schedule)
}

/// Slice a mesh along a caller-supplied layer schedule (preserving variable per-layer heights).
pub fn slice_mesh_with_schedule(
    mesh: &TriangleMesh,
    schedule: &[LayerPlane],
) -> Result<Vec<RawSliceLayer>, MeshError> {
    let z_planes: Vec<f64> = schedule.iter().map(|p| p.z_slice_mm).collect();
    let slices = slice_mesh_at_z_planes(mesh, &z_planes)?;
    Ok(schedule
        .iter()
        .copied()
        .zip(slices)
        .map(|(plane, regions)| RawSliceLayer { plane, regions })
        .collect())
}

const CONTOUR_SIMPLIFY_EPS_NM: i64 = 5_000;
const CONTOUR_SIMPLIFY_MIN_VERTICES: usize = 64;

fn simplify_ring(poly: Polygon, eps_nm: i64) -> Polygon {
    if poly.points.len() <= CONTOUR_SIMPLIFY_MIN_VERTICES {
        return poly;
    }
    let simplified = simplify(&poly, eps_nm);
    if simplified.points.len() >= 3 && simplified.area_mm2() > 0.0 {
        simplified
    } else {
        poly
    }
}

fn simplify_regions(regions: Vec<ExPolygon>, eps_nm: i64) -> Vec<ExPolygon> {
    regions
        .into_iter()
        .filter_map(|ex| {
            let outer = simplify_ring(ex.outer, eps_nm);
            let holes: Vec<Polygon> = ex
                .holes
                .into_iter()
                .map(|h| simplify_ring(h, eps_nm))
                .collect();
            let merged = ExPolygon::new(outer, holes);
            (merged.outer.points.len() >= 3 && merged.area_mm2() > 0.0).then_some(merged)
        })
        .collect()
}

fn parallel_try_map<T, R, E, F>(items: Vec<T>, f: &F) -> Result<Vec<R>, E>
where
    T: Send,
    R: Send,
    E: Send,
    F: Fn(T) -> Result<R, E> + Sync,
{
    #[cfg(not(target_arch = "wasm32"))]
    {
        let n = items.len();
        let workers = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1)
            .clamp(1, 16)
            .min(n);
        if workers > 1 && n >= 4 {
            let next_idx = std::sync::atomic::AtomicUsize::new(0);
            let slots: Vec<std::sync::Mutex<Option<T>>> = items
                .into_iter()
                .map(|v| std::sync::Mutex::new(Some(v)))
                .collect();
            let results: Vec<std::sync::Mutex<Option<Result<R, E>>>> =
                (0..n).map(|_| std::sync::Mutex::new(None)).collect();
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    let next_ref = &next_idx;
                    let slots_ref = &slots;
                    let results_ref = &results;
                    scope.spawn(move || {
                        loop {
                            let idx = next_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if idx >= n {
                                break;
                            }
                            let item = slots_ref
                                .get(idx)
                                .and_then(|m| m.lock().ok().and_then(|mut g| g.take()));
                            let Some(val) = item else {
                                continue;
                            };
                            let res = f(val);
                            if let Some(rm) = results_ref.get(idx)
                                && let Ok(mut g) = rm.lock()
                            {
                                *g = Some(res);
                            }
                        }
                    });
                }
            });
            let mut out = Vec::with_capacity(n);
            for rm in results {
                if let Ok(mut g) = rm.into_inner()
                    && let Some(r) = g.take()
                {
                    out.push(r?);
                }
            }
            return Ok(out);
        }
    }
    items.into_iter().map(f).collect()
}

/// Intersect a mesh with an arbitrary list of Z planes, returning `Vec<ExPolygon>` per plane.
pub fn slice_mesh_at_z_planes(
    mesh: &TriangleMesh,
    z_planes_mm: &[f64],
) -> Result<Vec<Vec<ExPolygon>>, MeshError> {
    let aabb = mesh.aabb().ok_or(GeomError::EmptyMesh)?;
    if z_planes_mm.is_empty() {
        return Ok(Vec::new());
    }

    let is_monotonic = z_planes_mm.iter().all(|z| z.is_finite())
        && z_planes_mm
            .windows(2)
            .all(|w| w.first().zip(w.get(1)).is_some_and(|(a, b)| a <= b));

    if is_monotonic {
        let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); z_planes_mm.len()];
        for (tri_idx, tri) in mesh.triangles.iter().enumerate() {
            let zmin = tri.a.z.min(tri.b.z).min(tri.c.z);
            let zmax = tri.a.z.max(tri.b.z).max(tri.c.z);
            let lo = z_planes_mm.partition_point(|&z| z < zmin);
            let hi = z_planes_mm.partition_point(|&z| z < zmax);
            for slot in buckets.get_mut(lo..hi).into_iter().flatten() {
                slot.push(tri_idx);
            }
        }
        let tasks: Vec<(f64, Vec<usize>)> = z_planes_mm.iter().copied().zip(buckets).collect();
        return parallel_try_map(tasks, &|(z, tri_indices)| {
            if z < aabb.min.z || z > aabb.max.z || tri_indices.is_empty() {
                return Ok(Vec::new());
            }
            let mut segments: Vec<(Point2, Point2)> = Vec::with_capacity(tri_indices.len());
            for tri_idx in tri_indices {
                let Some(tri) = mesh.triangles.get(tri_idx) else {
                    continue;
                };
                if let Some(seg) = intersect_triangle_at_z(tri, z)?
                    && seg.0 != seg.1
                {
                    segments.push(seg);
                }
            }
            let loops = stitch_segments_into_loops(&segments);
            let regions = union_polygons(&loops)?;
            Ok(simplify_regions(regions, CONTOUR_SIMPLIFY_EPS_NM))
        });
    }

    // Fallback when caller provides non-sorted Z planes.
    let tri_z: Vec<(f64, f64, &Triangle)> = mesh
        .triangles
        .iter()
        .map(|t| {
            let zmin = t.a.z.min(t.b.z).min(t.c.z);
            let zmax = t.a.z.max(t.b.z).max(t.c.z);
            (zmin, zmax, t)
        })
        .collect();

    let tasks: Vec<f64> = z_planes_mm.to_vec();
    parallel_try_map(tasks, &|z| {
        if !z.is_finite() || z < aabb.min.z || z > aabb.max.z {
            return Ok(Vec::new());
        }
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
        Ok(simplify_regions(regions, CONTOUR_SIMPLIFY_EPS_NM))
    })
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
    let mut open_chains: Vec<Vec<Point2>> = Vec::new();
    let max_snap_10_sq: i128 = 10_000i128 * 10_000i128;

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
                let mut best: Option<(usize, i128)> = None;
                for (idx, &(cand_start, _)) in segments.iter().enumerate() {
                    if used.get(idx).copied().unwrap_or(true) {
                        continue;
                    }
                    let d2 = cur_end.distance_sq_nm(cand_start);
                    if d2 <= max_snap_10_sq && best.is_none_or(|(_, bd)| d2 < bd) {
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
            && (cur_end == first_pt || cur_end.distance_sq_nm(first_pt) <= max_snap_10_sq)
        {
            loops.push(Polygon::new(pts));
        } else if pts.len() >= 2 {
            open_chains.push(pts);
        }
    }

    // Secondary 50 um open-chain fallback (including reversed chains) for slightly cracked/flipped meshes.
    if !open_chains.is_empty() {
        let max_snap_50_sq: i128 = 50_000i128 * 50_000i128;
        let mut chain_used = vec![false; open_chains.len()];
        for ci in 0..open_chains.len() {
            if chain_used.get(ci).copied().unwrap_or(true) {
                continue;
            }
            if let Some(u) = chain_used.get_mut(ci) {
                *u = true;
            }
            let Some(mut chain) = open_chains.get(ci).cloned() else {
                continue;
            };
            let mut flipped = false;
            let mut guard = 0usize;
            while guard <= open_chains.len().saturating_mul(2).saturating_add(2) {
                guard += 1;
                let (Some(&first_pt), Some(&cur_end)) = (chain.first(), chain.last()) else {
                    break;
                };
                if cur_end == first_pt && chain.len() >= 4 {
                    break;
                }
                let close_d2 = cur_end.distance_sq_nm(first_pt);
                let mut best: Option<(usize, bool, i128)> = None;
                for (oj, cand) in open_chains.iter().enumerate() {
                    if chain_used.get(oj).copied().unwrap_or(true) {
                        continue;
                    }
                    let (Some(&cf), Some(&cl)) = (cand.first(), cand.last()) else {
                        continue;
                    };
                    let d2_fwd = cur_end.distance_sq_nm(cf);
                    if d2_fwd <= max_snap_50_sq && best.is_none_or(|(_, _, bd)| d2_fwd < bd) {
                        best = Some((oj, false, d2_fwd));
                    }
                    let d2_rev = cur_end.distance_sq_nm(cl);
                    if d2_rev <= max_snap_50_sq && best.is_none_or(|(_, _, bd)| d2_rev < bd) {
                        best = Some((oj, true, d2_rev));
                    }
                }
                let Some((oj, rev, cand_d2)) = best else {
                    if !flipped {
                        chain.reverse();
                        flipped = true;
                        continue;
                    }
                    break;
                };
                if chain.len() >= 3 && close_d2 <= max_snap_50_sq && close_d2 <= cand_d2 {
                    break;
                }
                if let Some(u) = chain_used.get_mut(oj) {
                    *u = true;
                }
                let Some(cand) = open_chains.get(oj) else {
                    break;
                };
                flipped = false;
                if rev {
                    for &pt in cand.iter().rev() {
                        if chain.last().copied() != Some(pt) {
                            chain.push(pt);
                        }
                    }
                } else {
                    for &pt in cand {
                        if chain.last().copied() != Some(pt) {
                            chain.push(pt);
                        }
                    }
                }
            }
            if let (Some(&first_pt), Some(&last_pt)) = (chain.first(), chain.last()) {
                let closed =
                    last_pt == first_pt || last_pt.distance_sq_nm(first_pt) <= max_snap_50_sq;
                if last_pt == first_pt {
                    chain.pop();
                }
                if closed && chain.len() >= 3 {
                    loops.push(Polygon::new(chain));
                }
            }
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
    BridgeArch,
    Dome,
    TaperedWedge,
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
            Self::BridgeArch => "Bridge Arch",
            Self::Dome => "Dome R10 H18",
            Self::TaperedWedge => "Tapered Wedge",
        }
    }

    /// All built-in sample variants.
    pub const ALL: [Self; 8] = [
        Self::Cube20,
        Self::Cylinder,
        Self::HollowTube,
        Self::SteppedBlock,
        Self::ThinRib,
        Self::BridgeArch,
        Self::Dome,
        Self::TaperedWedge,
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
        SampleShape::BridgeArch => {
            let mut t = sample_box(6.0, 10.0, 8.0, 0.0, 0.0, 0.0);
            t.extend(sample_box(6.0, 10.0, 8.0, 24.0, 0.0, 0.0));
            t.extend(sample_box(30.0, 10.0, 3.0, 0.0, 0.0, 8.0));
            t
        }
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
        SampleShape::Dome => {
            let n = 48usize;
            let rings = 16usize;
            let r = 10.0;
            let base_h = 8.0;
            let mut t = Vec::with_capacity(n * (rings * 2 + 3));
            let cb = Point3::new(0.0, 0.0, 0.0);
            let mut ring_pts: Vec<Vec<Point3>> = Vec::with_capacity(rings);
            for ri in 0..rings {
                let phi = FRAC_PI_2 * (ri as f64) / (rings as f64);
                let rr = r * phi.cos();
                let zz = base_h + r * phi.sin();
                let mut pts = Vec::with_capacity(n);
                for i in 0..n {
                    let a = TAU * (i as f64) / (n as f64);
                    pts.push(Point3::new(rr * a.cos(), rr * a.sin(), zz));
                }
                ring_pts.push(pts);
            }
            if let Some(base_top) = ring_pts.first() {
                for i in 0..n {
                    let i1 = (i + 1) % n;
                    let a0 = TAU * (i as f64) / (n as f64);
                    let a1 = TAU * (i1 as f64) / (n as f64);
                    let b0 = Point3::new(r * a0.cos(), r * a0.sin(), 0.0);
                    let b1 = Point3::new(r * a1.cos(), r * a1.sin(), 0.0);
                    t.push(Triangle::new(cb, b1, b0));
                    if let (Some(&t0), Some(&t1)) = (base_top.get(i), base_top.get(i1)) {
                        push_sample_quad(&mut t, b0, b1, t1, t0);
                    }
                }
            }
            for ri in 0..rings.saturating_sub(1) {
                let (Some(cur), Some(nxt)) = (ring_pts.get(ri), ring_pts.get(ri + 1)) else {
                    continue;
                };
                for i in 0..n {
                    let i1 = (i + 1) % n;
                    if let (Some(&p00), Some(&p10), Some(&p11), Some(&p01)) =
                        (cur.get(i), cur.get(i1), nxt.get(i1), nxt.get(i))
                    {
                        push_sample_quad(&mut t, p00, p10, p11, p01);
                    }
                }
            }
            let apex = Point3::new(0.0, 0.0, base_h + r);
            if let Some(last_ring) = ring_pts.last() {
                for i in 0..n {
                    let i1 = (i + 1) % n;
                    if let (Some(&p0), Some(&p1)) = (last_ring.get(i), last_ring.get(i1)) {
                        t.push(Triangle::new(p0, p1, apex));
                    }
                }
            }
            t
        }
        SampleShape::TaperedWedge => {
            let length = 36.0;
            let hw0 = 2.0;
            let hw1 = 0.125;
            let h = 8.0;
            let p000 = Point3::new(0.0, -hw0, 0.0);
            let p100 = Point3::new(length, -hw1, 0.0);
            let p110 = Point3::new(length, hw1, 0.0);
            let p010 = Point3::new(0.0, hw0, 0.0);
            let p001 = Point3::new(0.0, -hw0, h);
            let p101 = Point3::new(length, -hw1, h);
            let p111 = Point3::new(length, hw1, h);
            let p011 = Point3::new(0.0, hw0, h);

            let mut t = Vec::with_capacity(12);
            push_sample_quad(&mut t, p000, p010, p110, p100);
            push_sample_quad(&mut t, p001, p101, p111, p011);
            push_sample_quad(&mut t, p000, p100, p101, p001);
            push_sample_quad(&mut t, p010, p011, p111, p110);
            push_sample_quad(&mut t, p000, p001, p011, p010);
            push_sample_quad(&mut t, p100, p110, p111, p101);
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

    #[test]
    fn transformed_object_and_auto_arrange_pack_without_overlap() {
        let base = unit_cube(20.0);
        let xf = ObjectTransform {
            position_xy_mm: [80.0, 90.0],
            rotation_deg: [0.0, 0.0, 45.0],
            scale: [1.5, 1.0, 0.5],
        };
        let placed = prepare_transformed_object(&base, xf).unwrap();
        let aabb = placed.aabb().unwrap();
        assert!((aabb.min.z - 0.0).abs() < 1e-6);
        assert!((aabb.max.z - 10.0).abs() < 1e-6);
        assert!((aabb.center().x - 80.0).abs() < 1e-4);
        assert!((aabb.center().y - 90.0).abs() < 1e-4);

        let two =
            auto_arrange_footprints(&[(20.0, 20.0), (30.0, 15.0)], 250.0, 250.0, 8.0).unwrap();
        assert_eq!(two.len(), 2);
        let dx = (two[0][0] - two[1][0]).abs();
        assert!(dx >= 0.5 * 20.0 + 0.5 * 30.0 + 8.0 - 1e-6);

        let four_sizes = [(40.0, 50.0), (60.0, 30.0), (50.0, 40.0), (80.0, 45.0)];
        let four = auto_arrange_footprints(&four_sizes, 180.0, 180.0, 10.0).unwrap();
        for i in 0..four.len() {
            let (wi, di) = four_sizes[i];
            assert!(four[i][0] - 0.5 * wi >= 0.0 && four[i][0] + 0.5 * wi <= 180.0);
            assert!(four[i][1] - 0.5 * di >= 0.0 && four[i][1] + 0.5 * di <= 180.0);
            for j in (i + 1)..four.len() {
                let (wj, dj) = four_sizes[j];
                let gap_x = (four[i][0] - four[j][0]).abs() - 0.5 * (wi + wj);
                let gap_y = (four[i][1] - four[j][1]).abs() - 0.5 * (di + dj);
                assert!(
                    gap_x >= 10.0 - 1e-6 || gap_y >= 10.0 - 1e-6,
                    "footprints {i} and {j} overlap or violate spacing: gap_x={gap_x}, gap_y={gap_y}"
                );
            }
        }

        assert!(auto_arrange_footprints(&[(300.0, 20.0)], 250.0, 250.0, 6.0).is_err());
        assert!(
            prepare_transformed_object(
                &base,
                ObjectTransform {
                    scale: [0.0, 1.0, 1.0],
                    ..Default::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn adaptive_schedule_distinguishes_vertical_cube_and_curved_dome() {
        let cube = prepare_for_plate(&unit_cube(12.0), 250.0, 250.0).unwrap();
        let adapt_params = VariableScheduleParams {
            first_layer_height_mm: 0.2,
            nominal_layer_height_mm: 0.2,
            adaptive: true,
            min_layer_height_mm: 0.08,
            max_layer_height_mm: 0.28,
            quality: 0.6,
            smooth_radius_mm: 2.0,
            max_step_ratio: 1.5,
            explicit_ranges: Vec::new(),
        };
        let cube_sched = build_variable_layer_schedule(&cube, &adapt_params).unwrap();
        assert!((cube_sched.last().unwrap().z_top_mm - 12.0).abs() < 1e-6);
        let thick_count = cube_sched
            .iter()
            .skip(1)
            .filter(|l| l.height_mm >= 0.275)
            .count();
        assert!(
            thick_count >= 40 && cube_sched.iter().skip(1).all(|l| l.height_mm >= 0.14),
            "vertical walls on cube should stay thick: {:?}",
            cube_sched.iter().map(|l| l.height_mm).collect::<Vec<_>>()
        );

        let dome_raw = build_sample_shape(SampleShape::Dome).unwrap();
        let dome_insp = inspect_mesh(&dome_raw);
        assert_eq!(dome_insp.non_manifold_edges, 0);
        let dome = prepare_for_plate(&dome_raw, 250.0, 250.0).unwrap();
        let dome_sched = build_variable_layer_schedule(&dome, &adapt_params).unwrap();
        assert!((dome_sched.last().unwrap().z_top_mm - 18.0).abs() < 1e-6);
        let upper_heights: Vec<f64> = dome_sched.iter().skip(1).map(|l| l.height_mm).collect();
        let min_h = upper_heights.iter().copied().fold(f64::INFINITY, f64::min);
        let max_h = upper_heights.iter().copied().fold(0.0, f64::max);
        assert!(
            min_h <= 0.10,
            "expected fine top cap layers, got min_h={min_h}"
        );
        assert!(
            max_h >= 0.24,
            "expected thick vertical base layers, got max_h={max_h}"
        );

        let banded = build_variable_layer_schedule(
            &cube,
            &VariableScheduleParams {
                first_layer_height_mm: 0.2,
                nominal_layer_height_mm: 0.2,
                adaptive: false,
                min_layer_height_mm: 0.08,
                max_layer_height_mm: 0.28,
                quality: 0.5,
                smooth_radius_mm: 2.0,
                max_step_ratio: 1.5,
                explicit_ranges: vec![
                    ExplicitHeightRange {
                        min_z_mm: 0.0,
                        max_z_mm: 4.0,
                        layer_height_mm: 0.25,
                    },
                    ExplicitHeightRange {
                        min_z_mm: 4.0,
                        max_z_mm: 8.0,
                        layer_height_mm: 0.10,
                    },
                    ExplicitHeightRange {
                        min_z_mm: 8.0,
                        max_z_mm: 12.0,
                        layer_height_mm: 0.20,
                    },
                ],
            },
        )
        .unwrap();
        assert!((banded.last().unwrap().z_top_mm - 12.0).abs() < 1e-6);
        let band_hs: Vec<f64> = banded.iter().skip(1).map(|l| l.height_mm).collect();
        let bmin = band_hs.iter().copied().fold(f64::INFINITY, f64::min);
        let bmax = band_hs.iter().copied().fold(0.0, f64::max);
        assert!(bmin <= 0.105 && bmax >= 0.245, "min={bmin} max={bmax}");
    }

    #[test]
    fn modifier_primitives_transform_without_grounding_and_slice_selected_z() {
        let box_raw = build_modifier_primitive(ModifierPrimitive::Box {
            width_mm: 10.0,
            depth_mm: 8.0,
            height_mm: 4.0,
        })
        .unwrap();
        let xf = ModifierTransform {
            position_xyz_mm: [125.0, 125.0, 10.0],
            rotation_deg: [0.0, 0.0, 90.0],
            scale: [1.0, 1.0, 1.0],
        };
        let prepared = prepare_modifier_mesh(&box_raw, xf).unwrap();
        let bb = prepared.aabb().unwrap();
        assert!((bb.min.z - 8.0).abs() < 1e-6);
        assert!((bb.max.z - 12.0).abs() < 1e-6);
        assert!((bb.size().x - 8.0).abs() < 1e-4);
        assert!((bb.size().y - 10.0).abs() < 1e-4);

        let cyl_raw = build_modifier_primitive(ModifierPrimitive::Cylinder {
            radius_mm: 5.0,
            height_mm: 6.0,
            segments: 32,
        })
        .unwrap();
        assert_eq!(inspect_mesh(&cyl_raw).non_manifold_edges, 0);

        let slices = slice_mesh_at_z_planes(&prepared, &[2.0, 10.0, 15.0]).unwrap();
        assert_eq!(slices.len(), 3);
        assert!(slices[0].is_empty());
        assert_eq!(slices[1].len(), 1);
        assert!((slices[1][0].area_mm2() - 80.0).abs() < 1e-2);
        assert!(slices[2].is_empty());

        assert!(
            build_modifier_primitive(ModifierPrimitive::Box {
                width_mm: 0.0,
                depth_mm: 5.0,
                height_mm: 5.0,
            })
            .is_err()
        );
    }

    #[test]
    fn stitch_50um_open_chain_and_reversed_chain_fallback() {
        let p = |x, y| Point2::from_mm(x, y).unwrap();
        // Watertight loop stays exact.
        let exact = vec![
            (p(0.0, 0.0), p(10.0, 0.0)),
            (p(10.0, 0.0), p(10.0, 10.0)),
            (p(10.0, 10.0), p(0.0, 10.0)),
            (p(0.0, 10.0), p(0.0, 0.0)),
        ];
        let exact_loops = stitch_segments_into_loops(&exact);
        assert_eq!(exact_loops.len(), 1);
        assert!((exact_loops[0].area_mm2() - 100.0).abs() < 1e-6);

        // One reversed edge + 30 um (0.03 mm) crack exceeds 10 um pass -> 50 um open-chain fallback closes it.
        let cracked_and_flipped = vec![
            (p(0.0, 0.0), p(10.0, 0.0)),
            (p(10.03, 10.0), p(10.0, 0.0)), // reversed + 30 um gap
            (p(10.03, 10.0), p(0.0, 10.0)),
            (p(0.0, 10.0), p(0.0, 0.0)),
        ];
        let repaired_loops = stitch_segments_into_loops(&cracked_and_flipped);
        assert_eq!(repaired_loops.len(), 1);
        assert!((repaired_loops[0].area_mm2() - 100.0).abs() < 0.5);
    }

    #[test]
    fn all_sample_shapes_are_watertight_and_tapered_wedge_matches_spec() {
        for shape in SampleShape::ALL {
            let mesh = build_sample_shape(shape).unwrap();
            let insp = inspect_mesh(&mesh);
            assert_eq!(insp.boundary_edges, 0, "boundary edges on {shape:?}");
            assert!(insp.volume_mm3 > 0.0, "non-positive volume on {shape:?}");
            if shape != SampleShape::BridgeArch {
                assert_eq!(
                    insp.non_manifold_edges, 0,
                    "non-manifold edges on {shape:?}"
                );
            }
        }

        let wedge = build_sample_shape(SampleShape::TaperedWedge).unwrap();
        let insp = inspect_mesh(&wedge);
        assert_eq!(wedge.triangles.len(), 12);
        let size = wedge.aabb().unwrap().size();
        assert!((size.x - 36.0).abs() < 1e-6);
        assert!((size.y - 4.0).abs() < 1e-6);
        assert!((size.z - 8.0).abs() < 1e-6);
        assert!((insp.volume_mm3 - 612.0).abs() < 1e-3);
    }
}
