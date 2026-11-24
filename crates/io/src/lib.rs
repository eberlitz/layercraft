//! Pure-Rust STL (binary & ASCII) and 3MF readers/writers.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::{Cursor, Read, Write};

use layercraft_geom::{GeomError, Point3, Triangle, TriangleMesh};
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use thiserror::Error;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub mod printer;
pub use printer::{
    DigestChallenge, PrinterAuth, PrinterError, PrinterProfile, PrinterProfileSummary,
    PrinterProtocol, PrinterStatusReport, PrinterStorageInfo, UploadOptions, UploadProgress,
    UploadResult, build_digest_authorization, build_multipart_upload, normalize_base_url,
    parse_digest_challenge, sanitize_remote_path,
};
#[cfg(not(target_arch = "wasm32"))]
pub use printer::{
    default_printers_config_path, load_printer_profiles, printer_credential_storage_mode,
    probe_printer, save_printer_profiles, upload_to_printer, upload_to_printer_with_progress,
};

/// Archive entry path storing LayerCraft project state inside a `.3mf` package.
pub const LAYERCRAFT_PROJECT_PATH: &str = "Metadata/layercraft_project.json";

/// Archive entry path storing plate assignments and slicer object metadata inside a `.3mf` package.
pub const MODEL_SETTINGS_PATH: &str = "Metadata/model_settings.config";

/// Archive entry path storing third-party slicer project process settings JSON inside a `.3mf` package.
pub const PROJECT_SETTINGS_PATH: &str = "Metadata/project_settings.config";

const MAX_3MF_COMPONENT_DEPTH: usize = 32;
const MAX_3MF_ARCHIVE_ENTRIES: usize = 4096;

/// Errors produced when reading or writing 3D mesh files.
#[derive(Debug, Error)]
pub enum IoError {
    #[error(transparent)]
    Geom(#[from] GeomError),
    #[error("malformed STL data: {0}")]
    MalformedStl(String),
    #[error("malformed 3MF archive: {0}")]
    Malformed3mf(String),
    #[cfg(not(target_arch = "wasm32"))]
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Parse either an ASCII or binary STL from raw bytes.
pub fn read_stl_bytes(bytes: &[u8]) -> Result<TriangleMesh, IoError> {
    if bytes.len() < 15 {
        return Err(IoError::MalformedStl("input too short".to_owned()));
    }
    if is_likely_ascii_stl(bytes)
        && let Ok(mesh) = parse_ascii_stl(bytes)
    {
        return Ok(mesh);
    }
    parse_binary_stl(bytes)
}

fn is_likely_ascii_stl(bytes: &[u8]) -> bool {
    let prefix = bytes.get(..5).unwrap_or(&[]);
    if !prefix.eq_ignore_ascii_case(b"solid") {
        return false;
    }
    // Some binary STLs accidentally start with "solid"; confirm "facet" appears in text header.
    let head = bytes.get(..bytes.len().min(512)).unwrap_or(bytes);
    let text = String::from_utf8_lossy(head);
    text.contains("facet") || text.contains("endsolid")
}

fn parse_ascii_stl(bytes: &[u8]) -> Result<TriangleMesh, IoError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| IoError::MalformedStl("invalid UTF-8 in ASCII STL".to_owned()))?;
    let mut tris = Vec::new();
    let mut verts: Vec<Point3> = Vec::with_capacity(3);

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("vertex") {
            let mut coords = rest.split_whitespace();
            let (Some(xs), Some(ys), Some(zs)) = (coords.next(), coords.next(), coords.next())
            else {
                return Err(IoError::MalformedStl("incomplete vertex line".to_owned()));
            };
            let x: f64 = xs
                .parse()
                .map_err(|_| IoError::MalformedStl("invalid float in vertex".to_owned()))?;
            let y: f64 = ys
                .parse()
                .map_err(|_| IoError::MalformedStl("invalid float in vertex".to_owned()))?;
            let z: f64 = zs
                .parse()
                .map_err(|_| IoError::MalformedStl("invalid float in vertex".to_owned()))?;
            verts.push(Point3::new(x, y, z));
            if verts.len() == 3 {
                if let (Some(&a), Some(&b), Some(&c)) = (verts.first(), verts.get(1), verts.get(2))
                {
                    tris.push(Triangle::new(a, b, c));
                }
                verts.clear();
            }
        }
    }

    if !verts.is_empty() {
        return Err(IoError::MalformedStl(
            "dangling vertices in ASCII STL".to_owned(),
        ));
    }
    Ok(TriangleMesh::new(tris)?)
}

fn read_f32_le(slice: &[u8]) -> Result<f64, IoError> {
    let arr: [u8; 4] = slice
        .try_into()
        .map_err(|_| IoError::MalformedStl("unexpected EOF in binary STL".to_owned()))?;
    Ok(f32::from_le_bytes(arr) as f64)
}

fn read_point3_le(slice: &[u8]) -> Result<Point3, IoError> {
    let x = read_f32_le(
        slice
            .get(0..4)
            .ok_or_else(|| IoError::MalformedStl("truncated point X".to_owned()))?,
    )?;
    let y = read_f32_le(
        slice
            .get(4..8)
            .ok_or_else(|| IoError::MalformedStl("truncated point Y".to_owned()))?,
    )?;
    let z = read_f32_le(
        slice
            .get(8..12)
            .ok_or_else(|| IoError::MalformedStl("truncated point Z".to_owned()))?,
    )?;
    Ok(Point3::new(x, y, z))
}

fn parse_binary_stl(bytes: &[u8]) -> Result<TriangleMesh, IoError> {
    if bytes.len() < 84 {
        return Err(IoError::MalformedStl(
            "binary STL shorter than 84-byte header".to_owned(),
        ));
    }
    let count_slice = bytes
        .get(80..84)
        .ok_or_else(|| IoError::MalformedStl("missing triangle count".to_owned()))?;
    let count_arr: [u8; 4] = count_slice
        .try_into()
        .map_err(|_| IoError::MalformedStl("invalid triangle count".to_owned()))?;
    let tri_count = u32::from_le_bytes(count_arr) as usize;
    let expected_len = 84usize
        .checked_add(tri_count.saturating_mul(50))
        .ok_or_else(|| IoError::MalformedStl("triangle count overflow".to_owned()))?;
    if bytes.len() < expected_len {
        return Err(IoError::MalformedStl(format!(
            "binary STL truncated: expected at least {expected_len} bytes, got {}",
            bytes.len()
        )));
    }

    let mut tris = Vec::with_capacity(tri_count);
    for i in 0..tri_count {
        let base = 84 + i * 50;
        let rec = bytes
            .get(base..base + 50)
            .ok_or_else(|| IoError::MalformedStl("truncated triangle record".to_owned()))?;
        // Bytes 0..12 are facet normal; bytes 12..48 are vertices a, b, c.
        let a = read_point3_le(
            rec.get(12..24)
                .ok_or_else(|| IoError::MalformedStl("missing vertex A".to_owned()))?,
        )?;
        let b = read_point3_le(
            rec.get(24..36)
                .ok_or_else(|| IoError::MalformedStl("missing vertex B".to_owned()))?,
        )?;
        let c = read_point3_le(
            rec.get(36..48)
                .ok_or_else(|| IoError::MalformedStl("missing vertex C".to_owned()))?,
        )?;
        tris.push(Triangle::new(a, b, c));
    }

    Ok(TriangleMesh::new(tris)?)
}

/// Encode a [`TriangleMesh`] as binary STL bytes.
pub fn write_stl_binary(mesh: &TriangleMesh) -> Vec<u8> {
    let mut out = Vec::with_capacity(84 + mesh.triangles.len() * 50);
    let mut header = [0u8; 80];
    let tag = b"LayerCraft binary STL";
    if let Some(dst) = header.get_mut(..tag.len()) {
        dst.copy_from_slice(tag);
    }
    out.extend_from_slice(&header);
    let count = u32::try_from(mesh.triangles.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&count.to_le_bytes());

    for tri in &mesh.triangles {
        let n = tri.double_area_vec().normalize_or_zero();
        for v in [
            n.x, n.y, n.z, tri.a.x, tri.a.y, tri.a.z, tri.b.x, tri.b.y, tri.b.z, tri.c.x, tri.c.y,
            tri.c.z,
        ] {
            out.extend_from_slice(&(v as f32).to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    out
}

/// Row-major 4×3 affine matrix (`[m00, m01, m02, m10, m11, m12, m20, m21, m22, m30, m31, m32]`)
/// matching the 3MF Core Specification row-vector convention `v' = [x, y, z, 1] * M`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThreeMfMatrix(pub [f64; 12]);

impl Default for ThreeMfMatrix {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl ThreeMfMatrix {
    /// Identity 4×3 affine transform.
    pub const IDENTITY: Self = Self([
        1.0, 0.0, 0.0, //
        0.0, 1.0, 0.0, //
        0.0, 0.0, 1.0, //
        0.0, 0.0, 0.0, //
    ]);

    /// Parse 12 whitespace-separated finite floats from a 3MF `transform` attribute.
    pub fn parse(s: &str) -> Result<Self, IoError> {
        let mut vals = [0.0_f64; 12];
        let mut count = 0usize;
        for tok in s.split_whitespace() {
            let v: f64 = tok
                .parse()
                .map_err(|_| IoError::Malformed3mf(format!("invalid float in transform: {tok}")))?;
            if !v.is_finite() {
                return Err(IoError::Malformed3mf(
                    "non-finite value in 3MF transform attribute".to_owned(),
                ));
            }
            let slot = vals.get_mut(count).ok_or_else(|| {
                IoError::Malformed3mf("3MF transform attribute has more than 12 numbers".to_owned())
            })?;
            *slot = v;
            count += 1;
        }
        if count != 12 {
            return Err(IoError::Malformed3mf(format!(
                "3MF transform attribute must have 12 numbers, got {count}"
            )));
        }
        Ok(Self(vals))
    }

    /// Format the 12 matrix entries for XML attribute serialization.
    pub fn format_attr(&self) -> String {
        let [m00, m01, m02, m10, m11, m12, m20, m21, m22, m30, m31, m32] = self.0;
        let mut out = String::with_capacity(140);
        for (idx, val) in [m00, m01, m02, m10, m11, m12, m20, m21, m22, m30, m31, m32]
            .into_iter()
            .enumerate()
        {
            if idx > 0 {
                out.push(' ');
            }
            let clean = if val.abs() < 1e-12 { 0.0 } else { val };
            let _ = write!(out, "{clean:.9}");
        }
        out
    }

    /// Scale only the translation component `(m30, m31, m32)` by the model unit factor.
    pub fn with_scaled_translation(self, unit_scale: f64) -> Self {
        let [m00, m01, m02, m10, m11, m12, m20, m21, m22, m30, m31, m32] = self.0;
        Self([
            m00,
            m01,
            m02,
            m10,
            m11,
            m12,
            m20,
            m21,
            m22,
            m30 * unit_scale,
            m31 * unit_scale,
            m32 * unit_scale,
        ])
    }

    /// Compose `self` followed by `parent` (`child * parent` in row-vector convention).
    pub fn then(self, parent: Self) -> Self {
        let [a00, a01, a02, a10, a11, a12, a20, a21, a22, a30, a31, a32] = self.0;
        let [b00, b01, b02, b10, b11, b12, b20, b21, b22, b30, b31, b32] = parent.0;
        Self([
            a00 * b00 + a01 * b10 + a02 * b20,
            a00 * b01 + a01 * b11 + a02 * b21,
            a00 * b02 + a01 * b12 + a02 * b22,
            a10 * b00 + a11 * b10 + a12 * b20,
            a10 * b01 + a11 * b11 + a12 * b21,
            a10 * b02 + a11 * b12 + a12 * b22,
            a20 * b00 + a21 * b10 + a22 * b20,
            a20 * b01 + a21 * b11 + a22 * b21,
            a20 * b02 + a21 * b12 + a22 * b22,
            a30 * b00 + a31 * b10 + a32 * b20 + b30,
            a30 * b01 + a31 * b11 + a32 * b21 + b31,
            a30 * b02 + a31 * b12 + a32 * b22 + b32,
        ])
    }

    /// Transform a 3D point in row-vector convention `v' = [x, y, z, 1] * M`.
    pub fn apply(self, p: Point3) -> Point3 {
        let [m00, m01, m02, m10, m11, m12, m20, m21, m22, m30, m31, m32] = self.0;
        Point3::new(
            p.x * m00 + p.y * m10 + p.z * m20 + m30,
            p.x * m01 + p.y * m11 + p.z * m21 + m31,
            p.x * m02 + p.y * m12 + p.z * m22 + m32,
        )
    }

    /// Determinant of the 3×3 linear submatrix (negative values indicate handedness reflection).
    pub fn determinant(self) -> f64 {
        let [m00, m01, m02, m10, m11, m12, m20, m21, m22, _, _, _] = self.0;
        m00 * (m11 * m22 - m12 * m21) - m01 * (m10 * m22 - m12 * m20)
            + m02 * (m10 * m21 - m11 * m20)
    }
}

/// Logical build plate metadata parsed from `Metadata/model_settings.config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreeMfPlateMeta {
    pub id: u32,
    pub name: String,
}

/// Resolved world-space mesh part instantiated from a 3MF `<build>` item (or fallback model object).
#[derive(Debug, Clone)]
pub struct ThreeMfPart {
    pub name: String,
    pub mesh: TriangleMesh,
    pub item_transform: Option<[f64; 12]>,
    pub object_id: u32,
    pub instance_id: u32,
    pub plate_index: usize,
}

/// Raw resource `<mesh>` object read from a 3MF model file (used for LayerCraft project loading).
#[derive(Debug, Clone)]
pub struct ThreeMfResourceMesh {
    pub id: u32,
    pub name: String,
    pub object_type: String,
    pub mesh: TriangleMesh,
}

/// Single `<build><item .../>` entry for 3MF export.
#[derive(Debug, Clone)]
pub struct ThreeMfBuildItem {
    pub object_id: u32,
    pub transform: Option<[f64; 12]>,
}

/// Logical build plate entry for 3MF `Metadata/model_settings.config` export.
#[derive(Debug, Clone)]
pub struct ThreeMfExportPlate {
    pub id: u32,
    pub name: String,
    /// `(object_id, instance_id)` pairs belonging to this plate.
    pub instances: Vec<(u32, u32)>,
}

/// Role tagged on a sub-part inside third-party 3MF `Metadata/model_settings.config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreeMfPartRole {
    NegativePart,
    Modifier,
    SupportBlocker,
    SupportEnforcer,
}

/// Extracted modifier or negative-volume sub-mesh instantiated from a 3MF archive.
#[derive(Debug, Clone)]
pub struct ThreeMfModifierPart {
    pub name: String,
    pub role: ThreeMfPartRole,
    pub mesh: TriangleMesh,
    pub parent_object_id: u32,
    pub instance_id: u32,
    pub plate_index: usize,
}

/// Full parsed 3MF package: transformed parts, modifier sub-meshes, root resource meshes, plates, and optional project JSON.
#[derive(Debug, Clone)]
pub struct ThreeMfDocument {
    pub parts: Vec<ThreeMfPart>,
    pub modifier_parts: Vec<ThreeMfModifierPart>,
    pub resources: Vec<ThreeMfResourceMesh>,
    pub plates: Vec<ThreeMfPlateMeta>,
    pub project_json: Option<String>,
    pub project_settings_json: Option<String>,
}

/// Complete 3MF package payload to serialize as a standard ZIP container.
#[derive(Debug, Clone)]
pub struct ThreeMfExportDocument {
    pub resources: Vec<ThreeMfResourceMesh>,
    pub build_items: Vec<ThreeMfBuildItem>,
    pub plates: Vec<ThreeMfExportPlate>,
    pub project_json: Option<String>,
}

#[derive(Debug, Clone)]
struct ParsedComponent {
    object_id: u32,
    path: Option<String>,
    transform: ThreeMfMatrix,
}

#[derive(Debug, Clone)]
struct ParsedObject {
    id: u32,
    name: String,
    object_type: String,
    vertices: Vec<Point3>,
    triangles: Vec<(usize, usize, usize)>,
    components: Vec<ParsedComponent>,
}

#[derive(Debug, Clone)]
struct ParsedBuildItem {
    object_id: u32,
    path: Option<String>,
    transform: ThreeMfMatrix,
    has_explicit_transform: bool,
}

#[derive(Debug, Clone)]
struct ParsedModelFile {
    objects: BTreeMap<u32, ParsedObject>,
    build_items: Vec<ParsedBuildItem>,
}

fn normalize_archive_path(raw: &str) -> String {
    let replaced = raw.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for seg in replaced.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                let _ = parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/").to_ascii_lowercase()
}

fn resolve_target_path(base_file_norm: &str, raw_target: &str) -> String {
    let trimmed = raw_target.trim().replace('\\', "/");
    if let Some(abs) = trimmed.strip_prefix('/') {
        normalize_archive_path(abs)
    } else if let Some((dir, _)) = base_file_norm.rsplit_once('/') {
        normalize_archive_path(&format!("{dir}/{trimmed}"))
    } else {
        normalize_archive_path(&trimmed)
    }
}

fn attr_value_utf8(attr: &quick_xml::events::attributes::Attribute<'_>) -> Option<String> {
    attr.normalized_value(XmlVersion::Implicit1_0)
        .ok()
        .map(|s| s.into_owned())
}

fn parse_rels_root_model(xml: &str) -> Result<Option<String>, IoError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.local_name().as_ref() == "Relationship" {
                    let mut rel_type = None;
                    let mut target = None;
                    for attr in e.attributes().flatten() {
                        let Some(val) = attr_value_utf8(&attr) else {
                            continue;
                        };
                        match attr.key.local_name().as_ref() {
                            "Type" => rel_type = Some(val),
                            "Target" => target = Some(val),
                            _ => {}
                        }
                    }
                    if let (Some(rt), Some(tgt)) = (rel_type, target)
                        && rt.to_ascii_lowercase().ends_with("/3dmodel")
                    {
                        return Ok(Some(normalize_archive_path(&tgt)));
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(IoError::Malformed3mf(format!("_rels/.rels XML error: {e}")));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn parse_3mf_model_file(xml: &str, model_path_norm: &str) -> Result<ParsedModelFile, IoError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut unit_scale = 1.0_f64;
    let mut objects: BTreeMap<u32, ParsedObject> = BTreeMap::new();
    let mut build_items: Vec<ParsedBuildItem> = Vec::new();
    let mut current_obj: Option<ParsedObject> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                handle_model_open_tag(
                    e,
                    false,
                    model_path_norm,
                    &mut unit_scale,
                    &mut current_obj,
                    &mut objects,
                    &mut build_items,
                )?;
            }
            Ok(Event::Empty(ref e)) => {
                handle_model_open_tag(
                    e,
                    true,
                    model_path_norm,
                    &mut unit_scale,
                    &mut current_obj,
                    &mut objects,
                    &mut build_items,
                )?;
            }
            Ok(Event::End(ref e)) => {
                if e.local_name().as_ref() == "object"
                    && let Some(obj) = current_obj.take()
                {
                    objects.insert(obj.id, obj);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(IoError::Malformed3mf(format!(
                    "XML parse error in {model_path_norm}: {e}"
                )));
            }
            _ => {}
        }
    }

    if let Some(obj) = current_obj.take() {
        objects.insert(obj.id, obj);
    }

    Ok(ParsedModelFile {
        objects,
        build_items,
    })
}

#[allow(clippy::too_many_arguments)]
fn handle_model_open_tag(
    e: &quick_xml::events::BytesStart<'_>,
    is_empty: bool,
    model_path_norm: &str,
    unit_scale: &mut f64,
    current_obj: &mut Option<ParsedObject>,
    objects: &mut BTreeMap<u32, ParsedObject>,
    build_items: &mut Vec<ParsedBuildItem>,
) -> Result<(), IoError> {
    match e.local_name().as_ref() {
        "model" => {
            for attr in e.attributes().flatten() {
                if attr.key.local_name().as_ref() == "unit"
                    && let Some(val) = attr_value_utf8(&attr)
                {
                    *unit_scale = match val.as_str() {
                        "micron" => 0.001,
                        "millimeter" => 1.0,
                        "centimeter" => 10.0,
                        "inch" => 25.4,
                        "foot" => 304.8,
                        "meter" => 1000.0,
                        _ => 1.0,
                    };
                }
            }
        }
        "object" => {
            if let Some(prev) = current_obj.take() {
                objects.insert(prev.id, prev);
            }
            let mut id: Option<u32> = None;
            let mut name = String::new();
            let mut object_type = String::new();
            for attr in e.attributes().flatten() {
                let Some(val) = attr_value_utf8(&attr) else {
                    continue;
                };
                match attr.key.local_name().as_ref() {
                    "id" => id = val.parse::<u32>().ok(),
                    "name" => name = val,
                    "type" => object_type = val,
                    _ => {}
                }
            }
            let Some(obj_id) = id else {
                return Err(IoError::Malformed3mf(
                    "3MF <object> element missing valid numeric id".to_owned(),
                ));
            };
            let obj = ParsedObject {
                id: obj_id,
                name,
                object_type,
                vertices: Vec::new(),
                triangles: Vec::new(),
                components: Vec::new(),
            };
            if is_empty {
                objects.insert(obj.id, obj);
            } else {
                *current_obj = Some(obj);
            }
        }
        "vertex" => {
            let mut x = None;
            let mut y = None;
            let mut z = None;
            for attr in e.attributes().flatten() {
                let Some(val) = attr_value_utf8(&attr) else {
                    continue;
                };
                match attr.key.local_name().as_ref() {
                    "x" => x = val.parse::<f64>().ok(),
                    "y" => y = val.parse::<f64>().ok(),
                    "z" => z = val.parse::<f64>().ok(),
                    _ => {}
                }
            }
            if let (Some(x), Some(y), Some(z)) = (x, y, z)
                && let Some(obj) = current_obj.as_mut()
            {
                let scale = *unit_scale;
                let pt = Point3::new(x * scale, y * scale, z * scale);
                if !pt.is_finite() {
                    return Err(IoError::Malformed3mf(
                        "non-finite vertex coordinate in 3MF model".to_owned(),
                    ));
                }
                obj.vertices.push(pt);
            }
        }
        "triangle" => {
            let mut v1 = None;
            let mut v2 = None;
            let mut v3 = None;
            for attr in e.attributes().flatten() {
                let Some(val) = attr_value_utf8(&attr) else {
                    continue;
                };
                match attr.key.local_name().as_ref() {
                    "v1" => v1 = val.parse::<usize>().ok(),
                    "v2" => v2 = val.parse::<usize>().ok(),
                    "v3" => v3 = val.parse::<usize>().ok(),
                    _ => {}
                }
            }
            if let (Some(i1), Some(i2), Some(i3)) = (v1, v2, v3)
                && let Some(obj) = current_obj.as_mut()
            {
                if obj.vertices.get(i1).is_none()
                    || obj.vertices.get(i2).is_none()
                    || obj.vertices.get(i3).is_none()
                {
                    return Err(IoError::Malformed3mf(
                        "triangle vertex index out of range".to_owned(),
                    ));
                }
                obj.triangles.push((i1, i2, i3));
            }
        }
        "component" => {
            let mut object_id: Option<u32> = None;
            let mut path: Option<String> = None;
            let mut transform = ThreeMfMatrix::IDENTITY;
            for attr in e.attributes().flatten() {
                let Some(val) = attr_value_utf8(&attr) else {
                    continue;
                };
                match attr.key.local_name().as_ref() {
                    "objectid" => object_id = val.parse::<u32>().ok(),
                    "path" => path = Some(resolve_target_path(model_path_norm, &val)),
                    "transform" => {
                        transform =
                            ThreeMfMatrix::parse(&val)?.with_scaled_translation(*unit_scale);
                    }
                    _ => {}
                }
            }
            let Some(comp_id) = object_id else {
                return Err(IoError::Malformed3mf(
                    "3MF <component> missing valid objectid".to_owned(),
                ));
            };
            if let Some(obj) = current_obj.as_mut() {
                obj.components.push(ParsedComponent {
                    object_id: comp_id,
                    path,
                    transform,
                });
            }
        }
        "item" => {
            let mut object_id: Option<u32> = None;
            let mut path: Option<String> = None;
            let mut transform = ThreeMfMatrix::IDENTITY;
            let mut has_explicit_transform = false;
            for attr in e.attributes().flatten() {
                let Some(val) = attr_value_utf8(&attr) else {
                    continue;
                };
                match attr.key.local_name().as_ref() {
                    "objectid" => object_id = val.parse::<u32>().ok(),
                    "path" => path = Some(resolve_target_path(model_path_norm, &val)),
                    "transform" => {
                        transform =
                            ThreeMfMatrix::parse(&val)?.with_scaled_translation(*unit_scale);
                        has_explicit_transform = true;
                    }
                    _ => {}
                }
            }
            let Some(item_id) = object_id else {
                return Err(IoError::Malformed3mf(
                    "3MF <build><item> missing valid objectid".to_owned(),
                ));
            };
            build_items.push(ParsedBuildItem {
                object_id: item_id,
                path,
                transform,
                has_explicit_transform,
            });
        }
        _ => {}
    }
    Ok(())
}

fn load_parsed_model<'a>(
    archive_files: &HashMap<String, Vec<u8>>,
    cache: &'a mut HashMap<String, ParsedModelFile>,
    model_path_norm: &str,
) -> Result<&'a ParsedModelFile, IoError> {
    if !cache.contains_key(model_path_norm) {
        let raw = archive_files.get(model_path_norm).ok_or_else(|| {
            IoError::Malformed3mf(format!("referenced 3MF model not found: {model_path_norm}"))
        })?;
        let xml = std::str::from_utf8(raw)
            .map_err(|e| IoError::Malformed3mf(format!("UTF-8 error in {model_path_norm}: {e}")))?;
        let parsed = parse_3mf_model_file(xml, model_path_norm)?;
        cache.insert(model_path_norm.to_owned(), parsed);
    }
    cache.get(model_path_norm).ok_or_else(|| {
        IoError::Malformed3mf(format!("failed caching 3MF model: {model_path_norm}"))
    })
}

fn collect_object_triangles(
    archive_files: &HashMap<String, Vec<u8>>,
    cache: &mut HashMap<String, ParsedModelFile>,
    model_path_norm: &str,
    object_id: u32,
    parent_xf: ThreeMfMatrix,
    depth: usize,
    out_tris: &mut Vec<Triangle>,
) -> Result<String, IoError> {
    if depth > MAX_3MF_COMPONENT_DEPTH {
        return Err(IoError::Malformed3mf(
            "3MF component reference depth exceeded (possible cycle)".to_owned(),
        ));
    }

    let obj = {
        let model = load_parsed_model(archive_files, cache, model_path_norm)?;
        model
            .objects
            .get(&object_id)
            .ok_or_else(|| {
                IoError::Malformed3mf(format!(
                    "object id {object_id} not found in {model_path_norm}"
                ))
            })?
            .clone()
    };

    if !obj.triangles.is_empty() {
        let flip_winding = parent_xf.determinant() < 0.0;
        let transformed_verts: Vec<Point3> = obj
            .vertices
            .iter()
            .copied()
            .map(|p| parent_xf.apply(p))
            .collect();
        out_tris.reserve(obj.triangles.len());
        for &(i1, i2, i3) in &obj.triangles {
            let (Some(&a), Some(&b), Some(&c)) = (
                transformed_verts.get(i1),
                transformed_verts.get(i2),
                transformed_verts.get(i3),
            ) else {
                return Err(IoError::Malformed3mf(
                    "triangle vertex index out of range".to_owned(),
                ));
            };
            if flip_winding {
                out_tris.push(Triangle::new(a, c, b));
            } else {
                out_tris.push(Triangle::new(a, b, c));
            }
        }
    }

    for comp in &obj.components {
        let child_model = comp.path.as_deref().unwrap_or(model_path_norm);
        let child_xf = comp.transform.then(parent_xf);
        let _ = collect_object_triangles(
            archive_files,
            cache,
            child_model,
            comp.object_id,
            child_xf,
            depth + 1,
            out_tris,
        )?;
    }

    Ok(obj.name)
}

fn find_root_model_path(archive_files: &HashMap<String, Vec<u8>>) -> Result<String, IoError> {
    if let Some(rels_bytes) = archive_files.get("_rels/.rels")
        && let Ok(rels_xml) = std::str::from_utf8(rels_bytes)
        && let Some(target) = parse_rels_root_model(rels_xml)?
        && archive_files.contains_key(&target)
    {
        return Ok(target);
    }

    if archive_files.contains_key("3d/3dmodel.model") {
        return Ok("3d/3dmodel.model".to_owned());
    }

    let mut candidates: Vec<&str> = archive_files
        .keys()
        .map(String::as_str)
        .filter(|name| name.ends_with(".model"))
        .collect();
    candidates.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    candidates
        .into_iter()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| {
            IoError::Malformed3mf("missing .model XML payload in 3MF archive".to_owned())
        })
}

#[derive(Debug, Clone)]
struct PendingModifierExtract {
    name: String,
    role: ThreeMfPartRole,
    triangles: Vec<Triangle>,
}

struct CollectedObjectParts {
    name: String,
    triangles: Vec<Triangle>,
    modifiers: Vec<PendingModifierExtract>,
}

fn collect_root_object_with_modifiers(
    archive_files: &HashMap<String, Vec<u8>>,
    cache: &mut HashMap<String, ParsedModelFile>,
    model_path_norm: &str,
    object_id: u32,
    parent_xf: ThreeMfMatrix,
    model_settings: Option<&ParsedModelSettings>,
) -> Result<CollectedObjectParts, IoError> {
    let mut out_tris = Vec::new();
    let mut out_mods = Vec::new();
    let obj = {
        let model = load_parsed_model(archive_files, cache, model_path_norm)?;
        model
            .objects
            .get(&object_id)
            .ok_or_else(|| {
                IoError::Malformed3mf(format!(
                    "object id {object_id} not found in {model_path_norm}"
                ))
            })?
            .clone()
    };

    if !obj.triangles.is_empty() {
        let flip_winding = parent_xf.determinant() < 0.0;
        let transformed_verts: Vec<Point3> = obj
            .vertices
            .iter()
            .copied()
            .map(|p| parent_xf.apply(p))
            .collect();
        out_tris.reserve(obj.triangles.len());
        for &(i1, i2, i3) in &obj.triangles {
            let (Some(&a), Some(&b), Some(&c)) = (
                transformed_verts.get(i1),
                transformed_verts.get(i2),
                transformed_verts.get(i3),
            ) else {
                return Err(IoError::Malformed3mf(
                    "triangle vertex index out of range".to_owned(),
                ));
            };
            if flip_winding {
                out_tris.push(Triangle::new(a, c, b));
            } else {
                out_tris.push(Triangle::new(a, b, c));
            }
        }
    }

    for comp in &obj.components {
        let child_model = comp.path.as_deref().unwrap_or(model_path_norm);
        let child_xf = comp.transform.then(parent_xf);
        if let Some(ms) = model_settings
            && let Some(&role) = ms.part_roles.get(&(object_id, comp.object_id))
        {
            let mut mod_tris = Vec::new();
            let comp_name = collect_object_triangles(
                archive_files,
                cache,
                child_model,
                comp.object_id,
                child_xf,
                1,
                &mut mod_tris,
            )?;
            if !mod_tris.is_empty() {
                let fallback_label = match role {
                    ThreeMfPartRole::NegativePart => "Negative part",
                    ThreeMfPartRole::Modifier => "Modifier",
                    ThreeMfPartRole::SupportBlocker => "Support blocker",
                    ThreeMfPartRole::SupportEnforcer => "Support enforcer",
                };
                let name = ms
                    .part_names
                    .get(&(object_id, comp.object_id))
                    .cloned()
                    .filter(|s| !s.is_empty())
                    .or_else(|| (!comp_name.trim().is_empty()).then_some(comp_name))
                    .unwrap_or_else(|| fallback_label.to_owned());
                out_mods.push(PendingModifierExtract {
                    name,
                    role,
                    triangles: mod_tris,
                });
            }
        } else {
            let _ = collect_object_triangles(
                archive_files,
                cache,
                child_model,
                comp.object_id,
                child_xf,
                1,
                &mut out_tris,
            )?;
        }
    }

    Ok(CollectedObjectParts {
        name: obj.name,
        triangles: out_tris,
        modifiers: out_mods,
    })
}

#[derive(Debug, Default)]
struct ParsedModelSettings {
    object_names: HashMap<u32, String>,
    part_roles: HashMap<(u32, u32), ThreeMfPartRole>,
    part_names: HashMap<(u32, u32), String>,
    plates: Vec<ThreeMfPlateMeta>,
    instance_plate: HashMap<(u32, u32), usize>,
}

fn should_skip_3mf_entry(norm_name: &str) -> bool {
    if norm_name.starts_with("auxiliaries/") {
        return true;
    }
    [".png", ".jpg", ".jpeg", ".webp", ".bmp", ".gif", ".ico"]
        .iter()
        .any(|ext| norm_name.ends_with(ext))
}

fn read_metadata_key_value(e: &quick_xml::events::BytesStart<'_>) -> Option<(String, String)> {
    let mut key = None;
    let mut value = None;
    for attr in e.attributes().flatten() {
        let Some(val) = attr_value_utf8(&attr) else {
            continue;
        };
        match attr.key.local_name().as_ref() {
            "key" => key = Some(val),
            "value" => value = Some(val),
            _ => {}
        }
    }
    Some((key?, value.unwrap_or_default()))
}

fn parse_part_attrs(
    e: &quick_xml::events::BytesStart<'_>,
) -> (Option<u32>, Option<ThreeMfPartRole>) {
    let mut part_id = None;
    let mut role = None;
    for attr in e.attributes().flatten() {
        let Some(val) = attr_value_utf8(&attr) else {
            continue;
        };
        match attr.key.local_name().as_ref() {
            "id" => part_id = val.trim().parse::<u32>().ok(),
            "subtype" => {
                role = match val.trim().to_ascii_lowercase().as_str() {
                    "negative_part" => Some(ThreeMfPartRole::NegativePart),
                    "modifier_part" => Some(ThreeMfPartRole::Modifier),
                    "support_blocker" => Some(ThreeMfPartRole::SupportBlocker),
                    "support_enforcer" => Some(ThreeMfPartRole::SupportEnforcer),
                    _ => None,
                };
            }
            _ => {}
        }
    }
    (part_id, role)
}

fn parse_model_settings_config(xml: &str) -> Result<ParsedModelSettings, IoError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out = ParsedModelSettings::default();
    let mut current_object_id: Option<u32> = None;
    let mut current_part_id: Option<u32> = None;
    let mut object_part_depth: usize = 0;
    let mut in_plate = false;
    let mut current_plater_id: Option<u32> = None;
    let mut current_plater_name = String::new();
    let mut current_instances: Vec<(u32, u32)> = Vec::new();
    let mut in_model_instance = false;
    let mut inst_object_id: Option<u32> = None;
    let mut inst_instance_id: Option<u32> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                "object" => {
                    object_part_depth = 0;
                    current_part_id = None;
                    current_object_id = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.local_name().as_ref() == "id")
                        .and_then(|a| attr_value_utf8(&a))
                        .and_then(|v| v.trim().parse::<u32>().ok());
                }
                "part" if current_object_id.is_some() => {
                    object_part_depth += 1;
                    if object_part_depth == 1
                        && let Some(obj_id) = current_object_id
                    {
                        let (pid, role) = parse_part_attrs(e);
                        current_part_id = pid;
                        if let (Some(part_id), Some(part_role)) = (pid, role) {
                            out.part_roles.insert((obj_id, part_id), part_role);
                        }
                    }
                }
                "plate" => {
                    in_plate = true;
                    current_plater_id = None;
                    current_plater_name.clear();
                    current_instances.clear();
                }
                "model_instance" if in_plate => {
                    in_model_instance = true;
                    inst_object_id = None;
                    inst_instance_id = None;
                }
                "metadata" => {
                    if let Some((key, val)) = read_metadata_key_value(e) {
                        apply_model_settings_metadata(
                            &key,
                            val,
                            current_object_id,
                            current_part_id,
                            object_part_depth,
                            in_plate,
                            in_model_instance,
                            &mut current_plater_id,
                            &mut current_plater_name,
                            &mut inst_object_id,
                            &mut inst_instance_id,
                            &mut out.object_names,
                            &out.part_roles,
                            &mut out.part_names,
                        );
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(ref e)) => {
                if e.local_name().as_ref() == "metadata"
                    && let Some((key, val)) = read_metadata_key_value(e)
                {
                    apply_model_settings_metadata(
                        &key,
                        val,
                        current_object_id,
                        current_part_id,
                        object_part_depth,
                        in_plate,
                        in_model_instance,
                        &mut current_plater_id,
                        &mut current_plater_name,
                        &mut inst_object_id,
                        &mut inst_instance_id,
                        &mut out.object_names,
                        &out.part_roles,
                        &mut out.part_names,
                    );
                } else if e.local_name().as_ref() == "part"
                    && let Some(obj_id) = current_object_id
                    && object_part_depth == 0
                {
                    let (pid, role) = parse_part_attrs(e);
                    if let (Some(part_id), Some(part_role)) = (pid, role) {
                        out.part_roles.insert((obj_id, part_id), part_role);
                    }
                } else if e.local_name().as_ref() == "plate" {
                    let id = u32::try_from(out.plates.len() + 1).unwrap_or(u32::MAX);
                    out.plates.push(ThreeMfPlateMeta {
                        id,
                        name: format!("Plate {id}"),
                    });
                }
            }
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                "object" => {
                    current_object_id = None;
                    current_part_id = None;
                    object_part_depth = 0;
                }
                "part" => {
                    object_part_depth = object_part_depth.saturating_sub(1);
                    if object_part_depth == 0 {
                        current_part_id = None;
                    }
                }
                "model_instance" => {
                    if in_model_instance && let Some(obj_id) = inst_object_id.take() {
                        let inst_id = inst_instance_id.take().unwrap_or(0);
                        current_instances.push((obj_id, inst_id));
                    }
                    in_model_instance = false;
                }
                "plate" => {
                    if in_plate
                        && (current_plater_id.is_some()
                            || !current_plater_name.trim().is_empty()
                            || !current_instances.is_empty())
                    {
                        let id = current_plater_id.unwrap_or_else(|| {
                            u32::try_from(out.plates.len() + 1).unwrap_or(u32::MAX)
                        });
                        let trimmed = current_plater_name.trim();
                        let name = if trimmed.is_empty() {
                            format!("Plate {id}")
                        } else {
                            trimmed.to_owned()
                        };
                        let plate_idx = out.plates.len();
                        out.plates.push(ThreeMfPlateMeta { id, name });
                        for (obj_id, inst_id) in current_instances.drain(..) {
                            out.instance_plate
                                .entry((obj_id, inst_id))
                                .or_insert(plate_idx);
                        }
                    }
                    in_plate = false;
                    in_model_instance = false;
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(IoError::Malformed3mf(format!(
                    "XML parse error in {MODEL_SETTINGS_PATH}: {e}"
                )));
            }
            _ => {}
        }
    }

    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn apply_model_settings_metadata(
    key: &str,
    val: String,
    current_object_id: Option<u32>,
    current_part_id: Option<u32>,
    object_part_depth: usize,
    in_plate: bool,
    in_model_instance: bool,
    current_plater_id: &mut Option<u32>,
    current_plater_name: &mut String,
    inst_object_id: &mut Option<u32>,
    inst_instance_id: &mut Option<u32>,
    object_names: &mut HashMap<u32, String>,
    part_roles: &HashMap<(u32, u32), ThreeMfPartRole>,
    part_names: &mut HashMap<(u32, u32), String>,
) {
    if in_model_instance {
        match key {
            "object_id" => *inst_object_id = val.trim().parse::<u32>().ok(),
            "instance_id" => *inst_instance_id = val.trim().parse::<u32>().ok(),
            _ => {}
        }
        return;
    }
    if in_plate {
        match key {
            "plater_id" => *current_plater_id = val.trim().parse::<u32>().ok(),
            "plater_name" => *current_plater_name = val,
            _ => {}
        }
        return;
    }
    if let Some(obj_id) = current_object_id
        && key == "name"
    {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            if object_part_depth == 0 {
                object_names.insert(obj_id, trimmed.to_owned());
            } else if let Some(pid) = current_part_id {
                part_names.insert((obj_id, pid), trimmed.to_owned());
                if !part_roles.contains_key(&(obj_id, pid)) {
                    object_names
                        .entry(obj_id)
                        .or_insert_with(|| trimmed.to_owned());
                }
            } else {
                object_names
                    .entry(obj_id)
                    .or_insert_with(|| trimmed.to_owned());
            }
        }
    }
}

/// Parse a complete 3MF archive into individual transformed parts, resource meshes, plates, and optional project JSON.
pub fn read_3mf_document_bytes(bytes: &[u8]) -> Result<ThreeMfDocument, IoError> {
    let cursor = Cursor::new(bytes);
    let mut archive = ZipArchive::new(cursor)
        .map_err(|e| IoError::Malformed3mf(format!("invalid zip container: {e}")))?;

    if archive.len() > MAX_3MF_ARCHIVE_ENTRIES {
        return Err(IoError::Malformed3mf(
            "3MF archive exceeds maximum allowed entry count".to_owned(),
        ));
    }

    let mut archive_files: HashMap<String, Vec<u8>> = HashMap::with_capacity(archive.len());
    for idx in 0..archive.len() {
        let mut entry = archive
            .by_index(idx)
            .map_err(|e| IoError::Malformed3mf(format!("zip entry error: {e}")))?;
        if entry.is_dir() {
            continue;
        }
        let raw_name: String = entry
            .name()
            .map_err(|e| IoError::Malformed3mf(format!("invalid entry name: {e}")))?
            .into_owned();
        let norm_name = normalize_archive_path(&raw_name);
        if should_skip_3mf_entry(&norm_name) {
            continue;
        }
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|e| IoError::Malformed3mf(format!("zip read error in {raw_name}: {e}")))?;
        archive_files.insert(norm_name, buf);
    }

    let root_path = find_root_model_path(&archive_files)?;
    let mut cache: HashMap<String, ParsedModelFile> = HashMap::new();
    let root_model = load_parsed_model(&archive_files, &mut cache, &root_path)?.clone();

    let model_settings = archive_files
        .get(&normalize_archive_path(MODEL_SETTINGS_PATH))
        .and_then(|raw| std::str::from_utf8(raw).ok())
        .map(parse_model_settings_config)
        .transpose()?;

    let project_json = archive_files
        .get(&normalize_archive_path(LAYERCRAFT_PROJECT_PATH))
        .map(|raw| {
            std::str::from_utf8(raw)
                .map(str::to_owned)
                .map_err(|e| IoError::Malformed3mf(format!("UTF-8 error in project JSON: {e}")))
        })
        .transpose()?;

    let project_settings_json = archive_files
        .get(&normalize_archive_path(PROJECT_SETTINGS_PATH))
        .and_then(|raw| std::str::from_utf8(raw).ok())
        .map(str::to_owned);

    let mut plates = model_settings
        .as_ref()
        .map(|ms| ms.plates.clone())
        .unwrap_or_default();

    let mut resources = Vec::new();
    for obj in root_model.objects.values() {
        if obj.triangles.is_empty() {
            continue;
        }
        let mut tris = Vec::with_capacity(obj.triangles.len());
        for &(i1, i2, i3) in &obj.triangles {
            let (Some(&a), Some(&b), Some(&c)) = (
                obj.vertices.get(i1),
                obj.vertices.get(i2),
                obj.vertices.get(i3),
            ) else {
                return Err(IoError::Malformed3mf(
                    "triangle vertex index out of range".to_owned(),
                ));
            };
            tris.push(Triangle::new(a, b, c));
        }
        if let Ok(mesh) = TriangleMesh::new(tris) {
            let name = if obj.name.trim().is_empty() {
                model_settings
                    .as_ref()
                    .and_then(|ms| ms.object_names.get(&obj.id))
                    .cloned()
                    .unwrap_or_default()
            } else {
                obj.name.clone()
            };
            resources.push(ThreeMfResourceMesh {
                id: obj.id,
                name,
                object_type: if obj.object_type.is_empty() {
                    "model".to_owned()
                } else {
                    obj.object_type.clone()
                },
                mesh,
            });
        }
    }

    let mut parts = Vec::new();
    let mut modifier_parts = Vec::new();
    let mut instance_counts: HashMap<u32, u32> = HashMap::new();
    if !root_model.build_items.is_empty() {
        for (idx, item) in root_model.build_items.iter().enumerate() {
            let inst_counter = instance_counts.entry(item.object_id).or_insert(0);
            let instance_id = *inst_counter;
            *inst_counter += 1;

            let target_model = item.path.as_deref().unwrap_or(&root_path);
            let collected = collect_root_object_with_modifiers(
                &archive_files,
                &mut cache,
                target_model,
                item.object_id,
                item.transform,
                model_settings.as_ref(),
            )?;
            let obj_name = collected.name;
            let tris = collected.triangles;
            let item_mods = collected.modifiers;
            let plate_index = model_settings
                .as_ref()
                .and_then(|ms| {
                    ms.instance_plate
                        .get(&(item.object_id, instance_id))
                        .copied()
                })
                .unwrap_or(0);
            for pm in item_mods {
                if let Ok(mod_mesh) = TriangleMesh::new(pm.triangles) {
                    modifier_parts.push(ThreeMfModifierPart {
                        name: pm.name,
                        role: pm.role,
                        mesh: mod_mesh,
                        parent_object_id: item.object_id,
                        instance_id,
                        plate_index,
                    });
                }
            }
            if tris.is_empty() {
                continue;
            }
            let mesh = TriangleMesh::new(tris)?;
            let config_name = model_settings
                .as_ref()
                .and_then(|ms| ms.object_names.get(&item.object_id))
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            let name = config_name
                .map(str::to_owned)
                .or_else(|| (!obj_name.trim().is_empty()).then_some(obj_name))
                .unwrap_or_else(|| format!("Part {}", idx + 1));
            parts.push(ThreeMfPart {
                name,
                mesh,
                item_transform: item.has_explicit_transform.then_some(item.transform.0),
                object_id: item.object_id,
                instance_id,
                plate_index,
            });
        }
    } else {
        for (idx, obj) in root_model.objects.values().enumerate() {
            let ty = obj.object_type.to_ascii_lowercase();
            if !ty.is_empty() && ty != "model" {
                continue;
            }
            let collected = collect_root_object_with_modifiers(
                &archive_files,
                &mut cache,
                &root_path,
                obj.id,
                ThreeMfMatrix::IDENTITY,
                model_settings.as_ref(),
            )?;
            let obj_name = collected.name;
            let tris = collected.triangles;
            let obj_mods = collected.modifiers;
            let plate_index = model_settings
                .as_ref()
                .and_then(|ms| ms.instance_plate.get(&(obj.id, 0)).copied())
                .unwrap_or(0);
            for pm in obj_mods {
                if let Ok(mod_mesh) = TriangleMesh::new(pm.triangles) {
                    modifier_parts.push(ThreeMfModifierPart {
                        name: pm.name,
                        role: pm.role,
                        mesh: mod_mesh,
                        parent_object_id: obj.id,
                        instance_id: 0,
                        plate_index,
                    });
                }
            }
            if tris.is_empty() {
                continue;
            }
            let mesh = TriangleMesh::new(tris)?;
            let config_name = model_settings
                .as_ref()
                .and_then(|ms| ms.object_names.get(&obj.id))
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            let name = config_name
                .map(str::to_owned)
                .or_else(|| (!obj_name.trim().is_empty()).then_some(obj_name))
                .unwrap_or_else(|| format!("Part {}", idx + 1));
            parts.push(ThreeMfPart {
                name,
                mesh,
                item_transform: None,
                object_id: obj.id,
                instance_id: 0,
                plate_index,
            });
        }
    }

    if plates.is_empty() && !parts.is_empty() {
        plates.push(ThreeMfPlateMeta {
            id: 1,
            name: "Plate 1".to_owned(),
        });
    }

    if parts.is_empty() && project_json.is_none() {
        return Err(IoError::Malformed3mf(
            "3MF archive contains no mesh geometry".to_owned(),
        ));
    }

    Ok(ThreeMfDocument {
        parts,
        modifier_parts,
        resources,
        plates,
        project_json,
        project_settings_json,
    })
}

fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

fn quant_vertex_key(p: Point3) -> Result<(i64, i64, i64), IoError> {
    if !p.is_finite() || p.x.abs() > 1e10 || p.y.abs() > 1e10 || p.z.abs() > 1e10 {
        return Err(IoError::Malformed3mf(
            "3MF export vertex coordinate out of valid range".to_owned(),
        ));
    }
    Ok((
        (p.x * 1_000_000.0).round() as i64,
        (p.y * 1_000_000.0).round() as i64,
        (p.z * 1_000_000.0).round() as i64,
    ))
}

/// Encode a 3MF container (with indexed resources, optional build item transforms, and project JSON) to bytes.
pub fn write_3mf_document_bytes(doc: &ThreeMfExportDocument) -> Result<Vec<u8>, IoError> {
    let cursor = Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(cursor);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    let content_types = if doc.plates.is_empty() {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="model" ContentType="application/vnd.ms-package.3dmanufacturing-3dmodel+xml"/>
  <Default Extension="json" ContentType="application/json"/>
</Types>
"#
    } else {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="model" ContentType="application/vnd.ms-package.3dmanufacturing-3dmodel+xml"/>
  <Default Extension="json" ContentType="application/json"/>
  <Default Extension="config" ContentType="text/xml"/>
</Types>
"#
    };
    zip.start_file("[Content_Types].xml", opts)
        .map_err(|e| IoError::Malformed3mf(format!("zip write error: {e}")))?;
    zip.write_all(content_types.as_bytes())
        .map_err(|e| IoError::Malformed3mf(format!("zip write error: {e}")))?;

    let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Target="/3D/3dmodel.model" Id="rel0" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/>
</Relationships>
"#;
    zip.start_file("_rels/.rels", opts)
        .map_err(|e| IoError::Malformed3mf(format!("zip write error: {e}")))?;
    zip.write_all(rels.as_bytes())
        .map_err(|e| IoError::Malformed3mf(format!("zip write error: {e}")))?;

    let mut model_xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <model unit=\"millimeter\" xml:lang=\"en-US\" xmlns=\"http://schemas.microsoft.com/3dmanufacturing/core/2015/02\">\n\
         \x20 <metadata name=\"Application\">LayerCraft</metadata>\n\
         \x20 <resources>\n",
    );

    for res in &doc.resources {
        let obj_type = if res.object_type.trim().is_empty() {
            "model"
        } else {
            res.object_type.trim()
        };
        let escaped_type = xml_escape(obj_type);
        let escaped_name = xml_escape(&res.name);
        if escaped_name.is_empty() {
            let _ = writeln!(
                model_xml,
                "    <object id=\"{}\" type=\"{escaped_type}\">",
                res.id
            );
        } else {
            let _ = writeln!(
                model_xml,
                "    <object id=\"{}\" type=\"{escaped_type}\" name=\"{escaped_name}\">",
                res.id
            );
        }

        let mut vert_map: HashMap<(i64, i64, i64), usize> = HashMap::new();
        let mut verts: Vec<Point3> = Vec::new();
        let mut tri_indices: Vec<(usize, usize, usize)> =
            Vec::with_capacity(res.mesh.triangles.len());
        let mut intern = |p: Point3| -> Result<usize, IoError> {
            let key = quant_vertex_key(p)?;
            if let Some(&idx) = vert_map.get(&key) {
                Ok(idx)
            } else {
                let idx = verts.len();
                verts.push(p);
                vert_map.insert(key, idx);
                Ok(idx)
            }
        };

        for tri in &res.mesh.triangles {
            let i1 = intern(tri.a)?;
            let i2 = intern(tri.b)?;
            let i3 = intern(tri.c)?;
            tri_indices.push((i1, i2, i3));
        }

        model_xml.push_str("      <mesh>\n        <vertices>\n");
        for v in &verts {
            let _ = writeln!(
                model_xml,
                "          <vertex x=\"{:.7}\" y=\"{:.7}\" z=\"{:.7}\"/>",
                v.x, v.y, v.z
            );
        }
        model_xml.push_str("        </vertices>\n        <triangles>\n");
        for (i1, i2, i3) in tri_indices {
            let _ = writeln!(
                model_xml,
                "          <triangle v1=\"{i1}\" v2=\"{i2}\" v3=\"{i3}\"/>"
            );
        }
        model_xml.push_str("        </triangles>\n      </mesh>\n    </object>\n");
    }

    model_xml.push_str("  </resources>\n  <build>\n");
    for item in &doc.build_items {
        if let Some(mat) = item.transform {
            let xf_str = ThreeMfMatrix(mat).format_attr();
            let _ = writeln!(
                model_xml,
                "    <item objectid=\"{}\" transform=\"{xf_str}\"/>",
                item.object_id
            );
        } else {
            let _ = writeln!(model_xml, "    <item objectid=\"{}\"/>", item.object_id);
        }
    }
    model_xml.push_str("  </build>\n</model>\n");

    zip.start_file("3D/3dmodel.model", opts)
        .map_err(|e| IoError::Malformed3mf(format!("zip write error: {e}")))?;
    zip.write_all(model_xml.as_bytes())
        .map_err(|e| IoError::Malformed3mf(format!("zip write error: {e}")))?;

    if let Some(project_json) = &doc.project_json {
        zip.start_file(LAYERCRAFT_PROJECT_PATH, opts)
            .map_err(|e| IoError::Malformed3mf(format!("zip write error: {e}")))?;
        zip.write_all(project_json.as_bytes())
            .map_err(|e| IoError::Malformed3mf(format!("zip write error: {e}")))?;
    }

    if !doc.plates.is_empty() {
        let mut settings_xml =
            String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<config>\n");
        for res in &doc.resources {
            let trimmed = res.name.trim();
            if trimmed.is_empty() {
                continue;
            }
            let escaped = xml_escape(trimmed);
            let _ = writeln!(
                settings_xml,
                "  <object id=\"{}\">\n    <metadata key=\"name\" value=\"{escaped}\"/>\n  </object>",
                res.id
            );
        }
        for plate in &doc.plates {
            let escaped_name = xml_escape(&plate.name);
            let _ = writeln!(
                settings_xml,
                "  <plate>\n    <metadata key=\"plater_id\" value=\"{}\"/>\n    <metadata key=\"plater_name\" value=\"{escaped_name}\"/>",
                plate.id
            );
            for &(object_id, instance_id) in &plate.instances {
                let _ = writeln!(
                    settings_xml,
                    "    <model_instance>\n      <metadata key=\"object_id\" value=\"{object_id}\"/>\n      <metadata key=\"instance_id\" value=\"{instance_id}\"/>\n    </model_instance>"
                );
            }
            settings_xml.push_str("  </plate>\n");
        }
        settings_xml.push_str("</config>\n");
        zip.start_file(MODEL_SETTINGS_PATH, opts)
            .map_err(|e| IoError::Malformed3mf(format!("zip write error: {e}")))?;
        zip.write_all(settings_xml.as_bytes())
            .map_err(|e| IoError::Malformed3mf(format!("zip write error: {e}")))?;
    }

    let cursor = zip
        .finish()
        .map_err(|e| IoError::Malformed3mf(format!("zip finish error: {e}")))?;
    Ok(cursor.into_inner())
}

/// Parse a 3MF archive from raw bytes, resolving `<build>` transforms & components and merging all parts in millimeters.
pub fn read_3mf_bytes(bytes: &[u8]) -> Result<TriangleMesh, IoError> {
    let doc = read_3mf_document_bytes(bytes)?;
    let meshes: Vec<TriangleMesh> = doc.parts.into_iter().map(|p| p.mesh).collect();
    if meshes.is_empty() {
        return Err(IoError::Malformed3mf(
            "3MF archive contains no mesh geometry".to_owned(),
        ));
    }
    Ok(TriangleMesh::merge(&meshes)?)
}

/// Read a full 3MF document (parts, resources, project manifest) from disk (native targets only).
#[cfg(not(target_arch = "wasm32"))]
pub fn read_3mf_document_file(path: &std::path::Path) -> Result<ThreeMfDocument, IoError> {
    let bytes = std::fs::read(path)?;
    read_3mf_document_bytes(&bytes)
}

/// Write a 3MF project/export archive to disk (native targets only).
#[cfg(not(target_arch = "wasm32"))]
pub fn write_3mf_document_file(
    path: &std::path::Path,
    doc: &ThreeMfExportDocument,
) -> Result<(), IoError> {
    let bytes = write_3mf_document_bytes(doc)?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Read an STL or 3MF mesh from disk (native targets only).
#[cfg(not(target_arch = "wasm32"))]
pub fn read_mesh_file(path: &std::path::Path) -> Result<TriangleMesh, IoError> {
    let bytes = std::fs::read(path)?;
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if ext == "3mf" {
        read_3mf_bytes(&bytes)
    } else {
        read_stl_bytes(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_cube_mesh(size: f64) -> TriangleMesh {
        let s = size;
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
        TriangleMesh::new(tris).unwrap()
    }

    fn make_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, body) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn stl_binary_and_ascii_roundtrip() {
        let ascii = b"solid tri\n  facet normal 0 0 1\n    outer loop\n      vertex 0 0 0\n      vertex 10 0 0\n      vertex 0 10 0\n    endloop\n  endfacet\nendsolid tri\n";
        let mesh = read_stl_bytes(ascii).unwrap();
        assert_eq!(mesh.triangles.len(), 1);

        let bin = write_stl_binary(&mesh);
        let mesh2 = read_stl_bytes(&bin).unwrap();
        assert_eq!(mesh2.triangles.len(), 1);
        assert!((mesh2.triangles[0].area_mm2() - 50.0).abs() < 1e-4);
    }

    #[test]
    fn hostile_stl_bytes_never_panic() {
        assert!(read_stl_bytes(&[]).is_err());
        assert!(read_stl_bytes(&[0u8; 84]).is_err());
        let mut bad_count = vec![0u8; 84];
        bad_count[80..84].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_stl_bytes(&bad_count).is_err());
        assert!(read_3mf_bytes(b"not a zip").is_err());
        assert!(read_3mf_document_bytes(b"not a zip").is_err());
    }

    #[test]
    fn three_mf_multipart_transforms_and_external_object_paths() {
        // Put 3D/Objects/sub.model BEFORE 3D/main.model in ZIP order to verify _rels/.rels discovery.
        let sub_model = r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="centimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
  <resources>
    <object id="7" type="model" name="SubCube">
      <mesh>
        <vertices>
          <vertex x="0" y="0" z="0"/><vertex x="1" y="0" z="0"/>
          <vertex x="1" y="1" z="0"/><vertex x="0" y="1" z="0"/>
          <vertex x="0" y="0" z="1"/><vertex x="1" y="0" z="1"/>
          <vertex x="1" y="1" z="1"/><vertex x="0" y="1" z="1"/>
        </vertices>
        <triangles>
          <triangle v1="0" v2="3" v3="2"/><triangle v1="0" v2="2" v3="1"/>
          <triangle v1="4" v2="5" v3="6"/><triangle v1="4" v2="6" v3="7"/>
          <triangle v1="0" v2="1" v3="5"/><triangle v1="0" v2="5" v3="4"/>
          <triangle v1="3" v2="7" v3="6"/><triangle v1="3" v2="6" v3="2"/>
          <triangle v1="0" v2="4" v3="7"/><triangle v1="0" v2="7" v3="3"/>
          <triangle v1="1" v2="2" v3="6"/><triangle v1="1" v2="6" v3="5"/>
        </triangles>
      </mesh>
    </object>
  </resources>
</model>"#;

        let main_model = r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter"
       xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02"
       xmlns:p="http://schemas.microsoft.com/3dmanufacturing/production/2015/06">
  <resources>
    <object id="1" type="model" name="Left Block">
      <mesh>
        <vertices>
          <vertex x="0" y="0" z="0"/><vertex x="20" y="0" z="0"/>
          <vertex x="20" y="20" z="0"/><vertex x="0" y="20" z="0"/>
          <vertex x="0" y="0" z="10"/><vertex x="20" y="0" z="10"/>
          <vertex x="20" y="20" z="10"/><vertex x="0" y="20" z="10"/>
        </vertices>
        <triangles>
          <triangle v1="0" v2="3" v3="2"/><triangle v1="0" v2="2" v3="1"/>
          <triangle v1="4" v2="5" v3="6"/><triangle v1="4" v2="6" v3="7"/>
          <triangle v1="0" v2="1" v3="5"/><triangle v1="0" v2="5" v3="4"/>
          <triangle v1="3" v2="7" v3="6"/><triangle v1="3" v2="6" v3="2"/>
          <triangle v1="0" v2="4" v3="7"/><triangle v1="0" v2="7" v3="3"/>
          <triangle v1="1" v2="2" v3="6"/><triangle v1="1" v2="6" v3="5"/>
        </triangles>
      </mesh>
    </object>
    <object id="2" type="model" name="Right Assembly">
      <components>
        <component p:path="/3D/Objects/sub.model" objectid="7" transform="2 0 0  0 1 0  0 0 1  5 0 0"/>
      </components>
    </object>
  </resources>
  <build>
    <item objectid="1" transform="1 0 0  0 1 0  0 0 1  10 20 0"/>
    <item objectid="2" transform="1 0 0  0 1 0  0 0 1  50 20 0"/>
  </build>
</model>"#;

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Target="/3D/main.model" Id="rel0" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/>
</Relationships>"#;

        let bytes = make_zip(&[
            ("3D/Objects/sub.model", sub_model),
            ("_rels/.rels", rels),
            ("3D/main.model", main_model),
        ]);

        let doc = read_3mf_document_bytes(&bytes).unwrap();
        assert_eq!(doc.parts.len(), 2);
        assert_eq!(doc.parts[0].name, "Left Block");
        let bb0 = doc.parts[0].mesh.aabb().unwrap();
        assert!((bb0.min.x - 10.0).abs() < 1e-6);
        assert!((bb0.max.x - 30.0).abs() < 1e-6);
        assert!((bb0.min.y - 20.0).abs() < 1e-6);

        assert_eq!(doc.parts[1].name, "Right Assembly");
        let bb1 = doc.parts[1].mesh.aabb().unwrap();
        // SubCube is 1cm = 10mm cube; component scales X by 2 (0..20) and shifts X +5 (5..25), build item shifts X +50 -> 55..75.
        assert!((bb1.min.x - 55.0).abs() < 1e-6);
        assert!((bb1.max.x - 75.0).abs() < 1e-6);
        assert!((bb1.min.y - 20.0).abs() < 1e-6);
        assert!((bb1.max.z - 10.0).abs() < 1e-6);

        let merged = read_3mf_bytes(&bytes).unwrap();
        assert_eq!(merged.triangles.len(), 24);
    }

    #[test]
    fn three_mf_negative_determinant_flips_winding() {
        let model = r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
  <resources>
    <object id="1" type="model" name="Mirrored">
      <mesh>
        <vertices>
          <vertex x="0" y="0" z="0"/><vertex x="10" y="0" z="0"/>
          <vertex x="10" y="10" z="0"/><vertex x="0" y="10" z="0"/>
          <vertex x="0" y="0" z="10"/><vertex x="10" y="0" z="10"/>
          <vertex x="10" y="10" z="10"/><vertex x="0" y="10" z="10"/>
        </vertices>
        <triangles>
          <triangle v1="0" v2="3" v3="2"/><triangle v1="0" v2="2" v3="1"/>
          <triangle v1="4" v2="5" v3="6"/><triangle v1="4" v2="6" v3="7"/>
          <triangle v1="0" v2="1" v3="5"/><triangle v1="0" v2="5" v3="4"/>
          <triangle v1="3" v2="7" v3="6"/><triangle v1="3" v2="6" v3="2"/>
          <triangle v1="0" v2="4" v3="7"/><triangle v1="0" v2="7" v3="3"/>
          <triangle v1="1" v2="2" v3="6"/><triangle v1="1" v2="6" v3="5"/>
        </triangles>
      </mesh>
    </object>
  </resources>
  <build>
    <item objectid="1" transform="-1 0 0  0 1 0  0 0 1  20 0 0"/>
  </build>
</model>"#;
        let bytes = make_zip(&[("3D/3dmodel.model", model)]);
        let doc = read_3mf_document_bytes(&bytes).unwrap();
        assert_eq!(doc.parts.len(), 1);
        assert!(
            doc.parts[0].mesh.signed_volume_mm3() > 999.0,
            "expected positive outward volume, got {}",
            doc.parts[0].mesh.signed_volume_mm3()
        );
    }

    #[test]
    fn three_mf_cyclic_components_rejected_without_panic() {
        let model = r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
  <resources>
    <object id="1" type="model"><components><component objectid="2"/></components></object>
    <object id="2" type="model"><components><component objectid="1"/></components></object>
  </resources>
  <build><item objectid="1"/></build>
</model>"#;
        let bytes = make_zip(&[("3D/3dmodel.model", model)]);
        assert!(matches!(
            read_3mf_document_bytes(&bytes),
            Err(IoError::Malformed3mf(_))
        ));
    }

    #[test]
    fn three_mf_export_and_reload_roundtrip() {
        let c1 = unit_cube_mesh(10.0);
        let c2 = unit_cube_mesh(12.0);
        let mod_mesh = unit_cube_mesh(8.0);
        let doc_out = ThreeMfExportDocument {
            resources: vec![
                ThreeMfResourceMesh {
                    id: 1,
                    name: "Block & <One>".to_owned(),
                    object_type: "model".to_owned(),
                    mesh: c1,
                },
                ThreeMfResourceMesh {
                    id: 2,
                    name: "Block Two".to_owned(),
                    object_type: "model".to_owned(),
                    mesh: c2,
                },
                ThreeMfResourceMesh {
                    id: 3,
                    name: "Custom Mod".to_owned(),
                    object_type: "other".to_owned(),
                    mesh: mod_mesh,
                },
            ],
            build_items: vec![
                ThreeMfBuildItem {
                    object_id: 1,
                    transform: Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 40.0, 50.0, 0.0]),
                },
                ThreeMfBuildItem {
                    object_id: 2,
                    transform: Some([
                        1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 120.0, 50.0, 0.0,
                    ]),
                },
            ],
            plates: Vec::new(),
            project_json: Some("{\"format_version\":1}".to_owned()),
        };

        let bytes = write_3mf_document_bytes(&doc_out).unwrap();
        let doc_in = read_3mf_document_bytes(&bytes).unwrap();
        assert_eq!(doc_in.parts.len(), 2);
        assert_eq!(doc_in.resources.len(), 3);
        assert_eq!(doc_in.plates.len(), 1);
        assert_eq!(doc_in.plates[0].name, "Plate 1");
        assert_eq!(doc_in.parts[0].name, "Block & <One>");
        assert_eq!(doc_in.resources[2].object_type, "other");
        assert_eq!(
            doc_in.project_json.as_deref(),
            Some("{\"format_version\":1}")
        );
        let bb0 = doc_in.parts[0].mesh.aabb().unwrap();
        assert!((bb0.min.x - 40.0).abs() < 1e-5);
        assert!((bb0.min.y - 50.0).abs() < 1e-5);
    }

    #[test]
    fn three_mf_multiplate_model_settings_parses_plates_and_object_names() {
        let model = r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
  <resources>
    <object id="2" type="model">
      <mesh>
        <vertices>
          <vertex x="0" y="0" z="0"/><vertex x="10" y="0" z="0"/>
          <vertex x="10" y="10" z="0"/><vertex x="0" y="10" z="0"/>
          <vertex x="0" y="0" z="10"/><vertex x="10" y="0" z="10"/>
          <vertex x="10" y="10" z="10"/><vertex x="0" y="10" z="10"/>
        </vertices>
        <triangles>
          <triangle v1="0" v2="2" v3="1"/><triangle v1="0" v2="3" v3="2"/>
          <triangle v1="4" v2="5" v3="6"/><triangle v1="4" v2="6" v3="7"/>
          <triangle v1="0" v2="1" v3="5"/><triangle v1="0" v2="5" v3="4"/>
          <triangle v1="1" v2="2" v3="6"/><triangle v1="1" v2="6" v3="5"/>
          <triangle v1="2" v2="3" v3="7"/><triangle v1="2" v2="7" v3="6"/>
          <triangle v1="3" v2="0" v3="4"/><triangle v1="3" v2="4" v3="7"/>
        </triangles>
      </mesh>
    </object>
    <object id="9" type="model">
      <mesh>
        <vertices>
          <vertex x="0" y="0" z="0"/><vertex x="12" y="0" z="0"/>
          <vertex x="12" y="12" z="0"/><vertex x="0" y="12" z="0"/>
          <vertex x="0" y="0" z="12"/><vertex x="12" y="0" z="12"/>
          <vertex x="12" y="12" z="12"/><vertex x="0" y="12" z="12"/>
        </vertices>
        <triangles>
          <triangle v1="0" v2="2" v3="1"/><triangle v1="0" v2="3" v3="2"/>
          <triangle v1="4" v2="5" v3="6"/><triangle v1="4" v2="6" v3="7"/>
          <triangle v1="0" v2="1" v3="5"/><triangle v1="0" v2="5" v3="4"/>
          <triangle v1="1" v2="2" v3="6"/><triangle v1="1" v2="6" v3="5"/>
          <triangle v1="2" v2="3" v3="7"/><triangle v1="2" v2="7" v3="6"/>
          <triangle v1="3" v2="0" v3="4"/><triangle v1="3" v2="4" v3="7"/>
        </triangles>
      </mesh>
    </object>
  </resources>
  <build>
    <item objectid="2" transform="1 0 0 0 1 0 0 0 1 125 125 0"/>
    <item objectid="9" transform="1 0 0 0 1 0 0 0 1 435 125 0"/>
    <item objectid="9" transform="1 0 0 0 1 0 0 0 1 435 -180 0"/>
  </build>
</model>"#;
        let settings = r#"<?xml version="1.0" encoding="UTF-8"?>
<config>
  <object id="2">
    <metadata key="name" value="01_Base.stl"/>
    <part id="1"><metadata key="name" value="inner_part"/></part>
  </object>
  <object id="9">
    <metadata key="name" value="04_Bolt.stl"/>
  </object>
  <plate>
    <metadata key="plater_id" value="1"/>
    <metadata key="plater_name" value="Base Plate"/>
    <model_instance>
      <metadata key="object_id" value="2"/>
      <metadata key="instance_id" value="0"/>
    </model_instance>
  </plate>
  <plate>
    <metadata key="plater_id" value="2"/>
    <metadata key="plater_name" value=""/>
    <model_instance>
      <metadata key="object_id" value="9"/>
      <metadata key="instance_id" value="0"/>
    </model_instance>
    <model_instance>
      <metadata key="object_id" value="9"/>
      <metadata key="instance_id" value="1"/>
    </model_instance>
  </plate>
</config>"#;
        let bytes = make_zip(&[
            ("3D/3dmodel.model", model),
            ("Metadata/model_settings.config", settings),
            ("Auxiliaries/thumb.png", "not_actually_parsed"),
        ]);
        let doc = read_3mf_document_bytes(&bytes).unwrap();
        assert_eq!(doc.plates.len(), 2);
        assert_eq!(doc.plates[0].name, "Base Plate");
        assert_eq!(doc.plates[1].name, "Plate 2");
        assert_eq!(doc.parts.len(), 3);
        assert_eq!(doc.parts[0].name, "01_Base.stl");
        assert_eq!(doc.parts[0].plate_index, 0);
        assert_eq!(doc.parts[1].name, "04_Bolt.stl");
        assert_eq!(doc.parts[1].instance_id, 0);
        assert_eq!(doc.parts[1].plate_index, 1);
        assert_eq!(doc.parts[2].name, "04_Bolt.stl");
        assert_eq!(doc.parts[2].instance_id, 1);
        assert_eq!(doc.parts[2].plate_index, 1);

        // Also test multi-plate export & reload roundtrip.
        let export = ThreeMfExportDocument {
            resources: vec![
                ThreeMfResourceMesh {
                    id: 2,
                    name: doc.parts[0].name.clone(),
                    object_type: "model".to_owned(),
                    mesh: unit_cube_mesh(10.0),
                },
                ThreeMfResourceMesh {
                    id: 9,
                    name: doc.parts[1].name.clone(),
                    object_type: "model".to_owned(),
                    mesh: unit_cube_mesh(12.0),
                },
            ],
            build_items: vec![
                ThreeMfBuildItem {
                    object_id: 2,
                    transform: None,
                },
                ThreeMfBuildItem {
                    object_id: 9,
                    transform: None,
                },
            ],
            plates: vec![
                ThreeMfExportPlate {
                    id: 1,
                    name: "Base Plate".to_owned(),
                    instances: vec![(2, 0)],
                },
                ThreeMfExportPlate {
                    id: 2,
                    name: "Bolts".to_owned(),
                    instances: vec![(9, 0)],
                },
            ],
            project_json: None,
        };
        let re_bytes = write_3mf_document_bytes(&export).unwrap();
        let re_doc = read_3mf_document_bytes(&re_bytes).unwrap();
        assert_eq!(re_doc.plates.len(), 2);
        assert_eq!(re_doc.plates[0].name, "Base Plate");
        assert_eq!(re_doc.plates[1].name, "Bolts");
        assert_eq!(re_doc.parts[0].plate_index, 0);
        assert_eq!(re_doc.parts[1].plate_index, 1);
    }

    #[test]
    fn read_3mf_extracts_negative_part_and_project_settings() {
        let model = r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
  <resources>
    <object id="2" type="model" name="Body">
      <mesh>
        <vertices>
          <vertex x="0" y="0" z="0"/><vertex x="20" y="0" z="0"/>
          <vertex x="20" y="20" z="0"/><vertex x="0" y="20" z="0"/>
          <vertex x="0" y="0" z="10"/><vertex x="20" y="0" z="10"/>
          <vertex x="20" y="20" z="10"/><vertex x="0" y="20" z="10"/>
        </vertices>
        <triangles>
          <triangle v1="0" v2="2" v3="1"/><triangle v1="0" v2="3" v3="2"/>
          <triangle v1="4" v2="5" v3="6"/><triangle v1="4" v2="6" v3="7"/>
          <triangle v1="0" v2="1" v3="5"/><triangle v1="0" v2="5" v3="4"/>
          <triangle v1="1" v2="2" v3="6"/><triangle v1="1" v2="6" v3="5"/>
          <triangle v1="2" v2="3" v3="7"/><triangle v1="2" v2="7" v3="6"/>
          <triangle v1="3" v2="0" v3="4"/><triangle v1="3" v2="4" v3="7"/>
        </triangles>
      </mesh>
    </object>
    <object id="3" type="model" name="NegCutter">
      <mesh>
        <vertices>
          <vertex x="7" y="7" z="-1"/><vertex x="13" y="7" z="-1"/>
          <vertex x="13" y="13" z="-1"/><vertex x="7" y="13" z="-1"/>
          <vertex x="7" y="7" z="11"/><vertex x="13" y="7" z="11"/>
          <vertex x="13" y="13" z="11"/><vertex x="7" y="13" z="11"/>
        </vertices>
        <triangles>
          <triangle v1="0" v2="2" v3="1"/><triangle v1="0" v2="3" v3="2"/>
          <triangle v1="4" v2="5" v3="6"/><triangle v1="4" v2="6" v3="7"/>
          <triangle v1="0" v2="1" v3="5"/><triangle v1="0" v2="5" v3="4"/>
          <triangle v1="1" v2="2" v3="6"/><triangle v1="1" v2="6" v3="5"/>
          <triangle v1="2" v2="3" v3="7"/><triangle v1="2" v2="7" v3="6"/>
          <triangle v1="3" v2="0" v3="4"/><triangle v1="3" v2="4" v3="7"/>
        </triangles>
      </mesh>
    </object>
    <object id="1" type="model" name="MountedBlock">
      <components>
        <component objectid="2"/>
        <component objectid="3"/>
      </components>
    </object>
  </resources>
  <build>
    <item objectid="1" transform="1 0 0 0 1 0 0 0 1 100 100 0"/>
  </build>
</model>"#;
        let model_settings = r#"<?xml version="1.0" encoding="UTF-8"?>
<config>
  <object id="1">
    <metadata key="name" value="MountedBlock"/>
    <part id="2" subtype="normal_part"><metadata key="name" value="Body"/></part>
    <part id="3" subtype="negative_part"><metadata key="name" value="CenterVoid"/></part>
  </object>
</config>"#;
        let project_settings = r#"{"layer_height": "0.16", "wall_loops": "4"}"#;

        let bytes = make_zip(&[
            ("3D/3dmodel.model", model),
            ("Metadata/model_settings.config", model_settings),
            ("Metadata/project_settings.config", project_settings),
        ]);
        let doc = read_3mf_document_bytes(&bytes).unwrap();
        assert_eq!(doc.parts.len(), 1);
        assert_eq!(doc.parts[0].mesh.triangles.len(), 12);
        assert_eq!(doc.modifier_parts.len(), 1);
        assert_eq!(doc.modifier_parts[0].role, ThreeMfPartRole::NegativePart);
        assert_eq!(doc.modifier_parts[0].name, "CenterVoid");
        assert!(doc.project_settings_json.is_some());
    }
}
