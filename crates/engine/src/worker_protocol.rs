//! Serializable Web Worker request/event protocol shared between UI and background WASM workers.

use layercraft_config::{ModifierOverrides, PrintConfig};
use layercraft_gcode::{GcodeOutput, emit_gcode};
use layercraft_geom::TriangleMesh;
use layercraft_slice::{
    ModifierVolume, PlateSliceObject, SliceError, SliceProgress, SlicedPlate,
    slice_plate_objects_with_progress, slice_prepared_mesh_with_progress,
};
use serde::{Deserialize, Serialize};

/// Owned build-plate object entry transferred to a background slice worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SliceJobObject {
    pub id: u64,
    pub name: String,
    pub prepared_mesh: TriangleMesh,
    pub overrides: ModifierOverrides,
}

/// Complete serializable slicing job sent from UI main thread to worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SliceJobRequest {
    pub job_id: u64,
    pub config: PrintConfig,
    pub objects: Vec<SliceJobObject>,
    pub prepared_mesh: Option<TriangleMesh>,
    pub modifiers: Vec<ModifierVolume>,
}

/// Asynchronous messages streamed from the slicing worker back to UI main thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SliceJobEvent {
    Ready,
    Progress {
        job_id: u64,
        completed_layers: usize,
        total_layers: usize,
    },
    Done {
        job_id: u64,
        elapsed_ms: f64,
        plate: SlicedPlate,
        gcode: GcodeOutput,
    },
    Error {
        job_id: u64,
        message: String,
    },
}

/// Execute a deserialized [`SliceJobRequest`] and emit G-code, reporting progress and cancellation.
pub fn execute_slice_job(
    req: &SliceJobRequest,
    progress: Option<&SliceProgress>,
) -> Result<(SlicedPlate, GcodeOutput), String> {
    if progress.is_some_and(SliceProgress::is_cancelled) {
        return Err(SliceError::Cancelled.to_string());
    }
    let plate = if !req.objects.is_empty() {
        let plate_objs: Vec<PlateSliceObject<'_>> = req
            .objects
            .iter()
            .map(|o| PlateSliceObject {
                id: o.id,
                name: o.name.as_str(),
                prepared_mesh: &o.prepared_mesh,
                overrides: &o.overrides,
            })
            .collect();
        slice_plate_objects_with_progress(&plate_objs, &req.config, &req.modifiers, progress)
            .map_err(|e| e.to_string())?
    } else if let Some(prep) = req.prepared_mesh.as_ref() {
        slice_prepared_mesh_with_progress(prep, &req.config, &req.modifiers, progress)
            .map_err(|e| e.to_string())?
    } else {
        return Err("no mesh loaded on build plate".to_owned());
    };

    if progress.is_some_and(SliceProgress::is_cancelled) {
        return Err(SliceError::Cancelled.to_string());
    }
    let gcode = emit_gcode(&plate, &req.config).map_err(|e| e.to_string())?;
    if progress.is_some_and(SliceProgress::is_cancelled) {
        return Err(SliceError::Cancelled.to_string());
    }
    Ok((plate, gcode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use layercraft_mesh::{SampleShape, build_sample_shape, prepare_for_plate};
    use std::sync::{Arc, Mutex};

    #[test]
    fn worker_protocol_roundtrips_and_executes_slice_job() {
        let mesh = build_sample_shape(SampleShape::Cube20).unwrap();
        let prepared = prepare_for_plate(&mesh, 250.0, 250.0).unwrap();
        let req = SliceJobRequest {
            job_id: 42,
            config: PrintConfig::default(),
            objects: vec![SliceJobObject {
                id: 7,
                name: "Cube20".to_owned(),
                prepared_mesh: prepared,
                overrides: ModifierOverrides::default(),
            }],
            prepared_mesh: None,
            modifiers: Vec::new(),
        };
        let req_json = serde_json::to_string(&req).unwrap();
        let decoded_req: SliceJobRequest = serde_json::from_str(&req_json).unwrap();

        let updates = Arc::new(Mutex::new(Vec::<(usize, usize)>::new()));
        let updates_cb = Arc::clone(&updates);
        let progress = SliceProgress::with_callback(move |done, total| {
            if let Ok(mut g) = updates_cb.lock() {
                g.push((done, total));
            }
        });

        let (plate, gcode) = execute_slice_job(&decoded_req, Some(&progress)).unwrap();
        assert!(!plate.layers.is_empty());
        assert!(!gcode.gcode.is_empty());
        assert_eq!(
            updates.lock().unwrap().last().copied(),
            Some((plate.layers.len(), plate.layers.len()))
        );

        let ev = SliceJobEvent::Done {
            job_id: decoded_req.job_id,
            elapsed_ms: 12.5,
            plate,
            gcode,
        };
        let ev_json = serde_json::to_string(&ev).unwrap();
        let decoded_ev: SliceJobEvent = serde_json::from_str(&ev_json).unwrap();
        assert!(matches!(decoded_ev, SliceJobEvent::Done { job_id: 42, .. }));
    }
}
