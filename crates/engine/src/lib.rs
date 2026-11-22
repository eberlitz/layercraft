//! Stateful slicing session and schema-typed command registry (`everything is a command`).

use std::collections::BTreeMap;

#[cfg(not(target_arch = "wasm32"))]
use layercraft_config::ProfileDirLoadReport;
use layercraft_config::{
    ActiveProfiles, ConfigError, ModifierOverrides, ModifierRole, PrintConfig, ProfileApplyResult,
    ProfileError, ProfileKind, ProfileLibrary, ProfileSummary, ResolvedProfile,
};
use layercraft_gcode::{GcodeError, GcodeOutput, GcodeReport, emit_gcode, evaluate_gcode};
use layercraft_geom::{Aabb3, GeomError, TriangleMesh};
use layercraft_io::{IoError, read_3mf_bytes, read_stl_bytes};
use layercraft_mesh::{
    MeshError, MeshInspection, ModifierPrimitive, ModifierTransform, ObjectTransform, SampleShape,
    auto_arrange_footprints, build_modifier_primitive, build_sample_shape, inspect_mesh,
    prepare_modifier_mesh, prepare_transformed_object,
};
use layercraft_slice::{
    ModifierVolume, SliceError, SlicedPlate, slice_prepared_mesh_with_modifiers,
    slice_single_mesh_with_modifiers,
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
    #[error("no mesh loaded on build plate")]
    NoMeshLoaded,
    #[error("plate object id {0} not found")]
    ObjectNotFound(u64),
    #[error("plate modifier id {0} not found")]
    ModifierNotFound(u64),
    #[error("plate has not been sliced yet")]
    NotSliced,
    #[error("layer index {index} out of range (plate has {count} layers)")]
    LayerOutOfRange { index: usize, count: usize },
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
    Io(#[from] IoError),
}

/// Stored plate object entry with raw mesh, transform, and prepared world-space mesh.
#[derive(Debug, Clone)]
pub struct PlateObjectEntry {
    pub id: u64,
    pub name: String,
    pub raw_mesh: TriangleMesh,
    pub transform: ObjectTransform,
    pub prepared_mesh: TriangleMesh,
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
    pub raw_mesh: TriangleMesh,
    pub transform: ModifierTransform,
    pub prepared_mesh: TriangleMesh,
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
            mesh: self.prepared_mesh.clone(),
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

/// Stateful slicing session driving CLI, WASM, GUI, and MCP frontends.
#[derive(Debug)]
pub struct Session {
    pub config: PrintConfig,
    pub profiles: ProfileLibrary,
    pub active_profiles: ActiveProfiles,
    pub objects: Vec<PlateObjectEntry>,
    pub next_object_id: u64,
    pub modifiers: Vec<PlateModifierEntry>,
    pub next_modifier_id: u64,
    pub mesh: Option<TriangleMesh>,
    pub prepared_mesh: Option<TriangleMesh>,
    pub sliced: Option<SlicedPlate>,
    pub last_gcode: Option<GcodeOutput>,
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
            sliced: None,
            last_gcode: None,
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
            name: "gcode.evaluate",
            summary: "Evaluate physical invariants and safety metrics on a G-code string.",
            params_schema: schema_val::<EvaluateGcodeParams>(),
            result_schema: schema_val::<GcodeReport>(),
        },
    ];
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

fn rebuild_plate_mesh(session: &mut Session) -> Result<Option<MeshInspection>, EngineError> {
    session.sliced = None;
    session.last_gcode = None;
    if session.objects.is_empty() {
        session.mesh = None;
        session.prepared_mesh = None;
        return Ok(None);
    }
    let raw_list: Vec<TriangleMesh> = session.objects.iter().map(|o| o.raw_mesh.clone()).collect();
    let prep_list: Vec<TriangleMesh> = session
        .objects
        .iter()
        .map(|o| o.prepared_mesh.clone())
        .collect();
    let merged_raw = TriangleMesh::merge(&raw_list)?;
    let merged_prep = TriangleMesh::merge(&prep_list)?;
    let insp = inspect_mesh(&merged_prep);
    session.mesh = Some(merged_raw);
    session.prepared_mesh = Some(merged_prep);
    Ok(Some(insp))
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
        obj.prepared_mesh = prepare_transformed_object(&obj.raw_mesh, obj.transform)?;
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
        raw_mesh: mesh,
        transform,
        prepared_mesh: prepared,
    });
    if append && session.objects.len() > 1 {
        auto_arrange_session_objects(session, 6.0)?;
    }
    rebuild_plate_mesh(session)?.ok_or(EngineError::NoMeshLoaded)
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
    let prepared_mesh = prepare_modifier_mesh(&raw_mesh, transform)?;
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
        obj.prepared_mesh = prepare_transformed_object(&obj.raw_mesh, obj.transform)?;
        let _ = rebuild_plate_mesh(session)?;
    } else {
        session.sliced = None;
        session.last_gcode = None;
    }
    Ok(())
}

/// Execute a named command with JSON parameters against `session`.
pub fn execute(session: &mut Session, name: &str, params: Value) -> Result<Value, EngineError> {
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
            let mesh = read_3mf_bytes(&p.bytes)?;
            let label = p.name.unwrap_or_else(|| "3MF Part".to_owned());
            let insp = add_plate_object(session, label, mesh, p.append)?;
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
        "plate.inspect" => {
            let mesh = session
                .prepared_mesh
                .as_ref()
                .or(session.mesh.as_ref())
                .ok_or(EngineError::NoMeshLoaded)?;
            Ok(serde_json::to_value(inspect_mesh(mesh))?)
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
            entry.prepared_mesh = next_prep;
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
            let raw = read_3mf_bytes(&p.bytes)?;
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
            entry.prepared_mesh = next_prep;
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
        "slice.run" => {
            let mod_vols: Vec<ModifierVolume> = session
                .modifiers
                .iter()
                .map(PlateModifierEntry::to_volume)
                .collect();
            let plate = if let Some(prepared) = session.prepared_mesh.as_ref() {
                slice_prepared_mesh_with_modifiers(prepared, &session.config, &mod_vols)?
            } else if let Some(raw) = session.mesh.as_ref() {
                slice_single_mesh_with_modifiers(raw, &session.config, &mod_vols)?
            } else {
                return Err(EngineError::NoMeshLoaded);
            };
            let total_paths = plate.layers.iter().map(|l| l.paths.len()).sum();
            let res = SliceSummaryResult {
                layer_count: plate.layers.len(),
                total_paths,
            };
            session.sliced = Some(plate);
            session.last_gcode = None;
            Ok(serde_json::to_value(res)?)
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
        other => Err(EngineError::UnknownCommand(other.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use layercraft_geom::{Point3, Triangle};
    use layercraft_io::write_stl_binary;

    fn sample_cube_bytes() -> Vec<u8> {
        let s = 20.0;
        let tris = [
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
        .collect();
        write_stl_binary(&TriangleMesh::new(tris).unwrap())
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
}
