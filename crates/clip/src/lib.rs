//! 2D polygon boolean, offsetting, line-clipping, and simplification seam.
//!
//! `layercraft-clip` is the sole crate in the workspace allowed to depend on
//! the underlying 2D clipping kernel (`clipper2-rust`), isolating upper layers
//! behind [`layercraft_geom`] types.

use clipper2_rust::{
    ClipType, Clipper64, ClipperOffset, EndType, FillRule, JoinType as C2JoinType, Path64, Paths64,
    Point64, PolyTree64, poly_tree_to_faces64, simplify_path,
};
use layercraft_geom::{ExPolygon, Point2, Polygon};
use thiserror::Error;

/// Join style used when offsetting polygons.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JoinType {
    /// Sharp corners up to `limit * delta`, squared beyond.
    Miter { limit: f64 },
    /// Circular arcs approximated to `arc_tolerance_nm`.
    Round { arc_tolerance_nm: i64 },
    /// Beveled / squared corners.
    Square,
}

impl Default for JoinType {
    fn default() -> Self {
        Self::Miter { limit: 2.0 }
    }
}

/// Errors produced by 2D geometry kernel operations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ClipError {
    /// Underlying clipper execution failed with an error code.
    #[error("2D kernel execution failed (code {0})")]
    KernelFailure(i32),
    /// An invalid join parameter (e.g., NaN miter limit) was provided.
    #[error("invalid join parameter: {0}")]
    InvalidParameter(&'static str),
}

fn to_path64(poly: &Polygon) -> Path64 {
    poly.points.iter().map(|p| Point64::new(p.x, p.y)).collect()
}

fn to_paths64(polys: &[ExPolygon]) -> Paths64 {
    let mut paths = Vec::new();
    for ex in polys {
        let mut outer = ex.outer.clone();
        outer.ensure_ccw();
        if outer.points.len() >= 3 {
            paths.push(to_path64(&outer));
        }
        for hole in &ex.holes {
            let mut h = hole.clone();
            h.ensure_cw();
            if h.points.len() >= 3 {
                paths.push(to_path64(&h));
            }
        }
    }
    paths
}

fn path64_to_polygon(path: &Path64) -> Polygon {
    Polygon::new(path.iter().map(|p| Point2::new(p.x, p.y)).collect())
}

fn polytree_to_expolygons(tree: &PolyTree64) -> Vec<ExPolygon> {
    let faces = poly_tree_to_faces64(tree);
    let mut out = Vec::with_capacity(faces.len());
    for face in faces {
        if face.outer.len() < 3 {
            continue;
        }
        let outer = path64_to_polygon(&face.outer);
        let holes = face
            .holes
            .iter()
            .filter(|h| h.len() >= 3)
            .map(path64_to_polygon)
            .collect();
        let ex = ExPolygon::new(outer, holes);
        if ex.outer.area_mm2() > 0.0 {
            out.push(ex);
        }
    }
    out
}

fn boolean_op(
    subjects: &Paths64,
    clips: &Paths64,
    clip_type: ClipType,
    fill_rule: FillRule,
) -> Result<Vec<ExPolygon>, ClipError> {
    if subjects.is_empty() {
        return match clip_type {
            ClipType::Union => {
                if clips.is_empty() {
                    Ok(Vec::new())
                } else {
                    let mut clipper = Clipper64::new();
                    clipper.add_subject(clips);
                    let mut tree = PolyTree64::new();
                    let mut open = Paths64::new();
                    if !clipper.execute_tree(ClipType::Union, fill_rule, &mut tree, &mut open) {
                        return Err(ClipError::KernelFailure(clipper.error_code()));
                    }
                    Ok(polytree_to_expolygons(&tree))
                }
            }
            ClipType::Intersection | ClipType::Difference => Ok(Vec::new()),
            ClipType::Xor | ClipType::NoClip => Ok(Vec::new()),
        };
    }

    let mut clipper = Clipper64::new();
    clipper.add_subject(subjects);
    if !clips.is_empty() {
        clipper.add_clip(clips);
    }
    let mut tree = PolyTree64::new();
    let mut open = Paths64::new();
    if !clipper.execute_tree(clip_type, fill_rule, &mut tree, &mut open) {
        return Err(ClipError::KernelFailure(clipper.error_code()));
    }
    Ok(polytree_to_expolygons(&tree))
}

/// Boolean union of `ExPolygon` regions.
pub fn union(polys: &[ExPolygon]) -> Result<Vec<ExPolygon>, ClipError> {
    let paths = to_paths64(polys);
    let empty = Paths64::new();
    boolean_op(&paths, &empty, ClipType::Union, FillRule::NonZero)
}

/// Normalize and union raw closed loops (e.g., from mesh slicing) into properly nested `ExPolygon`s.
pub fn union_polygons(polys: &[Polygon]) -> Result<Vec<ExPolygon>, ClipError> {
    let paths: Paths64 = polys
        .iter()
        .filter(|p| p.points.len() >= 3)
        .map(to_path64)
        .collect();
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    // Raw stitched triangle-slice loops follow consistent outward winding (outer CCW, holes CW),
    // so NonZero cleanly resolves nested islands and holes while preserving genuine holes.
    let empty = Paths64::new();
    boolean_op(&paths, &empty, ClipType::Union, FillRule::NonZero)
}

/// Boolean difference (`subject \ clip`).
pub fn difference(subject: &[ExPolygon], clip: &[ExPolygon]) -> Result<Vec<ExPolygon>, ClipError> {
    let subj = to_paths64(subject);
    if subj.is_empty() {
        return Ok(Vec::new());
    }
    let clp = to_paths64(clip);
    if clp.is_empty() {
        return union(subject);
    }
    boolean_op(&subj, &clp, ClipType::Difference, FillRule::NonZero)
}

/// Boolean intersection (`a ∩ b`).
pub fn intersection(a: &[ExPolygon], b: &[ExPolygon]) -> Result<Vec<ExPolygon>, ClipError> {
    let subj = to_paths64(a);
    let clp = to_paths64(b);
    if subj.is_empty() || clp.is_empty() {
        return Ok(Vec::new());
    }
    boolean_op(&subj, &clp, ClipType::Intersection, FillRule::NonZero)
}

/// Offset closed `ExPolygon` regions inward (`delta_nm < 0`) or outward (`delta_nm > 0`).
pub fn offset(
    polys: &[ExPolygon],
    delta_nm: i64,
    join: JoinType,
) -> Result<Vec<ExPolygon>, ClipError> {
    if polys.is_empty() {
        return Ok(Vec::new());
    }
    if delta_nm == 0 {
        return union(polys);
    }

    let (jt, miter_limit, arc_tolerance) = match join {
        JoinType::Miter { limit } => {
            if !limit.is_finite() || limit <= 0.0 {
                return Err(ClipError::InvalidParameter("miter limit must be positive"));
            }
            (C2JoinType::Miter, limit, 10_000.0)
        }
        JoinType::Round { arc_tolerance_nm } => {
            if arc_tolerance_nm <= 0 {
                return Err(ClipError::InvalidParameter(
                    "arc tolerance must be positive",
                ));
            }
            (C2JoinType::Round, 2.0, arc_tolerance_nm as f64)
        }
        JoinType::Square => (C2JoinType::Square, 2.0, 10_000.0),
    };

    let paths = to_paths64(polys);
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    let mut offsetter = ClipperOffset::new(miter_limit, arc_tolerance, false, false);
    offsetter.add_paths(&paths, jt, EndType::Polygon);
    let mut tree = PolyTree64::new();
    offsetter.execute_tree(delta_nm as f64, &mut tree);
    if offsetter.error_code() != 0 {
        return Err(ClipError::KernelFailure(offsetter.error_code()));
    }
    Ok(polytree_to_expolygons(&tree))
}

fn clip_open_paths(
    polylines: &[Vec<Point2>],
    region: &[ExPolygon],
    clip_type: ClipType,
) -> Result<Vec<Vec<Point2>>, ClipError> {
    let valid_inputs: Vec<Vec<Point2>> =
        polylines.iter().filter(|p| p.len() >= 2).cloned().collect();
    if valid_inputs.is_empty() {
        return Ok(Vec::new());
    }
    let clip_paths = to_paths64(region);
    if clip_paths.is_empty() {
        return match clip_type {
            ClipType::Difference => Ok(valid_inputs),
            _ => Ok(Vec::new()),
        };
    }

    let open_paths: Paths64 = valid_inputs
        .iter()
        .map(|poly| poly.iter().map(|p| Point64::new(p.x, p.y)).collect())
        .collect();

    let mut clipper = Clipper64::new();
    clipper.add_open_subject(&open_paths);
    clipper.add_clip(&clip_paths);

    let mut closed_out = Paths64::new();
    let mut open_out = Paths64::new();
    if !clipper.execute(
        clip_type,
        FillRule::NonZero,
        &mut closed_out,
        Some(&mut open_out),
    ) {
        return Err(ClipError::KernelFailure(clipper.error_code()));
    }

    let mut result = Vec::with_capacity(open_out.len());
    for path in open_out {
        if path.len() >= 2 {
            result.push(path.into_iter().map(|p| Point2::new(p.x, p.y)).collect());
        }
    }
    Ok(result)
}

/// Clip open line segments `(start, end)` against closed `region` polygons.
pub fn clip_lines(
    lines: &[(Point2, Point2)],
    region: &[ExPolygon],
) -> Result<Vec<Vec<Point2>>, ClipError> {
    let polylines: Vec<Vec<Point2>> = lines
        .iter()
        .filter(|(a, b)| a != b)
        .map(|&(a, b)| vec![a, b])
        .collect();
    clip_open_paths(&polylines, region, ClipType::Intersection)
}

/// Intersect open polylines with closed `region` polygons (`polylines ∩ region`).
pub fn clip_polylines(
    polylines: &[Vec<Point2>],
    region: &[ExPolygon],
) -> Result<Vec<Vec<Point2>>, ClipError> {
    clip_open_paths(polylines, region, ClipType::Intersection)
}

/// Subtract closed `region` polygons from open polylines (`polylines \ region`).
pub fn clip_polylines_difference(
    polylines: &[Vec<Point2>],
    region: &[ExPolygon],
) -> Result<Vec<Vec<Point2>>, ClipError> {
    clip_open_paths(polylines, region, ClipType::Difference)
}

/// Simplify a closed polygon using Clipper2 collinear/epsilon pruning.
pub fn simplify(poly: &Polygon, epsilon_nm: i64) -> Polygon {
    if poly.points.len() < 3 || epsilon_nm <= 0 {
        return poly.clone();
    }
    let path = to_path64(poly);
    let simplified = simplify_path(&path, epsilon_nm as f64, true);
    if simplified.len() < 3 {
        return poly.clone();
    }
    path64_to_polygon(&simplified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use layercraft_geom::NM_PER_MM;

    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Polygon {
        Polygon::new(vec![
            Point2::from_mm(x0, y0).unwrap(),
            Point2::from_mm(x1, y0).unwrap(),
            Point2::from_mm(x1, y1).unwrap(),
            Point2::from_mm(x0, y1).unwrap(),
        ])
    }

    #[test]
    fn difference_creates_hole_expolygon() {
        let outer = vec![ExPolygon::new(rect(0.0, 0.0, 20.0, 20.0), vec![])];
        let hole = vec![ExPolygon::new(rect(5.0, 5.0, 15.0, 15.0), vec![])];
        let diff = difference(&outer, &hole).unwrap();
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].holes.len(), 1);
        assert!((diff[0].area_mm2() - 300.0).abs() < 1e-4);
    }

    #[test]
    fn inset_offset_shrinks_and_collapses() {
        let sq = vec![ExPolygon::new(rect(0.0, 0.0, 10.0, 10.0), vec![])];
        let inset = offset(&sq, -NM_PER_MM, JoinType::default()).unwrap();
        assert_eq!(inset.len(), 1);
        assert!((inset[0].area_mm2() - 64.0).abs() < 1e-3);

        // Inset past half-width collapses cleanly to empty without panicking.
        let collapsed = offset(&sq, -6 * NM_PER_MM, JoinType::default()).unwrap();
        assert!(collapsed.is_empty());
    }

    #[test]
    fn clip_lines_clips_to_holed_square() {
        let outer = rect(0.0, 0.0, 20.0, 20.0);
        let mut hole = rect(8.0, 8.0, 12.0, 12.0);
        hole.ensure_cw();
        let region = vec![ExPolygon::new(outer, vec![hole])];

        let line = (
            Point2::from_mm(-5.0, 10.0).unwrap(),
            Point2::from_mm(25.0, 10.0).unwrap(),
        );
        let clipped = clip_lines(&[line], &region).unwrap();
        assert_eq!(clipped.len(), 2);
        let total_len: f64 = clipped.iter().map(|seg| seg[0].distance_mm(seg[1])).sum();
        assert!((total_len - 16.0).abs() < 1e-4);
    }

    #[test]
    fn clip_polylines_and_difference_handle_holed_square() {
        let outer = rect(0.0, 0.0, 20.0, 20.0);
        let mut hole = rect(8.0, 8.0, 12.0, 12.0);
        hole.ensure_cw();
        let region = vec![ExPolygon::new(outer, vec![hole])];

        let wave = vec![
            Point2::from_mm(-5.0, 10.0).unwrap(),
            Point2::from_mm(10.0, 10.0).unwrap(),
            Point2::from_mm(25.0, 10.0).unwrap(),
        ];
        let inside = clip_polylines(std::slice::from_ref(&wave), &region).unwrap();
        assert_eq!(inside.len(), 2);
        let inside_len: f64 = inside
            .iter()
            .map(|p| p.windows(2).map(|w| w[0].distance_mm(w[1])).sum::<f64>())
            .sum();
        assert!((inside_len - 16.0).abs() < 1e-4);

        let outside = clip_polylines_difference(std::slice::from_ref(&wave), &region).unwrap();
        assert_eq!(outside.len(), 3);
        let outside_len: f64 = outside
            .iter()
            .map(|p| p.windows(2).map(|w| w[0].distance_mm(w[1])).sum::<f64>())
            .sum();
        assert!((outside_len - 14.0).abs() < 1e-4);

        assert!(clip_polylines(&[], &region).unwrap().is_empty());
        assert_eq!(clip_polylines_difference(&[wave], &[]).unwrap().len(), 1);
    }

    #[test]
    fn hostile_empty_and_invalid_params() {
        assert!(union(&[]).unwrap().is_empty());
        assert!(offset(&[], -100, JoinType::default()).unwrap().is_empty());
        assert!(clip_lines(&[], &[]).unwrap().is_empty());
        assert!(clip_polylines(&[], &[]).unwrap().is_empty());
        assert!(clip_polylines_difference(&[], &[]).unwrap().is_empty());
        let sq = vec![ExPolygon::new(rect(0.0, 0.0, 10.0, 10.0), vec![])];
        assert!(offset(&sq, -100, JoinType::Miter { limit: f64::NAN }).is_err());
    }
}
