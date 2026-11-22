//! Pure-Rust STL (binary & ASCII) and 3MF readers/writers.

use std::io::{Cursor, Read};

use layercraft_geom::{GeomError, Point3, Triangle, TriangleMesh};
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use thiserror::Error;
use zip::ZipArchive;

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

/// Parse a 3MF archive from raw bytes, merging all `<mesh>` objects scaled to millimeters.
pub fn read_3mf_bytes(bytes: &[u8]) -> Result<TriangleMesh, IoError> {
    let cursor = Cursor::new(bytes);
    let mut archive = ZipArchive::new(cursor)
        .map_err(|e| IoError::Malformed3mf(format!("invalid zip container: {e}")))?;

    let mut model_xml = String::new();
    let mut found = false;
    for idx in 0..archive.len() {
        let mut entry = archive
            .by_index(idx)
            .map_err(|e| IoError::Malformed3mf(format!("zip entry error: {e}")))?;
        let name = entry
            .name()
            .map_err(|e| IoError::Malformed3mf(format!("invalid entry name: {e}")))?
            .to_ascii_lowercase();
        if name.ends_with(".model") {
            entry
                .read_to_string(&mut model_xml)
                .map_err(|e| IoError::Malformed3mf(format!("UTF-8 error in {name}: {e}")))?;
            found = true;
            break;
        }
    }
    if !found {
        return Err(IoError::Malformed3mf(
            "missing .model XML payload in 3MF archive".to_owned(),
        ));
    }

    parse_3mf_model_xml(&model_xml)
}

fn parse_3mf_model_xml(xml: &str) -> Result<TriangleMesh, IoError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut unit_scale = 1.0f64;
    let mut vertices: Vec<Point3> = Vec::new();
    let mut all_tris: Vec<Triangle> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    "model" => {
                        for attr in e.attributes().flatten() {
                            if attr.key.local_name().as_ref() == "unit"
                                && let Ok(val) = attr.normalized_value(XmlVersion::Implicit1_0)
                            {
                                unit_scale = match val.as_ref() {
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
                    "mesh" => {
                        vertices.clear();
                    }
                    "vertex" => {
                        let mut x = None;
                        let mut y = None;
                        let mut z = None;
                        for attr in e.attributes().flatten() {
                            let Ok(val) = attr.normalized_value(XmlVersion::Implicit1_0) else {
                                continue;
                            };
                            match attr.key.local_name().as_ref() {
                                "x" => x = val.parse::<f64>().ok(),
                                "y" => y = val.parse::<f64>().ok(),
                                "z" => z = val.parse::<f64>().ok(),
                                _ => {}
                            }
                        }
                        if let (Some(x), Some(y), Some(z)) = (x, y, z) {
                            vertices.push(Point3::new(
                                x * unit_scale,
                                y * unit_scale,
                                z * unit_scale,
                            ));
                        }
                    }
                    "triangle" => {
                        let mut v1 = None;
                        let mut v2 = None;
                        let mut v3 = None;
                        for attr in e.attributes().flatten() {
                            let Ok(val) = attr.normalized_value(XmlVersion::Implicit1_0) else {
                                continue;
                            };
                            match attr.key.local_name().as_ref() {
                                "v1" => v1 = val.parse::<usize>().ok(),
                                "v2" => v2 = val.parse::<usize>().ok(),
                                "v3" => v3 = val.parse::<usize>().ok(),
                                _ => {}
                            }
                        }
                        if let (Some(i1), Some(i2), Some(i3)) = (v1, v2, v3) {
                            let (Some(&a), Some(&b), Some(&c)) =
                                (vertices.get(i1), vertices.get(i2), vertices.get(i3))
                            else {
                                return Err(IoError::Malformed3mf(
                                    "triangle vertex index out of range".to_owned(),
                                ));
                            };
                            all_tris.push(Triangle::new(a, b, c));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(IoError::Malformed3mf(format!("XML parse error: {e}")));
            }
            _ => {}
        }
    }

    Ok(TriangleMesh::new(all_tris)?)
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
    }
}
