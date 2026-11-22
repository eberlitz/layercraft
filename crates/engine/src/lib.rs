//! Stateful slicing session and schema-typed command registry (`everything is a command`).

use std::collections::BTreeMap;

use layercraft_config::{ConfigError, PrintConfig};
use layercraft_gcode::{GcodeError, GcodeOutput, GcodeReport, emit_gcode, evaluate_gcode};
use layercraft_geom::{GeomError, TriangleMesh};
use layercraft_io::{IoError, read_3mf_bytes, read_stl_bytes};
use layercraft_mesh::{
    MeshError, MeshInspection, SampleShape, build_sample_shape, inspect_mesh, prepare_for_plate,
};
use layercraft_slice::{SliceError, SlicedPlate, slice_single_mesh};
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
    Slice(#[from] SliceError),
    #[error(transparent)]
    Gcode(#[from] GcodeError),
    #[error(transparent)]
    Io(#[from] IoError),
}

/// Stateful slicing session driving CLI, WASM, GUI, and MCP frontends.
#[derive(Debug, Default)]
pub struct Session {
    pub config: PrintConfig,
    pub mesh: Option<TriangleMesh>,
    pub prepared_mesh: Option<TriangleMesh>,
    pub sliced: Option<SlicedPlate>,
    pub last_gcode: Option<GcodeOutput>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Add3mfBytesParams {
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddSampleShapeParams {
    pub shape: SampleShape,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddMeshFileParams {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SetConfigParams {
    pub config: PrintConfig,
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
    cmds
}

fn schema_val<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

fn set_loaded_mesh(
    session: &mut Session,
    mesh: TriangleMesh,
) -> Result<MeshInspection, EngineError> {
    let prepared = prepare_for_plate(
        &mesh,
        session.config.machine.bed_width_mm,
        session.config.machine.bed_depth_mm,
    )?;
    let insp = inspect_mesh(&prepared);
    session.mesh = Some(mesh);
    session.prepared_mesh = Some(prepared);
    session.sliced = None;
    session.last_gcode = None;
    Ok(insp)
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
            session.mesh = None;
            session.prepared_mesh = None;
            session.sliced = None;
            session.last_gcode = None;
            Ok(serde_json::to_value(OkResult { ok: true })?)
        }
        "plate.add_stl_bytes" => {
            let p: AddStlBytesParams = serde_json::from_value(params)?;
            let mesh = read_stl_bytes(&p.bytes)?;
            let insp = set_loaded_mesh(session, mesh)?;
            Ok(serde_json::to_value(insp)?)
        }
        "plate.add_3mf_bytes" => {
            let p: Add3mfBytesParams = serde_json::from_value(params)?;
            let mesh = read_3mf_bytes(&p.bytes)?;
            let insp = set_loaded_mesh(session, mesh)?;
            Ok(serde_json::to_value(insp)?)
        }
        "plate.add_sample_shape" => {
            let p: AddSampleShapeParams = serde_json::from_value(params)?;
            let mesh = build_sample_shape(p.shape)?;
            let insp = set_loaded_mesh(session, mesh)?;
            Ok(serde_json::to_value(insp)?)
        }
        #[cfg(not(target_arch = "wasm32"))]
        "plate.add_mesh_file" => {
            let p: AddMeshFileParams = serde_json::from_value(params)?;
            let mesh = layercraft_io::read_mesh_file(std::path::Path::new(&p.path))?;
            let insp = set_loaded_mesh(session, mesh)?;
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
        "config.get" => Ok(serde_json::to_value(&session.config)?),
        "config.set" => {
            let p: SetConfigParams = serde_json::from_value(params)?;
            p.config.validate()?;
            session.config = p.config;
            if let Some(raw) = session.mesh.as_ref() {
                session.prepared_mesh = Some(prepare_for_plate(
                    raw,
                    session.config.machine.bed_width_mm,
                    session.config.machine.bed_depth_mm,
                )?);
            }
            session.sliced = None;
            session.last_gcode = None;
            Ok(serde_json::to_value(&session.config)?)
        }
        "slice.run" => {
            let mesh = session
                .prepared_mesh
                .as_ref()
                .or(session.mesh.as_ref())
                .ok_or(EngineError::NoMeshLoaded)?;
            let plate = slice_single_mesh(mesh, &session.config)?;
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
}
