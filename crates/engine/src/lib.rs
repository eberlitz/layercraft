//! Stateful slicing session and schema-typed command registry (`everything is a command`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use base64::Engine as _;
#[cfg(not(target_arch = "wasm32"))]
use layercraft_config::ProfileDirLoadReport;
use layercraft_config::{
    ActiveProfiles, ConfigError, MachineConfig, ModifierOverrides, ModifierRole, PrintConfig,
    ProfileApplyResult, ProfileError, ProfileKind, ProfileLibrary, ProfileSummary, ResolvedProfile,
    profile_document_from_config,
};
#[cfg(not(target_arch = "wasm32"))]
use layercraft_config::{
    default_user_profiles_dir, delete_user_profile_from_dir, save_user_profile_to_dir,
};
pub use layercraft_gcode::{BgcodeCompression, decode_bgcode, encode_bgcode, is_bgcode};
use layercraft_gcode::{
    BgcodeError, BgcodeOptions, GcodeError, GcodeOutput, GcodeReport, PrintStatistics,
    RasterComparison, compare_layer_iou, emit_gcode, evaluate_gcode,
};
use layercraft_geom::{Aabb3, GeomError, TriangleMesh};
use layercraft_io::{
    IoError, ThreeMfBuildItem, ThreeMfDocument, ThreeMfExportDocument, ThreeMfExportPlate,
    ThreeMfPart, ThreeMfResourceMesh, normalize_base_url, read_3mf_document_bytes, read_stl_bytes,
    write_3mf_document_bytes,
};
pub use layercraft_io::{
    PrinterAuth, PrinterError, PrinterProfile, PrinterProfileSummary, PrinterProtocol,
    PrinterStatusReport, PrinterStorageInfo, UploadOptions, UploadResult,
};
#[cfg(not(target_arch = "wasm32"))]
use layercraft_io::{default_printers_config_path, load_printer_profiles, save_printer_profiles};
#[cfg(not(target_arch = "wasm32"))]
pub use layercraft_io::{probe_printer, upload_to_printer};
use layercraft_mesh::{
    MeshError, MeshInspection, ModifierPrimitive, ModifierTransform, ObjectTransform, SampleShape,
    auto_arrange_footprints, build_modifier_primitive, build_sample_shape, inspect_mesh,
    prepare_modifier_mesh, prepare_transformed_object, repair_mesh,
};
pub use layercraft_mesh::{MeshRepairConfig, MeshRepairReport};
use layercraft_slice::{
    ModifierVolume, PrintabilityReport, SliceError, SlicedPlate, analyze_printability,
    slice_prepared_mesh_with_modifiers, slice_single_mesh_with_modifiers,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Errors returned by command execution in [`Session`].
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("unknown command `{0}`")]
    UnknownCommand(String),
    #[error("invalid command parameters: {0}")]
    InvalidParams(#[from] serde_json::Error),
    #[error("invalid argument `{field}`: {reason}")]
    InvalidArgument {
        field: &'static str,
        reason: &'static str,
    },
    #[error("no mesh loaded on build plate")]
    NoMeshLoaded,
    #[error("plate object id {0} not found")]
    ObjectNotFound(u64),
    #[error("plate modifier id {0} not found")]
    ModifierNotFound(u64),
    #[error("build plate {0} not found")]
    PlateNotFound(String),
    #[error("plate has not been sliced yet")]
    NotSliced,
    #[error("layer index {index} out of range (plate has {count} layers)")]
    LayerOutOfRange { index: usize, count: usize },
    #[cfg(not(target_arch = "wasm32"))]
    #[error("file error: {0}")]
    FileWrite(String),
    #[error("printer profile `{0}` not found")]
    PrinterNotFound(String),
    #[error("safety invariant check failed before upload: {0}")]
    SafetyCheckFailed(String),
    #[error("invalid LayerCraft project archive: {0}")]
    InvalidProject(String),
    #[error(transparent)]
    Printer(#[from] PrinterError),
    #[error(transparent)]
    Geom(#[from] GeomError),
    #[error(transparent)]
    Mesh(#[from] MeshError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(transparent)]
    Slice(#[from] SliceError),
    #[error(transparent)]
    Gcode(#[from] GcodeError),
    #[error(transparent)]
    Bgcode(#[from] BgcodeError),
    #[error(transparent)]
    Io(#[from] IoError),
}

/// Stored plate object entry with raw mesh, transform, and prepared world-space mesh.
#[derive(Debug, Clone)]
pub struct PlateObjectEntry {
    pub id: u64,
    pub name: String,
    pub raw_mesh: Arc<TriangleMesh>,
    pub transform: ObjectTransform,
    pub prepared_mesh: Arc<TriangleMesh>,
}

impl PlateObjectEntry {
    fn info(&self) -> PlateObjectInfo {
        PlateObjectInfo {
            id: self.id,
            name: self.name.clone(),
            transform: self.transform,
            triangle_count: self.prepared_mesh.triangles.len(),
            aabb: self.prepared_mesh.aabb(),
        }
    }
}

/// Public serializable summary of a single object on the build plate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlateObjectInfo {
    pub id: u64,
    pub name: String,
    pub transform: ObjectTransform,
    pub triangle_count: usize,
    pub aabb: Option<Aabb3>,
}

/// Stored 3D modifier volume entry with raw mesh, transform, role, and local overrides.
#[derive(Debug, Clone)]
pub struct PlateModifierEntry {
    pub id: u64,
    pub name: String,
    pub role: ModifierRole,
    pub primitive: Option<ModifierPrimitive>,
    pub raw_mesh: Arc<TriangleMesh>,
    pub transform: ModifierTransform,
    pub prepared_mesh: Arc<TriangleMesh>,
    pub overrides: ModifierOverrides,
}

impl PlateModifierEntry {
    fn info(&self) -> PlateModifierInfo {
        PlateModifierInfo {
            id: self.id,
            name: self.name.clone(),
            role: self.role,
            primitive: self.primitive,
            transform: self.transform,
            overrides: self.overrides.clone(),
            triangle_count: self.prepared_mesh.triangles.len(),
            aabb: self.prepared_mesh.aabb(),
        }
    }

    fn to_volume(&self) -> ModifierVolume {
        ModifierVolume {
            id: self.id,
            name: self.name.clone(),
            role: self.role,
            mesh: TriangleMesh::clone(&self.prepared_mesh),
            overrides: self.overrides.clone(),
        }
    }
}

/// Public serializable summary of a 3D modifier volume on the build plate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlateModifierInfo {
    pub id: u64,
    pub name: String,
    pub role: ModifierRole,
    pub primitive: Option<ModifierPrimitive>,
    pub transform: ModifierTransform,
    pub overrides: ModifierOverrides,
    pub triangle_count: usize,
    pub aabb: Option<Aabb3>,
}

const MAX_UNDO_HISTORY: usize = 50;
const PROJECT_FORMAT_VERSION: u32 = 1;

/// Public serializable summary of one logical build plate in a multi-plate session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlateSummary {
    pub id: u32,
    pub index: usize,
    pub name: String,
    pub active: bool,
    pub object_count: usize,
    pub modifier_count: usize,
    pub sliced: bool,
}

#[derive(Debug, Clone)]
struct StoredPlate {
    id: u32,
    name: String,
    objects: Vec<PlateObjectEntry>,
    modifiers: Vec<PlateModifierEntry>,
    mesh: Option<Arc<TriangleMesh>>,
    prepared_mesh: Option<Arc<TriangleMesh>>,
    plate_inspection: Option<MeshInspection>,
    sliced: Option<SlicedPlate>,
    last_gcode: Option<GcodeOutput>,
}

impl StoredPlate {
    fn empty(id: u32, name: String) -> Self {
        Self {
            id,
            name,
            objects: Vec::new(),
            modifiers: Vec::new(),
            mesh: None,
            prepared_mesh: None,
            plate_inspection: None,
            sliced: None,
            last_gcode: None,
        }
    }
}

#[derive(Debug, Clone)]
struct StoredPlateSnapshot {
    id: u32,
    name: String,
    objects: Vec<PlateObjectEntry>,
    modifiers: Vec<PlateModifierEntry>,
    mesh: Option<Arc<TriangleMesh>>,
    prepared_mesh: Option<Arc<TriangleMesh>>,
    plate_inspection: Option<MeshInspection>,
}

#[derive(Debug, Clone)]
struct SessionSnapshot {
    label: String,
    config: PrintConfig,
    active_profiles: ActiveProfiles,
    objects: Vec<PlateObjectEntry>,
    next_object_id: u64,
    modifiers: Vec<PlateModifierEntry>,
    next_modifier_id: u64,
    mesh: Option<Arc<TriangleMesh>>,
    prepared_mesh: Option<Arc<TriangleMesh>>,
    plate_inspection: Option<MeshInspection>,
    plate_slots: Vec<StoredPlateSnapshot>,
    active_plate_idx: usize,
    next_plate_id: u32,
}

/// Summary of the bounded session undo/redo history stacks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryStatus {
    pub can_undo: bool,
    pub can_redo: bool,
    pub undo_depth: usize,
    pub redo_depth: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redo_label: Option<String>,
    pub recent_undo: Vec<String>,
    pub recent_redo: Vec<String>,
}

/// Result returned by `session.undo` and `session.redo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryActionResult {
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub history: HistoryStatus,
}

/// Pair of build-plate object IDs whose XY footprints overlap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ObjectCollisionPair {
    pub a_id: u64,
    pub b_id: u64,
}

/// Cached XY/Z bed bounds and object-overlap layout report for the active plate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlateLayoutCheck {
    pub out_of_bounds_object_ids: Vec<u64>,
    pub colliding_pairs: Vec<ObjectCollisionPair>,
    pub colliding_object_ids: Vec<u64>,
    pub has_issues: bool,
}

fn push_unique_id(list: &mut Vec<u64>, id: u64) {
    if !list.contains(&id) {
        list.push(id);
    }
}

/// Compute bed-bounds violations and pairwise XY footprint overlaps for `objects`.
pub fn check_plate_layout(
    objects: &[PlateObjectEntry],
    machine: &MachineConfig,
) -> PlateLayoutCheck {
    let mut out_of_bounds_object_ids = Vec::new();
    let mut colliding_pairs = Vec::new();
    let mut colliding_object_ids = Vec::new();
    let mut bounds: Vec<(u64, Aabb3)> = Vec::with_capacity(objects.len());

    let tol_mm = 1e-4;
    for obj in objects {
        let Some(bb) = obj.prepared_mesh.aabb() else {
            continue;
        };
        if bb.min.x < -tol_mm
            || bb.max.x > machine.bed_width_mm + tol_mm
            || bb.min.y < -tol_mm
            || bb.max.y > machine.bed_depth_mm + tol_mm
            || bb.max.z > machine.bed_height_mm + tol_mm
        {
            out_of_bounds_object_ids.push(obj.id);
        }
        bounds.push((obj.id, bb));
    }

    let overlap_eps = 1e-3;
    for (i, (a_id, a_bb)) in bounds.iter().enumerate() {
        for (b_id, b_bb) in bounds.iter().skip(i + 1) {
            let x_overlap =
                a_bb.min.x < b_bb.max.x - overlap_eps && a_bb.max.x > b_bb.min.x + overlap_eps;
            let y_overlap =
                a_bb.min.y < b_bb.max.y - overlap_eps && a_bb.max.y > b_bb.min.y + overlap_eps;
            if x_overlap && y_overlap {
                colliding_pairs.push(ObjectCollisionPair {
                    a_id: *a_id,
                    b_id: *b_id,
                });
                push_unique_id(&mut colliding_object_ids, *a_id);
                push_unique_id(&mut colliding_object_ids, *b_id);
            }
        }
    }

    let has_issues = !out_of_bounds_object_ids.is_empty() || !colliding_pairs.is_empty();
    PlateLayoutCheck {
        out_of_bounds_object_ids,
        colliding_pairs,
        colliding_object_ids,
        has_issues,
    }
}

/// Stateful slicing session driving CLI, WASM, GUI, and MCP frontends.
#[derive(Debug, Clone)]
pub struct Session {
    pub config: PrintConfig,
    pub profiles: ProfileLibrary,
    pub active_profiles: ActiveProfiles,
    pub objects: Vec<PlateObjectEntry>,
    pub next_object_id: u64,
    pub modifiers: Vec<PlateModifierEntry>,
    pub next_modifier_id: u64,
    /// Merged raw (untransformed) plate mesh; shares the object's `Arc` for single-object plates.
    pub mesh: Option<Arc<TriangleMesh>>,
    /// Merged prepared (world-space) plate mesh; shares the object's `Arc` for single-object plates.
    pub prepared_mesh: Option<Arc<TriangleMesh>>,
    /// Cached topology/volume inspection of [`Session::prepared_mesh`] (kept in sync by every mutation).
    pub plate_inspection: Option<MeshInspection>,
    /// Cached bed-bounds and pairwise overlap status of the active build plate.
    pub plate_layout: PlateLayoutCheck,
    pub sliced: Option<SlicedPlate>,
    pub last_gcode: Option<GcodeOutput>,
    /// Saved local/LAN printer connection profiles (never persisted inside `.3mf` files).
    pub printers: Vec<PrinterProfile>,
    plate_slots: Vec<StoredPlate>,
    active_plate_idx: usize,
    next_plate_id: u32,
    undo_stack: Vec<SessionSnapshot>,
    redo_stack: Vec<SessionSnapshot>,
    suppress_undo_capture: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            config: PrintConfig::default(),
            profiles: ProfileLibrary::with_builtin_presets(),
            active_profiles: ActiveProfiles::default(),
            objects: Vec::new(),
            next_object_id: 0,
            modifiers: Vec::new(),
            next_modifier_id: 0,
            mesh: None,
            prepared_mesh: None,
            plate_inspection: None,
            plate_layout: PlateLayoutCheck::default(),
            sliced: None,
            last_gcode: None,
            printers: Vec::new(),
            plate_slots: vec![StoredPlate::empty(1, "Plate 1".to_owned())],
            active_plate_idx: 0,
            next_plate_id: 2,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            suppress_undo_capture: false,
        }
    }
}

impl Session {
    /// Total number of logical build plates in this session (always at least 1).
    pub fn plate_count(&self) -> usize {
        self.plate_slots.len()
    }

    /// Zero-based index of the currently active build plate.
    pub fn active_plate_index(&self) -> usize {
        self.active_plate_idx
    }

    /// Persistent numeric ID of the currently active build plate.
    pub fn active_plate_id(&self) -> u32 {
        self.plate_slots
            .get(self.active_plate_idx)
            .map(|p| p.id)
            .unwrap_or(1)
    }

    /// Display name of the currently active build plate.
    pub fn active_plate_name(&self) -> &str {
        self.plate_slots
            .get(self.active_plate_idx)
            .map(|p| p.name.as_str())
            .unwrap_or("Plate 1")
    }

    /// Resolve a printer profile from an inline override, saved `printer_id`, or single saved printer.
    pub fn resolve_printer_profile(
        &self,
        printer_id: Option<&str>,
        inline: Option<PrinterProfile>,
    ) -> Result<PrinterProfile, EngineError> {
        if let Some(mut prof) = inline {
            prof.base_url = normalize_base_url(&prof.base_url)?;
            return Ok(prof);
        }
        if let Some(id) = printer_id.map(str::trim).filter(|s| !s.is_empty()) {
            let found = self
                .printers
                .iter()
                .find(|p| p.id == id || p.name == id)
                .cloned()
                .ok_or_else(|| EngineError::PrinterNotFound(id.to_owned()))?;
            return Ok(found);
        }
        if let Some(first) = self.printers.first()
            && self.printers.len() == 1
        {
            return Ok(first.clone());
        }
        Err(EngineError::InvalidArgument {
            field: "printer_id",
            reason: "specify `printer_id` or `profile` (or configure exactly one saved printer)",
        })
    }

    /// Derive a safe filename for the active build plate with `.gcode` or `.bgcode` extension.
    pub fn default_export_filename(&self, binary: bool) -> String {
        let ext = if binary { "bgcode" } else { "gcode" };
        if self.plate_count() <= 1 && self.active_plate_name() == "Plate 1" {
            return format!("layercraft_out.{ext}");
        }
        let mut slug: String = self
            .active_plate_name()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        while slug.contains("__") {
            slug = slug.replace("__", "_");
        }
        let trimmed = slug.trim_matches('_');
        if trimmed.is_empty() {
            format!("plate_{}.{ext}", self.active_plate_id())
        } else {
            format!("{trimmed}.{ext}")
        }
    }

    /// Load saved printer profiles from the default user config path on native platforms.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load_printers_from_disk(&mut self) -> Result<usize, PrinterError> {
        let Some(path) = default_printers_config_path() else {
            return Ok(0);
        };
        let loaded = load_printer_profiles(&path)?;
        self.printers = loaded;
        Ok(self.printers.len())
    }

    /// Save current printer profiles to the default user config path on native platforms.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn save_printers_to_disk(&self) -> Result<(), PrinterError> {
        let Some(path) = default_printers_config_path() else {
            return Ok(());
        };
        save_printer_profiles(&path, &self.printers)
    }

    /// Recompute cached bed bounds and pairwise overlap state for the active build plate.
    pub fn recompute_plate_layout(&mut self) {
        self.plate_layout = check_plate_layout(&self.objects, &self.config.machine);
    }

    /// Load user-saved machine, filament, and process presets from disk without overwriting built-ins.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load_user_profiles_from_disk(&mut self) -> Result<usize, ProfileError> {
        let Some(dir) = default_user_profiles_dir() else {
            return Ok(0);
        };
        self.profiles.load_user_profiles_from_dir(&dir)
    }

    fn plate_summary_at(&self, index: usize) -> Option<PlateSummary> {
        let slot = self.plate_slots.get(index)?;
        let active = index == self.active_plate_idx;
        let (object_count, modifier_count, sliced) = if active {
            (
                self.objects.len(),
                self.modifiers.len(),
                self.sliced.is_some(),
            )
        } else {
            (
                slot.objects.len(),
                slot.modifiers.len(),
                slot.sliced.is_some(),
            )
        };
        Some(PlateSummary {
            id: slot.id,
            index,
            name: slot.name.clone(),
            active,
            object_count,
            modifier_count,
            sliced,
        })
    }

    /// Summarize all build plates in this session in plate order.
    pub fn list_plates(&self) -> Vec<PlateSummary> {
        (0..self.plate_slots.len())
            .filter_map(|i| self.plate_summary_at(i))
            .collect()
    }

    fn swap_slot_with_live(&mut self, slot_idx: usize) {
        if let Some(slot) = self.plate_slots.get_mut(slot_idx) {
            std::mem::swap(&mut slot.objects, &mut self.objects);
            std::mem::swap(&mut slot.modifiers, &mut self.modifiers);
            std::mem::swap(&mut slot.mesh, &mut self.mesh);
            std::mem::swap(&mut slot.prepared_mesh, &mut self.prepared_mesh);
            std::mem::swap(&mut slot.plate_inspection, &mut self.plate_inspection);
            std::mem::swap(&mut slot.sliced, &mut self.sliced);
            std::mem::swap(&mut slot.last_gcode, &mut self.last_gcode);
        }
    }

    /// Switch the active build plate in O(1) time without cloning meshes or discarding cached slices.
    pub fn select_plate(&mut self, index: usize) -> Result<PlateSummary, EngineError> {
        if index >= self.plate_slots.len() {
            return Err(EngineError::PlateNotFound(format!("index {index}")));
        }
        if index != self.active_plate_idx {
            let prev_idx = self.active_plate_idx;
            self.swap_slot_with_live(prev_idx);
            self.swap_slot_with_live(index);
            self.active_plate_idx = index;
            self.recompute_plate_layout();
        }
        self.plate_summary_at(index)
            .ok_or_else(|| EngineError::PlateNotFound(format!("index {index}")))
    }

    /// Append a new empty build plate, optionally selecting it as active.
    pub fn add_plate(
        &mut self,
        name: Option<&str>,
        select: bool,
    ) -> Result<PlateSummary, EngineError> {
        let id = if self.next_plate_id == 0 {
            1
        } else {
            self.next_plate_id
        };
        self.next_plate_id = id + 1;
        let plate_name = name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Plate {id}"));
        self.plate_slots.push(StoredPlate::empty(id, plate_name));
        let new_idx = self.plate_slots.len() - 1;
        if select {
            self.select_plate(new_idx)
        } else {
            self.plate_summary_at(new_idx)
                .ok_or_else(|| EngineError::PlateNotFound(format!("index {new_idx}")))
        }
    }

    /// Rename an existing build plate by zero-based index.
    pub fn rename_plate(&mut self, index: usize, name: &str) -> Result<PlateSummary, EngineError> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(EngineError::InvalidArgument {
                field: "name",
                reason: "plate name must not be empty",
            });
        }
        let slot = self
            .plate_slots
            .get_mut(index)
            .ok_or_else(|| EngineError::PlateNotFound(format!("index {index}")))?;
        slot.name = trimmed.to_owned();
        self.plate_summary_at(index)
            .ok_or_else(|| EngineError::PlateNotFound(format!("index {index}")))
    }

    /// Remove a build plate (at least one plate always remains), switching away first if active.
    pub fn remove_plate(&mut self, index: usize) -> Result<PlateSummary, EngineError> {
        if self.plate_slots.len() <= 1 {
            return Err(EngineError::InvalidArgument {
                field: "plate",
                reason: "cannot remove the only build plate",
            });
        }
        if index >= self.plate_slots.len() {
            return Err(EngineError::PlateNotFound(format!("index {index}")));
        }
        if index == self.active_plate_idx {
            let next_idx = if index > 0 { index - 1 } else { 1 };
            let _ = self.select_plate(next_idx)?;
        }
        self.plate_slots.remove(index);
        if self.active_plate_idx > index {
            self.active_plate_idx -= 1;
        }
        self.plate_summary_at(self.active_plate_idx)
            .ok_or_else(|| EngineError::PlateNotFound(format!("index {}", self.active_plate_idx)))
    }

    fn capture_snapshot(&self, label: impl Into<String>) -> SessionSnapshot {
        let plate_slots = self
            .plate_slots
            .iter()
            .map(|p| StoredPlateSnapshot {
                id: p.id,
                name: p.name.clone(),
                objects: p.objects.clone(),
                modifiers: p.modifiers.clone(),
                mesh: p.mesh.clone(),
                prepared_mesh: p.prepared_mesh.clone(),
                plate_inspection: p.plate_inspection.clone(),
            })
            .collect();
        SessionSnapshot {
            label: label.into(),
            config: self.config.clone(),
            active_profiles: self.active_profiles.clone(),
            objects: self.objects.clone(),
            next_object_id: self.next_object_id,
            modifiers: self.modifiers.clone(),
            next_modifier_id: self.next_modifier_id,
            mesh: self.mesh.clone(),
            prepared_mesh: self.prepared_mesh.clone(),
            plate_inspection: self.plate_inspection.clone(),
            plate_slots,
            active_plate_idx: self.active_plate_idx,
            next_plate_id: self.next_plate_id,
        }
    }

    fn restore_snapshot(&mut self, snap: SessionSnapshot) {
        self.config = snap.config;
        self.active_profiles = snap.active_profiles;
        self.objects = snap.objects;
        self.next_object_id = snap.next_object_id;
        self.modifiers = snap.modifiers;
        self.next_modifier_id = snap.next_modifier_id;
        self.mesh = snap.mesh;
        self.prepared_mesh = snap.prepared_mesh;
        self.plate_inspection = snap.plate_inspection;
        self.plate_slots = if snap.plate_slots.is_empty() {
            vec![StoredPlate::empty(1, "Plate 1".to_owned())]
        } else {
            snap.plate_slots
                .into_iter()
                .map(|p| StoredPlate {
                    id: p.id,
                    name: p.name,
                    objects: p.objects,
                    modifiers: p.modifiers,
                    mesh: p.mesh,
                    prepared_mesh: p.prepared_mesh,
                    plate_inspection: p.plate_inspection,
                    sliced: None,
                    last_gcode: None,
                })
                .collect()
        };
        self.active_plate_idx = snap
            .active_plate_idx
            .min(self.plate_slots.len().saturating_sub(1));
        self.next_plate_id = snap.next_plate_id.max(2);
        self.sliced = None;
        self.last_gcode = None;
        self.recompute_plate_layout();
    }

    fn push_undo_entry(&mut self, snap: SessionSnapshot) {
        self.undo_stack.push(snap);
        if self.undo_stack.len() > MAX_UNDO_HISTORY {
            let excess = self.undo_stack.len() - MAX_UNDO_HISTORY;
            self.undo_stack.drain(0..excess);
        }
        self.redo_stack.clear();
    }

    /// Inspect current undo/redo availability and labels.
    pub fn history_status(&self) -> HistoryStatus {
        let undo_label = self.undo_stack.last().map(|s| s.label.clone());
        let redo_label = self.redo_stack.last().map(|s| s.label.clone());
        let recent_undo = self
            .undo_stack
            .iter()
            .rev()
            .take(5)
            .map(|s| s.label.clone())
            .collect();
        let recent_redo = self
            .redo_stack
            .iter()
            .rev()
            .take(5)
            .map(|s| s.label.clone())
            .collect();
        HistoryStatus {
            can_undo: !self.undo_stack.is_empty(),
            can_redo: !self.redo_stack.is_empty(),
            undo_depth: self.undo_stack.len(),
            redo_depth: self.redo_stack.len(),
            undo_label,
            redo_label,
            recent_undo,
            recent_redo,
        }
    }

    /// Clear undo and redo history (e.g. after initial startup sample preload).
    pub fn clear_history(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
    }

    /// Temporarily suppress automatic command undo snapshots (used when coalescing pointer drags).
    pub fn set_undo_suppressed(&mut self, suppressed: bool) {
        self.suppress_undo_capture = suppressed;
    }

    /// Record the current session state on the undo stack under `label`.
    pub fn push_undo_snapshot(&mut self, label: impl Into<String>) {
        let snap = self.capture_snapshot(label);
        self.push_undo_entry(snap);
    }

    /// Record a config edit snapshot given the pre-edit `PrintConfig` (for direct UI bindings).
    pub fn push_config_undo_snapshot(
        &mut self,
        label: impl Into<String>,
        previous_config: PrintConfig,
    ) {
        let mut snap = self.capture_snapshot(label);
        snap.config = previous_config;
        self.push_undo_entry(snap);
    }

    /// Revert to the most recent undo snapshot, pushing the current state to redo history.
    pub fn undo(&mut self) -> HistoryActionResult {
        let Some(prev) = self.undo_stack.pop() else {
            return HistoryActionResult {
                applied: false,
                label: None,
                history: self.history_status(),
            };
        };
        let label = prev.label.clone();
        let redo_snap = self.capture_snapshot(label.clone());
        self.redo_stack.push(redo_snap);
        if self.redo_stack.len() > MAX_UNDO_HISTORY {
            let excess = self.redo_stack.len() - MAX_UNDO_HISTORY;
            self.redo_stack.drain(0..excess);
        }
        self.restore_snapshot(prev);
        HistoryActionResult {
            applied: true,
            label: Some(label),
            history: self.history_status(),
        }
    }

    /// Re-apply the most recently undone snapshot, pushing the current state back to undo history.
    pub fn redo(&mut self) -> HistoryActionResult {
        let Some(next) = self.redo_stack.pop() else {
            return HistoryActionResult {
                applied: false,
                label: None,
                history: self.history_status(),
            };
        };
        let label = next.label.clone();
        let undo_snap = self.capture_snapshot(label.clone());
        self.undo_stack.push(undo_snap);
        if self.undo_stack.len() > MAX_UNDO_HISTORY {
            let excess = self.undo_stack.len() - MAX_UNDO_HISTORY;
            self.undo_stack.drain(0..excess);
        }
        self.restore_snapshot(next);
        HistoryActionResult {
            applied: true,
            label: Some(label),
            history: self.history_status(),
        }
    }
}

/// Metadata + JSON schemas describing a registered engine command.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CommandDescriptor {
    pub name: &'static str,
    pub summary: &'static str,
    pub params_schema: Value,
    pub result_schema: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EmptyParams {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddStlBytesParams {
    pub bytes: Vec<u8>,
    #[serde(default)]
    pub append: bool,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Add3mfBytesParams {
    pub bytes: Vec<u8>,
    #[serde(default)]
    pub append: bool,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddSampleShapeParams {
    pub shape: SampleShape,
    #[serde(default)]
    pub append: bool,
    #[serde(default)]
    pub name: Option<String>,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddMeshFileParams {
    pub path: String,
    #[serde(default)]
    pub append: bool,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RemoveObjectParams {
    pub id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TransformObjectParams {
    pub id: u64,
    pub position_xy_mm: Option<[f64; 2]>,
    pub rotation_deg: Option<[f64; 3]>,
    pub scale: Option<[f64; 3]>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct AutoArrangeParams {
    pub spacing_mm: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddModifierPrimitiveParams {
    pub primitive: ModifierPrimitive,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub role: ModifierRole,
    #[serde(default)]
    pub transform: Option<ModifierTransform>,
    #[serde(default)]
    pub overrides: ModifierOverrides,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddModifierBytesParams {
    pub bytes: Vec<u8>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub role: ModifierRole,
    #[serde(default)]
    pub transform: Option<ModifierTransform>,
    #[serde(default)]
    pub overrides: ModifierOverrides,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddModifierMeshFileParams {
    pub path: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub role: ModifierRole,
    #[serde(default)]
    pub transform: Option<ModifierTransform>,
    #[serde(default)]
    pub overrides: ModifierOverrides,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UpdateModifierParams {
    pub id: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub role: Option<ModifierRole>,
    #[serde(default)]
    pub position_xyz_mm: Option<[f64; 3]>,
    #[serde(default)]
    pub rotation_deg: Option<[f64; 3]>,
    #[serde(default)]
    pub scale: Option<[f64; 3]>,
    #[serde(default)]
    pub overrides: Option<ModifierOverrides>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RemoveModifierParams {
    pub id: u64,
}

/// Parameters for `plate.repair_mesh`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RepairMeshParams {
    /// Plate object to repair; omit to repair every object on the active plate.
    #[serde(default)]
    pub object_id: Option<u64>,
    /// Repair tuning; omit for defaults.
    #[serde(default)]
    pub config: Option<MeshRepairConfig>,
}

/// Repair outcome for one plate object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ObjectRepairResult {
    pub object_id: u64,
    pub name: String,
    pub report: MeshRepairReport,
}

/// Result of `plate.repair_mesh`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlateRepairResult {
    /// Per-object repair reports, in plate order.
    pub objects: Vec<ObjectRepairResult>,
    /// `true` when at least one object mesh was modified.
    pub changed: bool,
    /// Inspection of the rebuilt plate mesh after repair.
    pub plate_inspection: Option<MeshInspection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SetConfigParams {
    pub config: PrintConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ListProfilesParams {
    #[serde(default)]
    pub kind: Option<ProfileKind>,
    #[serde(default)]
    pub include_abstract: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LoadProfileJsonParams {
    pub json: String,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LoadProfileFileParams {
    pub path: String,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LoadProfileDirParams {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResolveProfileParams {
    pub kind: ProfileKind,
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ApplyProfilesParams {
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub filament: Option<String>,
    #[serde(default)]
    pub process: Option<String>,
    #[serde(default)]
    pub reset_to_defaults: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SaveUserProfileParams {
    pub kind: ProfileKind,
    pub name: String,
    #[serde(default = "default_true")]
    pub activate: bool,
    #[serde(default)]
    pub persist: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SaveUserProfileResult {
    pub summary: ProfileSummary,
    pub persisted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeleteUserProfileParams {
    pub kind: ProfileKind,
    pub name: String,
    #[serde(default)]
    pub persist: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeleteUserProfileResult {
    pub deleted: bool,
    pub deleted_from_disk: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExportProfileJsonParams {
    pub kind: ProfileKind,
    pub name: String,
    #[serde(default = "default_true")]
    pub pretty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExportProfileJsonResult {
    pub kind: ProfileKind,
    pub name: String,
    pub json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InspectLayerParams {
    pub index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EvaluateGcodeParams {
    pub gcode: String,
    pub filament_diameter_mm: Option<f64>,
    pub bed_width_mm: Option<f64>,
    pub bed_depth_mm: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OkResult {
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SliceSummaryResult {
    pub layer_count: usize,
    pub total_paths: usize,
}

/// Per-layer inspection metrics returned by `layer.inspect`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LayerInspection {
    pub index: usize,
    pub z_top_mm: f64,
    pub height_mm: f64,
    pub fan_percent: u8,
    pub estimated_time_s: f64,
    pub region_count: usize,
    pub path_count: usize,
    pub role_lengths_mm: BTreeMap<String, f64>,
}

fn default_true() -> bool {
    true
}

/// Compact metadata entry returned by [`list_command_summaries`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CommandSummary {
    pub name: &'static str,
    pub summary: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PatchConfigParams {
    /// Partial JSON object merged onto the active `PrintConfig` (RFC 7396 merge patch).
    pub patch: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AnalyzePrintabilityParams {
    /// Slice the plate automatically if no cached slice exists yet (default `true`).
    #[serde(default = "default_true")]
    pub auto_slice: bool,
}

impl Default for AnalyzePrintabilityParams {
    fn default() -> Self {
        Self { auto_slice: true }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct CompareSlicesParams {
    /// Optional partial `PrintConfig` patch for baseline config A (defaults to current config).
    pub patch_a: Option<Value>,
    /// Partial `PrintConfig` patch for candidate config B.
    pub patch_b: Value,
    /// Raster resolution (mm/pixel) for 2D swept-bead IoU comparison (default `0.5`).
    pub pixel_mm: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SliceSideMetrics {
    pub layer_count: usize,
    pub filament_volume_mm3: f64,
    pub filament_mass_g: f64,
    pub estimated_time_s: f64,
    pub travel_distance_mm: f64,
    pub arc_moves_count: usize,
    pub safety_clean: bool,
    pub stats: PrintStatistics,
    pub report: GcodeReport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SliceDeltaMetrics {
    pub layer_count_delta: i64,
    pub filament_volume_delta_mm3: f64,
    pub filament_volume_delta_percent: f64,
    pub filament_mass_delta_g: f64,
    pub estimated_time_delta_s: f64,
    pub estimated_time_delta_percent: f64,
    pub travel_distance_delta_mm: f64,
    pub arc_moves_delta: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SliceComparisonReport {
    pub a: SliceSideMetrics,
    pub b: SliceSideMetrics,
    pub delta: SliceDeltaMetrics,
    pub raster: RasterComparison,
}

/// Output file encoding format for G-code export and printer upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GcodeExportFormat {
    #[default]
    Auto,
    Gcode,
    Bgcode,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ExportBgcodeParams {
    #[serde(default)]
    pub compression: Option<BgcodeCompression>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExportBgcodeResult {
    pub bytes_base64: String,
    pub byte_len: usize,
    pub ascii_byte_len: usize,
    pub compression_ratio: f64,
    pub compression: BgcodeCompression,
    pub stats: PrintStatistics,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SaveGcodeFileParams {
    pub path: String,
    #[serde(default = "default_true")]
    pub evaluate: bool,
    #[serde(default)]
    pub format: GcodeExportFormat,
    #[serde(default)]
    pub compression: Option<BgcodeCompression>,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SaveGcodeFileResult {
    pub path: String,
    pub format: String,
    pub bytes_written: usize,
    pub stats: PrintStatistics,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<GcodeReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UpsertPrinterParams {
    pub profile: PrinterProfile,
    #[serde(default)]
    pub persist: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RemovePrinterParams {
    pub id: String,
    #[serde(default)]
    pub persist: bool,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PrinterStatusParams {
    #[serde(default)]
    pub printer_id: Option<String>,
    #[serde(default)]
    pub profile: Option<PrinterProfile>,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrinterUploadParams {
    #[serde(default)]
    pub printer_id: Option<String>,
    #[serde(default)]
    pub profile: Option<PrinterProfile>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub storage: Option<String>,
    #[serde(default)]
    pub format: GcodeExportFormat,
    #[serde(default)]
    pub compression: Option<BgcodeCompression>,
    #[serde(default)]
    pub start_print: bool,
    #[serde(default = "default_true")]
    pub overwrite: bool,
    #[serde(default = "default_true")]
    pub auto_slice: bool,
    #[serde(default = "default_true")]
    pub evaluate: bool,
    #[serde(default = "default_true")]
    pub require_clean_safety: bool,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PrinterUploadResult {
    pub format: String,
    pub upload: UploadResult,
    pub stats: PrintStatistics,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<GcodeReport>,
}

/// One step in a [`execute_batch`] call.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BatchStep {
    pub command: String,
    #[serde(default)]
    pub params: Value,
}

/// Output from one executed step in a batch.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BatchStepResult {
    pub index: usize,
    pub command: String,
    pub result: Value,
}

/// Aggregate result returned by [`execute_batch`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BatchResult {
    pub ok: bool,
    pub completed_steps: usize,
    pub rolled_back: bool,
    pub results: Vec<BatchStepResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Parameters for loading a `.3mf` project or model archive from raw bytes.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LoadProject3mfBytesParams {
    pub bytes: Vec<u8>,
    #[serde(default)]
    pub name: Option<String>,
}

/// Raw `.3mf` project archive bytes and counts returned by `project.export_3mf_bytes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectExportBytesResult {
    pub bytes: Vec<u8>,
    pub object_count: usize,
    pub modifier_count: usize,
}

/// Parameters for selecting the active build plate (`plates.select`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SelectPlateParams {
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub id: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
}

/// Parameters for adding a new empty build plate (`plates.add`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct AddPlateParams {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "default_true")]
    pub select: bool,
}

/// Parameters for renaming an existing build plate (`plates.rename`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RenamePlateParams {
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub id: Option<u32>,
    #[serde(default)]
    pub name_match: Option<String>,
    pub name: String,
}

/// Parameters for removing a build plate (`plates.remove`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RemovePlateParams {
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub id: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
}

/// Summary returned by `project.load_3mf_bytes` and `project.load_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectLoadResult {
    pub is_layercraft_project: bool,
    pub object_count: usize,
    pub modifier_count: usize,
    #[serde(default)]
    pub plate_count: usize,
    #[serde(default)]
    pub active_plate_id: u32,
    pub active_profiles: ActiveProfiles,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SaveProjectFileParams {
    pub path: String,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SaveProjectFileResult {
    pub path: String,
    pub bytes_written: usize,
    pub object_count: usize,
    pub modifier_count: usize,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LoadProjectFileParams {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectObjectRecord {
    id: u64,
    name: String,
    resource_id: u32,
    transform: ObjectTransform,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectModifierRecord {
    id: u64,
    name: String,
    role: ModifierRole,
    #[serde(default)]
    primitive: Option<ModifierPrimitive>,
    #[serde(default)]
    resource_id: Option<u32>,
    transform: ModifierTransform,
    overrides: ModifierOverrides,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectPlateRecord {
    id: u32,
    name: String,
    objects: Vec<ProjectObjectRecord>,
    #[serde(default)]
    modifiers: Vec<ProjectModifierRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LayerCraftProjectManifest {
    format_version: u32,
    config: PrintConfig,
    #[serde(default)]
    active_profiles: ActiveProfiles,
    #[serde(default)]
    objects: Vec<ProjectObjectRecord>,
    #[serde(default)]
    modifiers: Vec<ProjectModifierRecord>,
    #[serde(default)]
    plates: Vec<ProjectPlateRecord>,
    #[serde(default)]
    active_plate_id: Option<u32>,
}

/// List compact command names and one-line summaries (optionally filtered by prefix).
pub fn list_command_summaries(prefix: Option<&str>) -> Vec<CommandSummary> {
    let filter = prefix.map(str::trim).filter(|s| !s.is_empty());
    command_registry()
        .into_iter()
        .filter(|c| match filter {
            Some(p) => c.name.starts_with(p),
            None => true,
        })
        .map(|c| CommandSummary {
            name: c.name,
            summary: c.summary,
        })
        .collect()
}

/// Look up the full descriptor and schemas for one command name.
pub fn describe_command(name: &str) -> Option<CommandDescriptor> {
    command_registry().into_iter().find(|c| c.name == name)
}

/// Return descriptors and JSON schemas for all supported commands.
pub fn command_registry() -> Vec<CommandDescriptor> {
    #[cfg_attr(target_arch = "wasm32", allow(unused_mut))]
    let mut cmds = vec![
        CommandDescriptor {
            name: "plate.clear",
            summary: "Clear loaded meshes, cached slices, and generated G-code from the plate.",
            params_schema: schema_val::<EmptyParams>(),
            result_schema: schema_val::<OkResult>(),
        },
        CommandDescriptor {
            name: "plate.add_stl_bytes",
            summary: "Load a binary or ASCII STL mesh from a raw byte array.",
            params_schema: schema_val::<AddStlBytesParams>(),
            result_schema: schema_val::<MeshInspection>(),
        },
        CommandDescriptor {
            name: "plate.add_3mf_bytes",
            summary: "Load a 3MF mesh archive from a raw byte array.",
            params_schema: schema_val::<Add3mfBytesParams>(),
            result_schema: schema_val::<MeshInspection>(),
        },
        CommandDescriptor {
            name: "plate.add_sample_shape",
            summary: "Load one of LayerCraft's built-in sample primitives onto the plate.",
            params_schema: schema_val::<AddSampleShapeParams>(),
            result_schema: schema_val::<MeshInspection>(),
        },
        CommandDescriptor {
            name: "plate.inspect",
            summary: "Inspect the currently loaded mesh (bounds, volume, manifold edges).",
            params_schema: schema_val::<EmptyParams>(),
            result_schema: schema_val::<MeshInspection>(),
        },
        CommandDescriptor {
            name: "plate.repair_mesh",
            summary: "Repair object meshes (weld, dedupe, T-junctions, winding, inverted shells, hole filling); undoable.",
            params_schema: schema_val::<RepairMeshParams>(),
            result_schema: schema_val::<PlateRepairResult>(),
        },
        CommandDescriptor {
            name: "plate.check_layout",
            summary: "Check active build plate object bounds and pairwise XY footprint overlaps.",
            params_schema: schema_val::<EmptyParams>(),
            result_schema: schema_val::<PlateLayoutCheck>(),
        },
        CommandDescriptor {
            name: "plate.list_objects",
            summary: "List individual objects on the build plate with IDs, names, transforms, and bounds.",
            params_schema: schema_val::<EmptyParams>(),
            result_schema: schema_val::<Vec<PlateObjectInfo>>(),
        },
        CommandDescriptor {
            name: "plate.remove_object",
            summary: "Remove an object from the build plate by ID.",
            params_schema: schema_val::<RemoveObjectParams>(),
            result_schema: schema_val::<OkResult>(),
        },
        CommandDescriptor {
            name: "plate.transform_object",
            summary: "Update XY position, Euler rotation, or XYZ scale on a plate object by ID.",
            params_schema: schema_val::<TransformObjectParams>(),
            result_schema: schema_val::<PlateObjectInfo>(),
        },
        CommandDescriptor {
            name: "plate.auto_arrange",
            summary: "Pack all plate objects onto the bed with non-overlapping spacing.",
            params_schema: schema_val::<AutoArrangeParams>(),
            result_schema: schema_val::<Vec<PlateObjectInfo>>(),
        },
        CommandDescriptor {
            name: "plate.add_modifier_primitive",
            summary: "Add a box or cylinder 3D modifier volume (parameter modifier, blocker, or enforcer) to the plate.",
            params_schema: schema_val::<AddModifierPrimitiveParams>(),
            result_schema: schema_val::<PlateModifierInfo>(),
        },
        CommandDescriptor {
            name: "plate.add_modifier_stl_bytes",
            summary: "Add an STL mesh 3D modifier volume from raw bytes.",
            params_schema: schema_val::<AddModifierBytesParams>(),
            result_schema: schema_val::<PlateModifierInfo>(),
        },
        CommandDescriptor {
            name: "plate.add_modifier_3mf_bytes",
            summary: "Add a 3MF mesh 3D modifier volume from raw bytes.",
            params_schema: schema_val::<AddModifierBytesParams>(),
            result_schema: schema_val::<PlateModifierInfo>(),
        },
        CommandDescriptor {
            name: "plate.list_modifiers",
            summary: "List all 3D modifier volumes on the build plate.",
            params_schema: schema_val::<EmptyParams>(),
            result_schema: schema_val::<Vec<PlateModifierInfo>>(),
        },
        CommandDescriptor {
            name: "plate.update_modifier",
            summary: "Update role, transform, name, or local overrides of a plate modifier volume by ID.",
            params_schema: schema_val::<UpdateModifierParams>(),
            result_schema: schema_val::<PlateModifierInfo>(),
        },
        CommandDescriptor {
            name: "plate.remove_modifier",
            summary: "Remove a 3D modifier volume from the build plate by ID.",
            params_schema: schema_val::<RemoveModifierParams>(),
            result_schema: schema_val::<OkResult>(),
        },
        CommandDescriptor {
            name: "config.get",
            summary: "Return the active print configuration.",
            params_schema: schema_val::<EmptyParams>(),
            result_schema: schema_val::<PrintConfig>(),
        },
        CommandDescriptor {
            name: "config.set",
            summary: "Validate and replace the active print configuration.",
            params_schema: schema_val::<SetConfigParams>(),
            result_schema: schema_val::<PrintConfig>(),
        },
        CommandDescriptor {
            name: "profile.list",
            summary: "List loaded machine, filament, and process profiles.",
            params_schema: schema_val::<ListProfilesParams>(),
            result_schema: schema_val::<Vec<ProfileSummary>>(),
        },
        CommandDescriptor {
            name: "profile.load_json",
            summary: "Load one or more JSON profile objects into the session library.",
            params_schema: schema_val::<LoadProfileJsonParams>(),
            result_schema: schema_val::<Vec<ProfileSummary>>(),
        },
        CommandDescriptor {
            name: "profile.resolve",
            summary: "Resolve a named profile with its full inheritance chain.",
            params_schema: schema_val::<ResolveProfileParams>(),
            result_schema: schema_val::<ResolvedProfile>(),
        },
        CommandDescriptor {
            name: "profile.apply",
            summary: "Resolve and apply machine, filament, and/or process profiles onto the active configuration.",
            params_schema: schema_val::<ApplyProfilesParams>(),
            result_schema: schema_val::<ProfileApplyResult>(),
        },
        CommandDescriptor {
            name: "profile.save_user",
            summary: "Snapshot the active configuration section as a user preset (optionally persisting to disk).",
            params_schema: schema_val::<SaveUserProfileParams>(),
            result_schema: schema_val::<SaveUserProfileResult>(),
        },
        CommandDescriptor {
            name: "profile.delete_user",
            summary: "Delete a user preset from the library (optionally removing its disk file; built-ins protected).",
            params_schema: schema_val::<DeleteUserProfileParams>(),
            result_schema: schema_val::<DeleteUserProfileResult>(),
        },
        CommandDescriptor {
            name: "profile.export_json",
            summary: "Serialize a stored machine, filament, or process profile into JSON.",
            params_schema: schema_val::<ExportProfileJsonParams>(),
            result_schema: schema_val::<ExportProfileJsonResult>(),
        },
        CommandDescriptor {
            name: "slice.run",
            summary: "Slice the loaded mesh using the active print configuration.",
            params_schema: schema_val::<EmptyParams>(),
            result_schema: schema_val::<SliceSummaryResult>(),
        },
        CommandDescriptor {
            name: "layer.inspect",
            summary: "Inspect a single sliced layer (Z height, region count, feature lengths).",
            params_schema: schema_val::<InspectLayerParams>(),
            result_schema: schema_val::<LayerInspection>(),
        },
        CommandDescriptor {
            name: "gcode.export",
            summary: "Emit G-code and print statistics from the sliced plate.",
            params_schema: schema_val::<EmptyParams>(),
            result_schema: schema_val::<GcodeOutput>(),
        },
        CommandDescriptor {
            name: "gcode.export_bgcode",
            summary: "Encode the sliced plate into Prusa Binary G-code (.bgcode) with Heatshrink/Deflate compression and CRC32.",
            params_schema: schema_val::<ExportBgcodeParams>(),
            result_schema: schema_val::<ExportBgcodeResult>(),
        },
        CommandDescriptor {
            name: "gcode.evaluate",
            summary: "Evaluate physical invariants and safety metrics on a G-code string.",
            params_schema: schema_val::<EvaluateGcodeParams>(),
            result_schema: schema_val::<GcodeReport>(),
        },
        CommandDescriptor {
            name: "printer.list",
            summary: "List configured saved printers with secret credentials redacted.",
            params_schema: schema_val::<EmptyParams>(),
            result_schema: schema_val::<Vec<PrinterProfileSummary>>(),
        },
        CommandDescriptor {
            name: "printer.upsert",
            summary: "Create or update a saved printer profile by ID (optionally persisting to disk).",
            params_schema: schema_val::<UpsertPrinterParams>(),
            result_schema: schema_val::<PrinterProfileSummary>(),
        },
        CommandDescriptor {
            name: "printer.remove",
            summary: "Remove a saved printer profile by ID.",
            params_schema: schema_val::<RemovePrinterParams>(),
            result_schema: schema_val::<OkResult>(),
        },
        CommandDescriptor {
            name: "config.patch",
            summary: "Partially patch the active PrintConfig via JSON merge patch and validate.",
            params_schema: schema_val::<PatchConfigParams>(),
            result_schema: schema_val::<PrintConfig>(),
        },
        CommandDescriptor {
            name: "plate.analyze_printability",
            summary: "Run Printability / DFM diagnostics (overhangs, floating islands, thin walls, adhesion, feature breakdown).",
            params_schema: schema_val::<AnalyzePrintabilityParams>(),
            result_schema: schema_val::<PrintabilityReport>(),
        },
        CommandDescriptor {
            name: "slice.compare",
            summary: "Non-destructively slice and compare two config variants (deltas and per-layer 2D raster IoU).",
            params_schema: schema_val::<CompareSlicesParams>(),
            result_schema: schema_val::<SliceComparisonReport>(),
        },
        CommandDescriptor {
            name: "session.undo",
            summary: "Undo the most recent mutating plate, modifier, profile, or settings change.",
            params_schema: schema_val::<EmptyParams>(),
            result_schema: schema_val::<HistoryActionResult>(),
        },
        CommandDescriptor {
            name: "session.redo",
            summary: "Re-apply the most recently undone change.",
            params_schema: schema_val::<EmptyParams>(),
            result_schema: schema_val::<HistoryActionResult>(),
        },
        CommandDescriptor {
            name: "session.history",
            summary: "Inspect current undo and redo stack depths and recent action labels.",
            params_schema: schema_val::<EmptyParams>(),
            result_schema: schema_val::<HistoryStatus>(),
        },
        CommandDescriptor {
            name: "project.export_3mf_bytes",
            summary: "Serialize the full plate (objects, transforms, modifiers, profiles, settings) into a `.3mf` project archive.",
            params_schema: schema_val::<EmptyParams>(),
            result_schema: schema_val::<ProjectExportBytesResult>(),
        },
        CommandDescriptor {
            name: "project.load_3mf_bytes",
            summary: "Load a LayerCraft `.3mf` project archive (or multi-part `.3mf` model) from raw bytes.",
            params_schema: schema_val::<LoadProject3mfBytesParams>(),
            result_schema: schema_val::<ProjectLoadResult>(),
        },
        CommandDescriptor {
            name: "plates.list",
            summary: "List all logical build plates in the session with object/modifier/slice status.",
            params_schema: schema_val::<EmptyParams>(),
            result_schema: schema_val::<Vec<PlateSummary>>(),
        },
        CommandDescriptor {
            name: "plates.select",
            summary: "Switch the active build plate by zero-based index, numeric id, or plate name.",
            params_schema: schema_val::<SelectPlateParams>(),
            result_schema: schema_val::<PlateSummary>(),
        },
        CommandDescriptor {
            name: "plates.add",
            summary: "Add a new empty build plate to the project and optionally switch to it.",
            params_schema: schema_val::<AddPlateParams>(),
            result_schema: schema_val::<PlateSummary>(),
        },
        CommandDescriptor {
            name: "plates.rename",
            summary: "Rename an existing build plate (defaults to the active plate when selector is omitted).",
            params_schema: schema_val::<RenamePlateParams>(),
            result_schema: schema_val::<PlateSummary>(),
        },
        CommandDescriptor {
            name: "plates.remove",
            summary: "Remove a build plate by index, id, or name (at least one plate must remain).",
            params_schema: schema_val::<RemovePlateParams>(),
            result_schema: schema_val::<PlateSummary>(),
        },
    ];
    #[cfg(not(target_arch = "wasm32"))]
    cmds.push(CommandDescriptor {
        name: "project.save_file",
        summary: "Save the full LayerCraft `.3mf` project archive directly to disk.",
        params_schema: schema_val::<SaveProjectFileParams>(),
        result_schema: schema_val::<SaveProjectFileResult>(),
    });
    #[cfg(not(target_arch = "wasm32"))]
    cmds.push(CommandDescriptor {
        name: "project.load_file",
        summary: "Load a LayerCraft `.3mf` project (or multi-part `.3mf` model) from disk.",
        params_schema: schema_val::<LoadProjectFileParams>(),
        result_schema: schema_val::<ProjectLoadResult>(),
    });
    #[cfg(not(target_arch = "wasm32"))]
    cmds.push(CommandDescriptor {
        name: "gcode.save_file",
        summary: "Emit G-code or .bgcode if needed, write directly to a file on disk, and optionally run physical evaluation.",
        params_schema: schema_val::<SaveGcodeFileParams>(),
        result_schema: schema_val::<SaveGcodeFileResult>(),
    });
    #[cfg(not(target_arch = "wasm32"))]
    cmds.push(CommandDescriptor {
        name: "printer.status",
        summary: "Query live firmware info, temperatures, operational state, and storage list from a configured printer.",
        params_schema: schema_val::<PrinterStatusParams>(),
        result_schema: schema_val::<PrinterStatusReport>(),
    });
    #[cfg(not(target_arch = "wasm32"))]
    cmds.push(CommandDescriptor {
        name: "printer.upload",
        summary: "Slice if needed, validate safety invariants, and upload .bgcode or .gcode directly to a printer.",
        params_schema: schema_val::<PrinterUploadParams>(),
        result_schema: schema_val::<PrinterUploadResult>(),
    });
    #[cfg(not(target_arch = "wasm32"))]
    cmds.push(CommandDescriptor {
        name: "plate.add_mesh_file",
        summary: "Load an STL or 3MF mesh file from disk.",
        params_schema: schema_val::<AddMeshFileParams>(),
        result_schema: schema_val::<MeshInspection>(),
    });
    #[cfg(not(target_arch = "wasm32"))]
    cmds.push(CommandDescriptor {
        name: "plate.add_modifier_mesh_file",
        summary: "Load an STL or 3MF mesh file from disk as a 3D modifier volume.",
        params_schema: schema_val::<AddModifierMeshFileParams>(),
        result_schema: schema_val::<PlateModifierInfo>(),
    });
    #[cfg(not(target_arch = "wasm32"))]
    cmds.push(CommandDescriptor {
        name: "profile.load_file",
        summary: "Load one or more JSON profiles from a file on disk.",
        params_schema: schema_val::<LoadProfileFileParams>(),
        result_schema: schema_val::<Vec<ProfileSummary>>(),
    });
    #[cfg(not(target_arch = "wasm32"))]
    cmds.push(CommandDescriptor {
        name: "profile.load_dir",
        summary: "Recursively load JSON profiles from a directory tree.",
        params_schema: schema_val::<LoadProfileDirParams>(),
        result_schema: schema_val::<ProfileDirLoadReport>(),
    });
    cmds
}

fn schema_val<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

/// Merged raw mesh, merged prepared mesh, and cached inspection for one plate's objects.
type MergedPlateMeshes = (
    Option<Arc<TriangleMesh>>,
    Option<Arc<TriangleMesh>>,
    Option<MeshInspection>,
);

/// Build the merged plate meshes for `objects`.
///
/// Single-object plates share the object's `Arc`s (zero copy); multi-object plates merge
/// borrowed meshes without cloning each source mesh first.
fn merge_plate_meshes(objects: &[PlateObjectEntry]) -> Result<MergedPlateMeshes, EngineError> {
    match objects {
        [] => Ok((None, None, None)),
        [only] => {
            let insp = inspect_mesh(&only.prepared_mesh);
            Ok((
                Some(Arc::clone(&only.raw_mesh)),
                Some(Arc::clone(&only.prepared_mesh)),
                Some(insp),
            ))
        }
        many => {
            let merged_raw = TriangleMesh::merge_refs(many.iter().map(|o| o.raw_mesh.as_ref()))?;
            let merged_prep =
                TriangleMesh::merge_refs(many.iter().map(|o| o.prepared_mesh.as_ref()))?;
            let insp = inspect_mesh(&merged_prep);
            Ok((
                Some(Arc::new(merged_raw)),
                Some(Arc::new(merged_prep)),
                Some(insp),
            ))
        }
    }
}

fn rebuild_plate_mesh(session: &mut Session) -> Result<Option<MeshInspection>, EngineError> {
    session.sliced = None;
    session.last_gcode = None;
    let (mesh, prepared_mesh, insp) = merge_plate_meshes(&session.objects)?;
    session.mesh = mesh;
    session.prepared_mesh = prepared_mesh;
    session.plate_inspection = insp.clone();
    session.recompute_plate_layout();
    Ok(insp)
}

/// Repair one (or every) object's raw mesh, re-prepare it with its transform, and rebuild the plate.
///
/// Transactional: all repairs are computed before any object is modified, so an error leaves
/// the session untouched. Unchanged meshes keep their `Arc`s (and cached slices survive a no-op).
fn repair_plate_objects(
    session: &mut Session,
    object_id: Option<u64>,
    config: MeshRepairConfig,
) -> Result<PlateRepairResult, EngineError> {
    if session.objects.is_empty() {
        return Err(EngineError::NoMeshLoaded);
    }
    if let Some(id) = object_id
        && !session.objects.iter().any(|o| o.id == id)
    {
        return Err(EngineError::ObjectNotFound(id));
    }
    type PendingRepair = (usize, Option<(Arc<TriangleMesh>, Arc<TriangleMesh>)>);
    let mut pending: Vec<PendingRepair> = Vec::new();
    let mut reports = Vec::new();
    for (idx, obj) in session.objects.iter().enumerate() {
        if object_id.is_some_and(|id| id != obj.id) {
            continue;
        }
        let repaired = repair_mesh(&obj.raw_mesh, &config)?;
        let replacement = if repaired.report.changed {
            let prepared = prepare_transformed_object(&repaired.mesh, obj.transform)?;
            Some((Arc::new(repaired.mesh), Arc::new(prepared)))
        } else {
            None
        };
        pending.push((idx, replacement));
        reports.push(ObjectRepairResult {
            object_id: obj.id,
            name: obj.name.clone(),
            report: repaired.report,
        });
    }
    let mut changed = false;
    for (idx, replacement) in pending {
        if let Some((raw, prepared)) = replacement
            && let Some(obj) = session.objects.get_mut(idx)
        {
            obj.raw_mesh = raw;
            obj.prepared_mesh = prepared;
            changed = true;
        }
    }
    if changed {
        let _ = rebuild_plate_mesh(session)?;
    } else if session.plate_inspection.is_none() {
        session.plate_inspection = session.prepared_mesh.as_deref().map(inspect_mesh);
    }
    Ok(PlateRepairResult {
        objects: reports,
        changed,
        plate_inspection: session.plate_inspection.clone(),
    })
}

fn auto_arrange_session_objects(session: &mut Session, spacing_mm: f64) -> Result<(), EngineError> {
    if session.objects.is_empty() {
        return Ok(());
    }
    let mut footprints = Vec::with_capacity(session.objects.len());
    for obj in &session.objects {
        let size = obj
            .prepared_mesh
            .aabb()
            .map(|bb| bb.size())
            .ok_or(EngineError::NoMeshLoaded)?;
        footprints.push((size.x.max(0.1), size.y.max(0.1)));
    }
    let positions = auto_arrange_footprints(
        &footprints,
        session.config.machine.bed_width_mm,
        session.config.machine.bed_depth_mm,
        spacing_mm,
    )?;
    for (obj, pos) in session.objects.iter_mut().zip(positions) {
        obj.transform.position_xy_mm = pos;
        obj.prepared_mesh = Arc::new(prepare_transformed_object(&obj.raw_mesh, obj.transform)?);
    }
    Ok(())
}

fn add_plate_object(
    session: &mut Session,
    name: String,
    mesh: TriangleMesh,
    append: bool,
) -> Result<MeshInspection, EngineError> {
    if !append {
        session.objects.clear();
    }
    let id = if session.next_object_id == 0 {
        1
    } else {
        session.next_object_id
    };
    session.next_object_id = id + 1;

    let transform = ObjectTransform {
        position_xy_mm: [
            0.5 * session.config.machine.bed_width_mm,
            0.5 * session.config.machine.bed_depth_mm,
        ],
        rotation_deg: [0.0, 0.0, 0.0],
        scale: [1.0, 1.0, 1.0],
    };
    let prepared = prepare_transformed_object(&mesh, transform)?;
    session.objects.push(PlateObjectEntry {
        id,
        name,
        raw_mesh: Arc::new(mesh),
        transform,
        prepared_mesh: Arc::new(prepared),
    });
    if append && session.objects.len() > 1 {
        auto_arrange_session_objects(session, 6.0)?;
    }
    rebuild_plate_mesh(session)?.ok_or(EngineError::NoMeshLoaded)
}

fn build_centered_plate_objects(
    parts: Vec<ThreeMfPart>,
    next_object_id: &mut u64,
    bed_width_mm: f64,
    bed_depth_mm: f64,
) -> Result<Vec<PlateObjectEntry>, EngineError> {
    if parts.is_empty() {
        return Ok(Vec::new());
    }
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut part_entries: Vec<(String, TriangleMesh, [f64; 2])> = Vec::with_capacity(parts.len());

    for part in parts {
        let Some(bb) = part.mesh.aabb() else {
            continue;
        };
        min_x = min_x.min(bb.min.x);
        min_y = min_y.min(bb.min.y);
        max_x = max_x.max(bb.max.x);
        max_y = max_y.max(bb.max.y);
        let c = bb.center();
        part_entries.push((part.name, part.mesh, [c.x, c.y]));
    }
    if part_entries.is_empty() {
        return Ok(Vec::new());
    }

    let group_cx = 0.5 * (min_x + max_x);
    let group_cy = 0.5 * (min_y + max_y);
    let shift_x = 0.5 * bed_width_mm - group_cx;
    let shift_y = 0.5 * bed_depth_mm - group_cy;

    let mut entries = Vec::with_capacity(part_entries.len());
    for (name, raw_mesh, [cx, cy]) in part_entries {
        let id = if *next_object_id == 0 {
            1
        } else {
            *next_object_id
        };
        *next_object_id = id + 1;
        let transform = ObjectTransform {
            position_xy_mm: [cx + shift_x, cy + shift_y],
            rotation_deg: [0.0, 0.0, 0.0],
            scale: [1.0, 1.0, 1.0],
        };
        let prepared_mesh = Arc::new(prepare_transformed_object(&raw_mesh, transform)?);
        entries.push(PlateObjectEntry {
            id,
            name,
            raw_mesh: Arc::new(raw_mesh),
            transform,
            prepared_mesh,
        });
    }
    Ok(entries)
}

fn add_3mf_document_objects(
    session: &mut Session,
    doc: ThreeMfDocument,
    name_override: Option<String>,
    append: bool,
) -> Result<MeshInspection, EngineError> {
    let mut parts = doc.parts;
    let plates_meta = doc.plates;
    if parts.is_empty() {
        return Err(EngineError::NoMeshLoaded);
    }

    let is_single_plate_single_part = plates_meta.len() <= 1 && parts.len() == 1;
    if (append && parts.len() == 1) || (is_single_plate_single_part && !append) {
        if is_single_plate_single_part && !append {
            session.plate_slots = vec![StoredPlate::empty(
                plates_meta.first().map(|p| p.id).unwrap_or(1),
                plates_meta
                    .first()
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| "Plate 1".to_owned()),
            )];
            session.active_plate_idx = 0;
            session.next_plate_id = session
                .plate_slots
                .first()
                .map(|p| p.id.saturating_add(1))
                .unwrap_or(2);
            session.modifiers.clear();
        }
        if let Some(part) = parts.pop() {
            let label = name_override
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .unwrap_or(part.name);
            return add_plate_object(session, label, part.mesh, append);
        }
        return Err(EngineError::NoMeshLoaded);
    }

    if append {
        let bed_w = session.config.machine.bed_width_mm;
        let bed_d = session.config.machine.bed_depth_mm;
        let mut next_id = session.next_object_id;
        let entries = build_centered_plate_objects(parts, &mut next_id, bed_w, bed_d)?;
        if entries.is_empty() {
            return Err(EngineError::NoMeshLoaded);
        }
        session.next_object_id = next_id;
        session.objects.extend(entries);
        if session.objects.len() > 1 {
            auto_arrange_session_objects(session, 6.0)?;
        }
        return rebuild_plate_mesh(session)?.ok_or(EngineError::NoMeshLoaded);
    }

    let plate_defs: Vec<(u32, String)> = if plates_meta.is_empty() {
        vec![(1, "Plate 1".to_owned())]
    } else {
        plates_meta.into_iter().map(|p| (p.id, p.name)).collect()
    };

    let mut buckets: Vec<Vec<ThreeMfPart>> = (0..plate_defs.len()).map(|_| Vec::new()).collect();
    for part in parts {
        let target_idx = if part.plate_index < buckets.len() {
            part.plate_index
        } else {
            0
        };
        if let Some(bucket) = buckets.get_mut(target_idx) {
            bucket.push(part);
        }
    }

    let bed_w = session.config.machine.bed_width_mm;
    let bed_d = session.config.machine.bed_depth_mm;
    let mut next_id = session.next_object_id;
    let mut max_plate_id: u32 = 0;
    let mut new_slots: Vec<StoredPlate> = Vec::with_capacity(plate_defs.len());

    for ((plate_id, plate_name), bucket) in plate_defs.into_iter().zip(buckets) {
        max_plate_id = max_plate_id.max(plate_id);
        let objs = build_centered_plate_objects(bucket, &mut next_id, bed_w, bed_d)?;
        let (mesh, prepared_mesh, plate_inspection) = merge_plate_meshes(&objs)?;
        new_slots.push(StoredPlate {
            id: plate_id,
            name: plate_name,
            objects: objs,
            modifiers: Vec::new(),
            mesh,
            prepared_mesh,
            plate_inspection,
            sliced: None,
            last_gcode: None,
        });
    }

    let first_non_empty = new_slots
        .iter()
        .position(|p| !p.objects.is_empty())
        .ok_or(EngineError::NoMeshLoaded)?;

    session.next_object_id = next_id;
    session.objects.clear();
    session.modifiers.clear();
    session.mesh = None;
    session.prepared_mesh = None;
    session.plate_inspection = None;
    session.sliced = None;
    session.last_gcode = None;
    session.plate_slots = new_slots;
    session.active_plate_idx = first_non_empty;
    session.next_plate_id = max_plate_id.saturating_add(1).max(2);
    session.swap_slot_with_live(first_non_empty);
    session.recompute_plate_layout();

    session
        .plate_inspection
        .clone()
        .ok_or(EngineError::NoMeshLoaded)
}

/// Compute the exact 3MF 4×3 row-vector affine matrix matching [`prepare_transformed_object`].
pub fn object_transform_3mf_matrix(
    raw_mesh: &TriangleMesh,
    transform: ObjectTransform,
) -> Result<[f64; 12], EngineError> {
    let [px, py] = transform.position_xy_mm;
    let [rx, ry, rz] = transform.rotation_deg;
    let [sx, sy, sz] = transform.scale;
    if !px.is_finite()
        || !py.is_finite()
        || !rx.is_finite()
        || !ry.is_finite()
        || !rz.is_finite()
        || !sx.is_finite()
        || !sy.is_finite()
        || !sz.is_finite()
        || sx <= 0.0
        || sy <= 0.0
        || sz <= 0.0
    {
        return Err(EngineError::InvalidArgument {
            field: "transform",
            reason: "object transform must be finite with positive scale",
        });
    }
    let valid: Vec<_> = raw_mesh
        .triangles
        .iter()
        .copied()
        .filter(|t| t.is_finite() && t.area_mm2() > 1e-12)
        .collect();
    let mut clean = TriangleMesh::new(valid)?;
    let bb0 = clean.aabb().ok_or(GeomError::EmptyMesh)?;
    let c0 = bb0.center();
    let ps = [c0.x, c0.y, bb0.min.z];

    clean.scale(sx, sy, sz)?;
    let bb1 = clean.aabb().ok_or(GeomError::EmptyMesh)?;
    let pr = bb1.center();

    clean.rotate_euler_deg(rx, ry, rz)?;
    let bb2 = clean.aabb().ok_or(GeomError::EmptyMesh)?;
    let c2 = bb2.center();
    let gx = px - c2.x;
    let gy = py - c2.y;
    let gz = -bb2.min.z;

    let [col0, col1, col2] = TriangleMesh::euler_xyz_deg_columns(rx, ry, rz)?;
    let [r00, r10, r20] = col0;
    let [r01, r11, r21] = col1;
    let [r02, r12, r22] = col2;

    let [psx, psy, psz] = ps;
    let ux = psx * (1.0 - sx) - pr.x;
    let uy = psy * (1.0 - sy) - pr.y;
    let uz = psz * (1.0 - sz) - pr.z;

    let tx = r00 * ux + r01 * uy + r02 * uz + pr.x + gx;
    let ty = r10 * ux + r11 * uy + r12 * uz + pr.y + gy;
    let tz = r20 * ux + r21 * uy + r22 * uz + pr.z + gz;

    Ok([
        r00 * sx,
        r10 * sx,
        r20 * sx,
        r01 * sy,
        r11 * sy,
        r21 * sy,
        r02 * sz,
        r12 * sz,
        r22 * sz,
        tx,
        ty,
        tz,
    ])
}

/// Export the full multi-plate session (all plates, objects, transforms, modifiers, profiles, and settings)
/// as a `.3mf` project archive.
pub fn export_project_3mf_bytes(session: &Session) -> Result<Vec<u8>, EngineError> {
    let mut resources: Vec<ThreeMfResourceMesh> = Vec::new();
    let mut build_items: Vec<ThreeMfBuildItem> = Vec::new();
    let mut export_plates: Vec<ThreeMfExportPlate> = Vec::with_capacity(session.plate_slots.len());
    let mut plate_records: Vec<ProjectPlateRecord> = Vec::with_capacity(session.plate_slots.len());
    let mut active_obj_records: Vec<ProjectObjectRecord> = Vec::new();
    let mut active_mod_records: Vec<ProjectModifierRecord> = Vec::new();
    let mut next_res_id: u32 = 1;

    for (idx, slot) in session.plate_slots.iter().enumerate() {
        let is_active = idx == session.active_plate_idx;
        let (objs, mods) = if is_active {
            (session.objects.as_slice(), session.modifiers.as_slice())
        } else {
            (slot.objects.as_slice(), slot.modifiers.as_slice())
        };

        let mut plate_obj_records = Vec::with_capacity(objs.len());
        let mut plate_instances = Vec::with_capacity(objs.len());
        for obj in objs {
            let res_id = next_res_id;
            next_res_id += 1;
            resources.push(ThreeMfResourceMesh {
                id: res_id,
                name: obj.name.clone(),
                object_type: "model".to_owned(),
                mesh: TriangleMesh::clone(&obj.raw_mesh),
            });
            let mat = object_transform_3mf_matrix(&obj.raw_mesh, obj.transform)?;
            build_items.push(ThreeMfBuildItem {
                object_id: res_id,
                transform: Some(mat),
            });
            plate_instances.push((res_id, 0));
            plate_obj_records.push(ProjectObjectRecord {
                id: obj.id,
                name: obj.name.clone(),
                resource_id: res_id,
                transform: obj.transform,
            });
        }

        let mut plate_mod_records = Vec::with_capacity(mods.len());
        for m in mods {
            let resource_id = if m.primitive.is_none() {
                let res_id = next_res_id;
                next_res_id += 1;
                resources.push(ThreeMfResourceMesh {
                    id: res_id,
                    name: m.name.clone(),
                    object_type: "other".to_owned(),
                    mesh: TriangleMesh::clone(&m.raw_mesh),
                });
                Some(res_id)
            } else {
                None
            };
            plate_mod_records.push(ProjectModifierRecord {
                id: m.id,
                name: m.name.clone(),
                role: m.role,
                primitive: m.primitive,
                resource_id,
                transform: m.transform,
                overrides: m.overrides.clone(),
            });
        }

        if is_active {
            active_obj_records = plate_obj_records.clone();
            active_mod_records = plate_mod_records.clone();
        }
        export_plates.push(ThreeMfExportPlate {
            id: slot.id,
            name: slot.name.clone(),
            instances: plate_instances,
        });
        plate_records.push(ProjectPlateRecord {
            id: slot.id,
            name: slot.name.clone(),
            objects: plate_obj_records,
            modifiers: plate_mod_records,
        });
    }

    let manifest = LayerCraftProjectManifest {
        format_version: PROJECT_FORMAT_VERSION,
        config: session.config.clone(),
        active_profiles: session.active_profiles.clone(),
        objects: active_obj_records,
        modifiers: active_mod_records,
        plates: plate_records,
        active_plate_id: Some(session.active_plate_id()),
    };
    let project_json = serde_json::to_string_pretty(&manifest)?;
    let doc = ThreeMfExportDocument {
        resources,
        build_items,
        plates: export_plates,
        project_json: Some(project_json),
    };
    Ok(write_3mf_document_bytes(&doc)?)
}

fn load_manifest_plate_records(
    objects: Vec<ProjectObjectRecord>,
    modifiers: Vec<ProjectModifierRecord>,
    resource_map: &HashMap<u32, Arc<TriangleMesh>>,
    max_obj_id: &mut u64,
    max_mod_id: &mut u64,
) -> Result<(Vec<PlateObjectEntry>, Vec<PlateModifierEntry>), EngineError> {
    let mut new_objects = Vec::with_capacity(objects.len());
    for rec in objects {
        let res = resource_map.get(&rec.resource_id).ok_or_else(|| {
            EngineError::InvalidProject(format!(
                "missing 3MF resource {} for object `{}`",
                rec.resource_id, rec.name
            ))
        })?;
        let prepared_mesh = Arc::new(prepare_transformed_object(res, rec.transform)?);
        *max_obj_id = (*max_obj_id).max(rec.id);
        new_objects.push(PlateObjectEntry {
            id: rec.id,
            name: rec.name,
            raw_mesh: Arc::clone(res),
            transform: rec.transform,
            prepared_mesh,
        });
    }

    let mut new_modifiers = Vec::with_capacity(modifiers.len());
    for rec in modifiers {
        rec.overrides.validate()?;
        let raw_mesh = if let Some(prim) = rec.primitive {
            Arc::new(build_modifier_primitive(prim)?)
        } else if let Some(rid) = rec.resource_id {
            let res = resource_map.get(&rid).ok_or_else(|| {
                EngineError::InvalidProject(format!(
                    "missing 3MF resource {rid} for modifier `{}`",
                    rec.name
                ))
            })?;
            Arc::clone(res)
        } else {
            return Err(EngineError::InvalidProject(format!(
                "modifier `{}` has neither primitive nor resource_id",
                rec.name
            )));
        };
        let prepared_mesh = Arc::new(prepare_modifier_mesh(&raw_mesh, rec.transform)?);
        *max_mod_id = (*max_mod_id).max(rec.id);
        new_modifiers.push(PlateModifierEntry {
            id: rec.id,
            name: rec.name,
            role: rec.role,
            primitive: rec.primitive,
            raw_mesh,
            transform: rec.transform,
            prepared_mesh,
            overrides: rec.overrides,
        });
    }
    Ok((new_objects, new_modifiers))
}

/// Load a LayerCraft `.3mf` project archive (or fall back to multi-plate `.3mf` import if no manifest is present).
pub fn load_project_3mf_bytes(
    session: &mut Session,
    bytes: &[u8],
    fallback_name: Option<&str>,
) -> Result<ProjectLoadResult, EngineError> {
    let doc = read_3mf_document_bytes(bytes)?;
    let Some(project_json) = doc.project_json.as_deref() else {
        session.modifiers.clear();
        let use_fallback = doc.plates.len() <= 1
            && doc.parts.len() == 1
            && doc
                .parts
                .first()
                .is_some_and(|p| p.name.starts_with("Part "));
        let override_name = if use_fallback {
            fallback_name.map(str::to_owned)
        } else {
            None
        };
        add_3mf_document_objects(session, doc, override_name, false)?;
        return Ok(ProjectLoadResult {
            is_layercraft_project: false,
            object_count: session.objects.len(),
            modifier_count: session.modifiers.len(),
            plate_count: session.plate_count(),
            active_plate_id: session.active_plate_id(),
            active_profiles: session.active_profiles.clone(),
        });
    };

    let manifest: LayerCraftProjectManifest = serde_json::from_str(project_json)
        .map_err(|e| EngineError::InvalidProject(format!("invalid project manifest JSON: {e}")))?;
    if manifest.format_version != PROJECT_FORMAT_VERSION {
        return Err(EngineError::InvalidProject(format!(
            "unsupported project format version {}",
            manifest.format_version
        )));
    }
    manifest.config.validate()?;

    let resource_map: HashMap<u32, Arc<TriangleMesh>> = doc
        .resources
        .into_iter()
        .map(|r| (r.id, Arc::new(r.mesh)))
        .collect();

    let mut max_obj_id = 0u64;
    let mut max_mod_id = 0u64;

    if manifest.plates.is_empty() {
        let (new_objects, new_modifiers) = load_manifest_plate_records(
            manifest.objects,
            manifest.modifiers,
            &resource_map,
            &mut max_obj_id,
            &mut max_mod_id,
        )?;
        session.config = manifest.config;
        session.active_profiles = manifest.active_profiles;
        session.objects = new_objects;
        session.next_object_id = max_obj_id + 1;
        session.modifiers = new_modifiers;
        session.next_modifier_id = max_mod_id + 1;
        session.plate_slots = vec![StoredPlate::empty(1, "Plate 1".to_owned())];
        session.active_plate_idx = 0;
        session.next_plate_id = 2;
        let _ = rebuild_plate_mesh(session)?;
    } else {
        let mut max_plate_id = 0u32;
        let mut new_slots = Vec::with_capacity(manifest.plates.len());
        for plate in manifest.plates {
            max_plate_id = max_plate_id.max(plate.id);
            let (objs, mods) = load_manifest_plate_records(
                plate.objects,
                plate.modifiers,
                &resource_map,
                &mut max_obj_id,
                &mut max_mod_id,
            )?;
            let (mesh, prepared_mesh, plate_inspection) = merge_plate_meshes(&objs)?;
            let trimmed_name = plate.name.trim();
            let plate_name = if trimmed_name.is_empty() {
                format!("Plate {}", plate.id)
            } else {
                trimmed_name.to_owned()
            };
            new_slots.push(StoredPlate {
                id: plate.id,
                name: plate_name,
                objects: objs,
                modifiers: mods,
                mesh,
                prepared_mesh,
                plate_inspection,
                sliced: None,
                last_gcode: None,
            });
        }
        let target_idx = manifest
            .active_plate_id
            .and_then(|aid| new_slots.iter().position(|p| p.id == aid))
            .unwrap_or(0);
        session.config = manifest.config;
        session.active_profiles = manifest.active_profiles;
        session.objects.clear();
        session.modifiers.clear();
        session.mesh = None;
        session.prepared_mesh = None;
        session.plate_inspection = None;
        session.sliced = None;
        session.last_gcode = None;
        session.next_object_id = max_obj_id + 1;
        session.next_modifier_id = max_mod_id + 1;
        session.plate_slots = new_slots;
        session.active_plate_idx = target_idx;
        session.next_plate_id = max_plate_id.saturating_add(1).max(2);
        session.swap_slot_with_live(target_idx);
        session.recompute_plate_layout();
    }

    Ok(ProjectLoadResult {
        is_layercraft_project: true,
        object_count: session.objects.len(),
        modifier_count: session.modifiers.len(),
        plate_count: session.plate_count(),
        active_plate_id: session.active_plate_id(),
        active_profiles: session.active_profiles.clone(),
    })
}

fn object_infos(session: &Session) -> Vec<PlateObjectInfo> {
    session.objects.iter().map(PlateObjectEntry::info).collect()
}

fn modifier_infos(session: &Session) -> Vec<PlateModifierInfo> {
    session
        .modifiers
        .iter()
        .map(PlateModifierEntry::info)
        .collect()
}

fn default_modifier_transform(session: &Session, raw_mesh: &TriangleMesh) -> ModifierTransform {
    let position_xyz_mm = if let Some(bb) = session.prepared_mesh.as_ref().and_then(|m| m.aabb()) {
        let c = bb.center();
        [c.x, c.y, c.z]
    } else {
        let h = raw_mesh
            .aabb()
            .map(|bb| bb.size().z.max(1.0))
            .unwrap_or(10.0);
        [
            0.5 * session.config.machine.bed_width_mm,
            0.5 * session.config.machine.bed_depth_mm,
            0.5 * h,
        ]
    };
    ModifierTransform {
        position_xyz_mm,
        rotation_deg: [0.0, 0.0, 0.0],
        scale: [1.0, 1.0, 1.0],
    }
}

fn add_plate_modifier(
    session: &mut Session,
    name: String,
    role: ModifierRole,
    primitive: Option<ModifierPrimitive>,
    raw_mesh: TriangleMesh,
    transform_opt: Option<ModifierTransform>,
    overrides: ModifierOverrides,
) -> Result<PlateModifierInfo, EngineError> {
    overrides.validate()?;
    let transform = transform_opt.unwrap_or_else(|| default_modifier_transform(session, &raw_mesh));
    let prepared_mesh = Arc::new(prepare_modifier_mesh(&raw_mesh, transform)?);
    let raw_mesh = Arc::new(raw_mesh);
    let id = if session.next_modifier_id == 0 {
        1
    } else {
        session.next_modifier_id
    };
    session.next_modifier_id = id + 1;

    let entry = PlateModifierEntry {
        id,
        name,
        role,
        primitive,
        raw_mesh,
        transform,
        prepared_mesh,
        overrides,
    };
    let info = entry.info();
    session.modifiers.push(entry);
    session.sliced = None;
    session.last_gcode = None;
    Ok(info)
}

fn apply_session_config(session: &mut Session, new_config: PrintConfig) -> Result<(), EngineError> {
    new_config.validate()?;
    let old_cx = 0.5 * session.config.machine.bed_width_mm;
    let old_cy = 0.5 * session.config.machine.bed_depth_mm;
    session.config = new_config;
    let new_cx = 0.5 * session.config.machine.bed_width_mm;
    let new_cy = 0.5 * session.config.machine.bed_depth_mm;
    if session.objects.len() == 1
        && ((new_cx - old_cx).abs() > 1e-4 || (new_cy - old_cy).abs() > 1e-4)
        && let Some(obj) = session.objects.first_mut()
        && (obj.transform.position_xy_mm[0] - old_cx).abs() < 1e-4
        && (obj.transform.position_xy_mm[1] - old_cy).abs() < 1e-4
    {
        obj.transform.position_xy_mm = [new_cx, new_cy];
        obj.prepared_mesh = Arc::new(prepare_transformed_object(&obj.raw_mesh, obj.transform)?);
        let _ = rebuild_plate_mesh(session)?;
    } else {
        session.sliced = None;
        session.last_gcode = None;
    }
    for slot in &mut session.plate_slots {
        slot.sliced = None;
        slot.last_gcode = None;
    }
    session.recompute_plate_layout();
    Ok(())
}

/// Recursively merge a JSON Merge Patch (RFC 7396) onto `target`.
pub fn merge_json_patch(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(t_map), Value::Object(p_map)) => {
            for (k, v) in p_map {
                if v.is_null() {
                    t_map.remove(k);
                } else if let Some(existing) = t_map.get_mut(k) {
                    merge_json_patch(existing, v);
                } else {
                    t_map.insert(k.clone(), v.clone());
                }
            }
        }
        (target_slot, new_val) => {
            *target_slot = new_val.clone();
        }
    }
}

/// Apply a partial JSON merge patch onto `base` `PrintConfig` and validate it.
pub fn apply_config_patch_value(
    base: &PrintConfig,
    patch: &Value,
) -> Result<PrintConfig, EngineError> {
    if patch.is_null() {
        return Ok(base.clone());
    }
    if !patch.is_object() {
        return Err(EngineError::InvalidArgument {
            field: "patch",
            reason: "must be a JSON object",
        });
    }
    let mut val = serde_json::to_value(base)?;
    merge_json_patch(&mut val, patch);
    let cfg: PrintConfig = serde_json::from_value(val)?;
    cfg.validate()?;
    Ok(cfg)
}

/// Non-destructively slice the plate loaded in `session` under an explicit `config`.
pub fn slice_with_config(
    session: &Session,
    config: &PrintConfig,
) -> Result<SlicedPlate, EngineError> {
    let mod_vols: Vec<ModifierVolume> = session
        .modifiers
        .iter()
        .map(PlateModifierEntry::to_volume)
        .collect();
    if let Some(prepared) = session.prepared_mesh.as_ref() {
        Ok(slice_prepared_mesh_with_modifiers(
            prepared, config, &mod_vols,
        )?)
    } else if let Some(raw) = session.mesh.as_ref() {
        Ok(slice_single_mesh_with_modifiers(raw, config, &mod_vols)?)
    } else {
        Err(EngineError::NoMeshLoaded)
    }
}

fn undo_label_for_command(name: &str) -> Option<&'static str> {
    match name {
        "plate.clear" => Some("Clear plate"),
        "plate.add_stl_bytes"
        | "plate.add_3mf_bytes"
        | "plate.add_sample_shape"
        | "plate.add_mesh_file" => Some("Add object"),
        "plate.remove_object" => Some("Remove object"),
        "plate.transform_object" => Some("Transform object"),
        "plate.auto_arrange" => Some("Auto-arrange plate"),
        "plate.repair_mesh" => Some("Repair mesh"),
        "plate.add_modifier_primitive"
        | "plate.add_modifier_stl_bytes"
        | "plate.add_modifier_3mf_bytes"
        | "plate.add_modifier_mesh_file" => Some("Add modifier"),
        "plate.update_modifier" => Some("Update modifier"),
        "plate.remove_modifier" => Some("Remove modifier"),
        "config.set" | "config.patch" => Some("Update settings"),
        "profile.apply" => Some("Apply profiles"),
        "project.load_3mf_bytes" | "project.load_file" => Some("Load project"),
        "plates.add" => Some("Add plate"),
        "plates.rename" => Some("Rename plate"),
        "plates.remove" => Some("Remove plate"),
        _ => None,
    }
}

fn resolve_plate_index(
    session: &Session,
    index: Option<usize>,
    id: Option<u32>,
    name: Option<&str>,
) -> Result<usize, EngineError> {
    if let Some(i) = index {
        if i < session.plate_count() {
            return Ok(i);
        }
        return Err(EngineError::PlateNotFound(format!("index {i}")));
    }
    if let Some(pid) = id {
        if let Some(pos) = session.plate_slots.iter().position(|p| p.id == pid) {
            return Ok(pos);
        }
        return Err(EngineError::PlateNotFound(format!("id {pid}")));
    }
    if let Some(raw_name) = name {
        let trimmed = raw_name.trim();
        if trimmed.is_empty() {
            return Err(EngineError::InvalidArgument {
                field: "name",
                reason: "must not be empty",
            });
        }
        if let Some(pos) = session
            .plate_slots
            .iter()
            .position(|p| p.name.trim() == trimmed)
        {
            return Ok(pos);
        }
        if let Some(pos) = session
            .plate_slots
            .iter()
            .position(|p| p.name.trim().eq_ignore_ascii_case(trimmed))
        {
            return Ok(pos);
        }
        return Err(EngineError::PlateNotFound(trimmed.to_owned()));
    }
    Err(EngineError::InvalidArgument {
        field: "plate",
        reason: "must specify `index`, `id`, or `name`",
    })
}

/// Execute a sequence of engine commands, optionally rolling back `session` on failure
/// and coalescing any mutating commands into a single undo snapshot.
pub fn execute_batch(session: &mut Session, steps: &[BatchStep], atomic: bool) -> BatchResult {
    let rollback_backup = if atomic { Some(session.clone()) } else { None };
    let mut mutating_labels = steps
        .iter()
        .filter_map(|s| undo_label_for_command(&s.command));
    let first_label = mutating_labels.next();
    let batch_label = match (first_label, mutating_labels.next()) {
        (Some(single), None) => single,
        (Some(_), Some(_)) => "Batch edit",
        (None, _) => "Batch",
    };
    let pre_batch = if first_label.is_some() && !session.suppress_undo_capture {
        Some(session.capture_snapshot(batch_label))
    } else {
        None
    };

    let prev_suppress = session.suppress_undo_capture;
    session.suppress_undo_capture = true;
    let mut results = Vec::with_capacity(steps.len());
    let mut any_mutated = false;

    for (idx, step) in steps.iter().enumerate() {
        match execute_inner(session, &step.command, step.params.clone()) {
            Ok(result) => {
                if undo_label_for_command(&step.command).is_some() {
                    any_mutated = true;
                }
                results.push(BatchStepResult {
                    index: idx,
                    command: step.command.clone(),
                    result,
                });
            }
            Err(e) => {
                let mut rolled_back = false;
                if let Some(prev) = rollback_backup {
                    *session = prev;
                    rolled_back = true;
                } else {
                    session.suppress_undo_capture = prev_suppress;
                }
                return BatchResult {
                    ok: false,
                    completed_steps: results.len(),
                    rolled_back,
                    results,
                    error: Some(format!("step {idx} (`{}`): {e}", step.command)),
                };
            }
        }
    }

    session.suppress_undo_capture = prev_suppress;
    if any_mutated && let Some(snap) = pre_batch {
        session.push_undo_entry(snap);
    }

    BatchResult {
        ok: true,
        completed_steps: results.len(),
        rolled_back: false,
        results,
        error: None,
    }
}

fn delta_percent(a: f64, b: f64) -> f64 {
    if a.abs() > 1e-9 {
        ((b - a) / a) * 100.0
    } else {
        0.0
    }
}

fn build_side_metrics(
    session: &Session,
    cfg: &PrintConfig,
) -> Result<
    (
        SliceSideMetrics,
        Vec<layercraft_gcode::GcodeExtrusionSegment>,
    ),
    EngineError,
> {
    let plate = slice_with_config(session, cfg)?;
    let out = emit_gcode(&plate, cfg)?;
    let (report, segs) = evaluate_gcode(
        &out.gcode,
        cfg.filament.diameter_mm,
        cfg.machine.bed_width_mm,
        cfg.machine.bed_depth_mm,
    )?;
    let metrics = SliceSideMetrics {
        layer_count: out.stats.layer_count,
        filament_volume_mm3: out.stats.filament_volume_mm3,
        filament_mass_g: out.stats.filament_weight_g,
        estimated_time_s: out.stats.estimated_time_s,
        travel_distance_mm: out.stats.travel_distance_mm,
        arc_moves_count: report.arc_moves_count,
        safety_clean: report.safety.is_clean(),
        stats: out.stats,
        report,
    };
    Ok((metrics, segs))
}

/// Execute a named command with JSON parameters against `session` (capturing undo history for mutations).
pub fn execute(session: &mut Session, name: &str, params: Value) -> Result<Value, EngineError> {
    let pre_snap = if !session.suppress_undo_capture
        && let Some(label) = undo_label_for_command(name)
    {
        Some(session.capture_snapshot(label))
    } else {
        None
    };

    let out = execute_inner(session, name, params)?;

    if let Some(snap) = pre_snap {
        let is_noop =
            matches!(name, "config.set" | "config.patch") && session.config == snap.config;
        let is_noop_profile = name == "profile.apply"
            && session.config == snap.config
            && session.active_profiles == snap.active_profiles;
        let is_noop_repair = name == "plate.repair_mesh"
            && session.objects.len() == snap.objects.len()
            && session
                .objects
                .iter()
                .zip(&snap.objects)
                .all(|(a, b)| Arc::ptr_eq(&a.raw_mesh, &b.raw_mesh));
        if !is_noop && !is_noop_profile && !is_noop_repair {
            session.push_undo_entry(snap);
        }
    }
    Ok(out)
}

fn execute_inner(session: &mut Session, name: &str, params: Value) -> Result<Value, EngineError> {
    match name {
        "plate.clear" => {
            let _: EmptyParams = if params.is_null() {
                EmptyParams {}
            } else {
                serde_json::from_value(params)?
            };
            session.objects.clear();
            session.modifiers.clear();
            let _ = rebuild_plate_mesh(session)?;
            Ok(serde_json::to_value(OkResult { ok: true })?)
        }
        "plate.add_stl_bytes" => {
            let p: AddStlBytesParams = serde_json::from_value(params)?;
            let mesh = read_stl_bytes(&p.bytes)?;
            let label = p.name.unwrap_or_else(|| "STL Part".to_owned());
            let insp = add_plate_object(session, label, mesh, p.append)?;
            Ok(serde_json::to_value(insp)?)
        }
        "plate.add_3mf_bytes" => {
            let p: Add3mfBytesParams = serde_json::from_value(params)?;
            let doc = read_3mf_document_bytes(&p.bytes)?;
            let insp = add_3mf_document_objects(session, doc, p.name, p.append)?;
            Ok(serde_json::to_value(insp)?)
        }
        "plate.add_sample_shape" => {
            let p: AddSampleShapeParams = serde_json::from_value(params)?;
            let mesh = build_sample_shape(p.shape)?;
            let label = p.name.unwrap_or_else(|| p.shape.label().to_owned());
            let insp = add_plate_object(session, label, mesh, p.append)?;
            Ok(serde_json::to_value(insp)?)
        }
        #[cfg(not(target_arch = "wasm32"))]
        "plate.add_mesh_file" => {
            let p: AddMeshFileParams = serde_json::from_value(params)?;
            let path = std::path::Path::new(&p.path);
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            if ext == "3mf" {
                let doc = layercraft_io::read_3mf_document_file(path)?;
                let insp = add_3mf_document_objects(session, doc, p.name, p.append)?;
                Ok(serde_json::to_value(insp)?)
            } else {
                let mesh = layercraft_io::read_mesh_file(path)?;
                let label = p.name.unwrap_or_else(|| {
                    path.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("Mesh Part")
                        .to_owned()
                });
                let insp = add_plate_object(session, label, mesh, p.append)?;
                Ok(serde_json::to_value(insp)?)
            }
        }
        "plate.inspect" => {
            if let Some(cached) = session.plate_inspection.as_ref() {
                return Ok(serde_json::to_value(cached)?);
            }
            let mesh = session
                .prepared_mesh
                .as_deref()
                .or(session.mesh.as_deref())
                .ok_or(EngineError::NoMeshLoaded)?;
            let insp = inspect_mesh(mesh);
            let val = serde_json::to_value(&insp)?;
            session.plate_inspection = Some(insp);
            Ok(val)
        }
        "plate.repair_mesh" => {
            let p: RepairMeshParams = if params.is_null() {
                RepairMeshParams::default()
            } else {
                serde_json::from_value(params)?
            };
            let result = repair_plate_objects(session, p.object_id, p.config.unwrap_or_default())?;
            Ok(serde_json::to_value(result)?)
        }
        "plate.check_layout" => {
            let _: EmptyParams = if params.is_null() {
                EmptyParams {}
            } else {
                serde_json::from_value(params)?
            };
            session.recompute_plate_layout();
            Ok(serde_json::to_value(&session.plate_layout)?)
        }
        "plate.list_objects" => {
            let _: EmptyParams = if params.is_null() {
                EmptyParams {}
            } else {
                serde_json::from_value(params)?
            };
            Ok(serde_json::to_value(object_infos(session))?)
        }
        "plate.remove_object" => {
            let p: RemoveObjectParams = serde_json::from_value(params)?;
            let before = session.objects.len();
            session.objects.retain(|o| o.id != p.id);
            if session.objects.len() == before {
                return Err(EngineError::ObjectNotFound(p.id));
            }
            let _ = rebuild_plate_mesh(session)?;
            Ok(serde_json::to_value(OkResult { ok: true })?)
        }
        "plate.transform_object" => {
            let p: TransformObjectParams = serde_json::from_value(params)?;
            let entry = session
                .objects
                .iter_mut()
                .find(|o| o.id == p.id)
                .ok_or(EngineError::ObjectNotFound(p.id))?;
            let next_transform = ObjectTransform {
                position_xy_mm: p.position_xy_mm.unwrap_or(entry.transform.position_xy_mm),
                rotation_deg: p.rotation_deg.unwrap_or(entry.transform.rotation_deg),
                scale: p.scale.unwrap_or(entry.transform.scale),
            };
            let next_prep = prepare_transformed_object(&entry.raw_mesh, next_transform)?;
            entry.transform = next_transform;
            entry.prepared_mesh = Arc::new(next_prep);
            let info = entry.info();
            let _ = rebuild_plate_mesh(session)?;
            Ok(serde_json::to_value(info)?)
        }
        "plate.auto_arrange" => {
            let p: AutoArrangeParams = if params.is_null() {
                AutoArrangeParams::default()
            } else {
                serde_json::from_value(params)?
            };
            let spacing = p.spacing_mm.unwrap_or(6.0);
            auto_arrange_session_objects(session, spacing)?;
            let _ = rebuild_plate_mesh(session)?;
            Ok(serde_json::to_value(object_infos(session))?)
        }
        "plate.add_modifier_primitive" => {
            let p: AddModifierPrimitiveParams = serde_json::from_value(params)?;
            let raw = build_modifier_primitive(p.primitive)?;
            let default_name = match p.primitive {
                ModifierPrimitive::Box { .. } => "Box Modifier",
                ModifierPrimitive::Cylinder { .. } => "Cylinder Modifier",
            };
            let label = p.name.unwrap_or_else(|| default_name.to_owned());
            let info = add_plate_modifier(
                session,
                label,
                p.role,
                Some(p.primitive),
                raw,
                p.transform,
                p.overrides,
            )?;
            Ok(serde_json::to_value(info)?)
        }
        "plate.add_modifier_stl_bytes" => {
            let p: AddModifierBytesParams = serde_json::from_value(params)?;
            let raw = read_stl_bytes(&p.bytes)?;
            let label = p.name.unwrap_or_else(|| "STL Modifier".to_owned());
            let info =
                add_plate_modifier(session, label, p.role, None, raw, p.transform, p.overrides)?;
            Ok(serde_json::to_value(info)?)
        }
        "plate.add_modifier_3mf_bytes" => {
            let p: AddModifierBytesParams = serde_json::from_value(params)?;
            let raw = layercraft_io::read_3mf_bytes(&p.bytes)?;
            let label = p.name.unwrap_or_else(|| "3MF Modifier".to_owned());
            let info =
                add_plate_modifier(session, label, p.role, None, raw, p.transform, p.overrides)?;
            Ok(serde_json::to_value(info)?)
        }
        #[cfg(not(target_arch = "wasm32"))]
        "plate.add_modifier_mesh_file" => {
            let p: AddModifierMeshFileParams = serde_json::from_value(params)?;
            let path = std::path::Path::new(&p.path);
            let raw = layercraft_io::read_mesh_file(path)?;
            let label = p.name.unwrap_or_else(|| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Mesh Modifier")
                    .to_owned()
            });
            let info =
                add_plate_modifier(session, label, p.role, None, raw, p.transform, p.overrides)?;
            Ok(serde_json::to_value(info)?)
        }
        "plate.list_modifiers" => {
            let _: EmptyParams = if params.is_null() {
                EmptyParams {}
            } else {
                serde_json::from_value(params)?
            };
            Ok(serde_json::to_value(modifier_infos(session))?)
        }
        "plate.update_modifier" => {
            let p: UpdateModifierParams = serde_json::from_value(params)?;
            let entry = session
                .modifiers
                .iter_mut()
                .find(|m| m.id == p.id)
                .ok_or(EngineError::ModifierNotFound(p.id))?;
            if let Some(ov) = &p.overrides {
                ov.validate()?;
            }
            let next_xf = ModifierTransform {
                position_xyz_mm: p.position_xyz_mm.unwrap_or(entry.transform.position_xyz_mm),
                rotation_deg: p.rotation_deg.unwrap_or(entry.transform.rotation_deg),
                scale: p.scale.unwrap_or(entry.transform.scale),
            };
            let next_prep = prepare_modifier_mesh(&entry.raw_mesh, next_xf)?;
            if let Some(name) = p.name {
                entry.name = name;
            }
            if let Some(role) = p.role {
                entry.role = role;
            }
            if let Some(ov) = p.overrides {
                entry.overrides = ov;
            }
            entry.transform = next_xf;
            entry.prepared_mesh = Arc::new(next_prep);
            let info = entry.info();
            session.sliced = None;
            session.last_gcode = None;
            Ok(serde_json::to_value(info)?)
        }
        "plate.remove_modifier" => {
            let p: RemoveModifierParams = serde_json::from_value(params)?;
            let before = session.modifiers.len();
            session.modifiers.retain(|m| m.id != p.id);
            if session.modifiers.len() == before {
                return Err(EngineError::ModifierNotFound(p.id));
            }
            session.sliced = None;
            session.last_gcode = None;
            Ok(serde_json::to_value(OkResult { ok: true })?)
        }
        "config.get" => Ok(serde_json::to_value(&session.config)?),
        "config.set" => {
            let p: SetConfigParams = serde_json::from_value(params)?;
            apply_session_config(session, p.config)?;
            Ok(serde_json::to_value(&session.config)?)
        }
        "config.patch" => {
            let patch_val = match &params {
                Value::Null => Value::Object(serde_json::Map::new()),
                Value::Object(map) => {
                    if let Some(inner) = map.get("patch") {
                        inner.clone()
                    } else {
                        params
                    }
                }
                _ => {
                    return Err(EngineError::InvalidArgument {
                        field: "patch",
                        reason: "must be a JSON object",
                    });
                }
            };
            let new_cfg = apply_config_patch_value(&session.config, &patch_val)?;
            apply_session_config(session, new_cfg)?;
            Ok(serde_json::to_value(&session.config)?)
        }
        "profile.list" => {
            let p: ListProfilesParams = if params.is_null() {
                ListProfilesParams::default()
            } else {
                serde_json::from_value(params)?
            };
            let list = session.profiles.list(p.kind, p.include_abstract);
            Ok(serde_json::to_value(list)?)
        }
        "profile.load_json" => {
            let p: LoadProfileJsonParams = serde_json::from_value(params)?;
            let loaded = session.profiles.load_json_str(&p.json)?;
            Ok(serde_json::to_value(loaded)?)
        }
        #[cfg(not(target_arch = "wasm32"))]
        "profile.load_file" => {
            let p: LoadProfileFileParams = serde_json::from_value(params)?;
            let loaded = session.profiles.load_file(std::path::Path::new(&p.path))?;
            Ok(serde_json::to_value(loaded)?)
        }
        #[cfg(not(target_arch = "wasm32"))]
        "profile.load_dir" => {
            let p: LoadProfileDirParams = serde_json::from_value(params)?;
            let report = session.profiles.load_dir(std::path::Path::new(&p.path))?;
            Ok(serde_json::to_value(report)?)
        }
        "profile.resolve" => {
            let p: ResolveProfileParams = serde_json::from_value(params)?;
            let resolved = session.profiles.resolve(p.kind, &p.name)?;
            Ok(serde_json::to_value(resolved)?)
        }
        "profile.apply" => {
            let p: ApplyProfilesParams = serde_json::from_value(params)?;
            let baseline = if p.reset_to_defaults {
                PrintConfig::default()
            } else {
                session.config.clone()
            };
            let apply_res = session.profiles.build_print_config(
                &baseline,
                p.machine.as_deref(),
                p.filament.as_deref(),
                p.process.as_deref(),
            )?;
            apply_session_config(session, apply_res.config.clone())?;
            if p.reset_to_defaults {
                session.active_profiles = ActiveProfiles::default();
            }
            if let Some(m) = p
                .machine
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
            {
                session.active_profiles.machine = Some(m);
            }
            if let Some(f) = p
                .filament
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
            {
                session.active_profiles.filament = Some(f);
            }
            if let Some(pr) = p
                .process
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
            {
                session.active_profiles.process = Some(pr);
            }
            Ok(serde_json::to_value(apply_res)?)
        }
        "profile.save_user" => {
            let p: SaveUserProfileParams = serde_json::from_value(params)?;
            let doc = profile_document_from_config(p.kind, &p.name, &session.config)?;
            let summary = session.profiles.upsert_user_profile(doc.clone())?;
            if p.activate {
                match p.kind {
                    ProfileKind::Machine => {
                        session.active_profiles.machine = Some(summary.name.clone());
                    }
                    ProfileKind::Filament => {
                        session.active_profiles.filament = Some(summary.name.clone());
                    }
                    ProfileKind::Process => {
                        session.active_profiles.process = Some(summary.name.clone());
                    }
                    ProfileKind::MachineModel => {}
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            let (persisted, path) = if p.persist {
                if let Some(dir) = default_user_profiles_dir() {
                    let saved = save_user_profile_to_dir(&dir, &doc)?;
                    (true, Some(saved.display().to_string()))
                } else {
                    (false, None)
                }
            } else {
                (false, None)
            };
            #[cfg(target_arch = "wasm32")]
            let (persisted, path) = (false, None);
            Ok(serde_json::to_value(SaveUserProfileResult {
                summary,
                persisted,
                path,
            })?)
        }
        "profile.delete_user" => {
            let p: DeleteUserProfileParams = serde_json::from_value(params)?;
            let canonical_name = session
                .profiles
                .get(p.kind, &p.name)
                .map(|d| d.name.clone());
            session.profiles.remove_user_profile(p.kind, &p.name)?;
            let target_trimmed = p.name.trim();
            let matches_active = |slot: &Option<String>| {
                slot.as_deref()
                    .is_some_and(|s| s == target_trimmed || canonical_name.as_deref() == Some(s))
            };
            match p.kind {
                ProfileKind::Machine if matches_active(&session.active_profiles.machine) => {
                    session.active_profiles.machine = None;
                }
                ProfileKind::Filament if matches_active(&session.active_profiles.filament) => {
                    session.active_profiles.filament = None;
                }
                ProfileKind::Process if matches_active(&session.active_profiles.process) => {
                    session.active_profiles.process = None;
                }
                _ => {}
            }
            #[cfg(not(target_arch = "wasm32"))]
            let deleted_from_disk = if p.persist {
                if let Some(dir) = default_user_profiles_dir() {
                    let match_name = canonical_name.as_deref().unwrap_or(target_trimmed);
                    delete_user_profile_from_dir(&dir, p.kind, match_name)?
                } else {
                    false
                }
            } else {
                false
            };
            #[cfg(target_arch = "wasm32")]
            let deleted_from_disk = false;
            Ok(serde_json::to_value(DeleteUserProfileResult {
                deleted: true,
                deleted_from_disk,
            })?)
        }
        "profile.export_json" => {
            let p: ExportProfileJsonParams = serde_json::from_value(params)?;
            let canonical_name = session
                .profiles
                .get(p.kind, &p.name)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| p.name.trim().to_owned());
            let json = session
                .profiles
                .export_document_json(p.kind, &p.name, p.pretty)?;
            Ok(serde_json::to_value(ExportProfileJsonResult {
                kind: p.kind,
                name: canonical_name,
                json,
            })?)
        }
        "slice.run" => {
            let plate = slice_with_config(session, &session.config)?;
            let total_paths = plate.layers.iter().map(|l| l.paths.len()).sum();
            let res = SliceSummaryResult {
                layer_count: plate.layers.len(),
                total_paths,
            };
            session.sliced = Some(plate);
            session.last_gcode = None;
            Ok(serde_json::to_value(res)?)
        }
        "plate.analyze_printability" => {
            let p: AnalyzePrintabilityParams = if params.is_null() {
                AnalyzePrintabilityParams::default()
            } else {
                serde_json::from_value(params)?
            };
            if session.prepared_mesh.is_none() && session.mesh.is_none() {
                return Err(EngineError::NoMeshLoaded);
            }
            if session.sliced.is_none() && p.auto_slice {
                let plate = slice_with_config(session, &session.config)?;
                session.sliced = Some(plate);
                session.last_gcode = None;
            }
            let mesh = session
                .prepared_mesh
                .as_deref()
                .or(session.mesh.as_deref())
                .ok_or(EngineError::NoMeshLoaded)?;
            let report = analyze_printability(mesh, session.sliced.as_ref(), &session.config);
            Ok(serde_json::to_value(report)?)
        }
        "slice.compare" => {
            let p: CompareSlicesParams = serde_json::from_value(params)?;
            let pixel_mm = p.pixel_mm.unwrap_or(0.5);
            if !pixel_mm.is_finite() || pixel_mm <= 0.0 || pixel_mm > 10.0 {
                return Err(EngineError::InvalidArgument {
                    field: "pixel_mm",
                    reason: "must be finite and within (0.0, 10.0]",
                });
            }
            let cfg_a = match &p.patch_a {
                Some(pa) => apply_config_patch_value(&session.config, pa)?,
                None => session.config.clone(),
            };
            let cfg_b = apply_config_patch_value(&session.config, &p.patch_b)?;
            let (a, segs_a) = build_side_metrics(session, &cfg_a)?;
            let (b, segs_b) = build_side_metrics(session, &cfg_b)?;
            let bead_r = 0.5 * cfg_a.machine.nozzle_diameter_mm.max(0.2);
            let raster = compare_layer_iou(&segs_a, &segs_b, pixel_mm, bead_r);
            let delta = SliceDeltaMetrics {
                layer_count_delta: (b.layer_count as i64) - (a.layer_count as i64),
                filament_volume_delta_mm3: b.filament_volume_mm3 - a.filament_volume_mm3,
                filament_volume_delta_percent: delta_percent(
                    a.filament_volume_mm3,
                    b.filament_volume_mm3,
                ),
                filament_mass_delta_g: b.filament_mass_g - a.filament_mass_g,
                estimated_time_delta_s: b.estimated_time_s - a.estimated_time_s,
                estimated_time_delta_percent: delta_percent(a.estimated_time_s, b.estimated_time_s),
                travel_distance_delta_mm: b.travel_distance_mm - a.travel_distance_mm,
                arc_moves_delta: (b.arc_moves_count as i64) - (a.arc_moves_count as i64),
            };
            Ok(serde_json::to_value(SliceComparisonReport {
                a,
                b,
                delta,
                raster,
            })?)
        }
        "layer.inspect" => {
            let p: InspectLayerParams = serde_json::from_value(params)?;
            let plate = session.sliced.as_ref().ok_or(EngineError::NotSliced)?;
            let layer = plate
                .layers
                .get(p.index)
                .ok_or(EngineError::LayerOutOfRange {
                    index: p.index,
                    count: plate.layers.len(),
                })?;
            let mut role_lengths_mm: BTreeMap<String, f64> = BTreeMap::new();
            for path in &layer.paths {
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
                *role_lengths_mm
                    .entry(path.role.label().to_owned())
                    .or_insert(0.0) += len;
            }
            let info = LayerInspection {
                index: layer.index,
                z_top_mm: layer.z_top_mm,
                height_mm: layer.height_mm,
                fan_percent: layer.fan_percent,
                estimated_time_s: layer.estimated_time_s,
                region_count: layer.slice_regions.len(),
                path_count: layer.paths.len(),
                role_lengths_mm,
            };
            Ok(serde_json::to_value(info)?)
        }
        "gcode.export" => {
            let plate = session.sliced.as_ref().ok_or(EngineError::NotSliced)?;
            let out = emit_gcode(plate, &session.config)?;
            let val = serde_json::to_value(&out)?;
            session.last_gcode = Some(out);
            Ok(val)
        }
        "gcode.export_bgcode" => {
            let p: ExportBgcodeParams = if params.is_null() {
                ExportBgcodeParams::default()
            } else {
                serde_json::from_value(params)?
            };
            if session.last_gcode.is_none() {
                let plate = session.sliced.as_ref().ok_or(EngineError::NotSliced)?;
                let out = emit_gcode(plate, &session.config)?;
                session.last_gcode = Some(out);
            }
            let gcode_out = session.last_gcode.as_ref().ok_or(EngineError::NotSliced)?;
            let compression = p.compression.unwrap_or_default();
            let opts = BgcodeOptions {
                compression,
                ..BgcodeOptions::default()
            };
            let bgcode_bytes = encode_bgcode(gcode_out, &session.config, &opts)?;
            let ascii_len = gcode_out.gcode.len();
            let ratio = if ascii_len > 0 {
                (bgcode_bytes.len() as f64) / (ascii_len as f64)
            } else {
                1.0
            };
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bgcode_bytes);
            Ok(serde_json::to_value(ExportBgcodeResult {
                bytes_base64: b64,
                byte_len: bgcode_bytes.len(),
                ascii_byte_len: ascii_len,
                compression_ratio: ratio,
                compression,
                stats: gcode_out.stats.clone(),
            })?)
        }
        "gcode.evaluate" => {
            let p: EvaluateGcodeParams = serde_json::from_value(params)?;
            let d = p
                .filament_diameter_mm
                .unwrap_or(session.config.filament.diameter_mm);
            let w = p
                .bed_width_mm
                .unwrap_or(session.config.machine.bed_width_mm);
            let depth = p
                .bed_depth_mm
                .unwrap_or(session.config.machine.bed_depth_mm);
            let (report, _) = evaluate_gcode(&p.gcode, d, w, depth)?;
            Ok(serde_json::to_value(report)?)
        }
        #[cfg(not(target_arch = "wasm32"))]
        "gcode.save_file" => {
            let p: SaveGcodeFileParams = serde_json::from_value(params)?;
            let trimmed_path = p.path.trim();
            if trimmed_path.is_empty() {
                return Err(EngineError::InvalidArgument {
                    field: "path",
                    reason: "must not be empty",
                });
            }
            if session.last_gcode.is_none() {
                let plate = session.sliced.as_ref().ok_or(EngineError::NotSliced)?;
                let out = emit_gcode(plate, &session.config)?;
                session.last_gcode = Some(out);
            }
            let gcode_out = session.last_gcode.as_ref().ok_or(EngineError::NotSliced)?;
            let use_bgcode = match p.format {
                GcodeExportFormat::Bgcode => true,
                GcodeExportFormat::Gcode => false,
                GcodeExportFormat::Auto => trimmed_path.to_ascii_lowercase().ends_with(".bgcode"),
            };

            let (raw_bytes, format_str): (Vec<u8>, String) = if use_bgcode {
                let opts = BgcodeOptions {
                    compression: p.compression.unwrap_or_default(),
                    ..BgcodeOptions::default()
                };
                (
                    encode_bgcode(gcode_out, &session.config, &opts)?,
                    "bgcode".to_owned(),
                )
            } else {
                (gcode_out.gcode.as_bytes().to_vec(), "gcode".to_owned())
            };

            let out_path = std::path::Path::new(trimmed_path);
            if let Some(parent) = out_path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|e| EngineError::FileWrite(format!("{}: {e}", parent.display())))?;
            }
            std::fs::write(out_path, &raw_bytes)
                .map_err(|e| EngineError::FileWrite(format!("{}: {e}", out_path.display())))?;
            let evaluation = if p.evaluate {
                let eval_text = if use_bgcode {
                    decode_bgcode(&raw_bytes)?.gcode
                } else {
                    gcode_out.gcode.clone()
                };
                let (rep, _) = evaluate_gcode(
                    &eval_text,
                    session.config.filament.diameter_mm,
                    session.config.machine.bed_width_mm,
                    session.config.machine.bed_depth_mm,
                )?;
                Some(rep)
            } else {
                None
            };
            Ok(serde_json::to_value(SaveGcodeFileResult {
                path: trimmed_path.to_owned(),
                format: format_str,
                bytes_written: raw_bytes.len(),
                stats: gcode_out.stats.clone(),
                evaluation,
            })?)
        }
        "printer.list" => {
            let _: EmptyParams = if params.is_null() {
                EmptyParams {}
            } else {
                serde_json::from_value(params)?
            };
            let summaries: Vec<PrinterProfileSummary> = session
                .printers
                .iter()
                .map(PrinterProfile::summary)
                .collect();
            Ok(serde_json::to_value(summaries)?)
        }
        "printer.upsert" => {
            let mut p: UpsertPrinterParams = serde_json::from_value(params)?;
            let trimmed_id = p.profile.id.trim().to_owned();
            if trimmed_id.is_empty() {
                return Err(EngineError::InvalidArgument {
                    field: "profile.id",
                    reason: "must not be empty",
                });
            }
            p.profile.id = trimmed_id.clone();
            p.profile.base_url = normalize_base_url(&p.profile.base_url)?;
            let summary = p.profile.summary();
            if let Some(existing) = session.printers.iter_mut().find(|x| x.id == trimmed_id) {
                *existing = p.profile;
            } else {
                session.printers.push(p.profile);
            }
            #[cfg(not(target_arch = "wasm32"))]
            if p.persist {
                session.save_printers_to_disk()?;
            }
            #[cfg(target_arch = "wasm32")]
            let _ = p.persist;
            Ok(serde_json::to_value(summary)?)
        }
        "printer.remove" => {
            let p: RemovePrinterParams = serde_json::from_value(params)?;
            let target = p.id.trim();
            let before = session.printers.len();
            session.printers.retain(|pr| pr.id != target);
            if session.printers.len() == before {
                return Err(EngineError::PrinterNotFound(target.to_owned()));
            }
            #[cfg(not(target_arch = "wasm32"))]
            if p.persist {
                session.save_printers_to_disk()?;
            }
            #[cfg(target_arch = "wasm32")]
            let _ = p.persist;
            Ok(serde_json::to_value(OkResult { ok: true })?)
        }
        #[cfg(not(target_arch = "wasm32"))]
        "printer.status" => {
            let p: PrinterStatusParams = if params.is_null() {
                PrinterStatusParams::default()
            } else {
                serde_json::from_value(params)?
            };
            let prof = session.resolve_printer_profile(p.printer_id.as_deref(), p.profile)?;
            let report = probe_printer(&prof)?;
            Ok(serde_json::to_value(report)?)
        }
        #[cfg(not(target_arch = "wasm32"))]
        "printer.upload" => {
            let p: PrinterUploadParams = serde_json::from_value(params)?;
            let prof = session.resolve_printer_profile(p.printer_id.as_deref(), p.profile)?;
            if session.sliced.is_none() {
                if p.auto_slice {
                    let plate = slice_with_config(session, &session.config)?;
                    session.sliced = Some(plate);
                    session.last_gcode = None;
                } else {
                    return Err(EngineError::NotSliced);
                }
            }
            if session.last_gcode.is_none() {
                let plate = session.sliced.as_ref().ok_or(EngineError::NotSliced)?;
                let out = emit_gcode(plate, &session.config)?;
                session.last_gcode = Some(out);
            }
            let gcode_out = session.last_gcode.as_ref().ok_or(EngineError::NotSliced)?;

            let fname_trim = p
                .filename
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let use_bgcode = match p.format {
                GcodeExportFormat::Bgcode => true,
                GcodeExportFormat::Gcode => false,
                GcodeExportFormat::Auto => {
                    if let Some(name) = fname_trim {
                        let lower = name.to_ascii_lowercase();
                        if lower.ends_with(".bgcode") {
                            true
                        } else if lower.ends_with(".gcode") {
                            false
                        } else {
                            (prof.prefer_bgcode || session.config.machine.binary_gcode)
                                && prof.protocol.supports_bgcode()
                        }
                    } else {
                        (prof.prefer_bgcode || session.config.machine.binary_gcode)
                            && prof.protocol.supports_bgcode()
                    }
                }
            };

            let remote_path = fname_trim
                .map(str::to_owned)
                .unwrap_or_else(|| session.default_export_filename(use_bgcode));
            let (payload, format_str): (Vec<u8>, String) = if use_bgcode {
                let opts = BgcodeOptions {
                    compression: p.compression.unwrap_or_default(),
                    ..BgcodeOptions::default()
                };
                (
                    encode_bgcode(gcode_out, &session.config, &opts)?,
                    "bgcode".to_owned(),
                )
            } else {
                (gcode_out.gcode.as_bytes().to_vec(), "gcode".to_owned())
            };

            let evaluation = if p.evaluate || p.require_clean_safety {
                let eval_text = if use_bgcode {
                    decode_bgcode(&payload)?.gcode
                } else {
                    gcode_out.gcode.clone()
                };
                let (rep, _) = evaluate_gcode(
                    &eval_text,
                    session.config.filament.diameter_mm,
                    session.config.machine.bed_width_mm,
                    session.config.machine.bed_depth_mm,
                )?;
                if p.require_clean_safety && !rep.safety.is_clean() {
                    return Err(EngineError::SafetyCheckFailed(format!(
                        "non_finite={}, out_of_bounds={}, z_regressions={}",
                        rep.safety.non_finite_values,
                        rep.safety.out_of_bounds_moves,
                        rep.safety.z_regressions
                    )));
                }
                p.evaluate.then_some(rep)
            } else {
                None
            };

            let upload = upload_to_printer(
                &prof,
                &payload,
                &UploadOptions {
                    remote_path,
                    storage: p.storage,
                    start_print: p.start_print,
                    overwrite: p.overwrite,
                },
            )?;
            Ok(serde_json::to_value(PrinterUploadResult {
                format: format_str,
                upload,
                stats: gcode_out.stats.clone(),
                evaluation,
            })?)
        }
        "session.undo" => {
            let _: EmptyParams = if params.is_null() {
                EmptyParams {}
            } else {
                serde_json::from_value(params)?
            };
            let res = session.undo();
            Ok(serde_json::to_value(res)?)
        }
        "session.redo" => {
            let _: EmptyParams = if params.is_null() {
                EmptyParams {}
            } else {
                serde_json::from_value(params)?
            };
            let res = session.redo();
            Ok(serde_json::to_value(res)?)
        }
        "session.history" => {
            let _: EmptyParams = if params.is_null() {
                EmptyParams {}
            } else {
                serde_json::from_value(params)?
            };
            Ok(serde_json::to_value(session.history_status())?)
        }
        "project.export_3mf_bytes" => {
            let _: EmptyParams = if params.is_null() {
                EmptyParams {}
            } else {
                serde_json::from_value(params)?
            };
            let bytes = export_project_3mf_bytes(session)?;
            Ok(serde_json::to_value(ProjectExportBytesResult {
                bytes,
                object_count: session.objects.len(),
                modifier_count: session.modifiers.len(),
            })?)
        }
        "project.load_3mf_bytes" => {
            let p: LoadProject3mfBytesParams = serde_json::from_value(params)?;
            let res = load_project_3mf_bytes(session, &p.bytes, p.name.as_deref())?;
            Ok(serde_json::to_value(res)?)
        }
        #[cfg(not(target_arch = "wasm32"))]
        "project.save_file" => {
            let p: SaveProjectFileParams = serde_json::from_value(params)?;
            let trimmed = p.path.trim();
            if trimmed.is_empty() {
                return Err(EngineError::InvalidArgument {
                    field: "path",
                    reason: "must not be empty",
                });
            }
            let bytes = export_project_3mf_bytes(session)?;
            let out_path = std::path::Path::new(trimmed);
            if let Some(parent) = out_path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|e| EngineError::FileWrite(format!("{}: {e}", parent.display())))?;
            }
            std::fs::write(out_path, &bytes)
                .map_err(|e| EngineError::FileWrite(format!("{}: {e}", out_path.display())))?;
            Ok(serde_json::to_value(SaveProjectFileResult {
                path: trimmed.to_owned(),
                bytes_written: bytes.len(),
                object_count: session.objects.len(),
                modifier_count: session.modifiers.len(),
            })?)
        }
        #[cfg(not(target_arch = "wasm32"))]
        "project.load_file" => {
            let p: LoadProjectFileParams = serde_json::from_value(params)?;
            let trimmed = p.path.trim();
            if trimmed.is_empty() {
                return Err(EngineError::InvalidArgument {
                    field: "path",
                    reason: "must not be empty",
                });
            }
            let path = std::path::Path::new(trimmed);
            let bytes = std::fs::read(path)
                .map_err(|e| EngineError::FileWrite(format!("{}: {e}", path.display())))?;
            let fallback = path.file_name().and_then(|s| s.to_str());
            let res = load_project_3mf_bytes(session, &bytes, fallback)?;
            Ok(serde_json::to_value(res)?)
        }
        "plates.list" => {
            let _: EmptyParams = if params.is_null() {
                EmptyParams {}
            } else {
                serde_json::from_value(params)?
            };
            Ok(serde_json::to_value(session.list_plates())?)
        }
        "plates.select" => {
            let p: SelectPlateParams = serde_json::from_value(params)?;
            let idx = resolve_plate_index(session, p.index, p.id, p.name.as_deref())?;
            let summary = session.select_plate(idx)?;
            Ok(serde_json::to_value(summary)?)
        }
        "plates.add" => {
            let p: AddPlateParams = if params.is_null() {
                AddPlateParams {
                    name: None,
                    select: true,
                }
            } else {
                serde_json::from_value(params)?
            };
            let summary = session.add_plate(p.name.as_deref(), p.select)?;
            Ok(serde_json::to_value(summary)?)
        }
        "plates.rename" => {
            let p: RenamePlateParams = serde_json::from_value(params)?;
            let idx = if p.index.is_none() && p.id.is_none() && p.name_match.is_none() {
                session.active_plate_index()
            } else {
                resolve_plate_index(session, p.index, p.id, p.name_match.as_deref())?
            };
            let summary = session.rename_plate(idx, &p.name)?;
            Ok(serde_json::to_value(summary)?)
        }
        "plates.remove" => {
            let p: RemovePlateParams = serde_json::from_value(params)?;
            let idx = resolve_plate_index(session, p.index, p.id, p.name.as_deref())?;
            let summary = session.remove_plate(idx)?;
            Ok(serde_json::to_value(summary)?)
        }
        other => Err(EngineError::UnknownCommand(other.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use layercraft_geom::{Point3, Triangle};
    use layercraft_io::write_stl_binary;

    fn sample_cube_tris() -> Vec<Triangle> {
        let s = 20.0;
        [
            ((0.0, 0.0, 0.0), (0.0, s, 0.0), (s, s, 0.0)),
            ((0.0, 0.0, 0.0), (s, s, 0.0), (s, 0.0, 0.0)),
            ((0.0, 0.0, s), (s, 0.0, s), (s, s, s)),
            ((0.0, 0.0, s), (s, s, s), (0.0, s, s)),
            ((0.0, 0.0, 0.0), (s, 0.0, 0.0), (s, 0.0, s)),
            ((0.0, 0.0, 0.0), (s, 0.0, s), (0.0, 0.0, s)),
            ((0.0, s, 0.0), (0.0, s, s), (s, s, s)),
            ((0.0, s, 0.0), (s, s, s), (s, s, 0.0)),
            ((0.0, 0.0, 0.0), (0.0, 0.0, s), (0.0, s, s)),
            ((0.0, 0.0, 0.0), (0.0, s, s), (0.0, s, 0.0)),
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
        .collect()
    }

    fn sample_cube_bytes() -> Vec<u8> {
        write_stl_binary(&TriangleMesh::new(sample_cube_tris()).unwrap())
    }

    #[test]
    fn command_pipeline_and_hostile_params_never_panic() {
        let mut sess = Session::default();
        assert!(!command_registry().is_empty());

        // Hostile calls before loading or with bad params must return Err, never panic.
        assert!(execute(&mut sess, "slice.run", Value::Null).is_err());
        assert!(execute(&mut sess, "gcode.export", Value::Null).is_err());
        assert!(execute(&mut sess, "unknown.cmd", Value::Null).is_err());
        assert!(
            execute(
                &mut sess,
                "plate.add_stl_bytes",
                serde_json::json!({"bytes": [1,2,3]})
            )
            .is_err()
        );

        let bytes = sample_cube_bytes();
        execute(
            &mut sess,
            "plate.add_stl_bytes",
            serde_json::json!({ "bytes": bytes }),
        )
        .unwrap();
        let summary = execute(&mut sess, "slice.run", Value::Null).unwrap();
        assert_eq!(summary["layer_count"], 100);
        let layer0 = execute(
            &mut sess,
            "layer.inspect",
            serde_json::json!({ "index": 0 }),
        )
        .unwrap();
        assert_eq!(layer0["index"], 0);
        assert!(
            execute(
                &mut sess,
                "layer.inspect",
                serde_json::json!({ "index": 999 })
            )
            .is_err()
        );
        let gcode_val = execute(&mut sess, "gcode.export", Value::Null).unwrap();
        let gcode_str = gcode_val["gcode"].as_str().unwrap().to_owned();
        let report = execute(
            &mut sess,
            "gcode.evaluate",
            serde_json::json!({ "gcode": gcode_str }),
        )
        .unwrap();
        assert_eq!(report["active_layer_count"], 100);
    }

    #[test]
    fn sample_shape_command_loads_and_slices() {
        let mut sess = Session::default();
        let insp = execute(
            &mut sess,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": "hollow_tube" }),
        )
        .unwrap();
        assert!(insp["triangle_count"].as_u64().unwrap() > 100);
        let summary = execute(&mut sess, "slice.run", Value::Null).unwrap();
        assert_eq!(summary["layer_count"], 50);
    }

    #[test]
    fn hostile_params_never_panic() {
        let mut sess = Session::default();
        for bad_bytes in layercraft_testkit::hostile_stl_inputs() {
            assert!(
                execute(
                    &mut sess,
                    "plate.add_stl_bytes",
                    serde_json::json!({ "bytes": bad_bytes }),
                )
                .is_err()
            );
        }
        for bad_cfg in layercraft_testkit::hostile_configs() {
            assert!(
                execute(
                    &mut sess,
                    "config.set",
                    serde_json::json!({ "config": bad_cfg }),
                )
                .is_err()
            );
        }
        for mesh in [
            layercraft_testkit::make_hollow_cylinder(15.0, 8.0, 10.0, 48).unwrap(),
            layercraft_testkit::make_stepped_block(20.0, 20.0, 10.0, 10.0, 10.0, 10.0).unwrap(),
        ] {
            let stl = write_stl_binary(&mesh);
            execute(
                &mut sess,
                "plate.add_stl_bytes",
                serde_json::json!({ "bytes": stl }),
            )
            .unwrap();
            execute(&mut sess, "slice.run", Value::Null).unwrap();
            let g = execute(&mut sess, "gcode.export", Value::Null).unwrap();
            let gstr = g["gcode"].as_str().unwrap().to_owned();
            let rep = execute(
                &mut sess,
                "gcode.evaluate",
                serde_json::json!({ "gcode": gstr }),
            )
            .unwrap();
            assert_eq!(rep["safety"]["non_finite_values"], 0);
            assert_eq!(rep["safety"]["out_of_bounds_moves"], 0);
        }
    }

    #[test]
    fn multi_object_append_transform_arrange_and_slice() {
        let mut sess = Session::default();
        execute(
            &mut sess,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": "cube20", "append": false }),
        )
        .unwrap();
        execute(
            &mut sess,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": "cylinder", "append": true, "name": "Cyl" }),
        )
        .unwrap();
        execute(
            &mut sess,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": "thin_rib", "append": true }),
        )
        .unwrap();

        let objs: Vec<PlateObjectInfo> =
            serde_json::from_value(execute(&mut sess, "plate.list_objects", Value::Null).unwrap())
                .unwrap();
        assert_eq!(objs.len(), 3);
        let id1 = objs[0].id;
        let id3 = objs[2].id;

        execute(
            &mut sess,
            "plate.transform_object",
            serde_json::json!({
                "id": id3,
                "rotation_deg": [0.0, 0.0, 45.0],
                "scale": [1.2, 1.2, 1.0]
            }),
        )
        .unwrap();

        let arranged: Vec<PlateObjectInfo> = serde_json::from_value(
            execute(
                &mut sess,
                "plate.auto_arrange",
                serde_json::json!({ "spacing_mm": 8.0 }),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(arranged.len(), 3);

        execute(&mut sess, "slice.run", Value::Null).unwrap();
        let g = execute(&mut sess, "gcode.export", Value::Null).unwrap();
        let gstr = g["gcode"].as_str().unwrap().to_owned();
        let rep: GcodeReport = serde_json::from_value(
            execute(
                &mut sess,
                "gcode.evaluate",
                serde_json::json!({ "gcode": gstr }),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(rep.safety.is_clean());
        let bbox = rep.extrusion_bbox_xy.unwrap();
        assert!(bbox[2] - bbox[0] > 50.0, "expected arranged width > 50 mm");

        execute(
            &mut sess,
            "plate.remove_object",
            serde_json::json!({ "id": id1 }),
        )
        .unwrap();
        let after_rem: Vec<PlateObjectInfo> =
            serde_json::from_value(execute(&mut sess, "plate.list_objects", Value::Null).unwrap())
                .unwrap();
        assert_eq!(after_rem.len(), 2);

        assert!(
            execute(
                &mut sess,
                "plate.remove_object",
                serde_json::json!({ "id": 9999 })
            )
            .is_err()
        );
        assert!(
            execute(
                &mut sess,
                "plate.transform_object",
                serde_json::json!({ "id": 9999, "scale": [1.0, 1.0, 1.0] })
            )
            .is_err()
        );
    }

    #[test]
    fn profile_commands_load_resolve_apply_and_slice() {
        let mut sess = Session::default();

        let listed: Vec<ProfileSummary> =
            serde_json::from_value(execute(&mut sess, "profile.list", Value::Null).unwrap())
                .unwrap();
        assert!(listed.iter().any(|p| p.name == "LayerCraft Generic PETG"));

        let custom_json = r#"{
            "type": "filament",
            "name": "Custom Carbon PETG",
            "inherits": "LayerCraft Generic PETG",
            "nozzle_temperature": ["250"],
            "filament_z_hop": ["0.45"]
        }"#;
        let loaded: Vec<ProfileSummary> = serde_json::from_value(
            execute(
                &mut sess,
                "profile.load_json",
                serde_json::json!({ "json": custom_json }),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "Custom Carbon PETG");

        let resolved: ResolvedProfile = serde_json::from_value(
            execute(
                &mut sess,
                "profile.resolve",
                serde_json::json!({ "kind": "filament", "name": "Custom Carbon PETG" }),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            resolved.inheritance_chain,
            vec![
                "lc_filament_base",
                "LayerCraft Generic PETG",
                "Custom Carbon PETG"
            ]
        );

        let applied: ProfileApplyResult = serde_json::from_value(
            execute(
                &mut sess,
                "profile.apply",
                serde_json::json!({
                    "machine": "LayerCraft Generic Klipper 250mm",
                    "filament": "Custom Carbon PETG",
                    "process": "LayerCraft 0.16mm Quality",
                    "reset_to_defaults": true
                }),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(applied.config.filament.nozzle_temp_c, 250);
        assert!((applied.config.machine.z_hop_mm - 0.45).abs() < 1e-6);
        assert_eq!(
            sess.active_profiles.filament.as_deref(),
            Some("Custom Carbon PETG")
        );

        execute(
            &mut sess,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": "cube20" }),
        )
        .unwrap();
        let slice_res = execute(&mut sess, "slice.run", Value::Null).unwrap();
        assert_eq!(slice_res["layer_count"], 125);
        let g = execute(&mut sess, "gcode.export", Value::Null).unwrap();
        let gstr = g["gcode"].as_str().unwrap().to_owned();
        assert!(gstr.contains("M104 S250"));

        let rep: GcodeReport = serde_json::from_value(
            execute(
                &mut sess,
                "gcode.evaluate",
                serde_json::json!({ "gcode": gstr }),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(rep.safety.is_clean());
        assert!(rep.z_hop_count > 0);

        // Hostile inputs never panic.
        assert!(
            execute(
                &mut sess,
                "profile.load_json",
                serde_json::json!({ "json": "{" })
            )
            .is_err()
        );
        assert!(
            execute(
                &mut sess,
                "profile.resolve",
                serde_json::json!({ "kind": "process", "name": "missing_profile" })
            )
            .is_err()
        );
    }

    #[test]
    fn modifier_commands_and_adaptive_dome_layer_inspect_work() {
        let mut sess = Session::default();

        // Load built-in Dome sample & enable adaptive layer height
        execute(
            &mut sess,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": "dome" }),
        )
        .unwrap();

        let mut cfg = sess.config.clone();
        cfg.process.adaptive_layer_height.enabled = true;
        cfg.process.adaptive_layer_height.min_layer_height_mm = Some(0.08);
        cfg.process.adaptive_layer_height.max_layer_height_mm = Some(0.28);
        execute(
            &mut sess,
            "config.set",
            serde_json::json!({ "config": cfg }),
        )
        .unwrap();

        // Add box primitive modifier overriding infill density
        let box_mod: PlateModifierInfo = serde_json::from_value(
            execute(
                &mut sess,
                "plate.add_modifier_primitive",
                serde_json::json!({
                    "primitive": { "kind": "box", "width_mm": 12.0, "depth_mm": 12.0, "height_mm": 8.0 },
                    "name": "Core Boost",
                    "role": "modifier",
                    "overrides": { "sparse_infill_density": 0.65, "wall_loops": 4 }
                }),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(box_mod.name, "Core Boost");

        // Add STL bytes modifier
        let stl_bytes = sample_cube_bytes();
        let stl_mod: PlateModifierInfo = serde_json::from_value(
            execute(
                &mut sess,
                "plate.add_modifier_stl_bytes",
                serde_json::json!({
                    "bytes": stl_bytes,
                    "role": "support_blocker"
                }),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(stl_mod.role, ModifierRole::SupportBlocker);

        let listed: Vec<PlateModifierInfo> = serde_json::from_value(
            execute(&mut sess, "plate.list_modifiers", Value::Null).unwrap(),
        )
        .unwrap();
        assert_eq!(listed.len(), 2);

        // Remove STL blocker & update primitive modifier
        execute(
            &mut sess,
            "plate.remove_modifier",
            serde_json::json!({ "id": stl_mod.id }),
        )
        .unwrap();
        let updated: PlateModifierInfo = serde_json::from_value(
            execute(
                &mut sess,
                "plate.update_modifier",
                serde_json::json!({
                    "id": box_mod.id,
                    "position_xyz_mm": [125.0, 125.0, 4.0],
                    "overrides": { "sparse_infill_density": 0.70 }
                }),
            )
            .unwrap(),
        )
        .unwrap();
        assert!((updated.transform.position_xyz_mm[2] - 4.0).abs() < 1e-6);

        // Missing ID & hostile override fail cleanly
        assert!(
            execute(
                &mut sess,
                "plate.remove_modifier",
                serde_json::json!({ "id": 9999 })
            )
            .is_err()
        );
        assert!(
            execute(
                &mut sess,
                "plate.update_modifier",
                serde_json::json!({ "id": box_mod.id, "overrides": { "sparse_infill_density": 1.8 } })
            )
            .is_err()
        );

        let slice_res: SliceSummaryResult =
            serde_json::from_value(execute(&mut sess, "slice.run", Value::Null).unwrap()).unwrap();
        assert!(slice_res.layer_count > 60);

        let low_insp: LayerInspection = serde_json::from_value(
            execute(
                &mut sess,
                "layer.inspect",
                serde_json::json!({ "index": 3 }),
            )
            .unwrap(),
        )
        .unwrap();
        let top_insp: LayerInspection = serde_json::from_value(
            execute(
                &mut sess,
                "layer.inspect",
                serde_json::json!({ "index": slice_res.layer_count - 2 }),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            low_insp.height_mm > top_insp.height_mm + 0.08,
            "expected adaptive height variation: low={} top={}",
            low_insp.height_mm,
            top_insp.height_mm
        );

        let g = execute(&mut sess, "gcode.export", Value::Null).unwrap();
        let gstr = g["gcode"].as_str().unwrap().to_owned();
        let rep: GcodeReport = serde_json::from_value(
            execute(
                &mut sess,
                "gcode.evaluate",
                serde_json::json!({ "gcode": gstr }),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(rep.safety.is_clean(), "{:?}", rep.safety);
        assert!(rep.layer_height_variation_ratio > 2.0);

        execute(&mut sess, "plate.clear", Value::Null).unwrap();
        let cleared_mods: Vec<PlateModifierInfo> = serde_json::from_value(
            execute(&mut sess, "plate.list_modifiers", Value::Null).unwrap(),
        )
        .unwrap();
        assert!(cleared_mods.is_empty());
    }

    #[test]
    fn config_patch_and_atomic_batch_and_compare_work() {
        let mut sess = Session::default();

        // Discovery helpers
        let summaries = list_command_summaries(Some("config."));
        assert!(summaries.iter().any(|s| s.name == "config.patch"));
        let desc = describe_command("slice.compare").unwrap();
        assert_eq!(desc.name, "slice.compare");

        // Atomic batch rolls back completely on failure
        let bad_batch = execute_batch(
            &mut sess,
            &[
                BatchStep {
                    command: "plate.add_sample_shape".to_owned(),
                    params: serde_json::json!({ "shape": "cube20" }),
                },
                BatchStep {
                    command: "config.patch".to_owned(),
                    params: serde_json::json!({ "patch": { "process": { "layer_height_mm": 0.16 } } }),
                },
                BatchStep {
                    command: "nonexistent.command".to_owned(),
                    params: Value::Null,
                },
            ],
            true,
        );
        assert!(!bad_batch.ok);
        assert!(bad_batch.rolled_back);
        assert!(sess.objects.is_empty());
        assert!((sess.config.process.layer_height_mm - 0.2).abs() < 1e-6);

        // Successful batch
        let ok_batch = execute_batch(
            &mut sess,
            &[
                BatchStep {
                    command: "plate.add_sample_shape".to_owned(),
                    params: serde_json::json!({ "shape": "bridge_arch" }),
                },
                BatchStep {
                    command: "config.patch".to_owned(),
                    params: serde_json::json!({
                        "process": { "sparse_infill_density": 0.15 }
                    }),
                },
                BatchStep {
                    command: "slice.run".to_owned(),
                    params: Value::Null,
                },
            ],
            true,
        );
        assert!(ok_batch.ok, "{:?}", ok_batch.error);
        assert!((sess.config.process.sparse_infill_density - 0.15).abs() < 1e-6);

        // Printability DFM check
        let dfm: PrintabilityReport = serde_json::from_value(
            execute(&mut sess, "plate.analyze_printability", Value::Null).unwrap(),
        )
        .unwrap();
        assert!(dfm.sliced);
        assert!(dfm.fits_bed);
        assert!(dfm.bridge_count > 0 || dfm.overhang_triangle_area_mm2 > 0.0);

        // Non-destructive A/B slice comparison (does not mutate active config)
        let cmp: SliceComparisonReport = serde_json::from_value(
            execute(
                &mut sess,
                "slice.compare",
                serde_json::json!({
                    "patch_b": { "process": { "sparse_infill_density": 0.45 } }
                }),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(cmp.delta.filament_volume_delta_mm3 > 10.0);
        assert!((sess.config.process.sparse_infill_density - 0.15).abs() < 1e-6);

        // Direct disk export + built-in safety evaluation
        let tmp_gcode = std::env::temp_dir().join("layercraft_engine_phase5c_test.gcode");
        let saved: SaveGcodeFileResult = serde_json::from_value(
            execute(
                &mut sess,
                "gcode.save_file",
                serde_json::json!({
                    "path": tmp_gcode.to_string_lossy(),
                    "evaluate": true
                }),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(saved.bytes_written > 200);
        assert!(saved.evaluation.as_ref().unwrap().safety.is_clean());
        let _ = std::fs::remove_file(tmp_gcode);
    }

    #[test]
    fn object_transform_3mf_matrix_matches_prepare_transformed_object() {
        let raw = build_sample_shape(SampleShape::SteppedBlock).unwrap();
        let transform = ObjectTransform {
            position_xy_mm: [82.5, 163.0],
            rotation_deg: [17.0, -23.0, 41.0],
            scale: [1.3, 0.85, 1.6],
        };
        let prepared = prepare_transformed_object(&raw, transform).unwrap();
        let mat =
            layercraft_io::ThreeMfMatrix(object_transform_3mf_matrix(&raw, transform).unwrap());
        for (tri_raw, tri_prep) in raw.triangles.iter().zip(prepared.triangles.iter()) {
            for (pr, pp) in [
                (tri_raw.a, tri_prep.a),
                (tri_raw.b, tri_prep.b),
                (tri_raw.c, tri_prep.c),
            ] {
                let mapped = mat.apply(pr);
                assert!((mapped.x - pp.x).abs() < 1e-6);
                assert!((mapped.y - pp.y).abs() < 1e-6);
                assert!((mapped.z - pp.z).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn multipart_3mf_preserves_relative_layout() {
        let c1 = build_sample_shape(SampleShape::Cube20).unwrap();
        let c2 = build_sample_shape(SampleShape::Cylinder).unwrap();
        let doc_out = ThreeMfExportDocument {
            resources: vec![
                ThreeMfResourceMesh {
                    id: 1,
                    name: "Left Part".to_owned(),
                    object_type: "model".to_owned(),
                    mesh: c1,
                },
                ThreeMfResourceMesh {
                    id: 2,
                    name: "Right Part".to_owned(),
                    object_type: "model".to_owned(),
                    mesh: c2,
                },
            ],
            build_items: vec![
                ThreeMfBuildItem {
                    object_id: 1,
                    transform: Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 10.0, 15.0, 0.0]),
                },
                ThreeMfBuildItem {
                    object_id: 2,
                    transform: Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 60.0, 15.0, 0.0]),
                },
            ],
            plates: Vec::new(),
            project_json: None,
        };
        let bytes = write_3mf_document_bytes(&doc_out).unwrap();
        let mut sess = Session::default();
        execute(
            &mut sess,
            "plate.add_3mf_bytes",
            serde_json::json!({ "bytes": bytes }),
        )
        .unwrap();
        assert_eq!(sess.objects.len(), 2);
        let dx = sess.objects[1].transform.position_xy_mm[0]
            - sess.objects[0].transform.position_xy_mm[0];
        assert!(
            (dx - 40.0).abs() < 1e-4,
            "expected relative X delta 40mm, got {dx}"
        );
    }

    #[test]
    fn project_3mf_roundtrip_preserves_objects_modifiers_and_config() {
        let mut sess = Session::default();
        execute(
            &mut sess,
            "profile.apply",
            serde_json::json!({
                "machine": "LayerCraft Generic Klipper 250mm",
                "filament": "LayerCraft Generic PETG",
                "process": "LayerCraft 0.16mm Quality",
                "reset_to_defaults": true
            }),
        )
        .unwrap();
        execute(
            &mut sess,
            "config.patch",
            serde_json::json!({ "patch": { "process": { "sparse_infill_density": 0.37 } } }),
        )
        .unwrap();
        execute(
            &mut sess,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": "cube20", "name": "Base Cube" }),
        )
        .unwrap();
        execute(
            &mut sess,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": "cylinder", "append": true, "name": "Boss" }),
        )
        .unwrap();
        let id0 = sess.objects[0].id;
        execute(
            &mut sess,
            "plate.transform_object",
            serde_json::json!({
                "id": id0,
                "position_xy_mm": [75.0, 110.0],
                "rotation_deg": [0.0, 0.0, 35.0],
                "scale": [1.4, 0.9, 1.2]
            }),
        )
        .unwrap();

        execute(
            &mut sess,
            "plate.add_modifier_primitive",
            serde_json::json!({
                "primitive": { "kind": "box", "width_mm": 12.0, "depth_mm": 12.0, "height_mm": 10.0 },
                "name": "Local Shell Boost",
                "role": "modifier",
                "overrides": { "wall_loops": 5 }
            }),
        )
        .unwrap();
        execute(
            &mut sess,
            "plate.add_modifier_stl_bytes",
            serde_json::json!({
                "bytes": sample_cube_bytes(),
                "name": "Mesh Blocker",
                "role": "support_blocker",
                "transform": {
                    "position_xyz_mm": [90.0, 120.0, 8.0],
                    "rotation_deg": [0.0, 0.0, 15.0],
                    "scale": [0.5, 0.5, 0.5]
                }
            }),
        )
        .unwrap();

        let exported: ProjectExportBytesResult = serde_json::from_value(
            execute(&mut sess, "project.export_3mf_bytes", Value::Null).unwrap(),
        )
        .unwrap();
        assert_eq!(exported.object_count, 2);
        assert_eq!(exported.modifier_count, 2);

        // Plain 3MF readers see the 2 positioned print objects (and NOT the "other" modifier volume).
        let plain_doc = read_3mf_document_bytes(&exported.bytes).unwrap();
        assert_eq!(plain_doc.parts.len(), 2);
        let p0_bb = plain_doc.parts[0].mesh.aabb().unwrap();
        let orig_bb = sess.objects[0].prepared_mesh.aabb().unwrap();
        assert!((p0_bb.min.x - orig_bb.min.x).abs() < 1e-4);
        assert!((p0_bb.max.z - orig_bb.max.z).abs() < 1e-4);

        let mut loaded_sess = Session::default();
        let load_res: ProjectLoadResult = serde_json::from_value(
            execute(
                &mut loaded_sess,
                "project.load_3mf_bytes",
                serde_json::json!({ "bytes": exported.bytes }),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(load_res.is_layercraft_project);
        assert_eq!(loaded_sess.objects.len(), 2);
        assert_eq!(loaded_sess.modifiers.len(), 2);
        assert_eq!(loaded_sess.objects[0].transform, sess.objects[0].transform);
        assert_eq!(loaded_sess.modifiers[0].overrides.wall_loops, Some(5));
        assert_eq!(loaded_sess.modifiers[1].role, ModifierRole::SupportBlocker);
        assert!(loaded_sess.modifiers[1].primitive.is_none());
        assert!((loaded_sess.config.process.sparse_infill_density - 0.37).abs() < 1e-6);
        assert_eq!(
            loaded_sess.active_profiles.filament.as_deref(),
            Some("LayerCraft Generic PETG")
        );

        // Corrupt project archive returns Err cleanly without panic.
        assert!(
            execute(
                &mut loaded_sess,
                "project.load_3mf_bytes",
                serde_json::json!({ "bytes": [1, 2, 3] }),
            )
            .is_err()
        );
    }

    #[test]
    fn undo_redo_and_atomic_batch_coalescing() {
        let mut sess = Session::default();
        let default_density = sess.config.process.sparse_infill_density;
        let default_walls = sess.config.process.wall_loops;
        let h0: HistoryStatus =
            serde_json::from_value(execute(&mut sess, "session.history", Value::Null).unwrap())
                .unwrap();
        assert!(!h0.can_undo);
        assert!(!h0.can_redo);

        execute(
            &mut sess,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": "cube20" }),
        )
        .unwrap();
        let id = sess.objects[0].id;
        execute(
            &mut sess,
            "plate.transform_object",
            serde_json::json!({ "id": id, "position_xy_mm": [90.0, 100.0] }),
        )
        .unwrap();
        execute(
            &mut sess,
            "config.patch",
            serde_json::json!({ "patch": { "process": { "sparse_infill_density": 0.42 } } }),
        )
        .unwrap();
        // No-op config.set must NOT push onto undo stack.
        let same_cfg = sess.config.clone();
        execute(
            &mut sess,
            "config.set",
            serde_json::json!({ "config": same_cfg }),
        )
        .unwrap();

        let h1 = sess.history_status();
        assert_eq!(h1.undo_depth, 3);

        // Undo 1: revert infill density
        let u1: HistoryActionResult =
            serde_json::from_value(execute(&mut sess, "session.undo", Value::Null).unwrap())
                .unwrap();
        assert!(u1.applied);
        assert!((sess.config.process.sparse_infill_density - default_density).abs() < 1e-6);

        // Undo 2: revert transform
        execute(&mut sess, "session.undo", Value::Null).unwrap();
        assert!((sess.objects[0].transform.position_xy_mm[0] - 125.0).abs() < 1e-6);

        // Redo 1: re-apply transform
        execute(&mut sess, "session.redo", Value::Null).unwrap();
        assert!((sess.objects[0].transform.position_xy_mm[0] - 90.0).abs() < 1e-6);

        // Coalesced atomic batch produces ONE undo entry
        let depth_before_batch = sess.history_status().undo_depth;
        let batch = execute_batch(
            &mut sess,
            &[
                BatchStep {
                    command: "plate.add_sample_shape".to_owned(),
                    params: serde_json::json!({ "shape": "cylinder", "append": true }),
                },
                BatchStep {
                    command: "config.patch".to_owned(),
                    params: serde_json::json!({ "patch": { "process": { "wall_loops": 4 } } }),
                },
            ],
            true,
        );
        assert!(batch.ok);
        assert_eq!(sess.history_status().undo_depth, depth_before_batch + 1);
        assert_eq!(sess.objects.len(), 2);
        assert_eq!(sess.config.process.wall_loops, 4);

        // Failed atomic batch leaves undo stack untouched
        let fail_batch = execute_batch(
            &mut sess,
            &[
                BatchStep {
                    command: "config.patch".to_owned(),
                    params: serde_json::json!({ "patch": { "process": { "wall_loops": 6 } } }),
                },
                BatchStep {
                    command: "missing.command".to_owned(),
                    params: Value::Null,
                },
            ],
            true,
        );
        assert!(!fail_batch.ok);
        assert_eq!(sess.history_status().undo_depth, depth_before_batch + 1);
        assert_eq!(sess.config.process.wall_loops, 4);

        // Single undo reverts entire successful batch
        execute(&mut sess, "session.undo", Value::Null).unwrap();
        assert_eq!(sess.objects.len(), 1);
        assert_eq!(sess.config.process.wall_loops, default_walls);
    }

    #[test]
    fn multiplate_3mf_import_switch_and_project_roundtrip() {
        let cube = build_sample_shape(SampleShape::Cube20).unwrap();
        let cyl = build_sample_shape(SampleShape::Cylinder).unwrap();
        let block = build_sample_shape(SampleShape::SteppedBlock).unwrap();

        let multi_doc = ThreeMfExportDocument {
            resources: vec![
                ThreeMfResourceMesh {
                    id: 1,
                    name: "BaseCube.stl".to_owned(),
                    object_type: "model".to_owned(),
                    mesh: cube,
                },
                ThreeMfResourceMesh {
                    id: 2,
                    name: "WallLeft.stl".to_owned(),
                    object_type: "model".to_owned(),
                    mesh: cyl,
                },
                ThreeMfResourceMesh {
                    id: 3,
                    name: "WallRight.stl".to_owned(),
                    object_type: "model".to_owned(),
                    mesh: block,
                },
            ],
            build_items: vec![
                ThreeMfBuildItem {
                    object_id: 1,
                    transform: Some([
                        1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 100.0, 100.0, 0.0,
                    ]),
                },
                ThreeMfBuildItem {
                    object_id: 2,
                    transform: Some([
                        1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 720.0, 50.0, 0.0,
                    ]),
                },
                ThreeMfBuildItem {
                    object_id: 3,
                    transform: Some([
                        1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 780.0, 50.0, 0.0,
                    ]),
                },
            ],
            plates: vec![
                ThreeMfExportPlate {
                    id: 1,
                    name: "01_Base".to_owned(),
                    instances: vec![(1, 0)],
                },
                ThreeMfExportPlate {
                    id: 2,
                    name: "Walls".to_owned(),
                    instances: vec![(2, 0), (3, 0)],
                },
            ],
            project_json: None,
        };
        let raw_3mf = write_3mf_document_bytes(&multi_doc).unwrap();

        let mut sess = Session::default();
        let loaded: ProjectLoadResult = serde_json::from_value(
            execute(
                &mut sess,
                "project.load_3mf_bytes",
                serde_json::json!({ "bytes": raw_3mf, "name": "Fallback.3mf" }),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!loaded.is_layercraft_project);
        assert_eq!(loaded.plate_count, 2);
        assert_eq!(sess.plate_count(), 2);
        assert_eq!(sess.active_plate_name(), "01_Base");
        assert_eq!(sess.objects.len(), 1);
        assert_eq!(sess.objects[0].name, "BaseCube.stl");

        // Switch to Plate 2 ("Walls") and confirm both walls are independently bed-centered.
        let p2: PlateSummary = serde_json::from_value(
            execute(
                &mut sess,
                "plates.select",
                serde_json::json!({ "name": "Walls" }),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(p2.index, 1);
        assert_eq!(p2.object_count, 2);
        assert_eq!(sess.objects.len(), 2);
        assert_eq!(sess.objects[0].name, "WallLeft.stl");
        assert_eq!(sess.objects[1].name, "WallRight.stl");
        let walls_bb = sess.prepared_mesh.as_ref().unwrap().aabb().unwrap();
        assert!(walls_bb.min.x >= 0.0 && walls_bb.max.x <= 250.0);
        assert!(walls_bb.min.y >= 0.0 && walls_bb.max.y <= 250.0);

        // Slice Plate 2, switch back to Plate 1, and confirm Plate 2 keeps its cached slice.
        execute(&mut sess, "slice.run", Value::Null).unwrap();
        assert!(sess.sliced.is_some());
        execute(
            &mut sess,
            "plates.select",
            serde_json::json!({ "index": 0 }),
        )
        .unwrap();
        assert!(sess.sliced.is_none());
        let plates_list: Vec<PlateSummary> =
            serde_json::from_value(execute(&mut sess, "plates.list", Value::Null).unwrap())
                .unwrap();
        assert!(!plates_list[0].sliced);
        assert!(plates_list[1].sliced);

        // Add & rename a 3rd plate, switch to plate 2 again, export LayerCraft project, reload!
        execute(
            &mut sess,
            "plates.add",
            serde_json::json!({ "name": "Spare", "select": false }),
        )
        .unwrap();
        execute(
            &mut sess,
            "plates.rename",
            serde_json::json!({ "index": 2, "name": "Extras" }),
        )
        .unwrap();
        execute(
            &mut sess,
            "plates.select",
            serde_json::json!({ "index": 1 }),
        )
        .unwrap();
        let proj_bytes = export_project_3mf_bytes(&sess).unwrap();

        let mut reloaded = Session::default();
        let reload_res = load_project_3mf_bytes(&mut reloaded, &proj_bytes, None).unwrap();
        assert!(reload_res.is_layercraft_project);
        assert_eq!(reload_res.plate_count, 3);
        assert_eq!(reloaded.plate_count(), 3);
        assert_eq!(reloaded.active_plate_name(), "Walls");
        assert_eq!(reloaded.objects.len(), 2);
        assert_eq!(reloaded.list_plates()[2].name, "Extras");

        // Undo remove plate restores it cleanly.
        execute(
            &mut reloaded,
            "plates.remove",
            serde_json::json!({ "index": 2 }),
        )
        .unwrap();
        assert_eq!(reloaded.plate_count(), 2);
        execute(&mut reloaded, "session.undo", Value::Null).unwrap();
        assert_eq!(reloaded.plate_count(), 3);
        assert_eq!(reloaded.list_plates()[2].name, "Extras");
    }

    #[test]
    fn bgcode_export_save_and_printer_registry_keep_secrets_out_of_3mf() {
        let mut sess = Session::default();
        execute(
            &mut sess,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": "cube20" }),
        )
        .unwrap();
        execute(&mut sess, "slice.run", Value::Null).unwrap();

        let exported: ExportBgcodeResult =
            serde_json::from_value(execute(&mut sess, "gcode.export_bgcode", Value::Null).unwrap())
                .unwrap();
        let decoded_bytes = base64::engine::general_purpose::STANDARD
            .decode(&exported.bytes_base64)
            .unwrap();
        assert!(layercraft_gcode::is_bgcode(&decoded_bytes));
        assert!(exported.byte_len < exported.ascii_byte_len);

        #[cfg(not(target_arch = "wasm32"))]
        {
            let bg_path = std::env::temp_dir().join(format!(
                "layercraft_engine_test_{}.bgcode",
                std::process::id()
            ));
            let saved: SaveGcodeFileResult = serde_json::from_value(
                execute(
                    &mut sess,
                    "gcode.save_file",
                    serde_json::json!({ "path": bg_path.to_string_lossy(), "evaluate": true }),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(saved.format, "bgcode");
            assert!(saved.evaluation.as_ref().unwrap().safety.is_clean());
            let _ = std::fs::remove_file(&bg_path);
        }

        let summary: PrinterProfileSummary = serde_json::from_value(
            execute(
                &mut sess,
                "printer.upsert",
                serde_json::json!({
                    "profile": {
                        "id": "coreone",
                        "name": "CORE One+",
                        "protocol": "prusa_link",
                        "base_url": "192.168.1.55",
                        "auth": {
                            "kind": "digest",
                            "username": "maker",
                            "password": "top-secret-password"
                        }
                    }
                }),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(summary.has_credentials);
        let list_json = execute(&mut sess, "printer.list", Value::Null).unwrap();
        assert!(!list_json.to_string().contains("top-secret-password"));

        let proj_bytes = export_project_3mf_bytes(&sess).unwrap();
        let raw_text = String::from_utf8_lossy(&proj_bytes);
        assert!(!raw_text.contains("top-secret-password"));
        assert!(!raw_text.contains("192.168.1.55"));

        execute(
            &mut sess,
            "printer.remove",
            serde_json::json!({ "id": "coreone" }),
        )
        .unwrap();
        assert!(sess.printers.is_empty());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn printer_status_and_upload_commands_work_against_mock_prusalink() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let uploaded_payload: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let rec = Arc::clone(&uploaded_payload);

        let srv = std::thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let mut header_end = None;
                while header_end.is_none() {
                    let n = stream.read(&mut tmp).unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                    }
                }
                let h_end = header_end.unwrap();
                let head = String::from_utf8_lossy(&buf[..h_end]).into_owned();
                let req_line = head.lines().next().unwrap_or("");
                let content_len: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        if k.trim().eq_ignore_ascii_case("content-length") {
                            v.trim().parse().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                while buf.len() < h_end + content_len {
                    let n = stream.read(&mut tmp).unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body = buf[h_end..h_end + content_len].to_vec();
                let (st, resp_json) = if req_line.starts_with("GET /api/v1/info ") {
                    ("200 OK", r#"{"firmware":"6.2.0","type":"COREONE"}"#)
                } else if req_line.starts_with("GET /api/v1/status ") {
                    (
                        "200 OK",
                        r#"{"printer":{"state":"IDLE","temp_nozzle":22.0,"temp_bed":21.5}}"#,
                    )
                } else if req_line.starts_with("GET /api/v1/storage ") {
                    (
                        "200 OK",
                        r#"{"storage_list":[{"name":"usb","path":"/usb","read_only":false,"ready":true}]}"#,
                    )
                } else {
                    *rec.lock().unwrap() = body;
                    ("201 Created", r#"{"ok":true}"#)
                };
                let raw = format!(
                    "HTTP/1.1 {st}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{resp_json}",
                    resp_json.len()
                );
                stream.write_all(raw.as_bytes()).unwrap();
            }
        });

        let mut sess = Session::default();
        execute(
            &mut sess,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": "cube20" }),
        )
        .unwrap();
        execute(
            &mut sess,
            "printer.upsert",
            serde_json::json!({
                "profile": {
                    "id": "mock1",
                    "name": "Mock CORE One",
                    "protocol": "prusa_link",
                    "base_url": format!("http://{addr}"),
                    "auth": { "kind": "api_key", "api_key": "abc" }
                }
            }),
        )
        .unwrap();

        let status: PrinterStatusReport = serde_json::from_value(
            execute(
                &mut sess,
                "printer.status",
                serde_json::json!({ "printer_id": "mock1" }),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(status.reachable);
        assert_eq!(status.firmware.as_deref(), Some("6.2.0"));

        let up_res: PrinterUploadResult = serde_json::from_value(
            execute(
                &mut sess,
                "printer.upload",
                serde_json::json!({ "printer_id": "mock1" }),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(up_res.format, "bgcode");
        assert!(!up_res.upload.start_print_requested);
        assert!(up_res.evaluation.as_ref().unwrap().safety.is_clean());
        srv.join().unwrap();

        let bytes = uploaded_payload.lock().unwrap();
        assert!(layercraft_gcode::is_bgcode(&bytes));
        let doc = decode_bgcode(&bytes).unwrap();
        assert!(!doc.gcode.is_empty());
    }

    #[test]
    fn plate_mesh_arcs_shared_across_undo_redo_and_plate_switch() {
        let mut sess = Session::default();
        let insp: MeshInspection = serde_json::from_value(
            execute(
                &mut sess,
                "plate.add_stl_bytes",
                serde_json::json!({"bytes": sample_cube_bytes()}),
            )
            .unwrap(),
        )
        .unwrap();
        // Single-object fast path: plate meshes are the object's own Arcs.
        let obj_prep = Arc::clone(&sess.objects[0].prepared_mesh);
        let obj_raw = Arc::clone(&sess.objects[0].raw_mesh);
        assert!(Arc::ptr_eq(sess.prepared_mesh.as_ref().unwrap(), &obj_prep));
        assert!(Arc::ptr_eq(sess.mesh.as_ref().unwrap(), &obj_raw));
        assert_eq!(sess.plate_inspection.as_ref(), Some(&insp));
        assert!(!insp.needs_repair);
        let cached: MeshInspection =
            serde_json::from_value(execute(&mut sess, "plate.inspect", Value::Null).unwrap())
                .unwrap();
        assert_eq!(cached, insp);

        // Second plate with its own object.
        execute(&mut sess, "plates.add", serde_json::json!({"select": true})).unwrap();
        assert!(sess.plate_inspection.is_none());
        execute(
            &mut sess,
            "plate.add_sample_shape",
            serde_json::json!({"shape": "cylinder"}),
        )
        .unwrap();
        let p2_insp = sess.plate_inspection.clone().unwrap();
        let p2_prep = Arc::clone(sess.prepared_mesh.as_ref().unwrap());

        // Plate switch swaps Arcs and the cached inspection without copying.
        sess.select_plate(0).unwrap();
        assert!(Arc::ptr_eq(&sess.objects[0].prepared_mesh, &obj_prep));
        assert!(Arc::ptr_eq(sess.prepared_mesh.as_ref().unwrap(), &obj_prep));
        assert_eq!(sess.plate_inspection.as_ref(), Some(&insp));
        sess.select_plate(1).unwrap();
        assert!(Arc::ptr_eq(sess.prepared_mesh.as_ref().unwrap(), &p2_prep));
        assert_eq!(sess.plate_inspection.as_ref(), Some(&p2_insp));

        // Undo the cylinder add: plate 2 empties, plate 1 Arcs untouched.
        assert!(sess.undo().applied);
        assert!(sess.objects.is_empty());
        assert!(sess.plate_inspection.is_none());
        sess.select_plate(0).unwrap();
        assert!(Arc::ptr_eq(&sess.objects[0].prepared_mesh, &obj_prep));
        assert!(Arc::ptr_eq(&sess.objects[0].raw_mesh, &obj_raw));
        assert!(Arc::ptr_eq(sess.prepared_mesh.as_ref().unwrap(), &obj_prep));
        assert_eq!(sess.plate_inspection.as_ref(), Some(&insp));

        // Redo restores the cylinder plate with the very same Arcs.
        assert!(sess.redo().applied);
        sess.select_plate(1).unwrap();
        assert!(Arc::ptr_eq(sess.prepared_mesh.as_ref().unwrap(), &p2_prep));
        assert!(Arc::ptr_eq(&sess.objects[0].prepared_mesh, &p2_prep));
        assert_eq!(sess.plate_inspection.as_ref(), Some(&p2_insp));
        sess.select_plate(0).unwrap();
        assert!(Arc::ptr_eq(&sess.objects[0].prepared_mesh, &obj_prep));
    }

    #[test]
    fn multi_object_plate_mesh_merges_and_caches_inspection() {
        let mut sess = Session::default();
        let bytes = sample_cube_bytes();
        execute(
            &mut sess,
            "plate.add_stl_bytes",
            serde_json::json!({"bytes": bytes}),
        )
        .unwrap();
        execute(
            &mut sess,
            "plate.add_stl_bytes",
            serde_json::json!({"bytes": bytes, "append": true}),
        )
        .unwrap();
        let merged = sess.prepared_mesh.as_ref().unwrap();
        assert_eq!(merged.triangles.len(), 24);
        assert!(!Arc::ptr_eq(merged, &sess.objects[0].prepared_mesh));
        let insp = sess.plate_inspection.clone().unwrap();
        assert_eq!(insp, inspect_mesh(merged));
        assert!((insp.volume_mm3 - 16_000.0).abs() < 1.0);
    }

    #[test]
    fn repair_mesh_command_fixes_defects_and_undo_restores_original() {
        let mut sess = Session::default();
        // Empty plate and unknown object ids are errors, never panics.
        assert!(execute(&mut sess, "plate.repair_mesh", Value::Null).is_err());

        let mut tris = sample_cube_tris();
        tris.pop();
        if let Some(t) = tris.first_mut() {
            *t = Triangle::new(t.a, t.c, t.b);
        }
        let bytes = write_stl_binary(&TriangleMesh::new(tris).unwrap());
        let loaded: MeshInspection = serde_json::from_value(
            execute(
                &mut sess,
                "plate.add_stl_bytes",
                serde_json::json!({"bytes": bytes}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(loaded.needs_repair);
        assert!(loaded.boundary_edges > 0);
        let original_raw = Arc::clone(&sess.objects[0].raw_mesh);
        let original_insp = sess.plate_inspection.clone().unwrap();
        let depth_before = sess.history_status().undo_depth;

        assert!(
            execute(
                &mut sess,
                "plate.repair_mesh",
                serde_json::json!({"object_id": 999})
            )
            .is_err()
        );
        assert_eq!(sess.history_status().undo_depth, depth_before);

        let res: PlateRepairResult = serde_json::from_value(
            execute(&mut sess, "plate.repair_mesh", serde_json::json!({})).unwrap(),
        )
        .unwrap();
        assert!(res.changed);
        assert_eq!(res.objects.len(), 1);
        assert_eq!(res.objects[0].object_id, sess.objects[0].id);
        assert!(res.objects[0].report.before.needs_repair);
        let after = res.plate_inspection.clone().unwrap();
        assert!(!after.needs_repair, "{after:?}");
        assert_eq!(after.boundary_edges, 0);
        assert_eq!(after.inconsistent_winding_edges, 0);
        assert!((after.volume_mm3 - 8000.0).abs() < 8.0, "{after:?}");
        assert_eq!(sess.plate_inspection.as_ref(), Some(&after));
        assert!(!Arc::ptr_eq(&sess.objects[0].raw_mesh, &original_raw));
        assert!(Arc::ptr_eq(
            sess.prepared_mesh.as_ref().unwrap(),
            &sess.objects[0].prepared_mesh
        ));
        let hist = sess.history_status();
        assert_eq!(hist.undo_depth, depth_before + 1);
        assert_eq!(hist.undo_label.as_deref(), Some("Repair mesh"));

        // Re-running on a clean mesh is a no-op without a new undo entry.
        let repaired_id = sess.objects[0].id;
        let again: PlateRepairResult = serde_json::from_value(
            execute(
                &mut sess,
                "plate.repair_mesh",
                serde_json::json!({"object_id": repaired_id}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!again.changed);
        assert_eq!(sess.history_status().undo_depth, depth_before + 1);

        // Undo restores the exact original mesh Arc and cached inspection.
        assert!(sess.undo().applied);
        assert!(Arc::ptr_eq(&sess.objects[0].raw_mesh, &original_raw));
        assert_eq!(sess.plate_inspection.as_ref(), Some(&original_insp));
        assert!(sess.plate_inspection.as_ref().unwrap().needs_repair);

        assert!(sess.redo().applied);
        assert!(!sess.plate_inspection.as_ref().unwrap().needs_repair);
        execute(&mut sess, "slice.run", Value::Null).unwrap();
    }

    #[test]
    fn plate_layout_checks_bounds_collisions_and_plate_switches() {
        let mut sess = Session::default();
        execute(
            &mut sess,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": "cube20", "name": "A" }),
        )
        .unwrap();
        let a_id = sess.objects[0].id;
        assert!(!sess.plate_layout.has_issues);

        // Move A partially off the bed in -X.
        execute(
            &mut sess,
            "plate.transform_object",
            serde_json::json!({ "id": a_id, "position_xy_mm": [5.0, 125.0] }),
        )
        .unwrap();
        assert!(sess.plate_layout.has_issues);
        assert_eq!(sess.plate_layout.out_of_bounds_object_ids, vec![a_id]);

        // Re-center A and exceed bed height via Z scale.
        execute(
            &mut sess,
            "plate.transform_object",
            serde_json::json!({
                "id": a_id,
                "position_xy_mm": [125.0, 125.0],
                "scale": [1.0, 1.0, 20.0]
            }),
        )
        .unwrap();
        assert_eq!(sess.plate_layout.out_of_bounds_object_ids, vec![a_id]);

        // Restore unit scale and add B; auto-arrange resolves overlap.
        execute(
            &mut sess,
            "plate.transform_object",
            serde_json::json!({ "id": a_id, "scale": [1.0, 1.0, 1.0] }),
        )
        .unwrap();
        execute(
            &mut sess,
            "plate.add_sample_shape",
            serde_json::json!({ "shape": "cube20", "append": true, "name": "B" }),
        )
        .unwrap();
        let b_id = sess.objects[1].id;
        assert!(!sess.plate_layout.has_issues);

        // Force A and B onto the same XY spot -> collision detected.
        execute(
            &mut sess,
            "plate.transform_object",
            serde_json::json!({ "id": b_id, "position_xy_mm": [125.0, 125.0] }),
        )
        .unwrap();
        assert!(sess.plate_layout.has_issues);
        assert_eq!(sess.plate_layout.colliding_pairs.len(), 1);
        assert!(sess.plate_layout.colliding_object_ids.contains(&a_id));
        assert!(sess.plate_layout.colliding_object_ids.contains(&b_id));

        // Add empty second plate: plate_layout updates on switch; switching back restores issue.
        execute(
            &mut sess,
            "plates.add",
            serde_json::json!({ "name": "Empty", "select": true }),
        )
        .unwrap();
        assert!(!sess.plate_layout.has_issues);
        execute(
            &mut sess,
            "plates.select",
            serde_json::json!({ "index": 0 }),
        )
        .unwrap();
        assert!(sess.plate_layout.has_issues);

        // Auto-arrange clears overlap; shrinking bed smaller than object footprint triggers OOB.
        execute(&mut sess, "plate.auto_arrange", Value::Null).unwrap();
        let check: PlateLayoutCheck =
            serde_json::from_value(execute(&mut sess, "plate.check_layout", Value::Null).unwrap())
                .unwrap();
        assert!(!check.has_issues);
    }

    #[test]
    fn user_profile_commands_save_export_delete_and_protect_builtins() {
        let mut sess = Session::default();
        execute(
            &mut sess,
            "config.patch",
            serde_json::json!({
                "patch": {
                    "filament": { "nozzle_temp_c": 248, "bed_temp_c": 88 }
                }
            }),
        )
        .unwrap();

        let saved: SaveUserProfileResult = serde_json::from_value(
            execute(
                &mut sess,
                "profile.save_user",
                serde_json::json!({
                    "kind": "filament",
                    "name": "My Fast PETG",
                    "activate": true,
                    "persist": false
                }),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(saved.summary.name, "My Fast PETG");
        assert!(!saved.persisted);
        assert_eq!(
            sess.active_profiles.filament.as_deref(),
            Some("My Fast PETG")
        );

        let exported: ExportProfileJsonResult = serde_json::from_value(
            execute(
                &mut sess,
                "profile.export_json",
                serde_json::json!({
                    "kind": "filament",
                    "name": "My Fast PETG"
                }),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(exported.json.contains("My Fast PETG"));

        // Built-ins cannot be overwritten or deleted.
        assert!(
            execute(
                &mut sess,
                "profile.save_user",
                serde_json::json!({
                    "kind": "machine",
                    "name": "LayerCraft CORE One+ HF 0.4mm",
                    "persist": false
                }),
            )
            .is_err()
        );
        assert!(
            execute(
                &mut sess,
                "profile.delete_user",
                serde_json::json!({
                    "kind": "machine",
                    "name": "LayerCraft CORE One+ HF 0.4mm",
                    "persist": false
                }),
            )
            .is_err()
        );

        let del: DeleteUserProfileResult = serde_json::from_value(
            execute(
                &mut sess,
                "profile.delete_user",
                serde_json::json!({
                    "kind": "filament",
                    "name": "My Fast PETG",
                    "persist": false
                }),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(del.deleted);
        assert!(sess.active_profiles.filament.is_none());
        assert!(
            sess.profiles
                .get(ProfileKind::Filament, "My Fast PETG")
                .is_none()
        );
    }
}
