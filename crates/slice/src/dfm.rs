//! Printability and Design-for-Manufacturing (DFM) analysis for meshes and sliced plates.

use std::collections::{BTreeMap, HashMap};

use layercraft_clip::intersection;
use layercraft_config::{PrintConfig, WallGenerator};
use layercraft_geom::{ExPolygon, NM_PER_MM, Point3, TriangleMesh};
use layercraft_mesh::inspect_mesh;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ExtrusionPath, ExtrusionRole, SlicedLayer, SlicedPlate, bead_area_mm2};

/// Overall printability risk level summarized across detected issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrintabilityRisk {
    Low,
    Medium,
    High,
}

/// Severity of an individual DFM finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    Info,
    Warning,
    Critical,
}

/// Domain category for an individual DFM finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueCategory {
    Overhang,
    FloatingIsland,
    ThinWall,
    BedAdhesion,
    Bridge,
    Manifold,
    Bounds,
}

/// Actionable printability finding with affected layers and remediation guidance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PrintabilityIssue {
    pub severity: IssueSeverity,
    pub category: IssueCategory,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affected_layers: Vec<usize>,
    pub suggestion: String,
}

/// Unsupported cross-section island appearing in mid-air above a gap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FloatingIslandInfo {
    pub layer_index: usize,
    pub z_mm: f64,
    pub area_mm2: f64,
    pub center_xy_mm: [f64; 2],
}

/// Filament length, volume, share, and estimated print time for one extrusion role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FeatureBreakdown {
    pub role: String,
    pub length_mm: f64,
    pub volume_mm3: f64,
    pub volume_percent: f64,
    pub estimated_time_s: f64,
}

/// Comprehensive DFM & printability report for a plate mesh and optional sliced toolpaths.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PrintabilityReport {
    pub overall_risk: PrintabilityRisk,
    pub fits_bed: bool,
    pub bounds_mm: [f64; 6],
    pub size_mm: [f64; 3],
    pub is_manifold: bool,
    pub boundary_edges: usize,
    pub non_manifold_edges: usize,
    pub bed_contact_area_mm2: f64,
    pub footprint_area_mm2: f64,
    pub bed_contact_ratio: f64,
    pub height_to_base_ratio: f64,
    pub overhang_threshold_deg: f64,
    pub overhang_triangle_area_mm2: f64,
    pub max_overhang_angle_deg: f64,
    pub overhang_wall_length_mm: f64,
    pub floating_islands: Vec<FloatingIslandInfo>,
    pub bridge_count: usize,
    pub max_bridge_span_mm: f64,
    pub total_bridge_length_mm: f64,
    pub thin_wall_dropped_layers: Vec<usize>,
    pub min_wall_width_mm: Option<f64>,
    pub sliced: bool,
    pub layer_count: usize,
    pub total_filament_volume_mm3: f64,
    pub total_filament_mass_g: f64,
    pub total_estimated_time_s: f64,
    pub by_feature: Vec<FeatureBreakdown>,
    pub issues: Vec<PrintabilityIssue>,
}

type QuantPt3 = (i64, i64, i64);

fn quantize_pt3_micron(p: Point3) -> QuantPt3 {
    (
        (p.x * 1000.0).round() as i64,
        (p.y * 1000.0).round() as i64,
        (p.z * 1000.0).round() as i64,
    )
}

fn edge_key(a: QuantPt3, b: QuantPt3) -> (QuantPt3, QuantPt3) {
    if a <= b { (a, b) } else { (b, a) }
}

fn count_edge_defects(mesh: &TriangleMesh) -> (usize, usize) {
    let mut edge_counts: HashMap<(QuantPt3, QuantPt3), usize> = HashMap::new();
    for tri in &mesh.triangles {
        if tri.area_mm2() <= 1e-12 || !tri.is_finite() {
            continue;
        }
        let qa = quantize_pt3_micron(tri.a);
        let qb = quantize_pt3_micron(tri.b);
        let qc = quantize_pt3_micron(tri.c);
        for (u, v) in [(qa, qb), (qb, qc), (qc, qa)] {
            if u != v {
                *edge_counts.entry(edge_key(u, v)).or_insert(0) += 1;
            }
        }
    }
    let mut boundary_edges = 0usize;
    let mut non_manifold_edges = 0usize;
    for &c in edge_counts.values() {
        if c == 1 {
            boundary_edges += 1;
        } else if c > 2 {
            non_manifold_edges += 1;
        }
    }
    (boundary_edges, non_manifold_edges)
}

fn path_length_and_volume(path: &ExtrusionPath) -> (f64, f64, f64) {
    let n = path.points.len();
    if n < 2 {
        return (0.0, 0.0, 0.0);
    }
    let segs = if path.closed { n } else { n - 1 };
    let mut len_mm = 0.0_f64;
    let mut vol_mm3 = 0.0_f64;
    for i in 0..segs {
        let (Some(&pa), Some(&pb)) = (path.points.get(i), path.points.get((i + 1) % n)) else {
            continue;
        };
        let seg_len = pa.distance_mm(pb);
        let width = path.segment_width_mm(i);
        len_mm += seg_len;
        vol_mm3 += bead_area_mm2(width, path.height_mm) * seg_len;
    }
    let time_s = len_mm / path.speed_mm_s.max(1.0);
    (len_mm, vol_mm3, time_s)
}

fn is_supported_by_paths(reg: &ExPolygon, support_layers: &[&SlicedLayer]) -> bool {
    let Some(aabb) = reg.aabb() else {
        return false;
    };
    let margin_nm = NM_PER_MM; // 1.0 mm tolerance around support contact perimeter
    let min_x = aabb.min.x - margin_nm;
    let min_y = aabb.min.y - margin_nm;
    let max_x = aabb.max.x + margin_nm;
    let max_y = aabb.max.y + margin_nm;

    for sl in support_layers {
        for p in &sl.paths {
            if !matches!(
                p.role,
                ExtrusionRole::Support | ExtrusionRole::SupportInterface
            ) {
                continue;
            }
            for pt in &p.points {
                if pt.x >= min_x
                    && pt.x <= max_x
                    && pt.y >= min_y
                    && pt.y <= max_y
                    && (reg.contains_point(*pt) || p.role == ExtrusionRole::SupportInterface)
                {
                    return true;
                }
            }
        }
    }
    false
}

fn regions_aabb_min_span_mm(regions: &[ExPolygon]) -> Option<f64> {
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    let mut any = false;
    for reg in regions {
        if let Some(a) = reg.aabb() {
            any = true;
            min_x = min_x.min(a.min.x);
            min_y = min_y.min(a.min.y);
            max_x = max_x.max(a.max.x);
            max_y = max_y.max(a.max.y);
        }
    }
    if !any {
        return None;
    }
    let dx = (max_x - min_x) as f64 / (NM_PER_MM as f64);
    let dy = (max_y - min_y) as f64 / (NM_PER_MM as f64);
    Some(dx.min(dy))
}

/// Analyze a prepared plate mesh and optional sliced layers for DFM risks and resource usage.
pub fn analyze_printability(
    mesh: &TriangleMesh,
    sliced: Option<&SlicedPlate>,
    config: &PrintConfig,
) -> PrintabilityReport {
    let insp = inspect_mesh(mesh);
    let (boundary_edges, multi_sheet_edges) = count_edge_defects(mesh);
    let is_manifold = insp.non_manifold_edges == 0 && boundary_edges == 0 && multi_sheet_edges == 0;

    let (bounds_mm, size_mm, fits_bed) = match insp.aabb {
        Some(a) => {
            let sz = a.size();
            let fits = a.min.x >= -1e-4
                && a.min.y >= -1e-4
                && a.min.z >= -1e-4
                && a.max.x <= config.machine.bed_width_mm + 1e-4
                && a.max.y <= config.machine.bed_depth_mm + 1e-4
                && a.max.z <= config.machine.bed_height_mm + 1e-4;
            (
                [a.min.x, a.min.y, a.min.z, a.max.x, a.max.y, a.max.z],
                [sz.x, sz.y, sz.z],
                fits,
            )
        }
        None => ([0.0; 6], [0.0; 3], false),
    };

    let mut mesh_bed_contact_area_mm2 = 0.0_f64;
    let mut overhang_triangle_area_mm2 = 0.0_f64;
    let mut max_overhang_angle_deg = 0.0_f64;
    let threshold_deg = config.process.overhang_threshold_deg;

    for tri in &mesh.triangles {
        let dv = tri.double_area_vec();
        let len = dv.length();
        if len <= 1e-12 {
            continue;
        }
        let n = dv / len;
        let area = 0.5 * len;
        let max_tri_z = tri.a.z.max(tri.b.z).max(tri.c.z);
        if n.z < -0.5 && max_tri_z <= 0.05 {
            mesh_bed_contact_area_mm2 += area;
        } else if n.z < -1e-6 && max_tri_z > 0.05 {
            let angle_deg = (-n.z).clamp(0.0, 1.0).asin().to_degrees();
            if angle_deg > max_overhang_angle_deg {
                max_overhang_angle_deg = angle_deg;
            }
            if angle_deg > threshold_deg {
                overhang_triangle_area_mm2 += area;
            }
        }
    }

    let footprint_area_mm2 = (size_mm[0].max(0.0)) * (size_mm[1].max(0.0));
    let mut bed_contact_area_mm2 = mesh_bed_contact_area_mm2;
    let mut base_min_span_mm = size_mm[0].min(size_mm[1]);

    let mut overhang_wall_length_mm = 0.0_f64;
    let mut floating_islands = Vec::new();
    let mut bridge_count = 0usize;
    let mut max_bridge_span_mm = 0.0_f64;
    let mut total_bridge_length_mm = 0.0_f64;
    let mut thin_wall_dropped_layers = Vec::new();
    let mut min_wall_width_mm: Option<f64> = None;

    let mut role_totals: BTreeMap<ExtrusionRole, (f64, f64, f64)> = BTreeMap::new();
    let mut total_filament_volume_mm3 = 0.0_f64;
    let mut total_estimated_time_s = 0.0_f64;
    let mut layer_count = 0usize;

    if let Some(plate) = sliced {
        layer_count = plate.layers.len();
        if let Some(layer0) = plate.layers.first()
            && (layer0.z_top_mm - layer0.height_mm) <= 0.15
            && !layer0.slice_regions.is_empty()
        {
            bed_contact_area_mm2 = layer0.slice_regions.iter().map(ExPolygon::area_mm2).sum();
            if let Some(span) = regions_aabb_min_span_mm(&layer0.slice_regions) {
                base_min_span_mm = span;
            }
        }

        let support_z_window =
            config.process.support.top_z_distance_mm + config.process.layer_height_mm + 0.25;

        for (idx, layer) in plate.layers.iter().enumerate() {
            let layer_bottom_z = layer.z_top_mm - layer.height_mm;
            let mut layer_path_time_s = 0.0_f64;

            let area_sum: f64 = layer.slice_regions.iter().map(ExPolygon::area_mm2).sum();
            let has_wall = layer.paths.iter().any(|p| {
                matches!(
                    p.role,
                    ExtrusionRole::OuterWall
                        | ExtrusionRole::InnerWall
                        | ExtrusionRole::OverhangWall
                )
            });
            if area_sum > 0.25 && !has_wall {
                thin_wall_dropped_layers.push(layer.index);
            }

            for path in &layer.paths {
                let (len, vol, time_s) = path_length_and_volume(path);
                layer_path_time_s += time_s;
                total_filament_volume_mm3 += vol;

                let entry = role_totals.entry(path.role).or_insert((0.0, 0.0, 0.0));
                entry.0 += len;
                entry.1 += vol;
                entry.2 += time_s;

                if matches!(
                    path.role,
                    ExtrusionRole::OuterWall
                        | ExtrusionRole::InnerWall
                        | ExtrusionRole::OverhangWall
                ) {
                    let mw = path.min_width_mm();
                    min_wall_width_mm = Some(min_wall_width_mm.map_or(mw, |cur| cur.min(mw)));
                }
                if path.role == ExtrusionRole::OverhangWall {
                    overhang_wall_length_mm += len;
                } else if path.role == ExtrusionRole::Bridge {
                    bridge_count += 1;
                    total_bridge_length_mm += len;
                    if len > max_bridge_span_mm {
                        max_bridge_span_mm = len;
                    }
                }
            }

            total_estimated_time_s += if layer.estimated_time_s > 0.0 {
                layer.estimated_time_s
            } else {
                layer_path_time_s
            };

            if idx > 0
                && let Some(prev) = plate.layers.get(idx - 1)
            {
                let support_layers: Vec<&SlicedLayer> = plate
                    .layers
                    .get(..idx)
                    .unwrap_or(&[])
                    .iter()
                    .rev()
                    .take_while(|l| (layer_bottom_z - l.z_top_mm) <= support_z_window)
                    .collect();

                for reg in &layer.slice_regions {
                    let area = reg.area_mm2();
                    if area <= 0.25 {
                        continue;
                    }
                    let overlap_area: f64 =
                        intersection(std::slice::from_ref(reg), &prev.slice_regions)
                            .map(|ov| ov.iter().map(ExPolygon::area_mm2).sum())
                            .unwrap_or(0.0);
                    if overlap_area < 0.05 * area && !is_supported_by_paths(reg, &support_layers) {
                        let center = reg
                            .aabb()
                            .map(|b| {
                                let (x0, y0) = b.min.to_mm();
                                let (x1, y1) = b.max.to_mm();
                                [0.5 * (x0 + x1), 0.5 * (y0 + y1)]
                            })
                            .unwrap_or([0.0, 0.0]);
                        floating_islands.push(FloatingIslandInfo {
                            layer_index: layer.index,
                            z_mm: layer.z_top_mm,
                            area_mm2: area,
                            center_xy_mm: center,
                        });
                    }
                }
            }
        }
    }

    let bed_contact_ratio = if footprint_area_mm2 > 1e-6 {
        (bed_contact_area_mm2 / footprint_area_mm2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let height_to_base_ratio = if base_min_span_mm > 1e-3 {
        size_mm[2] / base_min_span_mm
    } else {
        0.0
    };

    let total_filament_mass_g =
        total_filament_volume_mm3 * 1e-3 * config.filament.density_g_cm3.max(0.0);

    let by_feature = role_totals
        .into_iter()
        .map(|(role, (len, vol, time_s))| {
            let pct = if total_filament_volume_mm3 > 1e-9 {
                (vol / total_filament_volume_mm3) * 100.0
            } else {
                0.0
            };
            FeatureBreakdown {
                role: role.label().to_owned(),
                length_mm: len,
                volume_mm3: vol,
                volume_percent: pct,
                estimated_time_s: time_s,
            }
        })
        .collect();

    let mut issues = Vec::new();
    if !fits_bed {
        issues.push(PrintabilityIssue {
            severity: IssueSeverity::Critical,
            category: IssueCategory::Bounds,
            summary: format!(
                "Part extends outside the {:.0}x{:.0}x{:.0} mm build volume.",
                config.machine.bed_width_mm,
                config.machine.bed_depth_mm,
                config.machine.bed_height_mm
            ),
            affected_layers: Vec::new(),
            suggestion: "Run `plate.auto_arrange`, scale down via `plate.transform_object`, or select a larger machine profile.".to_owned(),
        });
    }

    if !is_manifold {
        issues.push(PrintabilityIssue {
            severity: IssueSeverity::Warning,
            category: IssueCategory::Manifold,
            summary: format!(
                "Mesh is non-manifold ({boundary_edges} boundary edges, {multi_sheet_edges} >2-face edges)."
            ),
            affected_layers: Vec::new(),
            suggestion: "Repair open edges / self-intersections in CAD before slicing if slice regions look incomplete.".to_owned(),
        });
    }

    if !floating_islands.is_empty() {
        let layers: Vec<usize> = floating_islands.iter().map(|i| i.layer_index).collect();
        issues.push(PrintabilityIssue {
            severity: IssueSeverity::Critical,
            category: IssueCategory::FloatingIsland,
            summary: format!(
                "Detected {} unsupported floating island(s) starting above a gap.",
                floating_islands.len()
            ),
            affected_layers: layers,
            suggestion: "Enable supports (`process.support.enabled = true`, e.g. `style: \"tree\"` or `\"snug\"`) or reorient the part flat on the bed.".to_owned(),
        });
    }

    if !thin_wall_dropped_layers.is_empty() {
        let severity = if config.process.wall_generator == WallGenerator::Classic {
            IssueSeverity::Warning
        } else {
            IssueSeverity::Info
        };
        issues.push(PrintabilityIssue {
            severity,
            category: IssueCategory::ThinWall,
            summary: format!(
                "{} layer(s) have solid cross-section area (>0.25 mm2) but no perimeter wall paths.",
                thin_wall_dropped_layers.len()
            ),
            affected_layers: thin_wall_dropped_layers.clone(),
            suggestion: "Enable Arachne variable-width walls (`process.wall_generator = \"arachne\"`) or reduce `process.outer_wall_width_mm`.".to_owned(),
        });
    }

    if overhang_triangle_area_mm2 > 4.0 && !config.process.support.enabled {
        issues.push(PrintabilityIssue {
            severity: IssueSeverity::Warning,
            category: IssueCategory::Overhang,
            summary: format!(
                "{overhang_triangle_area_mm2:.1} mm2 of downward surface exceeds {threshold_deg:.0} deg overhang (max {max_overhang_angle_deg:.1} deg) with supports disabled."
            ),
            affected_layers: Vec::new(),
            suggestion: "Enable supports (`process.support.enabled = true`) or lower overhang speed / add cooling.".to_owned(),
        });
    }

    if (bed_contact_area_mm2 < 25.0 || height_to_base_ratio > 3.5)
        && config.process.brim_width_mm < 2.0
    {
        issues.push(PrintabilityIssue {
            severity: IssueSeverity::Warning,
            category: IssueCategory::BedAdhesion,
            summary: format!(
                "Low bed stability (contact {bed_contact_area_mm2:.1} mm2, height/base ratio {height_to_base_ratio:.1})."
            ),
            affected_layers: vec![0],
            suggestion: "Add a brim (`process.brim_width_mm = 5.0`) to prevent warping or tipping.".to_owned(),
        });
    }

    if bridge_count > 0 && max_bridge_span_mm > 20.0 && !config.process.support.enabled {
        issues.push(PrintabilityIssue {
            severity: IssueSeverity::Info,
            category: IssueCategory::Bridge,
            summary: format!(
                "Detected {bridge_count} unsupported bridge segment(s) up to {max_bridge_span_mm:.1} mm."
            ),
            affected_layers: Vec::new(),
            suggestion: "Inspect bridge layer screenshots; enable supports or lower `process.bridge_speed_mm_s` if sagging occurs.".to_owned(),
        });
    }

    let overall_risk = if issues.iter().any(|i| i.severity == IssueSeverity::Critical) {
        PrintabilityRisk::High
    } else if issues.iter().any(|i| i.severity == IssueSeverity::Warning) {
        PrintabilityRisk::Medium
    } else {
        PrintabilityRisk::Low
    };

    PrintabilityReport {
        overall_risk,
        fits_bed,
        bounds_mm,
        size_mm,
        is_manifold,
        boundary_edges,
        non_manifold_edges: multi_sheet_edges,
        bed_contact_area_mm2,
        footprint_area_mm2,
        bed_contact_ratio,
        height_to_base_ratio,
        overhang_threshold_deg: threshold_deg,
        overhang_triangle_area_mm2,
        max_overhang_angle_deg,
        overhang_wall_length_mm,
        floating_islands,
        bridge_count,
        max_bridge_span_mm,
        total_bridge_length_mm,
        thin_wall_dropped_layers,
        min_wall_width_mm,
        sliced: sliced.is_some(),
        layer_count,
        total_filament_volume_mm3,
        total_filament_mass_g,
        total_estimated_time_s,
        by_feature,
        issues,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use layercraft_config::SupportStyle;
    use layercraft_geom::Triangle;
    use layercraft_mesh::prepare_for_plate;

    fn add_box(tris: &mut Vec<Triangle>, x0: f64, x1: f64, y0: f64, y1: f64, z0: f64, z1: f64) {
        let p000 = Point3::new(x0, y0, z0);
        let p100 = Point3::new(x1, y0, z0);
        let p110 = Point3::new(x1, y1, z0);
        let p010 = Point3::new(x0, y1, z0);
        let p001 = Point3::new(x0, y0, z1);
        let p101 = Point3::new(x1, y0, z1);
        let p111 = Point3::new(x1, y1, z1);
        let p011 = Point3::new(x0, y1, z1);
        // Bottom (normal -Z)
        tris.push(Triangle::new(p000, p110, p100));
        tris.push(Triangle::new(p000, p010, p110));
        // Top (normal +Z)
        tris.push(Triangle::new(p001, p101, p111));
        tris.push(Triangle::new(p001, p111, p011));
        // -Y
        tris.push(Triangle::new(p000, p100, p101));
        tris.push(Triangle::new(p000, p101, p001));
        // +Y
        tris.push(Triangle::new(p010, p111, p110));
        tris.push(Triangle::new(p010, p011, p111));
        // -X
        tris.push(Triangle::new(p000, p001, p011));
        tris.push(Triangle::new(p000, p011, p010));
        // +X
        tris.push(Triangle::new(p100, p110, p111));
        tris.push(Triangle::new(p100, p111, p101));
    }

    #[test]
    fn dfm_detects_overhangs_and_floating_islands_and_clears_with_supports() {
        // Vertical pillar + top beam + hanging block at X=16..24, Z=4..8 starting in mid-air.
        let mut tris = Vec::new();
        add_box(&mut tris, 0.0, 10.0, 0.0, 10.0, 0.0, 10.0);
        add_box(&mut tris, 10.0, 24.0, 0.0, 10.0, 8.0, 10.0);
        add_box(&mut tris, 16.0, 24.0, 0.0, 10.0, 4.0, 8.0);
        let raw = TriangleMesh::new(tris).unwrap();

        let mut cfg = PrintConfig::default();
        cfg.process.layer_height_mm = 0.4;
        cfg.process.first_layer_height_mm = 0.4;
        cfg.process.support.enabled = false;
        let mesh =
            prepare_for_plate(&raw, cfg.machine.bed_width_mm, cfg.machine.bed_depth_mm).unwrap();

        let unsup = crate::slice_single_mesh(&mesh, &cfg).unwrap();
        let rep_unsup = analyze_printability(&mesh, Some(&unsup), &cfg);
        assert_eq!(rep_unsup.overall_risk, PrintabilityRisk::High);
        assert!(!rep_unsup.floating_islands.is_empty());
        assert!(rep_unsup.overhang_triangle_area_mm2 > 50.0);

        cfg.process.support.enabled = true;
        cfg.process.support.style = SupportStyle::Snug;
        cfg.process.support.top_z_distance_mm = 0.0;
        let sup = crate::slice_single_mesh(&mesh, &cfg).unwrap();
        let rep_sup = analyze_printability(&mesh, Some(&sup), &cfg);
        assert!(
            rep_sup.floating_islands.len() < rep_unsup.floating_islands.len(),
            "supports should eliminate floating islands (unsup={}, sup={})",
            rep_unsup.floating_islands.len(),
            rep_sup.floating_islands.len()
        );
    }

    #[test]
    fn dfm_detects_thin_wall_collapse_in_classic_and_improves_with_arachne() {
        // Narrow 0.35 mm wall rib that collapses with 2x 0.42 mm classic walls.
        let mut tris = Vec::new();
        add_box(&mut tris, 0.0, 20.0, 0.0, 12.0, 0.0, 2.0);
        add_box(&mut tris, 0.0, 20.0, 5.825, 6.175, 2.0, 8.0);
        let raw = TriangleMesh::new(tris).unwrap();

        let mut cfg = PrintConfig::default();
        cfg.process.layer_height_mm = 0.4;
        cfg.process.first_layer_height_mm = 0.4;
        cfg.process.wall_generator = WallGenerator::Classic;
        let mesh =
            prepare_for_plate(&raw, cfg.machine.bed_width_mm, cfg.machine.bed_depth_mm).unwrap();

        let classic = crate::slice_single_mesh(&mesh, &cfg).unwrap();
        let rep_classic = analyze_printability(&mesh, Some(&classic), &cfg);
        assert!(!rep_classic.thin_wall_dropped_layers.is_empty());

        cfg.process.wall_generator = WallGenerator::Arachne;
        let arachne = crate::slice_single_mesh(&mesh, &cfg).unwrap();
        let rep_arachne = analyze_printability(&mesh, Some(&arachne), &cfg);
        assert!(
            rep_arachne.thin_wall_dropped_layers.len() < rep_classic.thin_wall_dropped_layers.len()
        );
    }

    #[test]
    fn dfm_reports_bed_contact_and_per_feature_breakdown() {
        let mut tris = Vec::new();
        add_box(&mut tris, 0.0, 20.0, 0.0, 20.0, 0.0, 10.0);
        let raw = TriangleMesh::new(tris).unwrap();
        let mut cfg = PrintConfig::default();
        cfg.process.layer_height_mm = 0.4;
        cfg.process.first_layer_height_mm = 0.4;
        let mesh =
            prepare_for_plate(&raw, cfg.machine.bed_width_mm, cfg.machine.bed_depth_mm).unwrap();
        let sliced = crate::slice_single_mesh(&mesh, &cfg).unwrap();
        let rep = analyze_printability(&mesh, Some(&sliced), &cfg);

        assert!(rep.fits_bed);
        assert!(rep.is_manifold);
        assert!((rep.bed_contact_area_mm2 - 400.0).abs() < 5.0);
        assert!((rep.bed_contact_ratio - 1.0).abs() < 0.05);
        assert!(rep.total_filament_volume_mm3 > 100.0);
        assert!(rep.total_filament_mass_g > 0.1);
        assert!(!rep.by_feature.is_empty());
        assert_eq!(rep.overall_risk, PrintabilityRisk::Low);
    }
}
