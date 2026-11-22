//! Clean-room OrcaSlicer / Bambu JSON profile inheritance loader and [`PrintConfig`] mapper.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::{
    ConfigError, GcodeFlavor, InfillPattern, IroningType, PrintConfig, SeamPosition, SupportStyle,
    WallGenerator,
};

/// Maximum inheritance chain length allowed when resolving profiles.
pub const MAX_INHERITANCE_DEPTH: usize = 32;

const METADATA_KEYS: &[&str] = &[
    "name",
    "type",
    "inherits",
    "from",
    "instantiation",
    "version",
    "setting_id",
    "compatible_printers",
    "compatible_printers_condition",
    "description",
    "printer_model",
    "printer_variant",
    "printer_technology",
    "filament_id",
    "filament_type",
    "filament_vendor",
    "curr_bed_type",
    "nozzle_type",
    "default_materials",
    "default_filament_profile",
    "default_print_profile",
];

/// Supported profile categories.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    #[serde(alias = "printer")]
    Machine,
    MachineModel,
    Filament,
    #[serde(alias = "print")]
    Process,
}

impl ProfileKind {
    /// Parse a profile category string (supports Orca/Prusa/LayerCraft names, case-insensitive).
    pub fn parse_str(raw: &str) -> Result<Self, ProfileError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "machine" | "printer" => Ok(Self::Machine),
            "machine_model" => Ok(Self::MachineModel),
            "filament" => Ok(Self::Filament),
            "process" | "print" => Ok(Self::Process),
            _ => Err(ProfileError::UnknownProfileType(raw.to_owned())),
        }
    }

    /// Canonical snake_case identifier string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Machine => "machine",
            Self::MachineModel => "machine_model",
            Self::Filament => "filament",
            Self::Process => "process",
        }
    }
}

/// Errors returned while loading, resolving, or applying JSON profiles.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ProfileError {
    #[error("failed parsing profile JSON: {0}")]
    JsonParse(String),
    #[error("profile JSON root must be an object or array of objects")]
    NotAnObject,
    #[error("profile is missing required field `{field}`")]
    MissingField { field: &'static str },
    #[error("unknown profile type `{0}`")]
    UnknownProfileType(String),
    #[error("{kind:?} profile `{name}` not found in library")]
    NotFound { kind: ProfileKind, name: String },
    #[error("inheritance cycle detected at `{name}` ({chain})")]
    InheritanceCycle { name: String, chain: String },
    #[error("inheritance depth exceeded maximum ({max_depth}) while resolving `{name}`")]
    InheritanceDepthExceeded { name: String, max_depth: usize },
    #[error("invalid profile value for `{key}`: {reason}")]
    InvalidValue { key: String, reason: String },
    #[error("profile I/O error at `{path}`: {reason}")]
    Io { path: String, reason: String },
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// Non-fatal diagnostic emitted when a profile key uses a partial approximation or fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProfileWarning {
    pub profile: String,
    pub key: String,
    pub message: String,
}

/// Metadata summary of a profile registered in [`ProfileLibrary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProfileSummary {
    pub name: String,
    pub kind: ProfileKind,
    pub instantiation: bool,
    pub from: Option<String>,
    pub inherits: Vec<String>,
}

/// Parsed raw JSON profile before inheritance resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProfileDocument {
    pub name: String,
    pub kind: ProfileKind,
    pub instantiation: bool,
    pub from: Option<String>,
    pub inherits: Vec<String>,
    pub settings: BTreeMap<String, Value>,
}

impl ProfileDocument {
    /// Parse a single profile object from a JSON string.
    pub fn from_json_str(json: &str) -> Result<Self, ProfileError> {
        let val: Value =
            serde_json::from_str(json).map_err(|e| ProfileError::JsonParse(e.to_string()))?;
        Self::from_json_value(val, None)
    }

    /// Parse a single profile object from a parsed [`serde_json::Value`].
    pub fn from_json_value(val: Value, fallback_name: Option<&str>) -> Result<Self, ProfileError> {
        let Value::Object(map) = val else {
            return Err(ProfileError::NotAnObject);
        };

        let type_raw = map
            .get("type")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or(ProfileError::MissingField { field: "type" })?;
        let kind = ProfileKind::parse_str(type_raw)?;

        let name = map
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| fallback_name.map(str::trim).filter(|s| !s.is_empty()))
            .ok_or(ProfileError::MissingField { field: "name" })?
            .to_owned();

        let instantiation = match map.get("instantiation") {
            None | Some(Value::Null) => true,
            Some(v) => parse_bool_val("instantiation", v)?,
        };

        let from = match map.get("from") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_owned())
                }
            }
            Some(_) => {
                return Err(ProfileError::InvalidValue {
                    key: "from".to_owned(),
                    reason: "must be a string".to_owned(),
                });
            }
        };

        let inherits = match map.get("inherits") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(s)) => s
                .split(';')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
                .collect(),
            Some(Value::Array(arr)) => {
                let mut out = Vec::with_capacity(arr.len());
                for item in arr {
                    let s = item.as_str().ok_or_else(|| ProfileError::InvalidValue {
                        key: "inherits".to_owned(),
                        reason: "array elements must be strings".to_owned(),
                    })?;
                    for part in s.split(';').map(str::trim).filter(|p| !p.is_empty()) {
                        out.push(part.to_owned());
                    }
                }
                out
            }
            Some(_) => {
                return Err(ProfileError::InvalidValue {
                    key: "inherits".to_owned(),
                    reason: "must be a string or array of strings".to_owned(),
                });
            }
        };

        let mut settings = BTreeMap::new();
        for (k, v) in map {
            if matches!(
                k.as_str(),
                "name" | "type" | "inherits" | "from" | "instantiation"
            ) {
                continue;
            }
            settings.insert(k, v);
        }

        Ok(Self {
            name,
            kind,
            instantiation,
            from,
            inherits,
            settings,
        })
    }

    /// Return lightweight summary metadata for this profile document.
    pub fn summary(&self) -> ProfileSummary {
        ProfileSummary {
            name: self.name.clone(),
            kind: self.kind,
            instantiation: self.instantiation,
            from: self.from.clone(),
            inherits: self.inherits.clone(),
        }
    }
}

/// Flattened profile after resolving all inherited base profiles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedProfile {
    pub name: String,
    pub kind: ProfileKind,
    pub instantiation: bool,
    pub from: Option<String>,
    pub inheritance_chain: Vec<String>,
    pub settings: BTreeMap<String, Value>,
}

/// Result returned when applying resolved machine/filament/process profiles onto [`PrintConfig`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProfileApplyResult {
    pub config: PrintConfig,
    pub machine_chain: Vec<String>,
    pub filament_chain: Vec<String>,
    pub process_chain: Vec<String>,
    pub applied_keys: Vec<String>,
    pub unmapped_keys: Vec<String>,
    pub warnings: Vec<ProfileWarning>,
}

/// Active profile names stored on an engine session.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct ActiveProfiles {
    pub machine: Option<String>,
    pub filament: Option<String>,
    pub process: Option<String>,
}

/// Record of a JSON file skipped while scanning a directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkippedProfileFile {
    pub path: String,
    pub reason: String,
}

/// Summary report returned by [`ProfileLibrary::load_dir`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProfileDirLoadReport {
    pub loaded: Vec<ProfileSummary>,
    pub skipped: Vec<SkippedProfileFile>,
}

/// Collection of raw profile documents supporting inheritance resolution and [`PrintConfig`] mapping.
#[derive(Debug, Clone, Default)]
pub struct ProfileLibrary {
    docs: BTreeMap<(ProfileKind, String), ProfileDocument>,
}

impl ProfileLibrary {
    /// Construct an empty profile library.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a library pre-populated with clean-room built-in LayerCraft starter presets.
    pub fn with_builtin_presets() -> Self {
        let mut lib = Self::new();
        for doc in builtin_preset_documents() {
            lib.add_document(doc);
        }
        lib
    }

    /// Insert or replace a parsed [`ProfileDocument`] in the library.
    pub fn add_document(&mut self, doc: ProfileDocument) -> ProfileSummary {
        let summary = doc.summary();
        self.docs.insert((doc.kind, doc.name.clone()), doc);
        summary
    }

    /// Load one profile object, a JSON array of profiles, or `{"profiles": [...]}`.
    pub fn load_json_str(&mut self, json: &str) -> Result<Vec<ProfileSummary>, ProfileError> {
        self.load_json_str_with_fallback(json, None)
    }

    fn load_json_str_with_fallback(
        &mut self,
        json: &str,
        fallback_name: Option<&str>,
    ) -> Result<Vec<ProfileSummary>, ProfileError> {
        let val: Value =
            serde_json::from_str(json).map_err(|e| ProfileError::JsonParse(e.to_string()))?;
        match val {
            Value::Array(items) => {
                let mut summaries = Vec::with_capacity(items.len());
                for item in items {
                    let doc = ProfileDocument::from_json_value(item, None)?;
                    summaries.push(self.add_document(doc));
                }
                Ok(summaries)
            }
            Value::Object(ref obj) if obj.get("profiles").is_some_and(Value::is_array) => {
                let Some(Value::Array(items)) = obj.get("profiles").cloned() else {
                    return Err(ProfileError::NotAnObject);
                };
                let mut summaries = Vec::with_capacity(items.len());
                for item in items {
                    let doc = ProfileDocument::from_json_value(item, None)?;
                    summaries.push(self.add_document(doc));
                }
                Ok(summaries)
            }
            Value::Object(_) => {
                let doc = ProfileDocument::from_json_value(val, fallback_name)?;
                Ok(vec![self.add_document(doc)])
            }
            _ => Err(ProfileError::NotAnObject),
        }
    }

    /// Load profiles from a JSON file on disk (native targets only).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load_file(
        &mut self,
        path: &std::path::Path,
    ) -> Result<Vec<ProfileSummary>, ProfileError> {
        let raw = std::fs::read_to_string(path).map_err(|e| ProfileError::Io {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let fallback_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty());
        self.load_json_str_with_fallback(&raw, fallback_name)
    }

    /// Recursively scan a directory up to depth 8 for `.json` profiles (native targets only).
    ///
    /// Non-profile or malformed sidecar JSON files are recorded in `skipped` rather than
    /// aborting the directory load. Returns `Err(ProfileError::Io)` if `dir` itself cannot be read.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load_dir(
        &mut self,
        dir: &std::path::Path,
    ) -> Result<ProfileDirLoadReport, ProfileError> {
        const MAX_DIR_DEPTH: usize = 8;
        let _root_entries = std::fs::read_dir(dir).map_err(|e| ProfileError::Io {
            path: dir.display().to_string(),
            reason: e.to_string(),
        })?;
        let mut json_files = Vec::new();
        let mut skipped = Vec::new();
        collect_json_files(dir, 0, MAX_DIR_DEPTH, &mut json_files, &mut skipped);
        json_files.sort();

        let mut loaded = Vec::new();
        for file in json_files {
            match self.load_file(&file) {
                Ok(mut summaries) => loaded.append(&mut summaries),
                Err(e) => skipped.push(SkippedProfileFile {
                    path: file.display().to_string(),
                    reason: e.to_string(),
                }),
            }
        }
        Ok(ProfileDirLoadReport { loaded, skipped })
    }

    /// List profiles stored in this library, optionally filtered by kind or instantiation flag.
    pub fn list(&self, kind: Option<ProfileKind>, include_abstract: bool) -> Vec<ProfileSummary> {
        self.docs
            .values()
            .filter(|doc| kind.is_none_or(|k| doc.kind == k))
            .filter(|doc| include_abstract || doc.instantiation)
            .map(ProfileDocument::summary)
            .collect()
    }

    /// Resolve a profile and all of its inherited base profiles into a flattened map.
    pub fn resolve(&self, kind: ProfileKind, name: &str) -> Result<ResolvedProfile, ProfileError> {
        let mut visiting = Vec::new();
        let mut resolved = self.resolve_inner(kind, name, &mut visiting)?;

        if kind == ProfileKind::Machine
            && let Some(model_name) = resolved
                .settings
                .get("printer_model")
                .and_then(|v| unwrap_scalar(v))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
            && self
                .docs
                .contains_key(&(ProfileKind::MachineModel, model_name.clone()))
            && !resolved
                .inheritance_chain
                .iter()
                .any(|entry| entry == &model_name)
        {
            let mut model_visiting = Vec::new();
            let model_res =
                self.resolve_inner(ProfileKind::MachineModel, &model_name, &mut model_visiting)?;
            let mut combined_settings = model_res.settings;
            for (k, v) in resolved.settings {
                combined_settings.insert(k, v);
            }
            let mut combined_chain = Vec::with_capacity(
                model_res.inheritance_chain.len() + resolved.inheritance_chain.len(),
            );
            for item in model_res
                .inheritance_chain
                .into_iter()
                .chain(resolved.inheritance_chain)
            {
                if !combined_chain.contains(&item) {
                    combined_chain.push(item);
                }
            }
            resolved.settings = combined_settings;
            resolved.inheritance_chain = combined_chain;
        }

        Ok(resolved)
    }

    fn resolve_inner(
        &self,
        kind: ProfileKind,
        name: &str,
        visiting: &mut Vec<(ProfileKind, String)>,
    ) -> Result<ResolvedProfile, ProfileError> {
        let key = (kind, name.to_owned());
        if visiting.iter().any(|item| item == &key) {
            let mut chain_names: Vec<&str> = visiting.iter().map(|(_, n)| n.as_str()).collect();
            chain_names.push(name);
            return Err(ProfileError::InheritanceCycle {
                name: name.to_owned(),
                chain: chain_names.join(" -> "),
            });
        }
        if visiting.len() >= MAX_INHERITANCE_DEPTH {
            return Err(ProfileError::InheritanceDepthExceeded {
                name: name.to_owned(),
                max_depth: MAX_INHERITANCE_DEPTH,
            });
        }

        let Some(doc) = self.docs.get(&key) else {
            return Err(ProfileError::NotFound {
                kind,
                name: name.to_owned(),
            });
        };

        visiting.push(key);
        let res = (|| {
            let mut merged_settings = BTreeMap::new();
            let mut chain = Vec::new();

            for parent_name in &doc.inherits {
                let parent_kind = if self.docs.contains_key(&(kind, parent_name.clone())) {
                    kind
                } else if kind == ProfileKind::Machine
                    && self
                        .docs
                        .contains_key(&(ProfileKind::MachineModel, parent_name.clone()))
                {
                    ProfileKind::MachineModel
                } else {
                    return Err(ProfileError::NotFound {
                        kind,
                        name: parent_name.clone(),
                    });
                };

                let parent_resolved = self.resolve_inner(parent_kind, parent_name, visiting)?;
                for (k, v) in parent_resolved.settings {
                    merged_settings.insert(k, v);
                }
                for ancestor in parent_resolved.inheritance_chain {
                    if !chain.contains(&ancestor) {
                        chain.push(ancestor);
                    }
                }
            }

            for (k, v) in &doc.settings {
                merged_settings.insert(k.clone(), v.clone());
            }
            if !chain.contains(&doc.name) {
                chain.push(doc.name.clone());
            }

            Ok(ResolvedProfile {
                name: doc.name.clone(),
                kind: doc.kind,
                instantiation: doc.instantiation,
                from: doc.from.clone(),
                inheritance_chain: chain,
                settings: merged_settings,
            })
        })();
        visiting.pop();
        res
    }

    /// Resolve optional machine, filament, and process names and apply them onto `base`.
    pub fn build_print_config(
        &self,
        base: &PrintConfig,
        machine: Option<&str>,
        filament: Option<&str>,
        process: Option<&str>,
    ) -> Result<ProfileApplyResult, ProfileError> {
        let machine_res = match machine.map(str::trim).filter(|s| !s.is_empty()) {
            Some(name) => Some(self.resolve(ProfileKind::Machine, name)?),
            None => None,
        };
        let filament_res = match filament.map(str::trim).filter(|s| !s.is_empty()) {
            Some(name) => Some(self.resolve(ProfileKind::Filament, name)?),
            None => None,
        };
        let process_res = match process.map(str::trim).filter(|s| !s.is_empty()) {
            Some(name) => Some(self.resolve(ProfileKind::Process, name)?),
            None => None,
        };

        apply_resolved_profiles(
            base,
            machine_res.as_ref(),
            filament_res.as_ref(),
            process_res.as_ref(),
        )
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn collect_json_files(
    dir: &std::path::Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<std::path::PathBuf>,
    skipped: &mut Vec<SkippedProfileFile>,
) {
    if depth > max_depth {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            skipped.push(SkippedProfileFile {
                path: dir.display().to_string(),
                reason: e.to_string(),
            });
            return;
        }
    };
    let mut paths: Vec<std::path::PathBuf> =
        entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            collect_json_files(&path, depth + 1, max_depth, out, skipped);
        } else if meta.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        {
            out.push(path);
        }
    }
}

/// Apply pre-resolved machine, filament, and process profiles onto a baseline [`PrintConfig`].
pub fn apply_resolved_profiles(
    base: &PrintConfig,
    machine: Option<&ResolvedProfile>,
    filament: Option<&ResolvedProfile>,
    process: Option<&ResolvedProfile>,
) -> Result<ProfileApplyResult, ProfileError> {
    let mut cfg = base.clone();
    let mut ctx = MapperCtx::default();

    if let Some(m) = machine {
        apply_machine_settings(m, &mut cfg, &mut ctx)?;
    }
    if let Some(f) = filament {
        apply_filament_settings(f, machine, &mut cfg, &mut ctx)?;
    }
    if let Some(p) = process {
        apply_process_settings_pass1(p, &mut cfg, &mut ctx)?;
        apply_process_line_widths_pass2(p, &mut cfg, &mut ctx)?;
    }

    cfg.validate()?;

    let mut unmapped = BTreeSet::new();
    for prof in [machine, filament, process].into_iter().flatten() {
        for key in prof.settings.keys() {
            if !is_metadata_key(key) && !ctx.recognized_keys.contains(key) {
                unmapped.insert(key.clone());
            }
        }
    }

    Ok(ProfileApplyResult {
        config: cfg,
        machine_chain: machine
            .map(|p| p.inheritance_chain.clone())
            .unwrap_or_default(),
        filament_chain: filament
            .map(|p| p.inheritance_chain.clone())
            .unwrap_or_default(),
        process_chain: process
            .map(|p| p.inheritance_chain.clone())
            .unwrap_or_default(),
        applied_keys: ctx.applied_keys.into_iter().collect(),
        unmapped_keys: unmapped.into_iter().collect(),
        warnings: ctx.warnings,
    })
}

#[derive(Default)]
struct MapperCtx {
    applied_keys: BTreeSet<String>,
    recognized_keys: BTreeSet<String>,
    warnings: Vec<ProfileWarning>,
}

impl MapperCtx {
    fn mark_recognized_if_present(&mut self, settings: &BTreeMap<String, Value>, keys: &[&str]) {
        for &k in keys {
            if settings.contains_key(k) {
                self.recognized_keys.insert(k.to_owned());
            }
        }
    }

    fn get_primary<'a>(
        &mut self,
        settings: &'a BTreeMap<String, Value>,
        keys: &[&'static str],
    ) -> Option<(&'static str, &'a Value)> {
        self.mark_recognized_if_present(settings, keys);
        for &k in keys {
            if let Some(v) = settings.get(k) {
                return Some((k, v));
            }
        }
        None
    }

    fn mark_applied(&mut self, key: &str) {
        self.recognized_keys.insert(key.to_owned());
        self.applied_keys.insert(key.to_owned());
    }

    fn warn(&mut self, profile: &str, key: &str, message: impl Into<String>) {
        self.warnings.push(ProfileWarning {
            profile: profile.to_owned(),
            key: key.to_owned(),
            message: message.into(),
        });
    }
}

fn is_metadata_key(key: &str) -> bool {
    METADATA_KEYS.contains(&key)
}

fn unwrap_scalar(val: &Value) -> Option<&Value> {
    match val {
        Value::Array(arr) => arr.first(),
        other => Some(other),
    }
}

fn is_nil_value(val: &Value) -> bool {
    match unwrap_scalar(val) {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().eq_ignore_ascii_case("nil"),
        _ => false,
    }
}

fn parse_bool_val(key: &str, val: &Value) -> Result<bool, ProfileError> {
    let Some(scalar) = unwrap_scalar(val) else {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: "empty array cannot be parsed as boolean".to_owned(),
        });
    };
    match scalar {
        Value::Bool(b) => Ok(*b),
        Value::Number(n) => match n.as_i64() {
            Some(1) => Ok(true),
            Some(0) => Ok(false),
            _ => match n.as_f64() {
                Some(x) if (x - 1.0).abs() < 1e-9 => Ok(true),
                Some(x) if x.abs() < 1e-9 => Ok(false),
                _ => Err(ProfileError::InvalidValue {
                    key: key.to_owned(),
                    reason: format!("expected boolean (0/1), got `{scalar}`"),
                }),
            },
        },
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => Ok(true),
            "0" | "false" | "no" => Ok(false),
            _ => Err(ProfileError::InvalidValue {
                key: key.to_owned(),
                reason: format!("expected boolean, got `{s}`"),
            }),
        },
        _ => Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: format!("expected boolean, got `{scalar}`"),
        }),
    }
}

fn parse_f64_val(key: &str, val: &Value) -> Result<f64, ProfileError> {
    let Some(scalar) = unwrap_scalar(val) else {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: "empty array cannot be parsed as number".to_owned(),
        });
    };
    let parsed = match scalar {
        Value::Number(n) => n.as_f64().ok_or_else(|| ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: "non-finite number".to_owned(),
        })?,
        Value::String(s) => {
            let t = s.trim();
            t.parse::<f64>().map_err(|_| ProfileError::InvalidValue {
                key: key.to_owned(),
                reason: format!("invalid float `{t}`"),
            })?
        }
        _ => {
            return Err(ProfileError::InvalidValue {
                key: key.to_owned(),
                reason: format!("expected number, got `{scalar}`"),
            });
        }
    };
    if !parsed.is_finite() {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: "number must be finite".to_owned(),
        });
    }
    Ok(parsed)
}

fn parse_u16_val(key: &str, val: &Value) -> Result<u16, ProfileError> {
    let f = parse_f64_val(key, val)?;
    let rounded = f.round();
    if f < 0.0 || (f - rounded).abs() > 1e-6 || rounded > f64::from(u16::MAX) {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: format!("expected integer in 0..={}, got {f}", u16::MAX),
        });
    }
    Ok(rounded as u16)
}

fn parse_u8_percent_val(key: &str, val: &Value) -> Result<u8, ProfileError> {
    let Some(scalar) = unwrap_scalar(val) else {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: "empty array cannot be parsed as percentage".to_owned(),
        });
    };
    let f = match scalar {
        Value::String(s) => {
            let trimmed = s.trim().trim_end_matches('%').trim();
            trimmed
                .parse::<f64>()
                .map_err(|_| ProfileError::InvalidValue {
                    key: key.to_owned(),
                    reason: format!("invalid percentage `{s}`"),
                })?
        }
        _ => parse_f64_val(key, scalar)?,
    };
    let rounded = f.round();
    if !f.is_finite() || f < 0.0 || (f - rounded).abs() > 1e-6 || rounded > f64::from(u8::MAX) {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: format!("expected percentage in 0..=255, got {f}"),
        });
    }
    Ok(rounded as u8)
}

fn parse_string_val(key: &str, val: &Value) -> Result<String, ProfileError> {
    let Some(scalar) = unwrap_scalar(val) else {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: "empty array cannot be parsed as string".to_owned(),
        });
    };
    match scalar {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: format!("expected string, got `{scalar}`"),
        }),
    }
}

fn parse_speed_val(key: &str, val: &Value, reference_speed: f64) -> Result<f64, ProfileError> {
    let Some(scalar) = unwrap_scalar(val) else {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: "empty array cannot be parsed as speed".to_owned(),
        });
    };
    if let Value::String(s) = scalar
        && let Some(pct_str) = s.trim().strip_suffix('%')
    {
        let pct = pct_str
            .trim()
            .parse::<f64>()
            .map_err(|_| ProfileError::InvalidValue {
                key: key.to_owned(),
                reason: format!("invalid speed percentage `{s}`"),
            })?;
        if !pct.is_finite() || pct < 0.0 {
            return Err(ProfileError::InvalidValue {
                key: key.to_owned(),
                reason: format!("invalid speed percentage `{s}`"),
            });
        }
        return Ok(reference_speed * (pct / 100.0));
    }
    parse_f64_val(key, scalar)
}

fn parse_density_fraction(key: &str, val: &Value) -> Result<f64, ProfileError> {
    let Some(scalar) = unwrap_scalar(val) else {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: "empty array cannot be parsed as density".to_owned(),
        });
    };
    if let Value::String(s) = scalar
        && let Some(pct_str) = s.trim().strip_suffix('%')
    {
        let pct = pct_str
            .trim()
            .parse::<f64>()
            .map_err(|_| ProfileError::InvalidValue {
                key: key.to_owned(),
                reason: format!("invalid density percentage `{s}`"),
            })?;
        if !pct.is_finite() {
            return Err(ProfileError::InvalidValue {
                key: key.to_owned(),
                reason: "non-finite density percentage".to_owned(),
            });
        }
        return Ok(pct / 100.0);
    }
    parse_f64_val(key, scalar)
}

fn parse_accel_opt(
    key: &str,
    val: &Value,
    default_accel: Option<f64>,
) -> Result<Option<f64>, ProfileError> {
    if is_nil_value(val) {
        return Ok(None);
    }
    let Some(scalar) = unwrap_scalar(val) else {
        return Ok(None);
    };
    let v = if let Value::String(s) = scalar
        && let Some(pct_str) = s.trim().strip_suffix('%')
    {
        let pct = pct_str
            .trim()
            .parse::<f64>()
            .map_err(|_| ProfileError::InvalidValue {
                key: key.to_owned(),
                reason: format!("invalid acceleration percentage `{s}`"),
            })?;
        if !pct.is_finite() || pct < 0.0 {
            return Err(ProfileError::InvalidValue {
                key: key.to_owned(),
                reason: format!("invalid acceleration percentage `{s}`"),
            });
        }
        default_accel.unwrap_or(3000.0) * (pct / 100.0)
    } else {
        parse_f64_val(key, scalar)?
    };
    if v == 0.0 { Ok(None) } else { Ok(Some(v)) }
}

fn parse_line_width_opt(
    key: &str,
    val: &Value,
    nozzle_d: f64,
) -> Result<Option<f64>, ProfileError> {
    if is_nil_value(val) {
        return Ok(None);
    }
    let Some(scalar) = unwrap_scalar(val) else {
        return Ok(None);
    };
    if let Value::String(s) = scalar
        && let Some(pct_str) = s.trim().strip_suffix('%')
    {
        let pct = pct_str
            .trim()
            .parse::<f64>()
            .map_err(|_| ProfileError::InvalidValue {
                key: key.to_owned(),
                reason: format!("invalid line width percentage `{s}`"),
            })?;
        if !pct.is_finite() || pct < 0.0 {
            return Err(ProfileError::InvalidValue {
                key: key.to_owned(),
                reason: format!("invalid line width percentage `{s}`"),
            });
        }
        if pct == 0.0 {
            return Ok(None);
        }
        return Ok(Some(nozzle_d * (pct / 100.0)));
    }
    let mm = parse_f64_val(key, scalar)?;
    if mm < 0.0 {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: "line width cannot be negative".to_owned(),
        });
    }
    if mm == 0.0 { Ok(None) } else { Ok(Some(mm)) }
}

fn parse_xy_pair(key: &str, item: &str) -> Result<(f64, f64), ProfileError> {
    let trimmed = item.trim();
    let sep = if trimmed.contains('x') {
        'x'
    } else if trimmed.contains('X') {
        'X'
    } else if trimmed.contains(',') {
        ','
    } else {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: format!("expected `XxY` coordinate pair, got `{trimmed}`"),
        });
    };
    let Some((xs, ys)) = trimmed.split_once(sep) else {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: format!("invalid coordinate pair `{trimmed}`"),
        });
    };
    let x = xs
        .trim()
        .parse::<f64>()
        .map_err(|_| ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: format!("invalid X coordinate `{xs}`"),
        })?;
    let y = ys
        .trim()
        .parse::<f64>()
        .map_err(|_| ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: format!("invalid Y coordinate `{ys}`"),
        })?;
    if !x.is_finite() || !y.is_finite() {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: "non-finite bed coordinate".to_owned(),
        });
    }
    Ok((x, y))
}

fn parse_printable_area(key: &str, val: &Value) -> Result<(f64, f64), ProfileError> {
    let raw_items: Vec<String> = match val {
        Value::Array(arr) => {
            let mut items = Vec::with_capacity(arr.len());
            for elem in arr {
                let s = elem.as_str().ok_or_else(|| ProfileError::InvalidValue {
                    key: key.to_owned(),
                    reason: "bed area coordinates must be strings".to_owned(),
                })?;
                items.push(s.to_owned());
            }
            items
        }
        Value::String(s) => s
            .split([',', ';'])
            .flat_map(str::split_whitespace)
            .map(str::to_owned)
            .collect(),
        _ => {
            return Err(ProfileError::InvalidValue {
                key: key.to_owned(),
                reason: "expected printable_area array or string".to_owned(),
            });
        }
    };

    if raw_items.len() < 3 {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: "printable_area must define at least 3 polygon vertices".to_owned(),
        });
    }

    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;

    for item in &raw_items {
        let (x, y) = parse_xy_pair(key, item)?;
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }

    let width = max_x - min_x;
    let depth = max_y - min_y;
    if !width.is_finite() || !depth.is_finite() || width <= 0.0 || depth <= 0.0 {
        return Err(ProfileError::InvalidValue {
            key: key.to_owned(),
            reason: "printable_area polygon has zero or degenerate span".to_owned(),
        });
    }
    Ok((width, depth))
}

fn apply_macro_setting(
    target: &mut Option<String>,
    key: &str,
    val: &Value,
    ctx: &mut MapperCtx,
) -> Result<(), ProfileError> {
    if is_nil_value(val) {
        *target = None;
        ctx.mark_applied(key);
        return Ok(());
    }
    let s = parse_string_val(key, val)?;
    if s.trim().is_empty() {
        *target = None;
    } else {
        *target = Some(s);
    }
    ctx.mark_applied(key);
    Ok(())
}

fn apply_machine_settings(
    machine: &ResolvedProfile,
    cfg: &mut PrintConfig,
    ctx: &mut MapperCtx,
) -> Result<(), ProfileError> {
    let s = &machine.settings;

    if let Some((k, v)) = ctx.get_primary(s, &["gcode_flavor"])
        && !is_nil_value(v)
    {
        let raw = parse_string_val(k, v)?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "klipper" => cfg.machine.flavor = GcodeFlavor::Klipper,
            "marlin_buddy" | "buddy" | "prusa_buddy" => {
                cfg.machine.flavor = GcodeFlavor::MarlinBuddy;
            }
            "marlin" | "marlin2" | "marlinlegacy" => cfg.machine.flavor = GcodeFlavor::Marlin,
            "reprapfirmware" | "reprap" | "smoothie" | "sprinter" | "repetier" => {
                cfg.machine.flavor = GcodeFlavor::Marlin;
                ctx.warn(
                    &machine.name,
                    k,
                    format!("firmware dialect `{raw}` approximated using `marlin`"),
                );
            }
            _ => {
                return Err(ProfileError::InvalidValue {
                    key: k.to_owned(),
                    reason: format!("unsupported gcode_flavor `{raw}`"),
                });
            }
        }
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["nozzle_diameter"])
        && !is_nil_value(v)
    {
        cfg.machine.nozzle_diameter_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["nozzle_high_flow", "high_flow_nozzle"])
        && !is_nil_value(v)
    {
        cfg.machine.nozzle_high_flow = parse_bool_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["printer_model"])
        && !is_nil_value(v)
    {
        let model = parse_string_val(k, v)?;
        let trimmed = model.trim();
        cfg.machine.printer_model = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        };
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["remaining_times"])
        && !is_nil_value(v)
    {
        cfg.machine.emit_progress_m73 = parse_bool_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["printable_area", "bed_shape"])
        && !is_nil_value(v)
    {
        let (w, d) = parse_printable_area(k, v)?;
        cfg.machine.bed_width_mm = w;
        cfg.machine.bed_depth_mm = d;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["printable_height", "max_print_height"])
        && !is_nil_value(v)
    {
        cfg.machine.bed_height_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["use_relative_e_distances"])
        && !is_nil_value(v)
    {
        cfg.machine.relative_extrusion = parse_bool_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["retraction_length"])
        && !is_nil_value(v)
    {
        cfg.machine.retraction_length_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["retraction_speed"])
        && !is_nil_value(v)
    {
        cfg.machine.retraction_speed_mm_s = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["travel_speed"])
        && !is_nil_value(v)
    {
        cfg.machine.travel_speed_mm_s = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["z_hop"])
        && !is_nil_value(v)
    {
        cfg.machine.z_hop_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["z_hop_speed", "machine_max_speed_z"])
        && !is_nil_value(v)
    {
        cfg.machine.z_hop_speed_mm_s = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["wipe"])
        && !is_nil_value(v)
    {
        cfg.machine.wipe_on_retract = parse_bool_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["wipe_distance"])
        && !is_nil_value(v)
    {
        cfg.machine.wipe_distance_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["retraction_minimum_travel"])
        && !is_nil_value(v)
    {
        cfg.machine.min_travel_for_retract_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["machine_start_gcode"]) {
        apply_macro_setting(&mut cfg.machine.start_gcode, k, v, ctx)?;
    }
    if let Some((k, v)) = ctx.get_primary(s, &["machine_end_gcode"]) {
        apply_macro_setting(&mut cfg.machine.end_gcode, k, v, ctx)?;
    }
    if let Some((k, v)) = ctx.get_primary(s, &["layer_change_gcode", "before_layer_change_gcode"]) {
        apply_macro_setting(&mut cfg.machine.layer_change_gcode, k, v, ctx)?;
    }

    Ok(())
}

fn pick_bed_plate_keys(curr_bed_type: Option<&str>) -> Option<(&'static str, &'static str)> {
    let raw = curr_bed_type?.trim().to_ascii_lowercase();
    if raw.is_empty() {
        return None;
    }
    if raw.contains("textured") {
        Some(("textured_plate_temp", "textured_plate_temp_initial_layer"))
    } else if raw.contains("engineering") || raw.contains("eng plate") {
        Some(("eng_plate_temp", "eng_plate_temp_initial_layer"))
    } else if raw.contains("cool") {
        Some(("cool_plate_temp", "cool_plate_temp_initial_layer"))
    } else if raw.contains("supertack") {
        Some(("supertack_plate_temp", "supertack_plate_temp_initial_layer"))
    } else if raw.contains("high temp") || raw.contains("hot") || raw.contains("pei") {
        Some(("hot_plate_temp", "hot_plate_temp_initial_layer"))
    } else {
        None
    }
}

fn select_bed_temp(
    settings: &BTreeMap<String, Value>,
    preferred_key: Option<&'static str>,
    fallback_keys: &[&'static str],
    ctx: &mut MapperCtx,
) -> Result<Option<u16>, ProfileError> {
    ctx.mark_recognized_if_present(settings, fallback_keys);
    if let Some(pk) = preferred_key {
        ctx.mark_recognized_if_present(settings, &[pk]);
        if let Some(v) = settings.get(pk)
            && !is_nil_value(v)
        {
            let temp = parse_u16_val(pk, v)?;
            ctx.mark_applied(pk);
            return Ok(Some(temp));
        }
    }

    let mut zero_match: Option<&'static str> = None;
    for &k in fallback_keys {
        if let Some(v) = settings.get(k)
            && !is_nil_value(v)
        {
            let temp = parse_u16_val(k, v)?;
            if temp > 0 {
                ctx.mark_applied(k);
                return Ok(Some(temp));
            }
            if zero_match.is_none() {
                zero_match = Some(k);
            }
        }
    }

    if let Some(zk) = zero_match {
        ctx.mark_applied(zk);
        Ok(Some(0))
    } else {
        Ok(None)
    }
}

fn apply_filament_settings(
    filament: &ResolvedProfile,
    machine: Option<&ResolvedProfile>,
    cfg: &mut PrintConfig,
    ctx: &mut MapperCtx,
) -> Result<(), ProfileError> {
    let s = &filament.settings;

    if let Some((k, v)) = ctx.get_primary(s, &["filament_type"])
        && !is_nil_value(v)
    {
        let ft = parse_string_val(k, v)?;
        let trimmed = ft.trim();
        cfg.filament.filament_type = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        };
        ctx.mark_applied(k);
    }

    let pa_enabled = if let Some((k, v)) = ctx.get_primary(s, &["enable_pressure_advance"])
        && !is_nil_value(v)
    {
        let b = parse_bool_val(k, v)?;
        ctx.mark_applied(k);
        Some(b)
    } else {
        None
    };

    if pa_enabled == Some(false) {
        ctx.mark_recognized_if_present(s, &["pressure_advance"]);
        cfg.filament.pressure_advance = None;
    } else if let Some((k, v)) = ctx.get_primary(s, &["pressure_advance"])
        && !is_nil_value(v)
    {
        cfg.filament.pressure_advance = Some(parse_f64_val(k, v)?);
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["chamber_temperature", "chamber_temperatures"])
        && !is_nil_value(v)
    {
        cfg.filament.chamber_temp_c = Some(parse_u16_val(k, v)?);
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["wait_for_chamber_temp"])
        && !is_nil_value(v)
    {
        cfg.filament.wait_for_chamber_temp = parse_bool_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["filament_diameter"])
        && !is_nil_value(v)
    {
        cfg.filament.diameter_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["filament_density"])
        && !is_nil_value(v)
    {
        cfg.filament.density_g_cm3 = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["nozzle_temperature", "temperature"])
        && !is_nil_value(v)
    {
        cfg.filament.nozzle_temp_c = parse_u16_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(
        s,
        &[
            "nozzle_temperature_initial_layer",
            "first_layer_temperature",
        ],
    ) && !is_nil_value(v)
    {
        cfg.filament.first_layer_nozzle_temp_c = Some(parse_u16_val(k, v)?);
        ctx.mark_applied(k);
    }

    let curr_bed_type = s
        .get("curr_bed_type")
        .or_else(|| machine.and_then(|m| m.settings.get("curr_bed_type")))
        .and_then(unwrap_scalar)
        .and_then(Value::as_str);
    let preferred_plate = pick_bed_plate_keys(curr_bed_type);

    const BED_KEYS: &[&str] = &[
        "hot_plate_temp",
        "textured_plate_temp",
        "eng_plate_temp",
        "cool_plate_temp",
        "supertack_plate_temp",
        "bed_temperature",
    ];
    const FIRST_BED_KEYS: &[&str] = &[
        "hot_plate_temp_initial_layer",
        "textured_plate_temp_initial_layer",
        "eng_plate_temp_initial_layer",
        "cool_plate_temp_initial_layer",
        "supertack_plate_temp_initial_layer",
        "bed_temperature_initial_layer",
        "bed_temperature_initial_layer_single",
        "first_layer_bed_temperature",
    ];

    if let Some(temp) = select_bed_temp(s, preferred_plate.map(|(n, _)| n), BED_KEYS, ctx)? {
        cfg.filament.bed_temp_c = temp;
    }
    if let Some(temp) = select_bed_temp(s, preferred_plate.map(|(_, i)| i), FIRST_BED_KEYS, ctx)? {
        cfg.filament.first_layer_bed_temp_c = Some(temp);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["filament_max_volumetric_speed"])
        && !is_nil_value(v)
    {
        cfg.filament.max_volumetric_speed_mm3_s = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["fan_min_speed"])
        && !is_nil_value(v)
    {
        cfg.filament.fan_min_percent = parse_u8_percent_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["fan_max_speed"])
        && !is_nil_value(v)
    {
        cfg.filament.fan_max_percent = parse_u8_percent_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(
        s,
        &["close_fan_the_first_x_layers", "disable_fan_first_layers"],
    ) && !is_nil_value(v)
    {
        cfg.filament.disable_fan_first_layers = parse_u16_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["full_fan_speed_layer"])
        && !is_nil_value(v)
    {
        cfg.filament.full_fan_speed_layer = parse_u16_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["slow_down_layer_time", "fan_cooling_layer_time"])
        && !is_nil_value(v)
    {
        cfg.filament.slow_down_layer_time_s = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["slow_down_for_layer_cooling"])
        && !is_nil_value(v)
    {
        let enabled = parse_bool_val(k, v)?;
        if !enabled {
            cfg.filament.slow_down_layer_time_s = 0.0;
        }
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["slow_down_min_speed"])
        && !is_nil_value(v)
    {
        cfg.filament.slow_down_min_speed_mm_s = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["overhang_fan_speed", "bridge_fan_speed"])
        && !is_nil_value(v)
    {
        cfg.filament.bridge_fan_percent = parse_u8_percent_val(k, v)?;
        ctx.mark_applied(k);
    }

    // Filament-level machine retraction / travel overrides (non-nil only).
    if let Some((k, v)) = ctx.get_primary(s, &["filament_retraction_length"])
        && !is_nil_value(v)
    {
        cfg.machine.retraction_length_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["filament_retraction_speed"])
        && !is_nil_value(v)
    {
        cfg.machine.retraction_speed_mm_s = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["filament_z_hop"])
        && !is_nil_value(v)
    {
        cfg.machine.z_hop_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["filament_wipe"])
        && !is_nil_value(v)
    {
        cfg.machine.wipe_on_retract = parse_bool_val(k, v)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["filament_wipe_distance"])
        && !is_nil_value(v)
    {
        cfg.machine.wipe_distance_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["filament_retraction_minimum_travel"])
        && !is_nil_value(v)
    {
        cfg.machine.min_travel_for_retract_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    Ok(())
}

fn apply_process_settings_pass1(
    process: &ResolvedProfile,
    cfg: &mut PrintConfig,
    ctx: &mut MapperCtx,
) -> Result<(), ProfileError> {
    let s = &process.settings;

    if let Some((k, v)) = ctx.get_primary(s, &["layer_height"])
        && !is_nil_value(v)
    {
        cfg.process.layer_height_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["initial_layer_print_height", "first_layer_height"])
        && !is_nil_value(v)
    {
        cfg.process.first_layer_height_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["wall_loops", "perimeters"])
        && !is_nil_value(v)
    {
        cfg.process.wall_loops = parse_u16_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["wall_generator"])
        && !is_nil_value(v)
    {
        let raw = parse_string_val(k, v)?;
        cfg.process.wall_generator = match raw.trim().to_ascii_lowercase().as_str() {
            "classic" => WallGenerator::Classic,
            "arachne" => WallGenerator::Arachne,
            _ => {
                ctx.warn(
                    &process.name,
                    k,
                    format!("wall_generator `{raw}` approximated using `classic`"),
                );
                WallGenerator::Classic
            }
        };
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["min_bead_width"])
        && !is_nil_value(v)
    {
        cfg.process.arachne.min_bead_width_ratio = parse_density_fraction(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["min_feature_size"])
        && !is_nil_value(v)
    {
        cfg.process.arachne.min_feature_size_ratio = parse_density_fraction(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["wall_distribution_count"])
        && !is_nil_value(v)
    {
        cfg.process.arachne.wall_distribution_count = parse_u16_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["wall_transition_length"])
        && !is_nil_value(v)
    {
        cfg.process.arachne.transition_length_ratio = parse_density_fraction(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(
        s,
        &["wall_transition_angle", "wall_transition_threshold_angle"],
    ) && !is_nil_value(v)
    {
        cfg.process.arachne.transition_threshold_angle_deg = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["wall_transition_filter_deviation"])
        && !is_nil_value(v)
    {
        cfg.process.arachne.transition_filter_deviation_ratio = parse_density_fraction(k, v)?;
        ctx.mark_applied(k);
    }

    ctx.mark_recognized_if_present(s, &["min_length_factor", "wall_transition_filter_margin"]);

    if let Some((k, v)) = ctx.get_primary(s, &["top_shell_layers", "top_solid_layers"])
        && !is_nil_value(v)
    {
        cfg.process.top_shell_layers = parse_u16_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["bottom_shell_layers", "bottom_solid_layers"])
        && !is_nil_value(v)
    {
        cfg.process.bottom_shell_layers = parse_u16_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["sparse_infill_density", "fill_density"])
        && !is_nil_value(v)
    {
        cfg.process.sparse_infill_density = parse_density_fraction(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["sparse_infill_pattern", "fill_pattern"])
        && !is_nil_value(v)
    {
        let raw = parse_string_val(k, v)?;
        let norm = raw.trim().to_ascii_lowercase();
        cfg.process.sparse_infill_pattern = match norm.as_str() {
            "rectilinear" => InfillPattern::Rectilinear,
            "grid" => InfillPattern::Grid,
            "gyroid" => InfillPattern::Gyroid,
            "cubic" => InfillPattern::Cubic,
            "alignedrectilinear" | "line" | "monotonic" | "monotonicline" => {
                ctx.warn(
                    &process.name,
                    k,
                    format!("infill pattern `{raw}` approximated with `rectilinear`"),
                );
                InfillPattern::Rectilinear
            }
            "adaptivecubic" | "supportcubic" => {
                ctx.warn(
                    &process.name,
                    k,
                    format!("infill pattern `{raw}` approximated with `cubic`"),
                );
                InfillPattern::Cubic
            }
            _ => {
                ctx.warn(
                    &process.name,
                    k,
                    format!("infill pattern `{raw}` approximated with `grid`"),
                );
                InfillPattern::Grid
            }
        };
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["travel_speed"])
        && !is_nil_value(v)
    {
        cfg.machine.travel_speed_mm_s = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["outer_wall_speed"])
        && !is_nil_value(v)
    {
        let ref_spd = cfg.process.outer_wall_speed_mm_s;
        cfg.process.outer_wall_speed_mm_s = parse_speed_val(k, v, ref_spd)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["inner_wall_speed"])
        && !is_nil_value(v)
    {
        let ref_spd = cfg.process.outer_wall_speed_mm_s;
        cfg.process.inner_wall_speed_mm_s = parse_speed_val(k, v, ref_spd)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["sparse_infill_speed"])
        && !is_nil_value(v)
    {
        let ref_spd = cfg.process.outer_wall_speed_mm_s;
        cfg.process.infill_speed_mm_s = parse_speed_val(k, v, ref_spd)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["initial_layer_speed"])
        && !is_nil_value(v)
    {
        let ref_spd = cfg.process.outer_wall_speed_mm_s;
        cfg.process.first_layer_speed_mm_s = parse_speed_val(k, v, ref_spd)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["initial_layer_infill_speed"])
        && !is_nil_value(v)
    {
        let spd = parse_speed_val(k, v, cfg.process.first_layer_speed_mm_s)?;
        cfg.process.first_layer_infill_speed_mm_s = if spd == 0.0 { None } else { Some(spd) };
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["top_surface_speed"])
        && !is_nil_value(v)
    {
        let spd = parse_speed_val(k, v, cfg.process.infill_speed_mm_s)?;
        cfg.process.top_surface_speed_mm_s = if spd == 0.0 { None } else { Some(spd) };
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["internal_solid_infill_speed"])
        && !is_nil_value(v)
    {
        let spd = parse_speed_val(k, v, cfg.process.infill_speed_mm_s)?;
        cfg.process.solid_infill_speed_mm_s = if spd == 0.0 { None } else { Some(spd) };
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["seam_position"])
        && !is_nil_value(v)
    {
        let raw = parse_string_val(k, v)?;
        cfg.process.seam_position = match raw.trim().to_ascii_lowercase().as_str() {
            "aligned" => SeamPosition::Aligned,
            "back" | "rear" => SeamPosition::Back,
            "nearest" => SeamPosition::Nearest,
            "random" => SeamPosition::Random,
            _ => {
                ctx.warn(
                    &process.name,
                    k,
                    format!("seam position `{raw}` approximated with `aligned`"),
                );
                SeamPosition::Aligned
            }
        };
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["detect_bridges"])
        && !is_nil_value(v)
    {
        cfg.process.detect_bridges = parse_bool_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["bridge_speed"])
        && !is_nil_value(v)
    {
        cfg.process.bridge_speed_mm_s = parse_speed_val(k, v, cfg.process.outer_wall_speed_mm_s)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["bridge_flow", "bridge_flow_ratio"])
        && !is_nil_value(v)
    {
        cfg.process.bridge_flow_ratio = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["detect_overhang_wall", "enable_overhang_speed"])
        && !is_nil_value(v)
    {
        cfg.process.detect_overhangs = parse_bool_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["overhang_threshold_deg"])
        && !is_nil_value(v)
    {
        cfg.process.overhang_threshold_deg = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(
        s,
        &[
            "overhang_3_4_speed",
            "overhang_4_4_speed",
            "overhang_2_4_speed",
            "overhang_speed",
        ],
    ) && !is_nil_value(v)
    {
        let spd = parse_speed_val(k, v, cfg.process.outer_wall_speed_mm_s)?;
        if spd != 0.0 {
            cfg.process.overhang_speed_mm_s = spd;
        }
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["monotonic_solid_infill"])
        && !is_nil_value(v)
    {
        cfg.process.monotonic_solid_infill = parse_bool_val(k, v)?;
        ctx.mark_applied(k);
    } else if let Some((k, v)) =
        ctx.get_primary(s, &["top_surface_pattern", "bottom_surface_pattern"])
        && !is_nil_value(v)
    {
        let pat = parse_string_val(k, v)?;
        let mono = pat.trim().to_ascii_lowercase().starts_with("monotonic");
        cfg.process.monotonic_solid_infill = mono;
        if !mono {
            ctx.warn(
                &process.name,
                k,
                format!("surface pattern `{pat}` sets `monotonic_solid_infill = false`"),
            );
        }
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["skirt_loops", "skirts"])
        && !is_nil_value(v)
    {
        cfg.process.skirt_loops = parse_u16_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["brim_width"])
        && !is_nil_value(v)
    {
        cfg.process.brim_width_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["brim_type"])
        && !is_nil_value(v)
    {
        let bt = parse_string_val(k, v)?;
        match bt.trim().to_ascii_lowercase().as_str() {
            "no_brim" | "none" => {
                cfg.process.brim_width_mm = 0.0;
            }
            "outer_only" | "auto" => {}
            other => {
                if cfg.process.brim_width_mm > 0.0 {
                    ctx.warn(
                        &process.name,
                        k,
                        format!("brim type `{other}` approximated using outer brim"),
                    );
                }
            }
        }
        ctx.mark_applied(k);
    }

    // Support configuration
    if let Some((k, v)) = ctx.get_primary(s, &["enable_support"])
        && !is_nil_value(v)
    {
        cfg.process.support.enabled = parse_bool_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["support_style", "support_type"])
        && !is_nil_value(v)
    {
        let raw = parse_string_val(k, v)?;
        let norm = raw.trim().to_ascii_lowercase();
        cfg.process.support.style = if norm == "snug" {
            SupportStyle::Snug
        } else if norm == "grid" || norm == "default" || norm.starts_with("normal") {
            SupportStyle::Grid
        } else if norm.starts_with("tree") || norm == "organic" {
            SupportStyle::Tree
        } else {
            ctx.warn(
                &process.name,
                k,
                format!("unknown support style `{raw}` mapped to `grid`"),
            );
            SupportStyle::Grid
        };
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["support_on_build_plate_only"])
        && !is_nil_value(v)
    {
        cfg.process.support.on_build_plate_only = parse_bool_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["support_threshold_angle"])
        && !is_nil_value(v)
    {
        let angle = parse_f64_val(k, v)?;
        if angle == 0.0 {
            cfg.process.support.threshold_angle_deg = 45.0;
            ctx.warn(
                &process.name,
                k,
                "`support_threshold_angle: 0` mapped to default 45.0 deg",
            );
        } else {
            cfg.process.support.threshold_angle_deg = angle;
        }
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["support_top_z_distance"])
        && !is_nil_value(v)
    {
        cfg.process.support.top_z_distance_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["support_bottom_z_distance"])
        && !is_nil_value(v)
    {
        cfg.process.support.bottom_z_distance_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["support_object_xy_distance", "support_xy_distance"])
        && !is_nil_value(v)
    {
        cfg.process.support.xy_distance_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["support_base_pattern_spacing"])
        && !is_nil_value(v)
    {
        cfg.process.support.base_pattern_spacing_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(
        s,
        &["support_interface_top_layers", "support_interface_layers"],
    ) && !is_nil_value(v)
    {
        cfg.process.support.interface_layers = parse_u16_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["support_interface_spacing"])
        && !is_nil_value(v)
    {
        cfg.process.support.interface_spacing_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(
        s,
        &[
            "tree_support_branch_angle",
            "tree_support_branch_angle_organic",
        ],
    ) && !is_nil_value(v)
    {
        cfg.process.support.tree.max_branch_angle_deg = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["tree_support_tip_diameter"])
        && !is_nil_value(v)
    {
        cfg.process.support.tree.tip_diameter_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(
        s,
        &[
            "tree_support_branch_diameter",
            "tree_support_branch_diameter_organic",
        ],
    ) && !is_nil_value(v)
    {
        cfg.process.support.tree.branch_diameter_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["tree_support_branch_diameter_angle"])
        && !is_nil_value(v)
    {
        cfg.process.support.tree.branch_diameter_angle_deg = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(
        s,
        &[
            "tree_support_branch_distance",
            "tree_support_branch_distance_organic",
        ],
    ) && !is_nil_value(v)
    {
        cfg.process.support.tree.branch_distance_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["tree_support_wall_count"])
        && !is_nil_value(v)
    {
        let walls = parse_u16_val(k, v)?;
        if walls > 0 {
            cfg.process.support.tree.branch_wall_loops = walls;
        }
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["tree_support_brim_width"])
        && !is_nil_value(v)
    {
        cfg.process.support.tree.trunk_base_flare_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    ctx.mark_recognized_if_present(
        s,
        &[
            "tree_support_auto_brim",
            "tree_support_adaptive_layer_height",
            "tree_support_top_rate",
        ],
    );

    if let Some((k, v)) = ctx.get_primary(s, &["default_acceleration"])
        && !is_nil_value(v)
    {
        cfg.process.default_acceleration_mm_s2 = parse_accel_opt(k, v, None)?;
        ctx.mark_applied(k);
    }

    let ref_accel = cfg.process.default_acceleration_mm_s2;
    if let Some((k, v)) = ctx.get_primary(s, &["outer_wall_acceleration"])
        && !is_nil_value(v)
    {
        cfg.process.outer_wall_acceleration_mm_s2 = parse_accel_opt(k, v, ref_accel)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["inner_wall_acceleration"])
        && !is_nil_value(v)
    {
        cfg.process.inner_wall_acceleration_mm_s2 = parse_accel_opt(k, v, ref_accel)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(
        s,
        &["initial_layer_acceleration", "first_layer_acceleration"],
    ) && !is_nil_value(v)
    {
        cfg.process.first_layer_acceleration_mm_s2 = parse_accel_opt(k, v, ref_accel)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["travel_acceleration"])
        && !is_nil_value(v)
    {
        cfg.process.travel_acceleration_mm_s2 = parse_accel_opt(k, v, ref_accel)?;
        ctx.mark_applied(k);
    }

    // Ironing settings
    if let Some((k, v)) = ctx.get_primary(s, &["ironing_type"])
        && !is_nil_value(v)
    {
        let raw = parse_string_val(k, v)?;
        let norm = raw.trim().to_ascii_lowercase();
        cfg.process.ironing.mode = match norm.as_str() {
            "no ironing" | "no_ironing" | "none" | "off" => IroningType::Off,
            "top" | "topmost" | "topmost_surface" | "topmost_surface_only" => {
                IroningType::TopmostSurface
            }
            "all top" | "all_top" | "all_top_surfaces" | "top_surfaces" => {
                IroningType::AllTopSurfaces
            }
            _ => {
                ctx.warn(
                    &process.name,
                    k,
                    format!("ironing_type `{raw}` approximated using `all_top_surfaces`"),
                );
                IroningType::AllTopSurfaces
            }
        };
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["ironing_flow"])
        && !is_nil_value(v)
    {
        cfg.process.ironing.flow_ratio = parse_density_fraction(k, v)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["ironing_spacing"])
        && !is_nil_value(v)
    {
        cfg.process.ironing.line_spacing_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["ironing_inset"])
        && !is_nil_value(v)
    {
        cfg.process.ironing.inset_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["ironing_speed"])
        && !is_nil_value(v)
    {
        cfg.process.ironing.speed_mm_s = parse_speed_val(k, v, cfg.process.outer_wall_speed_mm_s)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["ironing_angle"])
        && !is_nil_value(v)
    {
        let angle = parse_f64_val(k, v)?;
        if (angle - (-1.0)).abs() > 1e-6 {
            cfg.process.ironing.angle_offset_deg = angle;
        }
        ctx.mark_applied(k);
    }

    // Scarf seam settings
    if let Some((k, v)) = ctx.get_primary(s, &["seam_slope_type"])
        && !is_nil_value(v)
    {
        let raw = parse_string_val(k, v)?;
        let norm = raw.trim().to_ascii_lowercase();
        match norm.as_str() {
            "none" | "off" => cfg.process.scarf_seam.enabled = false,
            "contour" | "contour_and_hole" | "all" => cfg.process.scarf_seam.enabled = true,
            _ => {
                ctx.warn(
                    &process.name,
                    k,
                    format!("seam_slope_type `{raw}` approximated as enabled scarf seam"),
                );
                cfg.process.scarf_seam.enabled = true;
            }
        }
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["seam_slope_start_height"])
        && !is_nil_value(v)
    {
        cfg.process.scarf_seam.start_height_ratio = parse_density_fraction(k, v)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["seam_slope_min_length"])
        && !is_nil_value(v)
    {
        cfg.process.scarf_seam.min_length_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["seam_slope_length", "scarf_seam_length"])
        && !is_nil_value(v)
    {
        cfg.process.scarf_seam.length_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["seam_slope_steps"])
        && !is_nil_value(v)
    {
        cfg.process.scarf_seam.steps = parse_u16_val(k, v)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["seam_slope_entire_loop"])
        && !is_nil_value(v)
    {
        cfg.process.scarf_seam.entire_loop = parse_bool_val(k, v)?;
        ctx.mark_applied(k);
    }
    ctx.mark_recognized_if_present(s, &["scarf_angle_threshold", "seam_slope_conditional"]);

    // Arc fitting settings
    if let Some((k, v)) = ctx.get_primary(s, &["enable_arc_fitting"])
        && !is_nil_value(v)
    {
        cfg.process.arc_fitting = parse_bool_val(k, v)?;
        ctx.mark_applied(k);
    }
    if let Some((k, v)) = ctx.get_primary(s, &["arc_fitting_tolerance"])
        && !is_nil_value(v)
    {
        cfg.process.arc_tolerance_mm = parse_f64_val(k, v)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["support_speed"])
        && !is_nil_value(v)
    {
        cfg.process.support.speed_mm_s = parse_speed_val(k, v, cfg.process.infill_speed_mm_s)?;
        ctx.mark_applied(k);
    }

    if let Some((k, v)) = ctx.get_primary(s, &["support_interface_speed"])
        && !is_nil_value(v)
    {
        let spd = parse_speed_val(k, v, cfg.process.support.speed_mm_s)?;
        if spd != 0.0 {
            cfg.process.support.interface_speed_mm_s = spd;
        }
        ctx.mark_applied(k);
    }

    Ok(())
}

fn apply_process_line_widths_pass2(
    process: &ResolvedProfile,
    cfg: &mut PrintConfig,
    ctx: &mut MapperCtx,
) -> Result<(), ProfileError> {
    let s = &process.settings;
    let nozzle_d = cfg.machine.nozzle_diameter_mm;
    let auto_default = nozzle_d * 1.05;

    let base_line_width =
        if let Some((k, v)) = ctx.get_primary(s, &["line_width", "extrusion_width"]) {
            ctx.mark_applied(k);
            Some(parse_line_width_opt(k, v, nozzle_d)?.unwrap_or(auto_default))
        } else {
            None
        };

    if let Some((k, v)) = ctx.get_primary(s, &["outer_wall_line_width"]) {
        if let Some(w) = parse_line_width_opt(k, v, nozzle_d)? {
            cfg.process.outer_wall_width_mm = w;
        } else if let Some(bw) = base_line_width {
            cfg.process.outer_wall_width_mm = bw;
        }
        ctx.mark_applied(k);
    } else if let Some(bw) = base_line_width {
        cfg.process.outer_wall_width_mm = bw;
    }

    if let Some((k, v)) = ctx.get_primary(s, &["inner_wall_line_width"]) {
        if let Some(w) = parse_line_width_opt(k, v, nozzle_d)? {
            cfg.process.inner_wall_width_mm = w;
        } else if let Some(bw) = base_line_width {
            cfg.process.inner_wall_width_mm = bw;
        }
        ctx.mark_applied(k);
    } else if let Some(bw) = base_line_width {
        cfg.process.inner_wall_width_mm = bw;
    }

    if let Some((k, v)) = ctx.get_primary(
        s,
        &["internal_solid_infill_line_width", "top_surface_line_width"],
    ) {
        if let Some(w) = parse_line_width_opt(k, v, nozzle_d)? {
            cfg.process.solid_infill_width_mm = w;
        } else if let Some(bw) = base_line_width {
            cfg.process.solid_infill_width_mm = bw;
        }
        ctx.mark_applied(k);
    } else if let Some(bw) = base_line_width {
        cfg.process.solid_infill_width_mm = bw;
    }

    if let Some((k, v)) = ctx.get_primary(s, &["sparse_infill_line_width"]) {
        if let Some(w) = parse_line_width_opt(k, v, nozzle_d)? {
            cfg.process.sparse_infill_width_mm = w;
        } else if let Some(bw) = base_line_width {
            cfg.process.sparse_infill_width_mm = bw;
        }
        ctx.mark_applied(k);
    } else if let Some(bw) = base_line_width {
        cfg.process.sparse_infill_width_mm = bw;
    }

    if let Some((k, v)) = ctx.get_primary(s, &["support_line_width"]) {
        if let Some(w) = parse_line_width_opt(k, v, nozzle_d)? {
            cfg.process.support.line_width_mm = w;
        } else if let Some(bw) = base_line_width {
            cfg.process.support.line_width_mm = bw;
        }
        ctx.mark_applied(k);
    } else if let Some(bw) = base_line_width {
        cfg.process.support.line_width_mm = bw;
    }

    Ok(())
}

fn doc_from_parts(
    name: &str,
    kind: ProfileKind,
    instantiation: bool,
    inherits: Vec<String>,
    settings_val: Value,
) -> ProfileDocument {
    let settings = match settings_val {
        Value::Object(map) => map.into_iter().collect(),
        _ => BTreeMap::new(),
    };
    ProfileDocument {
        name: name.to_owned(),
        kind,
        instantiation,
        from: Some("layercraft".to_owned()),
        inherits,
        settings,
    }
}

fn builtin_preset_documents() -> Vec<ProfileDocument> {
    vec![
        // Abstract Machine Base -> Klipper & Marlin presets
        doc_from_parts(
            "lc_machine_base",
            ProfileKind::Machine,
            false,
            Vec::new(),
            json!({
                "nozzle_diameter": ["0.4"],
                "printable_height": "250",
                "retraction_length": ["0.8"],
                "retraction_speed": ["35"],
                "travel_speed": "150",
                "z_hop_speed": ["15"],
                "retraction_minimum_travel": ["1.0"],
                "wipe": ["0"],
                "wipe_distance": ["2.0"]
            }),
        ),
        doc_from_parts(
            "LayerCraft Generic Klipper 250mm",
            ProfileKind::Machine,
            true,
            vec!["lc_machine_base".to_owned()],
            json!({
                "gcode_flavor": "klipper",
                "printable_area": ["0x0", "250x0", "250x250", "0x250"],
                "use_relative_e_distances": "1",
                "z_hop": ["0.2"]
            }),
        ),
        doc_from_parts(
            "LayerCraft Generic Marlin 220mm",
            ProfileKind::Machine,
            true,
            vec!["lc_machine_base".to_owned()],
            json!({
                "gcode_flavor": "marlin",
                "printable_area": ["0x0", "220x0", "220x220", "0x220"],
                "printable_height": "240",
                "use_relative_e_distances": "0",
                "retraction_length": ["1.2"],
                "z_hop": ["0.0"]
            }),
        ),
        doc_from_parts(
            "LayerCraft CORE One+ HF 0.2mm",
            ProfileKind::Machine,
            true,
            vec!["lc_machine_base".to_owned()],
            json!({
                "gcode_flavor": "marlin_buddy",
                "printer_model": "COREONE",
                "nozzle_diameter": ["0.2"],
                "nozzle_high_flow": "1",
                "printable_area": ["0x0", "250x0", "250x220", "0x220"],
                "printable_height": "270",
                "use_relative_e_distances": "1",
                "retraction_length": ["0.7"],
                "retraction_speed": ["35"],
                "z_hop": ["0.2"],
                "wipe": ["1"],
                "wipe_distance": ["1.5"],
                "travel_speed": "250",
                "remaining_times": "1",
                "machine_start_gcode": concat!(
                    "M862.3 P \"[printer_model]\"\n",
                    "M862.1 P[nozzle_diameter] F{if nozzle_high_flow}1{else}0{endif}\n",
                    "M555 X[first_layer_print_min_x] Y[first_layer_print_min_y] W[first_layer_print_size_x] H[first_layer_print_size_y]\n",
                    "G90\n",
                    "M83\n",
                    "M104 S170\n",
                    "M140 S[first_layer_bed_temperature]\n",
                    "{if chamber_temperature > 0}M141 S[chamber_temperature]{else}M141 S0{endif}\n",
                    "M190 S[first_layer_bed_temperature]\n",
                    "M109 S170\n",
                    "G28\n",
                    "G29\n",
                    "G1 X5 Y5 Z10 F6000\n",
                    "M109 S[first_layer_temperature]\n",
                    "G1 Z0.2 F1200\n",
                    "G1 X65 E1.8 F600\n",
                    "G1 X75 E0.2 F600\n",
                    "G92 E0"
                ),
                "machine_end_gcode": concat!(
                    "G1 E-1.0 F2100\n",
                    "M104 S0\n",
                    "M140 S0\n",
                    "M141 S0\n",
                    "M107\n",
                    "G1 Z{min(max_z + 15, bed_height)} F900\n",
                    "G1 X{bed_width - 10} Y{bed_depth - 10} F9000\n",
                    "M84"
                )
            }),
        ),
        // Abstract Filament Base -> PLA, PETG, ABS
        doc_from_parts(
            "lc_filament_base",
            ProfileKind::Filament,
            false,
            Vec::new(),
            json!({
                "filament_diameter": ["1.75"],
                "slow_down_for_layer_cooling": ["1"],
                "slow_down_layer_time": ["8"],
                "slow_down_min_speed": ["15"]
            }),
        ),
        doc_from_parts(
            "LayerCraft Generic PLA",
            ProfileKind::Filament,
            true,
            vec!["lc_filament_base".to_owned()],
            json!({
                "filament_type": ["PLA"],
                "filament_density": ["1.24"],
                "nozzle_temperature": ["210"],
                "nozzle_temperature_initial_layer": ["215"],
                "hot_plate_temp": ["60"],
                "hot_plate_temp_initial_layer": ["60"],
                "chamber_temperature": ["0"],
                "pressure_advance": ["0.03"],
                "filament_max_volumetric_speed": ["15"],
                "fan_min_speed": ["35"],
                "fan_max_speed": ["100"],
                "close_fan_the_first_x_layers": ["1"],
                "full_fan_speed_layer": ["4"],
                "overhang_fan_speed": ["100"]
            }),
        ),
        doc_from_parts(
            "LayerCraft PLA (0.2mm HF Nozzle)",
            ProfileKind::Filament,
            true,
            vec!["LayerCraft Generic PLA".to_owned()],
            json!({
                "nozzle_temperature": ["215"],
                "nozzle_temperature_initial_layer": ["220"],
                "filament_max_volumetric_speed": ["4.5"],
                "pressure_advance": ["0.05"]
            }),
        ),
        doc_from_parts(
            "LayerCraft Generic PETG",
            ProfileKind::Filament,
            true,
            vec!["lc_filament_base".to_owned()],
            json!({
                "filament_type": ["PETG"],
                "filament_density": ["1.27"],
                "nozzle_temperature": ["240"],
                "nozzle_temperature_initial_layer": ["245"],
                "hot_plate_temp": ["80"],
                "hot_plate_temp_initial_layer": ["80"],
                "chamber_temperature": ["0"],
                "pressure_advance": ["0.04"],
                "filament_max_volumetric_speed": ["12"],
                "fan_min_speed": ["20"],
                "fan_max_speed": ["50"],
                "close_fan_the_first_x_layers": ["2"],
                "full_fan_speed_layer": ["5"],
                "overhang_fan_speed": ["80"],
                "filament_z_hop": ["0.4"]
            }),
        ),
        doc_from_parts(
            "LayerCraft Generic ABS",
            ProfileKind::Filament,
            true,
            vec!["lc_filament_base".to_owned()],
            json!({
                "filament_type": ["ABS"],
                "filament_density": ["1.05"],
                "nozzle_temperature": ["255"],
                "nozzle_temperature_initial_layer": ["260"],
                "hot_plate_temp": ["100"],
                "hot_plate_temp_initial_layer": ["100"],
                "chamber_temperature": ["50"],
                "pressure_advance": ["0.03"],
                "filament_max_volumetric_speed": ["16"],
                "fan_min_speed": ["0"],
                "fan_max_speed": ["20"],
                "close_fan_the_first_x_layers": ["3"],
                "full_fan_speed_layer": ["6"],
                "overhang_fan_speed": ["40"]
            }),
        ),
        // Abstract Process Base -> Standard 0.20, Quality 0.16, Draft 0.28, plus 0.2mm Nozzle Fine / Ultra-Fine
        doc_from_parts(
            "lc_process_base",
            ProfileKind::Process,
            false,
            Vec::new(),
            json!({
                "initial_layer_print_height": "0.2",
                "wall_loops": "2",
                "top_shell_layers": "3",
                "bottom_shell_layers": "3",
                "sparse_infill_density": "20%",
                "sparse_infill_pattern": "gyroid",
                "outer_wall_line_width": "0.42",
                "inner_wall_line_width": "0.45",
                "internal_solid_infill_line_width": "0.42",
                "sparse_infill_line_width": "0.45",
                "initial_layer_speed": "30",
                "seam_position": "aligned",
                "detect_bridges": "1",
                "bridge_speed": "30",
                "bridge_flow": "0.95",
                "detect_overhang_wall": "1",
                "overhang_threshold_deg": "45",
                "overhang_3_4_speed": "25",
                "monotonic_solid_infill": "1",
                "skirt_loops": "0",
                "brim_type": "no_brim",
                "brim_width": "0"
            }),
        ),
        doc_from_parts(
            "LayerCraft 0.20mm Standard",
            ProfileKind::Process,
            true,
            vec!["lc_process_base".to_owned()],
            json!({
                "layer_height": "0.2",
                "outer_wall_speed": "60",
                "inner_wall_speed": "100",
                "sparse_infill_speed": "120"
            }),
        ),
        doc_from_parts(
            "LayerCraft 0.16mm Quality",
            ProfileKind::Process,
            true,
            vec!["lc_process_base".to_owned()],
            json!({
                "layer_height": "0.16",
                "initial_layer_print_height": "0.2",
                "wall_loops": "3",
                "top_shell_layers": "5",
                "bottom_shell_layers": "4",
                "outer_wall_speed": "45",
                "inner_wall_speed": "80",
                "sparse_infill_speed": "100"
            }),
        ),
        doc_from_parts(
            "LayerCraft 0.28mm Draft",
            ProfileKind::Process,
            true,
            vec!["lc_process_base".to_owned()],
            json!({
                "layer_height": "0.28",
                "initial_layer_print_height": "0.28",
                "sparse_infill_pattern": "grid",
                "outer_wall_speed": "80",
                "inner_wall_speed": "120",
                "sparse_infill_speed": "150"
            }),
        ),
        doc_from_parts(
            "lc_process_020_base",
            ProfileKind::Process,
            false,
            vec!["lc_process_base".to_owned()],
            json!({
                "outer_wall_line_width": "0.22",
                "inner_wall_line_width": "0.24",
                "internal_solid_infill_line_width": "0.22",
                "sparse_infill_line_width": "0.24",
                "support_line_width": "0.22",
                "wall_loops": "3",
                "top_shell_layers": "6",
                "bottom_shell_layers": "5",
                "sparse_infill_density": "15%",
                "initial_layer_speed": "25",
                "bridge_speed": "25",
                "overhang_3_4_speed": "20",
                "default_acceleration": "4000",
                "outer_wall_acceleration": "2000",
                "inner_wall_acceleration": "3000",
                "initial_layer_acceleration": "800",
                "travel_acceleration": "6000",
                "support_top_z_distance": "0.10",
                "support_bottom_z_distance": "0.10",
                "support_object_xy_distance": "0.25",
                "support_base_pattern_spacing": "1.5",
                "support_interface_top_layers": "3",
                "support_interface_spacing": "0.15"
            }),
        ),
        doc_from_parts(
            "LayerCraft 0.10mm Fine (0.2mm Nozzle)",
            ProfileKind::Process,
            true,
            vec!["lc_process_020_base".to_owned()],
            json!({
                "layer_height": "0.10",
                "initial_layer_print_height": "0.12",
                "outer_wall_speed": "60",
                "inner_wall_speed": "90",
                "sparse_infill_speed": "120"
            }),
        ),
        doc_from_parts(
            "LayerCraft 0.08mm Ultra-Fine (0.2mm Nozzle)",
            ProfileKind::Process,
            true,
            vec!["lc_process_020_base".to_owned()],
            json!({
                "layer_height": "0.08",
                "initial_layer_print_height": "0.10",
                "top_shell_layers": "8",
                "bottom_shell_layers": "6",
                "outer_wall_speed": "45",
                "inner_wall_speed": "75",
                "sparse_infill_speed": "100"
            }),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_presets_resolve_and_apply_cleanly_across_combinations() {
        let lib = ProfileLibrary::with_builtin_presets();
        let machines = lib.list(Some(ProfileKind::Machine), false);
        let filaments = lib.list(Some(ProfileKind::Filament), false);
        let processes = lib.list(Some(ProfileKind::Process), false);

        assert_eq!(machines.len(), 3);
        assert_eq!(filaments.len(), 4);
        assert_eq!(processes.len(), 5);

        // Abstract bases excluded when include_abstract=false, included when true.
        assert_eq!(lib.list(Some(ProfileKind::Machine), true).len(), 4);

        let base = PrintConfig::default();
        for m in &machines {
            for f in &filaments {
                for p in &processes {
                    let is_02_machine = m.name.contains("0.2mm");
                    let is_02_process = p.name.contains("0.2mm Nozzle");
                    if is_02_machine && !is_02_process && p.name == "LayerCraft 0.28mm Draft" {
                        let err = lib
                            .build_print_config(&base, Some(&m.name), Some(&f.name), Some(&p.name))
                            .unwrap_err();
                        assert!(matches!(
                            err,
                            ProfileError::Config(ConfigError::Invalid {
                                field: "process.layer_height_mm",
                                ..
                            })
                        ));
                        continue;
                    }
                    // Verify all compatible presets (0.4mm machines with 0.4mm processes,
                    // 0.2mm machine with 0.2mm processes) apply cleanly with 0 warnings/unmapped.
                    if is_02_machine != is_02_process {
                        continue;
                    }
                    let res = lib
                        .build_print_config(&base, Some(&m.name), Some(&f.name), Some(&p.name))
                        .unwrap();
                    assert!(res.config.validate().is_ok());
                    assert!(
                        res.warnings.is_empty(),
                        "unexpected warnings for {} / {} / {}: {:?}",
                        m.name,
                        f.name,
                        p.name,
                        res.warnings
                    );
                    assert!(
                        res.unmapped_keys.is_empty(),
                        "unexpected unmapped keys: {:?}",
                        res.unmapped_keys
                    );
                    assert_eq!(res.machine_chain.len(), 2);
                    assert!(res.filament_chain.len() >= 2);
                    assert!(res.process_chain.len() >= 2);
                }
            }
        }
    }

    #[test]
    fn multi_level_inheritance_model_fallback_and_macro_clear() {
        let mut lib = ProfileLibrary::new();
        lib.load_json_str(
            r#"[
              {
                "type": "machine_model",
                "name": "LC Model X1",
                "nozzle_diameter": ["0.6"],
                "printable_area": ["0x0", "300x0", "300x300", "0x300"],
                "printable_height": "320"
              },
              {
                "type": "machine",
                "name": "common_machine",
                "instantiation": "false",
                "printer_model": "LC Model X1",
                "gcode_flavor": "klipper",
                "layer_change_gcode": ";OLD_LAYER=[layer_num]",
                "travel_speed": "180"
              },
              {
                "type": "machine",
                "name": "Child Printer",
                "inherits": "common_machine; extra_mixin",
                "layer_change_gcode": ""
              },
              {
                "type": "machine",
                "name": "extra_mixin",
                "instantiation": "false",
                "use_relative_e_distances": "1",
                "z_hop": ["0.35"]
              },
              {
                "type": "filament",
                "name": "pla_root",
                "instantiation": "false",
                "filament_diameter": ["1.75"],
                "curr_bed_type": "Textured PEI Plate",
                "hot_plate_temp": ["55"],
                "textured_plate_temp": ["65"],
                "textured_plate_temp_initial_layer": ["70"],
                "filament_z_hop": ["nil"]
              },
              {
                "type": "process",
                "name": "proc_020",
                "layer_height": "0.2",
                "line_width": "110%",
                "outer_wall_line_width": "0",
                "inner_wall_line_width": "120%",
                "sparse_infill_density": "25%",
                "outer_wall_speed": "80",
                "inner_wall_speed": "150%",
                "top_surface_speed": "0",
                "brim_width": "5",
                "brim_type": "no_brim",
                "custom_unknown_setting": "42"
              }
            ]"#,
        )
        .unwrap();

        let res = lib
            .build_print_config(
                &PrintConfig::default(),
                Some("Child Printer"),
                Some("pla_root"),
                Some("proc_020"),
            )
            .unwrap();

        assert_eq!(
            res.machine_chain,
            vec![
                "LC Model X1",
                "common_machine",
                "extra_mixin",
                "Child Printer"
            ]
        );
        assert!((res.config.machine.nozzle_diameter_mm - 0.6).abs() < 1e-9);
        assert!((res.config.machine.bed_width_mm - 300.0).abs() < 1e-9);
        assert!((res.config.machine.bed_height_mm - 320.0).abs() < 1e-9);
        assert!(res.config.machine.relative_extrusion);
        // "nil" filament_z_hop leaves machine's 0.35 z_hop untouched.
        assert!((res.config.machine.z_hop_mm - 0.35).abs() < 1e-9);
        // Child's "" clears parent's layer_change_gcode.
        assert_eq!(res.config.machine.layer_change_gcode, None);
        // curr_bed_type picked Textured PEI Plate.
        assert_eq!(res.config.filament.bed_temp_c, 65);
        assert_eq!(res.config.filament.first_layer_bed_temp_c, Some(70));
        // Percentage line widths computed using resolved 0.6mm nozzle diameter.
        assert!((res.config.process.outer_wall_width_mm - 0.66).abs() < 1e-9);
        assert!((res.config.process.inner_wall_width_mm - 0.72).abs() < 1e-9);
        assert!((res.config.process.sparse_infill_density - 0.25).abs() < 1e-9);
        assert!((res.config.process.inner_wall_speed_mm_s - 120.0).abs() < 1e-9);
        assert_eq!(res.config.process.top_surface_speed_mm_s, None);
        // brim_type: no_brim overrides brim_width: 5 -> 0.
        assert!((res.config.process.brim_width_mm - 0.0).abs() < 1e-9);
        // Only genuinely unmapped key appears; metadata & nil overrides do not.
        assert_eq!(res.unmapped_keys, vec!["custom_unknown_setting"]);
    }

    #[test]
    fn warnings_emitted_for_approximated_mappings() {
        let mut lib = ProfileLibrary::new();
        lib.load_json_str(
            r#"[
              { "type": "machine", "name": "rrf", "gcode_flavor": "reprapfirmware" },
              {
                "type": "process",
                "name": "tree_hc",
                "sparse_infill_pattern": "honeycomb",
                "support_style": "custom_unknown",
                "support_threshold_angle": "0",
                "enable_support": "1"
              }
            ]"#,
        )
        .unwrap();
        let res = lib
            .build_print_config(&PrintConfig::default(), Some("rrf"), None, Some("tree_hc"))
            .unwrap();
        assert_eq!(res.config.machine.flavor, GcodeFlavor::Marlin);
        assert_eq!(
            res.config.process.sparse_infill_pattern,
            InfillPattern::Grid
        );
        assert_eq!(res.config.process.support.style, SupportStyle::Grid);
        assert!((res.config.process.support.threshold_angle_deg - 45.0).abs() < 1e-9);
        assert_eq!(res.warnings.len(), 4);
    }

    #[test]
    fn hostile_profiles_fail_cleanly_without_panicking() {
        assert!(matches!(
            ProfileDocument::from_json_str("{not json"),
            Err(ProfileError::JsonParse(_))
        ));
        assert!(matches!(
            ProfileDocument::from_json_str("123"),
            Err(ProfileError::NotAnObject)
        ));
        assert!(matches!(
            ProfileDocument::from_json_str(r#"{"type": "machine"}"#),
            Err(ProfileError::MissingField { field: "name" })
        ));
        assert!(matches!(
            ProfileDocument::from_json_str(r#"{"name": "x"}"#),
            Err(ProfileError::MissingField { field: "type" })
        ));
        assert!(matches!(
            ProfileDocument::from_json_str(r#"{"name": "x", "type": "laser"}"#),
            Err(ProfileError::UnknownProfileType(_))
        ));

        let mut lib = ProfileLibrary::new();
        // Self-cycle
        lib.load_json_str(r#"{"name": "a", "type": "process", "inherits": "a"}"#)
            .unwrap();
        assert!(matches!(
            lib.resolve(ProfileKind::Process, "a"),
            Err(ProfileError::InheritanceCycle { .. })
        ));

        // 3-node cycle
        lib.load_json_str(
            r#"[
              {"name": "c1", "type": "filament", "inherits": "c2"},
              {"name": "c2", "type": "filament", "inherits": "c3"},
              {"name": "c3", "type": "filament", "inherits": "c1"}
            ]"#,
        )
        .unwrap();
        assert!(matches!(
            lib.resolve(ProfileKind::Filament, "c1"),
            Err(ProfileError::InheritanceCycle { .. })
        ));

        // Missing parent
        lib.load_json_str(r#"{"name": "orphan", "type": "process", "inherits": "ghost"}"#)
            .unwrap();
        assert!(matches!(
            lib.resolve(ProfileKind::Process, "orphan"),
            Err(ProfileError::NotFound { .. })
        ));

        // Exceeds MAX_INHERITANCE_DEPTH
        let mut chain_lib = ProfileLibrary::new();
        chain_lib
            .load_json_str(r#"{"name": "d0", "type": "process"}"#)
            .unwrap();
        for i in 1..=40 {
            let doc = format!(
                r#"{{"name": "d{i}", "type": "process", "inherits": "d{}"}}"#,
                i - 1
            );
            chain_lib.load_json_str(&doc).unwrap();
        }
        assert!(matches!(
            chain_lib.resolve(ProfileKind::Process, "d40"),
            Err(ProfileError::InheritanceDepthExceeded { .. })
        ));

        // Garbage numbers, degenerate area, out-of-range validated bounds
        for bad_proc in [
            r#"{"name": "bad1", "type": "process", "layer_height": "not_a_number"}"#,
            r#"{"name": "bad2", "type": "process", "layer_height": "0.9"}"#,
            r#"{"name": "bad3", "type": "process", "sparse_infill_density": "150%"}"#,
        ] {
            let mut l = ProfileLibrary::new();
            l.load_json_str(bad_proc).unwrap();
            let name = l.list(Some(ProfileKind::Process), true)[0].name.clone();
            assert!(
                l.build_print_config(&PrintConfig::default(), None, None, Some(&name))
                    .is_err()
            );
        }

        let mut bad_area = ProfileLibrary::new();
        bad_area
            .load_json_str(
                r#"{"name": "flat", "type": "machine", "printable_area": ["0x0", "0x0", "0x0"]}"#,
            )
            .unwrap();
        assert!(
            bad_area
                .build_print_config(&PrintConfig::default(), Some("flat"), None, None)
                .is_err()
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_load_file_and_dir_skip_non_profile_json() {
        let tmp = std::env::temp_dir().join(format!(
            "layercraft_profile_dir_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("sub")).unwrap();

        std::fs::write(
            tmp.join("base_proc.json"),
            r#"{"type": "process", "instantiation": false, "layer_height": "0.2"}"#,
        )
        .unwrap();
        std::fs::write(
            tmp.join("sub").join("child.json"),
            r#"{"name": "Child 020", "type": "process", "inherits": "base_proc", "wall_loops": "4"}"#,
        )
        .unwrap();
        std::fs::write(tmp.join("sidecar.json"), r#"{"unrelated": [1, 2, 3]}"#).unwrap();

        let mut lib = ProfileLibrary::new();
        let report = lib.load_dir(&tmp).unwrap();
        assert_eq!(report.loaded.len(), 2);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].path.ends_with("sidecar.json"));

        let res = lib
            .build_print_config(&PrintConfig::default(), None, None, Some("Child 020"))
            .unwrap();
        assert_eq!(res.config.process.wall_loops, 4);
        assert_eq!(res.process_chain, vec!["base_proc", "Child 020"]);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ironing_scarf_and_arc_profile_keys_map_without_warnings() {
        let mut lib = ProfileLibrary::new();
        lib.load_json_str(
            r#"{
                "type": "process",
                "name": "finish_020",
                "layer_height": "0.2",
                "ironing_type": "all_top_surfaces",
                "ironing_flow": "12%",
                "ironing_spacing": "0.12",
                "ironing_inset": "0.20",
                "ironing_speed": "35",
                "ironing_angle": "-1",
                "seam_slope_type": "contour",
                "seam_slope_start_height": "15%",
                "seam_slope_min_length": "8",
                "seam_slope_length": "18",
                "seam_slope_steps": "12",
                "seam_slope_entire_loop": "0",
                "scarf_angle_threshold": "155",
                "seam_slope_conditional": "1",
                "enable_arc_fitting": "1",
                "arc_fitting_tolerance": "0.03"
            }"#,
        )
        .unwrap();

        let res = lib
            .build_print_config(&PrintConfig::default(), None, None, Some("finish_020"))
            .unwrap();
        assert!(res.config.validate().is_ok());
        assert!(res.warnings.is_empty(), "{:?}", res.warnings);
        assert!(res.unmapped_keys.is_empty(), "{:?}", res.unmapped_keys);
        assert_eq!(res.config.process.ironing.mode, IroningType::AllTopSurfaces);
        assert!((res.config.process.ironing.flow_ratio - 0.12).abs() < 1e-9);
        assert!((res.config.process.ironing.line_spacing_mm - 0.12).abs() < 1e-9);
        assert!((res.config.process.ironing.inset_mm - 0.20).abs() < 1e-9);
        assert!((res.config.process.ironing.speed_mm_s - 35.0).abs() < 1e-9);
        assert!((res.config.process.ironing.angle_offset_deg - 45.0).abs() < 1e-9);
        assert!(res.config.process.scarf_seam.enabled);
        assert!((res.config.process.scarf_seam.start_height_ratio - 0.15).abs() < 1e-9);
        assert!((res.config.process.scarf_seam.min_length_mm - 8.0).abs() < 1e-9);
        assert!((res.config.process.scarf_seam.length_mm - 18.0).abs() < 1e-9);
        assert_eq!(res.config.process.scarf_seam.steps, 12);
        assert!(!res.config.process.scarf_seam.entire_loop);
        assert!(res.config.process.arc_fitting);
        assert!((res.config.process.arc_tolerance_mm - 0.03).abs() < 1e-9);
    }

    #[test]
    fn arachne_profile_keys_map_without_warnings() {
        let mut lib = ProfileLibrary::new();
        lib.load_json_str(
            r#"{
                "type": "process",
                "name": "arachne_020",
                "layer_height": "0.2",
                "wall_generator": "arachne",
                "min_bead_width": "85%",
                "min_feature_size": "25%",
                "wall_distribution_count": "2",
                "wall_transition_length": "100%",
                "wall_transition_angle": "12",
                "wall_transition_filter_deviation": "25%",
                "min_length_factor": "1",
                "wall_transition_filter_margin": "25%"
            }"#,
        )
        .unwrap();

        let res = lib
            .build_print_config(&PrintConfig::default(), None, None, Some("arachne_020"))
            .unwrap();
        assert!(res.config.validate().is_ok());
        assert!(res.warnings.is_empty(), "{:?}", res.warnings);
        assert!(res.unmapped_keys.is_empty(), "{:?}", res.unmapped_keys);
        assert_eq!(res.config.process.wall_generator, WallGenerator::Arachne);
        assert!((res.config.process.arachne.min_bead_width_ratio - 0.85).abs() < 1e-9);
        assert!((res.config.process.arachne.min_feature_size_ratio - 0.25).abs() < 1e-9);
        assert_eq!(res.config.process.arachne.wall_distribution_count, 2);
        assert!((res.config.process.arachne.transition_length_ratio - 1.0).abs() < 1e-9);
        assert!((res.config.process.arachne.transition_threshold_angle_deg - 12.0).abs() < 1e-9);
        assert!((res.config.process.arachne.transition_filter_deviation_ratio - 0.25).abs() < 1e-9);
    }

    #[test]
    fn tree_support_profile_keys_map_without_warnings() {
        let mut lib = ProfileLibrary::new();
        lib.load_json_str(
            r#"{
                "type": "process",
                "name": "tree_organic_020",
                "layer_height": "0.2",
                "enable_support": "1",
                "support_style": "organic",
                "support_on_build_plate_only": "1",
                "tree_support_branch_angle": "48",
                "tree_support_tip_diameter": "1.2",
                "tree_support_branch_diameter": "2.4",
                "tree_support_branch_diameter_angle": "6",
                "tree_support_branch_distance": "3.2",
                "tree_support_wall_count": "2",
                "tree_support_brim_width": "1.5",
                "tree_support_auto_brim": "1",
                "tree_support_adaptive_layer_height": "1",
                "tree_support_top_rate": "30%"
            }"#,
        )
        .unwrap();

        let res = lib
            .build_print_config(
                &PrintConfig::default(),
                None,
                None,
                Some("tree_organic_020"),
            )
            .unwrap();
        assert!(res.config.validate().is_ok());
        assert!(res.warnings.is_empty(), "{:?}", res.warnings);
        assert!(res.unmapped_keys.is_empty(), "{:?}", res.unmapped_keys);
        assert!(res.config.process.support.enabled);
        assert_eq!(res.config.process.support.style, SupportStyle::Tree);
        assert!(res.config.process.support.on_build_plate_only);
        assert!((res.config.process.support.tree.max_branch_angle_deg - 48.0).abs() < 1e-9);
        assert!((res.config.process.support.tree.tip_diameter_mm - 1.2).abs() < 1e-9);
        assert!((res.config.process.support.tree.branch_diameter_mm - 2.4).abs() < 1e-9);
        assert!((res.config.process.support.tree.branch_diameter_angle_deg - 6.0).abs() < 1e-9);
        assert!((res.config.process.support.tree.branch_distance_mm - 3.2).abs() < 1e-9);
        assert_eq!(res.config.process.support.tree.branch_wall_loops, 2);
        assert!((res.config.process.support.tree.trunk_base_flare_mm - 1.5).abs() < 1e-9);
    }
}
