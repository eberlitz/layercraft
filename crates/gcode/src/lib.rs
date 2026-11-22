//! Volumetric stadium-bead extrusion, G-code emission (Klipper/Marlin),
//! and physical invariant evaluation + 2D raster IoU comparison.

use std::collections::BTreeMap;
use std::f64::consts::PI;
use std::fmt::Write as _;

use layercraft_config::{GcodeFlavor, PrintConfig, SeamPosition};
use layercraft_geom::Point2;
pub use layercraft_slice::bead_area_mm2;
use layercraft_slice::{ExtrusionPath, ExtrusionRole, SlicedPlate};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors produced during G-code emission or evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GcodeError {
    /// Failed while formatting output text.
    #[error("failed formatting G-code output")]
    FormatError,
    /// Invalid config passed to emitter or evaluator.
    #[error("invalid G-code parameter: {0}")]
    InvalidParameter(&'static str),
}

/// Template variable context for start, layer-change, and end G-code macros.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemplateVars {
    pub nozzle_temperature: u16,
    pub first_layer_temperature: u16,
    pub bed_temperature: u16,
    pub first_layer_bed_temperature: u16,
    pub layer_num: usize,
    pub layer_z: f64,
    pub layer_height: f64,
    #[serde(default)]
    pub first_layer_height: f64,
    pub total_layer_count: usize,
    pub max_z: f64,
    pub bed_width: f64,
    pub bed_depth: f64,
    pub bed_height: f64,
    pub nozzle_diameter: f64,
    pub filament_diameter: f64,
}

fn format_template_f64(val: f64) -> String {
    let s = format!("{val:.3}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed == "-0" || trimmed.is_empty() {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn lookup_template_var(key: &str, vars: &TemplateVars) -> Option<String> {
    match key {
        "nozzle_temperature" => Some(vars.nozzle_temperature.to_string()),
        "first_layer_temperature" | "nozzle_temperature_initial_layer" => {
            Some(vars.first_layer_temperature.to_string())
        }
        "bed_temperature" | "hot_plate_temp" => Some(vars.bed_temperature.to_string()),
        "first_layer_bed_temperature"
        | "bed_temperature_initial_layer"
        | "bed_temperature_initial_layer_single"
        | "hot_plate_temp_initial_layer" => Some(vars.first_layer_bed_temperature.to_string()),
        "layer_num" => Some(vars.layer_num.to_string()),
        "layer_z" => Some(format_template_f64(vars.layer_z)),
        "layer_height" => Some(format_template_f64(vars.layer_height)),
        "initial_layer_print_height" => Some(format_template_f64(vars.first_layer_height)),
        "total_layer_count" | "total_layer_number" => Some(vars.total_layer_count.to_string()),
        "max_z" | "max_layer_z" => Some(format_template_f64(vars.max_z)),
        "bed_width" => Some(format_template_f64(vars.bed_width)),
        "bed_depth" => Some(format_template_f64(vars.bed_depth)),
        "bed_height" => Some(format_template_f64(vars.bed_height)),
        "nozzle_diameter" => Some(format_template_f64(vars.nozzle_diameter)),
        "filament_diameter" => Some(format_template_f64(vars.filament_diameter)),
        _ => None,
    }
}

/// Single-pass expansion of LayerCraft `[placeholder]` macros in G-code templates.
/// Supports Orca/Bambu single-extruder aliases and `[var[0]]` / `[var[initial_extruder]]`
/// indexed syntax while preserving unknown `[...]` tokens verbatim.
pub fn expand_gcode_template(template: &str, vars: &TemplateVars) -> String {
    let normalized = template.replace("\r\n", "\n");
    let mut out = String::with_capacity(normalized.len() + 32);
    let mut cursor = 0usize;

    while let Some(slice) = normalized.get(cursor..) {
        let Some(rel_open) = slice.find('[') else {
            out.push_str(slice);
            break;
        };
        if let Some(prefix) = slice.get(..rel_open) {
            out.push_str(prefix);
        }
        let open_idx = cursor + rel_open;
        let after_open = open_idx + 1;
        let rest = normalized.get(after_open..).unwrap_or("");
        let close_pos = rest.find(']');
        let newline_pos = rest.find('\n');
        let nested_open = rest.find('[');

        if let Some(no) = nested_open
            && no > 0
            && newline_pos.is_none_or(|nl| no < nl)
            && let Some(tail) = rest.get(no..)
        {
            let suffix = if tail.starts_with("[0]]") {
                Some("[0]]")
            } else if tail.starts_with("[initial_extruder]]") {
                Some("[initial_extruder]]")
            } else {
                None
            };
            if let Some(suf) = suffix
                && let Some(key) = rest.get(..no)
                && let Some(val) = lookup_template_var(key, vars)
            {
                out.push_str(&val);
                cursor = after_open + no + suf.len();
                continue;
            }
        }

        let valid_close = close_pos.filter(|&c| {
            newline_pos.is_none_or(|nl| c < nl) && nested_open.is_none_or(|no| c < no)
        });

        if let Some(rel_close) = valid_close {
            let key = rest.get(..rel_close).unwrap_or("");
            if let Some(val) = lookup_template_var(key, vars) {
                out.push_str(&val);
                cursor = after_open + rel_close + 1;
                continue;
            }
        }

        out.push('[');
        cursor = after_open;
    }

    out
}

/// Filament cross-sectional area (`mm^2`) for a round filament of diameter `d`.
#[inline]
pub fn filament_area_mm2(filament_diameter_mm: f64) -> f64 {
    let r = 0.5 * filament_diameter_mm;
    PI * r * r
}

/// Filament linear feed (`mm`) required for an extrusion segment of length `len_mm`.
#[inline]
pub fn extrusion_delta_mm(
    len_mm: f64,
    width_mm: f64,
    height_mm: f64,
    filament_diameter_mm: f64,
) -> f64 {
    len_mm * bead_area_mm2(width_mm, height_mm) / filament_area_mm2(filament_diameter_mm)
}

/// Feature breakdown statistics emitted alongside G-code.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct FeatureStats {
    pub volume_mm3: f64,
    pub weight_g: f64,
    pub estimated_time_s: f64,
}

/// Print-level summary produced by G-code emission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PrintStatistics {
    pub layer_count: usize,
    pub max_z_mm: f64,
    pub filament_length_mm: f64,
    pub filament_volume_mm3: f64,
    pub filament_weight_g: f64,
    pub travel_distance_mm: f64,
    pub estimated_time_s: f64,
    pub features: BTreeMap<String, FeatureStats>,
}

/// Complete G-code artifact and associated physical statistics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GcodeOutput {
    pub gcode: String,
    pub stats: PrintStatistics,
}

fn make_template_vars(
    config: &PrintConfig,
    layer_num: usize,
    layer_z: f64,
    layer_height: f64,
    total_layer_count: usize,
    max_z: f64,
) -> TemplateVars {
    TemplateVars {
        nozzle_temperature: config.filament.nozzle_temp_c,
        first_layer_temperature: config.filament.effective_first_layer_nozzle_temp_c(),
        bed_temperature: config.filament.bed_temp_c,
        first_layer_bed_temperature: config.filament.effective_first_layer_bed_temp_c(),
        layer_num,
        layer_z,
        layer_height,
        first_layer_height: config.process.first_layer_height_mm,
        total_layer_count,
        max_z,
        bed_width: config.machine.bed_width_mm,
        bed_depth: config.machine.bed_depth_mm,
        bed_height: config.machine.bed_height_mm,
        nozzle_diameter: config.machine.nozzle_diameter_mm,
        filament_diameter: config.filament.diameter_mm,
    }
}

fn push_template_block(out: &mut String, expanded: &str) -> Result<(), GcodeError> {
    out.push_str(expanded);
    if !expanded.is_empty() && !expanded.ends_with('\n') {
        out.push('\n');
    }
    Ok(())
}

/// Emit G-code from a [`SlicedPlate`] and [`PrintConfig`].
pub fn emit_gcode(plate: &SlicedPlate, config: &PrintConfig) -> Result<GcodeOutput, GcodeError> {
    for (val, field) in [
        (config.filament.diameter_mm, "filament.diameter_mm"),
        (
            config.filament.max_volumetric_speed_mm3_s,
            "filament.max_volumetric_speed_mm3_s",
        ),
        (
            config.machine.travel_speed_mm_s,
            "machine.travel_speed_mm_s",
        ),
        (
            config.machine.retraction_speed_mm_s,
            "machine.retraction_speed_mm_s",
        ),
        (config.machine.z_hop_speed_mm_s, "machine.z_hop_speed_mm_s"),
    ] {
        if !val.is_finite() || val <= 0.0 {
            return Err(GcodeError::InvalidParameter(field));
        }
    }

    let mut out = String::with_capacity(64 * 1024);
    let fil_area = filament_area_mm2(config.filament.diameter_mm);
    let total_layers = plate.layers.len();
    let max_z = plate.layers.last().map(|l| l.z_top_mm).unwrap_or(0.0);
    let first_nozzle = config.filament.effective_first_layer_nozzle_temp_c();
    let first_bed = config.filament.effective_first_layer_bed_temp_c();
    let first_z = plate
        .layers
        .first()
        .map(|l| l.z_top_mm)
        .unwrap_or(config.process.first_layer_height_mm);
    let first_h = plate
        .layers
        .first()
        .map(|l| l.height_mm)
        .unwrap_or(config.process.first_layer_height_mm);

    writeln!(out, "; generated by LayerCraft 0.1.0").map_err(|_| GcodeError::FormatError)?;
    writeln!(
        out,
        "; flavor: {}",
        match config.machine.flavor {
            GcodeFlavor::Klipper => "klipper",
            GcodeFlavor::Marlin => "marlin",
        }
    )
    .map_err(|_| GcodeError::FormatError)?;
    writeln!(out, "; total layer number: {total_layers}").map_err(|_| GcodeError::FormatError)?;
    writeln!(
        out,
        "; filament_diameter: {:.3}",
        config.filament.diameter_mm
    )
    .map_err(|_| GcodeError::FormatError)?;
    writeln!(
        out,
        "; filament_density: {:.3}",
        config.filament.density_g_cm3
    )
    .map_err(|_| GcodeError::FormatError)?;
    writeln!(out, "; max_z_height: {max_z:.3}").map_err(|_| GcodeError::FormatError)?;

    writeln!(out, ";TYPE:Custom").map_err(|_| GcodeError::FormatError)?;
    writeln!(out, "G21 ; millimeters").map_err(|_| GcodeError::FormatError)?;
    writeln!(out, "G90 ; absolute XYZ").map_err(|_| GcodeError::FormatError)?;
    emit_e_mode(&mut out, config.machine.relative_extrusion)?;
    writeln!(out, "G92 E0").map_err(|_| GcodeError::FormatError)?;

    let mut current_fan_percent: Option<u8> = None;
    if let Some(tpl) = &config.machine.start_gcode {
        let vars = make_template_vars(config, 0, first_z, first_h, total_layers, max_z);
        let expanded = expand_gcode_template(tpl, &vars);
        push_template_block(&mut out, &expanded)?;
        writeln!(out, "G90 ; restore absolute XYZ").map_err(|_| GcodeError::FormatError)?;
        emit_e_mode(&mut out, config.machine.relative_extrusion)?;
        writeln!(out, "G92 E0").map_err(|_| GcodeError::FormatError)?;
    } else {
        writeln!(out, "M140 S{first_bed}").map_err(|_| GcodeError::FormatError)?;
        writeln!(out, "M104 S{first_nozzle}").map_err(|_| GcodeError::FormatError)?;
        writeln!(out, "M190 S{first_bed}").map_err(|_| GcodeError::FormatError)?;
        writeln!(out, "M109 S{first_nozzle}").map_err(|_| GcodeError::FormatError)?;
        writeln!(out, "G28 ; home").map_err(|_| GcodeError::FormatError)?;
        if config.filament.disable_fan_first_layers > 0 {
            writeln!(out, "M107").map_err(|_| GcodeError::FormatError)?;
            current_fan_percent = Some(0);
        }
    }

    let travel_f = config.machine.travel_speed_mm_s * 60.0;
    let retract_len = config.machine.retraction_length_mm;
    let retract_f = config.machine.retraction_speed_mm_s * 60.0;
    let zhop_f = config.machine.z_hop_speed_mm_s * 60.0;

    let mut cur_x = 0.0;
    let mut cur_y = 0.0;
    let mut cur_z = 0.0;
    let mut cur_e = 0.0;
    let mut retracted = false;
    let mut has_extruded = false;
    let mut total_e_mm = 0.0;
    let mut total_travel_mm = 0.0;
    let mut total_time_s = 0.0;
    let mut feature_stats: BTreeMap<String, FeatureStats> = BTreeMap::new();
    let mut current_role: Option<ExtrusionRole> = None;
    let mut prev_layer_seams: Vec<(f64, f64)> = Vec::new();
    let mut last_path_trail: Vec<(f64, f64)> = Vec::new();

    for layer in &plate.layers {
        last_path_trail.clear();
        let mut cur_layer_seams: Vec<(f64, f64)> = Vec::new();

        writeln!(out, ";LAYER_CHANGE").map_err(|_| GcodeError::FormatError)?;
        writeln!(out, ";Z:{:.3}", layer.z_top_mm).map_err(|_| GcodeError::FormatError)?;
        writeln!(out, ";HEIGHT:{:.3}", layer.height_mm).map_err(|_| GcodeError::FormatError)?;

        if let Some(tpl) = &config.machine.layer_change_gcode
            && !tpl.trim().is_empty()
        {
            let vars = make_template_vars(
                config,
                layer.index,
                layer.z_top_mm,
                layer.height_mm,
                total_layers,
                max_z,
            );
            let expanded = expand_gcode_template(tpl, &vars);
            push_template_block(&mut out, &expanded)?;
        }

        if layer.index == 1 {
            if config
                .filament
                .first_layer_nozzle_temp_c
                .is_some_and(|t| t != config.filament.nozzle_temp_c)
            {
                writeln!(out, "M104 S{}", config.filament.nozzle_temp_c)
                    .map_err(|_| GcodeError::FormatError)?;
            }
            if config
                .filament
                .first_layer_bed_temp_c
                .is_some_and(|t| t != config.filament.bed_temp_c)
            {
                writeln!(out, "M140 S{}", config.filament.bed_temp_c)
                    .map_err(|_| GcodeError::FormatError)?;
            }
        }

        if current_fan_percent != Some(layer.fan_percent) {
            if layer.fan_percent == 0 {
                writeln!(out, "M107").map_err(|_| GcodeError::FormatError)?;
            } else {
                let pwm = ((layer.fan_percent as f64) * 255.0 / 100.0)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                writeln!(out, "M106 S{pwm}").map_err(|_| GcodeError::FormatError)?;
            }
            current_fan_percent = Some(layer.fan_percent);
        }

        writeln!(out, "G0 Z{:.3} F{zhop_f:.0}", layer.z_top_mm)
            .map_err(|_| GcodeError::FormatError)?;
        total_time_s += (layer.z_top_mm - cur_z).abs() / config.machine.z_hop_speed_mm_s;
        cur_z = layer.z_top_mm;
        let mut z_hopped = false;

        for (path_idx, path) in layer.paths.iter().enumerate() {
            if path.points.len() < 2 {
                continue;
            }

            let ordered = orient_path_with_seam(
                path,
                layer.index,
                path_idx,
                cur_x,
                cur_y,
                &prev_layer_seams,
                config,
            );
            let Some(&first_pt) = ordered.first() else {
                continue;
            };
            let iter_pts: Vec<Point2> = if path.closed {
                ordered
                    .iter()
                    .copied()
                    .skip(1)
                    .chain(std::iter::once(first_pt))
                    .collect()
            } else {
                ordered.iter().copied().skip(1).collect()
            };
            if !has_nonzero_segment(first_pt, &iter_pts) {
                continue;
            }

            if current_role != Some(path.role) {
                writeln!(out, ";TYPE:{}", path.role.label())
                    .map_err(|_| GcodeError::FormatError)?;
                current_role = Some(path.role);
            }

            let (sx, sy) = first_pt.to_mm();
            if path.closed && path.role == ExtrusionRole::OuterWall {
                cur_layer_seams.push((sx, sy));
            }

            let mut travel_dist = (sx - cur_x).hypot(sy - cur_y);
            if has_extruded
                && travel_dist > config.machine.min_travel_for_retract_mm
                && retract_len > 0.0
                && !retracted
            {
                perform_retract(
                    &mut out,
                    config,
                    &mut cur_x,
                    &mut cur_y,
                    &mut cur_e,
                    &last_path_trail,
                    retract_len,
                    retract_f,
                    &mut total_travel_mm,
                    &mut total_time_s,
                )?;
                retracted = true;
                travel_dist = (sx - cur_x).hypot(sy - cur_y);
            }

            if retracted && config.machine.z_hop_mm > 0.0 && !z_hopped {
                let hop_z = cur_z + config.machine.z_hop_mm;
                writeln!(out, "G0 Z{hop_z:.3} F{zhop_f:.0}")
                    .map_err(|_| GcodeError::FormatError)?;
                total_time_s += config.machine.z_hop_mm / config.machine.z_hop_speed_mm_s;
                cur_z = hop_z;
                z_hopped = true;
            }

            if travel_dist > 1e-4 {
                writeln!(out, "G0 X{sx:.3} Y{sy:.3} F{travel_f:.0}")
                    .map_err(|_| GcodeError::FormatError)?;
                total_travel_mm += travel_dist;
                total_time_s += travel_dist / config.machine.travel_speed_mm_s;
                cur_x = sx;
                cur_y = sy;
            }

            if z_hopped || (cur_z - layer.z_top_mm).abs() > 1e-4 {
                writeln!(out, "G0 Z{:.3} F{zhop_f:.0}", layer.z_top_mm)
                    .map_err(|_| GcodeError::FormatError)?;
                total_time_s += (cur_z - layer.z_top_mm).abs() / config.machine.z_hop_speed_mm_s;
                cur_z = layer.z_top_mm;
                z_hopped = false;
            }

            if retracted && retract_len > 0.0 {
                emit_e_move(
                    &mut out,
                    config.machine.relative_extrusion,
                    &mut cur_e,
                    retract_len,
                    retract_f,
                )?;
                retracted = false;
                total_time_s += retract_len / config.machine.retraction_speed_mm_s;
            }

            let eff_flow = path.flow_ratio.max(0.01);
            let bead_area = bead_area_mm2(path.width_mm, path.height_mm) * eff_flow;
            let eff_speed = if bead_area > 1e-9 {
                path.speed_mm_s
                    .min(config.filament.max_volumetric_speed_mm3_s / bead_area)
            } else {
                path.speed_mm_s
            }
            .max(1e-3);
            let feed = (eff_speed * 60.0).floor().max(1.0);
            let actual_speed = feed / 60.0;

            let f_entry = feature_stats
                .entry(path.role.label().to_owned())
                .or_default();

            let mut seg_prev = (cur_x, cur_y);
            let mut path_trail = Vec::with_capacity(iter_pts.len() + 1);
            path_trail.push(seg_prev);

            for pt in iter_pts {
                let (nx, ny) = pt.to_mm();
                let seg_len = (nx - seg_prev.0).hypot(ny - seg_prev.1);
                if seg_len <= 1e-6 {
                    continue;
                }
                let de = extrusion_delta_mm(
                    seg_len,
                    path.width_mm,
                    path.height_mm,
                    config.filament.diameter_mm,
                ) * eff_flow;
                let e_val = next_e_val(config.machine.relative_extrusion, &mut cur_e, de);
                writeln!(out, "G1 X{nx:.3} Y{ny:.3} E{e_val:.5} F{feed:.0}")
                    .map_err(|_| GcodeError::FormatError)?;

                let vol = de * fil_area;
                let dt = seg_len / actual_speed;
                total_e_mm += de;
                total_time_s += dt;
                f_entry.volume_mm3 += vol;
                f_entry.weight_g += vol * 0.001 * config.filament.density_g_cm3;
                f_entry.estimated_time_s += dt;

                seg_prev = (nx, ny);
                cur_x = nx;
                cur_y = ny;
                path_trail.push((nx, ny));
                has_extruded = true;
            }

            last_path_trail = path_trail;
        }

        if !cur_layer_seams.is_empty() {
            prev_layer_seams = cur_layer_seams;
        }

        if !config.machine.relative_extrusion && cur_e > 100.0 {
            writeln!(out, "G92 E0").map_err(|_| GcodeError::FormatError)?;
            cur_e = 0.0;
        }
    }

    writeln!(out, ";TYPE:Custom").map_err(|_| GcodeError::FormatError)?;
    if let Some(tpl) = &config.machine.end_gcode {
        let last_h = plate
            .layers
            .last()
            .map(|l| l.height_mm)
            .unwrap_or(config.process.layer_height_mm);
        let vars = make_template_vars(
            config,
            total_layers.saturating_sub(1),
            max_z,
            last_h,
            total_layers,
            max_z,
        );
        let expanded = expand_gcode_template(tpl, &vars);
        push_template_block(&mut out, &expanded)?;
    } else {
        writeln!(out, "M107").map_err(|_| GcodeError::FormatError)?;
        writeln!(out, "M104 S0").map_err(|_| GcodeError::FormatError)?;
        writeln!(out, "M140 S0").map_err(|_| GcodeError::FormatError)?;
        if max_z > 0.0 {
            let park_z = (max_z + 5.0).min(config.machine.bed_height_mm);
            if park_z > cur_z + 1e-4 {
                writeln!(out, "G0 Z{park_z:.3} F{zhop_f:.0}")
                    .map_err(|_| GcodeError::FormatError)?;
            }
        }
        writeln!(out, "M84").map_err(|_| GcodeError::FormatError)?;
    }

    let total_vol = total_e_mm * fil_area;
    let total_weight = total_vol * 0.001 * config.filament.density_g_cm3;

    Ok(GcodeOutput {
        gcode: out,
        stats: PrintStatistics {
            layer_count: total_layers,
            max_z_mm: max_z,
            filament_length_mm: total_e_mm,
            filament_volume_mm3: total_vol,
            filament_weight_g: total_weight,
            travel_distance_mm: total_travel_mm,
            estimated_time_s: total_time_s,
            features: feature_stats,
        },
    })
}

fn emit_e_mode(out: &mut String, relative: bool) -> Result<(), GcodeError> {
    if relative {
        writeln!(out, "M83 ; relative extrusion").map_err(|_| GcodeError::FormatError)
    } else {
        writeln!(out, "M82 ; absolute extrusion").map_err(|_| GcodeError::FormatError)
    }
}

#[inline]
fn next_e_val(relative: bool, cur_e: &mut f64, delta_e: f64) -> f64 {
    if relative {
        delta_e
    } else {
        *cur_e += delta_e;
        *cur_e
    }
}

fn emit_e_move(
    out: &mut String,
    relative: bool,
    cur_e: &mut f64,
    delta_e: f64,
    feed_mm_min: f64,
) -> Result<(), GcodeError> {
    let e_val = next_e_val(relative, cur_e, delta_e);
    writeln!(out, "G1 E{e_val:.5} F{feed_mm_min:.0}").map_err(|_| GcodeError::FormatError)
}

fn has_nonzero_segment(first_pt: Point2, rest: &[Point2]) -> bool {
    let mut prev = first_pt.to_mm();
    for pt in rest {
        let cur = pt.to_mm();
        if (cur.0 - prev.0).hypot(cur.1 - prev.1) > 1e-6 {
            return true;
        }
        prev = cur;
    }
    false
}

fn collect_wipe_segments(trail: &[(f64, f64)], max_dist_mm: f64) -> (Vec<(f64, f64, f64)>, f64) {
    let mut segs = Vec::new();
    if trail.len() < 2 || max_dist_mm <= 1e-6 {
        return (segs, 0.0);
    }
    let mut remaining = max_dist_mm;
    let mut total_wiped = 0.0;
    let mut i = trail.len() - 1;
    while i > 0 && remaining > 1e-6 {
        let (Some(&cur), Some(&prev)) = (trail.get(i), trail.get(i - 1)) else {
            break;
        };
        let dx = prev.0 - cur.0;
        let dy = prev.1 - cur.1;
        let len = dx.hypot(dy);
        if len > 1e-6 {
            if len <= remaining + 1e-6 {
                let take = len.min(remaining);
                segs.push((prev.0, prev.1, len));
                remaining -= take;
                total_wiped += len;
            } else {
                let frac = remaining / len;
                let wx = cur.0 + dx * frac;
                let wy = cur.1 + dy * frac;
                segs.push((wx, wy, remaining));
                total_wiped += remaining;
                remaining = 0.0;
            }
        }
        i -= 1;
    }
    (segs, total_wiped)
}

#[allow(clippy::too_many_arguments)]
fn perform_retract(
    out: &mut String,
    config: &PrintConfig,
    cur_x: &mut f64,
    cur_y: &mut f64,
    cur_e: &mut f64,
    trail: &[(f64, f64)],
    retract_len: f64,
    retract_f: f64,
    total_travel_mm: &mut f64,
    total_time_s: &mut f64,
) -> Result<(), GcodeError> {
    if config.machine.wipe_on_retract && config.machine.wipe_distance_mm > 0.0 {
        let wipe_dist = config.machine.wipe_distance_mm;
        let (wipe_segs, total_wiped) = collect_wipe_segments(trail, wipe_dist);
        if total_wiped > 1e-4 && !wipe_segs.is_empty() {
            writeln!(out, ";WIPE_START").map_err(|_| GcodeError::FormatError)?;
            let wipe_e_total = if total_wiped >= wipe_dist - 1e-6 {
                retract_len
            } else {
                retract_len * (total_wiped / wipe_dist)
            };
            let wipe_speed = config
                .machine
                .travel_speed_mm_s
                .min(config.machine.retraction_speed_mm_s * (total_wiped / wipe_e_total.max(1e-6)))
                .max(1e-3);
            let wipe_f = (wipe_speed * 60.0).floor().max(1.0);
            let mut e_emitted = 0.0;
            for (idx, &(wx, wy, seg_len)) in wipe_segs.iter().enumerate() {
                let step_mag = if idx + 1 == wipe_segs.len() {
                    wipe_e_total - e_emitted
                } else {
                    wipe_e_total * (seg_len / total_wiped)
                };
                e_emitted += step_mag;
                let e_val = next_e_val(config.machine.relative_extrusion, cur_e, -step_mag);
                writeln!(out, "G1 X{wx:.3} Y{wy:.3} E{e_val:.5} F{wipe_f:.0}")
                    .map_err(|_| GcodeError::FormatError)?;
                *cur_x = wx;
                *cur_y = wy;
                *total_travel_mm += seg_len;
                *total_time_s += seg_len / (wipe_f / 60.0);
            }
            let in_place_e = retract_len - wipe_e_total;
            if in_place_e > 1e-6 {
                emit_e_move(
                    out,
                    config.machine.relative_extrusion,
                    cur_e,
                    -in_place_e,
                    retract_f,
                )?;
                *total_time_s += in_place_e / config.machine.retraction_speed_mm_s;
            }
            writeln!(out, ";WIPE_END").map_err(|_| GcodeError::FormatError)?;
            return Ok(());
        }
    }

    emit_e_move(
        out,
        config.machine.relative_extrusion,
        cur_e,
        -retract_len,
        retract_f,
    )?;
    *total_time_s += retract_len / config.machine.retraction_speed_mm_s;
    Ok(())
}

fn rotate_closed_points(points: &[Point2], start_idx: usize) -> Vec<Point2> {
    let n = points.len();
    let mut rotated = Vec::with_capacity(n);
    rotated.extend(points.iter().skip(start_idx).copied());
    rotated.extend(points.iter().take(start_idx).copied());
    rotated
}

fn orient_path_nearest(path: &ExtrusionPath, cur_x: f64, cur_y: f64) -> Vec<Point2> {
    let n = path.points.len();
    if n <= 1 {
        return path.points.clone();
    }
    if path.closed {
        let best_idx = closest_vertex_index(&path.points, cur_x, cur_y);
        rotate_closed_points(&path.points, best_idx)
    } else {
        let (Some(first), Some(last)) = (path.points.first(), path.points.last()) else {
            return path.points.clone();
        };
        let (fx, fy) = first.to_mm();
        let (lx, ly) = last.to_mm();
        let df = (fx - cur_x).powi(2) + (fy - cur_y).powi(2);
        let dl = (lx - cur_x).powi(2) + (ly - cur_y).powi(2);
        if dl < df {
            path.points.iter().rev().copied().collect()
        } else {
            path.points.clone()
        }
    }
}

fn closest_vertex_index(points: &[Point2], tx: f64, ty: f64) -> usize {
    let mut best_idx = 0usize;
    let mut best_d2 = f64::INFINITY;
    for (i, pt) in points.iter().enumerate() {
        let (px, py) = pt.to_mm();
        let d2 = (px - tx).powi(2) + (py - ty).powi(2);
        if d2 < best_d2 {
            best_d2 = d2;
            best_idx = i;
        }
    }
    best_idx
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn aligned_seam_index(
    points: &[Point2],
    prev_layer_seams: &[(f64, f64)],
    config: &PrintConfig,
) -> usize {
    let n = points.len();
    if n <= 1 {
        return 0;
    }

    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for pt in points {
        let (px, py) = pt.to_mm();
        min_x = min_x.min(px);
        min_y = min_y.min(py);
        max_x = max_x.max(px);
        max_y = max_y.max(py);
    }
    let diag = (max_x - min_x).hypot(max_y - min_y).max(1.0);

    let (ref_pt, has_prev_seam) = if let Some(best_seam) = prev_layer_seams
        .iter()
        .min_by(|&&(ax, ay), &&(bx, by)| {
            let da = points
                .iter()
                .map(|p| {
                    let (px, py) = p.to_mm();
                    (px - ax).hypot(py - ay)
                })
                .fold(f64::INFINITY, f64::min);
            let db = points
                .iter()
                .map(|p| {
                    let (px, py) = p.to_mm();
                    (px - bx).hypot(py - by)
                })
                .fold(f64::INFINITY, f64::min);
            da.total_cmp(&db)
        })
        .copied()
    {
        (best_seam, true)
    } else {
        (
            (
                0.5 * config.machine.bed_width_mm,
                config.machine.bed_depth_mm,
            ),
            false,
        )
    };

    let (align_w, corner_w) = if has_prev_seam {
        (1.5, 0.5)
    } else {
        (1.0, 1.0)
    };

    let min_chord_mm = (diag * 0.05).clamp(0.15, 0.4);
    let mut best_idx = 0usize;
    let mut best_score = f64::INFINITY;

    for i in 0..n {
        let Some(&cur_pt) = points.get(i) else {
            continue;
        };
        let (cx, cy) = cur_pt.to_mm();
        let (px, py) = chord_neighbor(points, i, -1, min_chord_mm);
        let (nx, ny) = chord_neighbor(points, i, 1, min_chord_mm);

        let (ix, iy) = (cx - px, cy - py);
        let (ox, oy) = (nx - cx, ny - cy);
        let ilen = ix.hypot(iy);
        let olen = ox.hypot(oy);

        let corner_term = if ilen > 1e-6 && olen > 1e-6 {
            let (ux, uy) = (ix / ilen, iy / ilen);
            let (vx, vy) = (ox / olen, oy / olen);
            let dot = (ux * vx + uy * vy).clamp(-1.0, 1.0);
            let cross = ux * vy - uy * vx;
            let sharpness = (0.5 * (1.0 - dot)).clamp(0.0, 1.0);
            if cross < -0.15 {
                -0.8 * sharpness
            } else {
                -0.35 * sharpness
            }
        } else {
            0.0
        };

        let dist_norm = (cx - ref_pt.0).hypot(cy - ref_pt.1) / diag;
        let score = align_w * dist_norm + corner_w * corner_term;
        if score < best_score {
            best_score = score;
            best_idx = i;
        }
    }

    best_idx
}

fn chord_neighbor(points: &[Point2], start: usize, step: isize, min_dist_mm: f64) -> (f64, f64) {
    let n = points.len();
    let Some(start_pt) = points.get(start) else {
        return (0.0, 0.0);
    };
    let mut cur = start;
    let mut acc = 0.0;
    let mut last_pos = start_pt.to_mm();
    for _ in 0..n.min(64) {
        let next = if step > 0 {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        };
        if next == start {
            break;
        }
        let Some(pt) = points.get(next) else {
            break;
        };
        let p = pt.to_mm();
        acc += (p.0 - last_pos.0).hypot(p.1 - last_pos.1);
        last_pos = p;
        cur = next;
        if acc >= min_dist_mm {
            break;
        }
    }
    last_pos
}

fn random_seam_index(points: &[Point2], layer_idx: usize, path_idx: usize) -> usize {
    let n = points.len();
    if n <= 1 {
        return 0;
    }
    let mut total_perim = 0.0;
    let mut cum_dists = Vec::with_capacity(n);
    cum_dists.push(0.0);
    for i in 0..n {
        let (Some(&a), Some(&b)) = (points.get(i), points.get((i + 1) % n)) else {
            continue;
        };
        total_perim += a.distance_mm(b);
        cum_dists.push(total_perim);
    }
    if total_perim <= 1e-6 {
        return 0;
    }

    let seed = ((layer_idx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)) ^ (path_idx as u64);
    let h = splitmix64(seed);
    let u = ((h >> 11) as f64) / ((1u64 << 53) as f64);
    let target = u * total_perim;

    for (i, &d) in cum_dists.iter().take(n).enumerate() {
        if d >= target {
            return i;
        }
    }
    n.saturating_sub(1)
}

fn orient_path_with_seam(
    path: &ExtrusionPath,
    layer_idx: usize,
    path_idx: usize,
    cur_x: f64,
    cur_y: f64,
    prev_layer_seams: &[(f64, f64)],
    config: &PrintConfig,
) -> Vec<Point2> {
    let is_wall = matches!(
        path.role,
        ExtrusionRole::OuterWall | ExtrusionRole::InnerWall
    );
    if !path.closed || !is_wall || path.points.len() <= 1 {
        return orient_path_nearest(path, cur_x, cur_y);
    }

    let start_idx = match config.process.seam_position {
        SeamPosition::Nearest => closest_vertex_index(&path.points, cur_x, cur_y),
        SeamPosition::Back => closest_vertex_index(
            &path.points,
            0.5 * config.machine.bed_width_mm,
            config.machine.bed_depth_mm + 1000.0,
        ),
        SeamPosition::Aligned => aligned_seam_index(&path.points, prev_layer_seams, config),
        SeamPosition::Random => random_seam_index(&path.points, layer_idx, path_idx),
    };
    rotate_closed_points(&path.points, start_idx)
}

/// An extruded 2D XY segment parsed from G-code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GcodeExtrusionSegment {
    pub z_mm: f64,
    pub x0_mm: f64,
    pub y0_mm: f64,
    pub x1_mm: f64,
    pub y1_mm: f64,
    pub volume_mm3: f64,
    pub feature: String,
}

/// Kinematic & geometric safety invariant counters checked on any G-code file.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct SafetyReport {
    pub non_finite_values: usize,
    pub out_of_bounds_moves: usize,
    pub z_regressions: usize,
    pub max_retraction_imbalance_mm: f64,
}

impl SafetyReport {
    /// True if zero safety violations occurred and retraction imbalance stays bounded.
    pub fn is_clean(&self) -> bool {
        self.non_finite_values == 0
            && self.out_of_bounds_moves == 0
            && self.z_regressions == 0
            && self.max_retraction_imbalance_mm <= 5.0
    }
}

/// Physical evaluation report parsed from an arbitrary FDM G-code stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GcodeReport {
    pub active_layer_count: usize,
    pub layer_z_mm: Vec<f64>,
    pub max_z_mm: f64,
    pub total_extrusion_length_mm: f64,
    pub total_filament_volume_mm3: f64,
    pub total_travel_mm: f64,
    pub feature_volumes_mm3: BTreeMap<String, f64>,
    /// `[min_x, min_y, max_x, max_y]` of all positive extrusion moves.
    pub extrusion_bbox_xy: Option<[f64; 4]>,
    pub safety: SafetyReport,
    #[serde(default)]
    pub max_speed_mm_s: f64,
    #[serde(default)]
    pub max_volumetric_flow_mm3_s: f64,
    #[serde(default)]
    pub fan_commands_count: usize,
    #[serde(default)]
    pub max_fan_pwm: u8,
    #[serde(default)]
    pub first_layer_max_fan_pwm: u8,
    #[serde(default)]
    pub z_hop_count: usize,
    #[serde(default)]
    pub wipe_moves_count: usize,
}

/// Evaluate physical invariants from any Marlin/Klipper G-code file (LayerCraft or external oracle).
pub fn evaluate_gcode(
    gcode: &str,
    filament_diameter_mm: f64,
    bed_width_mm: f64,
    bed_depth_mm: f64,
) -> Result<(GcodeReport, Vec<GcodeExtrusionSegment>), GcodeError> {
    if !filament_diameter_mm.is_finite() || filament_diameter_mm <= 0.0 {
        return Err(GcodeError::InvalidParameter("filament_diameter_mm"));
    }
    let fil_area = filament_area_mm2(filament_diameter_mm);

    let mut abs_xyz = true;
    let mut abs_e = true;
    let mut cur_x = 0.0;
    let mut cur_y = 0.0;
    let mut cur_z = 0.0;
    let mut cur_e = 0.0;
    let mut cur_feed_mm_min = 0.0;
    let mut has_xy = false;

    let mut current_feature = "Unknown".to_owned();
    let mut layer_change_count = 0usize;
    let mut retraction_debt_mm = 0.0;
    let mut max_retraction_imbalance_mm = 0.0f64;

    let mut total_e_mm = 0.0;
    let mut total_travel_mm = 0.0;
    let mut feature_volumes_mm3: BTreeMap<String, f64> = BTreeMap::new();
    let mut layer_z_mm: Vec<f64> = Vec::new();
    let mut max_active_z = 0.0f64;
    let mut last_extrusion_z: Option<f64> = None;

    let mut max_speed_mm_s = 0.0f64;
    let mut max_volumetric_flow_mm3_s = 0.0f64;
    let mut fan_commands_count = 0usize;
    let mut max_fan_pwm = 0u8;
    let mut first_layer_max_fan_pwm = 0u8;
    let mut z_hop_count = 0usize;
    let mut wipe_moves_count = 0usize;

    let mut min_ex_x = f64::INFINITY;
    let mut min_ex_y = f64::INFINITY;
    let mut max_ex_x = f64::NEG_INFINITY;
    let mut max_ex_y = f64::NEG_INFINITY;
    let mut has_extrusion = false;

    let mut safety = SafetyReport::default();
    let mut segments = Vec::new();

    for raw_line in gcode.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == ";LAYER_CHANGE" {
            layer_change_count += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(";TYPE:") {
            let label = rest.trim();
            if !label.is_empty() {
                current_feature = label.to_owned();
            }
            continue;
        }
        if trimmed.starts_with(';') {
            continue;
        }

        let code_part = trimmed.split(';').next().unwrap_or("").trim();
        if code_part.is_empty() {
            continue;
        }

        let mut tokens = code_part.split_whitespace();
        let Some(cmd) = tokens.next() else {
            continue;
        };

        match cmd {
            "G90" => abs_xyz = true,
            "G91" => abs_xyz = false,
            "M82" => abs_e = true,
            "M83" => abs_e = false,
            "M106" => {
                fan_commands_count += 1;
                let mut pwm = 255u8;
                for tok in tokens {
                    if let Some(rest) = tok.strip_prefix('S')
                        && let Ok(v) = rest.parse::<f64>()
                        && v.is_finite()
                    {
                        pwm = v.round().clamp(0.0, 255.0) as u8;
                    }
                }
                max_fan_pwm = max_fan_pwm.max(pwm);
                let in_first_layer = if layer_change_count > 0 {
                    layer_change_count == 1
                } else {
                    layer_z_mm.len() <= 1
                };
                if in_first_layer {
                    first_layer_max_fan_pwm = first_layer_max_fan_pwm.max(pwm);
                }
            }
            "M107" => {
                fan_commands_count += 1;
            }
            "G92" => {
                for tok in tokens {
                    let Some(first) = tok.chars().next() else {
                        continue;
                    };
                    let Ok(v) = tok.get(first.len_utf8()..).unwrap_or("").parse::<f64>() else {
                        continue;
                    };
                    if !v.is_finite() {
                        safety.non_finite_values += 1;
                        continue;
                    }
                    match first {
                        'E' => cur_e = v,
                        'X' => cur_x = v,
                        'Y' => cur_y = v,
                        'Z' => cur_z = v,
                        _ => {}
                    }
                }
            }
            "G0" | "G1" => {
                let mut next_x = None;
                let mut next_y = None;
                let mut next_z = None;
                let mut next_e = None;

                for tok in tokens {
                    let Some(first) = tok.chars().next() else {
                        continue;
                    };
                    let val_str = tok.get(first.len_utf8()..).unwrap_or("");
                    let Ok(val) = val_str.parse::<f64>() else {
                        continue;
                    };
                    if !val.is_finite() {
                        safety.non_finite_values += 1;
                        continue;
                    }
                    match first {
                        'X' => next_x = Some(if abs_xyz { val } else { cur_x + val }),
                        'Y' => next_y = Some(if abs_xyz { val } else { cur_y + val }),
                        'Z' => next_z = Some(if abs_xyz { val } else { cur_z + val }),
                        'E' => next_e = Some(val),
                        'F' if val > 0.0 => cur_feed_mm_min = val,
                        _ => {}
                    }
                }

                let tx = next_x.unwrap_or(cur_x);
                let ty = next_y.unwrap_or(cur_y);
                let tz = next_z.unwrap_or(cur_z);

                let de_raw = match next_e {
                    Some(e_val) => {
                        if abs_e {
                            let d = e_val - cur_e;
                            cur_e = e_val;
                            d
                        } else {
                            e_val
                        }
                    }
                    None => 0.0,
                };

                let xy_dist = if has_xy {
                    (tx - cur_x).hypot(ty - cur_y)
                } else {
                    0.0
                };
                let is_custom = current_feature.eq_ignore_ascii_case("Custom");

                if !is_custom
                    && xy_dist <= 1e-5
                    && de_raw.abs() <= 1e-7
                    && next_z.is_some()
                    && tz > cur_z + 1e-4
                    && retraction_debt_mm > 1e-4
                {
                    z_hop_count += 1;
                }

                if de_raw < -1e-7 {
                    retraction_debt_mm += -de_raw;
                    max_retraction_imbalance_mm =
                        max_retraction_imbalance_mm.max(retraction_debt_mm);
                    if !is_custom && xy_dist > 1e-5 {
                        wipe_moves_count += 1;
                        total_travel_mm += xy_dist;
                    }
                } else if de_raw > 1e-7 {
                    let net_de = if retraction_debt_mm > 0.0 {
                        let pay = retraction_debt_mm.min(de_raw);
                        retraction_debt_mm -= pay;
                        de_raw - pay
                    } else {
                        de_raw
                    };

                    if net_de > 1e-7 && xy_dist > 1e-5 && !is_custom {
                        if let Some(prev_z) = last_extrusion_z
                            && tz + 1e-4 < prev_z
                        {
                            safety.z_regressions += 1;
                        }
                        last_extrusion_z = Some(tz);

                        let vol = net_de * fil_area;
                        total_e_mm += net_de;
                        *feature_volumes_mm3
                            .entry(current_feature.clone())
                            .or_insert(0.0) += vol;

                        if cur_feed_mm_min > 0.0 {
                            let speed_mm_s = cur_feed_mm_min / 60.0;
                            let dt = xy_dist / speed_mm_s;
                            if dt > 1e-9 {
                                max_speed_mm_s = max_speed_mm_s.max(speed_mm_s);
                                max_volumetric_flow_mm3_s = max_volumetric_flow_mm3_s.max(vol / dt);
                            }
                        }

                        if tz > 0.0 {
                            max_active_z = max_active_z.max(tz);
                            let already =
                                layer_z_mm.last().is_some_and(|&lz| (lz - tz).abs() < 1e-3);
                            if !already {
                                layer_z_mm.push(tz);
                            }
                        }

                        if has_xy {
                            min_ex_x = min_ex_x.min(cur_x).min(tx);
                            min_ex_y = min_ex_y.min(cur_y).min(ty);
                            max_ex_x = max_ex_x.max(cur_x).max(tx);
                            max_ex_y = max_ex_y.max(cur_y).max(ty);
                            has_extrusion = true;

                            if tx < -1.0
                                || ty < -1.0
                                || tx > bed_width_mm + 1.0
                                || ty > bed_depth_mm + 1.0
                                || cur_x < -1.0
                                || cur_y < -1.0
                                || cur_x > bed_width_mm + 1.0
                                || cur_y > bed_depth_mm + 1.0
                            {
                                safety.out_of_bounds_moves += 1;
                            }

                            segments.push(GcodeExtrusionSegment {
                                z_mm: tz,
                                x0_mm: cur_x,
                                y0_mm: cur_y,
                                x1_mm: tx,
                                y1_mm: ty,
                                volume_mm3: vol,
                                feature: current_feature.clone(),
                            });
                        }
                    }
                } else if xy_dist > 1e-5 {
                    total_travel_mm += xy_dist;
                }

                if next_x.is_some() || next_y.is_some() {
                    has_xy = true;
                }
                cur_x = tx;
                cur_y = ty;
                cur_z = tz;
            }
            _ => {}
        }
    }

    safety.max_retraction_imbalance_mm = max_retraction_imbalance_mm;

    let report = GcodeReport {
        active_layer_count: layer_z_mm.len(),
        layer_z_mm,
        max_z_mm: max_active_z,
        total_extrusion_length_mm: total_e_mm,
        total_filament_volume_mm3: total_e_mm * fil_area,
        total_travel_mm,
        feature_volumes_mm3,
        extrusion_bbox_xy: if has_extrusion {
            Some([min_ex_x, min_ex_y, max_ex_x, max_ex_y])
        } else {
            None
        },
        safety,
        max_speed_mm_s,
        max_volumetric_flow_mm3_s,
        fan_commands_count,
        max_fan_pwm,
        first_layer_max_fan_pwm,
        z_hop_count,
        wipe_moves_count,
    };

    Ok((report, segments))
}

/// Summary of 2D rasterized extrusion coverage comparison between two G-code traces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RasterComparison {
    pub compared_layers: usize,
    pub mean_layer_iou: f64,
    pub min_layer_iou: f64,
}

/// Compare two G-code segment lists by rasterizing swept extrusion beads per matched Z layer.
pub fn compare_layer_iou(
    segs_a: &[GcodeExtrusionSegment],
    segs_b: &[GcodeExtrusionSegment],
    pixel_mm: f64,
    bead_radius_mm: f64,
) -> RasterComparison {
    if segs_a.is_empty() || segs_b.is_empty() || pixel_mm <= 0.0 || bead_radius_mm <= 0.0 {
        return RasterComparison {
            compared_layers: 0,
            mean_layer_iou: 0.0,
            min_layer_iou: 0.0,
        };
    }

    let layers_a = group_by_layer_z(segs_a);
    let layers_b = group_by_layer_z(segs_b);

    let mut ious = Vec::new();
    for (za, sa) in &layers_a {
        // Find matching layer in B within 0.05 mm.
        let Some((_, sb)) = layers_b.iter().find(|(zb, _)| (za - zb).abs() <= 0.05) else {
            continue;
        };
        let iou = single_layer_iou(sa, sb, pixel_mm, bead_radius_mm);
        ious.push(iou);
    }

    if ious.is_empty() {
        return RasterComparison {
            compared_layers: 0,
            mean_layer_iou: 0.0,
            min_layer_iou: 0.0,
        };
    }

    let sum: f64 = ious.iter().sum();
    let min = ious.iter().copied().fold(f64::INFINITY, f64::min);
    RasterComparison {
        compared_layers: ious.len(),
        mean_layer_iou: sum / (ious.len() as f64),
        min_layer_iou: min,
    }
}

fn group_by_layer_z(segs: &[GcodeExtrusionSegment]) -> Vec<(f64, Vec<&GcodeExtrusionSegment>)> {
    let mut map: BTreeMap<i64, Vec<&GcodeExtrusionSegment>> = BTreeMap::new();
    for s in segs {
        let key = (s.z_mm * 1000.0).round() as i64;
        map.entry(key).or_default().push(s);
    }
    map.into_iter()
        .map(|(k, v)| ((k as f64) / 1000.0, v))
        .collect()
}

fn single_layer_iou(
    sa: &[&GcodeExtrusionSegment],
    sb: &[&GcodeExtrusionSegment],
    pixel_mm: f64,
    radius_mm: f64,
) -> f64 {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for s in sa.iter().chain(sb.iter()) {
        min_x = min_x.min(s.x0_mm).min(s.x1_mm);
        min_y = min_y.min(s.y0_mm).min(s.y1_mm);
        max_x = max_x.max(s.x0_mm).max(s.x1_mm);
        max_y = max_y.max(s.y0_mm).max(s.y1_mm);
    }
    let pad = radius_mm + pixel_mm;
    min_x -= pad;
    min_y -= pad;
    max_x += pad;
    max_y += pad;

    let nx = (((max_x - min_x) / pixel_mm).ceil() as usize).clamp(1, 2048);
    let ny = (((max_y - min_y) / pixel_mm).ceil() as usize).clamp(1, 2048);
    let total = nx * ny;
    let mut grid_a = vec![false; total];
    let mut grid_b = vec![false; total];

    rasterize_segments(sa, min_x, min_y, pixel_mm, radius_mm, nx, ny, &mut grid_a);
    rasterize_segments(sb, min_x, min_y, pixel_mm, radius_mm, nx, ny, &mut grid_b);

    let mut inter = 0usize;
    let mut uni = 0usize;
    for (a, b) in grid_a.iter().zip(grid_b.iter()) {
        if *a && *b {
            inter += 1;
            uni += 1;
        } else if *a || *b {
            uni += 1;
        }
    }
    if uni == 0 {
        1.0
    } else {
        (inter as f64) / (uni as f64)
    }
}

#[allow(clippy::too_many_arguments)]
fn rasterize_segments(
    segs: &[&GcodeExtrusionSegment],
    origin_x: f64,
    origin_y: f64,
    pixel_mm: f64,
    radius_mm: f64,
    nx: usize,
    ny: usize,
    grid: &mut [bool],
) {
    let r2 = radius_mm * radius_mm;
    for s in segs {
        let s_min_x = s.x0_mm.min(s.x1_mm) - radius_mm;
        let s_max_x = s.x0_mm.max(s.x1_mm) + radius_mm;
        let s_min_y = s.y0_mm.min(s.y1_mm) - radius_mm;
        let s_max_y = s.y0_mm.max(s.y1_mm) + radius_mm;

        let ix0 =
            (((s_min_x - origin_x) / pixel_mm).floor() as isize).clamp(0, nx as isize - 1) as usize;
        let ix1 =
            (((s_max_x - origin_x) / pixel_mm).ceil() as isize).clamp(0, nx as isize - 1) as usize;
        let iy0 =
            (((s_min_y - origin_y) / pixel_mm).floor() as isize).clamp(0, ny as isize - 1) as usize;
        let iy1 =
            (((s_max_y - origin_y) / pixel_mm).ceil() as isize).clamp(0, ny as isize - 1) as usize;

        let dx = s.x1_mm - s.x0_mm;
        let dy = s.y1_mm - s.y0_mm;
        let len2 = dx * dx + dy * dy;

        for iy in iy0..=iy1 {
            let py = origin_y + ((iy as f64) + 0.5) * pixel_mm;
            for ix in ix0..=ix1 {
                let px = origin_x + ((ix as f64) + 0.5) * pixel_mm;
                let t = if len2 > 1e-12 {
                    (((px - s.x0_mm) * dx + (py - s.y0_mm) * dy) / len2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let cx = s.x0_mm + t * dx;
                let cy = s.y0_mm + t * dy;
                if (px - cx).powi(2) + (py - cy).powi(2) <= r2
                    && let Some(cell) = grid.get_mut(iy * nx + ix)
                {
                    *cell = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use layercraft_geom::{Point3, Triangle, TriangleMesh};
    use layercraft_slice::slice_single_mesh;

    fn make_box(w: f64, d: f64, h: f64) -> TriangleMesh {
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

    fn make_cylinder(r: f64, h: f64, n: usize) -> TriangleMesh {
        let mut tris = Vec::with_capacity(n * 4);
        let cb = Point3::new(0.0, 0.0, 0.0);
        let ct = Point3::new(0.0, 0.0, h);
        for i in 0..n {
            let a0 = std::f64::consts::TAU * (i as f64) / (n as f64);
            let a1 = std::f64::consts::TAU * (((i + 1) % n) as f64) / (n as f64);
            let b0 = Point3::new(r * a0.cos(), r * a0.sin(), 0.0);
            let b1 = Point3::new(r * a1.cos(), r * a1.sin(), 0.0);
            let t0 = Point3::new(r * a0.cos(), r * a0.sin(), h);
            let t1 = Point3::new(r * a1.cos(), r * a1.sin(), h);
            tris.push(Triangle::new(cb, b1, b0));
            tris.push(Triangle::new(ct, t0, t1));
            tris.push(Triangle::new(b0, b1, t1));
            tris.push(Triangle::new(b0, t1, t0));
        }
        TriangleMesh::new(tris).unwrap()
    }

    fn outer_wall_seam_starts(segs: &[GcodeExtrusionSegment]) -> Vec<(f64, f64)> {
        let by_layer = group_by_layer_z(segs);
        by_layer
            .into_iter()
            .filter_map(|(_, lsegs)| {
                lsegs
                    .into_iter()
                    .find(|s| s.feature == "Outer wall")
                    .map(|s| (s.x0_mm, s.y0_mm))
            })
            .collect()
    }

    #[test]
    fn emitted_gcode_roundtrips_through_evaluator() {
        let mesh = make_box(20.0, 20.0, 20.0);
        let cfg = PrintConfig::default();
        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let out = emit_gcode(&plate, &cfg).unwrap();
        let (report, segs) = evaluate_gcode(
            &out.gcode,
            cfg.filament.diameter_mm,
            cfg.machine.bed_width_mm,
            cfg.machine.bed_depth_mm,
        )
        .unwrap();

        assert!(report.safety.is_clean());
        assert_eq!(report.active_layer_count, 100);
        assert!((report.max_z_mm - 20.0).abs() < 1e-3);
        assert!((report.total_filament_volume_mm3 - out.stats.filament_volume_mm3).abs() < 1.0);
        let iou = compare_layer_iou(&segs, &segs, 0.5, 0.25);
        assert!((iou.mean_layer_iou - 1.0).abs() < 1e-6);
    }

    #[test]
    fn seam_positions_behave_accurately() {
        let cyl = make_cylinder(10.0, 2.0, 48);
        let bx = make_box(20.0, 20.0, 2.0);

        // Aligned: < 0.05 mm drift across layers on cylinder & box.
        for mesh in [&cyl, &bx] {
            let mut cfg = PrintConfig::default();
            cfg.process.seam_position = SeamPosition::Aligned;
            let plate = slice_single_mesh(mesh, &cfg).unwrap();
            let out = emit_gcode(&plate, &cfg).unwrap();
            let (_, segs) = evaluate_gcode(&out.gcode, 1.75, 250.0, 250.0).unwrap();
            let starts = outer_wall_seam_starts(&segs);
            assert!(starts.len() >= 8);
            let s0 = starts[0];
            for (li, s) in starts.iter().enumerate() {
                assert!(
                    (s.0 - s0.0).hypot(s.1 - s0.1) < 0.05,
                    "layer {li} seam drifted: {s:?} vs layer0 {s0:?} (all: {starts:?})"
                );
            }
        }

        // Back: picks highest Y on cylinder.
        let mut cfg_back = PrintConfig::default();
        cfg_back.process.seam_position = SeamPosition::Back;
        let plate_back = slice_single_mesh(&cyl, &cfg_back).unwrap();
        let out_back = emit_gcode(&plate_back, &cfg_back).unwrap();
        let (_, segs_back) = evaluate_gcode(&out_back.gcode, 1.75, 250.0, 250.0).unwrap();
        for (_, y) in outer_wall_seam_starts(&segs_back) {
            assert!(y > 134.0, "expected rear seam near Y=135, got {y}");
        }

        // Random: varies across layers and is deterministic across repeated runs.
        let mut cfg_rand = PrintConfig::default();
        cfg_rand.process.seam_position = SeamPosition::Random;
        let plate_r1 = slice_single_mesh(&cyl, &cfg_rand).unwrap();
        let out_r1 = emit_gcode(&plate_r1, &cfg_rand).unwrap();
        let out_r2 = emit_gcode(&plate_r1, &cfg_rand).unwrap();
        assert_eq!(out_r1.gcode, out_r2.gcode);
        let (_, segs_r) = evaluate_gcode(&out_r1.gcode, 1.75, 250.0, 250.0).unwrap();
        let starts_r = outer_wall_seam_starts(&segs_r);
        let distinct_positions = starts_r
            .windows(2)
            .filter(|w| (w[0].0 - w[1].0).hypot(w[0].1 - w[1].1) > 0.5)
            .count();
        assert!(distinct_positions >= 4);
    }

    #[test]
    fn wipe_and_zhop_roundtrip_both_extrusion_modes() {
        let cyl = make_cylinder(12.0, 2.0, 48);
        for rel_e in [false, true] {
            let mut cfg = PrintConfig::default();
            cfg.machine.relative_extrusion = rel_e;
            cfg.machine.z_hop_mm = 0.4;
            cfg.machine.wipe_on_retract = true;
            cfg.machine.wipe_distance_mm = 2.0;
            cfg.process.sparse_infill_density = 0.25;

            let plate = slice_single_mesh(&cyl, &cfg).unwrap();
            let out = emit_gcode(&plate, &cfg).unwrap();
            let (rep, _) = evaluate_gcode(&out.gcode, 1.75, 250.0, 250.0).unwrap();

            assert!(rep.safety.is_clean(), "rel_e={rel_e}: {:?}", rep.safety);
            assert_eq!(rep.safety.z_regressions, 0);
            assert!(rep.z_hop_count > 0, "rel_e={rel_e} missing z_hop_count");
            assert!(
                rep.wipe_moves_count > 0,
                "rel_e={rel_e} missing wipe_moves_count"
            );
            assert!(
                (rep.total_filament_volume_mm3 - out.stats.filament_volume_mm3).abs() < 0.5,
                "volume mismatch"
            );
        }
    }

    #[test]
    fn template_expansion_and_custom_start_purge_work() {
        let vars = TemplateVars {
            nozzle_temperature: 205,
            first_layer_temperature: 215,
            bed_temperature: 60,
            first_layer_bed_temperature: 65,
            layer_num: 3,
            layer_z: 0.600000000001,
            layer_height: 0.2,
            first_layer_height: 0.25,
            total_layer_count: 10,
            max_z: 2.0,
            bed_width: 250.0,
            bed_depth: 250.0,
            bed_height: 250.0,
            nozzle_diameter: 0.4,
            filament_diameter: 1.75,
        };
        let tpl = "M104 S[first_layer_temperature]\r\n;Z=[layer_z] [unknown_tok] [[nozzle_temperature]]\n;MAX=[max_z]/[total_layer_count] D=[nozzle_diameter]/[filament_diameter] BED=[bed_width]x[bed_depth]x[bed_height] NEXT=[nozzle_temperature]/[bed_temperature]/[first_layer_bed_temperature]/[layer_num]/[layer_height]";
        let expanded = expand_gcode_template(tpl, &vars);
        assert!(!expanded.contains('\r'));
        assert!(expanded.contains("M104 S215"));
        assert!(expanded.contains(";Z=0.6 [unknown_tok] [205]"));
        assert!(expanded.contains(";MAX=2/10 D=0.4/1.75 BED=250x250x250 NEXT=205/60/65/3/0.2"));

        let mesh = make_box(20.0, 20.0, 0.8);
        let mut cfg = PrintConfig::default();
        cfg.filament.nozzle_temp_c = 200;
        cfg.filament.bed_temp_c = 60;
        cfg.filament.first_layer_nozzle_temp_c = Some(215);
        cfg.filament.first_layer_bed_temp_c = Some(65);
        cfg.machine.start_gcode = Some(
            ";CUSTOM_START\nG0 X5 Y5 Z0.3 F6000\nG1 X100 Y5 E10 F1200\nM109 S[first_layer_temperature]".to_owned(),
        );
        cfg.machine.layer_change_gcode = Some(";LC layer=[layer_num] z=[layer_z]".to_owned());
        cfg.machine.end_gcode = Some(";CUSTOM_END max=[max_z]".to_owned());

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let out = emit_gcode(&plate, &cfg).unwrap();
        assert!(out.gcode.contains("M109 S215"));
        assert!(out.gcode.contains("M104 S200"));
        assert!(out.gcode.contains("M140 S60"));
        assert!(out.gcode.contains(";LC layer=1 z=0.4"));
        assert!(out.gcode.contains(";CUSTOM_END max=0.8"));

        // Custom start purge line under `;TYPE:Custom` must not alter part evaluation.
        let (rep, _) = evaluate_gcode(&out.gcode, 1.75, 250.0, 250.0).unwrap();
        assert!(rep.safety.is_clean());
        let bbox = rep.extrusion_bbox_xy.unwrap();
        assert!(bbox[0] > 100.0 && bbox[1] > 100.0);
    }

    #[test]
    fn orca_bambu_template_aliases_and_indexed_placeholders() {
        let vars = TemplateVars {
            nozzle_temperature: 210,
            first_layer_temperature: 220,
            bed_temperature: 60,
            first_layer_bed_temperature: 65,
            layer_num: 2,
            layer_z: 0.45,
            layer_height: 0.2,
            first_layer_height: 0.25,
            total_layer_count: 15,
            max_z: 3.05,
            bed_width: 250.0,
            bed_depth: 250.0,
            bed_height: 250.0,
            nozzle_diameter: 0.4,
            filament_diameter: 1.75,
        };
        let tpl = "M104 S[nozzle_temperature_initial_layer[0]] \
                   M109 S[first_layer_temperature[initial_extruder]] \
                   M140 S[bed_temperature_initial_layer_single] \
                   M190 S[hot_plate_temp_initial_layer] \
                   M140 S[hot_plate_temp] \
                   ;BED=[bed_temperature_initial_layer] \
                   ;Z=[max_layer_z]/[total_layer_number] H=[initial_layer_print_height] \
                   ;KEEP=[[nozzle_temperature]] [unknown_indexed[0]]";
        let out = expand_gcode_template(tpl, &vars);
        assert!(out.contains("M104 S220"));
        assert!(out.contains("M109 S220"));
        assert!(out.contains("M140 S65"));
        assert!(out.contains("M190 S65"));
        assert!(out.contains("M140 S60"));
        assert!(out.contains(";BED=65"));
        assert!(out.contains(";Z=3.05/15 H=0.25"));
        assert!(out.contains(";KEEP=[210] [unknown_indexed[0]]"));
    }

    #[test]
    fn volumetric_flow_cap_enforced_in_evaluator() {
        let mesh = make_box(20.0, 20.0, 2.0);
        let mut cfg = PrintConfig::default();
        cfg.process.infill_speed_mm_s = 250.0;
        cfg.process.outer_wall_speed_mm_s = 180.0;
        cfg.filament.max_volumetric_speed_mm3_s = 6.0;
        cfg.filament.slow_down_layer_time_s = 0.0;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let out = emit_gcode(&plate, &cfg).unwrap();
        let (rep, _) = evaluate_gcode(&out.gcode, 1.75, 250.0, 250.0).unwrap();
        assert!(rep.safety.is_clean());
        assert!(
            rep.max_volumetric_flow_mm3_s <= 6.05,
            "got {} mm3/s",
            rep.max_volumetric_flow_mm3_s
        );
    }
}
