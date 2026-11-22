//! Per-layer parameter resolution and disjoint 2D region splitting for modifier volumes.

use layercraft_clip::{difference, intersection, union};
use layercraft_config::{
    HeightRangeModifier, InfillPattern, IroningType, ModifierOverrides, ModifierRole, PrintConfig,
};
use layercraft_geom::{ExPolygon, TriangleMesh};
use layercraft_mesh::{RawSliceLayer, slice_mesh_at_z_planes};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::SliceError;

/// A world-space 3D volume placed on the build plate that modifies slicing behavior inside its intersection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModifierVolume {
    pub id: u64,
    pub name: String,
    pub role: ModifierRole,
    pub mesh: TriangleMesh,
    pub overrides: ModifierOverrides,
}

/// Scalar process parameters that may vary by height range or 2D modifier region.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EffectiveLayerParams {
    pub sparse_infill_density: f64,
    pub sparse_infill_pattern: InfillPattern,
    pub wall_loops: u16,
    pub top_shell_layers: u16,
    pub bottom_shell_layers: u16,
    pub ironing_mode: IroningType,
    pub infill_speed_mm_s: f64,
    pub outer_wall_speed_mm_s: f64,
    pub inner_wall_speed_mm_s: f64,
}

impl EffectiveLayerParams {
    pub(crate) fn from_config(cfg: &PrintConfig) -> Self {
        Self {
            sparse_infill_density: cfg.process.sparse_infill_density,
            sparse_infill_pattern: cfg.process.sparse_infill_pattern,
            wall_loops: cfg.process.wall_loops,
            top_shell_layers: cfg.process.top_shell_layers,
            bottom_shell_layers: cfg.process.bottom_shell_layers,
            ironing_mode: cfg.process.ironing.mode,
            infill_speed_mm_s: cfg.process.infill_speed_mm_s,
            outer_wall_speed_mm_s: cfg.process.outer_wall_speed_mm_s,
            inner_wall_speed_mm_s: cfg.process.inner_wall_speed_mm_s,
        }
    }

    pub(crate) fn apply_overrides(&mut self, ov: &ModifierOverrides) {
        if let Some(v) = ov.sparse_infill_density {
            self.sparse_infill_density = v;
        }
        if let Some(v) = ov.sparse_infill_pattern {
            self.sparse_infill_pattern = v;
        }
        if let Some(v) = ov.wall_loops {
            self.wall_loops = v;
        }
        if let Some(v) = ov.top_shell_layers {
            self.top_shell_layers = v;
        }
        if let Some(v) = ov.bottom_shell_layers {
            self.bottom_shell_layers = v;
        }
        if let Some(v) = ov.ironing_mode {
            self.ironing_mode = v;
        }
        if let Some(v) = ov.infill_speed_mm_s {
            self.infill_speed_mm_s = v;
        }
        if let Some(v) = ov.outer_wall_speed_mm_s {
            self.outer_wall_speed_mm_s = v;
        }
        if let Some(v) = ov.inner_wall_speed_mm_s {
            self.inner_wall_speed_mm_s = v;
        }
    }

    pub(crate) fn apply_height_ranges(&mut self, ranges: &[HeightRangeModifier], z_slice_mm: f64) {
        for hr in ranges {
            if z_slice_mm >= hr.min_z_mm - 1e-9 && z_slice_mm <= hr.max_z_mm + 1e-9 {
                self.apply_overrides(&hr.overrides);
            }
        }
    }

    pub(crate) fn to_layer_config(&self, base: &PrintConfig) -> PrintConfig {
        let mut cfg = base.clone();
        cfg.process.sparse_infill_density = self.sparse_infill_density;
        cfg.process.sparse_infill_pattern = self.sparse_infill_pattern;
        cfg.process.wall_loops = self.wall_loops;
        cfg.process.top_shell_layers = self.top_shell_layers;
        cfg.process.bottom_shell_layers = self.bottom_shell_layers;
        cfg.process.ironing.mode = self.ironing_mode;
        cfg.process.infill_speed_mm_s = self.infill_speed_mm_s;
        cfg.process.outer_wall_speed_mm_s = self.outer_wall_speed_mm_s;
        cfg.process.inner_wall_speed_mm_s = self.inner_wall_speed_mm_s;
        cfg
    }
}

pub(crate) struct ParamModifierSlices {
    pub per_layer_regions: Vec<Vec<ExPolygon>>,
    pub overrides: ModifierOverrides,
}

pub(crate) struct SlicedModifiers {
    pub param_mods: Vec<ParamModifierSlices>,
    pub blocker_masks: Vec<Vec<ExPolygon>>,
    pub enforcer_masks: Vec<Vec<ExPolygon>>,
}

impl SlicedModifiers {
    pub(crate) fn active_param_mods(
        &self,
        layer_idx: usize,
        pred: impl Fn(&ModifierOverrides) -> bool,
    ) -> Vec<(&[ExPolygon], &ModifierOverrides)> {
        let mut out = Vec::new();
        for pm in &self.param_mods {
            if pred(&pm.overrides)
                && let Some(regs) = pm.per_layer_regions.get(layer_idx)
                && !regs.is_empty()
            {
                out.push((regs.as_slice(), &pm.overrides));
            }
        }
        out
    }
}

/// Slice all modifier volumes against the Z planes of `raw_layers`.
pub(crate) fn slice_modifier_volumes(
    modifiers: &[ModifierVolume],
    raw_layers: &[RawSliceLayer],
) -> Result<SlicedModifiers, SliceError> {
    let n = raw_layers.len();
    let z_planes: Vec<f64> = raw_layers.iter().map(|r| r.plane.z_slice_mm).collect();
    let mut param_mods = Vec::new();
    let mut blocker_raw: Vec<Vec<ExPolygon>> = vec![Vec::new(); n];
    let mut enforcer_raw: Vec<Vec<ExPolygon>> = vec![Vec::new(); n];

    for m in modifiers {
        let sliced = slice_mesh_at_z_planes(&m.mesh, &z_planes)?;
        match m.role {
            ModifierRole::Modifier => {
                if !m.overrides.is_empty() {
                    param_mods.push(ParamModifierSlices {
                        per_layer_regions: sliced,
                        overrides: m.overrides.clone(),
                    });
                }
            }
            ModifierRole::SupportBlocker => {
                for (dst, src) in blocker_raw.iter_mut().zip(sliced) {
                    if !src.is_empty() {
                        dst.extend(src);
                    }
                }
            }
            ModifierRole::SupportEnforcer => {
                for (dst, src) in enforcer_raw.iter_mut().zip(sliced) {
                    if !src.is_empty() {
                        dst.extend(src);
                    }
                }
            }
        }
    }

    for slot in &mut blocker_raw {
        if slot.len() > 1 {
            *slot = union(slot)?;
        }
    }
    for slot in &mut enforcer_raw {
        if slot.len() > 1 {
            *slot = union(slot)?;
        }
    }

    Ok(SlicedModifiers {
        param_mods,
        blocker_masks: blocker_raw,
        enforcer_masks: enforcer_raw,
    })
}

/// Partition `base_regions` into pairwise-disjoint sub-regions with merged parameters.
///
/// Later entries in `active_mods` take precedence over earlier entries on overlapping areas.
pub(crate) fn partition_regions(
    base_regions: &[ExPolygon],
    base_params: &EffectiveLayerParams,
    active_mods: &[(&[ExPolygon], &ModifierOverrides)],
) -> Result<Vec<(Vec<ExPolygon>, EffectiveLayerParams)>, SliceError> {
    if base_regions.is_empty() {
        return Ok(Vec::new());
    }
    if active_mods
        .iter()
        .all(|(polys, ov)| polys.is_empty() || ov.is_empty())
    {
        return Ok(vec![(base_regions.to_vec(), base_params.clone())]);
    }

    let mut buckets: Vec<(Vec<ExPolygon>, EffectiveLayerParams)> =
        vec![(base_regions.to_vec(), base_params.clone())];

    for &(mod_polys, ov) in active_mods {
        if mod_polys.is_empty() || ov.is_empty() {
            continue;
        }
        let mut next_buckets = Vec::with_capacity(buckets.len().saturating_mul(2));
        for (b_regs, b_params) in buckets {
            let inside = intersection(&b_regs, mod_polys)?;
            let outside = difference(&b_regs, mod_polys)?;
            if !outside.is_empty() {
                next_buckets.push((outside, b_params.clone()));
            }
            if !inside.is_empty() {
                let mut modified = b_params;
                modified.apply_overrides(ov);
                next_buckets.push((inside, modified));
            }
        }
        buckets = next_buckets;
    }

    Ok(buckets)
}
