//! Deterministic FDM slicing pipeline: slice -> perimeters -> skins -> infill -> adhesion.

mod arachne;
pub mod dfm;
mod modifiers;
mod spatial;
mod tree_support;

pub use dfm::*;
pub use modifiers::ModifierVolume;
use spatial::MaskClipIndex;

use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI, SQRT_2, TAU};

use layercraft_clip::{
    ClipError, JoinType, clip_polylines, difference, intersection, offset, union,
};
use layercraft_config::{
    InfillPattern, IroningType, ModifierOverrides, PrintConfig, PrintSequence, SupportStyle,
    SurfacePattern, WallGenerator, WallOrder,
};
use layercraft_geom::{Aabb2, ExPolygon, GeomError, Point2, Polygon, TriangleMesh, mm_to_nm};
use layercraft_mesh::{
    ExplicitHeightRange, MeshError, RawSliceLayer, VariableScheduleParams, build_layer_schedule,
    build_variable_layer_schedule, prepare_for_plate, slice_mesh, slice_mesh_with_schedule,
};
use modifiers::{EffectiveLayerParams, partition_regions, slice_modifier_volumes};
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
    #[error(
        "sequential print clearance violation between `{object_a}` and `{object_b}` ({clearance_mm:.1} mm < required {required_mm:.1} mm)"
    )]
    SequentialClearanceViolation {
        object_a: String,
        object_b: String,
        clearance_mm: f64,
        required_mm: f64,
    },
    #[error(
        "sequential print height violation on `{object}` ({height_mm:.1} mm > gantry clearance {max_clearance_height_mm:.1} mm)"
    )]
    SequentialHeightViolation {
        object: String,
        height_mm: f64,
        max_clearance_height_mm: f64,
    },
    #[error("slicing cancelled")]
    Cancelled,
}

/// Cooperative cancellation flag and per-layer progress counters shared with slicing workers.
#[derive(Default)]
pub struct SliceProgress {
    completed_layers: std::sync::atomic::AtomicUsize,
    total_layers: std::sync::atomic::AtomicUsize,
    cancelled: std::sync::atomic::AtomicBool,
    on_update: Option<Box<dyn Fn(usize, usize) + Send + Sync>>,
}

impl std::fmt::Debug for SliceProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SliceProgress")
            .field(
                "completed_layers",
                &self
                    .completed_layers
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .field(
                "total_layers",
                &self.total_layers.load(std::sync::atomic::Ordering::Relaxed),
            )
            .field(
                "cancelled",
                &self.cancelled.load(std::sync::atomic::Ordering::Relaxed),
            )
            .field("has_callback", &self.on_update.is_some())
            .finish()
    }
}

impl SliceProgress {
    /// Construct a fresh zeroed progress/cancellation handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a progress handle that invokes `on_update(completed_layers, total_layers)` whenever counters change.
    pub fn with_callback<F>(on_update: F) -> Self
    where
        F: Fn(usize, usize) + Send + Sync + 'static,
    {
        Self {
            completed_layers: std::sync::atomic::AtomicUsize::new(0),
            total_layers: std::sync::atomic::AtomicUsize::new(0),
            cancelled: std::sync::atomic::AtomicBool::new(false),
            on_update: Some(Box::new(on_update)),
        }
    }

    fn notify(&self) {
        if let Some(cb) = &self.on_update {
            let (done, total) = self.snapshot();
            cb(done, total);
        }
    }

    /// Request cooperative cancellation at the next layer or stage boundary.
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Return `true` if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Set the total expected layer work count.
    pub fn set_total_layers(&self, total: usize) {
        self.total_layers
            .store(total, std::sync::atomic::Ordering::Relaxed);
        self.notify();
    }

    /// Increment the total expected layer work count.
    pub fn add_total_layers(&self, delta: usize) {
        self.total_layers
            .fetch_add(delta, std::sync::atomic::Ordering::Relaxed);
        self.notify();
    }

    /// Increment completed layer count by 1.
    pub fn inc_completed_layers(&self) {
        self.completed_layers
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.notify();
    }

    /// Read `(completed_layers, total_layers)`.
    pub fn snapshot(&self) -> (usize, usize) {
        (
            self.completed_layers
                .load(std::sync::atomic::Ordering::Relaxed),
            self.total_layers.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

/// Classification of an extruded toolpath segment.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum ExtrusionRole {
    OuterWall,
    InnerWall,
    OverhangWall,
    GapFill,
    BottomSurface,
    TopSurface,
    Ironing,
    InternalSolidInfill,
    Bridge,
    InternalBridge,
    SparseInfill,
    Support,
    SupportInterface,
    Skirt,
    Brim,
}

impl ExtrusionRole {
    /// Human-readable feature tag matching standard slicer G-code `;TYPE:` conventions.
    pub fn label(self) -> &'static str {
        match self {
            Self::OuterWall => "Outer wall",
            Self::InnerWall => "Inner wall",
            Self::OverhangWall => "Overhang wall",
            Self::GapFill => "Gap fill",
            Self::BottomSurface => "Bottom surface",
            Self::TopSurface => "Top surface",
            Self::Ironing => "Ironing",
            Self::InternalSolidInfill => "Internal solid infill",
            Self::Bridge => "Bridge",
            Self::InternalBridge => "Internal Bridge",
            Self::SparseInfill => "Sparse infill",
            Self::Support => "Support",
            Self::SupportInterface => "Support interface",
            Self::Skirt => "Skirt",
            Self::Brim => "Brim",
        }
    }
}

fn default_flow_ratio() -> f64 {
    1.0
}

/// A 2D toolpath polyline at a single layer height.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExtrusionPath {
    pub role: ExtrusionRole,
    pub width_mm: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub point_widths_mm: Vec<f64>,
    pub height_mm: f64,
    pub speed_mm_s: f64,
    #[serde(default = "default_flow_ratio")]
    pub flow_ratio: f64,
    pub points: Vec<Point2>,
    pub closed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_percent: Option<u8>,
}

impl ExtrusionPath {
    /// True if per-vertex extrusion widths are populated and aligned with [`Self::points`].
    #[inline]
    pub fn has_variable_width(&self) -> bool {
        !self.point_widths_mm.is_empty() && self.point_widths_mm.len() == self.points.len()
    }

    /// Extrusion width (mm) at vertex `idx` (falls back to nominal [`Self::width_mm`]).
    #[inline]
    pub fn vertex_width_mm(&self, idx: usize) -> f64 {
        if self.has_variable_width() {
            self.point_widths_mm
                .get(idx)
                .copied()
                .unwrap_or(self.width_mm)
        } else {
            self.width_mm
        }
    }

    /// Mean extrusion width (mm) across segment `seg_idx -> next`.
    #[inline]
    pub fn segment_width_mm(&self, seg_idx: usize) -> f64 {
        let n = self.points.len();
        if n == 0 || !self.has_variable_width() {
            return self.width_mm;
        }
        let w0 = self.vertex_width_mm(seg_idx);
        let next_idx = if self.closed {
            (seg_idx + 1) % n
        } else {
            (seg_idx + 1).min(n - 1)
        };
        let w1 = self.vertex_width_mm(next_idx);
        0.5 * (w0 + w1)
    }

    /// Minimum vertex width along this path (mm).
    pub fn min_width_mm(&self) -> f64 {
        if self.has_variable_width() {
            self.point_widths_mm
                .iter()
                .copied()
                .fold(f64::INFINITY, f64::min)
        } else {
            self.width_mm
        }
    }

    /// Maximum vertex width along this path (mm).
    pub fn max_width_mm(&self) -> f64 {
        if self.has_variable_width() {
            self.point_widths_mm.iter().copied().fold(0.0, f64::max)
        } else {
            self.width_mm
        }
    }
}

/// Metadata describing one logical print object for Cancel-Object (`EXCLUDE_OBJECT` / `M486`) output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SlicedObjectInfo {
    pub id: u64,
    pub name: String,
    pub gcode_id: String,
    pub center_xy_mm: [f64; 2],
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub polygon_xy_mm: Vec<[f64; 2]>,
    pub max_z_mm: f64,
}

/// Prepared world-space plate object passed to [`slice_plate_objects`].
#[derive(Debug, Clone, Copy)]
pub struct PlateSliceObject<'a> {
    pub id: u64,
    pub name: &'a str,
    pub prepared_mesh: &'a TriangleMesh,
    pub overrides: &'a ModifierOverrides,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_id: Option<u64>,
}

/// Output of slicing a single plate.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct SlicedPlate {
    pub layers: Vec<SlicedLayer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<SlicedObjectInfo>,
    #[serde(default)]
    pub print_sequence: PrintSequence,
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

/// Run the full slicing pipeline for a single mesh, centering and grounding it on the configured bed.
pub fn slice_single_mesh(
    mesh: &TriangleMesh,
    config: &PrintConfig,
) -> Result<SlicedPlate, SliceError> {
    slice_single_mesh_with_modifiers(mesh, config, &[])
}

/// Run the full slicing pipeline for a single mesh with modifier volumes and/or support masks.
pub fn slice_single_mesh_with_modifiers(
    mesh: &TriangleMesh,
    config: &PrintConfig,
    modifiers: &[ModifierVolume],
) -> Result<SlicedPlate, SliceError> {
    config.validate()?;
    let prepared = prepare_for_plate(
        mesh,
        config.machine.bed_width_mm,
        config.machine.bed_depth_mm,
    )?;
    slice_prepared_mesh_with_modifiers(&prepared, config, modifiers)
}

/// Run the full slicing pipeline on an already positioned/grounded plate mesh without re-centering.
pub fn slice_prepared_mesh(
    prepared: &TriangleMesh,
    config: &PrintConfig,
) -> Result<SlicedPlate, SliceError> {
    slice_prepared_mesh_with_modifiers(prepared, config, &[])
}

/// Run the full slicing pipeline on a prepared plate mesh with optional modifier volumes.
pub fn slice_prepared_mesh_with_modifiers(
    prepared: &TriangleMesh,
    config: &PrintConfig,
    modifiers: &[ModifierVolume],
) -> Result<SlicedPlate, SliceError> {
    slice_prepared_mesh_with_progress(prepared, config, modifiers, None)
}

/// Run the full slicing pipeline on a prepared plate mesh with cooperative progress & cancellation.
pub fn slice_prepared_mesh_with_progress(
    prepared: &TriangleMesh,
    config: &PrintConfig,
    modifiers: &[ModifierVolume],
    progress: Option<&SliceProgress>,
) -> Result<SlicedPlate, SliceError> {
    if progress.is_some_and(SliceProgress::is_cancelled) {
        return Err(SliceError::Cancelled);
    }
    config.validate()?;
    for m in modifiers {
        m.overrides.validate()?;
    }

    let raw_layers = slice_mesh_with_config_schedule(prepared, config)?;
    if progress.is_some_and(SliceProgress::is_cancelled) {
        return Err(SliceError::Cancelled);
    }
    if let Some(p) = progress {
        p.set_total_layers(raw_layers.len());
    }
    let layers = slice_raw_layers_internal(&raw_layers, config, modifiers, true, None, progress)?;
    Ok(SlicedPlate {
        layers,
        objects: Vec::new(),
        print_sequence: PrintSequence::ByLayer,
    })
}

fn slice_mesh_with_config_schedule(
    prepared: &TriangleMesh,
    config: &PrintConfig,
) -> Result<Vec<RawSliceLayer>, SliceError> {
    let al = &config.process.adaptive_layer_height;
    let explicit_ranges: Vec<ExplicitHeightRange> = config
        .process
        .height_ranges
        .iter()
        .filter_map(|hr| {
            hr.layer_height_mm.map(|h| ExplicitHeightRange {
                min_z_mm: hr.min_z_mm,
                max_z_mm: hr.max_z_mm,
                layer_height_mm: h,
            })
        })
        .collect();

    if al.enabled || !explicit_ranges.is_empty() {
        let nozzle_d = config.machine.nozzle_diameter_mm;
        let nom_h = config.process.layer_height_mm;
        let sched_params = VariableScheduleParams {
            first_layer_height_mm: config.process.first_layer_height_mm,
            nominal_layer_height_mm: nom_h,
            adaptive: al.enabled,
            min_layer_height_mm: al.effective_min_layer_height_mm(nozzle_d, nom_h),
            max_layer_height_mm: al.effective_max_layer_height_mm(nozzle_d, nom_h),
            quality: al.quality,
            smooth_radius_mm: al.smooth_radius_mm,
            max_step_ratio: al.max_step_ratio,
            explicit_ranges,
        };
        let schedule = build_variable_layer_schedule(prepared, &sched_params)?;
        Ok(slice_mesh_with_schedule(prepared, &schedule)?)
    } else {
        Ok(slice_mesh(
            prepared,
            config.process.first_layer_height_mm,
            config.process.layer_height_mm,
        )?)
    }
}

/// Merge multiple prepared plate meshes and slice them together while preserving their transforms.
pub fn slice_meshes(
    prepared_meshes: &[TriangleMesh],
    config: &PrintConfig,
) -> Result<SlicedPlate, SliceError> {
    slice_meshes_with_modifiers(prepared_meshes, config, &[])
}

/// Merge multiple prepared plate meshes and slice them together with optional modifier volumes.
pub fn slice_meshes_with_modifiers(
    prepared_meshes: &[TriangleMesh],
    config: &PrintConfig,
    modifiers: &[ModifierVolume],
) -> Result<SlicedPlate, SliceError> {
    config.validate()?;
    let combined = TriangleMesh::merge(prepared_meshes)?;
    slice_prepared_mesh_with_modifiers(&combined, config, modifiers)
}

fn sanitize_gcode_object_id(id: u64, name: &str) -> String {
    let mut slug = String::new();
    let mut prev_underscore = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_underscore = false;
        } else if !prev_underscore && !slug.is_empty() {
            slug.push('_');
            prev_underscore = true;
        }
        if slug.len() >= 32 {
            break;
        }
    }
    while slug.ends_with('_') {
        slug.pop();
    }
    if slug.is_empty() {
        format!("obj_{id}")
    } else {
        format!("obj_{id}_{slug}")
    }
}

fn effective_object_config(base: &PrintConfig, overrides: &ModifierOverrides) -> PrintConfig {
    if overrides.is_empty() {
        return base.clone();
    }
    let mut params = EffectiveLayerParams::from_config(base);
    params.apply_overrides(overrides);
    params.to_layer_config(base)
}

fn build_sliced_object_info(obj: &PlateSliceObject<'_>) -> Result<SlicedObjectInfo, SliceError> {
    let bb = obj
        .prepared_mesh
        .aabb()
        .ok_or(SliceError::Geom(GeomError::EmptyMesh))?;
    let center_xy_mm = [0.5 * (bb.min.x + bb.max.x), 0.5 * (bb.min.y + bb.max.y)];
    let polygon_xy_mm = vec![
        [bb.min.x, bb.min.y],
        [bb.max.x, bb.min.y],
        [bb.max.x, bb.max.y],
        [bb.min.x, bb.max.y],
    ];
    Ok(SlicedObjectInfo {
        id: obj.id,
        name: obj.name.to_owned(),
        gcode_id: sanitize_gcode_object_id(obj.id, obj.name),
        center_xy_mm,
        polygon_xy_mm,
        max_z_mm: (bb.max.z - bb.min.z).max(0.0),
    })
}

fn validate_sequential_clearances(
    ordered: &[(&PlateSliceObject<'_>, layercraft_geom::Aabb3)],
    config: &PrintConfig,
) -> Result<(), SliceError> {
    if ordered.len() <= 1 {
        return Ok(());
    }
    let max_h = config.process.extruder_clearance_height_mm;
    let req_r = config.process.extruder_clearance_radius_mm;

    for (idx, (obj, bb)) in ordered.iter().enumerate() {
        let obj_h = (bb.max.z - bb.min.z).max(0.0);
        if idx + 1 < ordered.len() && obj_h > max_h + 1e-4 {
            return Err(SliceError::SequentialHeightViolation {
                object: obj.name.to_owned(),
                height_mm: obj_h,
                max_clearance_height_mm: max_h,
            });
        }
        for (other, other_bb) in ordered.iter().skip(idx + 1) {
            let dx = (bb.min.x - other_bb.max.x)
                .max(other_bb.min.x - bb.max.x)
                .max(0.0);
            let dy = (bb.min.y - other_bb.max.y)
                .max(other_bb.min.y - bb.max.y)
                .max(0.0);
            let clearance = dx.hypot(dy);
            if clearance + 1e-4 < req_r {
                return Err(SliceError::SequentialClearanceViolation {
                    object_a: obj.name.to_owned(),
                    object_b: other.name.to_owned(),
                    clearance_mm: clearance,
                    required_mm: req_r,
                });
            }
        }
    }
    Ok(())
}

/// Slice a collection of distinct build-plate objects, preserving per-object IDs, overrides, and sequence mode.
pub fn slice_plate_objects(
    objects: &[PlateSliceObject<'_>],
    config: &PrintConfig,
    modifiers: &[ModifierVolume],
) -> Result<SlicedPlate, SliceError> {
    slice_plate_objects_with_progress(objects, config, modifiers, None)
}

/// Slice a collection of distinct build-plate objects with cooperative layer progress and cancellation.
pub fn slice_plate_objects_with_progress(
    objects: &[PlateSliceObject<'_>],
    config: &PrintConfig,
    modifiers: &[ModifierVolume],
    progress: Option<&SliceProgress>,
) -> Result<SlicedPlate, SliceError> {
    if progress.is_some_and(SliceProgress::is_cancelled) {
        return Err(SliceError::Cancelled);
    }
    if objects.is_empty() {
        return Err(SliceError::Geom(GeomError::EmptyMesh));
    }
    config.validate()?;
    for m in modifiers {
        m.overrides.validate()?;
    }
    for obj in objects {
        obj.overrides.validate()?;
    }

    let mut obj_infos = Vec::with_capacity(objects.len());
    for obj in objects {
        obj_infos.push(build_sliced_object_info(obj)?);
    }

    match config.process.print_sequence {
        PrintSequence::ByObject => {
            let mut ranked: Vec<(&PlateSliceObject<'_>, layercraft_geom::Aabb3)> =
                Vec::with_capacity(objects.len());
            for obj in objects {
                let bb = obj
                    .prepared_mesh
                    .aabb()
                    .ok_or(SliceError::Geom(GeomError::EmptyMesh))?;
                ranked.push((obj, bb));
            }
            ranked.sort_by(|(a, a_bb), (b, b_bb)| {
                let az = a_bb.max.z - a_bb.min.z;
                let bz = b_bb.max.z - b_bb.min.z;
                az.total_cmp(&bz)
                    .then_with(|| a_bb.min.y.total_cmp(&b_bb.min.y))
                    .then_with(|| a_bb.min.x.total_cmp(&b_bb.min.x))
                    .then_with(|| a.id.cmp(&b.id))
            });
            validate_sequential_clearances(&ranked, config)?;

            let mut prepared_objs = Vec::with_capacity(ranked.len());
            let mut total_layers = 0_usize;
            for (obj, _) in ranked {
                if progress.is_some_and(SliceProgress::is_cancelled) {
                    return Err(SliceError::Cancelled);
                }
                let obj_cfg = effective_object_config(config, obj.overrides);
                let raw = slice_mesh_with_config_schedule(obj.prepared_mesh, &obj_cfg)?;
                total_layers = total_layers.saturating_add(raw.len());
                prepared_objs.push((obj, obj_cfg, raw));
            }
            if let Some(p) = progress {
                p.set_total_layers(total_layers);
            }

            let mut all_layers = Vec::with_capacity(total_layers);
            for (obj, obj_cfg, raw) in prepared_objs {
                if progress.is_some_and(SliceProgress::is_cancelled) {
                    return Err(SliceError::Cancelled);
                }
                let mut layers =
                    slice_raw_layers_internal(&raw, &obj_cfg, modifiers, true, None, progress)?;
                for layer in &mut layers {
                    layer.object_id = Some(obj.id);
                    for path in &mut layer.paths {
                        path.object_id = Some(obj.id);
                    }
                }
                all_layers.extend(layers);
            }
            Ok(SlicedPlate {
                layers: all_layers,
                objects: obj_infos,
                print_sequence: PrintSequence::ByObject,
            })
        }
        PrintSequence::ByLayer => {
            if objects.len() == 1
                && let Some(obj) = objects.first()
            {
                let obj_cfg = effective_object_config(config, obj.overrides);
                let mut plate = slice_prepared_mesh_with_progress(
                    obj.prepared_mesh,
                    &obj_cfg,
                    modifiers,
                    progress,
                )?;
                for layer in &mut plate.layers {
                    for path in &mut layer.paths {
                        if !matches!(path.role, ExtrusionRole::Skirt | ExtrusionRole::Brim) {
                            path.object_id = Some(obj.id);
                        }
                    }
                }
                plate.objects = obj_infos;
                plate.print_sequence = PrintSequence::ByLayer;
                return Ok(plate);
            }

            let al = &config.process.adaptive_layer_height;
            let explicit_ranges: Vec<ExplicitHeightRange> = config
                .process
                .height_ranges
                .iter()
                .filter_map(|hr| {
                    hr.layer_height_mm.map(|h| ExplicitHeightRange {
                        min_z_mm: hr.min_z_mm,
                        max_z_mm: hr.max_z_mm,
                        layer_height_mm: h,
                    })
                })
                .collect();

            let max_z = obj_infos.iter().map(|o| o.max_z_mm).fold(0.0_f64, f64::max);
            let schedule = if al.enabled || !explicit_ranges.is_empty() {
                let merged_tris: Vec<layercraft_geom::Triangle> = objects
                    .iter()
                    .flat_map(|o| o.prepared_mesh.triangles.iter().copied())
                    .collect();
                let merged_mesh = TriangleMesh::new(merged_tris)?;
                let nozzle_d = config.machine.nozzle_diameter_mm;
                let nom_h = config.process.layer_height_mm;
                let sched_params = VariableScheduleParams {
                    first_layer_height_mm: config.process.first_layer_height_mm,
                    nominal_layer_height_mm: nom_h,
                    adaptive: al.enabled,
                    min_layer_height_mm: al.effective_min_layer_height_mm(nozzle_d, nom_h),
                    max_layer_height_mm: al.effective_max_layer_height_mm(nozzle_d, nom_h),
                    quality: al.quality,
                    smooth_radius_mm: al.smooth_radius_mm,
                    max_step_ratio: al.max_step_ratio,
                    explicit_ranges,
                };
                build_variable_layer_schedule(&merged_mesh, &sched_params)?
            } else {
                build_layer_schedule(
                    max_z,
                    config.process.first_layer_height_mm,
                    config.process.layer_height_mm,
                )?
            };

            let n_layers = schedule.len();
            let mut raw_per_obj = Vec::with_capacity(objects.len());
            let mut total_layers = 0_usize;
            for obj in objects {
                if progress.is_some_and(SliceProgress::is_cancelled) {
                    return Err(SliceError::Cancelled);
                }
                let obj_cfg = effective_object_config(config, obj.overrides);
                let mut raw = slice_mesh_with_schedule(obj.prepared_mesh, &schedule)?;
                while raw.last().is_some_and(|l| l.regions.is_empty()) {
                    raw.pop();
                }
                if raw.is_empty() {
                    continue;
                }
                total_layers = total_layers.saturating_add(raw.len());
                raw_per_obj.push((obj.id, obj_cfg, raw));
            }
            if let Some(p) = progress {
                p.set_total_layers(total_layers);
            }

            let mut per_obj_layers: Vec<(u64, Vec<SlicedLayer>)> =
                Vec::with_capacity(raw_per_obj.len());
            for (obj_id, obj_cfg, raw) in raw_per_obj {
                if progress.is_some_and(SliceProgress::is_cancelled) {
                    return Err(SliceError::Cancelled);
                }
                let sliced_layers = slice_raw_layers_internal(
                    &raw,
                    &obj_cfg,
                    modifiers,
                    false,
                    Some(obj_id),
                    progress,
                )?;
                per_obj_layers.push((obj_id, sliced_layers));
            }

            let mut combined_layers = Vec::with_capacity(n_layers);
            for idx in 0..n_layers {
                if progress.is_some_and(SliceProgress::is_cancelled) {
                    return Err(SliceError::Cancelled);
                }
                let Some(plane) = schedule.get(idx) else {
                    continue;
                };
                let mut slice_regions = Vec::new();
                let mut layer_paths = Vec::new();
                for (_id, obj_layers) in &per_obj_layers {
                    if let Some(ol) = obj_layers.get(idx) {
                        slice_regions.extend(ol.slice_regions.iter().cloned());
                        layer_paths.extend(ol.paths.iter().cloned());
                    }
                }
                if idx == 0 && !slice_regions.is_empty() {
                    let mut adhesion = Vec::new();
                    append_adhesion_paths(&slice_regions, plane.height_mm, config, &mut adhesion)?;
                    adhesion.extend(layer_paths);
                    layer_paths = adhesion;
                }
                let (fan_percent, estimated_time_s) =
                    apply_flow_and_cooling(idx, &mut layer_paths, config);
                combined_layers.push(SlicedLayer {
                    index: plane.index,
                    z_top_mm: plane.z_top_mm,
                    height_mm: plane.height_mm,
                    slice_regions,
                    paths: layer_paths,
                    fan_percent,
                    estimated_time_s,
                    object_id: None,
                });
            }
            while combined_layers
                .last()
                .is_some_and(|l| l.slice_regions.is_empty() && l.paths.is_empty())
            {
                combined_layers.pop();
            }

            Ok(SlicedPlate {
                layers: combined_layers,
                objects: obj_infos,
                print_sequence: PrintSequence::ByLayer,
            })
        }
    }
}

fn parallel_try_map_slice<T, R, F>(
    items: Vec<T>,
    progress: Option<&SliceProgress>,
    count_progress: bool,
    f: &F,
) -> Result<Vec<R>, SliceError>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Result<R, SliceError> + Sync,
{
    if progress.is_some_and(SliceProgress::is_cancelled) {
        return Err(SliceError::Cancelled);
    }

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
            let cancelled = std::sync::atomic::AtomicBool::new(false);
            let slots: Vec<std::sync::Mutex<Option<T>>> = items
                .into_iter()
                .map(|v| std::sync::Mutex::new(Some(v)))
                .collect();
            let results: Vec<std::sync::Mutex<Option<Result<R, SliceError>>>> =
                (0..n).map(|_| std::sync::Mutex::new(None)).collect();
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    let next_ref = &next_idx;
                    let cancel_ref = &cancelled;
                    let slots_ref = &slots;
                    let results_ref = &results;
                    scope.spawn(move || {
                        loop {
                            if cancel_ref.load(std::sync::atomic::Ordering::Relaxed)
                                || progress.is_some_and(SliceProgress::is_cancelled)
                            {
                                cancel_ref.store(true, std::sync::atomic::Ordering::Relaxed);
                                break;
                            }
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
                            if res.is_ok()
                                && count_progress
                                && let Some(p) = progress
                            {
                                p.inc_completed_layers();
                            }
                            if let Some(rm) = results_ref.get(idx)
                                && let Ok(mut g) = rm.lock()
                            {
                                *g = Some(res);
                            }
                        }
                    });
                }
            });
            if cancelled.load(std::sync::atomic::Ordering::Relaxed)
                || progress.is_some_and(SliceProgress::is_cancelled)
            {
                return Err(SliceError::Cancelled);
            }
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

    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if progress.is_some_and(SliceProgress::is_cancelled) {
            return Err(SliceError::Cancelled);
        }
        let val = f(item)?;
        if count_progress && let Some(p) = progress {
            p.inc_completed_layers();
        }
        out.push(val);
    }
    Ok(out)
}

/// Execute perimeters, skin detection, infill, supports, and adhesion from pre-sliced Z layers.
pub fn slice_from_raw_layers(
    raw_layers: &[RawSliceLayer],
    config: &PrintConfig,
) -> Result<SlicedPlate, SliceError> {
    slice_from_raw_layers_with_modifiers(raw_layers, config, &[])
}

fn union_if_multi(mut polys: Vec<ExPolygon>) -> Result<Vec<ExPolygon>, SliceError> {
    if polys.len() > 1 {
        polys = union(&polys)?;
    }
    Ok(polys)
}

fn bottom_shell_start_idx(
    raw_layers: &[RawSliceLayer],
    idx: usize,
    min_layers: usize,
    min_thickness_mm: Option<f64>,
) -> usize {
    if min_layers == 0 {
        return idx + 1;
    }
    let mut start = idx.saturating_sub(min_layers - 1);
    if let Some(thick) = min_thickness_mm
        && thick > 0.0
        && let Some(cur) = raw_layers.get(idx)
    {
        let z_top_idx = cur.plane.z_top_mm;
        for j in (0..idx).rev() {
            let Some(cand) = raw_layers.get(j) else {
                continue;
            };
            let z_bot_j = cand.plane.z_top_mm - cand.plane.height_mm;
            if z_top_idx - z_bot_j <= thick + 1e-4 {
                start = start.min(j);
            } else {
                break;
            }
        }
    }
    start
}

fn top_shell_end_idx(
    raw_layers: &[RawSliceLayer],
    idx: usize,
    min_layers: usize,
    min_thickness_mm: Option<f64>,
) -> usize {
    let n = raw_layers.len();
    if min_layers == 0 || n == 0 {
        return idx;
    }
    let mut end = (idx + min_layers - 1).min(n - 1);
    if let Some(thick) = min_thickness_mm
        && thick > 0.0
        && let Some(cur) = raw_layers.get(idx)
    {
        let z_bot_idx = cur.plane.z_top_mm - cur.plane.height_mm;
        for j in (idx + 1)..n {
            let Some(cand) = raw_layers.get(j) else {
                continue;
            };
            if cand.plane.z_top_mm - z_bot_idx <= thick + 1e-4 {
                end = end.max(j);
            } else {
                break;
            }
        }
    }
    end
}

#[allow(clippy::too_many_arguments)]
fn generate_sparse_infill_for_region(
    sparse_region: &[ExPolygon],
    layer_idx: usize,
    z_top_mm: f64,
    height_mm: f64,
    density: f64,
    pattern: InfillPattern,
    width_mm: f64,
    speed_mm_s: f64,
) -> Result<Vec<ExtrusionPath>, SliceError> {
    if sparse_region.is_empty() || density <= 1e-6 {
        return Ok(Vec::new());
    }
    match pattern {
        InfillPattern::Rectilinear => {
            let pitch = (width_mm / density).max(width_mm);
            let angle = if layer_idx.is_multiple_of(2) {
                FRAC_PI_4
            } else {
                -FRAC_PI_4
            };
            generate_rectilinear_hatch(
                sparse_region,
                angle,
                pitch,
                0.0,
                false,
                ExtrusionRole::SparseInfill,
                width_mm,
                height_mm,
                speed_mm_s,
                1.0,
            )
        }
        InfillPattern::Grid => {
            // Two orthogonal hatch families at pitch = 2 * width / density yield effective
            // area fill density = density.
            let pitch = (2.0 * width_mm / density).max(width_mm);
            let mut out = Vec::new();
            for angle in [FRAC_PI_4, -FRAC_PI_4] {
                let hatched = generate_rectilinear_hatch(
                    sparse_region,
                    angle,
                    pitch,
                    0.0,
                    false,
                    ExtrusionRole::SparseInfill,
                    width_mm,
                    height_mm,
                    speed_mm_s,
                    1.0,
                )?;
                out.extend(hatched);
            }
            Ok(out)
        }
        InfillPattern::Cubic => {
            let pitch = (3.0 * width_mm / density).max(width_mm);
            let phase = -z_top_mm / SQRT_2;
            let mut out = Vec::new();
            for j in 0..3 {
                let theta = (j as f64) * (2.0 * PI / 3.0);
                let hatch_angle = theta - FRAC_PI_2;
                let hatched = generate_rectilinear_hatch(
                    sparse_region,
                    hatch_angle,
                    pitch,
                    phase,
                    false,
                    ExtrusionRole::SparseInfill,
                    width_mm,
                    height_mm,
                    speed_mm_s,
                    1.0,
                )?;
                out.extend(hatched);
            }
            Ok(out)
        }
        InfillPattern::Gyroid => generate_gyroid_infill(
            sparse_region,
            z_top_mm,
            density,
            width_mm,
            height_mm,
            speed_mm_s,
        ),
    }
}

/// Execute perimeters, skin detection, infill, supports, and adhesion from pre-sliced Z layers with modifiers.
pub fn slice_from_raw_layers_with_modifiers(
    raw_layers: &[RawSliceLayer],
    config: &PrintConfig,
    modifiers: &[ModifierVolume],
) -> Result<SlicedPlate, SliceError> {
    let layers = slice_raw_layers_internal(raw_layers, config, modifiers, true, None, None)?;
    Ok(SlicedPlate {
        layers,
        objects: Vec::new(),
        print_sequence: PrintSequence::ByLayer,
    })
}

fn slice_raw_layers_internal(
    raw_layers: &[RawSliceLayer],
    config: &PrintConfig,
    modifiers: &[ModifierVolume],
    emit_adhesion: bool,
    object_id: Option<u64>,
    progress: Option<&SliceProgress>,
) -> Result<Vec<SlicedLayer>, SliceError> {
    if progress.is_some_and(SliceProgress::is_cancelled) {
        return Err(SliceError::Cancelled);
    }
    config.validate()?;
    for m in modifiers {
        m.overrides.validate()?;
    }

    let n = raw_layers.len();
    let contour_comp = config.process.xy_contour_compensation_mm;
    let hole_comp = config.process.xy_hole_compensation_mm;
    let elephant_comp = config.process.elephant_foot_compensation_mm;
    let has_uniform_xy = contour_comp.abs() > 1e-6 || hole_comp.abs() > 1e-6;
    let comp_raw_holder: Option<Vec<RawSliceLayer>> = if has_uniform_xy {
        let indices: Vec<usize> = (0..n).collect();
        let layers_vec = parallel_try_map_slice(indices, progress, false, &|idx| -> Result<
            RawSliceLayer,
            SliceError,
        > {
            let Some(raw) = raw_layers.get(idx) else {
                return Err(SliceError::Geom(GeomError::EmptyMesh));
            };
            Ok(RawSliceLayer {
                plane: raw.plane,
                regions: apply_xy_compensations(&raw.regions, contour_comp, hole_comp, 0.0)?,
            })
        })?;
        Some(layers_vec)
    } else {
        None
    };
    let base_comp_layers: &[RawSliceLayer] = comp_raw_holder.as_deref().unwrap_or(raw_layers);
    let mut layer0_effective: Option<Vec<ExPolygon>> = if elephant_comp.abs() > 1e-6 {
        raw_layers
            .first()
            .map(|raw0| {
                apply_xy_compensations(&raw0.regions, contour_comp, hole_comp, elephant_comp)
            })
            .transpose()?
    } else {
        None
    };

    if progress.is_some_and(SliceProgress::is_cancelled) {
        return Err(SliceError::Cancelled);
    }
    let sliced_mods = slice_modifier_volumes(modifiers, base_comp_layers)?;
    let has_negatives = sliced_mods.negative_masks.iter().any(|m| !m.is_empty());
    let cut_layers_holder: Option<Vec<RawSliceLayer>> = if has_negatives {
        let indices: Vec<usize> = (0..n).collect();
        let cut_vec = parallel_try_map_slice(indices, progress, false, &|idx| -> Result<
            RawSliceLayer,
            SliceError,
        > {
            let Some(raw) = base_comp_layers.get(idx) else {
                return Err(SliceError::Geom(GeomError::EmptyMesh));
            };
            let neg = sliced_mods
                .negative_masks
                .get(idx)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let regions = if neg.is_empty() || raw.regions.is_empty() {
                raw.regions.clone()
            } else {
                difference(&raw.regions, neg)?
            };
            Ok(RawSliceLayer {
                plane: raw.plane,
                regions,
            })
        })?;
        if let Some(l0) = &mut layer0_effective
            && let Some(neg0) = sliced_mods.negative_masks.first()
            && !neg0.is_empty()
            && !l0.is_empty()
        {
            *l0 = difference(l0, neg0)?;
        }
        Some(cut_vec)
    } else {
        None
    };
    let comp_layers: &[RawSliceLayer] = cut_layers_holder.as_deref().unwrap_or(base_comp_layers);

    if progress.is_some_and(SliceProgress::is_cancelled) {
        return Err(SliceError::Cancelled);
    }
    let support_paths_per_layer = generate_support_paths(
        comp_layers,
        config,
        &sliced_mods.blocker_masks,
        &sliced_mods.enforcer_masks,
    )?;

    let mut per_layer_base_params: Vec<EffectiveLayerParams> = Vec::with_capacity(n);
    for raw in comp_layers {
        let mut base_p = EffectiveLayerParams::from_config(config);
        base_p.apply_height_ranges(&config.process.height_ranges, raw.plane.z_slice_mm);
        per_layer_base_params.push(base_p);
    }

    let layer_indices: Vec<usize> = (0..n).collect();
    let effective_for = |idx: usize| -> &[ExPolygon] {
        if idx == 0 {
            layer0_effective
                .as_deref()
                .or_else(|| comp_layers.first().map(|r| r.regions.as_slice()))
                .unwrap_or(&[])
        } else {
            comp_layers
                .get(idx)
                .map(|r| r.regions.as_slice())
                .unwrap_or(&[])
        }
    };

    // Stage 2: Perimeters & infill boundary extraction (per-layer independent).
    let stage2_results = parallel_try_map_slice(layer_indices.clone(), progress, false, &|idx| {
        let Some(raw) = comp_layers.get(idx) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let Some(base_p) = per_layer_base_params.get(idx) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let effective_regions = effective_for(idx);
        let h = raw.plane.height_mm;
        let active_perim = sliced_mods.active_param_mods(idx, |ov| ov.affects_perimeters());
        if active_perim.is_empty() {
            let layer_cfg = if base_p.wall_loops != config.process.wall_loops
                || (base_p.outer_wall_speed_mm_s - config.process.outer_wall_speed_mm_s).abs()
                    > 1e-9
                || (base_p.inner_wall_speed_mm_s - config.process.inner_wall_speed_mm_s).abs()
                    > 1e-9
            {
                Some(base_p.to_layer_config(config))
            } else {
                None
            };
            let cfg_ref = layer_cfg.as_ref().unwrap_or(config);
            build_perimeters(effective_regions, raw.plane.index, h, cfg_ref)
        } else {
            let buckets = partition_regions(effective_regions, base_p, &active_perim)?;
            let mut layer_walls = Vec::new();
            let mut combined_infill = Vec::new();
            for (sub_regs, b_params) in buckets {
                let bucket_cfg = b_params.to_layer_config(config);
                let (walls, sub_infill) =
                    build_perimeters(&sub_regs, raw.plane.index, h, &bucket_cfg)?;
                layer_walls.extend(walls);
                combined_infill.extend(sub_infill);
            }
            Ok((layer_walls, union_if_multi(combined_infill)?))
        }
    })?;
    let mut per_layer_walls: Vec<Vec<ExtrusionPath>> = Vec::with_capacity(n);
    let mut per_layer_infill_areas: Vec<Vec<ExPolygon>> = Vec::with_capacity(n);
    for (walls, infill_area) in stage2_results {
        per_layer_walls.push(walls);
        per_layer_infill_areas.push(infill_area);
    }

    // Stage 3: Top / bottom exposed surface detection & shell propagation (per-layer independent).
    let stage3_results =
        parallel_try_map_slice(layer_indices.clone(), progress, false, &|idx| -> Result<
            (Vec<ExPolygon>, Vec<ExPolygon>),
            SliceError,
        > {
            let Some(infill_here) = per_layer_infill_areas.get(idx) else {
                return Ok((Vec::new(), Vec::new()));
            };
            let below = idx
                .checked_sub(1)
                .and_then(|j| comp_layers.get(j))
                .map(|l| l.regions.as_slice())
                .unwrap_or(&[]);
            let above = comp_layers
                .get(idx + 1)
                .map(|l| l.regions.as_slice())
                .unwrap_or(&[]);
            Ok((
                difference(infill_here, below)?,
                difference(infill_here, above)?,
            ))
        })?;
    let mut bottom_exposed: Vec<Vec<ExPolygon>> = Vec::with_capacity(n);
    let mut top_exposed: Vec<Vec<ExPolygon>> = Vec::with_capacity(n);
    for (b, t) in stage3_results {
        bottom_exposed.push(b);
        top_exposed.push(t);
    }

    // Stage 4A: Shell classification, bridge anchoring, and sparse region prep per layer.
    struct LayerShellPrep {
        top_here: Vec<ExPolygon>,
        bottom_here: Vec<ExPolygon>,
        internal_solid: Vec<ExPolygon>,
        sparse_region: Vec<ExPolygon>,
        bridge_paths: Vec<ExtrusionPath>,
        sparse_buckets: Vec<(Vec<ExPolygon>, EffectiveLayerParams)>,
        next_prev_sparse: Vec<ExPolygon>,
    }

    let stage4a =
        parallel_try_map_slice(layer_indices.clone(), progress, false, &|idx| -> Result<
            LayerShellPrep,
            SliceError,
        > {
            let Some(raw) = comp_layers.get(idx) else {
                return Err(SliceError::Geom(GeomError::EmptyMesh));
            };
            let Some(infill_here) = per_layer_infill_areas.get(idx) else {
                return Err(SliceError::Geom(GeomError::EmptyMesh));
            };
            let Some(base_p) = per_layer_base_params.get(idx) else {
                return Err(SliceError::Geom(GeomError::EmptyMesh));
            };
            let h = raw.plane.height_mm;
            let active_shells = sliced_mods.active_param_mods(idx, |ov| ov.affects_shells());
            let (top_here, mut bottom_here, mut internal_solid, mut sparse_region) =
                if active_shells.is_empty() {
                    let bottom_shells = base_p.bottom_shell_layers as usize;
                    let top_shells = base_p.top_shell_layers as usize;

                    let mut shell_sources: Vec<ExPolygon> = Vec::new();
                    if bottom_shells > 0 {
                        let min_j = bottom_shell_start_idx(
                            comp_layers,
                            idx,
                            bottom_shells,
                            config.process.bottom_shell_thickness_mm,
                        );
                        for j in min_j..=idx {
                            if let Some(exp) = bottom_exposed.get(j) {
                                shell_sources.extend(exp.iter().cloned());
                            }
                        }
                    }
                    if top_shells > 0 {
                        let max_j = top_shell_end_idx(
                            comp_layers,
                            idx,
                            top_shells,
                            config.process.top_shell_thickness_mm,
                        );
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

                    let top_h = if top_shells > 0 {
                        top_exposed.get(idx).cloned().unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let bot_h = if bottom_shells > 0 {
                        let b = bottom_exposed.get(idx).cloned().unwrap_or_default();
                        difference(&b, &top_h)?
                    } else {
                        Vec::new()
                    };
                    let exposed_h = union(&[top_h.as_slice(), bot_h.as_slice()].concat())?;
                    let internal_h = difference(&solid_union, &exposed_h)?;
                    let sparse_h = difference(infill_here, &solid_union)?;
                    (top_h, bot_h, internal_h, sparse_h)
                } else {
                    let shell_buckets = partition_regions(infill_here, base_p, &active_shells)?;
                    let top_exp_here = top_exposed.get(idx).map(Vec::as_slice).unwrap_or(&[]);
                    let bot_exp_here = bottom_exposed.get(idx).map(Vec::as_slice).unwrap_or(&[]);
                    let mut acc_top = Vec::new();
                    let mut acc_bot = Vec::new();
                    let mut acc_int = Vec::new();
                    let mut acc_spr = Vec::new();

                    for (sub_infill, b_params) in shell_buckets {
                        let b_bottom_n = b_params.bottom_shell_layers as usize;
                        let b_top_n = b_params.top_shell_layers as usize;

                        let mut shell_sources: Vec<ExPolygon> = Vec::new();
                        if b_bottom_n > 0 {
                            let min_j = bottom_shell_start_idx(
                                comp_layers,
                                idx,
                                b_bottom_n,
                                config.process.bottom_shell_thickness_mm,
                            );
                            for j in min_j..=idx {
                                if let Some(exp) = bottom_exposed.get(j) {
                                    shell_sources.extend(exp.iter().cloned());
                                }
                            }
                        }
                        if b_top_n > 0 {
                            let max_j = top_shell_end_idx(
                                comp_layers,
                                idx,
                                b_top_n,
                                config.process.top_shell_thickness_mm,
                            );
                            for j in idx..=max_j {
                                if let Some(exp) = top_exposed.get(j) {
                                    shell_sources.extend(exp.iter().cloned());
                                }
                            }
                        }

                        let solid_union = if shell_sources.is_empty() {
                            Vec::new()
                        } else {
                            intersection(&sub_infill, &union(&shell_sources)?)?
                        };
                        let b_top = if b_top_n > 0 && !top_exp_here.is_empty() {
                            intersection(&sub_infill, top_exp_here)?
                        } else {
                            Vec::new()
                        };
                        let b_bot = if b_bottom_n > 0 && !bot_exp_here.is_empty() {
                            let raw_b = intersection(&sub_infill, bot_exp_here)?;
                            difference(&raw_b, &b_top)?
                        } else {
                            Vec::new()
                        };
                        let b_exposed = union(&[b_top.as_slice(), b_bot.as_slice()].concat())?;
                        let b_internal = difference(&solid_union, &b_exposed)?;
                        let b_sparse = difference(&sub_infill, &solid_union)?;

                        acc_top.extend(b_top);
                        acc_bot.extend(b_bot);
                        acc_int.extend(b_internal);
                        acc_spr.extend(b_sparse);
                    }

                    (
                        union_if_multi(acc_top)?,
                        union_if_multi(acc_bot)?,
                        union_if_multi(acc_int)?,
                        union_if_multi(acc_spr)?,
                    )
                };

            let solid_w = config.process.solid_infill_width_mm;
            let solid_spacing = stadium_spacing_mm(solid_w, h);

            let mut bridge_paths = Vec::new();
            if idx > 0 && config.process.detect_bridges && !bottom_here.is_empty() {
                let below = comp_layers
                    .get(idx - 1)
                    .map(|l| l.regions.as_slice())
                    .unwrap_or(&[]);
                let bridge_raw = std::mem::take(&mut bottom_here);
                let anchor_mm = 2.0 * solid_w;
                let expanded = offset(
                    &bridge_raw,
                    mm_to_nm(anchor_mm)?,
                    JoinType::Miter { limit: 2.0 },
                )?;
                let anchor_ext = intersection(&expanded, below)?;
                let bridge_union = union(&[bridge_raw.as_slice(), anchor_ext.as_slice()].concat())?;
                let anchored_bridges = intersection(infill_here, &bridge_union)?;

                internal_solid = difference(&internal_solid, &anchored_bridges)?;
                sparse_region = difference(&sparse_region, &anchored_bridges)?;

                for bridge_poly in &anchored_bridges {
                    let bridge_angle = compute_bridge_angle(bridge_poly, below)?;
                    let hatched = generate_rectilinear_hatch(
                        std::slice::from_ref(bridge_poly),
                        bridge_angle,
                        solid_spacing,
                        0.0,
                        false,
                        ExtrusionRole::Bridge,
                        solid_w,
                        h,
                        config.process.bridge_speed_mm_s,
                        config.process.bridge_flow_ratio,
                    )?;
                    bridge_paths.extend(hatched);
                }
            }

            let mut next_prev_sparse_src = Vec::new();
            let sparse_buckets = if !sparse_region.is_empty() {
                let active_infill = sliced_mods.active_param_mods(idx, |ov| {
                    ov.sparse_infill_density.is_some()
                        || ov.sparse_infill_pattern.is_some()
                        || ov.infill_speed_mm_s.is_some()
                });
                let buckets = partition_regions(&sparse_region, base_p, &active_infill)?;
                for (sub_sparse, b_params) in &buckets {
                    if !sub_sparse.is_empty() && b_params.sparse_infill_density < 0.99 {
                        next_prev_sparse_src.extend(sub_sparse.iter().cloned());
                    }
                }
                buckets
            } else {
                Vec::new()
            };
            let next_prev_sparse = if next_prev_sparse_src.is_empty() {
                Vec::new()
            } else {
                union_if_multi(next_prev_sparse_src)?
            };

            Ok(LayerShellPrep {
                top_here,
                bottom_here,
                internal_solid,
                sparse_region,
                bridge_paths,
                sparse_buckets,
                next_prev_sparse,
            })
        })?;

    // Stage 4B: Overhang splitting, internal bridges, solid/sparse hatching, and ironing (per-layer parallel).
    let stage4b_paths = parallel_try_map_slice(layer_indices, progress, true, &|idx| -> Result<
        Vec<ExtrusionPath>,
        SliceError,
    > {
        let Some(raw) = comp_layers.get(idx) else {
            return Ok(Vec::new());
        };
        let Some(prep) = stage4a.get(idx) else {
            return Ok(Vec::new());
        };
        let Some(base_p) = per_layer_base_params.get(idx) else {
            return Ok(Vec::new());
        };
        let effective_regions = effective_for(idx);
        let h = raw.plane.height_mm;
        let is_first = idx == 0;
        let mut paths = Vec::new();

        if is_first && emit_adhesion {
            append_adhesion_paths(effective_regions, h, config, &mut paths)?;
        }
        if let Some(spaths) = support_paths_per_layer.get(idx) {
            paths.extend(spaths.iter().cloned());
        }

        if let Some(wpaths) = per_layer_walls.get(idx) {
            if idx > 0 && config.process.detect_overhangs {
                let below = comp_layers
                    .get(idx - 1)
                    .map(|l| l.regions.as_slice())
                    .unwrap_or(&[]);
                let split_walls = split_overhang_wall_paths(wpaths, below, h, config)?;
                paths.extend(split_walls);
            } else {
                paths.extend(wpaths.iter().cloned());
            }
        }

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

        let mut internal_solid_rem: Option<Vec<ExPolygon>> = None;
        let mut internal_bridge_paths = Vec::new();
        let prev_sparse_region: &[ExPolygon] = idx
            .checked_sub(1)
            .and_then(|j| stage4a.get(j))
            .map(|p| p.next_prev_sparse.as_slice())
            .unwrap_or(&[]);
        if idx > 0
            && config.process.detect_bridges
            && !prep.internal_solid.is_empty()
            && !prev_sparse_region.is_empty()
        {
            let raw_ib = intersection(&prep.internal_solid, prev_sparse_region)?;
            if !raw_ib.is_empty() {
                let anchor_mm = 2.0 * solid_w;
                let expanded_ib = offset(
                    &raw_ib,
                    mm_to_nm(anchor_mm)?,
                    JoinType::Miter { limit: 2.0 },
                )?;
                let anchored_ib = intersection(&prep.internal_solid, &expanded_ib)?;
                if !anchored_ib.is_empty() {
                    internal_solid_rem = Some(difference(&prep.internal_solid, &anchored_ib)?);
                    for ib_poly in &anchored_ib {
                        let ib_angle = ib_poly
                            .aabb()
                            .map(|bb| {
                                let (x0, y0) = bb.min.to_mm();
                                let (x1, y1) = bb.max.to_mm();
                                if (x1 - x0) <= (y1 - y0) {
                                    0.0
                                } else {
                                    FRAC_PI_2
                                }
                            })
                            .unwrap_or(0.0);
                        let hatched = generate_rectilinear_hatch(
                            std::slice::from_ref(ib_poly),
                            ib_angle,
                            solid_spacing,
                            0.0,
                            false,
                            ExtrusionRole::InternalBridge,
                            solid_w,
                            h,
                            config.process.bridge_speed_mm_s,
                            config.process.bridge_flow_ratio,
                        )?;
                        internal_bridge_paths.extend(hatched);
                    }
                }
            }
        }
        let effective_internal_solid = internal_solid_rem
            .as_deref()
            .unwrap_or(prep.internal_solid.as_slice());

        let bottom_pat = config.process.effective_bottom_surface_pattern();
        let top_pat = config.process.effective_top_surface_pattern();
        for (region, role, speed, pat) in [
            (
                prep.bottom_here.as_slice(),
                ExtrusionRole::BottomSurface,
                solid_speed,
                bottom_pat,
            ),
            (
                prep.top_here.as_slice(),
                ExtrusionRole::TopSurface,
                top_speed,
                top_pat,
            ),
            (
                effective_internal_solid,
                ExtrusionRole::InternalSolidInfill,
                solid_speed,
                SurfacePattern::Rectilinear,
            ),
        ] {
            if !region.is_empty() {
                let fill_paths = match pat {
                    SurfacePattern::Concentric => {
                        generate_concentric_fill(region, role, solid_w, h, speed, 1.0)?
                    }
                    SurfacePattern::Monotonic => generate_rectilinear_hatch(
                        region,
                        solid_angle,
                        solid_spacing,
                        0.0,
                        true,
                        role,
                        solid_w,
                        h,
                        speed,
                        1.0,
                    )?,
                    SurfacePattern::Rectilinear => generate_rectilinear_hatch(
                        region,
                        solid_angle,
                        solid_spacing,
                        0.0,
                        false,
                        role,
                        solid_w,
                        h,
                        speed,
                        1.0,
                    )?,
                };
                paths.extend(fill_paths);
            }
        }
        paths.extend(prep.bridge_paths.iter().cloned());
        paths.extend(internal_bridge_paths);

        if !prep.sparse_region.is_empty() {
            let sparse_w = config.process.sparse_infill_width_mm;
            for (sub_sparse, b_params) in &prep.sparse_buckets {
                if sub_sparse.is_empty() || b_params.sparse_infill_density <= 1e-6 {
                    continue;
                }
                let speed = if is_first
                    && (b_params.infill_speed_mm_s - config.process.infill_speed_mm_s).abs() < 1e-9
                {
                    config.process.effective_first_layer_infill_speed_mm_s()
                } else {
                    b_params.infill_speed_mm_s
                };
                let hatched = generate_sparse_infill_for_region(
                    sub_sparse,
                    idx,
                    raw.plane.z_top_mm,
                    h,
                    b_params.sparse_infill_density,
                    b_params.sparse_infill_pattern,
                    sparse_w,
                    speed,
                )?;
                paths.extend(hatched);
            }
        }

        let iron = &config.process.ironing;
        let active_iron = sliced_mods.active_param_mods(idx, |ov| ov.ironing_mode.is_some());
        let any_iron = base_p.ironing_mode != IroningType::Off
            || active_iron
                .iter()
                .any(|(_, ov)| ov.ironing_mode.is_some_and(|m| m != IroningType::Off));
        if any_iron && !effective_regions.is_empty() {
            let above = comp_layers
                .get(idx + 1)
                .map(|l| l.regions.as_slice())
                .unwrap_or(&[]);
            let exposed_top = difference(effective_regions, above)?;
            if !exposed_top.is_empty() {
                let iron_regions = if iron.inset_mm > 1e-4 {
                    offset(
                        &exposed_top,
                        -mm_to_nm(iron.inset_mm)?,
                        JoinType::Miter { limit: 2.0 },
                    )?
                } else {
                    exposed_top
                };
                if !iron_regions.is_empty() {
                    let iron_buckets = partition_regions(&iron_regions, base_p, &active_iron)?;
                    let iron_angle = solid_angle + iron.angle_offset_deg.to_radians();
                    let scaled_flow = (iron.flow_ratio * iron.line_spacing_mm / solid_w).max(0.005);
                    for (sub_iron, b_params) in iron_buckets {
                        let iron_here = match b_params.ironing_mode {
                            IroningType::Off => false,
                            IroningType::TopmostSurface => idx + 1 == n,
                            IroningType::AllTopSurfaces => true,
                        };
                        if iron_here && !sub_iron.is_empty() {
                            let iron_paths = generate_rectilinear_hatch(
                                &sub_iron,
                                iron_angle,
                                iron.line_spacing_mm,
                                0.0,
                                true,
                                ExtrusionRole::Ironing,
                                solid_w,
                                h,
                                iron.speed_mm_s,
                                scaled_flow,
                            )?;
                            paths.extend(iron_paths);
                        }
                    }
                }
            }
        }

        Ok(paths)
    })?;

    // Stage 4C: Deterministic sequential island travel ordering & cooling flow/speed calculation.
    if progress.is_some_and(SliceProgress::is_cancelled) {
        return Err(SliceError::Cancelled);
    }
    let mut cursor_xy = (0.0_f64, 0.0_f64);
    let mut layers = Vec::with_capacity(n);
    for (idx, raw_paths) in stage4b_paths.into_iter().enumerate() {
        let Some(raw) = comp_layers.get(idx) else {
            continue;
        };
        let effective_regions = effective_for(idx);
        let h = raw.plane.height_mm;
        let ordered =
            order_layer_paths_by_island(idx, raw_paths, effective_regions, &mut cursor_xy, config);
        let mut paths = ordered;
        if let Some(oid) = object_id {
            for path in &mut paths {
                if !matches!(path.role, ExtrusionRole::Skirt | ExtrusionRole::Brim) {
                    path.object_id = Some(oid);
                }
            }
        }
        let (fan_percent, estimated_time_s) = apply_flow_and_cooling(idx, &mut paths, config);

        layers.push(SlicedLayer {
            index: raw.plane.index,
            z_top_mm: raw.plane.z_top_mm,
            height_mm: h,
            slice_regions: effective_regions.to_vec(),
            paths,
            fan_percent,
            estimated_time_s,
            object_id: None,
        });
    }

    Ok(layers)
}

#[cfg(test)]
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

/// Fallback extrusion acceleration (mm/s^2) when config leaves accelerations unset.
pub const DEFAULT_EXTRUSION_ACCEL_MM_S2: f64 = 3000.0;
/// Fallback travel acceleration (mm/s^2) when config leaves travel/default acceleration unset.
pub const DEFAULT_TRAVEL_ACCEL_MM_S2: f64 = 5000.0;
/// Conservative Z-axis acceleration (mm/s^2) used for layer changes and Z-hops.
pub const DEFAULT_Z_ACCEL_MM_S2: f64 = 500.0;
/// Classical junction-deviation cornering tolerance (mm).
pub const DEFAULT_JUNCTION_DEVIATION_MM: f64 = 0.05;

/// Effective XY acceleration (mm/s^2) for extruding `role` on `layer_idx`.
pub fn effective_role_accel_mm_s2(
    layer_idx: usize,
    role: ExtrusionRole,
    config: &PrintConfig,
) -> f64 {
    let raw = if layer_idx == 0 {
        config
            .process
            .first_layer_acceleration_mm_s2
            .or(config.process.default_acceleration_mm_s2)
    } else {
        match role {
            ExtrusionRole::OuterWall | ExtrusionRole::OverhangWall => config
                .process
                .outer_wall_acceleration_mm_s2
                .or(config.process.default_acceleration_mm_s2),
            ExtrusionRole::InnerWall => config
                .process
                .inner_wall_acceleration_mm_s2
                .or(config.process.default_acceleration_mm_s2),
            _ => config.process.default_acceleration_mm_s2,
        }
    };
    raw.unwrap_or(DEFAULT_EXTRUSION_ACCEL_MM_S2).max(10.0)
}

/// Effective XY travel acceleration (mm/s^2).
pub fn effective_travel_accel_mm_s2(config: &PrintConfig) -> f64 {
    config
        .process
        .travel_acceleration_mm_s2
        .or(config.process.default_acceleration_mm_s2)
        .unwrap_or(DEFAULT_TRAVEL_ACCEL_MM_S2)
        .max(10.0)
}

/// Estimate rest-to-rest 1D trapezoidal (or triangular) move duration in seconds.
pub fn estimate_trapezoid_move_time_s(
    dist_mm: f64,
    cruise_speed_mm_s: f64,
    accel_mm_s2: f64,
) -> f64 {
    if !dist_mm.is_finite()
        || dist_mm <= 1e-9
        || !cruise_speed_mm_s.is_finite()
        || cruise_speed_mm_s <= 1e-9
    {
        return 0.0;
    }
    if !accel_mm_s2.is_finite() || accel_mm_s2 <= 1e-9 {
        return dist_mm / cruise_speed_mm_s;
    }
    let d_ramp = (cruise_speed_mm_s * cruise_speed_mm_s) / accel_mm_s2;
    if dist_mm >= d_ramp {
        dist_mm / cruise_speed_mm_s + cruise_speed_mm_s / accel_mm_s2
    } else {
        2.0 * (dist_mm / accel_mm_s2).sqrt()
    }
}

#[derive(Clone, Copy)]
struct KinSeg {
    len: f64,
    ux: f64,
    uy: f64,
    v: f64,
}

/// Estimate polyline duration (s) with junction-deviation cornering and forward/backward lookahead.
pub fn estimate_polyline_kinematic_time_s(
    pts_mm: &[(f64, f64)],
    seg_speeds_mm_s: &[f64],
    accel_mm_s2: f64,
    junction_dev_mm: f64,
) -> f64 {
    if pts_mm.len() < 2 || seg_speeds_mm_s.is_empty() {
        return 0.0;
    }
    let seg_count = pts_mm.len().saturating_sub(1).min(seg_speeds_mm_s.len());
    let mut segs: Vec<KinSeg> = Vec::with_capacity(seg_count);
    for i in 0..seg_count {
        let (Some(&a), Some(&b), Some(&spd_raw)) =
            (pts_mm.get(i), pts_mm.get(i + 1), seg_speeds_mm_s.get(i))
        else {
            continue;
        };
        let dx = b.0 - a.0;
        let dy = b.1 - a.1;
        let len = dx.hypot(dy);
        if len <= 1e-9 || !len.is_finite() {
            continue;
        }
        let v = if spd_raw.is_finite() && spd_raw > 1e-6 {
            spd_raw
        } else {
            1e-3
        };
        segs.push(KinSeg {
            len,
            ux: dx / len,
            uy: dy / len,
            v,
        });
    }
    let m = segs.len();
    if m == 0 {
        return 0.0;
    }
    if !accel_mm_s2.is_finite() || accel_mm_s2 <= 1e-9 {
        return segs.iter().map(|s| s.len / s.v).sum();
    }

    let a = accel_mm_s2;
    let mut v_junc = vec![0.0_f64; m + 1];
    for k in 1..m {
        let (Some(&prev_s), Some(&cur_s)) = (segs.get(k - 1), segs.get(k)) else {
            continue;
        };
        let cos_phi = (prev_s.ux * cur_s.ux + prev_s.uy * cur_s.uy).clamp(-1.0, 1.0);
        let sin_half_alpha = (0.5 * (1.0 + cos_phi)).max(0.0).sqrt();
        let v_cap = prev_s.v.min(cur_s.v);
        let v_c = if sin_half_alpha >= 0.9999 {
            v_cap
        } else if sin_half_alpha <= 1e-4 || junction_dev_mm <= 1e-9 {
            0.0
        } else {
            ((a * junction_dev_mm * sin_half_alpha) / (1.0 - sin_half_alpha))
                .sqrt()
                .min(v_cap)
        };
        if let Some(slot) = v_junc.get_mut(k) {
            *slot = v_c;
        }
    }

    for i in 0..m {
        let Some(seg) = segs.get(i) else {
            continue;
        };
        let v0 = v_junc.get(i).copied().unwrap_or(0.0);
        let reachable = (v0 * v0 + 2.0 * a * seg.len).max(0.0).sqrt();
        if let Some(next_slot) = v_junc.get_mut(i + 1) {
            *next_slot = next_slot.min(reachable);
        }
    }
    for i in (0..m).rev() {
        let Some(seg) = segs.get(i) else {
            continue;
        };
        let v1 = v_junc.get(i + 1).copied().unwrap_or(0.0);
        let reachable = (v1 * v1 + 2.0 * a * seg.len).max(0.0).sqrt();
        if let Some(cur_slot) = v_junc.get_mut(i) {
            *cur_slot = cur_slot.min(reachable);
        }
    }

    let mut total = 0.0_f64;
    for i in 0..m {
        let Some(seg) = segs.get(i) else {
            continue;
        };
        let v = seg.v;
        let v0 = v_junc.get(i).copied().unwrap_or(0.0).min(v);
        let v1 = v_junc.get(i + 1).copied().unwrap_or(0.0).min(v);
        let d_acc = ((v * v - v0 * v0) / (2.0 * a)).max(0.0);
        let d_dec = ((v * v - v1 * v1) / (2.0 * a)).max(0.0);
        let dt = if d_acc + d_dec <= seg.len {
            ((v - v0) / a).max(0.0) + ((v - v1) / a).max(0.0) + (seg.len - d_acc - d_dec) / v
        } else {
            let v_peak = (0.5 * (2.0 * a * seg.len + v0 * v0 + v1 * v1))
                .max(v0 * v0)
                .max(v1 * v1)
                .sqrt();
            ((v_peak - v0) / a).max(0.0) + ((v_peak - v1) / a).max(0.0)
        };
        total += dt;
    }
    total
}

fn estimate_path_kinematic_time_s(
    layer_idx: usize,
    path: &ExtrusionPath,
    config: &PrintConfig,
) -> f64 {
    if path.points.len() < 2 {
        return 0.0;
    }
    let n = path.points.len();
    let seg_count = if path.closed { n } else { n.saturating_sub(1) };
    let mut pts_mm = Vec::with_capacity(seg_count + 1);
    for pt in &path.points {
        pts_mm.push(pt.to_mm());
    }
    if path.closed
        && let Some(&first) = path.points.first()
    {
        pts_mm.push(first.to_mm());
    }

    let max_vol_flow = config.filament.max_volumetric_speed_mm3_s;
    let fil_flow = config.filament.flow_ratio.max(0.001);
    let flow = path.flow_ratio.max(0.001) * fil_flow;
    let var_w = path.has_variable_width() && max_vol_flow.is_finite() && max_vol_flow > 0.0;
    let mut speeds = Vec::with_capacity(seg_count);
    for si in 0..seg_count {
        let s = if var_w {
            let sw = path.segment_width_mm(si);
            let sarea = bead_area_mm2(sw, path.height_mm) * flow;
            if sarea > 1e-9 {
                path.speed_mm_s.min(max_vol_flow / sarea)
            } else {
                path.speed_mm_s
            }
        } else {
            path.speed_mm_s
        };
        speeds.push(s.max(1e-3));
    }

    let accel = effective_role_accel_mm_s2(layer_idx, path.role, config);
    estimate_polyline_kinematic_time_s(&pts_mm, &speeds, accel, DEFAULT_JUNCTION_DEVIATION_MM)
}

fn apply_flow_and_cooling(
    layer_idx: usize,
    paths: &mut [ExtrusionPath],
    config: &PrintConfig,
) -> (u8, f64) {
    let max_vol_flow = config.filament.max_volumetric_speed_mm3_s;
    let travel_speed = config.machine.travel_speed_mm_s.max(1e-6);
    let travel_accel = effective_travel_accel_mm_s2(config);

    let mut extrusion_time_s = 0.0;
    let mut travel_time_s = 0.0;
    let mut prev_end: Option<Point2> = None;

    let fil_flow = config.filament.flow_ratio.max(0.001);
    for path in paths.iter_mut() {
        let min_w = path.min_width_mm();
        let eff_area = bead_area_mm2(min_w, path.height_mm) * path.flow_ratio.max(0.001) * fil_flow;
        if eff_area > 1e-9 && max_vol_flow.is_finite() && max_vol_flow > 0.0 {
            path.speed_mm_s = path.speed_mm_s.min(max_vol_flow / eff_area);
        }
        extrusion_time_s += estimate_path_kinematic_time_s(layer_idx, path, config);

        if let Some(&start) = path.points.first() {
            if let Some(prev) = prev_end {
                travel_time_s += estimate_trapezoid_move_time_s(
                    prev.distance_mm(start),
                    travel_speed,
                    travel_accel,
                );
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
            for _ in 0..3 {
                let cur_ext_time: f64 = paths
                    .iter()
                    .map(|p| estimate_path_kinematic_time_s(layer_idx, p, config))
                    .sum();
                if cur_ext_time >= needed_ext * 0.99 || cur_ext_time <= 1e-9 {
                    break;
                }
                let scale = (cur_ext_time / needed_ext).clamp(0.0, 0.999);
                let mut any_changed = false;
                for path in paths.iter_mut() {
                    let floor_speed = config
                        .filament
                        .slow_down_min_speed_mm_s
                        .min(path.speed_mm_s);
                    let next_s = (path.speed_mm_s * scale).max(floor_speed);
                    if (next_s - path.speed_mm_s).abs() > 1e-6 {
                        path.speed_mm_s = next_s;
                        any_changed = true;
                    }
                }
                if !any_changed {
                    break;
                }
            }
        }
    }

    let base_fan_u8 = fan_pct.round().clamp(0.0, 100.0) as u8;
    let boost_fan = config.filament.bridge_fan_percent;
    for path in paths.iter_mut() {
        if layer_idx >= disable_first
            && boost_fan > base_fan_u8
            && matches!(
                path.role,
                ExtrusionRole::Bridge | ExtrusionRole::InternalBridge | ExtrusionRole::OverhangWall
            )
        {
            path.fan_percent = Some(boost_fan);
        } else {
            path.fan_percent = None;
        }
    }

    let final_ext_time_s: f64 = paths
        .iter()
        .map(|p| estimate_path_kinematic_time_s(layer_idx, p, config))
        .sum();
    (base_fan_u8, final_ext_time_s + travel_time_s)
}

fn is_midair_bridge_segment(bridge_wall_mask: &MaskClipIndex, a: Point2, b: Point2) -> bool {
    let ax = a.x as f64;
    let ay = a.y as f64;
    let dx = (b.x as f64) - ax;
    let dy = (b.y as f64) - ay;
    for t in [0.25, 0.5, 0.75] {
        let pt = Point2::new((ax + t * dx).round() as i64, (ay + t * dy).round() as i64);
        if bridge_wall_mask.contains_point(pt) {
            return false;
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn flush_split_wall_segment(
    active_role: Option<(ExtrusionRole, f64, f64)>,
    active_pts: &mut Vec<Point2>,
    active_widths: &mut Vec<f64>,
    has_var_w: bool,
    base_width_mm: f64,
    height_mm: f64,
    out: &mut Vec<ExtrusionPath>,
) {
    if let Some((prev_role, prev_speed, prev_flow)) = active_role
        && active_pts.len() >= 2
    {
        let pts = std::mem::take(active_pts);
        let ws = std::mem::take(active_widths);
        let mean_w = if has_var_w && !ws.is_empty() {
            ws.iter().sum::<f64>() / (ws.len() as f64)
        } else {
            base_width_mm
        };
        let pw = if has_var_w && ws.len() == pts.len() {
            ws
        } else {
            Vec::new()
        };
        out.push(ExtrusionPath {
            role: prev_role,
            width_mm: mean_w,
            point_widths_mm: pw,
            height_mm,
            speed_mm_s: prev_speed,
            flow_ratio: prev_flow,
            points: pts,
            closed: false,
            object_id: None,
            fan_percent: None,
        });
    } else {
        active_pts.clear();
        active_widths.clear();
    }
}

struct SplitWallPiece {
    t: f64,
    role: ExtrusionRole,
    speed: f64,
    flow: f64,
    p0: Point2,
    w0: f64,
    p1: Point2,
    w1: f64,
}

struct ProgressiveOverhangMasks {
    m10: MaskClipIndex,
    m25: MaskClipIndex,
    m50: MaskClipIndex,
    m75: MaskClipIndex,
}

fn build_progressive_overhang_masks(
    below: &[ExPolygon],
    outer_w: f64,
) -> Result<ProgressiveOverhangMasks, SliceError> {
    let join = JoinType::Miter { limit: 2.0 };
    let make_idx = |delta_mm: f64| -> Result<MaskClipIndex, SliceError> {
        let polys = if delta_mm.abs() <= 1e-4 {
            below.to_vec()
        } else {
            offset(below, mm_to_nm(delta_mm)?, join)?
        };
        Ok(MaskClipIndex::from_regions(&polys))
    };
    Ok(ProgressiveOverhangMasks {
        m10: make_idx(-0.40 * outer_w + 0.015)?,
        m25: make_idx(-0.25 * outer_w + 0.015)?,
        m50: make_idx(0.015)?,
        m75: make_idx(0.25 * outer_w + 0.015)?,
    })
}

fn refine_subsegment_with_mask(
    segs: Vec<(Point2, Point2, bool)>,
    mask: &MaskClipIndex,
) -> Vec<(Point2, Point2, bool)> {
    let mut out = Vec::new();
    for (p0, p1, _prev_inside) in segs {
        for (sa, sb, inside) in mask.split_segment(p0, p1) {
            if sa.distance_mm(sb) > 1e-6 {
                out.push((sa, sb, inside));
            }
        }
    }
    out
}

struct ProgressiveOverhangContext<'a> {
    masks: ProgressiveOverhangMasks,
    outer_support: &'a MaskClipIndex,
    bridge_mask: &'a MaskClipIndex,
    bridge_wall_speed: f64,
}

fn split_outer_edge_progressive(
    a: Point2,
    b: Point2,
    nominal_speed: f64,
    nominal_flow: f64,
    ctx: &ProgressiveOverhangContext<'_>,
    config: &PrintConfig,
) -> Vec<(Point2, Point2, ExtrusionRole, f64, f64)> {
    let s1 = config.process.overhang_1_4_speed_mm_s;
    let s2 = config.process.overhang_2_4_speed_mm_s;
    let s3 = config.process.overhang_speed_mm_s;
    let s4 = config.process.overhang_4_4_speed_mm_s;
    let bridge_flow = config.process.bridge_flow_ratio;
    let prog = &ctx.masks;
    let outer_support_mask = ctx.outer_support;
    let bridge_mask_idx = ctx.bridge_mask;
    let bridge_wall_speed = ctx.bridge_wall_speed;

    let band4_speed = s4.unwrap_or(s3);
    let band2_speed = s2.unwrap_or_else(|| s1.unwrap_or(nominal_speed));
    let band1_speed = s1.unwrap_or(nominal_speed);

    let mut out = Vec::new();
    for (p0, p1, in_m10) in prog.m10.split_segment(a, b) {
        if p0.distance_mm(p1) <= 1e-6 {
            continue;
        }
        if in_m10 {
            out.push((
                p0,
                p1,
                ExtrusionRole::OuterWall,
                nominal_speed,
                nominal_flow,
            ));
            continue;
        }
        for (q0, q1, in_m25) in refine_subsegment_with_mask(vec![(p0, p1, false)], &prog.m25) {
            if in_m25 {
                let is_supported = outer_support_mask
                    .split_segment(q0, q1)
                    .into_iter()
                    .all(|(_, _, sup)| sup);
                let (role, spd) = if let Some(sp) = s1 {
                    (ExtrusionRole::OverhangWall, sp.min(nominal_speed))
                } else if !is_supported {
                    (ExtrusionRole::OverhangWall, s3)
                } else {
                    (ExtrusionRole::OuterWall, band1_speed)
                };
                out.push((q0, q1, role, spd, nominal_flow));
                continue;
            }
            for (r0, r1, in_m50) in refine_subsegment_with_mask(vec![(q0, q1, false)], &prog.m50) {
                if in_m50 {
                    let is_supported = outer_support_mask
                        .split_segment(r0, r1)
                        .into_iter()
                        .all(|(_, _, sup)| sup);
                    let (role, spd) = if let Some(sp) = s2 {
                        (ExtrusionRole::OverhangWall, sp.min(nominal_speed))
                    } else if !is_supported {
                        (ExtrusionRole::OverhangWall, s3)
                    } else {
                        (ExtrusionRole::OuterWall, band2_speed)
                    };
                    out.push((r0, r1, role, spd, nominal_flow));
                    continue;
                }
                for (u0, u1, in_m75) in
                    refine_subsegment_with_mask(vec![(r0, r1, false)], &prog.m75)
                {
                    if in_m75 {
                        out.push((
                            u0,
                            u1,
                            ExtrusionRole::OverhangWall,
                            s3.min(nominal_speed),
                            nominal_flow,
                        ));
                        continue;
                    }
                    for (v0, v1, in_bridge) in
                        refine_subsegment_with_mask(vec![(u0, u1, false)], bridge_mask_idx)
                    {
                        if in_bridge {
                            out.push((
                                v0,
                                v1,
                                ExtrusionRole::OverhangWall,
                                band4_speed.min(nominal_speed),
                                nominal_flow,
                            ));
                        } else {
                            out.push((
                                v0,
                                v1,
                                ExtrusionRole::OverhangWall,
                                bridge_wall_speed.min(band4_speed),
                                bridge_flow,
                            ));
                        }
                    }
                }
            }
        }
    }
    out
}

fn split_overhang_wall_paths(
    walls: &[ExtrusionPath],
    below: &[ExPolygon],
    layer_height_mm: f64,
    config: &PrintConfig,
) -> Result<Vec<ExtrusionPath>, SliceError> {
    let outer_margin_mm = layer_height_mm
        * config.process.overhang_threshold_deg.to_radians().tan()
        - 0.5 * config.process.outer_wall_width_mm
        + 0.015;
    let outer_support_mask = if outer_margin_mm.abs() <= 1e-4 {
        below.to_vec()
    } else {
        offset(
            below,
            mm_to_nm(outer_margin_mm)?,
            JoinType::Miter { limit: 2.0 },
        )?
    };
    let inner_support_mask = offset(below, mm_to_nm(0.015)?, JoinType::Miter { limit: 2.0 })?;
    let bridge_margin_mm = 0.5 * config.process.outer_wall_width_mm - 0.015;
    let bridge_wall_mask = if bridge_margin_mm.abs() <= 1e-4 {
        below.to_vec()
    } else {
        offset(
            below,
            mm_to_nm(bridge_margin_mm)?,
            JoinType::Miter { limit: 2.0 },
        )?
    };
    let bridge_wall_speed = config
        .process
        .overhang_speed_mm_s
        .min(config.process.bridge_speed_mm_s);
    let bridge_flow = config.process.bridge_flow_ratio;

    let outer_mask_idx = MaskClipIndex::from_regions(&outer_support_mask);
    let inner_mask_idx = MaskClipIndex::from_regions(&inner_support_mask);
    let bridge_mask_idx = MaskClipIndex::from_regions(&bridge_wall_mask);
    let has_progressive = config.process.overhang_1_4_speed_mm_s.is_some()
        || config.process.overhang_2_4_speed_mm_s.is_some()
        || config.process.overhang_4_4_speed_mm_s.is_some();
    let progressive_ctx = if has_progressive {
        Some(ProgressiveOverhangContext {
            masks: build_progressive_overhang_masks(below, config.process.outer_wall_width_mm)?,
            outer_support: &outer_mask_idx,
            bridge_mask: &bridge_mask_idx,
            bridge_wall_speed,
        })
    } else {
        None
    };

    let mut out = Vec::with_capacity(walls.len());
    for path in walls {
        if !matches!(
            path.role,
            ExtrusionRole::OuterWall | ExtrusionRole::InnerWall
        ) || !path.closed
            || path.points.len() < 3
        {
            out.push(path.clone());
            continue;
        }

        let is_inner = path.role == ExtrusionRole::InnerWall;
        let support_mask: &MaskClipIndex = if is_inner {
            &inner_mask_idx
        } else {
            &outer_mask_idx
        };
        let supported_role = path.role;

        let n = path.points.len();
        let has_var_w = path.has_variable_width();
        let mut active_role: Option<(ExtrusionRole, f64, f64)> = None;
        let mut active_pts: Vec<Point2> = Vec::new();
        let mut active_widths: Vec<f64> = Vec::new();
        let mut split_out: Vec<ExtrusionPath> = Vec::new();
        let mut any_overhang = false;

        for i in 0..n {
            let (Some(&a), Some(&b)) = (path.points.get(i), path.points.get((i + 1) % n)) else {
                continue;
            };
            if a == b {
                continue;
            }
            let wa = path.vertex_width_mm(i);
            let wb = path.vertex_width_mm((i + 1) % n);
            let (ax, ay) = a.to_mm();
            let (bx, by) = b.to_mm();
            let edx = bx - ax;
            let edy = by - ay;
            let elen2 = edx * edx + edy * edy;
            if elen2 <= 1e-12 {
                continue;
            }

            let mut edge_pieces: Vec<SplitWallPiece> = Vec::new();
            if !is_inner && let Some(prog_ctx) = progressive_ctx.as_ref() {
                for (sp0, sp1, role, speed, flow) in split_outer_edge_progressive(
                    a,
                    b,
                    path.speed_mm_s,
                    path.flow_ratio,
                    prog_ctx,
                    config,
                ) {
                    if role == ExtrusionRole::OverhangWall {
                        any_overhang = true;
                    }
                    let (p0x, p0y) = sp0.to_mm();
                    let (p1x, p1y) = sp1.to_mm();
                    let t0 = (((p0x - ax) * edx + (p0y - ay) * edy) / elen2).clamp(0.0, 1.0);
                    let t1 = (((p1x - ax) * edx + (p1y - ay) * edy) / elen2).clamp(0.0, 1.0);
                    let w0 = wa + t0 * (wb - wa);
                    let w1 = wa + t1 * (wb - wa);
                    let (ta, pa, w_pa, pb, w_pb) = if t1 < t0 {
                        (t1, sp1, w1, sp0, w0)
                    } else {
                        (t0, sp0, w0, sp1, w1)
                    };
                    edge_pieces.push(SplitWallPiece {
                        t: ta,
                        role,
                        speed,
                        flow,
                        p0: pa,
                        w0: w_pa,
                        p1: pb,
                        w1: w_pb,
                    });
                }
            } else {
                let split_pieces = support_mask.split_segment(a, b);
                edge_pieces.reserve(split_pieces.len());
                for (sp0, sp1, is_supported) in split_pieces {
                    if sp0.distance_mm(sp1) <= 1e-6 {
                        continue;
                    }
                    let (role, speed, flow) = if is_supported {
                        (supported_role, path.speed_mm_s, path.flow_ratio)
                    } else {
                        any_overhang = true;
                        if is_inner || is_midair_bridge_segment(&bridge_mask_idx, sp0, sp1) {
                            (ExtrusionRole::OverhangWall, bridge_wall_speed, bridge_flow)
                        } else {
                            (
                                ExtrusionRole::OverhangWall,
                                config.process.overhang_speed_mm_s,
                                path.flow_ratio,
                            )
                        }
                    };
                    let (p0x, p0y) = sp0.to_mm();
                    let (p1x, p1y) = sp1.to_mm();
                    let t0 = (((p0x - ax) * edx + (p0y - ay) * edy) / elen2).clamp(0.0, 1.0);
                    let t1 = (((p1x - ax) * edx + (p1y - ay) * edy) / elen2).clamp(0.0, 1.0);
                    let w0 = wa + t0 * (wb - wa);
                    let w1 = wa + t1 * (wb - wa);
                    let (ta, pa, w_pa, pb, w_pb) = if t1 < t0 {
                        (t1, sp1, w1, sp0, w0)
                    } else {
                        (t0, sp0, w0, sp1, w1)
                    };
                    edge_pieces.push(SplitWallPiece {
                        t: ta,
                        role,
                        speed,
                        flow,
                        p0: pa,
                        w0: w_pa,
                        p1: pb,
                        w1: w_pb,
                    });
                }
            }
            edge_pieces.sort_by(|x, y| x.t.total_cmp(&y.t));

            for piece in edge_pieces {
                let can_extend = active_role.is_some_and(|(r, s, f)| {
                    r == piece.role
                        && (s - piece.speed).abs() <= 1e-6
                        && (f - piece.flow).abs() <= 1e-6
                }) && active_pts
                    .last()
                    .is_some_and(|&lp| lp.distance_mm(piece.p0) <= 1e-3);
                if can_extend {
                    if active_pts
                        .last()
                        .is_none_or(|&lp| lp.distance_mm(piece.p0) > 1e-6)
                    {
                        active_pts.push(piece.p0);
                        active_widths.push(piece.w0);
                    }
                    active_pts.push(piece.p1);
                    active_widths.push(piece.w1);
                } else {
                    flush_split_wall_segment(
                        active_role,
                        &mut active_pts,
                        &mut active_widths,
                        has_var_w,
                        path.width_mm,
                        path.height_mm,
                        &mut split_out,
                    );
                    active_role = Some((piece.role, piece.speed, piece.flow));
                    active_pts = vec![piece.p0, piece.p1];
                    active_widths = vec![piece.w0, piece.w1];
                }
            }
        }

        if !any_overhang {
            out.push(path.clone());
            continue;
        }
        flush_split_wall_segment(
            active_role,
            &mut active_pts,
            &mut active_widths,
            has_var_w,
            path.width_mm,
            path.height_mm,
            &mut split_out,
        );
        out.extend(split_out);
    }

    Ok(out)
}

fn polygon_centroid(poly: &Polygon) -> (f64, f64) {
    let n = poly.points.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    let mut area2 = 0.0;
    let mut cx = 0.0;
    let mut cy = 0.0;
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    for i in 0..n {
        let (Some(&p0), Some(&p1)) = (poly.points.get(i), poly.points.get((i + 1) % n)) else {
            continue;
        };
        let (x0, y0) = p0.to_mm();
        let (x1, y1) = p1.to_mm();
        sum_x += x0;
        sum_y += y0;
        let cross = x0 * y1 - x1 * y0;
        area2 += cross;
        cx += (x0 + x1) * cross;
        cy += (y0 + y1) * cross;
    }
    if area2.abs() > 1e-9 {
        (cx / (3.0 * area2), cy / (3.0 * area2))
    } else {
        let nf = n as f64;
        (sum_x / nf, sum_y / nf)
    }
}

fn compute_bridge_angle(bridge: &ExPolygon, below: &[ExPolygon]) -> Result<f64, SliceError> {
    let expanded = offset(
        std::slice::from_ref(bridge),
        mm_to_nm(1.0)?,
        JoinType::Miter { limit: 2.0 },
    )?;
    let mut anchors = intersection(&expanded, below)?;
    if anchors.len() >= 2 {
        anchors.sort_by(|a, b| b.area_mm2().total_cmp(&a.area_mm2()));
        if let (Some(a0), Some(a1)) = (anchors.first(), anchors.get(1)) {
            let c0 = polygon_centroid(&a0.outer);
            let c1 = polygon_centroid(&a1.outer);
            let dx = c1.0 - c0.0;
            let dy = c1.1 - c0.1;
            if dx.hypot(dy) > 1e-6 {
                return Ok(dy.atan2(dx));
            }
        }
    }

    let mut best_angle = 0.0;
    let mut best_span = f64::INFINITY;
    for step in 0..12 {
        let angle = (step as f64) * (PI / 12.0);
        let ca = angle.cos();
        let sa = angle.sin();
        let mut u_min = f64::INFINITY;
        let mut u_max = f64::NEG_INFINITY;
        for pt in &bridge.outer.points {
            let (px, py) = pt.to_mm();
            let u = px * ca + py * sa;
            u_min = u_min.min(u);
            u_max = u_max.max(u);
        }
        let span = u_max - u_min;
        if span < best_span {
            best_span = span;
            best_angle = angle;
        }
    }
    Ok(best_angle)
}

pub(crate) fn bed_clamp_expolygon(config: &PrintConfig) -> Result<ExPolygon, SliceError> {
    let bw = config.machine.bed_width_mm;
    let bd = config.machine.bed_depth_mm;
    let pad = if bw > 2.0 && bd > 2.0 { 0.5 } else { 0.0 };
    let outer = Polygon::new(vec![
        Point2::from_mm(pad, pad)?,
        Point2::from_mm(bw - pad, pad)?,
        Point2::from_mm(bw - pad, bd - pad)?,
        Point2::from_mm(pad, bd - pad)?,
    ]);
    Ok(ExPolygon::new(outer, Vec::new()))
}

pub(crate) fn find_support_contact_layer(
    raw_layers: &[RawSliceLayer],
    overhang_idx: usize,
    gap_mm: f64,
) -> Option<usize> {
    if overhang_idx == 0 {
        return None;
    }
    let ovh_layer = raw_layers.get(overhang_idx)?;
    let z_ovh_bot = ovh_layer.plane.z_top_mm - ovh_layer.plane.height_mm;
    if gap_mm <= 1e-6 {
        return Some(overhang_idx - 1);
    }

    let mut best_diff = (z_ovh_bot - gap_mm).abs();
    let mut best_gap = z_ovh_bot;
    let mut best_idx: Option<usize> = None;

    for j in (0..overhang_idx).rev() {
        let Some(cand) = raw_layers.get(j) else {
            continue;
        };
        let gap = (z_ovh_bot - cand.plane.z_top_mm).max(0.0);
        let diff = (gap - gap_mm).abs();
        if diff + 1e-9 < best_diff || ((diff - best_diff).abs() <= 1e-9 && gap > best_gap) {
            best_diff = diff;
            best_gap = gap;
            best_idx = Some(j);
        }
        if gap > gap_mm && diff > best_diff + 1e-6 {
            break;
        }
    }
    best_idx
}

pub(crate) fn bottom_gap_exclusion_range(
    raw_layers: &[RawSliceLayer],
    idx: usize,
    gap_mm: f64,
) -> std::ops::Range<usize> {
    if idx == 0 || gap_mm <= 1e-6 {
        return 0..0;
    }
    let start = match find_support_contact_layer(raw_layers, idx, gap_mm) {
        Some(keep_below) => keep_below + 1,
        None => 0,
    };
    start..idx
}

pub(crate) fn detect_contact_overhangs(
    raw_layers: &[RawSliceLayer],
    bed_slice: &[ExPolygon],
    config: &PrintConfig,
    blocker_masks: &[Vec<ExPolygon>],
    enforcer_masks: &[Vec<ExPolygon>],
) -> Result<Vec<Vec<ExPolygon>>, SliceError> {
    let n = raw_layers.len();
    let s = &config.process.support;
    let join = JoinType::Miter { limit: 2.0 };
    let mut contact_overhangs: Vec<Vec<ExPolygon>> = vec![Vec::new(); n];

    for idx in 1..n {
        let Some(here_layer) = raw_layers.get(idx) else {
            continue;
        };
        let Some(below_layer) = raw_layers.get(idx - 1) else {
            continue;
        };
        if here_layer.regions.is_empty() {
            continue;
        }
        let h = here_layer.plane.height_mm;
        let mut ovh = if s.enabled {
            let dx_safe = h * s.threshold_angle_deg.to_radians().tan();
            let supported = offset(&below_layer.regions, mm_to_nm(dx_safe)?, join)?;
            difference(&here_layer.regions, &supported)?
        } else {
            Vec::new()
        };

        let enf_here = enforcer_masks.get(idx).map(Vec::as_slice).unwrap_or(&[]);
        let enf_below = enforcer_masks
            .get(idx - 1)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if !enf_here.is_empty() || !enf_below.is_empty() {
            let raw_unsupported = difference(&here_layer.regions, &below_layer.regions)?;
            if !raw_unsupported.is_empty() {
                let combined_enf = if enf_here.is_empty() {
                    enf_below.to_vec()
                } else if enf_below.is_empty() {
                    enf_here.to_vec()
                } else {
                    union(&[enf_here, enf_below].concat())?
                };
                let forced = intersection(&raw_unsupported, &combined_enf)?;
                if !forced.is_empty() {
                    if ovh.is_empty() {
                        ovh = forced;
                    } else {
                        ovh.extend(forced);
                        ovh = union(&ovh)?;
                    }
                }
            }
        }

        let blocker_here = blocker_masks.get(idx).map(Vec::as_slice).unwrap_or(&[]);
        if !blocker_here.is_empty() && !ovh.is_empty() {
            ovh = difference(&ovh, blocker_here)?;
        }
        if ovh.is_empty() {
            continue;
        }

        if s.style == SupportStyle::Grid {
            let expanded = offset(&ovh, mm_to_nm(0.5 * s.line_width_mm)?, join)?;
            ovh = intersection(&expanded, bed_slice)?;
        }

        if let Some(target_idx) = find_support_contact_layer(raw_layers, idx, s.top_z_distance_mm) {
            let blocker_target = blocker_masks
                .get(target_idx)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if !blocker_target.is_empty() {
                ovh = difference(&ovh, blocker_target)?;
            }
            if !ovh.is_empty()
                && let Some(slot) = contact_overhangs.get_mut(target_idx)
            {
                slot.extend(ovh);
            }
        }
    }

    for slot in &mut contact_overhangs {
        if slot.len() > 1 {
            *slot = union(slot)?;
        }
    }

    Ok(contact_overhangs)
}

fn generate_support_paths(
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
    if s.style == SupportStyle::Tree {
        return tree_support::generate_tree_support_paths(
            raw_layers,
            config,
            blocker_masks,
            enforcer_masks,
        );
    }

    let join = JoinType::Miter { limit: 2.0 };
    let bed_box = bed_clamp_expolygon(config)?;
    let bed_slice = std::slice::from_ref(&bed_box);

    // Pass 1: Overhang detection & top Z contact mapping.
    let contact_overhangs =
        detect_contact_overhangs(raw_layers, bed_slice, config, blocker_masks, enforcer_masks)?;

    // Pass 2: Top-down column projection with XY & bottom-Z model clearance.
    let mut support_volume: Vec<Vec<ExPolygon>> = vec![Vec::new(); n];
    let mut carried: Vec<ExPolygon> = Vec::new();

    for idx in (0..n).rev() {
        let mut combined = contact_overhangs.get(idx).cloned().unwrap_or_default();
        combined.extend(carried.iter().cloned());
        if combined.is_empty() {
            carried.clear();
            continue;
        }
        let combined = union(&combined)?;

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
        let clipped = if excl_sources.is_empty() {
            combined
        } else {
            let excl = union(&excl_sources)?;
            difference(&combined, &excl)?
        };
        let mut clamped = intersection(&clipped, bed_slice)?;
        clamped.retain(|ex| ex.area_mm2() >= 0.05);

        carried = clamped.clone();
        if let Some(slot) = support_volume.get_mut(idx) {
            *slot = clamped;
        }
    }

    // Pass 3: Optional build-plate-only reachability filter.
    if s.on_build_plate_only {
        let mut reachable = support_volume.first().cloned().unwrap_or_default();
        for idx in 1..n {
            if let Some(vol) = support_volume.get_mut(idx) {
                if reachable.is_empty() || vol.is_empty() {
                    vol.clear();
                    reachable.clear();
                } else {
                    reachable = intersection(vol, &reachable)?;
                    *vol = reachable.clone();
                }
            }
        }
    }

    // Pass 4: Interface vs base splitting & hatching.
    let iface = s.interface_layers as usize;
    let mut bottom_contacts: Vec<Vec<ExPolygon>> = vec![Vec::new(); n];
    if iface > 0 {
        for j in 1..n {
            let (Some(cur), Some(prev)) = (support_volume.get(j), support_volume.get(j - 1)) else {
                continue;
            };
            if !cur.is_empty() {
                let bc = difference(cur, prev)?;
                if let Some(slot) = bottom_contacts.get_mut(j) {
                    *slot = bc;
                }
            }
        }
    }

    for idx in 0..n {
        let Some(vol) = support_volume.get(idx) else {
            continue;
        };
        if vol.is_empty() {
            continue;
        }
        let h = raw_layers
            .get(idx)
            .map(|l| l.plane.height_mm)
            .unwrap_or(config.process.layer_height_mm);

        let interface_here = if iface > 0 {
            let mut iface_sources: Vec<ExPolygon> = Vec::new();
            let top_end = (idx + iface - 1).min(n.saturating_sub(1));
            for j in idx..=top_end {
                if let Some(co) = contact_overhangs.get(j) {
                    iface_sources.extend(co.iter().cloned());
                }
            }
            if idx >= 1 {
                let bot_start = idx.saturating_sub(iface - 1).max(1);
                for j in bot_start..=idx {
                    if let Some(bc) = bottom_contacts.get(j) {
                        iface_sources.extend(bc.iter().cloned());
                    }
                }
            }
            if iface_sources.is_empty() {
                Vec::new()
            } else {
                intersection(vol, &union(&iface_sources)?)?
            }
        } else {
            Vec::new()
        };

        let base_here = if interface_here.is_empty() {
            vol.clone()
        } else {
            difference(vol, &interface_here)?
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
            match s.style {
                SupportStyle::Grid => {
                    for angle in [0.0, FRAC_PI_2] {
                        let hatched = generate_rectilinear_hatch(
                            &base_here,
                            angle,
                            s.base_pattern_spacing_mm,
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
                SupportStyle::Snug | SupportStyle::Tree => {
                    let angle = if idx % 2 == 0 { 0.0 } else { FRAC_PI_2 };
                    let hatched = generate_rectilinear_hatch(
                        &base_here,
                        angle,
                        s.base_pattern_spacing_mm,
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
            let dense_floor = stadium_spacing_mm(s.line_width_mm, h);
            let iface_spacing = if s.interface_spacing_mm <= 1e-6 {
                dense_floor
            } else {
                (s.line_width_mm + s.interface_spacing_mm).max(dense_floor)
            };
            let angle = if idx % 2 == 0 { FRAC_PI_4 } else { -FRAC_PI_4 };
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

fn region_aabb(region: &[ExPolygon]) -> Option<Aabb2> {
    let mut bounds: Option<Aabb2> = None;
    for ex in region {
        if let Some(bb) = ex.aabb() {
            bounds = Some(match bounds {
                Some(cur) => cur.union(bb),
                None => bb,
            });
        }
    }
    bounds
}

fn segment_inside_same_region(
    region: &[ExPolygon],
    a: Point2,
    b: Point2,
    check_endpoints: bool,
) -> bool {
    if a == b {
        return true;
    }
    let ax = a.x as f64;
    let ay = a.y as f64;
    let dx = (b.x as f64) - ax;
    let dy = (b.y as f64) - ay;
    let (start_step, end_step) = if check_endpoints { (0, 6) } else { (1, 5) };
    for ex in region {
        let mut all_in = true;
        for step in start_step..=end_step {
            let t = (step as f64) / 6.0;
            let px = (ax + t * dx).round() as i64;
            let py = (ay + t * dy).round() as i64;
            if !ex.contains_point(Point2::new(px, py)) {
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

fn generate_gyroid_infill(
    region: &[ExPolygon],
    z_mm: f64,
    density: f64,
    width_mm: f64,
    height_mm: f64,
    speed_mm_s: f64,
) -> Result<Vec<ExtrusionPath>, SliceError> {
    if region.is_empty() || density <= 1e-6 {
        return Ok(Vec::new());
    }
    let Some(aabb) = region_aabb(region) else {
        return Ok(Vec::new());
    };
    let (min_x, min_y) = aabb.min.to_mm();
    let (max_x, max_y) = aabb.max.to_mm();

    let l = (2.44 * width_mm / density).max(2.0 * width_mm);
    let k = TAU / l;
    let kz = k * z_mm;
    let cos_z = kz.cos();
    let sin_z = kz.sin();

    let x0 = min_x - l;
    let x1 = max_x + l;
    let y0 = min_y - l;
    let y1 = max_y + l;

    let ds = (l / 16.0).clamp(0.25, 1.2);
    let mut raw_waves: Vec<Vec<Point2>> = Vec::new();

    if cos_z.abs() >= sin_z.abs() {
        let sign = if cos_z >= 0.0 { 1.0 } else { -1.0 };
        let a = sign * cos_z;
        let samples = (((x1 - x0) / ds).ceil() as usize + 1).clamp(2, 2000);
        let dx = (x1 - x0) / ((samples - 1) as f64);

        let c_start = ((k * y0) / TAU).floor() as i64 - 1;
        let c_end = ((k * y1) / TAU).ceil() as i64 + 1;
        let cell_count = c_end.saturating_sub(c_start).clamp(0, 1000);

        for c_off in 0..=cell_count {
            let cell = c_start + c_off;
            for odd in [false, true] {
                let mut wave = Vec::with_capacity(samples);
                for s in 0..samples {
                    let x = x0 + (s as f64) * dx;
                    let kx = k * x;
                    let b = sign * kx.sin();
                    let c = -sign * sin_z * kx.cos();
                    let r = a.hypot(b);
                    let alpha = b.atan2(a);
                    let beta = (c / r).clamp(-1.0, 1.0).asin();
                    let ky =
                        TAU * (cell as f64) + if odd { PI - beta - alpha } else { beta - alpha };
                    wave.push(Point2::from_mm(x, ky / k)?);
                }
                raw_waves.push(wave);
            }
        }
    } else {
        let sign = if sin_z >= 0.0 { 1.0 } else { -1.0 };
        let a = sign * sin_z;
        let samples = (((y1 - y0) / ds).ceil() as usize + 1).clamp(2, 2000);
        let dy = (y1 - y0) / ((samples - 1) as f64);

        let c_start = ((k * x0) / TAU).floor() as i64 - 1;
        let c_end = ((k * x1) / TAU).ceil() as i64 + 1;
        let cell_count = c_end.saturating_sub(c_start).clamp(0, 1000);

        for c_off in 0..=cell_count {
            let cell = c_start + c_off;
            for odd in [false, true] {
                let mut wave = Vec::with_capacity(samples);
                for s in 0..samples {
                    let y = y0 + (s as f64) * dy;
                    let ky = k * y;
                    let b = sign * ky.cos();
                    let c = -sign * cos_z * ky.sin();
                    let r = a.hypot(b);
                    let gamma = b.atan2(a);
                    let delta = (c / r).clamp(-1.0, 1.0).acos();
                    let kx = TAU * (cell as f64) + if odd { gamma - delta } else { gamma + delta };
                    wave.push(Point2::from_mm(kx / k, y)?);
                }
                raw_waves.push(wave);
            }
        }
    }

    let clipped = clip_polylines(&raw_waves, region)?;
    let mut waves: Vec<Vec<Point2>> = clipped.into_iter().filter(|pts| pts.len() >= 2).collect();
    if waves.is_empty() {
        return Ok(Vec::new());
    }

    let chain_region_owned = match offset(region, mm_to_nm(0.02)?, JoinType::Miter { limit: 2.0 }) {
        Ok(expanded) if !expanded.is_empty() => Some(expanded),
        _ => None,
    };
    let chain_region: &[ExPolygon] = chain_region_owned.as_deref().unwrap_or(region);
    let check_endpoints = chain_region_owned.is_some();

    let max_link_mm = (1.15 * l).max(2.0 * width_mm);
    let mut start_idx = 0usize;
    let mut start_rev = false;
    let mut start_dist = f64::INFINITY;
    for (idx, w) in waves.iter().enumerate() {
        if let Some(&s) = w.first() {
            let d = aabb.min.distance_mm(s);
            if d < start_dist {
                start_dist = d;
                start_idx = idx;
                start_rev = false;
            }
        }
        if let Some(&e) = w.last() {
            let d = aabb.min.distance_mm(e);
            if d < start_dist {
                start_dist = d;
                start_idx = idx;
                start_rev = true;
            }
        }
    }
    let mut cur_pts = waves.remove(start_idx);
    if start_rev {
        cur_pts.reverse();
    }
    let mut paths = Vec::new();
    while !waves.is_empty() {
        let Some(&last_pt) = cur_pts.last() else {
            cur_pts = waves.remove(0);
            continue;
        };
        let mut best_chain: Option<(usize, bool, f64)> = None;
        let mut best_jump: Option<(usize, bool, f64)> = None;
        for (idx, w) in waves.iter().enumerate() {
            for (rev, pt_opt) in [(false, w.first().copied()), (true, w.last().copied())] {
                let Some(pt) = pt_opt else {
                    continue;
                };
                let d = last_pt.distance_mm(pt);
                if best_jump.is_none_or(|(_, _, bd)| d < bd) {
                    best_jump = Some((idx, rev, d));
                }
                if d <= max_link_mm
                    && best_chain.is_none_or(|(_, _, bd)| d < bd)
                    && segment_inside_same_region(chain_region, last_pt, pt, check_endpoints)
                {
                    best_chain = Some((idx, rev, d));
                }
            }
        }
        if let Some((idx, rev, _)) = best_chain {
            let mut chosen = waves.remove(idx);
            if rev {
                chosen.reverse();
            }
            if let Some(&first_pt) = chosen.first() {
                if first_pt != last_pt {
                    cur_pts.push(first_pt);
                }
                cur_pts.extend(chosen.into_iter().skip(1));
            }
        } else {
            paths.push(ExtrusionPath {
                role: ExtrusionRole::SparseInfill,
                width_mm,
                point_widths_mm: Vec::new(),
                height_mm,
                speed_mm_s,
                flow_ratio: 1.0,
                points: std::mem::take(&mut cur_pts),
                closed: false,
                object_id: None,
                fan_percent: None,
            });
            if let Some((idx, rev, _)) = best_jump {
                let mut chosen = waves.remove(idx);
                if rev {
                    chosen.reverse();
                }
                cur_pts = chosen;
            } else {
                cur_pts = waves.remove(0);
            }
        }
    }
    if cur_pts.len() >= 2 {
        paths.push(ExtrusionPath {
            role: ExtrusionRole::SparseInfill,
            width_mm,
            point_widths_mm: Vec::new(),
            height_mm,
            speed_mm_s,
            flow_ratio: 1.0,
            points: cur_pts,
            closed: false,
            object_id: None,
            fan_percent: None,
        });
    }
    Ok(paths)
}

fn build_perimeters(
    regions: &[ExPolygon],
    layer_idx: usize,
    layer_height_mm: f64,
    config: &PrintConfig,
) -> Result<(Vec<ExtrusionPath>, Vec<ExPolygon>), SliceError> {
    match config.process.wall_generator {
        WallGenerator::Classic => {
            build_perimeters_classic(regions, layer_idx, layer_height_mm, config)
        }
        WallGenerator::Arachne => {
            arachne::build_perimeters_arachne(regions, layer_idx, layer_height_mm, config)
        }
    }
}

pub(crate) fn build_perimeters_classic(
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
    let mut infill_area = if true_interior.is_empty() {
        Vec::new()
    } else {
        let infill_overlap =
            config.process.infill_wall_overlap_ratio * config.process.solid_infill_width_mm;
        offset(&true_interior, mm_to_nm(infill_overlap)?, join)?
    };

    // Emit perimeter walls according to configured wall order.
    let mut paths = Vec::new();
    for ring_idx in wall_order_indices(rings.len(), config.process.wall_order) {
        if let Some((role, width_mm, speed_mm_s, poly_group)) = rings.get(ring_idx) {
            for ex in poly_group {
                push_closed_poly(
                    &ex.outer,
                    *role,
                    *width_mm,
                    layer_height_mm,
                    *speed_mm_s,
                    &mut paths,
                );
                for hole in &ex.holes {
                    push_closed_poly(
                        hole,
                        *role,
                        *width_mm,
                        layer_height_mm,
                        *speed_mm_s,
                        &mut paths,
                    );
                }
            }
        }
    }

    if config.process.gap_fill_enabled {
        let ribbon_join = JoinType::Miter { limit: 1.45 };
        let mut wall_ribbons = Vec::new();
        for (_, w_mm, _, poly_group) in &rings {
            let half_w = 0.5 * *w_mm;
            let outer_env = offset(poly_group, mm_to_nm(half_w)?, ribbon_join)?;
            let inner_void = offset(poly_group, -mm_to_nm(half_w)?, ribbon_join)?;
            wall_ribbons.extend(difference(&outer_env, &inner_void)?);
        }
        let covered_by_walls = union_if_multi(wall_ribbons)?;
        let uncovered = difference(regions, &covered_by_walls)?;
        if !uncovered.is_empty() {
            let solid_w = config.process.solid_infill_width_mm;
            let wide_eroded = offset(&infill_area, -mm_to_nm(0.5 * solid_w)?, join)?;
            let wide_infill = if wide_eroded.is_empty() {
                Vec::new()
            } else {
                offset(&wide_eroded, mm_to_nm(0.5 * solid_w)?, join)?
            };
            let gap_regions = if wide_infill.is_empty() {
                uncovered
            } else {
                difference(&uncovered, &wide_infill)?
            };
            if !gap_regions.is_empty() {
                let gap_speed = config
                    .process
                    .gap_fill_speed_mm_s
                    .unwrap_or((0.6 * inner_speed).max(10.0));
                let gap_paths = arachne::extract_gap_fill_paths(
                    &gap_regions,
                    &covered_by_walls,
                    regions,
                    config.process.gap_fill_min_width_mm,
                    1.8 * inner_w,
                    layer_height_mm,
                    gap_speed,
                )?;
                if !gap_paths.is_empty() {
                    infill_area = difference(&infill_area, &gap_regions)?;
                    paths.extend(gap_paths);
                }
            }
        }
    }

    Ok((paths, infill_area))
}

pub(crate) fn wall_order_indices(ring_count: usize, order: WallOrder) -> Vec<usize> {
    match order {
        WallOrder::InnerOuter => (0..ring_count).rev().collect(),
        WallOrder::OuterInner => (0..ring_count).collect(),
        WallOrder::InnerOuterInner => {
            if ring_count >= 2 {
                let mut order_vec = Vec::with_capacity(ring_count);
                order_vec.push(1);
                order_vec.push(0);
                order_vec.extend(2..ring_count);
                order_vec
            } else {
                (0..ring_count).collect()
            }
        }
    }
}

fn apply_xy_compensations(
    regions: &[ExPolygon],
    contour_delta_mm: f64,
    hole_delta_mm: f64,
    elephant_foot_mm: f64,
) -> Result<Vec<ExPolygon>, SliceError> {
    if regions.is_empty()
        || (contour_delta_mm.abs() <= 1e-6
            && hole_delta_mm.abs() <= 1e-6
            && elephant_foot_mm.abs() <= 1e-6)
    {
        return Ok(regions.to_vec());
    }
    let join = JoinType::Miter { limit: 2.0 };
    let outer_delta_mm = contour_delta_mm - elephant_foot_mm;
    let hole_expand_mm = hole_delta_mm + elephant_foot_mm;
    let mut out = Vec::with_capacity(regions.len());

    for ex in regions {
        let outer_only = vec![ExPolygon::new(ex.outer.clone(), Vec::new())];
        let outer_comp = if outer_delta_mm.abs() <= 1e-6 {
            outer_only
        } else {
            offset(&outer_only, mm_to_nm(outer_delta_mm)?, join)?
        };
        if outer_comp.is_empty() {
            continue;
        }
        if ex.holes.is_empty() {
            out.extend(outer_comp);
            continue;
        }
        let mut hole_polys = Vec::with_capacity(ex.holes.len());
        for hole in &ex.holes {
            let h_ex = [ExPolygon::new(hole.clone(), Vec::new())];
            if hole_expand_mm.abs() <= 1e-6 {
                hole_polys.extend(h_ex);
            } else {
                hole_polys.extend(offset(&h_ex, mm_to_nm(hole_expand_mm)?, join)?);
            }
        }
        if hole_polys.is_empty() {
            out.extend(outer_comp);
        } else {
            out.extend(difference(&outer_comp, &hole_polys)?);
        }
    }

    union_if_multi(out)
}

fn generate_concentric_fill(
    region: &[ExPolygon],
    role: ExtrusionRole,
    width_mm: f64,
    height_mm: f64,
    speed_mm_s: f64,
    flow_ratio: f64,
) -> Result<Vec<ExtrusionPath>, SliceError> {
    if region.is_empty() || width_mm <= 0.0 || height_mm <= 0.0 {
        return Ok(Vec::new());
    }
    let spacing_mm = stadium_spacing_mm(width_mm, height_mm);
    let join = JoinType::Miter { limit: 2.0 };
    let mut inset_mm = 0.5 * width_mm;
    let mut current = offset(region, -mm_to_nm(inset_mm)?, join)?;
    if current.is_empty() {
        inset_mm = 0.25 * width_mm;
        current = offset(region, -mm_to_nm(inset_mm)?, join)?;
    }

    let mut out = Vec::new();
    for _ in 0..2000 {
        if current.is_empty() {
            break;
        }
        for ex in current {
            if ex.outer.points.len() >= 3 {
                out.push(ExtrusionPath {
                    role,
                    width_mm,
                    point_widths_mm: Vec::new(),
                    height_mm,
                    speed_mm_s,
                    flow_ratio,
                    points: ex.outer.points,
                    closed: true,
                    object_id: None,
                    fan_percent: None,
                });
            }
            for hole in ex.holes {
                if hole.points.len() >= 3 {
                    out.push(ExtrusionPath {
                        role,
                        width_mm,
                        point_widths_mm: Vec::new(),
                        height_mm,
                        speed_mm_s,
                        flow_ratio,
                        points: hole.points,
                        closed: true,
                        object_id: None,
                        fan_percent: None,
                    });
                }
            }
        }
        inset_mm += spacing_mm;
        current = offset(region, -mm_to_nm(inset_mm)?, join)?;
    }
    Ok(out)
}

fn path_start_end_mm(path: &ExtrusionPath) -> Option<((f64, f64), (f64, f64))> {
    let first = path.points.first()?.to_mm();
    let last = if path.closed {
        first
    } else {
        path.points.last()?.to_mm()
    };
    Some((first, last))
}

fn nearest_island_idx(
    path: &ExtrusionPath,
    regions: &[ExPolygon],
    centers: &[(f64, f64)],
) -> usize {
    if regions.len() <= 1 || path.points.is_empty() {
        return 0;
    }
    let mut samples = Vec::with_capacity(4);
    if let Some(&first) = path.points.first() {
        samples.push(first);
    }
    if let Some(&mid) = path.points.get(path.points.len() / 2) {
        samples.push(mid);
    }
    if let (Some(&p0), Some(&p1)) = (path.points.first(), path.points.get(1)) {
        samples.push(Point2::new((p0.x + p1.x) / 2, (p0.y + p1.y) / 2));
    }
    for pt in &samples {
        if let Some(idx) = regions.iter().position(|ex| ex.contains_point(*pt)) {
            return idx;
        }
    }
    let ref_mm = samples.first().map(|p| p.to_mm()).unwrap_or((0.0, 0.0));
    centers
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            let da = (a.0 - ref_mm.0).hypot(a.1 - ref_mm.1);
            let db = (b.0 - ref_mm.0).hypot(b.1 - ref_mm.1);
            da.total_cmp(&db)
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn greedy_order_paths(mut pool: Vec<ExtrusionPath>, cursor: &mut (f64, f64)) -> Vec<ExtrusionPath> {
    let mut out = Vec::with_capacity(pool.len());
    while !pool.is_empty() {
        let mut best_idx = 0usize;
        let mut best_rev = false;
        let mut best_dist = f64::INFINITY;
        for (i, p) in pool.iter().enumerate() {
            let Some((s, e)) = path_start_end_mm(p) else {
                continue;
            };
            let ds = (s.0 - cursor.0).hypot(s.1 - cursor.1);
            if ds < best_dist {
                best_dist = ds;
                best_idx = i;
                best_rev = false;
            }
            if !p.closed {
                let de = (e.0 - cursor.0).hypot(e.1 - cursor.1);
                if de < best_dist {
                    best_dist = de;
                    best_idx = i;
                    best_rev = true;
                }
            }
        }
        let mut chosen = pool.swap_remove(best_idx);
        if best_rev && !chosen.closed {
            chosen.points.reverse();
            if !chosen.point_widths_mm.is_empty() {
                chosen.point_widths_mm.reverse();
            }
        }
        if let Some((_, end)) = path_start_end_mm(&chosen) {
            *cursor = end;
        }
        out.push(chosen);
    }
    out
}

#[derive(Default)]
struct IslandPathBucket {
    walls: Vec<ExtrusionPath>,
    gap_fill: Vec<ExtrusionPath>,
    surfaces: Vec<ExtrusionPath>,
    internal_solid: Vec<ExtrusionPath>,
    bridges: Vec<ExtrusionPath>,
    sparse: Vec<ExtrusionPath>,
    ironing: Vec<ExtrusionPath>,
}

impl IslandPathBucket {
    fn supports_infill_first(&self, layer_idx: usize, config: &PrintConfig) -> bool {
        layer_idx > 0
            && !layer_idx.is_multiple_of(2)
            && config.process.wall_loops >= 2
            && config.process.wall_order != WallOrder::OuterInner
            && !self.sparse.is_empty()
            && !self.walls.is_empty()
            && self.gap_fill.is_empty()
            && self.surfaces.is_empty()
            && self.bridges.is_empty()
            && self.internal_solid.is_empty()
            && self.ironing.is_empty()
            && !self
                .walls
                .iter()
                .any(|p| p.role == ExtrusionRole::OverhangWall)
    }

    fn first_anchor_mm(&self, infill_first: bool) -> Option<(f64, f64)> {
        let first = if infill_first {
            self.sparse
                .first()
                .or_else(|| self.walls.first())
                .or_else(|| self.gap_fill.first())
                .or_else(|| self.surfaces.first())
                .or_else(|| self.internal_solid.first())
                .or_else(|| self.bridges.first())
                .or_else(|| self.ironing.first())
        } else {
            self.walls
                .first()
                .or_else(|| self.gap_fill.first())
                .or_else(|| self.surfaces.first())
                .or_else(|| self.internal_solid.first())
                .or_else(|| self.bridges.first())
                .or_else(|| self.sparse.first())
                .or_else(|| self.ironing.first())
        };
        first.and_then(|p| path_start_end_mm(p).map(|(s, _)| s))
    }

    fn is_empty(&self) -> bool {
        self.walls.is_empty()
            && self.gap_fill.is_empty()
            && self.surfaces.is_empty()
            && self.internal_solid.is_empty()
            && self.bridges.is_empty()
            && self.sparse.is_empty()
            && self.ironing.is_empty()
    }
}

fn append_ordered_bucket_group(
    group: Vec<ExtrusionPath>,
    cursor_xy: &mut (f64, f64),
    out: &mut Vec<ExtrusionPath>,
) {
    for p in group {
        if let Some((_, end)) = path_start_end_mm(&p) {
            *cursor_xy = end;
        }
        out.push(p);
    }
}

fn order_layer_paths_by_island(
    layer_idx: usize,
    paths: Vec<ExtrusionPath>,
    slice_regions: &[ExPolygon],
    cursor_xy: &mut (f64, f64),
    config: &PrintConfig,
) -> Vec<ExtrusionPath> {
    if paths.is_empty() {
        return paths;
    }
    let island_count = slice_regions.len().max(1);
    let centers: Vec<(f64, f64)> = slice_regions
        .iter()
        .map(|ex| {
            ex.aabb()
                .map(|bb| {
                    let (x0, y0) = bb.min.to_mm();
                    let (x1, y1) = bb.max.to_mm();
                    (0.5 * (x0 + x1), 0.5 * (y0 + y1))
                })
                .unwrap_or((0.0, 0.0))
        })
        .collect();

    let mut adhesion = Vec::new();
    let mut support_base = Vec::new();
    let mut support_iface = Vec::new();
    let mut buckets: Vec<IslandPathBucket> = (0..island_count)
        .map(|_| IslandPathBucket::default())
        .collect();

    for path in paths {
        if path.points.len() < 2 {
            continue;
        }
        match path.role {
            ExtrusionRole::Skirt | ExtrusionRole::Brim => adhesion.push(path),
            ExtrusionRole::Support => support_base.push(path),
            ExtrusionRole::SupportInterface => support_iface.push(path),
            _ => {
                let idx = nearest_island_idx(&path, slice_regions, &centers);
                if let Some(bucket) = buckets.get_mut(idx) {
                    match path.role {
                        ExtrusionRole::OuterWall
                        | ExtrusionRole::InnerWall
                        | ExtrusionRole::OverhangWall => bucket.walls.push(path),
                        ExtrusionRole::GapFill => bucket.gap_fill.push(path),
                        ExtrusionRole::BottomSurface | ExtrusionRole::TopSurface => {
                            bucket.surfaces.push(path);
                        }
                        ExtrusionRole::InternalSolidInfill => bucket.internal_solid.push(path),
                        ExtrusionRole::Bridge | ExtrusionRole::InternalBridge => {
                            bucket.bridges.push(path)
                        }
                        ExtrusionRole::SparseInfill => bucket.sparse.push(path),
                        ExtrusionRole::Ironing => bucket.ironing.push(path),
                        ExtrusionRole::Support
                        | ExtrusionRole::SupportInterface
                        | ExtrusionRole::Skirt
                        | ExtrusionRole::Brim => {}
                    }
                }
            }
        }
    }

    let mut out = Vec::new();
    for p in adhesion {
        if let Some((_, end)) = path_start_end_mm(&p) {
            *cursor_xy = end;
        }
        out.push(p);
    }
    if !support_base.is_empty() {
        out.extend(greedy_order_paths(support_base, cursor_xy));
    }
    if !support_iface.is_empty() {
        out.extend(greedy_order_paths(support_iface, cursor_xy));
    }

    let mut remaining: Vec<IslandPathBucket> =
        buckets.into_iter().filter(|b| !b.is_empty()).collect();
    while !remaining.is_empty() {
        let mut best_i = 0usize;
        let mut best_dist = f64::INFINITY;
        for (i, b) in remaining.iter().enumerate() {
            let infill_first = b.supports_infill_first(layer_idx, config);
            if let Some(anchor) = b.first_anchor_mm(infill_first) {
                let d = (anchor.0 - cursor_xy.0).hypot(anchor.1 - cursor_xy.1);
                if d < best_dist {
                    best_dist = d;
                    best_i = i;
                }
            }
        }
        let chosen = remaining.swap_remove(best_i);
        if chosen.supports_infill_first(layer_idx, config) {
            if !chosen.sparse.is_empty() {
                out.extend(greedy_order_paths(chosen.sparse, cursor_xy));
            }
            append_ordered_bucket_group(chosen.walls, cursor_xy, &mut out);
        } else {
            append_ordered_bucket_group(chosen.walls, cursor_xy, &mut out);
            append_ordered_bucket_group(chosen.surfaces, cursor_xy, &mut out);
            if !chosen.internal_solid.is_empty() {
                out.extend(greedy_order_paths(chosen.internal_solid, cursor_xy));
            }
            if !chosen.bridges.is_empty() {
                out.extend(greedy_order_paths(chosen.bridges, cursor_xy));
            }
            if !chosen.gap_fill.is_empty() {
                out.extend(greedy_order_paths(chosen.gap_fill, cursor_xy));
            }
            if !chosen.sparse.is_empty() {
                out.extend(greedy_order_paths(chosen.sparse, cursor_xy));
            }
            append_ordered_bucket_group(chosen.ironing, cursor_xy, &mut out);
        }
    }

    out
}

pub(crate) fn push_closed_poly(
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
            point_widths_mm: Vec::new(),
            height_mm,
            speed_mm_s,
            flow_ratio: 1.0,
            points: poly.points.clone(),
            closed: true,
            object_id: None,
            fan_percent: None,
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

#[allow(clippy::too_many_arguments)]
fn flush_hatch_pts(
    pts: &mut Vec<Point2>,
    role: ExtrusionRole,
    width_mm: f64,
    height_mm: f64,
    speed_mm_s: f64,
    flow_ratio: f64,
    paths: &mut Vec<ExtrusionPath>,
) {
    if pts.len() >= 2 {
        paths.push(ExtrusionPath {
            role,
            width_mm,
            point_widths_mm: Vec::new(),
            height_mm,
            speed_mm_s,
            flow_ratio,
            points: std::mem::take(pts),
            closed: false,
            object_id: None,
            fan_percent: None,
        });
    } else {
        pts.clear();
    }
}

#[cfg(test)]
fn point_in_region(region: &[ExPolygon], p: Point2) -> bool {
    region.iter().any(|ex| ex.contains_point(p))
}

struct HatchSeg {
    row: i64,
    u0: f64,
    u1: f64,
    u_mid: f64,
    v_mid: f64,
    pa: Point2,
    pb: Point2,
}

fn unlock_hatch_dependents(
    chosen: usize,
    deps: &[Vec<usize>],
    in_deg: &mut [usize],
    ready: &mut Vec<usize>,
) {
    let Some(nexts) = deps.get(chosen) else {
        return;
    };
    for &nxt in nexts {
        if let Some(deg) = in_deg.get_mut(nxt)
            && *deg > 0
        {
            *deg -= 1;
            if *deg == 0 {
                ready.push(nxt);
            }
        }
    }
}

fn build_monotonic_hatch_deps(
    segs: &[HatchSeg],
    row_runs: &[(i64, usize, usize)],
    spacing_mm: f64,
    cos_a: f64,
    sin_a: f64,
    chain_mask: &MaskClipIndex,
) -> (Vec<Vec<usize>>, Vec<usize>) {
    let n = segs.len();
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut in_deg: Vec<usize> = vec![0; n];
    for w in row_runs.windows(2) {
        let (Some(&(r0, s0, e0)), Some(&(r1, s1, e1))) = (w.first(), w.get(1)) else {
            continue;
        };
        if r1 != r0 + 1 {
            continue;
        }
        for i in s0..e0 {
            let Some(si) = segs.get(i) else {
                continue;
            };
            for j in s1..e1 {
                let Some(sj) = segs.get(j) else {
                    continue;
                };
                let u_lo = si.u0.max(sj.u0);
                let u_hi = si.u1.min(sj.u1);
                if u_hi < u_lo - 0.5 * spacing_mm {
                    continue;
                }
                let v_test = 0.5 * (si.v_mid + sj.v_mid);
                let connected = if u_hi >= u_lo {
                    let span = u_hi - u_lo;
                    [u_lo + 0.25 * span, 0.5 * (u_lo + u_hi), u_lo + 0.75 * span]
                        .into_iter()
                        .any(|u_t| {
                            let x = u_t * cos_a - v_test * sin_a;
                            let y = u_t * sin_a + v_test * cos_a;
                            Point2::from_mm(x, y).is_ok_and(|pt| chain_mask.contains_point(pt))
                        })
                } else {
                    let u_t = 0.5 * (u_lo + u_hi);
                    let x = u_t * cos_a - v_test * sin_a;
                    let y = u_t * sin_a + v_test * cos_a;
                    Point2::from_mm(x, y).is_ok_and(|pt| chain_mask.contains_point(pt))
                };
                if connected {
                    if let Some(list) = deps.get_mut(i) {
                        list.push(j);
                    }
                    if let Some(deg) = in_deg.get_mut(j) {
                        *deg += 1;
                    }
                }
            }
        }
    }
    (deps, in_deg)
}

#[allow(clippy::too_many_arguments)]
fn chain_hatch_monotonic(
    segs: &[HatchSeg],
    row_runs: &[(i64, usize, usize)],
    spacing_mm: f64,
    cos_a: f64,
    sin_a: f64,
    max_link_mm: f64,
    chain_mask: &MaskClipIndex,
    check_endpoints: bool,
    role: ExtrusionRole,
    width_mm: f64,
    height_mm: f64,
    speed_mm_s: f64,
    flow_ratio: f64,
) -> Vec<ExtrusionPath> {
    let n = segs.len();
    let (deps, mut in_deg) =
        build_monotonic_hatch_deps(segs, row_runs, spacing_mm, cos_a, sin_a, chain_mask);
    let mut ready: Vec<usize> = (0..n)
        .filter(|&i| in_deg.get(i).copied() == Some(0))
        .collect();
    let mut paths: Vec<ExtrusionPath> = Vec::new();
    let mut current_pts: Vec<Point2> = Vec::new();

    while !ready.is_empty() {
        if let Some(&last_pt) = current_pts.last() {
            let mut best_chain: Option<(usize, Point2, Point2, f64)> = None;
            let mut best_jump: Option<(usize, Point2, Point2, f64, i64)> = None;
            for (pos, &cand_idx) in ready.iter().enumerate() {
                let Some(seg) = segs.get(cand_idx) else {
                    continue;
                };
                for (entry_pt, exit_pt) in [(seg.pa, seg.pb), (seg.pb, seg.pa)] {
                    let d = last_pt.distance_mm(entry_pt);
                    let better_jump = best_jump.is_none_or(|(_, _, _, bd, br)| {
                        d < bd - 1e-6 || ((d - bd).abs() <= 1e-6 && seg.row < br)
                    });
                    if better_jump {
                        best_jump = Some((pos, entry_pt, exit_pt, d, seg.row));
                    }
                    if d <= max_link_mm
                        && best_chain.is_none_or(|(_, _, _, bd)| d < bd)
                        && chain_mask.segment_inside_same_region(last_pt, entry_pt, check_endpoints)
                    {
                        best_chain = Some((pos, entry_pt, exit_pt, d));
                    }
                }
            }
            if let Some((pos, entry_pt, exit_pt, _)) = best_chain {
                let chosen = ready.remove(pos);
                if entry_pt != last_pt {
                    current_pts.push(entry_pt);
                }
                current_pts.push(exit_pt);
                unlock_hatch_dependents(chosen, &deps, &mut in_deg, &mut ready);
            } else {
                flush_hatch_pts(
                    &mut current_pts,
                    role,
                    width_mm,
                    height_mm,
                    speed_mm_s,
                    flow_ratio,
                    &mut paths,
                );
                if let Some((pos, entry_pt, exit_pt, _, _)) = best_jump {
                    let chosen = ready.remove(pos);
                    current_pts = vec![entry_pt, exit_pt];
                    unlock_hatch_dependents(chosen, &deps, &mut in_deg, &mut ready);
                } else {
                    let chosen = ready.remove(0);
                    unlock_hatch_dependents(chosen, &deps, &mut in_deg, &mut ready);
                }
            }
        } else {
            let chosen = ready.remove(0);
            if let Some(seg) = segs.get(chosen) {
                current_pts = vec![seg.pa, seg.pb];
            }
            unlock_hatch_dependents(chosen, &deps, &mut in_deg, &mut ready);
        }
    }

    flush_hatch_pts(
        &mut current_pts,
        role,
        width_mm,
        height_mm,
        speed_mm_s,
        flow_ratio,
        &mut paths,
    );
    paths
}

#[allow(clippy::too_many_arguments)]
fn chain_hatch_strip(
    segs: &[HatchSeg],
    row_runs: &[(i64, usize, usize)],
    seg_run_idx: &[usize],
    max_link_mm: f64,
    chain_mask: &MaskClipIndex,
    check_endpoints: bool,
    role: ExtrusionRole,
    width_mm: f64,
    height_mm: f64,
    speed_mm_s: f64,
    flow_ratio: f64,
) -> Vec<ExtrusionPath> {
    let n = segs.len();
    let Some(seg0) = segs.first() else {
        return Vec::new();
    };
    let mut visited = vec![false; n];
    if let Some(v0) = visited.first_mut() {
        *v0 = true;
    }
    let mut remaining = n - 1;
    let mut next_unvisited = 1usize;
    let mut cur_run_idx = seg_run_idx.first().copied().unwrap_or(0);
    let mut paths: Vec<ExtrusionPath> = Vec::new();
    let mut current_pts: Vec<Point2> = vec![seg0.pa, seg0.pb];

    while remaining > 0 {
        let Some(&last_pt) = current_pts.last() else {
            break;
        };
        let run_lo = cur_run_idx.saturating_sub(2);
        let run_hi = (cur_run_idx + 2).min(row_runs.len().saturating_sub(1));
        let mut best_chain: Option<(usize, Point2, Point2, f64)> = None;
        for run_idx in run_lo..=run_hi {
            let Some(&(_, s, e)) = row_runs.get(run_idx) else {
                continue;
            };
            for cand_idx in s..e {
                if visited.get(cand_idx).copied().unwrap_or(true) {
                    continue;
                }
                let Some(seg) = segs.get(cand_idx) else {
                    continue;
                };
                for (entry_pt, exit_pt) in [(seg.pa, seg.pb), (seg.pb, seg.pa)] {
                    let d = last_pt.distance_mm(entry_pt);
                    if d <= max_link_mm
                        && best_chain.is_none_or(|(_, _, _, bd)| d < bd)
                        && chain_mask.segment_inside_same_region(last_pt, entry_pt, check_endpoints)
                    {
                        best_chain = Some((cand_idx, entry_pt, exit_pt, d));
                    }
                }
            }
        }

        if let Some((chosen, entry_pt, exit_pt, _)) = best_chain {
            if let Some(v) = visited.get_mut(chosen) {
                *v = true;
            }
            remaining -= 1;
            cur_run_idx = seg_run_idx.get(chosen).copied().unwrap_or(cur_run_idx);
            if entry_pt != last_pt {
                current_pts.push(entry_pt);
            }
            current_pts.push(exit_pt);
        } else {
            flush_hatch_pts(
                &mut current_pts,
                role,
                width_mm,
                height_mm,
                speed_mm_s,
                flow_ratio,
                &mut paths,
            );
            while next_unvisited < n && visited.get(next_unvisited).copied().unwrap_or(true) {
                next_unvisited += 1;
            }
            let mut best_jump: Option<(usize, Point2, Point2, f64)> = None;
            for cand_idx in next_unvisited..n {
                if visited.get(cand_idx).copied().unwrap_or(true) {
                    continue;
                }
                let Some(seg) = segs.get(cand_idx) else {
                    continue;
                };
                for (entry_pt, exit_pt) in [(seg.pa, seg.pb), (seg.pb, seg.pa)] {
                    let d = last_pt.distance_mm(entry_pt);
                    if best_jump.is_none_or(|(_, _, _, bd)| d < bd) {
                        best_jump = Some((cand_idx, entry_pt, exit_pt, d));
                    }
                }
            }
            let Some((chosen, entry_pt, exit_pt, _)) = best_jump else {
                break;
            };
            if let Some(v) = visited.get_mut(chosen) {
                *v = true;
            }
            remaining -= 1;
            cur_run_idx = seg_run_idx.get(chosen).copied().unwrap_or(0);
            current_pts = vec![entry_pt, exit_pt];
        }
    }

    flush_hatch_pts(
        &mut current_pts,
        role,
        width_mm,
        height_mm,
        speed_mm_s,
        flow_ratio,
        &mut paths,
    );
    paths
}

fn scanline_intersect_ring(
    pts: &[Point2],
    cos_a: f64,
    sin_a: f64,
    spacing_mm: f64,
    phase_offset_mm: f64,
    k_min: i64,
    row_hits: &mut [Vec<f64>],
) {
    let n = pts.len();
    if n < 3 || row_hits.is_empty() {
        return;
    }
    let k_max = k_min + (row_hits.len() as i64) - 1;
    let rot_pts: Vec<(f64, f64)> = pts
        .iter()
        .map(|p| {
            let (x, y) = p.to_mm();
            (x * cos_a + y * sin_a, -x * sin_a + y * cos_a)
        })
        .collect();

    for i in 0..n {
        let (Some(&(u0, v0)), Some(&(u1, v1))) = (rot_pts.get(i), rot_pts.get((i + 1) % n)) else {
            continue;
        };
        let (lo_v, hi_v) = if v0 <= v1 { (v0, v1) } else { (v1, v0) };
        if hi_v - lo_v <= 1e-12 {
            continue;
        }
        let k0 = (((lo_v - phase_offset_mm) / spacing_mm).floor() as i64 - 1).max(k_min);
        let k1 = (((hi_v - phase_offset_mm) / spacing_mm).ceil() as i64 + 1).min(k_max);
        let inv_dv = 1.0 / (v1 - v0);
        for k in k0..=k1 {
            let vk = (k as f64) * spacing_mm + phase_offset_mm;
            if vk >= lo_v && vk < hi_v {
                let t = ((vk - v0) * inv_dv).clamp(0.0, 1.0);
                let u_hit = u0 + t * (u1 - u0);
                if let Some(bucket) = row_hits.get_mut((k - k_min) as usize) {
                    bucket.push(u_hit);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn generate_rectilinear_hatch(
    region: &[ExPolygon],
    angle_rad: f64,
    spacing_mm: f64,
    phase_offset_mm: f64,
    monotonic: bool,
    role: ExtrusionRole,
    width_mm: f64,
    height_mm: f64,
    speed_mm_s: f64,
    flow_ratio: f64,
) -> Result<Vec<ExtrusionPath>, SliceError> {
    if region.is_empty() || spacing_mm <= 0.0 {
        return Ok(Vec::new());
    }
    if region.len() > 1 {
        let mut combined = Vec::new();
        for ex in region {
            combined.extend(generate_rectilinear_hatch(
                std::slice::from_ref(ex),
                angle_rad,
                spacing_mm,
                phase_offset_mm,
                monotonic,
                role,
                width_mm,
                height_mm,
                speed_mm_s,
                flow_ratio,
            )?);
        }
        return Ok(combined);
    }

    let Some(aabb) = region_aabb(region) else {
        return Ok(Vec::new());
    };

    let (min_x, min_y) = aabb.min.to_mm();
    let (max_x, max_y) = aabb.max.to_mm();
    let cos_a = angle_rad.cos();
    let sin_a = angle_rad.sin();

    // Project AABB corners into rotated hatch space:
    // u =  x * cos + y * sin
    // v = -x * sin + y * cos
    let mut v_min = f64::INFINITY;
    let mut v_max = f64::NEG_INFINITY;
    for (cx, cy) in [
        (min_x, min_y),
        (max_x, min_y),
        (max_x, max_y),
        (min_x, max_y),
    ] {
        let v = -cx * sin_a + cy * cos_a;
        v_min = v_min.min(v);
        v_max = v_max.max(v);
    }

    let k_start = ((v_min - phase_offset_mm) / spacing_mm).floor() as i64 - 1;
    let k_end = ((v_max - phase_offset_mm) / spacing_mm).ceil() as i64 + 1;
    let count = k_end.saturating_sub(k_start).clamp(0, 100_000) as usize;
    let mut row_hits: Vec<Vec<f64>> = (0..=count).map(|_| Vec::new()).collect();

    for ex in region {
        scanline_intersect_ring(
            &ex.outer.points,
            cos_a,
            sin_a,
            spacing_mm,
            phase_offset_mm,
            k_start,
            &mut row_hits,
        );
        for hole in &ex.holes {
            scanline_intersect_ring(
                &hole.points,
                cos_a,
                sin_a,
                spacing_mm,
                phase_offset_mm,
                k_start,
                &mut row_hits,
            );
        }
    }

    let min_seg_mm = (0.2 * width_mm).min(0.08);
    let mut segs: Vec<HatchSeg> = Vec::new();
    for (row_off, mut hits) in row_hits.into_iter().enumerate() {
        let k = k_start + (row_off as i64);
        let vk = (k as f64) * spacing_mm + phase_offset_mm;
        if vk < v_min - 1e-4 || vk > v_max + 1e-4 || hits.len() < 2 {
            continue;
        }
        hits.sort_by(f64::total_cmp);
        for &[u0_s, u1_s] in hits.as_chunks::<2>().0 {
            if u1_s - u0_s < min_seg_mm {
                continue;
            }
            let ax = u0_s * cos_a - vk * sin_a;
            let ay = u0_s * sin_a + vk * cos_a;
            let bx = u1_s * cos_a - vk * sin_a;
            let by = u1_s * sin_a + vk * cos_a;
            let pa = Point2::from_mm(ax, ay)?;
            let pb = Point2::from_mm(bx, by)?;
            segs.push(HatchSeg {
                row: k,
                u0: u0_s,
                u1: u1_s,
                u_mid: 0.5 * (u0_s + u1_s),
                v_mid: vk,
                pa,
                pb,
            });
        }
    }
    if segs.is_empty() {
        return Ok(Vec::new());
    }
    segs.sort_by(|a, b| a.row.cmp(&b.row).then_with(|| a.u_mid.total_cmp(&b.u_mid)));

    let chain_region_owned = match offset(region, mm_to_nm(0.02)?, JoinType::Miter { limit: 2.0 }) {
        Ok(expanded) if !expanded.is_empty() => Some(expanded),
        _ => None,
    };
    let chain_region: &[ExPolygon] = chain_region_owned.as_deref().unwrap_or(region);
    let check_endpoints = chain_region_owned.is_some();
    let chain_mask = MaskClipIndex::from_regions(chain_region);

    let max_link_mm = if spacing_mm <= 2.0 * width_mm {
        (3.0 * spacing_mm).max(2.5 * width_mm)
    } else {
        (2.5 * spacing_mm).max(2.0 * width_mm)
    };

    let n = segs.len();
    let mut row_runs: Vec<(i64, usize, usize)> = Vec::new();
    let mut seg_run_idx: Vec<usize> = vec![0; n];
    let mut idx = 0usize;
    while let Some(first_seg) = segs.get(idx) {
        let row = first_seg.row;
        let start = idx;
        let run_i = row_runs.len();
        while let Some(cur) = segs.get(idx)
            && cur.row == row
        {
            if let Some(slot) = seg_run_idx.get_mut(idx) {
                *slot = run_i;
            }
            idx += 1;
        }
        row_runs.push((row, start, idx));
    }

    let paths = if monotonic {
        chain_hatch_monotonic(
            &segs,
            &row_runs,
            spacing_mm,
            cos_a,
            sin_a,
            max_link_mm,
            &chain_mask,
            check_endpoints,
            role,
            width_mm,
            height_mm,
            speed_mm_s,
            flow_ratio,
        )
    } else {
        chain_hatch_strip(
            &segs,
            &row_runs,
            &seg_run_idx,
            max_link_mm,
            &chain_mask,
            check_endpoints,
            role,
            width_mm,
            height_mm,
            speed_mm_s,
            flow_ratio,
        )
    };

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

    fn make_bridge_arch_mesh(
        pillar_w: f64,
        span: f64,
        depth: f64,
        pillar_h: f64,
        deck_h: f64,
    ) -> TriangleMesh {
        let mut tris = Vec::new();
        let right_x = pillar_w + span;
        let total_w = 2.0 * pillar_w + span;
        for (ox, oy, oz, w, d, h) in [
            (0.0, 0.0, 0.0, pillar_w, depth, pillar_h),
            (right_x, 0.0, 0.0, pillar_w, depth, pillar_h),
            (0.0, 0.0, pillar_h, total_w, depth, deck_h),
        ] {
            let mut m = make_box_mesh(w, d, h);
            m.translate(ox, oy, oz).unwrap();
            tris.extend(m.triangles);
        }
        TriangleMesh::new(tris).unwrap()
    }

    fn make_overhang_ramp_mesh(
        base_w: f64,
        depth: f64,
        height: f64,
        overhang_deg: f64,
    ) -> TriangleMesh {
        let dx = height * overhang_deg.to_radians().tan();
        let top_w = base_w + dx;
        let push_q = |tris: &mut Vec<Triangle>, a: Point3, b: Point3, c: Point3, d: Point3| {
            tris.push(Triangle::new(a, b, c));
            tris.push(Triangle::new(a, c, d));
        };
        let p000 = Point3::new(0.0, 0.0, 0.0);
        let p100 = Point3::new(base_w, 0.0, 0.0);
        let p110 = Point3::new(base_w, depth, 0.0);
        let p010 = Point3::new(0.0, depth, 0.0);
        let p001 = Point3::new(0.0, 0.0, height);
        let p101 = Point3::new(top_w, 0.0, height);
        let p111 = Point3::new(top_w, depth, height);
        let p011 = Point3::new(0.0, depth, height);

        let mut tris = Vec::with_capacity(12);
        push_q(&mut tris, p000, p010, p110, p100);
        push_q(&mut tris, p001, p101, p111, p011);
        push_q(&mut tris, p000, p100, p101, p001);
        push_q(&mut tris, p010, p011, p111, p110);
        push_q(&mut tris, p000, p001, p011, p010);
        push_q(&mut tris, p100, p110, p111, p101);
        TriangleMesh::new(tris).unwrap()
    }

    #[test]
    fn bridge_arch_emits_x_aligned_bridges_and_boosts_fan() {
        let mesh = make_bridge_arch_mesh(6.0, 18.0, 10.0, 4.0, 2.0);
        let mut cfg = PrintConfig::default();
        cfg.filament.fan_min_percent = 30;
        cfg.filament.bridge_fan_percent = 100;
        cfg.filament.slow_down_layer_time_s = 0.0;
        cfg.process.bridge_speed_mm_s = 28.0;
        cfg.process.bridge_flow_ratio = 0.90;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let bridge_layer = &plate.layers[20];
        let bridges: Vec<_> = bridge_layer
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::Bridge)
            .collect();
        assert!(!bridges.is_empty(), "expected Bridge paths on layer 20");
        assert_eq!(bridge_layer.fan_percent, 30);
        assert!(bridges.iter().all(|b| b.fan_percent == Some(100)));
        assert!(
            bridge_layer
                .paths
                .iter()
                .any(|p| p.role == ExtrusionRole::OuterWall && p.fan_percent.is_none())
        );

        let mut x_span_len = 0.0;
        let mut total_len = 0.0;
        for b in bridges {
            assert!((b.speed_mm_s - 28.0).abs() < 1e-6);
            assert!((b.flow_ratio - 0.90).abs() < 1e-6);
            for w in b.points.windows(2) {
                let (x0, y0) = w[0].to_mm();
                let (x1, y1) = w[1].to_mm();
                let dx = (x1 - x0).abs();
                let dy = (y1 - y0).abs();
                let len = dx.hypot(dy);
                total_len += len;
                if len > 1.0 {
                    assert!(
                        dy < dx * 0.05 + 1e-4,
                        "expected X-aligned bridge span, got ({x0},{y0}) -> ({x1},{y1})"
                    );
                    x_span_len += len;
                }
            }
        }
        assert!(x_span_len > 0.90 * total_len);
    }

    #[test]
    fn bridge_walls_slow_down_both_outer_and_inner_walls_over_air_gap() {
        let mesh = make_bridge_arch_mesh(6.0, 18.0, 10.0, 6.0, 3.0);
        let mut cfg = PrintConfig::default();
        cfg.process.wall_loops = 2;
        cfg.process.outer_wall_speed_mm_s = 120.0;
        cfg.process.inner_wall_speed_mm_s = 180.0;
        cfg.process.overhang_speed_mm_s = 30.0;
        cfg.process.bridge_speed_mm_s = 35.0;
        cfg.process.bridge_flow_ratio = 0.95;
        cfg.filament.slow_down_layer_time_s = 0.0;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let bridge_layer = &plate.layers[30];
        assert!((bridge_layer.z_top_mm - 6.2).abs() < 1e-4);

        let mut bridge_overhang_paths = 0usize;
        let mut has_fast_inner_pillar = false;
        let mut has_fast_outer_pillar = false;
        for p in &bridge_layer.paths {
            match p.role {
                ExtrusionRole::InnerWall | ExtrusionRole::OuterWall => {
                    if p.role == ExtrusionRole::InnerWall
                        && (p.speed_mm_s - 180.0).abs() < 1e-6
                        && (p.flow_ratio - 1.0).abs() < 1e-6
                    {
                        has_fast_inner_pillar = true;
                    }
                    if p.role == ExtrusionRole::OuterWall
                        && (p.speed_mm_s - 120.0).abs() < 1e-6
                        && (p.flow_ratio - 1.0).abs() < 1e-6
                    {
                        has_fast_outer_pillar = true;
                    }
                    for w in p.points.windows(2) {
                        let mx = 0.5 * (w[0].to_mm().0 + w[1].to_mm().0);
                        assert!(
                            !(120.0..=130.0).contains(&mx) || p.speed_mm_s <= 35.0,
                            "fast {:?} crossed air gap at x={mx:.2} speed={}",
                            p.role,
                            p.speed_mm_s
                        );
                    }
                }
                ExtrusionRole::OverhangWall => {
                    let crosses_gap = p.points.windows(2).any(|w| {
                        let mx = 0.5 * (w[0].to_mm().0 + w[1].to_mm().0);
                        (120.0..=130.0).contains(&mx)
                    });
                    if crosses_gap {
                        bridge_overhang_paths += 1;
                        assert!(
                            p.speed_mm_s <= 30.0 + 1e-6,
                            "expected slow bridge wall <=30 mm/s, got {}",
                            p.speed_mm_s
                        );
                        assert!(
                            (p.flow_ratio - 0.95).abs() < 1e-6,
                            "expected bridge_flow_ratio 0.95, got {}",
                            p.flow_ratio
                        );
                    }
                }
                _ => {}
            }
        }
        assert!(
            bridge_overhang_paths >= 4,
            "expected >=4 slowed OverhangWall spans (2 outer + 2 inner), got {bridge_overhang_paths}"
        );
        assert!(
            has_fast_inner_pillar && has_fast_outer_pillar,
            "expected supported inner and outer pillar walls to keep full speed"
        );
    }

    #[test]
    fn overhang_detection_triggers_at_60deg_and_skips_30deg() {
        let mut cfg = PrintConfig::default();
        cfg.process.overhang_threshold_deg = 45.0;
        cfg.process.overhang_speed_mm_s = 22.0;
        cfg.filament.slow_down_layer_time_s = 0.0;

        let steep = make_overhang_ramp_mesh(12.0, 12.0, 4.0, 60.0);
        let steep_plate = slice_single_mesh(&steep, &cfg).unwrap();
        let overhang_paths: Vec<_> = steep_plate
            .layers
            .iter()
            .flat_map(|l| l.paths.iter())
            .filter(|p| p.role == ExtrusionRole::OverhangWall)
            .collect();
        assert!(
            !overhang_paths.is_empty(),
            "60 deg overhang should produce OverhangWall"
        );
        for p in overhang_paths {
            assert!((p.speed_mm_s - 22.0).abs() < 1e-6);
        }

        let shallow = make_overhang_ramp_mesh(12.0, 12.0, 4.0, 30.0);
        let shallow_plate = slice_single_mesh(&shallow, &cfg).unwrap();
        let shallow_overhangs = shallow_plate
            .layers
            .iter()
            .flat_map(|l| l.paths.iter())
            .filter(|p| p.role == ExtrusionRole::OverhangWall)
            .count();
        assert_eq!(shallow_overhangs, 0);
    }

    #[test]
    fn gyroid_and_cubic_infill_produce_3d_varying_paths() {
        let mesh = make_box_mesh(24.0, 24.0, 6.0);

        let mut cfg_g = PrintConfig::default();
        cfg_g.process.sparse_infill_pattern = InfillPattern::Gyroid;
        cfg_g.process.sparse_infill_density = 0.25;
        let plate_g = slice_single_mesh(&mesh, &cfg_g).unwrap();
        let l10_g: Vec<_> = plate_g.layers[10]
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::SparseInfill)
            .collect();
        let l15_g: Vec<_> = plate_g.layers[15]
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::SparseInfill)
            .collect();
        assert!(!l10_g.is_empty() && !l15_g.is_empty());
        assert!(l10_g.iter().any(|p| p.points.len() >= 5));
        assert_ne!(l10_g[0].points, l15_g[0].points);

        let mut cfg_c = PrintConfig::default();
        cfg_c.process.sparse_infill_pattern = InfillPattern::Cubic;
        cfg_c.process.sparse_infill_density = 0.25;
        let plate_c = slice_single_mesh(&mesh, &cfg_c).unwrap();
        let l10_c: Vec<_> = plate_c.layers[10]
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::SparseInfill)
            .collect();
        let l15_c: Vec<_> = plate_c.layers[15]
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::SparseInfill)
            .collect();
        assert!(!l10_c.is_empty() && !l15_c.is_empty());
        assert_ne!(l10_c[0].points, l15_c[0].points);
    }

    #[test]
    fn monotonic_top_surface_orders_holed_rows_non_decreasingly() {
        let outer = Polygon::new(vec![
            Point2::from_mm(0.0, 0.0).unwrap(),
            Point2::from_mm(20.0, 0.0).unwrap(),
            Point2::from_mm(20.0, 20.0).unwrap(),
            Point2::from_mm(0.0, 20.0).unwrap(),
        ]);
        let mut hole = Polygon::new(vec![
            Point2::from_mm(7.0, 7.0).unwrap(),
            Point2::from_mm(13.0, 7.0).unwrap(),
            Point2::from_mm(13.0, 13.0).unwrap(),
            Point2::from_mm(7.0, 13.0).unwrap(),
        ]);
        hole.ensure_cw();
        let region = vec![ExPolygon::new(outer, vec![hole])];

        let angle = FRAC_PI_4;
        let spacing = stadium_spacing_mm(0.42, 0.2);
        let paths = generate_rectilinear_hatch(
            &region,
            angle,
            spacing,
            0.0,
            true,
            ExtrusionRole::TopSurface,
            0.42,
            0.2,
            60.0,
            1.0,
        )
        .unwrap();
        assert!(
            !paths.is_empty() && paths.len() <= 4,
            "expected holed monotonic region to chain into <=4 paths, got {}",
            paths.len()
        );

        struct EmittedRow {
            order: usize,
            row: i64,
            u0: f64,
            u1: f64,
            v_mid: f64,
        }

        let sin_a = angle.sin();
        let cos_a = angle.cos();
        let mut emitted = Vec::new();
        let mut order_idx = 0usize;
        for p in &paths {
            let mut prev_row_in_path = i64::MIN;
            for seg in p.points.windows(2) {
                let (x0, y0) = seg[0].to_mm();
                let (x1, y1) = seg[1].to_mm();
                let u_diff = ((x1 - x0) * cos_a + (y1 - y0) * sin_a).abs();
                let v_diff = (-(x1 - x0) * sin_a + (y1 - y0) * cos_a).abs();
                if u_diff > v_diff {
                    let v_mid = -0.5 * (x0 + x1) * sin_a + 0.5 * (y0 + y1) * cos_a;
                    let row = (v_mid / spacing).round() as i64;
                    assert!(
                        row >= prev_row_in_path,
                        "within path monotonic order regressed from row {prev_row_in_path} to {row}"
                    );
                    prev_row_in_path = row;
                    let ua = x0 * cos_a + y0 * sin_a;
                    let ub = x1 * cos_a + y1 * sin_a;
                    emitted.push(EmittedRow {
                        order: order_idx,
                        row,
                        u0: ua.min(ub),
                        u1: ua.max(ub),
                        v_mid,
                    });
                    order_idx += 1;
                }
            }
        }
        assert!(order_idx > 10);

        for a in &emitted {
            for b in &emitted {
                if b.row != a.row + 1 {
                    continue;
                }
                let u_lo = a.u0.max(b.u0);
                let u_hi = a.u1.min(b.u1);
                if u_hi < u_lo {
                    continue;
                }
                let v_test = 0.5 * (a.v_mid + b.v_mid);
                let span = u_hi - u_lo;
                let connected = [u_lo + 0.25 * span, 0.5 * (u_lo + u_hi), u_lo + 0.75 * span]
                    .into_iter()
                    .any(|u_t| {
                        let x = u_t * cos_a - v_test * sin_a;
                        let y = u_t * sin_a + v_test * cos_a;
                        point_in_region(&region, Point2::from_mm(x, y).unwrap())
                    });
                if connected {
                    assert!(
                        a.order < b.order,
                        "monotonic dependency violated: row {} (order {}) printed after row {} (order {})",
                        a.row,
                        a.order,
                        b.row,
                        b.order
                    );
                }
            }
        }
    }

    #[test]
    fn alternating_infill_first_reorders_odd_interior_layers_only() {
        let mesh = make_box_mesh(20.0, 20.0, 4.0);
        let mut cfg = PrintConfig::default();
        cfg.process.wall_loops = 2;
        cfg.process.bottom_shell_layers = 3;
        cfg.process.top_shell_layers = 3;
        cfg.process.sparse_infill_density = 0.20;
        cfg.process.wall_order = WallOrder::InnerOuter;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        assert_eq!(plate.layers[0].paths[0].role, ExtrusionRole::InnerWall);
        // Even interior sparse layer (idx = 6): walls before sparse infill.
        assert!(matches!(
            plate.layers[6].paths[0].role,
            ExtrusionRole::InnerWall | ExtrusionRole::OuterWall
        ));
        // Odd interior sparse layer (idx = 7): sparse infill before walls.
        assert_eq!(plate.layers[7].paths[0].role, ExtrusionRole::SparseInfill);
        let wall_pos = plate.layers[7]
            .paths
            .iter()
            .position(|p| matches!(p.role, ExtrusionRole::InnerWall | ExtrusionRole::OuterWall))
            .unwrap();
        assert!(wall_pos > 0);

        // OuterInner wall order never reorders infill ahead of walls.
        let mut outer_first_cfg = cfg;
        outer_first_cfg.process.wall_order = WallOrder::OuterInner;
        let plate_outer = slice_single_mesh(&mesh, &outer_first_cfg).unwrap();
        assert_eq!(
            plate_outer.layers[7].paths[0].role,
            ExtrusionRole::OuterWall
        );
    }

    fn make_c_bracket_mesh(
        width_mm: f64,
        depth_mm: f64,
        base_h: f64,
        wall_w: f64,
        gap_h: f64,
        top_h: f64,
    ) -> TriangleMesh {
        let mut tris = make_box_mesh(width_mm, depth_mm, base_h).triangles;
        let mut wall = make_box_mesh(wall_w, depth_mm, gap_h);
        wall.translate(0.0, 0.0, base_h).unwrap();
        tris.extend(wall.triangles);
        let mut top = make_box_mesh(width_mm, depth_mm, top_h);
        top.translate(0.0, 0.0, base_h + gap_h).unwrap();
        tris.extend(top.triangles);
        TriangleMesh::new(tris).unwrap()
    }

    #[test]
    fn grid_support_on_bridge_arch_respects_z_and_xy_clearances() {
        let mesh = make_bridge_arch_mesh(6.0, 18.0, 10.0, 6.0, 3.0);
        let mut cfg = PrintConfig::default();
        cfg.process.support.enabled = true;
        cfg.process.support.style = SupportStyle::Grid;
        cfg.process.support.top_z_distance_mm = 0.2;
        cfg.process.support.xy_distance_mm = 0.5;
        cfg.process.support.interface_layers = 2;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let has_support = plate
            .layers
            .iter()
            .any(|l| l.paths.iter().any(|p| p.role == ExtrusionRole::Support));
        let has_iface = plate.layers.iter().any(|l| {
            l.paths
                .iter()
                .any(|p| p.role == ExtrusionRole::SupportInterface)
        });
        assert!(has_support);
        assert!(has_iface);

        // Layer 29 (z_top = 6.0 mm, right below the deck at z=6.0..6.2 mm) must have NO support due to 0.2 mm top Z gap.
        let gap_layer = &plate.layers[29];
        assert!(gap_layer.paths.iter().all(|p| !matches!(
            p.role,
            ExtrusionRole::Support | ExtrusionRole::SupportInterface
        )));

        // Every support segment stays clear of pillars [110..116] and [134..140] by xy_distance_mm.
        for layer in &plate.layers[..29] {
            for p in layer.paths.iter().filter(|p| {
                matches!(
                    p.role,
                    ExtrusionRole::Support | ExtrusionRole::SupportInterface
                )
            }) {
                for pt in &p.points {
                    let (x, _) = pt.to_mm();
                    assert!(
                        x > 116.45 && x < 133.55,
                        "support intruded into pillar XY clearance at x={x}"
                    );
                }
            }
        }
    }

    #[test]
    fn snug_support_on_60deg_ramp_stays_under_overhang() {
        let mesh = make_overhang_ramp_mesh(10.0, 12.0, 8.0, 60.0);
        let mut cfg = PrintConfig::default();
        cfg.process.support.enabled = true;
        cfg.process.support.style = SupportStyle::Snug;
        cfg.process.support.threshold_angle_deg = 45.0;
        cfg.process.support.interface_layers = 2;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let mut max_support_x = f64::NEG_INFINITY;
        let mut max_model_x = f64::NEG_INFINITY;
        let mut saw_support = false;
        let mut saw_iface = false;

        for layer in &plate.layers {
            for r in &layer.slice_regions {
                if let Some(bb) = r.aabb() {
                    max_model_x = max_model_x.max(bb.max.to_mm().0);
                }
            }
            for p in &layer.paths {
                if matches!(
                    p.role,
                    ExtrusionRole::Support | ExtrusionRole::SupportInterface
                ) {
                    if p.role == ExtrusionRole::Support {
                        saw_support = true;
                    } else {
                        saw_iface = true;
                    }
                    for pt in &p.points {
                        max_support_x = max_support_x.max(pt.to_mm().0);
                    }
                }
            }
        }
        assert!(saw_support);
        assert!(saw_iface);
        assert!(max_support_x <= max_model_x + 0.05);
    }

    #[test]
    fn support_on_build_plate_only_filters_c_bracket() {
        let mesh = make_c_bracket_mesh(20.0, 10.0, 3.0, 5.0, 6.0, 3.0);

        let mut cfg_plate_only = PrintConfig::default();
        cfg_plate_only.process.support.enabled = true;
        cfg_plate_only.process.support.style = SupportStyle::Snug;
        cfg_plate_only.process.support.on_build_plate_only = true;
        let plate_filtered = slice_single_mesh(&mesh, &cfg_plate_only).unwrap();
        let sup_count: usize = plate_filtered
            .layers
            .iter()
            .flat_map(|l| l.paths.iter())
            .filter(|p| {
                matches!(
                    p.role,
                    ExtrusionRole::Support | ExtrusionRole::SupportInterface
                )
            })
            .count();
        assert_eq!(sup_count, 0);

        let mut cfg_all = cfg_plate_only;
        cfg_all.process.support.on_build_plate_only = false;
        let plate_supported = slice_single_mesh(&mesh, &cfg_all).unwrap();
        // Bottom arm occupies layers 0..15 (z <= 3.0), layer 15 (z=3.0..3.2) is bottom_z_distance gap.
        let sup_in_gap = plate_supported.layers[..16].iter().any(|l| {
            l.paths.iter().any(|p| {
                matches!(
                    p.role,
                    ExtrusionRole::Support | ExtrusionRole::SupportInterface
                )
            })
        });
        assert!(!sup_in_gap, "bottom Z gap above lower arm violated");
        let sup_above_gap = plate_supported.layers[16..44].iter().any(|l| {
            l.paths.iter().any(|p| {
                matches!(
                    p.role,
                    ExtrusionRole::Support | ExtrusionRole::SupportInterface
                )
            })
        });
        assert!(
            sup_above_gap,
            "expected supports between lower and upper C-arms"
        );
    }

    #[test]
    fn ironing_modes_differentiate_topmost_and_all_top_surfaces() {
        let base = make_box_mesh(20.0, 20.0, 4.0);
        let mut top = make_box_mesh(10.0, 10.0, 4.0);
        top.translate(5.0, 5.0, 4.0).unwrap();
        let mesh = TriangleMesh::merge(&[base, top]).unwrap();
        let has_ironing =
            |l: &SlicedLayer| l.paths.iter().any(|p| p.role == ExtrusionRole::Ironing);

        // TopmostSurface: irons ONLY the last layer (layer 39), NOT step ledge at layer 19.
        let mut cfg_topmost = PrintConfig::default();
        cfg_topmost.process.ironing.mode = IroningType::TopmostSurface;
        let plate_top = slice_single_mesh(&mesh, &cfg_topmost).unwrap();
        let top_ironed_layers: Vec<usize> = plate_top
            .layers
            .iter()
            .filter(|l| has_ironing(l))
            .map(|l| l.index)
            .collect();
        assert_eq!(top_ironed_layers, vec![plate_top.layers.len() - 1]);

        // AllTopSurfaces: irons BOTH step ledge (layer 19) AND topmost layer (layer 39).
        let mut cfg_all = PrintConfig::default();
        cfg_all.process.ironing.mode = IroningType::AllTopSurfaces;
        let plate_all = slice_single_mesh(&mesh, &cfg_all).unwrap();
        let all_ironed_layers: Vec<usize> = plate_all
            .layers
            .iter()
            .filter(|l| has_ironing(l))
            .map(|l| l.index)
            .collect();
        assert_eq!(all_ironed_layers, vec![19, plate_all.layers.len() - 1]);

        // Verify scaled flow ratio stays low-flow on ironing paths.
        let sample_flow = plate_all
            .layers
            .last()
            .unwrap()
            .paths
            .iter()
            .find(|p| p.role == ExtrusionRole::Ironing)
            .unwrap()
            .flow_ratio;
        assert!(sample_flow > 0.005 && sample_flow < 0.05);
    }

    fn make_tapered_wedge_mesh(
        length: f64,
        wide_w: f64,
        narrow_w: f64,
        height: f64,
    ) -> TriangleMesh {
        let hw0 = 0.5 * wide_w;
        let hw1 = 0.5 * narrow_w;
        let p000 = Point3::new(0.0, -hw0, 0.0);
        let p100 = Point3::new(length, -hw1, 0.0);
        let p110 = Point3::new(length, hw1, 0.0);
        let p010 = Point3::new(0.0, hw0, 0.0);
        let p001 = Point3::new(0.0, -hw0, height);
        let p101 = Point3::new(length, -hw1, height);
        let p111 = Point3::new(length, hw1, height);
        let p011 = Point3::new(0.0, hw0, height);

        let mut tris = Vec::with_capacity(12);
        let mut push_q = |a: Point3, b: Point3, c: Point3, d: Point3| {
            tris.push(Triangle::new(a, b, c));
            tris.push(Triangle::new(a, c, d));
        };
        push_q(p000, p010, p110, p100);
        push_q(p001, p101, p111, p011);
        push_q(p000, p100, p101, p001);
        push_q(p010, p011, p111, p110);
        push_q(p000, p001, p011, p010);
        push_q(p100, p110, p111, p101);
        TriangleMesh::new(tris).unwrap()
    }

    fn make_hollow_cyl_mesh(
        outer_r: f64,
        inner_r: f64,
        height: f64,
        segments: usize,
    ) -> TriangleMesh {
        let mut tris = Vec::with_capacity(segments * 8);
        let mut push_q = |a: Point3, b: Point3, c: Point3, d: Point3| {
            tris.push(Triangle::new(a, b, c));
            tris.push(Triangle::new(a, c, d));
        };
        for i in 0..segments {
            let a0 = TAU * (i as f64) / (segments as f64);
            let a1 = TAU * (((i + 1) % segments) as f64) / (segments as f64);
            let ob0 = Point3::new(outer_r * a0.cos(), outer_r * a0.sin(), 0.0);
            let ob1 = Point3::new(outer_r * a1.cos(), outer_r * a1.sin(), 0.0);
            let ot0 = Point3::new(outer_r * a0.cos(), outer_r * a0.sin(), height);
            let ot1 = Point3::new(outer_r * a1.cos(), outer_r * a1.sin(), height);
            let ib0 = Point3::new(inner_r * a0.cos(), inner_r * a0.sin(), 0.0);
            let ib1 = Point3::new(inner_r * a1.cos(), inner_r * a1.sin(), 0.0);
            let it0 = Point3::new(inner_r * a0.cos(), inner_r * a0.sin(), height);
            let it1 = Point3::new(inner_r * a1.cos(), inner_r * a1.sin(), height);
            push_q(ob0, ob1, ot1, ot0);
            push_q(ib1, ib0, it0, it1);
            push_q(ib0, ib1, ob1, ob0);
            push_q(ot0, ot1, it1, it0);
        }
        TriangleMesh::new(tris).unwrap()
    }

    #[test]
    fn arachne_standard_cube_matches_classic_walls_and_infill() {
        let mesh = make_box_mesh(20.0, 20.0, 4.0);
        let mut cfg = PrintConfig::default();
        cfg.process.wall_generator = WallGenerator::Arachne;
        cfg.process.wall_loops = 2;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let mid = &plate.layers[10];
        let outer: Vec<_> = mid
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::OuterWall)
            .collect();
        let inner: Vec<_> = mid
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::InnerWall)
            .collect();
        let infill: Vec<_> = mid
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::SparseInfill)
            .collect();
        assert_eq!(outer.len(), 1);
        assert_eq!(inner.len(), 1);
        assert!(!infill.is_empty());
        assert!(outer[0].closed && inner[0].closed);
        assert!((outer[0].width_mm - cfg.process.outer_wall_width_mm).abs() < 0.01);
        assert!((inner[0].width_mm - cfg.process.inner_wall_width_mm).abs() < 0.01);
        assert!(outer[0].points.len() <= 6 && inner[0].points.len() <= 6);
    }

    #[test]
    fn arachne_thin_rib_prints_single_medial_bead() {
        let mesh = make_box_mesh(20.0, 0.50, 2.0);
        let mut cfg = PrintConfig::default();
        cfg.process.wall_generator = WallGenerator::Arachne;
        cfg.process.wall_loops = 2;
        cfg.process.outer_wall_width_mm = 0.42;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let mid = &plate.layers[5];
        let outer: Vec<_> = mid
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::OuterWall)
            .collect();
        let inner_count = mid
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::InnerWall)
            .count();
        assert_eq!(
            outer.len(),
            1,
            "expected 1 medial bead path, got {:?}",
            outer
        );
        assert_eq!(inner_count, 0);
        assert!(!outer[0].closed);
        assert!(
            (outer[0].width_mm - 0.50).abs() < 0.03,
            "width = {}",
            outer[0].width_mm
        );
    }

    #[test]
    fn arachne_gapfree_intermediate_wall() {
        let mesh = make_box_mesh(20.0, 1.05, 2.0);
        let mut cfg = PrintConfig::default();
        cfg.process.wall_generator = WallGenerator::Arachne;
        cfg.process.wall_loops = 2;
        cfg.process.outer_wall_width_mm = 0.42;
        cfg.process.inner_wall_width_mm = 0.45;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let mid = &plate.layers[5];
        let outer: Vec<_> = mid
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::OuterWall)
            .collect();
        let inner_count = mid
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::InnerWall)
            .count();
        let infill_count = mid
            .paths
            .iter()
            .filter(|p| {
                matches!(
                    p.role,
                    ExtrusionRole::SparseInfill
                        | ExtrusionRole::BottomSurface
                        | ExtrusionRole::TopSurface
                )
            })
            .count();
        assert_eq!(outer.len(), 1);
        assert!(outer[0].closed);
        assert_eq!(inner_count, 0);
        assert_eq!(infill_count, 0);
        assert!(
            (outer[0].width_mm - 0.546).abs() < 0.03,
            "outer width = {}",
            outer[0].width_mm
        );
    }

    #[test]
    fn arachne_tapered_wedge_modulates_width_and_prints_tip() {
        let mesh = make_tapered_wedge_mesh(24.0, 4.0, 0.25, 2.0);
        let mut cfg = PrintConfig::default();
        cfg.process.wall_generator = WallGenerator::Arachne;
        cfg.process.wall_loops = 2;
        cfg.process.sparse_infill_density = 0.20;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let mid = &plate.layers[5];
        let walls: Vec<_> = mid
            .paths
            .iter()
            .filter(|p| matches!(p.role, ExtrusionRole::OuterWall | ExtrusionRole::InnerWall))
            .collect();
        assert!(!walls.is_empty());

        let mut min_x = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut min_w = f64::INFINITY;
        let mut max_w = 0.0f64;
        for w in &walls {
            min_w = min_w.min(w.min_width_mm());
            max_w = max_w.max(w.max_width_mm());
            for pt in &w.points {
                let px = pt.to_mm().0;
                min_x = min_x.min(px);
                max_x = max_x.max(px);
            }
        }
        let x_span = max_x - min_x;
        assert!(
            x_span >= 22.8,
            "x_span = {x_span:.3} (< 22.8 mm tip dropped)"
        );
        assert!(
            max_w / min_w >= 1.35,
            "width ratio {:.3} < 1.35 (min={min_w:.3}, max={max_w:.3})",
            max_w / min_w
        );

        let inner_loop_max_x = walls
            .iter()
            .filter(|w| w.role == ExtrusionRole::InnerWall && w.closed)
            .flat_map(|w| w.points.iter())
            .map(|pt| pt.to_mm().0)
            .fold(f64::NEG_INFINITY, f64::max);
        let inner_medial_min_x = walls
            .iter()
            .filter(|w| w.role == ExtrusionRole::InnerWall && !w.closed)
            .flat_map(|w| w.points.iter())
            .map(|pt| pt.to_mm().0)
            .fold(f64::INFINITY, f64::min);
        assert!(
            inner_loop_max_x.is_finite() && inner_medial_min_x.is_finite(),
            "expected both closed and medial InnerWall paths"
        );
        let inner_gap = (inner_medial_min_x - inner_loop_max_x).abs();
        assert!(
            inner_gap <= 0.25,
            "InnerWall 2->1 gap {inner_gap:.3} mm > 0.25 mm (loop_max={inner_loop_max_x:.3}, medial_min={inner_medial_min_x:.3})"
        );

        let tip = walls
            .iter()
            .copied()
            .find(|w| w.role == ExtrusionRole::OuterWall && !w.closed)
            .expect("expected open OuterWall medial tip path");
        assert!(
            tip.points.len() >= 4,
            "expected subdivided OuterWall tip points, got {}",
            tip.points.len()
        );
        assert!(tip.has_variable_width());
        let tip_ratio = tip.max_width_mm() / tip.min_width_mm();
        assert!(
            tip_ratio >= 1.5,
            "expected progressive tip width ratio >= 1.5, got {tip_ratio:.3} (min={:.3}, max={:.3})",
            tip.min_width_mm(),
            tip.max_width_mm()
        );

        for w in &walls {
            if !w.has_variable_width() || w.points.len() < 2 {
                continue;
            }
            let pn = w.points.len();
            let segs = if w.closed { pn } else { pn - 1 };
            for i in 0..segs {
                let j = (i + 1) % pn;
                let a = w.points[i].to_mm();
                let b = w.points[j].to_mm();
                let len = (b.0 - a.0).hypot(b.1 - a.1);
                if len > 0.30 {
                    let dw = (w.vertex_width_mm(j) - w.vertex_width_mm(i)).abs();
                    assert!(
                        dw <= 0.05,
                        "un-subdivided width jump {dw:.3} mm over {len:.3} mm segment on {:?}",
                        w.role
                    );
                }
            }
        }
    }

    #[test]
    fn arachne_thin_hollow_cylinder_prints_closed_medial_ring() {
        let mesh = make_hollow_cyl_mesh(10.0, 9.4, 2.0, 64);
        let mut cfg = PrintConfig::default();
        cfg.process.wall_generator = WallGenerator::Arachne;
        cfg.process.wall_loops = 2;
        cfg.process.outer_wall_width_mm = 0.42;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let mid = &plate.layers[5];
        let outer: Vec<_> = mid
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::OuterWall)
            .collect();
        assert_eq!(outer.len(), 1, "expected single closed medial ring");
        assert!(outer[0].closed, "expected closed ring on hollow cylinder");
        assert!(
            (outer[0].width_mm - 0.60).abs() < 0.05,
            "ring width = {}",
            outer[0].width_mm
        );
    }

    #[test]
    fn arachne_hostile_tiny_feature_never_panics() {
        let mesh = make_box_mesh(10.0, 0.05, 1.0);
        let mut cfg = PrintConfig::default();
        cfg.process.wall_generator = WallGenerator::Arachne;
        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let wall_cnt: usize = plate
            .layers
            .iter()
            .flat_map(|l| l.paths.iter())
            .filter(|p| matches!(p.role, ExtrusionRole::OuterWall | ExtrusionRole::InnerWall))
            .count();
        assert_eq!(wall_cnt, 0);
    }

    #[test]
    fn tree_support_c_bracket_curves_around_lower_arm_to_build_plate() {
        let mesh = make_c_bracket_mesh(20.0, 8.0, 3.0, 5.0, 8.0, 3.0);
        let mut cfg = PrintConfig::default();
        cfg.process.support.enabled = true;
        cfg.process.support.style = SupportStyle::Tree;
        cfg.process.support.on_build_plate_only = true;
        cfg.process.support.interface_layers = 2;
        cfg.process.support.tree.max_branch_angle_deg = 45.0;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let has_layer0_sup = plate.layers[0]
            .paths
            .iter()
            .any(|p| p.role == ExtrusionRole::Support);
        assert!(
            has_layer0_sup,
            "expected tree trunk reaching build plate z=0"
        );

        let mut sup_count = 0usize;
        let mut iface_count = 0usize;
        for layer in &plate.layers {
            for p in &layer.paths {
                if p.role == ExtrusionRole::Support {
                    sup_count += 1;
                } else if p.role == ExtrusionRole::SupportInterface {
                    iface_count += 1;
                }
                if matches!(
                    p.role,
                    ExtrusionRole::Support | ExtrusionRole::SupportInterface
                ) {
                    for pt in &p.points {
                        for reg in &layer.slice_regions {
                            assert!(
                                !reg.contains_point(*pt),
                                "tree support collided with model at layer {} {:?}",
                                layer.index,
                                pt.to_mm()
                            );
                        }
                    }
                }
            }
        }
        assert!(sup_count > 10, "expected Support paths across gap");
        assert!(iface_count > 0, "expected SupportInterface paths");
    }

    #[test]
    fn tree_support_c_bracket_has_no_inverted_bulge_or_steep_overhang() {
        let mesh = make_c_bracket_mesh(24.0, 12.0, 4.0, 6.0, 6.0, 4.0);
        let mut cfg = PrintConfig::default();
        cfg.process.support.enabled = true;
        cfg.process.support.style = SupportStyle::Tree;
        cfg.process.support.on_build_plate_only = true;
        cfg.process.support.interface_layers = 2;
        cfg.process.support.tree.max_branch_angle_deg = 45.0;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        assert!(
            plate.layers[0]
                .paths
                .iter()
                .any(|p| p.role == ExtrusionRole::Support),
            "expected layer 0 trunks"
        );
        assert!(
            plate
                .layers
                .iter()
                .flat_map(|l| l.paths.iter())
                .any(|p| p.role == ExtrusionRole::SupportInterface),
            "expected SupportInterface paths under upper arm"
        );

        let max_step = cfg.process.layer_height_mm
            * cfg
                .process
                .support
                .tree
                .max_branch_angle_deg
                .to_radians()
                .tan();
        let max_flare = max_step + 0.15;

        let pt_to_seg_dist = |px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64| -> f64 {
            let dx = bx - ax;
            let dy = by - ay;
            let len2 = dx * dx + dy * dy;
            let t = if len2 > 1e-18 {
                (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            (px - (ax + t * dx)).hypot(py - (ay + t * dy))
        };

        let poly_area = |pts: &[Point2]| -> f64 {
            if pts.len() < 3 {
                return 0.0;
            }
            let mut sum = 0.0;
            for i in 0..pts.len() {
                let (x0, y0) = pts[i].to_mm();
                let (x1, y1) = pts[(i + 1) % pts.len()].to_mm();
                sum += x0 * y1 - x1 * y0;
            }
            0.5 * sum.abs()
        };

        for layer in &plate.layers {
            for p in &layer.paths {
                if matches!(
                    p.role,
                    ExtrusionRole::Support | ExtrusionRole::SupportInterface
                ) {
                    for pt in &p.points {
                        for reg in &layer.slice_regions {
                            assert!(
                                !reg.contains_point(*pt),
                                "support collided with model at layer {}",
                                layer.index
                            );
                        }
                    }
                }
            }
        }

        let mut trunk_areas = Vec::new();
        let mut mid_areas = Vec::new();
        for i in 1..plate.layers.len() {
            let cur = &plate.layers[i];
            let prev = &plate.layers[i - 1];
            let cur_sup: Vec<&ExtrusionPath> = cur
                .paths
                .iter()
                .filter(|p| p.role == ExtrusionRole::Support && p.closed && p.points.len() >= 3)
                .collect();
            let prev_sup: Vec<&ExtrusionPath> = prev
                .paths
                .iter()
                .filter(|p| {
                    matches!(
                        p.role,
                        ExtrusionRole::Support | ExtrusionRole::SupportInterface
                    ) && p.closed
                        && !p.points.is_empty()
                })
                .collect();

            let prev_outer_polys: Vec<Polygon> = prev_sup
                .iter()
                .map(|q| Polygon::new(q.points.clone()))
                .collect();

            for p in &cur_sup {
                for pt in &p.points {
                    if prev_outer_polys.iter().any(|poly| poly.contains_point(*pt)) {
                        continue;
                    }
                    let (px, py) = pt.to_mm();
                    let min_d = prev_sup
                        .iter()
                        .map(|q| {
                            let qn = q.points.len();
                            (0..qn)
                                .map(|si| {
                                    let a = q.points[si].to_mm();
                                    let b = q.points[(si + 1) % qn].to_mm();
                                    pt_to_seg_dist(px, py, a.0, a.1, b.0, b.1)
                                })
                                .fold(f64::INFINITY, f64::min)
                        })
                        .fold(f64::INFINITY, f64::min);
                    assert!(
                        min_d <= max_flare,
                        "steep support overhang on layer {} (z={:.2} mm): {min_d:.3} mm > {max_flare:.3} mm",
                        cur.index,
                        cur.z_top_mm
                    );
                }
            }

            let outer_polys: Vec<(f64, Polygon)> = cur_sup
                .iter()
                .map(|p| {
                    let poly = Polygon::new(p.points.clone());
                    (poly_area(&p.points), poly)
                })
                .collect();
            let mut layer_outer_areas = Vec::new();
            for (idx, (area, poly)) in outer_polys.iter().enumerate() {
                let sample = poly.points[0];
                let nested = outer_polys
                    .iter()
                    .enumerate()
                    .any(|(j, (other_area, other))| {
                        j != idx && *other_area > *area + 1e-3 && other.contains_point(sample)
                    });
                if !nested {
                    layer_outer_areas.push(*area);
                }
            }
            if !layer_outer_areas.is_empty() {
                if (1.0..=3.6).contains(&cur.z_top_mm) {
                    trunk_areas.extend(layer_outer_areas);
                } else if (4.6..=8.4).contains(&cur.z_top_mm) {
                    mid_areas.extend(layer_outer_areas);
                }
            }
        }

        assert!(!trunk_areas.is_empty() && !mid_areas.is_empty());
        let mean_trunk = trunk_areas.iter().sum::<f64>() / (trunk_areas.len() as f64);
        let mean_mid = mid_areas.iter().sum::<f64>() / (mid_areas.len() as f64);
        assert!(
            mean_trunk >= 0.65 * mean_mid,
            "inverted tree trunk detected: mean trunk area {mean_trunk:.2} mm2 < 0.65 * mid area {mean_mid:.2} mm2"
        );

        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for r in &plate.layers[50].slice_regions {
            if let Some(aabb) = r.aabb() {
                let (x0, y0) = aabb.min.to_mm();
                let (x1, y1) = aabb.max.to_mm();
                min_x = min_x.min(x0);
                min_y = min_y.min(y0);
                max_x = max_x.max(x1);
                max_y = max_y.max(y1);
            }
        }
        assert!(min_x < max_x && min_y < max_y);

        let top_iface: Vec<&ExtrusionPath> = plate.layers[48]
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::SupportInterface && !p.points.is_empty())
            .collect();
        assert!(
            !top_iface.is_empty(),
            "expected SupportInterface on layer 48 (z={:.2} mm)",
            plate.layers[48].z_top_mm
        );

        let min_iface_dist = |px: f64, py: f64| -> f64 {
            top_iface
                .iter()
                .map(|q| {
                    let qn = q.points.len();
                    if qn == 1 {
                        let (ax, ay) = q.points[0].to_mm();
                        return (px - ax).hypot(py - ay);
                    }
                    let seg_count = if q.closed { qn } else { qn - 1 };
                    (0..seg_count)
                        .map(|si| {
                            let a = q.points[si].to_mm();
                            let b = q.points[(si + 1) % qn].to_mm();
                            pt_to_seg_dist(px, py, a.0, a.1, b.0, b.1)
                        })
                        .fold(f64::INFINITY, f64::min)
                })
                .fold(f64::INFINITY, f64::min)
        };

        let mid_y = 0.5 * (min_y + max_y);
        for (label, px, py, max_dist) in [
            (
                "outer front-right corner",
                max_x - 0.225,
                min_y + 0.225,
                0.50,
            ),
            (
                "outer back-right corner",
                max_x - 0.225,
                max_y - 0.225,
                0.50,
            ),
            ("right-edge midpoint", max_x - 0.225, mid_y, 0.50),
            ("overhang center", min_x + 15.0, mid_y, 0.60),
        ] {
            let d = min_iface_dist(px, py);
            assert!(
                d <= max_dist,
                "layer 48 SupportInterface missed {label} ({px:.2}, {py:.2}): min dist {d:.3} mm > {max_dist:.3} mm"
            );
        }
    }

    #[test]
    fn tree_support_bridge_arch_merges_branches_and_saves_filament() {
        let mesh = make_bridge_arch_mesh(5.0, 16.0, 10.0, 10.0, 2.0);
        let mut cfg_tree = PrintConfig::default();
        cfg_tree.process.support.enabled = true;
        cfg_tree.process.support.style = SupportStyle::Tree;
        cfg_tree.process.support.interface_layers = 2;

        let plate_tree = slice_single_mesh(&mesh, &cfg_tree).unwrap();
        let mut cfg_grid = cfg_tree.clone();
        cfg_grid.process.support.style = SupportStyle::Grid;
        let plate_grid = slice_single_mesh(&mesh, &cfg_grid).unwrap();

        let sup_len = |plate: &SlicedPlate| -> f64 {
            plate
                .layers
                .iter()
                .flat_map(|l| l.paths.iter())
                .filter(|p| {
                    matches!(
                        p.role,
                        ExtrusionRole::Support | ExtrusionRole::SupportInterface
                    )
                })
                .map(path_length_mm)
                .sum()
        };

        let len_tree = sup_len(&plate_tree);
        let len_grid = sup_len(&plate_grid);
        assert!(len_tree > 0.0);
        assert!(
            len_tree < len_grid,
            "tree length {len_tree:.1} mm not less than grid {len_grid:.1} mm"
        );

        let top_tip_count = plate_tree.layers[46]
            .paths
            .iter()
            .filter(|p| {
                matches!(
                    p.role,
                    ExtrusionRole::Support | ExtrusionRole::SupportInterface
                )
            })
            .count();
        let base_trunk_count = plate_tree.layers[1]
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::Support && p.closed)
            .count();
        assert!(base_trunk_count >= 1);
        assert!(
            base_trunk_count < top_tip_count,
            "expected branches to merge into fewer trunks: base={base_trunk_count} vs top={top_tip_count}"
        );
    }

    #[test]
    fn tree_support_overhang_ramp_supports_60deg_slope() {
        let mesh = make_overhang_ramp_mesh(10.0, 12.0, 10.0, 60.0);
        let mut cfg = PrintConfig::default();
        cfg.process.support.enabled = true;
        cfg.process.support.style = SupportStyle::Tree;
        cfg.process.support.threshold_angle_deg = 45.0;
        cfg.process.support.interface_layers = 2;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let mut saw_sup = false;
        let mut saw_iface = false;
        for layer in &plate.layers {
            for p in &layer.paths {
                if p.role == ExtrusionRole::Support {
                    saw_sup = true;
                } else if p.role == ExtrusionRole::SupportInterface {
                    saw_iface = true;
                }
                if matches!(
                    p.role,
                    ExtrusionRole::Support | ExtrusionRole::SupportInterface
                ) {
                    for pt in &p.points {
                        for reg in &layer.slice_regions {
                            assert!(!reg.contains_point(*pt));
                        }
                    }
                }
            }
        }
        assert!(saw_sup, "expected Support paths on 60 deg overhang ramp");
        assert!(saw_iface, "expected SupportInterface paths on ramp");
    }

    #[test]
    fn tree_support_never_emits_floating_midair_circles() {
        let box_at = |x: f64, y: f64, z: f64, w: f64, d: f64, h: f64| -> Vec<Triangle> {
            make_box_mesh(w, d, h)
                .triangles
                .iter()
                .map(|t| {
                    let mv = |q: Point3| Point3::new(q.x + x, q.y + y, q.z + z);
                    Triangle::new(mv(t.a), mv(t.b), mv(t.c))
                })
                .collect()
        };

        let mut tris = Vec::new();
        // Thin vertical spine wall with staggered ribs on BOTH sides (similar to thin walls with ears)
        tris.extend(box_at(0.0, 0.0, 0.0, 3.0, 80.0, 26.0));
        for (k, z) in [6.0, 10.0, 14.0, 18.0].into_iter().enumerate() {
            let y0 = 6.0 + (k as f64) * 14.0;
            tris.extend(box_at(3.0, y0, z, 5.0, 8.0, 1.8));
            tris.extend(box_at(-5.0, y0 + 5.0, z + 1.0, 5.0, 8.0, 1.8));
        }
        // Multi-level stacked shelves + nearby pillar & parallel wall
        for (k, z) in [8.0, 14.0, 20.0].into_iter().enumerate() {
            tris.extend(box_at(
                14.0,
                10.0 + (k as f64) * 5.0,
                z,
                12.0,
                48.0 - (k as f64) * 8.0,
                1.6,
            ));
        }
        tris.extend(box_at(19.0, 28.0, 0.0, 4.0, 4.0, 26.0));
        tris.extend(box_at(29.0, 14.0, 0.0, 4.0, 50.0, 24.0));
        let composite = TriangleMesh::new(tris).unwrap();

        for plate_only in [false, true] {
            let mut cfg = PrintConfig::default();
            cfg.process.support.enabled = true;
            cfg.process.support.style = SupportStyle::Tree;
            cfg.process.support.on_build_plate_only = plate_only;
            cfg.process.support.interface_layers = 2;
            cfg.process.support.tree.max_branch_angle_deg = 45.0;

            let plate = slice_single_mesh(&composite, &cfg).unwrap();
            let mut saw_sup = false;
            let mut saw_iface = false;
            let has_l0_sup = plate.layers[0]
                .paths
                .iter()
                .any(|p| p.role == ExtrusionRole::Support);
            assert!(
                has_l0_sup,
                "expected layer 0 trunks for plate_only={plate_only}"
            );

            let max_step = cfg.process.layer_height_mm
                * cfg
                    .process
                    .support
                    .tree
                    .max_branch_angle_deg
                    .to_radians()
                    .tan();
            let reach_tol = max_step + cfg.process.support.line_width_mm + 0.35;

            for i in 1..plate.layers.len() {
                let cur = &plate.layers[i];
                let prev = &plate.layers[i - 1];
                let z_bot = cur.z_top_mm - cur.height_mm;
                let gap_target = (z_bot - cfg.process.support.bottom_z_distance_mm).max(0.0);

                for p in &cur.paths {
                    if !matches!(
                        p.role,
                        ExtrusionRole::Support | ExtrusionRole::SupportInterface
                    ) || !p.closed
                        || p.points.is_empty()
                    {
                        continue;
                    }
                    if p.role == ExtrusionRole::Support {
                        saw_sup = true;
                    } else {
                        saw_iface = true;
                    }
                    let n_pts = p.points.len() as f64;
                    let (mut cx, mut cy) = (0.0, 0.0);
                    for pt in &p.points {
                        let (x, y) = pt.to_mm();
                        cx += x / n_pts;
                        cy += y / n_pts;
                    }
                    let check_pts =
                        std::iter::once((cx, cy)).chain(p.points.iter().map(|pt| pt.to_mm()));

                    let pt_to_seg_dist =
                        |px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64| -> f64 {
                            let dx = bx - ax;
                            let dy = by - ay;
                            let len2 = dx * dx + dy * dy;
                            let t = if len2 > 1e-18 {
                                (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
                            } else {
                                0.0
                            };
                            (px - (ax + t * dx)).hypot(py - (ay + t * dy))
                        };

                    let supported_by_prev_sup = prev.paths.iter().any(|q| {
                        if !matches!(
                            q.role,
                            ExtrusionRole::Support | ExtrusionRole::SupportInterface
                        ) || q.points.is_empty()
                        {
                            return false;
                        }
                        let qn = q.points.len();
                        let seg_count = if q.closed { qn } else { qn.saturating_sub(1) };
                        let min_d = p
                            .points
                            .iter()
                            .copied()
                            .chain(Point2::from_mm(cx, cy).ok())
                            .map(|pt| {
                                let (px, py) = pt.to_mm();
                                (0..seg_count.max(1))
                                    .map(|si| {
                                        let a = q.points[si].to_mm();
                                        let b = if seg_count == 0 {
                                            a
                                        } else {
                                            q.points[(si + 1) % qn].to_mm()
                                        };
                                        pt_to_seg_dist(px, py, a.0, a.1, b.0, b.1)
                                    })
                                    .fold(f64::INFINITY, f64::min)
                            })
                            .fold(f64::INFINITY, f64::min);
                        min_d <= reach_tol
                    });

                    let supported_by_model = !plate_only
                        && z_bot > cfg.process.support.bottom_z_distance_mm
                        && plate.layers[..i].iter().any(|bl| {
                            (bl.z_top_mm - gap_target).abs() <= bl.height_mm * 0.75 + 1e-3
                                && check_pts.clone().any(|(px, py)| {
                                    Point2::from_mm(px, py).is_ok_and(|c| {
                                        bl.slice_regions.iter().any(|r| r.contains_point(c))
                                    })
                                })
                        });

                    assert!(
                        supported_by_prev_sup || supported_by_model,
                        "floating tree support contour on layer {} (z={:.2} mm, plate_only={plate_only}) at ({cx:.2}, {cy:.2})",
                        cur.index,
                        cur.z_top_mm
                    );
                }
            }
            assert!(
                saw_sup,
                "expected Support paths for plate_only={plate_only}"
            );
            assert!(
                saw_iface,
                "expected SupportInterface paths for plate_only={plate_only}"
            );
        }
    }

    #[test]
    fn adaptive_dome_and_top_shell_thickness_propagate_heights() {
        let dome = layercraft_mesh::build_sample_shape(layercraft_mesh::SampleShape::Dome).unwrap();
        let mut cfg = PrintConfig::default();
        cfg.process.adaptive_layer_height.enabled = true;
        cfg.process.adaptive_layer_height.min_layer_height_mm = Some(0.08);
        cfg.process.adaptive_layer_height.max_layer_height_mm = Some(0.28);
        cfg.process.adaptive_layer_height.quality = 0.6;
        cfg.process.top_shell_layers = 3;
        cfg.process.top_shell_thickness_mm = Some(0.6);

        let plate = slice_single_mesh(&dome, &cfg).unwrap();
        assert!((plate.layers.last().unwrap().z_top_mm - 18.0).abs() < 1e-6);

        let heights: Vec<f64> = plate.layers.iter().skip(1).map(|l| l.height_mm).collect();
        let min_h = heights.iter().copied().fold(f64::INFINITY, f64::min);
        let max_h = heights.iter().copied().fold(0.0, f64::max);
        assert!(
            min_h <= 0.10,
            "expected fine layers near dome cap, got {min_h}"
        );
        assert!(
            max_h >= 0.24,
            "expected thick layers on vertical base, got {max_h}"
        );

        // Every path in a variable-height layer carries the layer's exact height_mm.
        for layer in &plate.layers {
            for path in &layer.paths {
                assert!((path.height_mm - layer.height_mm).abs() < 1e-9);
            }
        }
        // Verify physical top_shell_thickness_mm propagates across > top_shell_layers thin layers.
        let flat_top = make_box_mesh(16.0, 16.0, 4.0);
        let mut cfg_thick = PrintConfig::default();
        cfg_thick.process.layer_height_mm = 0.08;
        cfg_thick.process.first_layer_height_mm = 0.2;
        cfg_thick.process.top_shell_layers = 2;
        cfg_thick.process.top_shell_thickness_mm = Some(0.6);
        let mut cfg_no_thick = cfg_thick.clone();
        cfg_no_thick.process.top_shell_thickness_mm = None;

        let plate_with_thick = slice_single_mesh(&flat_top, &cfg_thick).unwrap();
        let plate_no_thick = slice_single_mesh(&flat_top, &cfg_no_thick).unwrap();
        let top_region_solid_len = |pl: &SlicedPlate| -> f64 {
            pl.layers
                .iter()
                .filter(|l| l.z_top_mm > 2.0)
                .flat_map(|l| l.paths.iter())
                .filter(|p| {
                    matches!(
                        p.role,
                        ExtrusionRole::TopSurface | ExtrusionRole::InternalSolidInfill
                    )
                })
                .map(path_length_mm)
                .sum()
        };
        let len_with_thick = top_region_solid_len(&plate_with_thick);
        let len_without_thick = top_region_solid_len(&plate_no_thick);
        assert!(
            len_with_thick > len_without_thick * 2.5,
            "expected 0.6 mm top_shell_thickness to propagate across thin 0.08 mm layers ({len_with_thick:.1} vs {len_without_thick:.1})"
        );
    }

    #[test]
    fn height_ranges_override_height_infill_and_walls() {
        let mesh = make_box_mesh(20.0, 20.0, 12.0);
        let mut cfg = PrintConfig::default();
        cfg.process.height_ranges = vec![
            layercraft_config::HeightRangeModifier {
                min_z_mm: 0.0,
                max_z_mm: 4.0,
                layer_height_mm: Some(0.25),
                overrides: Default::default(),
            },
            layercraft_config::HeightRangeModifier {
                min_z_mm: 4.0,
                max_z_mm: 8.0,
                layer_height_mm: Some(0.10),
                overrides: layercraft_config::ModifierOverrides {
                    sparse_infill_density: Some(0.55),
                    sparse_infill_pattern: Some(InfillPattern::Gyroid),
                    ..Default::default()
                },
            },
            layercraft_config::HeightRangeModifier {
                min_z_mm: 8.0,
                max_z_mm: 12.0,
                layer_height_mm: Some(0.20),
                overrides: layercraft_config::ModifierOverrides {
                    wall_loops: Some(4),
                    ..Default::default()
                },
            },
        ];

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        assert!((plate.layers.last().unwrap().z_top_mm - 12.0).abs() < 1e-6);
        let mid_layer = plate
            .layers
            .iter()
            .find(|l| l.z_top_mm > 5.0 && l.z_top_mm < 7.0)
            .unwrap();
        assert!((mid_layer.height_mm - 0.10).abs() < 1e-4);

        let low_inner_walls = plate
            .layers
            .iter()
            .find(|l| l.z_top_mm > 1.5 && l.z_top_mm < 3.0)
            .unwrap()
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::InnerWall)
            .count();
        let top_inner_walls = plate
            .layers
            .iter()
            .find(|l| l.z_top_mm > 9.0 && l.z_top_mm < 11.0)
            .unwrap()
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::InnerWall)
            .count();
        assert_eq!(low_inner_walls, 1);
        assert_eq!(top_inner_walls, 3);
    }

    #[test]
    fn modifier_volumes_override_infill_walls_ironing_and_overlap_disjointly() {
        let mesh = make_box_mesh(30.0, 20.0, 12.0);
        let cfg = PrintConfig::default();

        // Modifier 1: high infill + more walls in center Box
        let box_raw =
            layercraft_mesh::build_modifier_primitive(layercraft_mesh::ModifierPrimitive::Box {
                width_mm: 12.0,
                depth_mm: 12.0,
                height_mm: 12.0,
            })
            .unwrap();
        let box_mod = ModifierVolume {
            id: 1,
            name: "dense_core".to_owned(),
            role: layercraft_config::ModifierRole::Modifier,
            mesh: layercraft_mesh::prepare_modifier_mesh(
                &box_raw,
                layercraft_mesh::ModifierTransform {
                    position_xyz_mm: [125.0, 125.0, 6.0],
                    ..Default::default()
                },
            )
            .unwrap(),
            overrides: layercraft_config::ModifierOverrides {
                sparse_infill_density: Some(0.60),
                sparse_infill_pattern: Some(InfillPattern::Gyroid),
                wall_loops: Some(4),
                ..Default::default()
            },
        };

        // Modifier 2: overlapping Cylinder overriding density even higher (0.80) + topmost ironing
        let cyl_raw = layercraft_mesh::build_modifier_primitive(
            layercraft_mesh::ModifierPrimitive::Cylinder {
                radius_mm: 4.0,
                height_mm: 6.0,
                segments: 24,
            },
        )
        .unwrap();
        let cyl_mod = ModifierVolume {
            id: 2,
            name: "iron_boss".to_owned(),
            role: layercraft_config::ModifierRole::Modifier,
            mesh: layercraft_mesh::prepare_modifier_mesh(
                &cyl_raw,
                layercraft_mesh::ModifierTransform {
                    position_xyz_mm: [125.0, 125.0, 10.0],
                    ..Default::default()
                },
            )
            .unwrap(),
            overrides: layercraft_config::ModifierOverrides {
                sparse_infill_density: Some(0.80),
                ironing_mode: Some(IroningType::TopmostSurface),
                ..Default::default()
            },
        };

        // Out-of-bounds modifier outside the bed/part should cleanly be a no-op without panicking
        let off_mod = ModifierVolume {
            id: 3,
            name: "off_bed".to_owned(),
            role: layercraft_config::ModifierRole::Modifier,
            mesh: layercraft_mesh::prepare_modifier_mesh(
                &box_raw,
                layercraft_mesh::ModifierTransform {
                    position_xyz_mm: [10.0, 10.0, 5.0],
                    ..Default::default()
                },
            )
            .unwrap(),
            overrides: layercraft_config::ModifierOverrides {
                sparse_infill_density: Some(0.90),
                ..Default::default()
            },
        };

        let base_plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let mod_plate =
            slice_single_mesh_with_modifiers(&mesh, &cfg, &[box_mod, cyl_mod, off_mod]).unwrap();

        let infill_len = |plate: &SlicedPlate| -> f64 {
            plate
                .layers
                .iter()
                .flat_map(|l| l.paths.iter())
                .filter(|p| p.role == ExtrusionRole::SparseInfill)
                .map(path_length_mm)
                .sum()
        };
        assert!(infill_len(&mod_plate) > infill_len(&base_plate));

        // Topmost ironing present ONLY on the last layer, and strictly inside cylinder footprint
        let iron_lower = mod_plate.layers[..mod_plate.layers.len() - 1]
            .iter()
            .flat_map(|l| l.paths.iter())
            .filter(|p| p.role == ExtrusionRole::Ironing)
            .count();
        assert_eq!(iron_lower, 0);
        let iron_top: Vec<&ExtrusionPath> = mod_plate
            .layers
            .last()
            .unwrap()
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::Ironing)
            .collect();
        assert!(!iron_top.is_empty());
        for p in iron_top {
            for pt in &p.points {
                let (x, y) = pt.to_mm();
                assert!(
                    (x - 125.0).hypot(y - 125.0) <= 4.1,
                    "ironing leaked outside modifier cylinder at ({x:.2}, {y:.2})"
                );
            }
        }
    }

    #[test]
    fn support_blocker_and_enforcer_modifiers_control_supports() {
        // 1) SupportBlocker removes tree support over CBracket overhang
        let cbracket = make_c_bracket_mesh(20.0, 8.0, 3.0, 5.0, 8.0, 3.0);
        let mut cfg_sup = PrintConfig::default();
        cfg_sup.process.support.enabled = true;
        cfg_sup.process.support.style = SupportStyle::Tree;
        cfg_sup.process.support.on_build_plate_only = true;

        let blocker_box =
            layercraft_mesh::build_modifier_primitive(layercraft_mesh::ModifierPrimitive::Box {
                width_mm: 30.0,
                depth_mm: 20.0,
                height_mm: 16.0,
            })
            .unwrap();
        let blocker = ModifierVolume {
            id: 1,
            name: "blocker".to_owned(),
            role: layercraft_config::ModifierRole::SupportBlocker,
            mesh: layercraft_mesh::prepare_modifier_mesh(
                &blocker_box,
                layercraft_mesh::ModifierTransform {
                    position_xyz_mm: [125.0, 125.0, 8.0],
                    ..Default::default()
                },
            )
            .unwrap(),
            overrides: Default::default(),
        };
        let blocked_plate =
            slice_single_mesh_with_modifiers(&cbracket, &cfg_sup, &[blocker]).unwrap();
        let sup_count = blocked_plate
            .layers
            .iter()
            .flat_map(|l| l.paths.iter())
            .filter(|p| {
                matches!(
                    p.role,
                    ExtrusionRole::Support | ExtrusionRole::SupportInterface
                )
            })
            .count();
        assert_eq!(
            sup_count, 0,
            "support blocker should suppress all support paths"
        );

        // 2) SupportEnforcer forces support under BridgeArch span even when global support.enabled == false
        let bridge = make_bridge_arch_mesh(5.0, 16.0, 10.0, 10.0, 2.0);
        let mut cfg_no_sup = PrintConfig::default();
        cfg_no_sup.process.support.enabled = false;
        cfg_no_sup.process.support.interface_layers = 2;

        let cyl_raw = layercraft_mesh::build_modifier_primitive(
            layercraft_mesh::ModifierPrimitive::Cylinder {
                radius_mm: 4.5,
                height_mm: 12.0,
                segments: 24,
            },
        )
        .unwrap();
        let enforcer = ModifierVolume {
            id: 2,
            name: "enforcer".to_owned(),
            role: layercraft_config::ModifierRole::SupportEnforcer,
            mesh: layercraft_mesh::prepare_modifier_mesh(
                &cyl_raw,
                layercraft_mesh::ModifierTransform {
                    position_xyz_mm: [125.0, 125.0, 6.0],
                    ..Default::default()
                },
            )
            .unwrap(),
            overrides: Default::default(),
        };
        let enforced_plate =
            slice_single_mesh_with_modifiers(&bridge, &cfg_no_sup, &[enforcer]).unwrap();
        let has_sup = enforced_plate
            .layers
            .iter()
            .flat_map(|l| l.paths.iter())
            .any(|p| p.role == ExtrusionRole::Support);
        let has_iface = enforced_plate
            .layers
            .iter()
            .flat_map(|l| l.paths.iter())
            .any(|p| p.role == ExtrusionRole::SupportInterface);
        assert!(has_sup, "support enforcer should emit Support paths");
        assert!(
            has_iface,
            "support enforcer should emit SupportInterface paths"
        );

        // 3) Hostile modifier overrides fail cleanly via ConfigError
        let bad_mod = ModifierVolume {
            id: 3,
            name: "hostile".to_owned(),
            role: layercraft_config::ModifierRole::Modifier,
            mesh: blocker_box,
            overrides: layercraft_config::ModifierOverrides {
                sparse_infill_density: Some(-0.5),
                ..Default::default()
            },
        };
        assert!(
            slice_single_mesh_with_modifiers(&mesh_for_bad_mod(), &cfg_no_sup, &[bad_mod]).is_err()
        );
    }

    fn mesh_for_bad_mod() -> TriangleMesh {
        make_box_mesh(10.0, 10.0, 4.0)
    }

    #[test]
    fn elephant_foot_and_xy_contour_hole_compensations_shift_dimensions() {
        let tube =
            layercraft_mesh::build_sample_shape(layercraft_mesh::SampleShape::HollowTube).unwrap();

        let base = slice_single_mesh(&tube, &PrintConfig::default()).unwrap();
        let mut comp_cfg = PrintConfig::default();
        comp_cfg.process.elephant_foot_compensation_mm = 0.25;
        comp_cfg.process.xy_contour_compensation_mm = -0.30;
        comp_cfg.process.xy_hole_compensation_mm = 0.35;
        let comp = slice_single_mesh(&tube, &comp_cfg).unwrap();

        let span_of = |layer_idx: usize, plate: &SlicedPlate| -> (f64, f64) {
            let l = &plate.layers[layer_idx];
            let reg = &l.slice_regions[0];
            let ob = reg.outer.aabb().unwrap();
            let hb = reg.holes[0].aabb().unwrap();
            let outer_w = ob.max.to_mm().0 - ob.min.to_mm().0;
            let hole_w = hb.max.to_mm().0 - hb.min.to_mm().0;
            (outer_w, hole_w)
        };

        let (base_l0_o, base_l0_h) = span_of(0, &base);
        let (comp_l0_o, comp_l0_h) = span_of(0, &comp);
        assert!(
            comp_l0_o < base_l0_o - 0.9,
            "layer 0 outer width should shrink by ~1.10mm: {base_l0_o} -> {comp_l0_o}"
        );
        assert!(
            comp_l0_h > base_l0_h + 1.0,
            "layer 0 hole width should grow by ~1.20mm: {base_l0_h} -> {comp_l0_h}"
        );

        let (base_l5_o, base_l5_h) = span_of(5, &base);
        let (comp_l5_o, comp_l5_h) = span_of(5, &comp);
        assert!(
            (comp_l5_o - (base_l5_o - 0.60)).abs() < 0.05,
            "layer 5 outer width expected {:.2}, got {:.2}",
            base_l5_o - 0.60,
            comp_l5_o
        );
        assert!(
            (comp_l5_h - (base_l5_h + 0.70)).abs() < 0.05,
            "layer 5 hole width expected {:.2}, got {:.2}",
            base_l5_h + 0.70,
            comp_l5_h
        );
    }

    #[test]
    fn wall_order_emits_inner_outer_outer_inner_and_inner_outer_inner() {
        let box_mesh = make_box_mesh(24.0, 24.0, 2.0);
        for wall_gen in [WallGenerator::Classic, WallGenerator::Arachne] {
            for (order, expected) in [
                (
                    WallOrder::InnerOuter,
                    vec![
                        ExtrusionRole::InnerWall,
                        ExtrusionRole::InnerWall,
                        ExtrusionRole::OuterWall,
                    ],
                ),
                (
                    WallOrder::OuterInner,
                    vec![
                        ExtrusionRole::OuterWall,
                        ExtrusionRole::InnerWall,
                        ExtrusionRole::InnerWall,
                    ],
                ),
                (
                    WallOrder::InnerOuterInner,
                    vec![
                        ExtrusionRole::InnerWall,
                        ExtrusionRole::OuterWall,
                        ExtrusionRole::InnerWall,
                    ],
                ),
            ] {
                let mut cfg = PrintConfig::default();
                cfg.process.wall_generator = wall_gen;
                cfg.process.wall_loops = 3;
                cfg.process.wall_order = order;
                let plate = slice_single_mesh(&box_mesh, &cfg).unwrap();
                let roles: Vec<ExtrusionRole> = plate.layers[1]
                    .paths
                    .iter()
                    .filter(|p| {
                        matches!(p.role, ExtrusionRole::OuterWall | ExtrusionRole::InnerWall)
                    })
                    .map(|p| p.role)
                    .collect();
                assert_eq!(roles, expected, "wall_gen={wall_gen:?} order={order:?}");
            }
        }
    }

    #[test]
    fn concentric_top_and_bottom_surfaces_emit_closed_nested_rings() {
        let box_mesh = make_box_mesh(20.0, 20.0, 4.0);
        let mut cfg = PrintConfig::default();
        cfg.process.top_surface_pattern = SurfacePattern::Concentric;
        cfg.process.bottom_surface_pattern = SurfacePattern::Concentric;
        let plate = slice_single_mesh(&box_mesh, &cfg).unwrap();

        let bot_paths: Vec<&ExtrusionPath> = plate.layers[0]
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::BottomSurface)
            .collect();
        assert!(bot_paths.len() >= 8);
        assert!(bot_paths.iter().all(|p| p.closed));

        let top_paths: Vec<&ExtrusionPath> = plate
            .layers
            .last()
            .unwrap()
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::TopSurface)
            .collect();
        assert!(top_paths.len() >= 8);
        assert!(top_paths.iter().all(|p| p.closed));
    }

    #[test]
    fn nearest_neighbor_island_ordering_groups_islands_and_reduces_travel() {
        let raw_box = make_box_mesh(16.0, 16.0, 2.0);
        let m1 = layercraft_mesh::prepare_transformed_object(
            &raw_box,
            layercraft_mesh::ObjectTransform {
                position_xy_mm: [40.0, 125.0],
                ..Default::default()
            },
        )
        .unwrap();
        let m2 = layercraft_mesh::prepare_transformed_object(
            &raw_box,
            layercraft_mesh::ObjectTransform {
                position_xy_mm: [200.0, 125.0],
                ..Default::default()
            },
        )
        .unwrap();
        let cfg = PrintConfig::default();
        let plate = slice_meshes(&[m1, m2], &cfg).unwrap();
        let top_layer = plate.layers.last().unwrap();

        // Expect island transitions (left <-> right) <= 1 because each island prints all of
        // its walls + skin before traveling to the other island.
        let mut sides: Vec<bool> = Vec::new();
        for p in &top_layer.paths {
            let x = p.points[0].to_mm().0;
            let right = x > 125.0;
            if sides.last().copied() != Some(right) {
                sides.push(right);
            }
        }
        assert_eq!(
            sides.len(),
            2,
            "expected 2 grouped islands, got {:?}",
            sides
        );
    }

    #[test]
    fn internal_bridge_detected_over_sparse_infill_and_skipped_over_solid() {
        let cube = make_box_mesh(20.0, 20.0, 8.0);
        let mut cfg = PrintConfig::default();
        cfg.process.top_shell_layers = 4;
        cfg.process.bottom_shell_layers = 3;
        cfg.process.sparse_infill_density = 0.15;
        cfg.process.detect_bridges = true;

        let plate = slice_single_mesh(&cube, &cfg).unwrap();
        let ib_layers: Vec<usize> = plate
            .layers
            .iter()
            .filter(|l| {
                l.paths
                    .iter()
                    .any(|p| p.role == ExtrusionRole::InternalBridge)
            })
            .map(|l| l.index)
            .collect();
        assert_eq!(
            ib_layers.len(),
            1,
            "expected 1 internal bridge layer: {ib_layers:?}"
        );

        // Stepped block (30x30x6 base + 16x16x6 top) has two top-shell transitions.
        let mut tris = Vec::new();
        tris.extend(make_box_mesh(30.0, 30.0, 6.0).triangles);
        let top = make_box_mesh(16.0, 16.0, 6.0);
        for t in top.triangles {
            tris.push(layercraft_geom::Triangle::new(
                Point3::new(t.a.x + 7.0, t.a.y + 7.0, t.a.z + 6.0),
                Point3::new(t.b.x + 7.0, t.b.y + 7.0, t.b.z + 6.0),
                Point3::new(t.c.x + 7.0, t.c.y + 7.0, t.c.z + 6.0),
            ));
        }
        let stepped = TriangleMesh::new(tris).unwrap();
        let stepped_plate = slice_single_mesh(&stepped, &cfg).unwrap();
        let stepped_ib_layers: usize = stepped_plate
            .layers
            .iter()
            .filter(|l| {
                l.paths
                    .iter()
                    .any(|p| p.role == ExtrusionRole::InternalBridge)
            })
            .count();
        assert_eq!(stepped_ib_layers, 2);

        // 100% sparse infill density has no sparse voids to bridge over.
        let mut solid_cfg = cfg.clone();
        solid_cfg.process.sparse_infill_density = 1.0;
        let solid_plate = slice_single_mesh(&cube, &solid_cfg).unwrap();
        let solid_ib: usize = solid_plate
            .layers
            .iter()
            .filter(|l| {
                l.paths
                    .iter()
                    .any(|p| p.role == ExtrusionRole::InternalBridge)
            })
            .count();
        assert_eq!(solid_ib, 0);
    }

    #[test]
    fn gyroid_waves_chain_cleanly_inside_region_without_crossing_holes() {
        let reg = vec![ExPolygon::new(
            Polygon::new(vec![
                Point2::from_mm(0.0, 0.0).unwrap(),
                Point2::from_mm(20.0, 0.0).unwrap(),
                Point2::from_mm(20.0, 20.0).unwrap(),
                Point2::from_mm(0.0, 20.0).unwrap(),
            ]),
            Vec::new(),
        )];
        let chained = generate_gyroid_infill(&reg, 1.0, 0.15, 0.45, 0.20, 120.0).unwrap();
        assert!(
            !chained.is_empty() && chained.len() <= 3,
            "expected gyroid waves to chain into <=3 paths on solid 20x20, got {}",
            chained.len()
        );

        let hole_center = Point2::from_mm(10.0, 10.0).unwrap();
        let holed = vec![ExPolygon::new(
            Polygon::new(vec![
                Point2::from_mm(0.0, 0.0).unwrap(),
                Point2::from_mm(20.0, 0.0).unwrap(),
                Point2::from_mm(20.0, 20.0).unwrap(),
                Point2::from_mm(0.0, 20.0).unwrap(),
            ]),
            vec![Polygon::new(vec![
                Point2::from_mm(6.0, 6.0).unwrap(),
                Point2::from_mm(6.0, 14.0).unwrap(),
                Point2::from_mm(14.0, 14.0).unwrap(),
                Point2::from_mm(14.0, 6.0).unwrap(),
            ])],
        )];
        let holed_paths = generate_gyroid_infill(&holed, 1.0, 0.25, 0.45, 0.20, 120.0).unwrap();
        assert!(!holed_paths.is_empty());
        for p in &holed_paths {
            for w in p.points.windows(2) {
                let mid = Point2::new((w[0].x + w[1].x) / 2, (w[0].y + w[1].y) / 2);
                assert!(
                    (mid.x - hole_center.x).abs() > 2_000_000
                        || (mid.y - hole_center.y).abs() > 2_000_000,
                    "gyroid segment crossed central hole at {:?}",
                    mid.to_mm()
                );
            }
        }
    }

    #[test]
    fn kinematic_time_estimator_matches_analytical_trapezoid_and_corner_physics() {
        // Triangular move: d = 4 < v^2/a = 100^2 / 1000 = 10 -> 2*sqrt(4/1000)
        let tri_t = estimate_trapezoid_move_time_s(4.0, 100.0, 1000.0);
        assert!((tri_t - 2.0 * (4.0_f64 / 1000.0).sqrt()).abs() < 1e-9);

        // Trapezoidal move: d = 50 >= 10 -> 50/100 + 100/1000 = 0.6s
        let trap_t = estimate_trapezoid_move_time_s(50.0, 100.0, 1000.0);
        assert!((trap_t - 0.6).abs() < 1e-9);

        let straight = estimate_polyline_kinematic_time_s(
            &[(0.0, 0.0), (10.0, 0.0), (20.0, 0.0)],
            &[100.0, 100.0],
            2000.0,
            0.05,
        );
        let corner = estimate_polyline_kinematic_time_s(
            &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)],
            &[100.0, 100.0],
            2000.0,
            0.05,
        );
        assert!(
            corner > straight,
            "90 deg corner ({corner}) must take longer than straight ({straight})"
        );

        let corner_slow_accel = estimate_polyline_kinematic_time_s(
            &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)],
            &[100.0, 100.0],
            500.0,
            0.05,
        );
        assert!(corner_slow_accel > corner);
    }

    #[test]
    fn mask_clip_index_matches_hole_queries_and_segment_splits() {
        let outer = Polygon::new(vec![
            Point2::from_mm(0.0, 0.0).unwrap(),
            Point2::from_mm(20.0, 0.0).unwrap(),
            Point2::from_mm(20.0, 20.0).unwrap(),
            Point2::from_mm(0.0, 20.0).unwrap(),
        ]);
        let hole = Polygon::new(vec![
            Point2::from_mm(6.0, 6.0).unwrap(),
            Point2::from_mm(6.0, 14.0).unwrap(),
            Point2::from_mm(14.0, 14.0).unwrap(),
            Point2::from_mm(14.0, 6.0).unwrap(),
        ]);
        let region = vec![ExPolygon::new(outer, vec![hole])];
        let mask = MaskClipIndex::from_regions(&region);
        assert!(mask.contains_point(Point2::from_mm(3.0, 3.0).unwrap()));
        assert!(!mask.contains_point(Point2::from_mm(10.0, 10.0).unwrap()));
        assert!(mask.contains_point(Point2::from_mm(0.0, 10.0).unwrap()));

        let split = mask.split_segment(
            Point2::from_mm(-2.0, 10.0).unwrap(),
            Point2::from_mm(22.0, 10.0).unwrap(),
        );
        // Should split into: [-2..0 out], [0..6 in], [6..14 out (hole)], [14..20 in], [20..22 out].
        assert_eq!(split.len(), 5);
        assert!(!split[0].2);
        assert!(split[1].2);
        assert!(!split[2].2);
        assert!(split[3].2);
        assert!(!split[4].2);
    }

    #[test]
    fn negative_part_modifier_cuts_void_through_sliced_cube() {
        let cube = make_box_mesh(20.0, 20.0, 10.0);
        let cutter = make_box_mesh(6.0, 6.0, 12.0);
        let prepared_cube = prepare_for_plate(&cube, 250.0, 250.0).unwrap();
        let mut prepared_cutter = prepare_for_plate(&cutter, 250.0, 250.0).unwrap();
        prepared_cutter.translate(0.0, 0.0, -1.0).unwrap();

        let mods = vec![ModifierVolume {
            id: 1,
            name: "Center Hole".to_owned(),
            role: layercraft_config::ModifierRole::NegativePart,
            mesh: prepared_cutter,
            overrides: ModifierOverrides::default(),
        }];
        let cfg = PrintConfig::default();
        let plate = slice_prepared_mesh_with_modifiers(&prepared_cube, &cfg, &mods).unwrap();
        assert!(!plate.layers.is_empty());
        let l5 = &plate.layers[5];
        assert!(
            !l5.slice_regions[0].holes.is_empty(),
            "expected hole cut out of cube"
        );
    }

    #[test]
    fn classic_gap_fill_fills_narrow_rib_channel() {
        // With outer_w = 0.42, inner_w = 0.45, wall_loops = 2:
        // Total 1.12 mm rib fits ring 0 (0.84 mm across both sides) cleanly, leaving ~0.28 mm center gap.
        let rib = make_box_mesh(1.12, 20.0, 2.0);
        let mut cfg = PrintConfig::default();
        cfg.process.wall_loops = 2;
        cfg.process.wall_generator = WallGenerator::Classic;
        cfg.process.gap_fill_enabled = true;
        cfg.process.gap_fill_speed_mm_s = Some(48.0);
        cfg.process.gap_fill_min_width_mm = 0.12;
        cfg.filament.slow_down_layer_time_s = 0.0;

        let plate = slice_single_mesh(&rib, &cfg).unwrap();
        let gap_paths: Vec<_> = plate.layers[2]
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::GapFill)
            .collect();
        assert!(
            !gap_paths.is_empty(),
            "expected classic GapFill paths in 1.12 mm rib"
        );
        assert!((gap_paths[0].speed_mm_s - 48.0).abs() < 1e-6);
    }

    #[test]
    fn classic_gap_fill_connects_tapered_wedge_tip_to_outer_wall() {
        let wedge = make_tapered_wedge_mesh(36.0, 4.0, 0.25, 4.0);
        let mut cfg = PrintConfig::default();
        cfg.process.wall_generator = WallGenerator::Classic;
        cfg.process.wall_loops = 2;
        cfg.process.outer_wall_width_mm = 0.45;
        cfg.process.inner_wall_width_mm = 0.45;
        cfg.process.gap_fill_enabled = true;

        let plate = slice_single_mesh(&wedge, &cfg).unwrap();
        let mid = &plate.layers[10];
        let outer_max_x = mid
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::OuterWall)
            .flat_map(|p| p.points.iter())
            .map(|pt| pt.to_mm().0)
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(outer_max_x.is_finite());

        let gap_paths: Vec<_> = mid
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::GapFill)
            .collect();
        let tip_gap = gap_paths
            .iter()
            .copied()
            .find(|p| {
                p.points
                    .iter()
                    .map(|pt| pt.to_mm().0)
                    .fold(f64::NEG_INFINITY, f64::max)
                    > outer_max_x + 0.5
            })
            .expect("expected GapFill tip path extending tapered wedge");
        let tip_min_x = tip_gap
            .points
            .iter()
            .map(|pt| pt.to_mm().0)
            .fold(f64::INFINITY, f64::min);
        let gap = tip_min_x - outer_max_x;
        assert!(
            gap <= 0.20,
            "Classic GapFill tip detached from OuterWall: {gap:.3} mm > 0.20 mm (outer_max_x={outer_max_x:.3}, tip_min_x={tip_min_x:.3})"
        );

        for gp in &gap_paths {
            for pt in &gp.points {
                assert!(
                    mid.slice_regions.iter().any(|r| r.contains_point(*pt)),
                    "GapFill endpoint {:?} poked outside model slice region",
                    pt.to_mm()
                );
            }
        }
    }

    #[test]
    fn progressive_overhang_speeds_slow_steeper_bands() {
        // Stepped block widens cleanly by progressive overhang steps:
        // step1 (+0.14 mm flare -> 2/4 = 40 mm/s), step2 (+0.31 mm flare from step1 -> 3/4 = 25 mm/s).
        let base = make_box_mesh(10.0, 10.0, 2.0);
        let step1 = make_box_mesh(10.28, 10.0, 2.0);
        let step2 = make_box_mesh(10.90, 10.0, 2.0);
        let p_base = prepare_for_plate(&base, 250.0, 250.0).unwrap();
        let mut p_step1 = prepare_for_plate(&step1, 250.0, 250.0).unwrap();
        let mut p_step2 = prepare_for_plate(&step2, 250.0, 250.0).unwrap();
        p_step1.translate(0.0, 0.0, 2.0).unwrap();
        p_step2.translate(0.0, 0.0, 4.0).unwrap();
        let mut tris = Vec::new();
        tris.extend(p_base.triangles.iter().copied());
        tris.extend(p_step1.triangles.iter().copied());
        tris.extend(p_step2.triangles.iter().copied());
        let combined = TriangleMesh::new(tris).unwrap();

        let mut cfg = PrintConfig::default();
        cfg.process.outer_wall_speed_mm_s = 100.0;
        cfg.process.overhang_1_4_speed_mm_s = Some(60.0);
        cfg.process.overhang_2_4_speed_mm_s = Some(40.0);
        cfg.process.overhang_speed_mm_s = 25.0;
        cfg.process.overhang_4_4_speed_mm_s = Some(14.0);
        cfg.filament.slow_down_layer_time_s = 0.0;

        let plate = slice_prepared_mesh(&combined, &cfg).unwrap();
        // Layer at z = 2.2 (index 10): 0.14 mm flare (~33% unsupported -> 2/4 = 40 mm/s).
        let l10_speeds: Vec<f64> = plate.layers[10]
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::OverhangWall)
            .map(|p| p.speed_mm_s)
            .collect();
        assert!(
            l10_speeds.iter().any(|&s| (s - 40.0).abs() < 1e-6),
            "expected ~40 mm/s 2/4 overhang band: {l10_speeds:?}"
        );
        // Layer at z = 4.2 (index 20): extra 0.31 mm flare (>50%..75% -> 3/4 = 25 mm/s).
        let l20_min = plate.layers[20]
            .paths
            .iter()
            .filter(|p| p.role == ExtrusionRole::OverhangWall)
            .map(|p| p.speed_mm_s)
            .fold(f64::INFINITY, f64::min);
        assert!(
            l20_min <= 25.0 + 1e-6,
            "expected <=25 mm/s on deeper flare, got {l20_min}"
        );
    }

    #[test]
    fn slice_plate_objects_preserves_ids_overrides_and_sequential_clearance() {
        let mesh = make_box_mesh(16.0, 16.0, 6.0);
        let mut p1 = prepare_for_plate(&mesh, 250.0, 250.0).unwrap();
        let mut p2 = prepare_for_plate(&mesh, 250.0, 250.0).unwrap();
        p1.translate(-50.0, 0.0, 0.0).unwrap();
        p2.translate(50.0, 0.0, 0.0).unwrap();

        let ov1 = ModifierOverrides {
            wall_loops: Some(1),
            ..ModifierOverrides::default()
        };
        let ov2 = ModifierOverrides {
            wall_loops: Some(4),
            ..ModifierOverrides::default()
        };
        let objs = [
            PlateSliceObject {
                id: 10,
                name: "Left Part",
                prepared_mesh: &p1,
                overrides: &ov1,
            },
            PlateSliceObject {
                id: 20,
                name: "Right Part",
                prepared_mesh: &p2,
                overrides: &ov2,
            },
        ];

        let mut cfg = PrintConfig::default();
        cfg.process.skirt_loops = 1;
        let by_layer = slice_plate_objects(&objs, &cfg, &[]).unwrap();
        assert_eq!(by_layer.objects.len(), 2);
        assert_eq!(by_layer.print_sequence, PrintSequence::ByLayer);
        let obj1_walls = by_layer.layers[1]
            .paths
            .iter()
            .filter(|p| {
                p.object_id == Some(10)
                    && matches!(p.role, ExtrusionRole::OuterWall | ExtrusionRole::InnerWall)
            })
            .count();
        let obj2_walls = by_layer.layers[1]
            .paths
            .iter()
            .filter(|p| {
                p.object_id == Some(20)
                    && matches!(p.role, ExtrusionRole::OuterWall | ExtrusionRole::InnerWall)
            })
            .count();
        assert_eq!(obj1_walls, 1);
        assert_eq!(obj2_walls, 4);

        // Sequential ByObject succeeds when spaced >= 35 mm and height <= 25 mm.
        cfg.process.print_sequence = PrintSequence::ByObject;
        let by_obj = slice_plate_objects(&objs, &cfg, &[]).unwrap();
        assert_eq!(by_obj.print_sequence, PrintSequence::ByObject);
        assert_eq!(by_obj.layers.len(), 60);
        assert!(by_obj.layers[..30].iter().all(|l| l.object_id == Some(10)));
        assert!(by_obj.layers[30..].iter().all(|l| l.object_id == Some(20)));

        // Exceeding clearance height on first object fails cleanly.
        cfg.process.extruder_clearance_height_mm = 4.0;
        assert!(matches!(
            slice_plate_objects(&objs, &cfg, &[]),
            Err(SliceError::SequentialHeightViolation { .. })
        ));
        cfg.process.extruder_clearance_height_mm = 25.0;

        // Placing objects too close in XY fails clearance check.
        let mut p2_close = prepare_for_plate(&mesh, 250.0, 250.0).unwrap();
        p2_close.translate(-25.0, 0.0, 0.0).unwrap();
        let objs_close = [
            objs[0],
            PlateSliceObject {
                id: 20,
                name: "Close Part",
                prepared_mesh: &p2_close,
                overrides: &ov2,
            },
        ];
        assert!(matches!(
            slice_plate_objects(&objs_close, &cfg, &[]),
            Err(SliceError::SequentialClearanceViolation { .. })
        ));
    }

    #[test]
    fn slice_progress_reports_layers_and_respects_cancellation() {
        let mesh = make_box_mesh(16.0, 16.0, 4.0);
        let prepared = prepare_for_plate(&mesh, 250.0, 250.0).unwrap();
        let cfg = PrintConfig::default();

        let progress = SliceProgress::new();
        let plate =
            slice_prepared_mesh_with_progress(&prepared, &cfg, &[], Some(&progress)).unwrap();
        let (done, total) = progress.snapshot();
        assert!(done > 0);
        assert_eq!(done, plate.layers.len());
        assert_eq!(total, plate.layers.len());

        let cancelled = SliceProgress::new();
        cancelled.cancel();
        assert!(matches!(
            slice_prepared_mesh_with_progress(&prepared, &cfg, &[], Some(&cancelled)),
            Err(SliceError::Cancelled)
        ));

        let ov = ModifierOverrides::default();
        let obj = PlateSliceObject {
            id: 1,
            name: "Part",
            prepared_mesh: &prepared,
            overrides: &ov,
        };
        assert!(matches!(
            slice_plate_objects_with_progress(&[obj], &cfg, &[], Some(&cancelled)),
            Err(SliceError::Cancelled)
        ));
    }

    #[test]
    fn slice_progress_callback_receives_layer_updates() {
        let mesh = make_box_mesh(12.0, 12.0, 3.0);
        let prepared = prepare_for_plate(&mesh, 250.0, 250.0).unwrap();
        let cfg = PrintConfig::default();

        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(usize, usize)>::new()));
        let rec_cb = std::sync::Arc::clone(&recorded);
        let progress = SliceProgress::with_callback(move |done, total| {
            if let Ok(mut guard) = rec_cb.lock() {
                guard.push((done, total));
            }
        });
        let plate =
            slice_prepared_mesh_with_progress(&prepared, &cfg, &[], Some(&progress)).unwrap();
        let log = recorded.lock().unwrap();
        assert!(!log.is_empty());
        assert!(log.iter().any(|&(d, t)| d == 0 && t == plate.layers.len()));
        assert_eq!(
            log.last().copied(),
            Some((plate.layers.len(), plate.layers.len()))
        );
    }
}
