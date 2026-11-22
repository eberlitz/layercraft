//! Volumetric stadium-bead extrusion, G-code emission (Klipper/Marlin),
//! and physical invariant evaluation + 2D raster IoU comparison.

use std::collections::BTreeMap;
use std::f64::consts::{PI, TAU};
use std::fmt::Write as _;

use layercraft_config::{GcodeFlavor, PrintConfig, SeamPosition};
use layercraft_geom::Point2;
pub use layercraft_slice::bead_area_mm2;
use layercraft_slice::{ExtrusionPath, ExtrusionRole, SlicedPlate};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod arc;
pub mod template;
pub use arc::MotionCmd;
pub use template::{TemplateVars, expand_gcode_template};

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

#[derive(Debug, Clone, Copy)]
struct PrintBounds {
    first_layer_min: [f64; 2],
    first_layer_max: [f64; 2],
    print_min: [f64; 2],
    print_max: [f64; 2],
}

fn compute_print_bounds(plate: &SlicedPlate, config: &PrintConfig) -> PrintBounds {
    let bw = config.machine.bed_width_mm;
    let bd = config.machine.bed_depth_mm;
    let mut fl_min_x = f64::INFINITY;
    let mut fl_min_y = f64::INFINITY;
    let mut fl_max_x = f64::NEG_INFINITY;
    let mut fl_max_y = f64::NEG_INFINITY;
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;

    for layer in &plate.layers {
        for path in &layer.paths {
            for pt in &path.points {
                let (x, y) = pt.to_mm();
                let cx = x.clamp(0.0, bw);
                let cy = y.clamp(0.0, bd);
                min_x = min_x.min(cx);
                min_y = min_y.min(cy);
                max_x = max_x.max(cx);
                max_y = max_y.max(cy);
                if layer.index == 0 {
                    fl_min_x = fl_min_x.min(cx);
                    fl_min_y = fl_min_y.min(cy);
                    fl_max_x = fl_max_x.max(cx);
                    fl_max_y = fl_max_y.max(cy);
                }
            }
        }
    }

    let (p_min, p_max) = if min_x <= max_x && min_y <= max_y {
        ([min_x, min_y], [max_x, max_y])
    } else {
        ([0.5 * bw, 0.5 * bd], [0.5 * bw, 0.5 * bd])
    };
    let (fl_min, fl_max) = if fl_min_x <= fl_max_x && fl_min_y <= fl_max_y {
        ([fl_min_x, fl_min_y], [fl_max_x, fl_max_y])
    } else {
        (p_min, p_max)
    };
    PrintBounds {
        first_layer_min: fl_min,
        first_layer_max: fl_max,
        print_min: p_min,
        print_max: p_max,
    }
}

fn make_template_vars(
    config: &PrintConfig,
    layer_num: usize,
    layer_z: f64,
    layer_height: f64,
    total_layer_count: usize,
    max_z: f64,
    bounds: &PrintBounds,
) -> TemplateVars {
    TemplateVars {
        nozzle_temperature: config.filament.nozzle_temp_c,
        first_layer_temperature: config.filament.effective_first_layer_nozzle_temp_c(),
        bed_temperature: config.filament.bed_temp_c,
        first_layer_bed_temperature: config.filament.effective_first_layer_bed_temp_c(),
        chamber_temperature: config.filament.chamber_temp_c.unwrap_or(0),
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
        nozzle_high_flow: config.machine.nozzle_high_flow,
        filament_diameter: config.filament.diameter_mm,
        filament_type: config.filament.filament_type.clone().unwrap_or_default(),
        printer_model: config.machine.printer_model.clone().unwrap_or_default(),
        first_layer_print_min: bounds.first_layer_min,
        first_layer_print_max: bounds.first_layer_max,
        print_min: bounds.print_min,
        print_max: bounds.print_max,
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
    let bounds = compute_print_bounds(plate, config);
    let total_est_time_s: f64 = plate
        .layers
        .iter()
        .map(|l| l.estimated_time_s.max(0.0))
        .sum();
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
            GcodeFlavor::MarlinBuddy => "marlin_buddy",
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
        let vars = make_template_vars(config, 0, first_z, first_h, total_layers, max_z, &bounds);
        let expanded = expand_gcode_template(tpl, &vars);
        push_template_block(&mut out, &expanded)?;
        writeln!(out, "G90 ; restore absolute XYZ").map_err(|_| GcodeError::FormatError)?;
        emit_e_mode(&mut out, config.machine.relative_extrusion)?;
        writeln!(out, "G92 E0").map_err(|_| GcodeError::FormatError)?;
    } else {
        if let Some(chamber) = config.filament.chamber_temp_c
            && chamber > 0
        {
            writeln!(out, "M141 S{chamber}").map_err(|_| GcodeError::FormatError)?;
            if config.filament.wait_for_chamber_temp {
                writeln!(out, "M191 S{chamber}").map_err(|_| GcodeError::FormatError)?;
            }
        }
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

    if let Some(pa) = config.filament.pressure_advance {
        match config.machine.flavor {
            GcodeFlavor::Klipper => {
                writeln!(out, "SET_PRESSURE_ADVANCE ADVANCE={pa:.4}")
                    .map_err(|_| GcodeError::FormatError)?;
            }
            GcodeFlavor::MarlinBuddy => {
                writeln!(out, "M572 S{pa:.4}").map_err(|_| GcodeError::FormatError)?;
            }
            GcodeFlavor::Marlin => {
                writeln!(out, "M900 K{pa:.4}").map_err(|_| GcodeError::FormatError)?;
            }
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
    let mut elapsed_est_s = 0.0;
    let mut current_accel_mm_s2: Option<f64> = None;
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
        if config.machine.emit_progress_m73 {
            let pct = if total_est_time_s > 1e-6 {
                ((elapsed_est_s / total_est_time_s) * 99.0)
                    .floor()
                    .clamp(0.0, 99.0) as u8
            } else {
                ((layer.index * 99) / total_layers.max(1)).min(99) as u8
            };
            let rem_min = ((total_est_time_s - elapsed_est_s).max(0.0) / 60.0).ceil() as u64;
            writeln!(out, "M73 P{pct} R{rem_min}").map_err(|_| GcodeError::FormatError)?;
        }

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
                &bounds,
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

            let (ordered, ordered_widths) = orient_path_with_seam(
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

            let widths_mm = if !ordered_widths.is_empty() && ordered_widths.len() == ordered.len() {
                let mut w = Vec::with_capacity(ordered_widths.len() + usize::from(path.closed));
                w.extend_from_slice(&ordered_widths);
                if path.closed
                    && let Some(&fw) = ordered_widths.first()
                {
                    w.push(fw);
                }
                w
            } else {
                Vec::new()
            };

            if current_role != Some(path.role) {
                writeln!(out, ";TYPE:{}", path.role.label())
                    .map_err(|_| GcodeError::FormatError)?;
                current_role = Some(path.role);
            }

            let desired_accel = if layer.index == 0 {
                config
                    .process
                    .first_layer_acceleration_mm_s2
                    .or(config.process.default_acceleration_mm_s2)
            } else {
                match path.role {
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
            if let Some(accel) = desired_accel
                && current_accel_mm_s2.is_none_or(|prev| (prev - accel).abs() > 0.5)
            {
                match config.machine.flavor {
                    GcodeFlavor::Klipper => {
                        writeln!(out, "SET_VELOCITY_LIMIT ACCEL={accel:.0}")
                            .map_err(|_| GcodeError::FormatError)?;
                    }
                    GcodeFlavor::Marlin | GcodeFlavor::MarlinBuddy => {
                        if let Some(ta) = config.process.travel_acceleration_mm_s2 {
                            writeln!(out, "M204 P{accel:.0} T{ta:.0}")
                                .map_err(|_| GcodeError::FormatError)?;
                        } else {
                            writeln!(out, "M204 P{accel:.0}")
                                .map_err(|_| GcodeError::FormatError)?;
                        }
                    }
                }
                current_accel_mm_s2 = Some(accel);
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

            let eff_flow = path.flow_ratio.max(0.001);
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

            let mut path_trail = Vec::with_capacity(iter_pts.len() + 2);
            path_trail.push((cur_x, cur_y));

            let mut pts_mm = Vec::with_capacity(iter_pts.len() + 1);
            pts_mm.push((cur_x, cur_y));
            for pt in iter_pts {
                pts_mm.push(pt.to_mm());
            }

            let is_varying_w = widths_mm.len() == pts_mm.len() && widths_mm.len() >= 2 && {
                let min_w = widths_mm.iter().copied().fold(f64::INFINITY, f64::min);
                let max_w = widths_mm.iter().copied().fold(0.0, f64::max);
                max_w - min_w > 1e-4
            };

            let total_loop_len: f64 = pts_mm
                .windows(2)
                .filter_map(|w| Some((w.first()?, w.get(1)?)))
                .map(|(a, b)| (b.0 - a.0).hypot(b.1 - a.1))
                .sum();
            let scarf = &config.process.scarf_seam;
            let use_scarf = scarf.enabled
                && layer.index > 0
                && path.closed
                && path.role == ExtrusionRole::OuterWall
                && !is_varying_w
                && total_loop_len >= scarf.min_length_mm;

            let mut emit_ctx = PathEmitCtx {
                out: &mut out,
                cur_x: &mut cur_x,
                cur_y: &mut cur_y,
                cur_e: &mut cur_e,
                total_e_mm: &mut total_e_mm,
                total_time_s: &mut total_time_s,
                has_extruded: &mut has_extruded,
                f_entry,
                path_trail: &mut path_trail,
                relative_e: config.machine.relative_extrusion,
                fil_area,
                fil_diam: config.filament.diameter_mm,
                density: config.filament.density_g_cm3,
                width_mm: path.width_mm,
                height_mm: path.height_mm,
                base_flow: eff_flow,
                base_speed_mm_s: path.speed_mm_s,
                max_vol_flow: config.filament.max_volumetric_speed_mm3_s,
                feed,
                actual_speed,
            };

            let mut scarf_done = false;
            if use_scarf {
                let target_len = if scarf.entire_loop {
                    total_loop_len
                } else {
                    scarf.length_mm.min(0.45 * total_loop_len)
                };
                if target_len >= 1.0
                    && let Some(sub) =
                        subdivide_loop_for_scarf(&pts_mm, target_len, scarf.steps as usize)
                {
                    let z_base = layer.z_top_mm - path.height_mm;
                    let z_start = z_base + scarf.start_height_ratio * path.height_mm;
                    writeln!(emit_ctx.out, ";SCARF_START").map_err(|_| GcodeError::FormatError)?;
                    writeln!(emit_ctx.out, "G0 Z{z_start:.3} F{zhop_f:.0}")
                        .map_err(|_| GcodeError::FormatError)?;
                    *emit_ctx.total_time_s +=
                        (cur_z - z_start).abs() / config.machine.z_hop_speed_mm_s;

                    // Phase A: ascending Z & flow ramp over [0..L].
                    let mut ramp_z = z_start;
                    for pair in sub.ramp_pts.windows(2) {
                        let (Some(&(x0, y0, s0)), Some(&(x1, y1, s1))) =
                            (pair.first(), pair.get(1))
                        else {
                            continue;
                        };
                        let seg_len = (x1 - x0).hypot(y1 - y0);
                        if seg_len <= 1e-6 {
                            continue;
                        }
                        let t_end = (s1 / target_len).clamp(0.0, 1.0);
                        let t_mid = (0.5 * (s0 + s1) / target_len).clamp(0.0, 1.0);
                        let z1 = z_start + t_end * (layer.z_top_mm - z_start);
                        let frac_a = (scarf.start_height_ratio
                            + t_mid * (1.0 - scarf.start_height_ratio))
                            * scarf.flow_ratio;
                        emit_ctx.emit_linear((x1, y1), seg_len, Some(z1), frac_a)?;
                        ramp_z = z1;
                    }
                    if (ramp_z - layer.z_top_mm).abs() > 1e-4 {
                        writeln!(emit_ctx.out, "G0 Z{:.3} F{zhop_f:.0}", layer.z_top_mm)
                            .map_err(|_| GcodeError::FormatError)?;
                    }
                    cur_z = layer.z_top_mm;

                    // Phase B: constant-Z body over [L..P].
                    if sub.body_pts.len() >= 2 {
                        emit_ctx.emit_xy_sequence(&sub.body_pts, config)?;
                    }

                    // Phase C: complementary flow overlap at constant Z = layer.z_top_mm over [0..L].
                    for pair in sub.ramp_pts.windows(2) {
                        let (Some(&(x0, y0, s0)), Some(&(x1, y1, s1))) =
                            (pair.first(), pair.get(1))
                        else {
                            continue;
                        };
                        let seg_len = (x1 - x0).hypot(y1 - y0);
                        if seg_len <= 1e-6 {
                            continue;
                        }
                        let t_mid = (0.5 * (s0 + s1) / target_len).clamp(0.0, 1.0);
                        let height_frac =
                            scarf.start_height_ratio + t_mid * (1.0 - scarf.start_height_ratio);
                        let frac_c = ((1.0 - height_frac) * scarf.flow_ratio).max(0.0);
                        emit_ctx.emit_linear((x1, y1), seg_len, None, frac_c)?;
                    }
                    writeln!(emit_ctx.out, ";SCARF_END").map_err(|_| GcodeError::FormatError)?;
                    scarf_done = true;
                }
            }

            if !scarf_done {
                emit_ctx.emit_xy_sequence_variable(&pts_mm, &widths_mm, config)?;
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
        elapsed_est_s += layer.estimated_time_s.max(0.0);
    }

    if config.machine.emit_progress_m73 {
        writeln!(out, "M73 P100 R0").map_err(|_| GcodeError::FormatError)?;
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
            &bounds,
        );
        let expanded = expand_gcode_template(tpl, &vars);
        push_template_block(&mut out, &expanded)?;
    } else {
        if !retracted && has_extruded && retract_len > 0.0 {
            emit_e_move(
                &mut out,
                config.machine.relative_extrusion,
                &mut cur_e,
                -retract_len,
                retract_f,
            )?;
        }
        writeln!(out, "M107").map_err(|_| GcodeError::FormatError)?;
        writeln!(out, "M104 S0").map_err(|_| GcodeError::FormatError)?;
        writeln!(out, "M140 S0").map_err(|_| GcodeError::FormatError)?;
        if config.filament.chamber_temp_c.is_some_and(|c| c > 0) {
            writeln!(out, "M141 S0").map_err(|_| GcodeError::FormatError)?;
        }
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

struct PathEmitCtx<'a> {
    out: &'a mut String,
    cur_x: &'a mut f64,
    cur_y: &'a mut f64,
    cur_e: &'a mut f64,
    total_e_mm: &'a mut f64,
    total_time_s: &'a mut f64,
    has_extruded: &'a mut bool,
    f_entry: &'a mut FeatureStats,
    path_trail: &'a mut Vec<(f64, f64)>,
    relative_e: bool,
    fil_area: f64,
    fil_diam: f64,
    density: f64,
    width_mm: f64,
    height_mm: f64,
    base_flow: f64,
    base_speed_mm_s: f64,
    max_vol_flow: f64,
    feed: f64,
    actual_speed: f64,
}

impl PathEmitCtx<'_> {
    fn compute_segment_feed(&self, width_mm: f64) -> (f64, f64) {
        let area = bead_area_mm2(width_mm, self.height_mm) * self.base_flow;
        let eff_speed =
            if area > 1e-9 && self.max_vol_flow.is_finite() && self.max_vol_flow > 0.0 {
                self.base_speed_mm_s.min(self.max_vol_flow / area)
            } else {
                self.base_speed_mm_s
            }
            .max(1e-3);
        let feed = (eff_speed * 60.0).floor().max(1.0);
        (feed, feed / 60.0)
    }

    fn account_segment(&mut self, len_mm: f64, flow_mult: f64) -> f64 {
        let width_mm = self.width_mm;
        let actual_speed = self.actual_speed;
        self.account_segment_with_width(len_mm, width_mm, actual_speed, flow_mult)
    }

    fn account_segment_with_width(
        &mut self,
        len_mm: f64,
        width_mm: f64,
        speed_mm_s: f64,
        flow_mult: f64,
    ) -> f64 {
        let de = extrusion_delta_mm(len_mm, width_mm, self.height_mm, self.fil_diam)
            * self.base_flow
            * flow_mult;
        let vol = de * self.fil_area;
        let dt = len_mm / speed_mm_s.max(1e-6);
        *self.total_e_mm += de;
        *self.total_time_s += dt;
        self.f_entry.volume_mm3 += vol;
        self.f_entry.weight_g += vol * 0.001 * self.density;
        self.f_entry.estimated_time_s += dt;
        *self.has_extruded = true;
        de
    }

    fn emit_linear(
        &mut self,
        to: (f64, f64),
        len_mm: f64,
        z_override: Option<f64>,
        flow_mult: f64,
    ) -> Result<(), GcodeError> {
        let width_mm = self.width_mm;
        let feed = self.feed;
        let speed = self.actual_speed;
        self.emit_linear_with_width(to, len_mm, width_mm, feed, speed, z_override, flow_mult)
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_linear_with_width(
        &mut self,
        to: (f64, f64),
        len_mm: f64,
        width_mm: f64,
        feed: f64,
        speed_mm_s: f64,
        z_override: Option<f64>,
        flow_mult: f64,
    ) -> Result<(), GcodeError> {
        let de = self.account_segment_with_width(len_mm, width_mm, speed_mm_s, flow_mult);
        let e_val = next_e_val(self.relative_e, self.cur_e, de);
        if let Some(z) = z_override {
            writeln!(
                self.out,
                "G1 X{:.3} Y{:.3} Z{z:.3} E{e_val:.5} F{:.0}",
                to.0, to.1, feed
            )
            .map_err(|_| GcodeError::FormatError)?;
        } else {
            writeln!(
                self.out,
                "G1 X{:.3} Y{:.3} E{e_val:.5} F{:.0}",
                to.0, to.1, feed
            )
            .map_err(|_| GcodeError::FormatError)?;
        }
        *self.cur_x = to.0;
        *self.cur_y = to.1;
        self.path_trail.push(to);
        Ok(())
    }

    fn emit_arc(
        &mut self,
        cw: bool,
        to: (f64, f64),
        center: (f64, f64),
        radius: f64,
        sweep_rad: f64,
        len_mm: f64,
    ) -> Result<(), GcodeError> {
        let from = (*self.cur_x, *self.cur_y);
        let i_off = center.0 - from.0;
        let j_off = center.1 - from.1;
        let de = self.account_segment(len_mm, 1.0);
        let e_val = next_e_val(self.relative_e, self.cur_e, de);
        let cmd = if cw { "G2" } else { "G3" };
        writeln!(
            self.out,
            "{cmd} X{:.3} Y{:.3} I{i_off:.4} J{j_off:.4} E{e_val:.5} F{:.0}",
            to.0, to.1, self.feed
        )
        .map_err(|_| GcodeError::FormatError)?;
        let samples = arc::sample_arc_points(from, center, radius, cw, sweep_rad, 0.25);
        self.path_trail.extend(samples);
        *self.cur_x = to.0;
        *self.cur_y = to.1;
        Ok(())
    }

    fn emit_xy_sequence_variable(
        &mut self,
        pts_mm: &[(f64, f64)],
        widths_mm: &[f64],
        config: &PrintConfig,
    ) -> Result<(), GcodeError> {
        let is_varying = widths_mm.len() == pts_mm.len() && widths_mm.len() >= 2 && {
            let min_w = widths_mm.iter().copied().fold(f64::INFINITY, f64::min);
            let max_w = widths_mm.iter().copied().fold(0.0, f64::max);
            max_w - min_w > 1e-4
        };
        if !is_varying {
            return self.emit_xy_sequence(pts_mm, config);
        }

        for i in 0..pts_mm.len().saturating_sub(1) {
            let (Some(&a), Some(&b), Some(&w0), Some(&w1)) = (
                pts_mm.get(i),
                pts_mm.get(i + 1),
                widths_mm.get(i),
                widths_mm.get(i + 1),
            ) else {
                continue;
            };
            let seg_len = (b.0 - a.0).hypot(b.1 - a.1);
            if seg_len <= 1e-6 {
                continue;
            }
            let seg_w = 0.5 * (w0 + w1);
            let (feed, spd) = self.compute_segment_feed(seg_w);
            self.emit_linear_with_width(b, seg_len, seg_w, feed, spd, None, 1.0)?;
        }
        Ok(())
    }

    fn emit_xy_sequence(
        &mut self,
        pts_mm: &[(f64, f64)],
        config: &PrintConfig,
    ) -> Result<(), GcodeError> {
        if config.process.arc_fitting {
            let cmds = arc::fit_polyline_arcs(pts_mm, config.process.arc_tolerance_mm);
            for cmd in cmds {
                match cmd {
                    MotionCmd::Line { to, len_mm } => {
                        self.emit_linear(to, len_mm, None, 1.0)?;
                    }
                    MotionCmd::Arc {
                        cw,
                        to,
                        center,
                        radius,
                        sweep_rad,
                        len_mm,
                    } => {
                        self.emit_arc(cw, to, center, radius, sweep_rad, len_mm)?;
                    }
                }
            }
            return Ok(());
        }
        for pair in pts_mm.windows(2) {
            let (Some(&a), Some(&b)) = (pair.first(), pair.get(1)) else {
                continue;
            };
            let seg_len = (b.0 - a.0).hypot(b.1 - a.1);
            if seg_len <= 1e-6 {
                continue;
            }
            self.emit_linear(b, seg_len, None, 1.0)?;
        }
        Ok(())
    }
}

fn sample_polyline_at_dist(pts: &[(f64, f64)], cum: &[f64], target_d: f64) -> Option<(f64, f64)> {
    let total = *cum.last()?;
    if target_d <= 0.0 {
        return pts.first().copied();
    }
    if target_d >= total {
        return pts.last().copied();
    }
    for k in 0..cum.len().saturating_sub(1) {
        let (Some(&d0), Some(&d1), Some(&p0), Some(&p1)) =
            (cum.get(k), cum.get(k + 1), pts.get(k), pts.get(k + 1))
        else {
            continue;
        };
        if target_d <= d1 + 1e-9 && d1 > d0 + 1e-9 {
            let t = ((target_d - d0) / (d1 - d0)).clamp(0.0, 1.0);
            return Some((p0.0 + t * (p1.0 - p0.0), p0.1 + t * (p1.1 - p0.1)));
        }
    }
    pts.last().copied()
}

struct ScarfSubdivision {
    ramp_pts: Vec<(f64, f64, f64)>,
    body_pts: Vec<(f64, f64)>,
}

fn subdivide_loop_for_scarf(
    points_mm: &[(f64, f64)],
    scarf_len_mm: f64,
    steps: usize,
) -> Option<ScarfSubdivision> {
    if points_mm.len() < 4 || steps == 0 || scarf_len_mm <= 0.0 {
        return None;
    }
    let mut cum = Vec::with_capacity(points_mm.len());
    cum.push(0.0);
    let mut acc = 0.0;
    for pair in points_mm.windows(2) {
        let (Some(&a), Some(&b)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        acc += (b.0 - a.0).hypot(b.1 - a.1);
        cum.push(acc);
    }
    let total_p = *cum.last()?;
    if total_p <= 1e-4 {
        return None;
    }
    let eff_len = scarf_len_mm.min(total_p);

    let mut dists = Vec::with_capacity(steps + cum.len() + 2);
    dists.push(0.0);
    for s in 1..steps {
        dists.push((s as f64 / steps as f64) * eff_len);
    }
    for &cd in &cum {
        if cd > 1e-4 && cd < eff_len - 1e-4 {
            dists.push(cd);
        }
    }
    dists.push(eff_len);
    dists.sort_by(f64::total_cmp);

    let mut clean_dists: Vec<f64> = Vec::with_capacity(dists.len());
    for d in dists {
        if let Some(&prev) = clean_dists.last()
            && (d - prev).abs() < 1e-4
        {
            continue;
        }
        clean_dists.push(d);
    }
    if let Some(last) = clean_dists.last_mut()
        && (*last - eff_len).abs() >= 1e-6
    {
        if eff_len - *last < 1e-4 {
            *last = eff_len;
        } else {
            clean_dists.push(eff_len);
        }
    }

    let mut ramp_pts = Vec::with_capacity(clean_dists.len());
    for s in clean_dists {
        let pt = sample_polyline_at_dist(points_mm, &cum, s)?;
        ramp_pts.push((pt.0, pt.1, s));
    }

    let mut body_pts = Vec::new();
    if eff_len < total_p - 1e-4 {
        let split_pt = sample_polyline_at_dist(points_mm, &cum, eff_len)?;
        body_pts.push(split_pt);
        for (&pt, &cd) in points_mm.iter().zip(cum.iter()) {
            if cd > eff_len + 1e-4 {
                body_pts.push(pt);
            }
        }
        if let Some(&end_pt) = points_mm.last()
            && body_pts
                .last()
                .is_some_and(|p| (p.0 - end_pt.0).hypot(p.1 - end_pt.1) > 1e-4)
        {
            body_pts.push(end_pt);
        }
    }

    Some(ScarfSubdivision { ramp_pts, body_pts })
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

fn rotate_closed_slice<T: Copy>(items: &[T], start_idx: usize) -> Vec<T> {
    let n = items.len();
    let mut rotated = Vec::with_capacity(n);
    rotated.extend(items.iter().skip(start_idx).copied());
    rotated.extend(items.iter().take(start_idx).copied());
    rotated
}

fn orient_path_nearest(path: &ExtrusionPath, cur_x: f64, cur_y: f64) -> (Vec<Point2>, Vec<f64>) {
    let n = path.points.len();
    let var_w = path.has_variable_width();
    if n <= 1 {
        let ws = if var_w {
            path.point_widths_mm.clone()
        } else {
            Vec::new()
        };
        return (path.points.clone(), ws);
    }
    if path.closed {
        let best_idx = closest_vertex_index(&path.points, cur_x, cur_y);
        let pts = rotate_closed_slice(&path.points, best_idx);
        let ws = if var_w {
            rotate_closed_slice(&path.point_widths_mm, best_idx)
        } else {
            Vec::new()
        };
        (pts, ws)
    } else {
        let (Some(first), Some(last)) = (path.points.first(), path.points.last()) else {
            return (path.points.clone(), Vec::new());
        };
        let (fx, fy) = first.to_mm();
        let (lx, ly) = last.to_mm();
        let df = (fx - cur_x).powi(2) + (fy - cur_y).powi(2);
        let dl = (lx - cur_x).powi(2) + (ly - cur_y).powi(2);
        if dl < df {
            let pts = path.points.iter().rev().copied().collect();
            let ws = if var_w {
                path.point_widths_mm.iter().rev().copied().collect()
            } else {
                Vec::new()
            };
            (pts, ws)
        } else {
            let ws = if var_w {
                path.point_widths_mm.clone()
            } else {
                Vec::new()
            };
            (path.points.clone(), ws)
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
) -> (Vec<Point2>, Vec<f64>) {
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
    let pts = rotate_closed_slice(&path.points, start_idx);
    let ws = if path.has_variable_width() {
        rotate_closed_slice(&path.point_widths_mm, start_idx)
    } else {
        Vec::new()
    };
    (pts, ws)
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
    #[serde(default)]
    pub accel_commands_count: usize,
    #[serde(default)]
    pub pressure_advance_commands_count: usize,
    #[serde(default)]
    pub arc_moves_count: usize,
    #[serde(default)]
    pub scarf_ramp_moves_count: usize,
    #[serde(default = "default_wall_variation_ratio")]
    pub wall_width_variation_ratio: f64,
    #[serde(default)]
    pub min_layer_height_mm: f64,
    #[serde(default)]
    pub max_layer_height_mm: f64,
    #[serde(default = "default_layer_height_variation_ratio")]
    pub layer_height_variation_ratio: f64,
}

fn default_wall_variation_ratio() -> f64 {
    1.0
}

fn default_layer_height_variation_ratio() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy)]
struct WallAreaTracker {
    min: f64,
    max: f64,
}

impl Default for WallAreaTracker {
    fn default() -> Self {
        Self {
            min: f64::INFINITY,
            max: 0.0,
        }
    }
}

impl WallAreaTracker {
    fn observe(&mut self, area: f64) {
        if area > 1e-9 && area.is_finite() {
            self.min = self.min.min(area);
            self.max = self.max.max(area);
        }
    }

    fn ratio_and_reset(&mut self) -> f64 {
        let r = if self.min.is_finite() && self.min > 1e-9 && self.max > self.min {
            self.max / self.min
        } else {
            1.0
        };
        *self = Self::default();
        r
    }
}

#[derive(Debug, Default)]
struct WallVariationState {
    last_wall_z: Option<f64>,
    outer: WallAreaTracker,
    inner: WallAreaTracker,
    max_ratio: f64,
}

impl WallVariationState {
    fn new() -> Self {
        Self {
            max_ratio: 1.0,
            ..Self::default()
        }
    }

    fn observe_move(
        &mut self,
        feature: &str,
        in_scarf: bool,
        move_len: f64,
        vol: f64,
        record_z: f64,
    ) {
        if in_scarf || move_len < 0.35 || (feature != "Outer wall" && feature != "Inner wall") {
            return;
        }
        if self
            .last_wall_z
            .is_some_and(|lz| (record_z - lz).abs() > 1e-3)
        {
            self.flush_layer();
        }
        self.last_wall_z = Some(record_z);
        let area = vol / move_len;
        if feature == "Outer wall" {
            self.outer.observe(area);
        } else {
            self.inner.observe(area);
        }
    }

    fn flush_layer(&mut self) -> f64 {
        self.max_ratio = self
            .max_ratio
            .max(self.outer.ratio_and_reset())
            .max(self.inner.ratio_and_reset());
        self.max_ratio
    }
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
    let mut in_scarf = false;
    let mut layer_change_count = 0usize;
    let mut retraction_debt_mm = 0.0;
    let mut max_retraction_imbalance_mm = 0.0f64;

    let mut wall_variation = WallVariationState::new();

    let mut total_e_mm = 0.0;
    let mut total_travel_mm = 0.0;
    let mut feature_volumes_mm3: BTreeMap<String, f64> = BTreeMap::new();
    let mut layer_z_mm: Vec<f64> = Vec::new();
    let mut max_active_z = 0.0f64;
    let mut last_extrusion_z: Option<f64> = None;
    let mut current_declared_layer_z: Option<f64> = None;
    let mut prev_layer_max_z = 0.0f64;

    let mut max_speed_mm_s = 0.0f64;
    let mut max_volumetric_flow_mm3_s = 0.0f64;
    let mut fan_commands_count = 0usize;
    let mut max_fan_pwm = 0u8;
    let mut first_layer_max_fan_pwm = 0u8;
    let mut z_hop_count = 0usize;
    let mut wipe_moves_count = 0usize;
    let mut accel_commands_count = 0usize;
    let mut pressure_advance_commands_count = 0usize;
    let mut arc_moves_count = 0usize;
    let mut scarf_ramp_moves_count = 0usize;

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
        if trimmed == ";SCARF_START" {
            in_scarf = true;
            continue;
        }
        if trimmed == ";SCARF_END" {
            in_scarf = false;
            continue;
        }
        if trimmed == ";LAYER_CHANGE" {
            layer_change_count += 1;
            prev_layer_max_z = prev_layer_max_z.max(max_active_z);
            current_declared_layer_z = None;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(";Z:") {
            if let Ok(zval) = rest.trim().parse::<f64>()
                && zval.is_finite()
                && zval > 0.0
            {
                current_declared_layer_z = Some(zval);
            }
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
            "M204" => {
                accel_commands_count += 1;
            }
            "SET_VELOCITY_LIMIT" => {
                if tokens.any(|tok| tok.starts_with("ACCEL=")) {
                    accel_commands_count += 1;
                }
            }
            "M572" | "M900" | "SET_PRESSURE_ADVANCE" => {
                pressure_advance_commands_count += 1;
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
                        if current_declared_layer_z.is_some() {
                            if tz + 1e-4 < prev_layer_max_z {
                                safety.z_regressions += 1;
                            }
                        } else if let Some(prev_z) = last_extrusion_z
                            && tz + 1e-4 < prev_z
                        {
                            safety.z_regressions += 1;
                        }
                        last_extrusion_z = Some(tz);

                        if current_feature == "Outer wall" && (tz - cur_z).abs() > 1e-4 {
                            scarf_ramp_moves_count += 1;
                        }

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

                        let record_z = current_declared_layer_z.unwrap_or(tz);
                        wall_variation.observe_move(
                            &current_feature,
                            in_scarf,
                            xy_dist,
                            vol,
                            record_z,
                        );
                        if record_z > 0.0 {
                            max_active_z = max_active_z.max(tz).max(record_z);
                            let already = layer_z_mm
                                .last()
                                .is_some_and(|&lz| (lz - record_z).abs() < 1e-3);
                            if !already {
                                layer_z_mm.push(record_z);
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
                                z_mm: record_z,
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
            "G2" | "G3" => {
                let cw = cmd == "G2";
                let mut next_x = None;
                let mut next_y = None;
                let mut next_z = None;
                let mut off_i = 0.0;
                let mut off_j = 0.0;
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
                        'I' => off_i = val,
                        'J' => off_j = val,
                        'E' => next_e = Some(val),
                        'F' if val > 0.0 => cur_feed_mm_min = val,
                        _ => {}
                    }
                }

                let tx = next_x.unwrap_or(cur_x);
                let ty = next_y.unwrap_or(cur_y);
                let tz = next_z.unwrap_or(cur_z);
                let cx = cur_x + off_i;
                let cy = cur_y + off_j;
                let r = off_i.hypot(off_j);
                let is_custom = current_feature.eq_ignore_ascii_case("Custom");

                let arc_sweep = if r > 1e-6 {
                    let a0 = (cur_y - cy).atan2(cur_x - cx);
                    let a1 = (ty - cy).atan2(tx - cx);
                    let mut sw = if cw { a0 - a1 } else { a1 - a0 };
                    while sw <= 1e-9 {
                        sw += TAU;
                    }
                    if sw > TAU + 1e-9 {
                        sw %= TAU;
                    }
                    sw
                } else {
                    0.0
                };
                let arc_len = r * arc_sweep;

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

                if !is_custom && arc_len > 1e-5 {
                    arc_moves_count += 1;
                }

                if de_raw > 1e-7 {
                    let net_de = if retraction_debt_mm > 0.0 {
                        let pay = retraction_debt_mm.min(de_raw);
                        retraction_debt_mm -= pay;
                        de_raw - pay
                    } else {
                        de_raw
                    };

                    if net_de > 1e-7 && arc_len > 1e-5 && !is_custom {
                        if current_declared_layer_z.is_some() {
                            if tz + 1e-4 < prev_layer_max_z {
                                safety.z_regressions += 1;
                            }
                        } else if let Some(prev_z) = last_extrusion_z
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
                            let dt = arc_len / speed_mm_s;
                            if dt > 1e-9 {
                                max_speed_mm_s = max_speed_mm_s.max(speed_mm_s);
                                max_volumetric_flow_mm3_s = max_volumetric_flow_mm3_s.max(vol / dt);
                            }
                        }

                        let record_z = current_declared_layer_z.unwrap_or(tz);
                        wall_variation.observe_move(
                            &current_feature,
                            in_scarf,
                            arc_len,
                            vol,
                            record_z,
                        );
                        if record_z > 0.0 {
                            max_active_z = max_active_z.max(tz).max(record_z);
                            let already = layer_z_mm
                                .last()
                                .is_some_and(|&lz| (lz - record_z).abs() < 1e-3);
                            if !already {
                                layer_z_mm.push(record_z);
                            }
                        }

                        if has_xy {
                            has_extrusion = true;
                            let n_steps =
                                (arc_sweep / 5.0_f64.to_radians()).ceil().clamp(4.0, 64.0) as usize;
                            let a0 = (cur_y - cy).atan2(cur_x - cx);
                            let dir = if cw { -1.0 } else { 1.0 };
                            let seg_vol = vol / (n_steps as f64);
                            let mut prev_p = (cur_x, cur_y);
                            for step in 1..=n_steps {
                                let p_next = if step == n_steps {
                                    (tx, ty)
                                } else {
                                    let t = (step as f64) / (n_steps as f64);
                                    let ang = a0 + dir * t * arc_sweep;
                                    (cx + r * ang.cos(), cy + r * ang.sin())
                                };
                                min_ex_x = min_ex_x.min(prev_p.0).min(p_next.0);
                                min_ex_y = min_ex_y.min(prev_p.1).min(p_next.1);
                                max_ex_x = max_ex_x.max(prev_p.0).max(p_next.0);
                                max_ex_y = max_ex_y.max(prev_p.1).max(p_next.1);
                                if p_next.0 < -1.0
                                    || p_next.1 < -1.0
                                    || p_next.0 > bed_width_mm + 1.0
                                    || p_next.1 > bed_depth_mm + 1.0
                                {
                                    safety.out_of_bounds_moves += 1;
                                }
                                segments.push(GcodeExtrusionSegment {
                                    z_mm: record_z,
                                    x0_mm: prev_p.0,
                                    y0_mm: prev_p.1,
                                    x1_mm: p_next.0,
                                    y1_mm: p_next.1,
                                    volume_mm3: seg_vol,
                                    feature: current_feature.clone(),
                                });
                                prev_p = p_next;
                            }
                        }
                    }
                } else if arc_len > 1e-5 {
                    total_travel_mm += arc_len;
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
    let max_wall_variation = wall_variation.flush_layer();

    let (min_layer_height_mm, max_layer_height_mm, layer_height_variation_ratio) =
        if layer_z_mm.len() >= 2 {
            let mut min_h = f64::INFINITY;
            let mut max_h = 0.0_f64;
            for pair in layer_z_mm.windows(2) {
                if let (Some(&z0), Some(&z1)) = (pair.first(), pair.get(1)) {
                    let dz = z1 - z0;
                    if dz > 1e-6 && dz.is_finite() {
                        min_h = min_h.min(dz);
                        max_h = max_h.max(dz);
                    }
                }
            }
            if min_h.is_finite() && min_h > 1e-6 {
                (min_h, max_h, max_h / min_h)
            } else {
                (0.0, 0.0, 1.0)
            }
        } else if let Some(&z0) = layer_z_mm.first() {
            (z0, z0, 1.0)
        } else {
            (0.0, 0.0, 1.0)
        };

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
        accel_commands_count,
        pressure_advance_commands_count,
        arc_moves_count,
        scarf_ramp_moves_count,
        wall_width_variation_ratio: max_wall_variation,
        min_layer_height_mm,
        max_layer_height_mm,
        layer_height_variation_ratio,
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
            ..TemplateVars::default()
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
            ..TemplateVars::default()
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

    #[test]
    fn conditional_braces_and_expressions_evaluate_safely() {
        let vars = TemplateVars {
            nozzle_temperature: 220,
            first_layer_temperature: 225,
            bed_temperature: 70,
            first_layer_bed_temperature: 75,
            chamber_temperature: 50,
            layer_num: 1,
            layer_z: 0.4,
            layer_height: 0.2,
            first_layer_height: 0.2,
            total_layer_count: 20,
            max_z: 4.0,
            bed_width: 250.0,
            bed_depth: 220.0,
            bed_height: 270.0,
            nozzle_diameter: 0.2,
            nozzle_high_flow: true,
            filament_diameter: 1.75,
            filament_type: "ABS".to_owned(),
            printer_model: "COREONE".to_owned(),
            first_layer_print_min: [110.0, 100.0],
            first_layer_print_max: [140.0, 120.0],
            print_min: [110.0, 100.0],
            print_max: [140.0, 120.0],
        };
        let tpl = concat!(
            "F{if nozzle_high_flow}1{else}0{endif}\n",
            "{if filament_type[0] == \"PETG\"}M141 S0",
            "{elsif chamber_temperature >= 45 && filament_type[0] == \"ABS\"}",
            "{if nozzle_diameter < 0.3}M141 S[chamber_temperature] ;FINE_ABS{endif}",
            "{else}M141 S20{endif}\n",
            "M555 W{first_layer_print_max[0] - first_layer_print_min[0]} ",
            "Z{min(max_z + 15, bed_height)} ",
            "N{round(1.6)} I{int(3.9)} X{bed_width - 10}\n",
            ";BAD {1/0} {if broken"
        );
        let expanded = expand_gcode_template(tpl, &vars);
        assert!(expanded.contains("F1\n"));
        assert!(expanded.contains("M141 S50 ;FINE_ABS\n"));
        assert!(!expanded.contains("M141 S0"));
        assert!(!expanded.contains("M141 S20"));
        assert!(expanded.contains("M555 W30 Z19 N2 I3 X240\n"));
        assert!(expanded.contains(";BAD {1/0} {if broken"));
    }

    #[test]
    fn real_printer_pa_accel_chamber_and_m73_roundtrip() {
        let mesh = make_box(16.0, 16.0, 2.0);
        for (flavor, pa_prefix, accel_prefix) in [
            (GcodeFlavor::MarlinBuddy, "M572 S0.0450", "M204 P1500 T5000"),
            (GcodeFlavor::Marlin, "M900 K0.0450", "M204 P1500 T5000"),
            (
                GcodeFlavor::Klipper,
                "SET_PRESSURE_ADVANCE ADVANCE=0.0450",
                "SET_VELOCITY_LIMIT ACCEL=1500",
            ),
        ] {
            let mut cfg = PrintConfig::default();
            cfg.machine.flavor = flavor;
            cfg.machine.emit_progress_m73 = true;
            cfg.filament.pressure_advance = Some(0.045);
            cfg.filament.chamber_temp_c = Some(48);
            cfg.filament.wait_for_chamber_temp = true;
            cfg.process.default_acceleration_mm_s2 = Some(3000.0);
            cfg.process.outer_wall_acceleration_mm_s2 = Some(1500.0);
            cfg.process.travel_acceleration_mm_s2 = Some(5000.0);

            let plate = slice_single_mesh(&mesh, &cfg).unwrap();
            let out = emit_gcode(&plate, &cfg).unwrap();
            assert!(out.gcode.contains(pa_prefix), "missing {pa_prefix}");
            assert!(out.gcode.contains(accel_prefix), "missing {accel_prefix}");
            assert!(out.gcode.contains("M141 S48"));
            assert!(out.gcode.contains("M191 S48"));
            assert!(out.gcode.contains("M141 S0"));
            assert!(out.gcode.contains("M73 P0 R"));
            assert!(out.gcode.contains("M73 P100 R0"));

            let (rep, _) = evaluate_gcode(&out.gcode, 1.75, 250.0, 250.0).unwrap();
            assert!(rep.safety.is_clean());
            assert_eq!(rep.pressure_advance_commands_count, 1);
            assert!(rep.accel_commands_count >= 1);
        }
    }

    #[test]
    fn arc_fitting_compresses_cylinder_and_preserves_iou() {
        let cyl = make_cylinder(12.0, 2.0, 64);

        let mut cfg_lin = PrintConfig::default();
        cfg_lin.process.arc_fitting = false;
        let plate = slice_single_mesh(&cyl, &cfg_lin).unwrap();
        let out_lin = emit_gcode(&plate, &cfg_lin).unwrap();
        let (rep_lin, segs_lin) = evaluate_gcode(&out_lin.gcode, 1.75, 250.0, 250.0).unwrap();

        let mut cfg_arc = cfg_lin;
        cfg_arc.process.arc_fitting = true;
        cfg_arc.process.arc_tolerance_mm = 0.025;
        let out_arc = emit_gcode(&plate, &cfg_arc).unwrap();
        let (rep_arc, segs_arc) = evaluate_gcode(&out_arc.gcode, 1.75, 250.0, 250.0).unwrap();

        assert!(rep_arc.safety.is_clean(), "{:?}", rep_arc.safety);
        assert!(rep_arc.arc_moves_count > 10);
        assert!(out_arc.gcode.lines().count() < out_lin.gcode.lines().count());

        let rel_vol_diff = (rep_arc.total_filament_volume_mm3 - rep_lin.total_filament_volume_mm3)
            .abs()
            / rep_lin.total_filament_volume_mm3.max(1e-6);
        assert!(
            rel_vol_diff < 0.01,
            "arc volume diff {rel_vol_diff:.4} too large"
        );

        let iou = compare_layer_iou(&segs_lin, &segs_arc, 0.25, 0.21);
        assert!(
            iou.mean_layer_iou > 0.95,
            "arc IoU {:.4} <= 0.95",
            iou.mean_layer_iou
        );
    }

    #[test]
    fn scarf_seam_ramps_z_conserves_volume_and_preserves_layer_count() {
        let cyl = make_cylinder(10.0, 4.0, 48);

        let mut cfg_base = PrintConfig::default();
        cfg_base.process.scarf_seam.enabled = false;
        let plate = slice_single_mesh(&cyl, &cfg_base).unwrap();
        let out_base = emit_gcode(&plate, &cfg_base).unwrap();
        let (rep_base, _) = evaluate_gcode(&out_base.gcode, 1.75, 250.0, 250.0).unwrap();

        let mut cfg_scarf = cfg_base;
        cfg_scarf.process.scarf_seam.enabled = true;
        cfg_scarf.process.scarf_seam.length_mm = 12.0;
        cfg_scarf.process.scarf_seam.steps = 10;
        cfg_scarf.process.scarf_seam.start_height_ratio = 0.10;
        let out_scarf = emit_gcode(&plate, &cfg_scarf).unwrap();
        let (rep_scarf, _) = evaluate_gcode(&out_scarf.gcode, 1.75, 250.0, 250.0).unwrap();

        assert!(rep_scarf.safety.is_clean(), "{:?}", rep_scarf.safety);
        assert_eq!(rep_scarf.safety.z_regressions, 0);
        assert_eq!(rep_scarf.active_layer_count, rep_base.active_layer_count);
        assert!(rep_scarf.scarf_ramp_moves_count > 10);

        let rel_vol_diff =
            (rep_scarf.total_filament_volume_mm3 - rep_base.total_filament_volume_mm3).abs()
                / rep_base.total_filament_volume_mm3.max(1e-6);
        assert!(
            rel_vol_diff < 0.02,
            "scarf volume diff {rel_vol_diff:.4} too large"
        );
    }

    #[test]
    fn variable_width_emission_caps_volumetric_flow_and_tracks_variation() {
        use layercraft_geom::{ExPolygon, Point2, Polygon};
        use layercraft_slice::SlicedLayer;

        let mut cfg = PrintConfig::default();
        cfg.machine.relative_extrusion = true;
        cfg.filament.max_volumetric_speed_mm3_s = 5.0;

        let pts = vec![
            Point2::from_mm(50.0, 50.0).unwrap(),
            Point2::from_mm(60.0, 50.0).unwrap(),
            Point2::from_mm(70.0, 50.0).unwrap(),
            Point2::from_mm(80.0, 50.0).unwrap(),
        ];
        let widths = vec![0.30, 0.45, 0.65, 0.85];
        let plate = SlicedPlate {
            layers: vec![SlicedLayer {
                index: 0,
                z_top_mm: 0.2,
                height_mm: 0.2,
                slice_regions: vec![ExPolygon::new(
                    Polygon::new(vec![
                        Point2::from_mm(49.0, 49.0).unwrap(),
                        Point2::from_mm(81.0, 49.0).unwrap(),
                        Point2::from_mm(81.0, 51.0).unwrap(),
                        Point2::from_mm(49.0, 51.0).unwrap(),
                    ]),
                    vec![],
                )],
                paths: vec![ExtrusionPath {
                    role: ExtrusionRole::OuterWall,
                    width_mm: 0.56,
                    point_widths_mm: widths,
                    height_mm: 0.2,
                    speed_mm_s: 120.0,
                    flow_ratio: 1.0,
                    points: pts,
                    closed: false,
                }],
                fan_percent: 0,
                estimated_time_s: 1.0,
            }],
        };

        let out = emit_gcode(&plate, &cfg).unwrap();
        let (rep, segs) = evaluate_gcode(&out.gcode, 1.75, 250.0, 250.0).unwrap();
        assert!(rep.safety.is_clean(), "{:?}", rep.safety);
        assert!(
            rep.max_volumetric_flow_mm3_s <= cfg.filament.max_volumetric_speed_mm3_s + 0.05,
            "flow {} exceeded cap {}",
            rep.max_volumetric_flow_mm3_s,
            cfg.filament.max_volumetric_speed_mm3_s
        );
        assert!(
            rep.wall_width_variation_ratio > 1.4,
            "variation ratio {} <= 1.4",
            rep.wall_width_variation_ratio
        );
        assert_eq!(segs.len(), 3);
        let area0 = segs[0].volume_mm3 / 10.0;
        let area1 = segs[1].volume_mm3 / 10.0;
        let area2 = segs[2].volume_mm3 / 10.0;
        assert!((area0 - bead_area_mm2(0.375, 0.2)).abs() < 1e-3);
        assert!((area1 - bead_area_mm2(0.55, 0.2)).abs() < 1e-3);
        assert!((area2 - bead_area_mm2(0.75, 0.2)).abs() < 1e-3);

        // Uniform classic box stays < 1.05 even with a thicker first layer height.
        let mut cfg_box = PrintConfig::default();
        cfg_box.process.first_layer_height_mm = 0.28;
        cfg_box.process.layer_height_mm = 0.20;
        let box_plate = slice_single_mesh(&make_box(16.0, 16.0, 1.2), &cfg_box).unwrap();
        let box_out = emit_gcode(&box_plate, &cfg_box).unwrap();
        let (box_rep, _) = evaluate_gcode(&box_out.gcode, 1.75, 250.0, 250.0).unwrap();
        assert!(
            box_rep.wall_width_variation_ratio < 1.05,
            "uniform box reported false variation {}",
            box_rep.wall_width_variation_ratio
        );
    }

    #[test]
    fn arachne_02mm_hollow_cylinder_and_arcs() {
        use layercraft_config::{ProfileLibrary, WallGenerator};
        use layercraft_geom::{Point3, Triangle, TriangleMesh};

        let n = 64usize;
        let outer_r = 8.0;
        let inner_r = 7.70;
        let height = 3.0;
        let mut tris = Vec::with_capacity(n * 8);
        let mut push_q = |a: Point3, b: Point3, c: Point3, d: Point3| {
            tris.push(Triangle::new(a, b, c));
            tris.push(Triangle::new(a, c, d));
        };
        for i in 0..n {
            let a0 = TAU * (i as f64) / (n as f64);
            let a1 = TAU * (((i + 1) % n) as f64) / (n as f64);
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
        let mesh = TriangleMesh::new(tris).unwrap();

        let lib = ProfileLibrary::with_builtin_presets();
        let res = lib
            .build_print_config(
                &PrintConfig::default(),
                Some("LayerCraft CORE One+ HF 0.2mm"),
                Some("LayerCraft PLA (0.2mm HF Nozzle)"),
                Some("LayerCraft 0.10mm Fine (0.2mm Nozzle)"),
            )
            .unwrap();
        let mut cfg = res.config;
        cfg.process.first_layer_height_mm = 0.10;
        cfg.process.wall_generator = WallGenerator::Arachne;
        cfg.process.arc_fitting = true;
        cfg.process.arc_tolerance_mm = 0.025;

        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let out = emit_gcode(&plate, &cfg).unwrap();
        let (rep, _) = evaluate_gcode(
            &out.gcode,
            cfg.filament.diameter_mm,
            cfg.machine.bed_width_mm,
            cfg.machine.bed_depth_mm,
        )
        .unwrap();
        assert!(rep.safety.is_clean(), "{:?}", rep.safety);
        assert_eq!(rep.active_layer_count, 30);
        assert!(rep.arc_moves_count > 10);
        assert!(
            (36.0..=46.0).contains(&rep.total_filament_volume_mm3),
            "unexpected filament volume {}",
            rep.total_filament_volume_mm3
        );
    }

    fn make_dome(radius: f64, base_h: f64, segs: usize, rings: usize) -> TriangleMesh {
        let mut tris = Vec::with_capacity(segs * (rings * 2 + 3));
        let cb = Point3::new(0.0, 0.0, 0.0);
        let mut ring_pts: Vec<Vec<Point3>> = Vec::with_capacity(rings);
        for ri in 0..rings {
            let phi = std::f64::consts::FRAC_PI_2 * (ri as f64) / (rings as f64);
            let rr = radius * phi.cos();
            let zz = base_h + radius * phi.sin();
            let mut pts = Vec::with_capacity(segs);
            for i in 0..segs {
                let a = TAU * (i as f64) / (segs as f64);
                pts.push(Point3::new(rr * a.cos(), rr * a.sin(), zz));
            }
            ring_pts.push(pts);
        }
        let base_top = &ring_pts[0];
        for i in 0..segs {
            let i1 = (i + 1) % segs;
            let a0 = TAU * (i as f64) / (segs as f64);
            let a1 = TAU * (i1 as f64) / (segs as f64);
            let b0 = Point3::new(radius * a0.cos(), radius * a0.sin(), 0.0);
            let b1 = Point3::new(radius * a1.cos(), radius * a1.sin(), 0.0);
            tris.push(Triangle::new(cb, b1, b0));
            tris.push(Triangle::new(b0, b1, base_top[i1]));
            tris.push(Triangle::new(b0, base_top[i1], base_top[i]));
        }
        for ri in 0..rings.saturating_sub(1) {
            let cur = &ring_pts[ri];
            let nxt = &ring_pts[ri + 1];
            for i in 0..segs {
                let i1 = (i + 1) % segs;
                tris.push(Triangle::new(cur[i], cur[i1], nxt[i1]));
                tris.push(Triangle::new(cur[i], nxt[i1], nxt[i]));
            }
        }
        let apex = Point3::new(0.0, 0.0, base_h + radius);
        let last_ring = &ring_pts[rings - 1];
        for i in 0..segs {
            let i1 = (i + 1) % segs;
            tris.push(Triangle::new(last_ring[i], last_ring[i1], apex));
        }
        TriangleMesh::new(tris).unwrap()
    }

    #[test]
    fn evaluate_gcode_reports_variable_and_uniform_layer_height_ratios() {
        let mut cfg_uni = PrintConfig::default();
        cfg_uni.process.first_layer_height_mm = 0.28;
        cfg_uni.process.layer_height_mm = 0.20;
        let cube_plate = slice_single_mesh(&make_box(16.0, 16.0, 4.08), &cfg_uni).unwrap();
        let cube_out = emit_gcode(&cube_plate, &cfg_uni).unwrap();
        let (cube_rep, _) = evaluate_gcode(&cube_out.gcode, 1.75, 250.0, 250.0).unwrap();
        assert!(cube_rep.safety.is_clean(), "{:?}", cube_rep.safety);
        assert!(
            cube_rep.layer_height_variation_ratio < 1.02,
            "uniform cube reported variation ratio {}",
            cube_rep.layer_height_variation_ratio
        );

        let dome = make_dome(10.0, 8.0, 48, 16);
        let mut cfg_adapt = PrintConfig::default();
        cfg_adapt.process.adaptive_layer_height.enabled = true;
        cfg_adapt.process.adaptive_layer_height.min_layer_height_mm = Some(0.08);
        cfg_adapt.process.adaptive_layer_height.max_layer_height_mm = Some(0.28);
        cfg_adapt.process.adaptive_layer_height.quality = 0.6;
        let dome_plate = slice_single_mesh(&dome, &cfg_adapt).unwrap();
        let dome_out = emit_gcode(&dome_plate, &cfg_adapt).unwrap();
        let (dome_rep, _) = evaluate_gcode(&dome_out.gcode, 1.75, 250.0, 250.0).unwrap();
        assert!(dome_rep.safety.is_clean(), "{:?}", dome_rep.safety);
        assert!(
            (dome_rep.max_z_mm - 18.0).abs() < 0.01,
            "unexpected dome max_z {}",
            dome_rep.max_z_mm
        );
        assert!(
            dome_rep.layer_height_variation_ratio > 2.0,
            "ratio={} <= 2.0",
            dome_rep.layer_height_variation_ratio
        );
        assert!(
            dome_rep.min_layer_height_mm <= 0.10,
            "min_h={}",
            dome_rep.min_layer_height_mm
        );
        assert!(
            dome_rep.max_layer_height_mm >= 0.24,
            "max_h={}",
            dome_rep.max_layer_height_mm
        );
    }
}
