//! Typed and validated machine, filament, and print-process configuration.

pub mod profile;
pub use profile::*;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors returned when validating a [`PrintConfig`].
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ConfigError {
    /// A numeric parameter was invalid (`NaN`, infinite, or out of bounds).
    #[error("invalid configuration parameter `{field}`: {reason}")]
    Invalid {
        field: &'static str,
        reason: &'static str,
    },
}

/// Supported G-code firmware dialects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GcodeFlavor {
    #[default]
    Klipper,
    Marlin,
}

/// Supported sparse infill patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InfillPattern {
    Rectilinear,
    #[default]
    Grid,
    Gyroid,
    Cubic,
}

impl InfillPattern {
    /// All supported sparse infill patterns.
    pub const ALL: [Self; 4] = [Self::Rectilinear, Self::Grid, Self::Gyroid, Self::Cubic];

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Rectilinear => "Rectilinear",
            Self::Grid => "Grid",
            Self::Gyroid => "Gyroid",
            Self::Cubic => "Cubic",
        }
    }
}

/// Strategy for choosing seam start locations on closed wall loops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SeamPosition {
    #[default]
    Aligned,
    Back,
    Nearest,
    Random,
}

impl SeamPosition {
    /// All seam alignment modes.
    pub const ALL: [Self; 4] = [Self::Aligned, Self::Back, Self::Nearest, Self::Random];

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Aligned => "Aligned",
            Self::Back => "Back",
            Self::Nearest => "Nearest",
            Self::Random => "Random",
        }
    }
}

/// Machine hardware parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MachineConfig {
    pub flavor: GcodeFlavor,
    pub nozzle_diameter_mm: f64,
    pub bed_width_mm: f64,
    pub bed_depth_mm: f64,
    pub bed_height_mm: f64,
    pub relative_extrusion: bool,
    pub retraction_length_mm: f64,
    pub retraction_speed_mm_s: f64,
    pub travel_speed_mm_s: f64,
    pub z_hop_mm: f64,
    pub z_hop_speed_mm_s: f64,
    pub wipe_on_retract: bool,
    pub wipe_distance_mm: f64,
    pub min_travel_for_retract_mm: f64,
    pub start_gcode: Option<String>,
    pub end_gcode: Option<String>,
    pub layer_change_gcode: Option<String>,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            flavor: GcodeFlavor::Klipper,
            nozzle_diameter_mm: 0.4,
            bed_width_mm: 250.0,
            bed_depth_mm: 250.0,
            bed_height_mm: 250.0,
            relative_extrusion: false,
            retraction_length_mm: 0.8,
            retraction_speed_mm_s: 35.0,
            travel_speed_mm_s: 150.0,
            z_hop_mm: 0.0,
            z_hop_speed_mm_s: 15.0,
            wipe_on_retract: false,
            wipe_distance_mm: 2.0,
            min_travel_for_retract_mm: 1.0,
            start_gcode: None,
            end_gcode: None,
            layer_change_gcode: None,
        }
    }
}

/// Filament material parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct FilamentConfig {
    pub diameter_mm: f64,
    pub density_g_cm3: f64,
    pub nozzle_temp_c: u16,
    pub bed_temp_c: u16,
    pub first_layer_nozzle_temp_c: Option<u16>,
    pub first_layer_bed_temp_c: Option<u16>,
    pub max_volumetric_speed_mm3_s: f64,
    pub fan_min_percent: u8,
    pub fan_max_percent: u8,
    pub disable_fan_first_layers: u16,
    pub full_fan_speed_layer: u16,
    pub slow_down_layer_time_s: f64,
    pub slow_down_min_speed_mm_s: f64,
    pub bridge_fan_percent: u8,
}

impl Default for FilamentConfig {
    fn default() -> Self {
        Self {
            diameter_mm: 1.75,
            density_g_cm3: 1.24,
            nozzle_temp_c: 200,
            bed_temp_c: 60,
            first_layer_nozzle_temp_c: None,
            first_layer_bed_temp_c: None,
            max_volumetric_speed_mm3_s: 15.0,
            fan_min_percent: 35,
            fan_max_percent: 100,
            disable_fan_first_layers: 1,
            full_fan_speed_layer: 4,
            slow_down_layer_time_s: 8.0,
            slow_down_min_speed_mm_s: 15.0,
            bridge_fan_percent: 100,
        }
    }
}

impl FilamentConfig {
    /// Nozzle temperature for layer 0 (`first_layer_nozzle_temp_c` if set, else `nozzle_temp_c`).
    pub fn effective_first_layer_nozzle_temp_c(&self) -> u16 {
        self.first_layer_nozzle_temp_c.unwrap_or(self.nozzle_temp_c)
    }

    /// Bed temperature for layer 0 (`first_layer_bed_temp_c` if set, else `bed_temp_c`).
    pub fn effective_first_layer_bed_temp_c(&self) -> u16 {
        self.first_layer_bed_temp_c.unwrap_or(self.bed_temp_c)
    }
}

/// Slicing process parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ProcessConfig {
    pub layer_height_mm: f64,
    pub first_layer_height_mm: f64,
    pub wall_loops: u16,
    pub top_shell_layers: u16,
    pub bottom_shell_layers: u16,
    /// Fraction in `0.0..=1.0` (e.g. `0.20` for 20%).
    pub sparse_infill_density: f64,
    pub sparse_infill_pattern: InfillPattern,
    pub outer_wall_width_mm: f64,
    pub inner_wall_width_mm: f64,
    pub solid_infill_width_mm: f64,
    pub sparse_infill_width_mm: f64,
    pub outer_wall_speed_mm_s: f64,
    pub inner_wall_speed_mm_s: f64,
    pub infill_speed_mm_s: f64,
    pub first_layer_speed_mm_s: f64,
    pub seam_position: SeamPosition,
    pub first_layer_infill_speed_mm_s: Option<f64>,
    pub top_surface_speed_mm_s: Option<f64>,
    pub solid_infill_speed_mm_s: Option<f64>,
    pub detect_bridges: bool,
    pub bridge_speed_mm_s: f64,
    pub bridge_flow_ratio: f64,
    pub detect_overhangs: bool,
    pub overhang_threshold_deg: f64,
    pub overhang_speed_mm_s: f64,
    pub monotonic_solid_infill: bool,
    pub support: SupportConfig,
    pub skirt_loops: u16,
    pub brim_width_mm: f64,
}

/// Support structure style dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SupportStyle {
    #[default]
    Grid,
    Snug,
}

impl SupportStyle {
    /// All supported support styles.
    pub const ALL: [Self; 2] = [Self::Grid, Self::Snug];

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Grid => "Grid",
            Self::Snug => "Snug",
        }
    }
}

/// Automatic support structure configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SupportConfig {
    pub enabled: bool,
    pub style: SupportStyle,
    pub on_build_plate_only: bool,
    pub threshold_angle_deg: f64,
    pub top_z_distance_mm: f64,
    pub bottom_z_distance_mm: f64,
    pub xy_distance_mm: f64,
    pub base_pattern_spacing_mm: f64,
    pub interface_layers: u16,
    pub interface_spacing_mm: f64,
    pub line_width_mm: f64,
    pub speed_mm_s: f64,
    pub interface_speed_mm_s: f64,
}

impl Default for SupportConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            style: SupportStyle::Grid,
            on_build_plate_only: false,
            threshold_angle_deg: 45.0,
            top_z_distance_mm: 0.2,
            bottom_z_distance_mm: 0.2,
            xy_distance_mm: 0.35,
            base_pattern_spacing_mm: 2.5,
            interface_layers: 2,
            interface_spacing_mm: 0.2,
            line_width_mm: 0.42,
            speed_mm_s: 80.0,
            interface_speed_mm_s: 50.0,
        }
    }
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            layer_height_mm: 0.2,
            first_layer_height_mm: 0.2,
            wall_loops: 2,
            top_shell_layers: 3,
            bottom_shell_layers: 3,
            sparse_infill_density: 0.20,
            sparse_infill_pattern: InfillPattern::Grid,
            outer_wall_width_mm: 0.42,
            inner_wall_width_mm: 0.45,
            solid_infill_width_mm: 0.42,
            sparse_infill_width_mm: 0.45,
            outer_wall_speed_mm_s: 60.0,
            inner_wall_speed_mm_s: 100.0,
            infill_speed_mm_s: 120.0,
            first_layer_speed_mm_s: 30.0,
            seam_position: SeamPosition::Aligned,
            first_layer_infill_speed_mm_s: None,
            top_surface_speed_mm_s: None,
            solid_infill_speed_mm_s: None,
            detect_bridges: true,
            bridge_speed_mm_s: 30.0,
            bridge_flow_ratio: 0.95,
            detect_overhangs: true,
            overhang_threshold_deg: 45.0,
            overhang_speed_mm_s: 25.0,
            monotonic_solid_infill: true,
            support: SupportConfig::default(),
            skirt_loops: 0,
            brim_width_mm: 0.0,
        }
    }
}

impl ProcessConfig {
    /// First-layer infill speed (`first_layer_infill_speed_mm_s` if set, else `first_layer_speed_mm_s`).
    pub fn effective_first_layer_infill_speed_mm_s(&self) -> f64 {
        self.first_layer_infill_speed_mm_s
            .unwrap_or(self.first_layer_speed_mm_s)
    }

    /// Top surface speed (`top_surface_speed_mm_s` if set, else `infill_speed_mm_s`).
    pub fn effective_top_surface_speed_mm_s(&self) -> f64 {
        self.top_surface_speed_mm_s
            .unwrap_or(self.infill_speed_mm_s)
    }

    /// Solid infill / bottom surface speed (`solid_infill_speed_mm_s` if set, else `infill_speed_mm_s`).
    pub fn effective_solid_infill_speed_mm_s(&self) -> f64 {
        self.solid_infill_speed_mm_s
            .unwrap_or(self.infill_speed_mm_s)
    }
}

/// Complete print configuration combining machine, filament, and process settings.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct PrintConfig {
    pub machine: MachineConfig,
    pub filament: FilamentConfig,
    pub process: ProcessConfig,
}

fn check_pos(field: &'static str, v: f64) -> Result<(), ConfigError> {
    if !v.is_finite() || v <= 0.0 {
        return Err(ConfigError::Invalid {
            field,
            reason: "must be finite and > 0",
        });
    }
    Ok(())
}

fn check_non_neg(field: &'static str, v: f64) -> Result<(), ConfigError> {
    if !v.is_finite() || v < 0.0 {
        return Err(ConfigError::Invalid {
            field,
            reason: "must be finite and >= 0",
        });
    }
    Ok(())
}

impl PrintConfig {
    /// Validate all physical bounds on the print configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        check_pos(
            "machine.nozzle_diameter_mm",
            self.machine.nozzle_diameter_mm,
        )?;
        check_pos("machine.bed_width_mm", self.machine.bed_width_mm)?;
        check_pos("machine.bed_depth_mm", self.machine.bed_depth_mm)?;
        check_pos("machine.bed_height_mm", self.machine.bed_height_mm)?;
        check_non_neg(
            "machine.retraction_length_mm",
            self.machine.retraction_length_mm,
        )?;
        check_pos(
            "machine.retraction_speed_mm_s",
            self.machine.retraction_speed_mm_s,
        )?;
        check_pos("machine.travel_speed_mm_s", self.machine.travel_speed_mm_s)?;
        check_non_neg("machine.z_hop_mm", self.machine.z_hop_mm)?;
        if self.machine.z_hop_mm > 50.0 {
            return Err(ConfigError::Invalid {
                field: "machine.z_hop_mm",
                reason: "exceeds 50.0 mm",
            });
        }
        check_pos("machine.z_hop_speed_mm_s", self.machine.z_hop_speed_mm_s)?;
        check_non_neg("machine.wipe_distance_mm", self.machine.wipe_distance_mm)?;
        if self.machine.wipe_distance_mm > 50.0 {
            return Err(ConfigError::Invalid {
                field: "machine.wipe_distance_mm",
                reason: "exceeds 50.0 mm",
            });
        }
        check_non_neg(
            "machine.min_travel_for_retract_mm",
            self.machine.min_travel_for_retract_mm,
        )?;

        check_pos("filament.diameter_mm", self.filament.diameter_mm)?;
        check_pos("filament.density_g_cm3", self.filament.density_g_cm3)?;
        check_pos(
            "filament.max_volumetric_speed_mm3_s",
            self.filament.max_volumetric_speed_mm3_s,
        )?;
        if self.filament.fan_min_percent > 100 {
            return Err(ConfigError::Invalid {
                field: "filament.fan_min_percent",
                reason: "must be <= 100",
            });
        }
        if self.filament.fan_max_percent > 100 {
            return Err(ConfigError::Invalid {
                field: "filament.fan_max_percent",
                reason: "must be <= 100",
            });
        }
        if self.filament.fan_min_percent > self.filament.fan_max_percent {
            return Err(ConfigError::Invalid {
                field: "filament.fan_min_percent",
                reason: "must be <= fan_max_percent",
            });
        }
        check_non_neg(
            "filament.slow_down_layer_time_s",
            self.filament.slow_down_layer_time_s,
        )?;
        check_pos(
            "filament.slow_down_min_speed_mm_s",
            self.filament.slow_down_min_speed_mm_s,
        )?;
        if self.filament.bridge_fan_percent > 100 {
            return Err(ConfigError::Invalid {
                field: "filament.bridge_fan_percent",
                reason: "must be <= 100",
            });
        }

        check_pos("process.layer_height_mm", self.process.layer_height_mm)?;
        check_pos(
            "process.first_layer_height_mm",
            self.process.first_layer_height_mm,
        )?;
        if !self.process.sparse_infill_density.is_finite()
            || !(0.0..=1.0).contains(&self.process.sparse_infill_density)
        {
            return Err(ConfigError::Invalid {
                field: "process.sparse_infill_density",
                reason: "must be within 0.0..=1.0",
            });
        }
        check_pos(
            "process.outer_wall_width_mm",
            self.process.outer_wall_width_mm,
        )?;
        check_pos(
            "process.inner_wall_width_mm",
            self.process.inner_wall_width_mm,
        )?;
        check_pos(
            "process.solid_infill_width_mm",
            self.process.solid_infill_width_mm,
        )?;
        check_pos(
            "process.sparse_infill_width_mm",
            self.process.sparse_infill_width_mm,
        )?;
        check_pos(
            "process.outer_wall_speed_mm_s",
            self.process.outer_wall_speed_mm_s,
        )?;
        check_pos(
            "process.inner_wall_speed_mm_s",
            self.process.inner_wall_speed_mm_s,
        )?;
        check_pos("process.infill_speed_mm_s", self.process.infill_speed_mm_s)?;
        check_pos(
            "process.first_layer_speed_mm_s",
            self.process.first_layer_speed_mm_s,
        )?;
        for (field, opt_val) in [
            (
                "process.first_layer_infill_speed_mm_s",
                self.process.first_layer_infill_speed_mm_s,
            ),
            (
                "process.top_surface_speed_mm_s",
                self.process.top_surface_speed_mm_s,
            ),
            (
                "process.solid_infill_speed_mm_s",
                self.process.solid_infill_speed_mm_s,
            ),
        ] {
            if let Some(v) = opt_val {
                check_pos(field, v)?;
            }
        }
        check_pos("process.bridge_speed_mm_s", self.process.bridge_speed_mm_s)?;
        if !self.process.bridge_flow_ratio.is_finite()
            || !(0.1..=2.0).contains(&self.process.bridge_flow_ratio)
        {
            return Err(ConfigError::Invalid {
                field: "process.bridge_flow_ratio",
                reason: "must be within 0.1..=2.0",
            });
        }
        if !self.process.overhang_threshold_deg.is_finite()
            || !(10.0..=85.0).contains(&self.process.overhang_threshold_deg)
        {
            return Err(ConfigError::Invalid {
                field: "process.overhang_threshold_deg",
                reason: "must be within 10.0..=85.0",
            });
        }
        check_pos(
            "process.overhang_speed_mm_s",
            self.process.overhang_speed_mm_s,
        )?;
        let sup = &self.process.support;
        for (field, val, min, max) in [
            (
                "process.support.threshold_angle_deg",
                sup.threshold_angle_deg,
                10.0,
                85.0,
            ),
            (
                "process.support.top_z_distance_mm",
                sup.top_z_distance_mm,
                0.0,
                2.0,
            ),
            (
                "process.support.bottom_z_distance_mm",
                sup.bottom_z_distance_mm,
                0.0,
                2.0,
            ),
            (
                "process.support.xy_distance_mm",
                sup.xy_distance_mm,
                0.0,
                5.0,
            ),
            (
                "process.support.base_pattern_spacing_mm",
                sup.base_pattern_spacing_mm,
                0.5,
                20.0,
            ),
            (
                "process.support.interface_spacing_mm",
                sup.interface_spacing_mm,
                0.0,
                5.0,
            ),
        ] {
            if !val.is_finite() || !(min..=max).contains(&val) {
                return Err(ConfigError::Invalid {
                    field,
                    reason: "out of valid range",
                });
            }
        }
        check_pos("process.support.line_width_mm", sup.line_width_mm)?;
        check_pos("process.support.speed_mm_s", sup.speed_mm_s)?;
        check_pos(
            "process.support.interface_speed_mm_s",
            sup.interface_speed_mm_s,
        )?;
        check_non_neg("process.brim_width_mm", self.process.brim_width_mm)?;

        if self.process.layer_height_mm > self.machine.nozzle_diameter_mm * 1.25 {
            return Err(ConfigError::Invalid {
                field: "process.layer_height_mm",
                reason: "exceeds 1.25x nozzle diameter",
            });
        }
        if self.process.first_layer_height_mm > self.machine.nozzle_diameter_mm * 1.25 {
            return Err(ConfigError::Invalid {
                field: "process.first_layer_height_mm",
                reason: "exceeds 1.25x nozzle diameter",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let cfg = PrintConfig::default();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.filament.effective_first_layer_nozzle_temp_c(), 200);
        assert_eq!(cfg.filament.effective_first_layer_bed_temp_c(), 60);
        assert_eq!(cfg.process.seam_position, SeamPosition::Aligned);
    }

    #[test]
    fn legacy_partial_json_deserializes_with_defaults() {
        let legacy = r#"{
            "machine": { "flavor": "klipper", "nozzle_diameter_mm": 0.4 },
            "filament": { "nozzle_temp_c": 215, "bed_temp_c": 65 },
            "process": { "layer_height_mm": 0.2, "wall_loops": 3 }
        }"#;
        let cfg: PrintConfig = serde_json::from_str(legacy).unwrap();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.filament.effective_first_layer_nozzle_temp_c(), 215);
        assert_eq!(cfg.filament.effective_first_layer_bed_temp_c(), 65);
        assert_eq!(cfg.filament.max_volumetric_speed_mm3_s, 15.0);
        assert_eq!(cfg.filament.fan_min_percent, 35);
        assert_eq!(cfg.filament.fan_max_percent, 100);
        assert_eq!(cfg.filament.bridge_fan_percent, 100);
        assert_eq!(cfg.machine.z_hop_mm, 0.0);
        assert_eq!(cfg.process.seam_position, SeamPosition::Aligned);
        assert!(cfg.process.detect_bridges);
        assert!(cfg.process.detect_overhangs);
        assert!(cfg.process.monotonic_solid_infill);
    }

    #[test]
    fn hostile_configs_fail_cleanly() {
        let mut cfg = PrintConfig::default();
        cfg.process.layer_height_mm = 0.0;
        assert!(cfg.validate().is_err());

        let mut cfg2 = PrintConfig::default();
        cfg2.process.sparse_infill_density = 1.5;
        assert!(cfg2.validate().is_err());

        let mut cfg3 = PrintConfig::default();
        cfg3.machine.bed_width_mm = f64::NAN;
        assert!(cfg3.validate().is_err());

        let mut cfg4 = PrintConfig::default();
        cfg4.machine.z_hop_mm = -0.1;
        assert!(cfg4.validate().is_err());
        cfg4.machine.z_hop_mm = 60.0;
        assert!(cfg4.validate().is_err());

        let mut cfg5 = PrintConfig::default();
        cfg5.machine.wipe_distance_mm = 51.0;
        assert!(cfg5.validate().is_err());

        let mut cfg6 = PrintConfig::default();
        cfg6.filament.fan_min_percent = 101;
        assert!(cfg6.validate().is_err());
        cfg6.filament.fan_min_percent = 80;
        cfg6.filament.fan_max_percent = 40;
        assert!(cfg6.validate().is_err());

        let mut cfg7 = PrintConfig::default();
        cfg7.filament.max_volumetric_speed_mm3_s = 0.0;
        assert!(cfg7.validate().is_err());

        let mut cfg8 = PrintConfig::default();
        cfg8.process.top_surface_speed_mm_s = Some(-5.0);
        assert!(cfg8.validate().is_err());

        let mut cfg9 = PrintConfig::default();
        cfg9.filament.bridge_fan_percent = 120;
        assert!(cfg9.validate().is_err());

        let mut cfg10 = PrintConfig::default();
        cfg10.process.bridge_flow_ratio = 0.05;
        assert!(cfg10.validate().is_err());
        cfg10.process.bridge_flow_ratio = 2.5;
        assert!(cfg10.validate().is_err());

        let mut cfg11 = PrintConfig::default();
        cfg11.process.overhang_threshold_deg = 5.0;
        assert!(cfg11.validate().is_err());
        cfg11.process.overhang_threshold_deg = 89.0;
        assert!(cfg11.validate().is_err());

        let mut cfg12 = PrintConfig::default();
        cfg12.process.bridge_speed_mm_s = 0.0;
        assert!(cfg12.validate().is_err());

        let mut cfg13 = PrintConfig::default();
        cfg13.process.overhang_speed_mm_s = -10.0;
        assert!(cfg13.validate().is_err());

        let mut cfg14 = PrintConfig::default();
        cfg14.process.support.threshold_angle_deg = 90.0;
        assert!(cfg14.validate().is_err());
        cfg14.process.support.threshold_angle_deg = 45.0;
        cfg14.process.support.top_z_distance_mm = 3.0;
        assert!(cfg14.validate().is_err());
        cfg14.process.support.top_z_distance_mm = 0.2;
        cfg14.process.support.xy_distance_mm = -0.1;
        assert!(cfg14.validate().is_err());
        cfg14.process.support.xy_distance_mm = 0.35;
        cfg14.process.support.base_pattern_spacing_mm = 0.2;
        assert!(cfg14.validate().is_err());
        cfg14.process.support.base_pattern_spacing_mm = 2.5;
        cfg14.process.support.line_width_mm = 0.0;
        assert!(cfg14.validate().is_err());
        cfg14.process.support.line_width_mm = 0.42;
        cfg14.process.support.interface_speed_mm_s = f64::NAN;
        assert!(cfg14.validate().is_err());
    }
}
