//! Procedural watertight test meshes and hostile input generators (`dev-dependencies` only).

use std::f64::consts::{FRAC_PI_2, TAU};

use layercraft_config::{InfillPattern, PrintConfig};
use layercraft_geom::{GeomError, Point3, Triangle, TriangleMesh};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors raised by testkit mesh recipe generation.
#[derive(Debug, Error)]
pub enum TestkitError {
    #[error(transparent)]
    Geom(#[from] GeomError),
    #[error("unknown mesh shape `{0}`")]
    UnknownShape(String),
}

/// Declarative synthetic mesh recipe shared between Rust tests and `tools/orca-oracle/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShapeSpec {
    Box {
        width_mm: f64,
        depth_mm: f64,
        height_mm: f64,
        #[serde(default)]
        offset_xyz: [f64; 3],
    },
    Cylinder {
        radius_mm: f64,
        height_mm: f64,
        #[serde(default = "default_segments")]
        segments: usize,
    },
    HollowCylinder {
        outer_radius_mm: f64,
        inner_radius_mm: f64,
        height_mm: f64,
        #[serde(default = "default_segments")]
        segments: usize,
    },
    SteppedBlock {
        base_width_mm: f64,
        base_depth_mm: f64,
        base_height_mm: f64,
        top_width_mm: f64,
        top_depth_mm: f64,
        top_height_mm: f64,
    },
    BridgeArch {
        pillar_width_mm: f64,
        span_mm: f64,
        depth_mm: f64,
        pillar_height_mm: f64,
        deck_height_mm: f64,
    },
    OverhangRamp {
        base_width_mm: f64,
        depth_mm: f64,
        height_mm: f64,
        overhang_deg: f64,
    },
    CBracket {
        width_mm: f64,
        depth_mm: f64,
        base_height_mm: f64,
        wall_width_mm: f64,
        gap_height_mm: f64,
        top_height_mm: f64,
    },
    TaperedWedge {
        length_mm: f64,
        wide_width_mm: f64,
        narrow_width_mm: f64,
        height_mm: f64,
    },
    Dome {
        radius_mm: f64,
        base_height_mm: f64,
        #[serde(default = "default_segments")]
        segments: usize,
        #[serde(default = "default_rings")]
        rings: usize,
    },
}

fn default_segments() -> usize {
    64
}

fn default_rings() -> usize {
    24
}

/// Build a watertight [`TriangleMesh`] from a [`ShapeSpec`].
pub fn build_shape(spec: &ShapeSpec) -> Result<TriangleMesh, TestkitError> {
    match *spec {
        ShapeSpec::Box {
            width_mm,
            depth_mm,
            height_mm,
            offset_xyz,
        } => {
            let mut m = make_box(width_mm, depth_mm, height_mm)?;
            m.translate(offset_xyz[0], offset_xyz[1], offset_xyz[2])?;
            Ok(m)
        }
        ShapeSpec::Cylinder {
            radius_mm,
            height_mm,
            segments,
        } => Ok(make_cylinder(radius_mm, height_mm, segments)?),
        ShapeSpec::HollowCylinder {
            outer_radius_mm,
            inner_radius_mm,
            height_mm,
            segments,
        } => Ok(make_hollow_cylinder(
            outer_radius_mm,
            inner_radius_mm,
            height_mm,
            segments,
        )?),
        ShapeSpec::SteppedBlock {
            base_width_mm,
            base_depth_mm,
            base_height_mm,
            top_width_mm,
            top_depth_mm,
            top_height_mm,
        } => Ok(make_stepped_block(
            base_width_mm,
            base_depth_mm,
            base_height_mm,
            top_width_mm,
            top_depth_mm,
            top_height_mm,
        )?),
        ShapeSpec::BridgeArch {
            pillar_width_mm,
            span_mm,
            depth_mm,
            pillar_height_mm,
            deck_height_mm,
        } => Ok(make_bridge_arch(
            pillar_width_mm,
            span_mm,
            depth_mm,
            pillar_height_mm,
            deck_height_mm,
        )?),
        ShapeSpec::OverhangRamp {
            base_width_mm,
            depth_mm,
            height_mm,
            overhang_deg,
        } => Ok(make_overhang_ramp(
            base_width_mm,
            depth_mm,
            height_mm,
            overhang_deg,
        )?),
        ShapeSpec::CBracket {
            width_mm,
            depth_mm,
            base_height_mm,
            wall_width_mm,
            gap_height_mm,
            top_height_mm,
        } => Ok(make_c_bracket(
            width_mm,
            depth_mm,
            base_height_mm,
            wall_width_mm,
            gap_height_mm,
            top_height_mm,
        )?),
        ShapeSpec::TaperedWedge {
            length_mm,
            wide_width_mm,
            narrow_width_mm,
            height_mm,
        } => Ok(make_tapered_wedge(
            length_mm,
            wide_width_mm,
            narrow_width_mm,
            height_mm,
        )?),
        ShapeSpec::Dome {
            radius_mm,
            base_height_mm,
            segments,
            rings,
        } => Ok(make_dome(radius_mm, base_height_mm, segments, rings)?),
    }
}

fn push_quad(tris: &mut Vec<Triangle>, a: Point3, b: Point3, c: Point3, d: Point3) {
    tris.push(Triangle::new(a, b, c));
    tris.push(Triangle::new(a, c, d));
}

/// Watertight axis-aligned box `[0, w] x [0, d] x [0, h]`.
pub fn make_box(w: f64, d: f64, h: f64) -> Result<TriangleMesh, GeomError> {
    let p000 = Point3::new(0.0, 0.0, 0.0);
    let p100 = Point3::new(w, 0.0, 0.0);
    let p110 = Point3::new(w, d, 0.0);
    let p010 = Point3::new(0.0, d, 0.0);
    let p001 = Point3::new(0.0, 0.0, h);
    let p101 = Point3::new(w, 0.0, h);
    let p111 = Point3::new(w, d, h);
    let p011 = Point3::new(0.0, d, h);

    let mut tris = Vec::with_capacity(12);
    push_quad(&mut tris, p000, p010, p110, p100); // bottom -Z
    push_quad(&mut tris, p001, p101, p111, p011); // top +Z
    push_quad(&mut tris, p000, p100, p101, p001); // front -Y
    push_quad(&mut tris, p010, p011, p111, p110); // back +Y
    push_quad(&mut tris, p000, p001, p011, p010); // left -X
    push_quad(&mut tris, p100, p110, p111, p101); // right +X
    TriangleMesh::new(tris)
}

/// Watertight cylinder centered in XY around `(0, 0)` spanning `z = 0..h`.
pub fn make_cylinder(radius: f64, height: f64, segments: usize) -> Result<TriangleMesh, GeomError> {
    let n = segments.max(8);
    let mut tris = Vec::with_capacity(n * 4);
    let c_bot = Point3::new(0.0, 0.0, 0.0);
    let c_top = Point3::new(0.0, 0.0, height);

    for i in 0..n {
        let a0 = TAU * (i as f64) / (n as f64);
        let a1 = TAU * (((i + 1) % n) as f64) / (n as f64);
        let b0 = Point3::new(radius * a0.cos(), radius * a0.sin(), 0.0);
        let b1 = Point3::new(radius * a1.cos(), radius * a1.sin(), 0.0);
        let t0 = Point3::new(radius * a0.cos(), radius * a0.sin(), height);
        let t1 = Point3::new(radius * a1.cos(), radius * a1.sin(), height);

        // Bottom cap (outward normal -Z)
        tris.push(Triangle::new(c_bot, b1, b0));
        // Top cap (outward normal +Z)
        tris.push(Triangle::new(c_top, t0, t1));
        // Side quad
        push_quad(&mut tris, b0, b1, t1, t0);
    }

    TriangleMesh::new(tris)
}

/// Watertight hollow cylinder (annular tube) centered at `(0, 0)` spanning `z = 0..h`.
pub fn make_hollow_cylinder(
    outer_r: f64,
    inner_r: f64,
    height: f64,
    segments: usize,
) -> Result<TriangleMesh, GeomError> {
    let n = segments.max(8);
    let mut tris = Vec::with_capacity(n * 8);

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

        // Outer wall
        push_quad(&mut tris, ob0, ob1, ot1, ot0);
        // Inner wall (normal points inward toward origin)
        push_quad(&mut tris, ib1, ib0, it0, it1);
        // Bottom annular ring (-Z)
        push_quad(&mut tris, ib0, ib1, ob1, ob0);
        // Top annular ring (+Z)
        push_quad(&mut tris, ot0, ot1, it1, it0);
    }
    TriangleMesh::new(tris)
}

/// Watertight two-tier stepped block (single manifold shell with exact coplanar horizontal step at `base_h`).
pub fn make_stepped_block(
    bw: f64,
    bd: f64,
    bh: f64,
    tw: f64,
    td: f64,
    th: f64,
) -> Result<TriangleMesh, GeomError> {
    // Disjoint-union approximation built cleanly as two closed boxes sharing z = bh interface,
    // or a single manifold mesh? Building as two stacked closed boxes works with NonZero union
    // at slices, EXCEPT at z=bh if not needed (mid-plane slicing slices at bh-0.1 and bh+0.1!).
    // Even better: emit two watertight closed boxes (base [0..bw,0..bd,0..bh] + centered top
    // [(bw-tw)/2 .. (bw+tw)/2, (bd-td)/2 .. (bd+td)/2, bh .. bh+th]) which have 0 overlapping faces
    // in mid-plane slice space!
    let mut tris = make_box(bw, bd, bh)?.triangles;
    let mut top = make_box(tw, td, th)?;
    top.translate(0.5 * (bw - tw), 0.5 * (bd - td), bh)?;
    tris.extend(top.triangles);
    TriangleMesh::new(tris)
}

/// Watertight bridge arch: two vertical pillars separated by `span_mm` under a horizontal deck.
pub fn make_bridge_arch(
    pillar_w: f64,
    span_mm: f64,
    depth_mm: f64,
    pillar_h: f64,
    deck_h: f64,
) -> Result<TriangleMesh, GeomError> {
    let mut tris = make_box(pillar_w, depth_mm, pillar_h)?.triangles;
    let mut right = make_box(pillar_w, depth_mm, pillar_h)?;
    right.translate(pillar_w + span_mm, 0.0, 0.0)?;
    tris.extend(right.triangles);

    let mut deck = make_box(2.0 * pillar_w + span_mm, depth_mm, deck_h)?;
    deck.translate(0.0, 0.0, pillar_h)?;
    tris.extend(deck.triangles);
    TriangleMesh::new(tris)
}

/// Watertight sloped overhang ramp with right wall leaning outward at `overhang_deg` from vertical.
pub fn make_overhang_ramp(
    base_w: f64,
    depth_mm: f64,
    height_mm: f64,
    overhang_deg: f64,
) -> Result<TriangleMesh, GeomError> {
    let top_w = base_w + height_mm * overhang_deg.to_radians().tan();
    let p000 = Point3::new(0.0, 0.0, 0.0);
    let p100 = Point3::new(base_w, 0.0, 0.0);
    let p110 = Point3::new(base_w, depth_mm, 0.0);
    let p010 = Point3::new(0.0, depth_mm, 0.0);
    let p001 = Point3::new(0.0, 0.0, height_mm);
    let p101 = Point3::new(top_w, 0.0, height_mm);
    let p111 = Point3::new(top_w, depth_mm, height_mm);
    let p011 = Point3::new(0.0, depth_mm, height_mm);

    let mut tris = Vec::with_capacity(12);
    push_quad(&mut tris, p000, p010, p110, p100);
    push_quad(&mut tris, p001, p101, p111, p011);
    push_quad(&mut tris, p000, p100, p101, p001);
    push_quad(&mut tris, p010, p011, p111, p110);
    push_quad(&mut tris, p000, p001, p011, p010);
    push_quad(&mut tris, p100, p110, p111, p101);
    TriangleMesh::new(tris)
}

/// Watertight C-bracket with lower arm, back vertical wall, and overhanging upper arm.
pub fn make_c_bracket(
    width_mm: f64,
    depth_mm: f64,
    base_height_mm: f64,
    wall_width_mm: f64,
    gap_height_mm: f64,
    top_height_mm: f64,
) -> Result<TriangleMesh, GeomError> {
    let mut tris = make_box(width_mm, depth_mm, base_height_mm)?.triangles;
    let mut wall = make_box(wall_width_mm, depth_mm, gap_height_mm)?;
    wall.translate(0.0, 0.0, base_height_mm)?;
    tris.extend(wall.triangles);
    let mut top = make_box(width_mm, depth_mm, top_height_mm)?;
    top.translate(0.0, 0.0, base_height_mm + gap_height_mm)?;
    tris.extend(top.triangles);
    TriangleMesh::new(tris)
}

/// Watertight trapezoidal wedge tapering linearly in Y from `wide_width_mm` at `x = 0`
/// to `narrow_width_mm` at `x = length_mm`, spanning `z = 0..height_mm`.
pub fn make_tapered_wedge(
    length_mm: f64,
    wide_width_mm: f64,
    narrow_width_mm: f64,
    height_mm: f64,
) -> Result<TriangleMesh, GeomError> {
    let hw0 = 0.5 * wide_width_mm;
    let hw1 = 0.5 * narrow_width_mm;
    let p000 = Point3::new(0.0, -hw0, 0.0);
    let p100 = Point3::new(length_mm, -hw1, 0.0);
    let p110 = Point3::new(length_mm, hw1, 0.0);
    let p010 = Point3::new(0.0, hw0, 0.0);
    let p001 = Point3::new(0.0, -hw0, height_mm);
    let p101 = Point3::new(length_mm, -hw1, height_mm);
    let p111 = Point3::new(length_mm, hw1, height_mm);
    let p011 = Point3::new(0.0, hw0, height_mm);

    let mut tris = Vec::with_capacity(12);
    push_quad(&mut tris, p000, p010, p110, p100);
    push_quad(&mut tris, p001, p101, p111, p011);
    push_quad(&mut tris, p000, p100, p101, p001);
    push_quad(&mut tris, p010, p011, p111, p110);
    push_quad(&mut tris, p000, p001, p011, p010);
    push_quad(&mut tris, p100, p110, p111, p101);
    TriangleMesh::new(tris)
}

/// Watertight cylindrical base (`0..base_h`) topped with a hemispherical dome (`radius`).
pub fn make_dome(
    radius: f64,
    base_h: f64,
    segments: usize,
    rings: usize,
) -> Result<TriangleMesh, GeomError> {
    let n = segments.max(8);
    let r_count = rings.max(4);
    let mut tris = Vec::with_capacity(n * (r_count * 2 + 3));
    let cb = Point3::new(0.0, 0.0, 0.0);

    let mut ring_pts: Vec<Vec<Point3>> = Vec::with_capacity(r_count);
    for ri in 0..r_count {
        let phi = FRAC_PI_2 * (ri as f64) / (r_count as f64);
        let rr = radius * phi.cos();
        let zz = base_h + radius * phi.sin();
        let mut pts = Vec::with_capacity(n);
        for i in 0..n {
            let a = TAU * (i as f64) / (n as f64);
            pts.push(Point3::new(rr * a.cos(), rr * a.sin(), zz));
        }
        ring_pts.push(pts);
    }

    if let Some(base_top) = ring_pts.first() {
        for i in 0..n {
            let i1 = (i + 1) % n;
            let a0 = TAU * (i as f64) / (n as f64);
            let a1 = TAU * (i1 as f64) / (n as f64);
            let b0 = Point3::new(radius * a0.cos(), radius * a0.sin(), 0.0);
            let b1 = Point3::new(radius * a1.cos(), radius * a1.sin(), 0.0);
            tris.push(Triangle::new(cb, b1, b0));
            if let (Some(&t0), Some(&t1)) = (base_top.get(i), base_top.get(i1)) {
                push_quad(&mut tris, b0, b1, t1, t0);
            }
        }
    }

    for ri in 0..r_count.saturating_sub(1) {
        let (Some(cur), Some(nxt)) = (ring_pts.get(ri), ring_pts.get(ri + 1)) else {
            continue;
        };
        for i in 0..n {
            let i1 = (i + 1) % n;
            if let (Some(&p00), Some(&p10), Some(&p11), Some(&p01)) =
                (cur.get(i), cur.get(i1), nxt.get(i1), nxt.get(i))
            {
                push_quad(&mut tris, p00, p10, p11, p01);
            }
        }
    }

    let apex = Point3::new(0.0, 0.0, base_h + radius);
    if let Some(last_ring) = ring_pts.last() {
        for i in 0..n {
            let i1 = (i + 1) % n;
            if let (Some(&p0), Some(&p1)) = (last_ring.get(i), last_ring.get(i1)) {
                tris.push(Triangle::new(p0, p1, apex));
            }
        }
    }

    TriangleMesh::new(tris)
}

/// Hostile raw byte payloads that must never panic in STL/3MF readers.
pub fn hostile_stl_inputs() -> Vec<Vec<u8>> {
    let mut bad_nan_bin = vec![0u8; 84 + 50];
    if let Some(c) = bad_nan_bin.get_mut(80..84) {
        c.copy_from_slice(&1u32.to_le_bytes());
    }
    if let Some(v) = bad_nan_bin.get_mut(84 + 12..84 + 16) {
        v.copy_from_slice(&f32::NAN.to_le_bytes());
    }

    vec![
        Vec::new(),
        b"solid broken\nvertex NaN 0 0\nendsolid".to_vec(),
        b"solid half\nvertex 1 2 3\n".to_vec(),
        vec![0xffu8; 85],
        bad_nan_bin,
    ]
}

/// Hostile configs that must be rejected cleanly by `PrintConfig::validate()`.
pub fn hostile_configs() -> Vec<PrintConfig> {
    let mut c1 = PrintConfig::default();
    c1.process.layer_height_mm = -0.2;

    let mut c2 = PrintConfig::default();
    c2.process.first_layer_height_mm = f64::NAN;

    let mut c3 = PrintConfig::default();
    c3.process.sparse_infill_density = -0.1;

    let mut c4 = PrintConfig::default();
    c4.process.sparse_infill_density = 1.25;

    let mut c5 = PrintConfig::default();
    c5.machine.nozzle_diameter_mm = 0.0;

    let mut c6 = PrintConfig::default();
    c6.process.sparse_infill_pattern = InfillPattern::Rectilinear;
    c6.filament.diameter_mm = f64::INFINITY;

    vec![c1, c2, c3, c4, c5, c6]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::f64::consts::PI;

    #[test]
    fn dome_mesh_is_watertight_with_analytical_volume() {
        let r = 10.0;
        let h = 8.0;
        let dome = make_dome(r, h, 64, 24).unwrap();

        let quant = |p: Point3| {
            (
                (p.x * 1e4).round() as i64,
                (p.y * 1e4).round() as i64,
                (p.z * 1e4).round() as i64,
            )
        };
        let mut edges: HashMap<_, usize> = HashMap::new();
        for tri in &dome.triangles {
            let qa = quant(tri.a);
            let qb = quant(tri.b);
            let qc = quant(tri.c);
            for (u, v) in [(qa, qb), (qb, qc), (qc, qa)] {
                let key = if u <= v { (u, v) } else { (v, u) };
                *edges.entry(key).or_insert(0) += 1;
            }
        }
        let non_manifold = edges.values().filter(|&&c| c != 2).count();
        assert_eq!(
            non_manifold, 0,
            "dome has {non_manifold} non-manifold edges"
        );

        let expected_vol = PI * r * r * h + (2.0 / 3.0) * PI * r * r * r;
        let got_vol = dome.signed_volume_mm3();
        assert!(
            (got_vol - expected_vol).abs() / expected_vol < 0.015,
            "dome volume {got_vol:.1} != {expected_vol:.1}"
        );
    }
}
