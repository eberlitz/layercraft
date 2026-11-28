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
    #[serde(alias = "buddy", alias = "prusa_buddy")]
    MarlinBuddy,
}

impl GcodeFlavor {
    /// All supported G-code firmware dialects.
    pub const ALL: [Self; 3] = [Self::Klipper, Self::Marlin, Self::MarlinBuddy];

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Klipper => "Klipper",
            Self::Marlin => "Marlin",
            Self::MarlinBuddy => "Marlin / Prusa Buddy",
        }
    }
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

/// Emission sequence for perimeter wall contours on each island.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WallOrder {
    #[default]
    InnerOuter,
    OuterInner,
    InnerOuterInner,
}

impl WallOrder {
    /// All supported perimeter wall emission orderings.
    pub const ALL: [Self; 3] = [Self::InnerOuter, Self::OuterInner, Self::InnerOuterInner];

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::InnerOuter => "Inner / Outer",
            Self::OuterInner => "Outer / Inner",
            Self::InnerOuterInner => "Inner / Outer / Inner",
        }
    }
}

/// Fill pattern used for exposed top and bottom skin surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SurfacePattern {
    #[default]
    Monotonic,
    Rectilinear,
    Concentric,
}

impl SurfacePattern {
    /// All supported top/bottom solid surface patterns.
    pub const ALL: [Self; 3] = [Self::Monotonic, Self::Rectilinear, Self::Concentric];

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Monotonic => "Monotonic",
            Self::Rectilinear => "Rectilinear",
            Self::Concentric => "Concentric",
        }
    }
}

/// Image encoding format for embedded G-code preview thumbnails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThumbnailFormat {
    #[default]
    Png,
    Qoi,
}

/// Dimensions (in pixels) and encoding format of a preview thumbnail embedded into G-code headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ThumbnailSize {
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub format: ThumbnailFormat,
}

impl ThumbnailSize {
    /// Construct a PNG thumbnail descriptor.
    pub const fn png(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            format: ThumbnailFormat::Png,
        }
    }

    /// Construct a QOI thumbnail descriptor (Prusa Buddy LCD).
    pub const fn qoi(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            format: ThumbnailFormat::Qoi,
        }
    }
}

/// Machine hardware parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MachineConfig {
    pub flavor: GcodeFlavor,
    pub nozzle_diameter_mm: f64,
    pub nozzle_high_flow: bool,
    pub printer_model: Option<String>,
    pub bed_width_mm: f64,
    pub bed_depth_mm: f64,
    pub bed_height_mm: f64,
    pub relative_extrusion: bool,
    pub emit_progress_m73: bool,
    pub binary_gcode: bool,
    pub thumbnails: Vec<ThumbnailSize>,
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
    /// Emit object boundary & definition markers (`EXCLUDE_OBJECT_*` / `M486`) when slicing objects.
    pub label_objects: bool,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            flavor: GcodeFlavor::Klipper,
            nozzle_diameter_mm: 0.4,
            nozzle_high_flow: false,
            printer_model: None,
            bed_width_mm: 250.0,
            bed_depth_mm: 250.0,
            bed_height_mm: 250.0,
            relative_extrusion: false,
            emit_progress_m73: false,
            binary_gcode: false,
            thumbnails: Vec::new(),
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
            label_objects: true,
        }
    }
}

/// Filament material parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct FilamentConfig {
    pub filament_type: Option<String>,
    pub diameter_mm: f64,
    pub density_g_cm3: f64,
    pub nozzle_temp_c: u16,
    pub bed_temp_c: u16,
    pub first_layer_nozzle_temp_c: Option<u16>,
    pub first_layer_bed_temp_c: Option<u16>,
    pub chamber_temp_c: Option<u16>,
    pub wait_for_chamber_temp: bool,
    pub pressure_advance: Option<f64>,
    pub flow_ratio: f64,
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
            filament_type: None,
            diameter_mm: 1.75,
            density_g_cm3: 1.24,
            nozzle_temp_c: 200,
            bed_temp_c: 60,
            first_layer_nozzle_temp_c: None,
            first_layer_bed_temp_c: None,
            chamber_temp_c: None,
            wait_for_chamber_temp: false,
            pressure_advance: None,
            flow_ratio: 1.0,
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

/// Strategy for generating perimeter walls around slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WallGenerator {
    #[default]
    Classic,
    Arachne,
}

impl WallGenerator {
    /// All supported perimeter wall generators.
    pub const ALL: [Self; 2] = [Self::Classic, Self::Arachne];

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::Arachne => "Arachne",
        }
    }
}

/// Variable-width perimeter wall (Arachne) configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ArachneConfig {
    /// Minimum extrusion bead width expressed as a fraction of nominal wall width (`0.20..=1.00`).
    pub min_bead_width_ratio: f64,
    /// Maximum extrusion bead width expressed as a fraction of nominal wall width (`1.00..=3.00`).
    pub max_bead_width_ratio: f64,
    /// Minimum thin feature thickness expressed as a fraction of nominal outer wall width (`0.05..=1.00`).
    pub min_feature_size_ratio: f64,
    /// Number of innermost wall loops across which extra width deficit/surplus is distributed (`1..=8`).
    pub wall_distribution_count: u16,
    /// Wall transition length expressed as a fraction of nominal line width (`0.10..=5.00`).
    pub transition_length_ratio: f64,
    /// Threshold wall angle (degrees) for width transition placement (`0.0..=60.0`).
    pub transition_threshold_angle_deg: f64,
    /// Maximum deviation filter ratio during transition cleanup (`0.0..=1.0`).
    pub transition_filter_deviation_ratio: f64,
}

impl Default for ArachneConfig {
    fn default() -> Self {
        Self {
            min_bead_width_ratio: 0.85,
            max_bead_width_ratio: 2.00,
            min_feature_size_ratio: 0.25,
            wall_distribution_count: 1,
            transition_length_ratio: 1.00,
            transition_threshold_angle_deg: 10.0,
            transition_filter_deviation_ratio: 0.25,
        }
    }
}

/// Curvature-driven adaptive layer height settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AdaptiveLayerConfig {
    /// Enable curvature-driven adaptive layer heights.
    pub enabled: bool,
    /// Optional minimum adaptive layer height in mm (`None` = `0.20 * nozzle_diameter_mm`, >= 0.04).
    pub min_layer_height_mm: Option<f64>,
    /// Optional maximum adaptive layer height in mm (`None` = `max(nominal, 0.75 * nozzle_diameter_mm)`).
    pub max_layer_height_mm: Option<f64>,
    /// Speed vs surface-quality bias (`0.0` = fastest / coarser, `1.0` = finest detail).
    pub quality: f64,
    /// 1D Gaussian Z-smoothing radius in mm across layer height targets (`0.0..=20.0`).
    pub smooth_radius_mm: f64,
    /// Maximum multiplicative step ratio between adjacent adaptive layers (`1.05..=3.0`).
    pub max_step_ratio: f64,
}

impl Default for AdaptiveLayerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_layer_height_mm: None,
            max_layer_height_mm: None,
            quality: 0.5,
            smooth_radius_mm: 2.0,
            max_step_ratio: 1.5,
        }
    }
}

impl AdaptiveLayerConfig {
    /// Effective maximum layer height (mm), clamped to physical nozzle bounds and >= min.
    pub fn effective_max_layer_height_mm(&self, nozzle_d: f64, nominal_h: f64) -> f64 {
        let max_limit = (1.25 * nozzle_d).max(0.02);
        let default_max = (0.75 * nozzle_d).max(nominal_h).min(max_limit);
        let eff_max = self
            .max_layer_height_mm
            .unwrap_or(default_max)
            .clamp(0.02, max_limit);
        if let Some(explicit_min) = self.min_layer_height_mm {
            eff_max.max(explicit_min.clamp(0.02, max_limit))
        } else {
            eff_max
        }
    }

    /// Effective minimum layer height (mm), guaranteed `<= effective_max_layer_height_mm`.
    pub fn effective_min_layer_height_mm(&self, nozzle_d: f64, nominal_h: f64) -> f64 {
        let eff_max = self.effective_max_layer_height_mm(nozzle_d, nominal_h);
        let default_min = (0.20 * nozzle_d)
            .max(0.04)
            .min(nominal_h.max(0.02))
            .min(eff_max);
        self.min_layer_height_mm
            .unwrap_or(default_min)
            .clamp(0.02, eff_max)
    }
}

/// Functional role of a 3D modifier volume on the build plate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModifierRole {
    /// Override local print parameters inside the volume.
    #[default]
    Modifier,
    /// Suppress automatic support generation and contact placement inside the volume.
    SupportBlocker,
    /// Force support generation for overhangs intersecting the volume.
    SupportEnforcer,
    /// Subtract boolean volume from positive parts before slicing.
    NegativePart,
}

impl ModifierRole {
    /// All supported modifier volume roles.
    pub const ALL: [Self; 4] = [
        Self::Modifier,
        Self::SupportBlocker,
        Self::SupportEnforcer,
        Self::NegativePart,
    ];

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Modifier => "Modifier",
            Self::SupportBlocker => "Support blocker",
            Self::SupportEnforcer => "Support enforcer",
            Self::NegativePart => "Negative part",
        }
    }
}

/// Plate print ordering strategy when multiple objects exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrintSequence {
    #[default]
    ByLayer,
    ByObject,
}

impl PrintSequence {
    /// All supported build-plate print sequences.
    pub const ALL: [Self; 2] = [Self::ByLayer, Self::ByObject];

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::ByLayer => "By layer",
            Self::ByObject => "By object",
        }
    }
}

/// Mid-print action triggered when reaching a configured Z height.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ZEventAction {
    #[default]
    Pause,
    FilamentChange,
    CustomGcode,
}

impl ZEventAction {
    /// All supported per-Z event actions.
    pub const ALL: [Self; 3] = [Self::Pause, Self::FilamentChange, Self::CustomGcode];

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pause => "Pause",
            Self::FilamentChange => "Filament Change (M600)",
            Self::CustomGcode => "Custom G-code",
        }
    }
}

/// User-scheduled mid-print action emitted at `z_mm`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct LayerZEvent {
    pub z_mm: f64,
    pub action: ZEventAction,
    pub message: Option<String>,
    pub custom_gcode: Option<String>,
}

impl Default for LayerZEvent {
    fn default() -> Self {
        Self {
            z_mm: 5.0,
            action: ZEventAction::Pause,
            message: None,
            custom_gcode: None,
        }
    }
}

/// Optional per-region process parameter overrides applied by height ranges or 3D modifier volumes.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ModifierOverrides {
    pub sparse_infill_density: Option<f64>,
    pub sparse_infill_pattern: Option<InfillPattern>,
    pub wall_loops: Option<u16>,
    pub top_shell_layers: Option<u16>,
    pub bottom_shell_layers: Option<u16>,
    pub ironing_mode: Option<IroningType>,
    pub infill_speed_mm_s: Option<f64>,
    pub outer_wall_speed_mm_s: Option<f64>,
    pub inner_wall_speed_mm_s: Option<f64>,
}

impl ModifierOverrides {
    /// Returns `true` if no local parameter override field is set.
    pub fn is_empty(&self) -> bool {
        self.sparse_infill_density.is_none()
            && self.sparse_infill_pattern.is_none()
            && self.wall_loops.is_none()
            && self.top_shell_layers.is_none()
            && self.bottom_shell_layers.is_none()
            && self.ironing_mode.is_none()
            && self.infill_speed_mm_s.is_none()
            && self.outer_wall_speed_mm_s.is_none()
            && self.inner_wall_speed_mm_s.is_none()
    }

    /// Returns `true` if any perimeter wall count or speed override is set.
    pub fn affects_perimeters(&self) -> bool {
        self.wall_loops.is_some()
            || self.outer_wall_speed_mm_s.is_some()
            || self.inner_wall_speed_mm_s.is_some()
    }

    /// Returns `true` if top or bottom shell counts are overridden.
    pub fn affects_shells(&self) -> bool {
        self.top_shell_layers.is_some() || self.bottom_shell_layers.is_some()
    }

    /// Merge set fields from `other`, giving `other` precedence over existing values.
    pub fn merge_from(&mut self, other: &Self) {
        if let Some(v) = other.sparse_infill_density {
            self.sparse_infill_density = Some(v);
        }
        if let Some(v) = other.sparse_infill_pattern {
            self.sparse_infill_pattern = Some(v);
        }
        if let Some(v) = other.wall_loops {
            self.wall_loops = Some(v);
        }
        if let Some(v) = other.top_shell_layers {
            self.top_shell_layers = Some(v);
        }
        if let Some(v) = other.bottom_shell_layers {
            self.bottom_shell_layers = Some(v);
        }
        if let Some(v) = other.ironing_mode {
            self.ironing_mode = Some(v);
        }
        if let Some(v) = other.infill_speed_mm_s {
            self.infill_speed_mm_s = Some(v);
        }
        if let Some(v) = other.outer_wall_speed_mm_s {
            self.outer_wall_speed_mm_s = Some(v);
        }
        if let Some(v) = other.inner_wall_speed_mm_s {
            self.inner_wall_speed_mm_s = Some(v);
        }
    }

    /// Validate numeric bounds on all set override fields.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(d) = self.sparse_infill_density
            && (!d.is_finite() || !(0.0..=1.0).contains(&d))
        {
            return Err(ConfigError::Invalid {
                field: "modifier.overrides.sparse_infill_density",
                reason: "must be within 0.0..=1.0",
            });
        }
        if let Some(w) = self.wall_loops
            && w > 20
        {
            return Err(ConfigError::Invalid {
                field: "modifier.overrides.wall_loops",
                reason: "must be <= 20",
            });
        }
        if let Some(t) = self.top_shell_layers
            && t > 20
        {
            return Err(ConfigError::Invalid {
                field: "modifier.overrides.top_shell_layers",
                reason: "must be <= 20",
            });
        }
        if let Some(b) = self.bottom_shell_layers
            && b > 20
        {
            return Err(ConfigError::Invalid {
                field: "modifier.overrides.bottom_shell_layers",
                reason: "must be <= 20",
            });
        }
        for (field, opt_v) in [
            (
                "modifier.overrides.infill_speed_mm_s",
                self.infill_speed_mm_s,
            ),
            (
                "modifier.overrides.outer_wall_speed_mm_s",
                self.outer_wall_speed_mm_s,
            ),
            (
                "modifier.overrides.inner_wall_speed_mm_s",
                self.inner_wall_speed_mm_s,
            ),
        ] {
            if let Some(v) = opt_v {
                check_pos(field, v)?;
            }
        }
        Ok(())
    }
}

/// Explicit Z-height band overriding layer height and/or local slicing settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct HeightRangeModifier {
    pub min_z_mm: f64,
    pub max_z_mm: f64,
    pub layer_height_mm: Option<f64>,
    pub overrides: ModifierOverrides,
}

impl Default for HeightRangeModifier {
    fn default() -> Self {
        Self {
            min_z_mm: 0.0,
            max_z_mm: 1.0,
            layer_height_mm: None,
            overrides: ModifierOverrides::default(),
        }
    }
}

/// Slicing process parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ProcessConfig {
    pub layer_height_mm: f64,
    pub first_layer_height_mm: f64,
    pub wall_loops: u16,
    pub wall_order: WallOrder,
    pub wall_generator: WallGenerator,
    pub arachne: ArachneConfig,
    pub top_shell_layers: u16,
    pub bottom_shell_layers: u16,
    pub top_shell_thickness_mm: Option<f64>,
    pub bottom_shell_thickness_mm: Option<f64>,
    pub top_surface_pattern: SurfacePattern,
    pub bottom_surface_pattern: SurfacePattern,
    pub elephant_foot_compensation_mm: f64,
    pub xy_contour_compensation_mm: f64,
    pub xy_hole_compensation_mm: f64,
    pub avoid_crossing_perimeters: bool,
    pub avoid_crossing_perimeters_max_detour_mm: f64,
    pub reduce_infill_retraction: bool,
    pub adaptive_layer_height: AdaptiveLayerConfig,
    pub height_ranges: Vec<HeightRangeModifier>,
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
    pub default_acceleration_mm_s2: Option<f64>,
    pub outer_wall_acceleration_mm_s2: Option<f64>,
    pub inner_wall_acceleration_mm_s2: Option<f64>,
    pub first_layer_acceleration_mm_s2: Option<f64>,
    pub travel_acceleration_mm_s2: Option<f64>,
    pub ironing: IroningConfig,
    pub scarf_seam: ScarfSeamConfig,
    pub arc_fitting: bool,
    pub arc_tolerance_mm: f64,
    pub support: SupportConfig,
    pub skirt_loops: u16,
    pub brim_width_mm: f64,
    pub print_sequence: PrintSequence,
    pub extruder_clearance_radius_mm: f64,
    pub extruder_clearance_height_mm: f64,
    pub infill_wall_overlap_ratio: f64,
    pub gap_fill_enabled: bool,
    pub gap_fill_speed_mm_s: Option<f64>,
    pub gap_fill_min_width_mm: f64,
    pub overhang_1_4_speed_mm_s: Option<f64>,
    pub overhang_2_4_speed_mm_s: Option<f64>,
    pub overhang_4_4_speed_mm_s: Option<f64>,
    pub z_events: Vec<LayerZEvent>,
}

/// Top-surface ironing scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IroningType {
    #[default]
    Off,
    TopmostSurface,
    AllTopSurfaces,
}

impl IroningType {
    /// All supported ironing modes.
    pub const ALL: [Self; 3] = [Self::Off, Self::TopmostSurface, Self::AllTopSurfaces];

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::TopmostSurface => "Topmost surface",
            Self::AllTopSurfaces => "All top surfaces",
        }
    }
}

/// Top-surface ironing pass parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct IroningConfig {
    pub mode: IroningType,
    pub flow_ratio: f64,
    pub line_spacing_mm: f64,
    pub inset_mm: f64,
    pub speed_mm_s: f64,
    pub angle_offset_deg: f64,
}

impl Default for IroningConfig {
    fn default() -> Self {
        Self {
            mode: IroningType::Off,
            flow_ratio: 0.15,
            line_spacing_mm: 0.15,
            inset_mm: 0.25,
            speed_mm_s: 30.0,
            angle_offset_deg: 45.0,
        }
    }
}

/// Ramped outer-wall scarf seam parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ScarfSeamConfig {
    pub enabled: bool,
    pub length_mm: f64,
    pub min_length_mm: f64,
    pub steps: u16,
    pub start_height_ratio: f64,
    pub flow_ratio: f64,
    pub entire_loop: bool,
}

impl Default for ScarfSeamConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            length_mm: 20.0,
            min_length_mm: 5.0,
            steps: 20,
            start_height_ratio: 0.05,
            flow_ratio: 0.90,
            entire_loop: false,
        }
    }
}

/// Support structure style dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SupportStyle {
    #[default]
    Grid,
    Snug,
    #[serde(alias = "organic", alias = "tree_auto", alias = "tree_organic")]
    Tree,
}

impl SupportStyle {
    /// All supported support styles.
    pub const ALL: [Self; 3] = [Self::Grid, Self::Snug, Self::Tree];

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Grid => "Grid",
            Self::Snug => "Snug",
            Self::Tree => "Tree (Organic)",
        }
    }
}

/// Organic / branching tree support parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct TreeSupportConfig {
    pub max_branch_angle_deg: f64,
    pub tip_diameter_mm: f64,
    pub branch_diameter_mm: f64,
    pub max_trunk_diameter_mm: f64,
    pub branch_diameter_angle_deg: f64,
    pub branch_distance_mm: f64,
    pub branch_wall_loops: u16,
    pub trunk_base_flare_mm: f64,
}

impl Default for TreeSupportConfig {
    fn default() -> Self {
        Self {
            max_branch_angle_deg: 45.0,
            tip_diameter_mm: 1.2,
            branch_diameter_mm: 2.0,
            max_trunk_diameter_mm: 8.0,
            branch_diameter_angle_deg: 5.0,
            branch_distance_mm: 3.0,
            branch_wall_loops: 2,
            trunk_base_flare_mm: 1.5,
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
    pub tree: TreeSupportConfig,
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
            tree: TreeSupportConfig::default(),
        }
    }
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            layer_height_mm: 0.2,
            first_layer_height_mm: 0.2,
            wall_loops: 2,
            wall_order: WallOrder::InnerOuter,
            wall_generator: WallGenerator::Classic,
            arachne: ArachneConfig::default(),
            top_shell_layers: 3,
            bottom_shell_layers: 3,
            top_shell_thickness_mm: None,
            bottom_shell_thickness_mm: None,
            top_surface_pattern: SurfacePattern::Monotonic,
            bottom_surface_pattern: SurfacePattern::Monotonic,
            elephant_foot_compensation_mm: 0.0,
            xy_contour_compensation_mm: 0.0,
            xy_hole_compensation_mm: 0.0,
            avoid_crossing_perimeters: false,
            avoid_crossing_perimeters_max_detour_mm: 0.0,
            reduce_infill_retraction: true,
            adaptive_layer_height: AdaptiveLayerConfig::default(),
            height_ranges: Vec::new(),
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
            default_acceleration_mm_s2: None,
            outer_wall_acceleration_mm_s2: None,
            inner_wall_acceleration_mm_s2: None,
            first_layer_acceleration_mm_s2: None,
            travel_acceleration_mm_s2: None,
            ironing: IroningConfig::default(),
            scarf_seam: ScarfSeamConfig::default(),
            arc_fitting: false,
            arc_tolerance_mm: 0.025,
            support: SupportConfig::default(),
            skirt_loops: 0,
            brim_width_mm: 0.0,
            print_sequence: PrintSequence::ByLayer,
            extruder_clearance_radius_mm: 35.0,
            extruder_clearance_height_mm: 25.0,
            infill_wall_overlap_ratio: 0.15,
            gap_fill_enabled: false,
            gap_fill_speed_mm_s: None,
            gap_fill_min_width_mm: 0.15,
            overhang_1_4_speed_mm_s: None,
            overhang_2_4_speed_mm_s: None,
            overhang_4_4_speed_mm_s: None,
            z_events: Vec::new(),
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

    /// Resolved top surface fill pattern, honoring legacy `monotonic_solid_infill = false`.
    pub fn effective_top_surface_pattern(&self) -> SurfacePattern {
        if self.top_surface_pattern == SurfacePattern::Monotonic && !self.monotonic_solid_infill {
            SurfacePattern::Rectilinear
        } else {
            self.top_surface_pattern
        }
    }

    /// Resolved bottom surface fill pattern, honoring legacy `monotonic_solid_infill = false`.
    pub fn effective_bottom_surface_pattern(&self) -> SurfacePattern {
        if self.bottom_surface_pattern == SurfacePattern::Monotonic && !self.monotonic_solid_infill
        {
            SurfacePattern::Rectilinear
        } else {
            self.bottom_surface_pattern
        }
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
        if self.machine.thumbnails.len() > 4 {
            return Err(ConfigError::Invalid {
                field: "machine.thumbnails",
                reason: "at most 4 embedded thumbnail sizes supported",
            });
        }
        for thumb in &self.machine.thumbnails {
            if !(16..=640).contains(&thumb.width) || !(16..=640).contains(&thumb.height) {
                return Err(ConfigError::Invalid {
                    field: "machine.thumbnails",
                    reason: "thumbnail width and height must be within 16..=640 px",
                });
            }
        }

        check_pos("filament.diameter_mm", self.filament.diameter_mm)?;
        check_pos("filament.density_g_cm3", self.filament.density_g_cm3)?;
        if !self.filament.flow_ratio.is_finite() || !(0.5..=1.5).contains(&self.filament.flow_ratio)
        {
            return Err(ConfigError::Invalid {
                field: "filament.flow_ratio",
                reason: "must be finite and within 0.5..=1.5",
            });
        }
        if let Some(pa) = self.filament.pressure_advance
            && (!pa.is_finite() || !(0.0..=2.0).contains(&pa))
        {
            return Err(ConfigError::Invalid {
                field: "filament.pressure_advance",
                reason: "must be within 0.0..=2.0",
            });
        }
        if let Some(ct) = self.filament.chamber_temp_c
            && ct > 100
        {
            return Err(ConfigError::Invalid {
                field: "filament.chamber_temp_c",
                reason: "must be <= 100",
            });
        }
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
        let tree = &sup.tree;
        for (field, val, min, max) in [
            (
                "process.support.tree.max_branch_angle_deg",
                tree.max_branch_angle_deg,
                10.0,
                70.0,
            ),
            (
                "process.support.tree.tip_diameter_mm",
                tree.tip_diameter_mm,
                0.4,
                10.0,
            ),
            (
                "process.support.tree.branch_diameter_mm",
                tree.branch_diameter_mm,
                0.6,
                20.0,
            ),
            (
                "process.support.tree.max_trunk_diameter_mm",
                tree.max_trunk_diameter_mm,
                1.0,
                50.0,
            ),
            (
                "process.support.tree.branch_diameter_angle_deg",
                tree.branch_diameter_angle_deg,
                0.0,
                30.0,
            ),
            (
                "process.support.tree.branch_distance_mm",
                tree.branch_distance_mm,
                0.8,
                25.0,
            ),
            (
                "process.support.tree.trunk_base_flare_mm",
                tree.trunk_base_flare_mm,
                0.0,
                10.0,
            ),
        ] {
            if !val.is_finite() || !(min..=max).contains(&val) {
                return Err(ConfigError::Invalid {
                    field,
                    reason: "out of valid range",
                });
            }
        }
        if tree.tip_diameter_mm > tree.max_trunk_diameter_mm {
            return Err(ConfigError::Invalid {
                field: "process.support.tree.tip_diameter_mm",
                reason: "must be <= max_trunk_diameter_mm",
            });
        }
        if tree.branch_diameter_mm > tree.max_trunk_diameter_mm {
            return Err(ConfigError::Invalid {
                field: "process.support.tree.branch_diameter_mm",
                reason: "must be <= max_trunk_diameter_mm",
            });
        }
        if !(1..=6).contains(&tree.branch_wall_loops) {
            return Err(ConfigError::Invalid {
                field: "process.support.tree.branch_wall_loops",
                reason: "must be within 1..=6",
            });
        }
        check_non_neg("process.brim_width_mm", self.process.brim_width_mm)?;
        if !self.process.elephant_foot_compensation_mm.is_finite()
            || !(0.0..=5.0).contains(&self.process.elephant_foot_compensation_mm)
        {
            return Err(ConfigError::Invalid {
                field: "process.elephant_foot_compensation_mm",
                reason: "must be within 0.0..=5.0",
            });
        }
        if !self.process.xy_contour_compensation_mm.is_finite()
            || !(-5.0..=5.0).contains(&self.process.xy_contour_compensation_mm)
        {
            return Err(ConfigError::Invalid {
                field: "process.xy_contour_compensation_mm",
                reason: "must be within -5.0..=5.0",
            });
        }
        if !self.process.xy_hole_compensation_mm.is_finite()
            || !(-5.0..=5.0).contains(&self.process.xy_hole_compensation_mm)
        {
            return Err(ConfigError::Invalid {
                field: "process.xy_hole_compensation_mm",
                reason: "must be within -5.0..=5.0",
            });
        }
        check_non_neg(
            "process.avoid_crossing_perimeters_max_detour_mm",
            self.process.avoid_crossing_perimeters_max_detour_mm,
        )?;

        let ir = &self.process.ironing;
        for (field, val, min, max) in [
            ("process.ironing.flow_ratio", ir.flow_ratio, 0.01, 0.5),
            (
                "process.ironing.line_spacing_mm",
                ir.line_spacing_mm,
                0.04,
                1.0,
            ),
            ("process.ironing.inset_mm", ir.inset_mm, 0.0, 5.0),
            (
                "process.ironing.angle_offset_deg",
                ir.angle_offset_deg,
                -180.0,
                180.0,
            ),
        ] {
            if !val.is_finite() || !(min..=max).contains(&val) {
                return Err(ConfigError::Invalid {
                    field,
                    reason: "out of valid range",
                });
            }
        }
        check_pos("process.ironing.speed_mm_s", ir.speed_mm_s)?;

        let sc = &self.process.scarf_seam;
        for (field, val, min, max) in [
            ("process.scarf_seam.length_mm", sc.length_mm, 1.0, 200.0),
            (
                "process.scarf_seam.min_length_mm",
                sc.min_length_mm,
                0.5,
                200.0,
            ),
            (
                "process.scarf_seam.start_height_ratio",
                sc.start_height_ratio,
                0.02,
                0.50,
            ),
            ("process.scarf_seam.flow_ratio", sc.flow_ratio, 0.5, 1.5),
            (
                "process.arc_tolerance_mm",
                self.process.arc_tolerance_mm,
                0.002,
                0.5,
            ),
        ] {
            if !val.is_finite() || !(min..=max).contains(&val) {
                return Err(ConfigError::Invalid {
                    field,
                    reason: "out of valid range",
                });
            }
        }
        if !(2..=64).contains(&sc.steps) {
            return Err(ConfigError::Invalid {
                field: "process.scarf_seam.steps",
                reason: "must be within 2..=64",
            });
        }

        let ar = &self.process.arachne;
        for (field, val, min, max) in [
            (
                "process.arachne.min_bead_width_ratio",
                ar.min_bead_width_ratio,
                0.20,
                1.00,
            ),
            (
                "process.arachne.max_bead_width_ratio",
                ar.max_bead_width_ratio,
                1.00,
                3.00,
            ),
            (
                "process.arachne.min_feature_size_ratio",
                ar.min_feature_size_ratio,
                0.05,
                1.00,
            ),
            (
                "process.arachne.transition_length_ratio",
                ar.transition_length_ratio,
                0.10,
                5.00,
            ),
            (
                "process.arachne.transition_threshold_angle_deg",
                ar.transition_threshold_angle_deg,
                0.0,
                60.0,
            ),
            (
                "process.arachne.transition_filter_deviation_ratio",
                ar.transition_filter_deviation_ratio,
                0.0,
                1.0,
            ),
        ] {
            if !val.is_finite() || !(min..=max).contains(&val) {
                return Err(ConfigError::Invalid {
                    field,
                    reason: "out of valid range",
                });
            }
        }
        if ar.min_bead_width_ratio > ar.max_bead_width_ratio {
            return Err(ConfigError::Invalid {
                field: "process.arachne.min_bead_width_ratio",
                reason: "must be <= max_bead_width_ratio",
            });
        }
        if !(1..=8).contains(&ar.wall_distribution_count) {
            return Err(ConfigError::Invalid {
                field: "process.arachne.wall_distribution_count",
                reason: "must be within 1..=8",
            });
        }

        for (field, opt_accel) in [
            (
                "process.default_acceleration_mm_s2",
                self.process.default_acceleration_mm_s2,
            ),
            (
                "process.outer_wall_acceleration_mm_s2",
                self.process.outer_wall_acceleration_mm_s2,
            ),
            (
                "process.inner_wall_acceleration_mm_s2",
                self.process.inner_wall_acceleration_mm_s2,
            ),
            (
                "process.first_layer_acceleration_mm_s2",
                self.process.first_layer_acceleration_mm_s2,
            ),
            (
                "process.travel_acceleration_mm_s2",
                self.process.travel_acceleration_mm_s2,
            ),
        ] {
            if let Some(a) = opt_accel
                && (!a.is_finite() || !(10.0..=100_000.0).contains(&a))
            {
                return Err(ConfigError::Invalid {
                    field,
                    reason: "must be within 10.0..=100000.0",
                });
            }
        }

        let max_nozzle_h = self.machine.nozzle_diameter_mm * 1.25;
        if self.process.layer_height_mm > max_nozzle_h {
            return Err(ConfigError::Invalid {
                field: "process.layer_height_mm",
                reason: "exceeds 1.25x nozzle diameter",
            });
        }
        if self.process.first_layer_height_mm > max_nozzle_h {
            return Err(ConfigError::Invalid {
                field: "process.first_layer_height_mm",
                reason: "exceeds 1.25x nozzle diameter",
            });
        }
        for (field, opt_thick) in [
            (
                "process.top_shell_thickness_mm",
                self.process.top_shell_thickness_mm,
            ),
            (
                "process.bottom_shell_thickness_mm",
                self.process.bottom_shell_thickness_mm,
            ),
        ] {
            if let Some(t) = opt_thick
                && (!t.is_finite() || !(0.0..=50.0).contains(&t))
            {
                return Err(ConfigError::Invalid {
                    field,
                    reason: "must be within 0.0..=50.0",
                });
            }
        }

        let al = &self.process.adaptive_layer_height;
        if let Some(min_h) = al.min_layer_height_mm
            && (!min_h.is_finite() || min_h < 0.02 || min_h > max_nozzle_h)
        {
            return Err(ConfigError::Invalid {
                field: "process.adaptive_layer_height.min_layer_height_mm",
                reason: "must be within 0.02..=1.25x nozzle diameter",
            });
        }
        if let Some(max_h) = al.max_layer_height_mm
            && (!max_h.is_finite() || max_h < 0.02 || max_h > max_nozzle_h)
        {
            return Err(ConfigError::Invalid {
                field: "process.adaptive_layer_height.max_layer_height_mm",
                reason: "must be within 0.02..=1.25x nozzle diameter",
            });
        }
        if let (Some(min_h), Some(max_h)) = (al.min_layer_height_mm, al.max_layer_height_mm)
            && min_h > max_h
        {
            return Err(ConfigError::Invalid {
                field: "process.adaptive_layer_height.min_layer_height_mm",
                reason: "must be <= max_layer_height_mm",
            });
        }
        for (field, val, min, max) in [
            (
                "process.adaptive_layer_height.quality",
                al.quality,
                0.0,
                1.0,
            ),
            (
                "process.adaptive_layer_height.smooth_radius_mm",
                al.smooth_radius_mm,
                0.0,
                20.0,
            ),
            (
                "process.adaptive_layer_height.max_step_ratio",
                al.max_step_ratio,
                1.05,
                3.0,
            ),
        ] {
            if !val.is_finite() || !(min..=max).contains(&val) {
                return Err(ConfigError::Invalid {
                    field,
                    reason: "out of valid range",
                });
            }
        }

        for hr in &self.process.height_ranges {
            if !hr.min_z_mm.is_finite() || hr.min_z_mm < 0.0 {
                return Err(ConfigError::Invalid {
                    field: "process.height_ranges.min_z_mm",
                    reason: "must be finite and >= 0",
                });
            }
            if !hr.max_z_mm.is_finite() || hr.max_z_mm <= hr.min_z_mm {
                return Err(ConfigError::Invalid {
                    field: "process.height_ranges.max_z_mm",
                    reason: "must be finite and > min_z_mm",
                });
            }
            if let Some(h) = hr.layer_height_mm
                && (!h.is_finite() || h < 0.02 || h > max_nozzle_h)
            {
                return Err(ConfigError::Invalid {
                    field: "process.height_ranges.layer_height_mm",
                    reason: "must be within 0.02..=1.25x nozzle diameter",
                });
            }
            hr.overrides.validate()?;
        }

        if !self.process.extruder_clearance_radius_mm.is_finite()
            || !(0.0..=200.0).contains(&self.process.extruder_clearance_radius_mm)
        {
            return Err(ConfigError::Invalid {
                field: "process.extruder_clearance_radius_mm",
                reason: "must be within 0.0..=200.0",
            });
        }
        if !self.process.extruder_clearance_height_mm.is_finite()
            || !(0.0..=500.0).contains(&self.process.extruder_clearance_height_mm)
        {
            return Err(ConfigError::Invalid {
                field: "process.extruder_clearance_height_mm",
                reason: "must be within 0.0..=500.0",
            });
        }
        if !self.process.infill_wall_overlap_ratio.is_finite()
            || !(0.0..=0.50).contains(&self.process.infill_wall_overlap_ratio)
        {
            return Err(ConfigError::Invalid {
                field: "process.infill_wall_overlap_ratio",
                reason: "must be within 0.0..=0.50",
            });
        }
        if !self.process.gap_fill_min_width_mm.is_finite()
            || !(0.05..=2.0).contains(&self.process.gap_fill_min_width_mm)
        {
            return Err(ConfigError::Invalid {
                field: "process.gap_fill_min_width_mm",
                reason: "must be within 0.05..=2.0",
            });
        }
        for (field, opt_spd) in [
            (
                "process.gap_fill_speed_mm_s",
                self.process.gap_fill_speed_mm_s,
            ),
            (
                "process.overhang_1_4_speed_mm_s",
                self.process.overhang_1_4_speed_mm_s,
            ),
            (
                "process.overhang_2_4_speed_mm_s",
                self.process.overhang_2_4_speed_mm_s,
            ),
            (
                "process.overhang_4_4_speed_mm_s",
                self.process.overhang_4_4_speed_mm_s,
            ),
        ] {
            if let Some(s) = opt_spd {
                check_pos(field, s)?;
            }
        }
        for ev in &self.process.z_events {
            check_pos("process.z_events.z_mm", ev.z_mm)?;
            if ev.action == ZEventAction::CustomGcode
                && ev
                    .custom_gcode
                    .as_deref()
                    .is_none_or(|g| g.trim().is_empty())
            {
                return Err(ConfigError::Invalid {
                    field: "process.z_events.custom_gcode",
                    reason: "custom_gcode action requires non-empty G-code",
                });
            }
            if let Some(msg) = &ev.message
                && (msg.contains('\n') || msg.contains('\r'))
            {
                return Err(ConfigError::Invalid {
                    field: "process.z_events.message",
                    reason: "must be a single line without newlines",
                });
            }
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

        for bad_pa in [-0.1, 3.0, f64::NAN] {
            let mut c = PrintConfig::default();
            c.filament.pressure_advance = Some(bad_pa);
            assert!(c.validate().is_err());
        }
        let mut bad_ct = PrintConfig::default();
        bad_ct.filament.chamber_temp_c = Some(120);
        assert!(bad_ct.validate().is_err());
        for bad_acc in [0.0, -100.0, 200_000.0, f64::INFINITY] {
            let mut c = PrintConfig::default();
            c.process.default_acceleration_mm_s2 = Some(bad_acc);
            assert!(c.validate().is_err());
        }

        let mut bad_iron = PrintConfig::default();
        bad_iron.process.ironing.flow_ratio = 0.8;
        assert!(bad_iron.validate().is_err());
        bad_iron.process.ironing.flow_ratio = 0.10;
        bad_iron.process.ironing.line_spacing_mm = 0.01;
        assert!(bad_iron.validate().is_err());
        bad_iron.process.ironing.line_spacing_mm = 0.10;
        bad_iron.process.ironing.speed_mm_s = 0.0;
        assert!(bad_iron.validate().is_err());

        let mut bad_scarf = PrintConfig::default();
        bad_scarf.process.scarf_seam.steps = 1;
        assert!(bad_scarf.validate().is_err());
        bad_scarf.process.scarf_seam.steps = 10;
        bad_scarf.process.scarf_seam.start_height_ratio = 0.9;
        assert!(bad_scarf.validate().is_err());
        bad_scarf.process.scarf_seam.start_height_ratio = 0.10;
        bad_scarf.process.arc_tolerance_mm = 0.0001;
        assert!(bad_scarf.validate().is_err());

        let mut arachne_ok = PrintConfig::default();
        arachne_ok.process.wall_generator = WallGenerator::Arachne;
        assert!(arachne_ok.validate().is_ok());
        assert_eq!(arachne_ok.process.wall_generator.label(), "Arachne");

        let mut bad_ar = PrintConfig::default();
        bad_ar.process.arachne.min_bead_width_ratio = f64::NAN;
        assert!(bad_ar.validate().is_err());
        bad_ar.process.arachne.min_bead_width_ratio = 0.0;
        assert!(bad_ar.validate().is_err());
        bad_ar.process.arachne.min_bead_width_ratio = 0.85;
        bad_ar.process.arachne.max_bead_width_ratio = 0.50;
        assert!(bad_ar.validate().is_err());
        bad_ar.process.arachne.max_bead_width_ratio = 2.00;
        bad_ar.process.arachne.min_feature_size_ratio = 0.01;
        assert!(bad_ar.validate().is_err());
        bad_ar.process.arachne.min_feature_size_ratio = 0.25;
        bad_ar.process.arachne.wall_distribution_count = 0;
        assert!(bad_ar.validate().is_err());
        bad_ar.process.arachne.wall_distribution_count = 9;
        assert!(bad_ar.validate().is_err());

        let mut tree_ok = PrintConfig::default();
        tree_ok.process.support.style = SupportStyle::Tree;
        assert!(tree_ok.validate().is_ok());
        assert_eq!(tree_ok.process.support.style.label(), "Tree (Organic)");

        let mut bad_tree = PrintConfig::default();
        bad_tree.process.support.tree.max_branch_angle_deg = f64::NAN;
        assert!(bad_tree.validate().is_err());
        bad_tree.process.support.tree.max_branch_angle_deg = 5.0;
        assert!(bad_tree.validate().is_err());
        bad_tree.process.support.tree.max_branch_angle_deg = 80.0;
        assert!(bad_tree.validate().is_err());
        bad_tree.process.support.tree.max_branch_angle_deg = 45.0;
        bad_tree.process.support.tree.branch_wall_loops = 0;
        assert!(bad_tree.validate().is_err());
        bad_tree.process.support.tree.branch_wall_loops = 2;
        bad_tree.process.support.tree.tip_diameter_mm = 9.0;
        assert!(bad_tree.validate().is_err());

        // Phase 4D adaptive layer height, height ranges & modifier overrides validation
        let mut adapt_ok = PrintConfig::default();
        adapt_ok.process.adaptive_layer_height.enabled = true;
        adapt_ok.process.adaptive_layer_height.min_layer_height_mm = Some(0.08);
        adapt_ok.process.adaptive_layer_height.max_layer_height_mm = Some(0.28);
        adapt_ok.process.top_shell_thickness_mm = Some(0.6);
        adapt_ok.process.bottom_shell_thickness_mm = Some(0.6);
        adapt_ok.process.height_ranges.push(HeightRangeModifier {
            min_z_mm: 2.0,
            max_z_mm: 6.0,
            layer_height_mm: Some(0.12),
            overrides: ModifierOverrides {
                sparse_infill_density: Some(0.45),
                wall_loops: Some(4),
                ..ModifierOverrides::default()
            },
        });
        assert!(adapt_ok.validate().is_ok());

        let al_def = AdaptiveLayerConfig::default();
        assert!((al_def.effective_min_layer_height_mm(0.4, 0.2) - 0.08).abs() < 1e-9);
        assert!((al_def.effective_max_layer_height_mm(0.4, 0.2) - 0.30).abs() < 1e-9);
        assert_eq!(ModifierRole::SupportBlocker.label(), "Support blocker");

        let mut bad_adapt = PrintConfig::default();
        bad_adapt.process.adaptive_layer_height.min_layer_height_mm = Some(0.30);
        bad_adapt.process.adaptive_layer_height.max_layer_height_mm = Some(0.15);
        assert!(bad_adapt.validate().is_err());
        bad_adapt.process.adaptive_layer_height.max_layer_height_mm = Some(0.32);
        bad_adapt.process.adaptive_layer_height.quality = 1.2;
        assert!(bad_adapt.validate().is_err());
        bad_adapt.process.adaptive_layer_height.quality = 0.5;
        bad_adapt.process.adaptive_layer_height.max_step_ratio = 4.0;
        assert!(bad_adapt.validate().is_err());
        bad_adapt.process.adaptive_layer_height.max_step_ratio = 1.5;
        bad_adapt.process.top_shell_thickness_mm = Some(-0.2);
        assert!(bad_adapt.validate().is_err());

        let mut bad_hr = PrintConfig::default();
        bad_hr.process.height_ranges.push(HeightRangeModifier {
            min_z_mm: 5.0,
            max_z_mm: 4.0,
            layer_height_mm: Some(0.15),
            overrides: ModifierOverrides::default(),
        });
        assert!(bad_hr.validate().is_err());

        let mut bad_ov = ModifierOverrides {
            sparse_infill_density: Some(1.5),
            ..ModifierOverrides::default()
        };
        assert!(bad_ov.validate().is_err());
        bad_ov.sparse_infill_density = Some(0.5);
        bad_ov.wall_loops = Some(25);
        assert!(bad_ov.validate().is_err());
        bad_ov.wall_loops = Some(3);
        bad_ov.infill_speed_mm_s = Some(0.0);
        assert!(bad_ov.validate().is_err());

        // Phase 5B dimensional compensations, wall order, surface pattern, combing & thumbnails
        assert_eq!(WallOrder::InnerOuterInner.label(), "Inner / Outer / Inner");
        assert_eq!(SurfacePattern::Concentric.label(), "Concentric");
        let mut p5b = PrintConfig::default();
        p5b.machine.thumbnails = vec![ThumbnailSize::png(160, 120), ThumbnailSize::png(220, 124)];
        p5b.process.elephant_foot_compensation_mm = 0.2;
        p5b.process.xy_contour_compensation_mm = -0.15;
        p5b.process.xy_hole_compensation_mm = 0.1;
        p5b.process.wall_order = WallOrder::OuterInner;
        p5b.process.top_surface_pattern = SurfacePattern::Concentric;
        p5b.process.bottom_surface_pattern = SurfacePattern::Rectilinear;
        p5b.process.avoid_crossing_perimeters = true;
        p5b.process.avoid_crossing_perimeters_max_detour_mm = 25.0;
        assert!(p5b.validate().is_ok());

        let mut legacy = PrintConfig::default();
        legacy.process.monotonic_solid_infill = false;
        assert_eq!(
            legacy.process.effective_top_surface_pattern(),
            SurfacePattern::Rectilinear
        );
        assert_eq!(
            legacy.process.effective_bottom_surface_pattern(),
            SurfacePattern::Rectilinear
        );
        legacy.process.top_surface_pattern = SurfacePattern::Concentric;
        assert_eq!(
            legacy.process.effective_top_surface_pattern(),
            SurfacePattern::Concentric
        );

        let mut bad_p5b = PrintConfig::default();
        bad_p5b.process.elephant_foot_compensation_mm = -0.1;
        assert!(bad_p5b.validate().is_err());
        bad_p5b.process.elephant_foot_compensation_mm = f64::NAN;
        assert!(bad_p5b.validate().is_err());
        bad_p5b.process.elephant_foot_compensation_mm = 0.0;
        bad_p5b.process.xy_contour_compensation_mm = 6.0;
        assert!(bad_p5b.validate().is_err());
        bad_p5b.process.xy_contour_compensation_mm = 0.0;
        bad_p5b.process.xy_hole_compensation_mm = -6.0;
        assert!(bad_p5b.validate().is_err());
        bad_p5b.process.xy_hole_compensation_mm = 0.0;
        bad_p5b.process.avoid_crossing_perimeters_max_detour_mm = -1.0;
        assert!(bad_p5b.validate().is_err());
        bad_p5b.process.avoid_crossing_perimeters_max_detour_mm = 0.0;
        bad_p5b.machine.thumbnails = vec![ThumbnailSize::png(8, 8)];
        assert!(bad_p5b.validate().is_err());
        bad_p5b.machine.thumbnails = vec![ThumbnailSize::png(64, 64); 5];
        assert!(bad_p5b.validate().is_err());

        let mut flow_cfg = PrintConfig::default();
        flow_cfg.filament.flow_ratio = 0.96;
        flow_cfg.machine.thumbnails = vec![ThumbnailSize::qoi(440, 240)];
        assert!(flow_cfg.validate().is_ok());

        for bad_flow in [0.4, 1.6, f64::NAN] {
            let mut c = PrintConfig::default();
            c.filament.flow_ratio = bad_flow;
            assert!(matches!(
                c.validate(),
                Err(ConfigError::Invalid {
                    field: "filament.flow_ratio",
                    ..
                })
            ));
        }
    }

    #[test]
    fn phase6b_config_validation_and_defaults() {
        let mut cfg = PrintConfig::default();
        assert!(cfg.machine.label_objects);
        assert_eq!(cfg.process.print_sequence, PrintSequence::ByLayer);
        assert!(!cfg.process.gap_fill_enabled);
        assert_eq!(ModifierRole::ALL.len(), 4);
        assert_eq!(ModifierRole::NegativePart.label(), "Negative part");

        cfg.process.infill_wall_overlap_ratio = 0.25;
        cfg.process.gap_fill_enabled = true;
        cfg.process.gap_fill_speed_mm_s = Some(50.0);
        cfg.process.overhang_1_4_speed_mm_s = Some(60.0);
        cfg.process.overhang_2_4_speed_mm_s = Some(40.0);
        cfg.process.overhang_4_4_speed_mm_s = Some(15.0);
        cfg.process.z_events = vec![
            LayerZEvent {
                z_mm: 4.2,
                action: ZEventAction::Pause,
                message: Some("Insert nut".to_owned()),
                custom_gcode: None,
            },
            LayerZEvent {
                z_mm: 8.0,
                action: ZEventAction::CustomGcode,
                message: None,
                custom_gcode: Some("M117 Custom".to_owned()),
            },
        ];
        assert!(cfg.validate().is_ok());

        let mut bad = cfg.clone();
        bad.process.infill_wall_overlap_ratio = 0.75;
        assert!(bad.validate().is_err());

        let mut bad_z = cfg.clone();
        bad_z.process.z_events[0].z_mm = 0.0;
        assert!(bad_z.validate().is_err());

        let mut bad_msg = cfg.clone();
        bad_msg.process.z_events[0].message = Some("line1\nline2".to_owned());
        assert!(bad_msg.validate().is_err());

        let mut bad_custom = cfg;
        bad_custom.process.z_events[1].custom_gcode = Some("   ".to_owned());
        assert!(bad_custom.validate().is_err());
    }
}
