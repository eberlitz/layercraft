//! Fixed-point 2D geometry (`i64` nanometers) and 3D mesh primitives for LayerCraft.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Fixed-point coordinate scale: 1 millimeter = 1,000,000 nanometers.
pub const NM_PER_MM: i64 = 1_000_000;

/// Errors returned when constructing or converting geometry values.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum GeomError {
    /// A floating-point coordinate was `NaN` or infinite.
    #[error("non-finite coordinate value: {0}")]
    NonFiniteCoordinate(f64),
    /// A coordinate exceeded the representable `i64` nanometer range.
    #[error("coordinate {0} mm overflows fixed-point nanometer range")]
    CoordinateOverflow(f64),
    /// A triangle mesh contained no valid triangles.
    #[error("triangle mesh is empty")]
    EmptyMesh,
}

/// Convert millimeters (`f64`) to fixed-point nanometers (`i64`).
pub fn mm_to_nm(mm: f64) -> Result<i64, GeomError> {
    if !mm.is_finite() {
        return Err(GeomError::NonFiniteCoordinate(mm));
    }
    let scaled = (mm * (NM_PER_MM as f64)).round();
    if !scaled.is_finite() || scaled < (i64::MIN as f64) * 0.5 || scaled > (i64::MAX as f64) * 0.5 {
        return Err(GeomError::CoordinateOverflow(mm));
    }
    Ok(scaled as i64)
}

/// Convert fixed-point nanometers (`i64`) to millimeters (`f64`).
#[inline]
pub fn nm_to_mm(nm: i64) -> f64 {
    (nm as f64) / (NM_PER_MM as f64)
}

/// A 2D point in fixed-point nanometers (`1 mm = 1_000_000`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
pub struct Point2 {
    pub x: i64,
    pub y: i64,
}

impl Point2 {
    /// Construct a point from nanometer coordinates.
    #[inline]
    pub const fn new(x: i64, y: i64) -> Self {
        Self { x, y }
    }

    /// Construct a point from millimeter coordinates.
    pub fn from_mm(x_mm: f64, y_mm: f64) -> Result<Self, GeomError> {
        Ok(Self {
            x: mm_to_nm(x_mm)?,
            y: mm_to_nm(y_mm)?,
        })
    }

    /// Convert to millimeter coordinates `(x_mm, y_mm)`.
    #[inline]
    pub fn to_mm(self) -> (f64, f64) {
        (nm_to_mm(self.x), nm_to_mm(self.y))
    }

    /// Euclidean distance between two points in millimeters.
    #[inline]
    pub fn distance_mm(self, other: Self) -> f64 {
        let dx = (self.x as i128 - other.x as i128) as f64;
        let dy = (self.y as i128 - other.y as i128) as f64;
        dx.hypot(dy) / (NM_PER_MM as f64)
    }

    /// Squared Euclidean distance in nanometer^2 as `i128`.
    #[inline]
    pub fn distance_sq_nm(self, other: Self) -> i128 {
        let dx = self.x as i128 - other.x as i128;
        let dy = self.y as i128 - other.y as i128;
        dx * dx + dy * dy
    }
}

/// 2D axis-aligned bounding box in nanometers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Aabb2 {
    pub min: Point2,
    pub max: Point2,
}

impl Aabb2 {
    /// Compute bounding box of a point slice, returning `None` if empty.
    pub fn from_points(pts: &[Point2]) -> Option<Self> {
        let first = pts.first()?;
        let mut min = *first;
        let mut max = *first;
        for &p in pts.iter().skip(1) {
            min.x = min.x.min(p.x);
            min.y = min.y.min(p.y);
            max.x = max.x.max(p.x);
            max.y = max.y.max(p.y);
        }
        Some(Self { min, max })
    }

    /// Merge two bounding boxes.
    pub fn union(self, other: Self) -> Self {
        Self {
            min: Point2::new(self.min.x.min(other.min.x), self.min.y.min(other.min.y)),
            max: Point2::new(self.max.x.max(other.max.x), self.max.y.max(other.max.y)),
        }
    }
}

/// A closed 2D polygon defined by ordered vertices in nanometers.
///
/// Convention: outer boundaries are counter-clockwise (positive signed area),
/// and holes are clockwise (negative signed area).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Polygon {
    pub points: Vec<Point2>,
}

impl Polygon {
    /// Create a polygon from vertices.
    pub fn new(points: Vec<Point2>) -> Self {
        Self { points }
    }

    /// Twice the signed area in `nm^2` (`i128`, exact for coordinates up to ~2e9 nm = 2 km).
    pub fn twice_signed_area_nm2(&self) -> i128 {
        let n = self.points.len();
        if n < 3 {
            return 0;
        }
        let mut sum: i128 = 0;
        for i in 0..n {
            let Some(a) = self.points.get(i) else {
                continue;
            };
            let Some(b) = self.points.get((i + 1) % n) else {
                continue;
            };
            sum += (a.x as i128) * (b.y as i128) - (b.x as i128) * (a.y as i128);
        }
        sum
    }

    /// Signed area in square millimeters (positive for CCW, negative for CW).
    pub fn signed_area_mm2(&self) -> f64 {
        let scale = (NM_PER_MM as f64) * (NM_PER_MM as f64) * 2.0;
        (self.twice_signed_area_nm2() as f64) / scale
    }

    /// Unsigned area in square millimeters.
    pub fn area_mm2(&self) -> f64 {
        self.signed_area_mm2().abs()
    }

    /// True if vertices wind counter-clockwise (strictly positive area).
    pub fn is_ccw(&self) -> bool {
        self.twice_signed_area_nm2() > 0
    }

    /// Reverse vertex order in-place.
    pub fn reverse(&mut self) {
        self.points.reverse();
    }

    /// Ensure counter-clockwise orientation.
    pub fn ensure_ccw(&mut self) {
        if self.twice_signed_area_nm2() < 0 {
            self.reverse();
        }
    }

    /// Ensure clockwise orientation.
    pub fn ensure_cw(&mut self) {
        if self.twice_signed_area_nm2() > 0 {
            self.reverse();
        }
    }

    /// Closed perimeter length in millimeters.
    pub fn perimeter_mm(&self) -> f64 {
        let n = self.points.len();
        if n < 2 {
            return 0.0;
        }
        let mut total = 0.0;
        for i in 0..n {
            if let (Some(&a), Some(&b)) = (self.points.get(i), self.points.get((i + 1) % n)) {
                total += a.distance_mm(b);
            }
        }
        total
    }

    /// Bounding box of the polygon.
    pub fn aabb(&self) -> Option<Aabb2> {
        Aabb2::from_points(&self.points)
    }

    /// Winding-number point-in-polygon check using exact `robust::orient2d`.
    pub fn contains_point(&self, p: Point2) -> bool {
        let n = self.points.len();
        if n < 3 {
            return false;
        }
        let rp = robust::Coord {
            x: p.x as f64,
            y: p.y as f64,
        };
        let mut wn: i32 = 0;
        for i in 0..n {
            let (Some(&a), Some(&b)) = (self.points.get(i), self.points.get((i + 1) % n)) else {
                continue;
            };
            let ra = robust::Coord {
                x: a.x as f64,
                y: a.y as f64,
            };
            let rb = robust::Coord {
                x: b.x as f64,
                y: b.y as f64,
            };
            if a.y <= p.y {
                if b.y > p.y && robust::orient2d(ra, rb, rp) > 0.0 {
                    wn += 1;
                }
            } else if b.y <= p.y && robust::orient2d(ra, rb, rp) < 0.0 {
                wn -= 1;
            }
        }
        wn != 0
    }
}

/// An outer boundary polygon with zero or more interior hole polygons.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct ExPolygon {
    pub outer: Polygon,
    pub holes: Vec<Polygon>,
}

impl ExPolygon {
    /// Construct an `ExPolygon`, normalizing outer to CCW and holes to CW.
    pub fn new(mut outer: Polygon, mut holes: Vec<Polygon>) -> Self {
        outer.ensure_ccw();
        for h in &mut holes {
            h.ensure_cw();
        }
        Self { outer, holes }
    }

    /// Net area in square millimeters (outer area minus hole areas).
    pub fn area_mm2(&self) -> f64 {
        let holes_area: f64 = self.holes.iter().map(Polygon::area_mm2).sum();
        (self.outer.area_mm2() - holes_area).max(0.0)
    }

    /// Bounding box of the outer contour.
    pub fn aabb(&self) -> Option<Aabb2> {
        self.outer.aabb()
    }

    /// Returns `true` if `p` lies inside the outer contour and outside all holes.
    pub fn contains_point(&self, p: Point2) -> bool {
        self.outer.contains_point(p) && !self.holes.iter().any(|h| h.contains_point(p))
    }
}

/// A 3D point in millimeters (`f64`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Point3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Point3 {
    #[inline]
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    #[inline]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }

    #[inline]
    pub fn to_vec3(self) -> glam::DVec3 {
        glam::DVec3::new(self.x, self.y, self.z)
    }

    #[inline]
    pub fn from_vec3(v: glam::DVec3) -> Self {
        Self {
            x: v.x,
            y: v.y,
            z: v.z,
        }
    }
}

/// A 3D triangle with vertices ordered counter-clockwise when viewed from outside.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Triangle {
    pub a: Point3,
    pub b: Point3,
    pub c: Point3,
}

impl Triangle {
    #[inline]
    pub const fn new(a: Point3, b: Point3, c: Point3) -> Self {
        Self { a, b, c }
    }

    #[inline]
    pub fn is_finite(self) -> bool {
        self.a.is_finite() && self.b.is_finite() && self.c.is_finite()
    }

    /// Unnormalized area vector `(b - a) x (c - a)` (magnitude is 2x triangle area).
    #[inline]
    pub fn double_area_vec(&self) -> glam::DVec3 {
        let ab = self.b.to_vec3() - self.a.to_vec3();
        let ac = self.c.to_vec3() - self.a.to_vec3();
        ab.cross(ac)
    }

    /// Surface area in `mm^2`.
    #[inline]
    pub fn area_mm2(&self) -> f64 {
        0.5 * self.double_area_vec().length()
    }
}

/// 3D axis-aligned bounding box in millimeters.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Aabb3 {
    pub min: Point3,
    pub max: Point3,
}

impl Aabb3 {
    /// Size `(dx, dy, dz)` in millimeters.
    pub fn size(self) -> Point3 {
        Point3::new(
            self.max.x - self.min.x,
            self.max.y - self.min.y,
            self.max.z - self.min.z,
        )
    }

    /// Center point in millimeters.
    pub fn center(self) -> Point3 {
        Point3::new(
            0.5 * (self.min.x + self.max.x),
            0.5 * (self.min.y + self.max.y),
            0.5 * (self.min.z + self.max.z),
        )
    }
}

/// A 3D triangle mesh in millimeters.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriangleMesh {
    pub triangles: Vec<Triangle>,
}

impl TriangleMesh {
    /// Construct a mesh from triangles, validating finiteness and non-emptiness.
    pub fn new(triangles: Vec<Triangle>) -> Result<Self, GeomError> {
        if triangles.is_empty() {
            return Err(GeomError::EmptyMesh);
        }
        for tri in &triangles {
            if !tri.is_finite() {
                let bad = [
                    tri.a.x, tri.a.y, tri.a.z, tri.b.x, tri.b.y, tri.b.z, tri.c.x, tri.c.y, tri.c.z,
                ]
                .into_iter()
                .find(|v| !v.is_finite())
                .unwrap_or(f64::NAN);
                return Err(GeomError::NonFiniteCoordinate(bad));
            }
        }
        Ok(Self { triangles })
    }

    /// Axis-aligned bounding box of all vertices, or `None` if empty.
    pub fn aabb(&self) -> Option<Aabb3> {
        let first = self.triangles.first()?;
        let mut min = first.a;
        let mut max = first.a;
        for tri in &self.triangles {
            for p in [tri.a, tri.b, tri.c] {
                min.x = min.x.min(p.x);
                min.y = min.y.min(p.y);
                min.z = min.z.min(p.z);
                max.x = max.x.max(p.x);
                max.y = max.y.max(p.y);
                max.z = max.z.max(p.z);
            }
        }
        Some(Aabb3 { min, max })
    }

    /// Signed volume via divergence theorem in `mm^3` (positive for outward-oriented closed meshes).
    pub fn signed_volume_mm3(&self) -> f64 {
        let mut sum6 = 0.0;
        for tri in &self.triangles {
            let a = tri.a.to_vec3();
            let b = tri.b.to_vec3();
            let c = tri.c.to_vec3();
            sum6 += a.dot(b.cross(c));
        }
        sum6 / 6.0
    }

    /// Total surface area in `mm^2`.
    pub fn surface_area_mm2(&self) -> f64 {
        self.triangles.iter().map(Triangle::area_mm2).sum()
    }

    /// Translate every vertex by `(dx, dy, dz)` in place.
    pub fn translate(&mut self, dx: f64, dy: f64, dz: f64) -> Result<(), GeomError> {
        if !dx.is_finite() {
            return Err(GeomError::NonFiniteCoordinate(dx));
        }
        if !dy.is_finite() {
            return Err(GeomError::NonFiniteCoordinate(dy));
        }
        if !dz.is_finite() {
            return Err(GeomError::NonFiniteCoordinate(dz));
        }
        for tri in &mut self.triangles {
            for p in [&mut tri.a, &mut tri.b, &mut tri.c] {
                p.x += dx;
                p.y += dy;
                p.z += dz;
            }
        }
        Ok(())
    }

    /// Ground the mesh so `min_z = 0.0` and center its XY bounding box at `(bed_cx, bed_cy)`.
    pub fn ground_and_center(&mut self, bed_cx: f64, bed_cy: f64) -> Result<(), GeomError> {
        let aabb = self.aabb().ok_or(GeomError::EmptyMesh)?;
        let c = aabb.center();
        self.translate(bed_cx - c.x, bed_cy - c.y, -aabb.min.z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polygon_orientation_and_area() {
        let mut square = Polygon::new(vec![
            Point2::from_mm(0.0, 0.0).unwrap(),
            Point2::from_mm(10.0, 0.0).unwrap(),
            Point2::from_mm(10.0, 10.0).unwrap(),
            Point2::from_mm(0.0, 10.0).unwrap(),
        ]);
        assert!(square.is_ccw());
        assert!((square.area_mm2() - 100.0).abs() < 1e-6);
        assert!((square.perimeter_mm() - 40.0).abs() < 1e-6);
        assert!(square.contains_point(Point2::from_mm(5.0, 5.0).unwrap()));
        assert!(!square.contains_point(Point2::from_mm(15.0, 5.0).unwrap()));

        square.ensure_cw();
        assert!(!square.is_ccw());
    }

    #[test]
    fn expolygon_hole_subtraction() {
        let outer = Polygon::new(vec![
            Point2::from_mm(0.0, 0.0).unwrap(),
            Point2::from_mm(20.0, 0.0).unwrap(),
            Point2::from_mm(20.0, 20.0).unwrap(),
            Point2::from_mm(0.0, 20.0).unwrap(),
        ]);
        let hole = Polygon::new(vec![
            Point2::from_mm(5.0, 5.0).unwrap(),
            Point2::from_mm(15.0, 5.0).unwrap(),
            Point2::from_mm(15.0, 15.0).unwrap(),
            Point2::from_mm(5.0, 15.0).unwrap(),
        ]);
        let ex = ExPolygon::new(outer, vec![hole]);
        assert!((ex.area_mm2() - 300.0).abs() < 1e-6);
        assert!(ex.contains_point(Point2::from_mm(2.0, 2.0).unwrap()));
        assert!(!ex.contains_point(Point2::from_mm(10.0, 10.0).unwrap()));
    }

    #[test]
    fn non_finite_coordinates_rejected() {
        assert!(matches!(
            mm_to_nm(f64::NAN),
            Err(GeomError::NonFiniteCoordinate(_))
        ));
        assert!(matches!(
            mm_to_nm(f64::INFINITY),
            Err(GeomError::NonFiniteCoordinate(_))
        ));
        assert!(matches!(
            TriangleMesh::new(vec![]),
            Err(GeomError::EmptyMesh)
        ));
    }
}
