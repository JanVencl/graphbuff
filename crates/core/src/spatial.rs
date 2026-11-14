//! Spatial values: 2D and 3D points in a Cartesian or a WGS-84 (geographic) coordinate
//! reference system, as in Neo4j.
//!
//! | CRS | SRID | coordinates |
//! |---|---|---|
//! | `cartesian` | 7203 | `x`, `y` |
//! | `cartesian-3d` | 9157 | `x`, `y`, `z` |
//! | `wgs-84` | 4326 | `longitude` (`x`), `latitude` (`y`) |
//! | `wgs-84-3d` | 4979 | `longitude`, `latitude`, `height` (`z`, metres) |
//!
//! Points of one CRS test equality by coordinates; points are never ordered by `<`
//! (the comparison is `null`), but `ORDER BY` sorts them by CRS, then coordinates.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::value::format_float;
use crate::{Error, Result, Value};

/// The mean Earth radius Neo4j uses for geographic distances, in metres.
pub const EARTH_RADIUS_METERS: f64 = 6_378_140.0;

/// A coordinate reference system.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Crs {
    Cartesian,
    Cartesian3D,
    Wgs84,
    Wgs84_3D,
}

impl Crs {
    pub const ALL: [Crs; 4] = [Crs::Cartesian, Crs::Cartesian3D, Crs::Wgs84, Crs::Wgs84_3D];

    pub fn name(self) -> &'static str {
        match self {
            Crs::Cartesian => "cartesian",
            Crs::Cartesian3D => "cartesian-3d",
            Crs::Wgs84 => "wgs-84",
            Crs::Wgs84_3D => "wgs-84-3d",
        }
    }

    pub fn srid(self) -> i64 {
        match self {
            Crs::Cartesian => 7203,
            Crs::Cartesian3D => 9157,
            Crs::Wgs84 => 4326,
            Crs::Wgs84_3D => 4979,
        }
    }

    /// Number of coordinates: 2 or 3.
    pub fn dims(self) -> usize {
        match self {
            Crs::Cartesian | Crs::Wgs84 => 2,
            Crs::Cartesian3D | Crs::Wgs84_3D => 3,
        }
    }

    pub fn is_geographic(self) -> bool {
        matches!(self, Crs::Wgs84 | Crs::Wgs84_3D)
    }

    pub fn from_name(name: &str) -> Option<Crs> {
        Crs::ALL.into_iter().find(|c| c.name().eq_ignore_ascii_case(name))
    }

    pub fn from_srid(srid: i64) -> Option<Crs> {
        Crs::ALL.into_iter().find(|c| c.srid() == srid)
    }

    /// The CRS of the same kind with `dims` coordinates.
    fn with_dims(self, dims: usize) -> Crs {
        match (self.is_geographic(), dims) {
            (false, 2) => Crs::Cartesian,
            (false, _) => Crs::Cartesian3D,
            (true, 2) => Crs::Wgs84,
            (true, _) => Crs::Wgs84_3D,
        }
    }

    /// A small code for the storage encoding.
    pub(crate) fn code(self) -> u8 {
        match self {
            Crs::Cartesian => 1,
            Crs::Cartesian3D => 2,
            Crs::Wgs84 => 3,
            Crs::Wgs84_3D => 4,
        }
    }

    pub(crate) fn from_code(code: u8) -> Option<Crs> {
        Crs::ALL.into_iter().find(|c| c.code() == code)
    }
}

/// A point. Coordinates are finite; `coords[2]` is `0.0` in a 2D CRS. Geographic points
/// keep longitude in `[-180, 180]` and latitude in `[-90, 90]`.
#[derive(Copy, Clone, Debug)]
pub struct Point {
    crs: Crs,
    coords: [f64; 3],
}

fn canonical(f: f64) -> f64 {
    if f == 0.0 { 0.0 } else { f }
}

impl PartialEq for Point {
    fn eq(&self, other: &Self) -> bool {
        self.crs == other.crs && self.coords().iter().zip(other.coords()).all(|(a, b)| a == b)
    }
}

impl Eq for Point {}

impl Hash for Point {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.crs.hash(state);
        for c in self.coords() {
            canonical(*c).to_bits().hash(state);
        }
    }
}

impl Point {
    /// A point from coordinates (`coords.len()` must match the CRS), checked.
    pub fn new(crs: Crs, coords: &[f64]) -> Result<Point> {
        if coords.len() != crs.dims() {
            return Err(Error::Runtime(format!(
                "a {} point has {} coordinates, not {}",
                crs.name(),
                crs.dims(),
                coords.len()
            )));
        }
        if let Some(c) = coords.iter().find(|c| !c.is_finite()) {
            return Err(Error::Runtime(format!("point coordinate {} is not finite", format_float(*c))));
        }
        if crs.is_geographic() {
            if !(-180.0..=180.0).contains(&coords[0]) {
                return Err(Error::Runtime(format!(
                    "longitude {} is outside [-180, 180]",
                    format_float(coords[0])
                )));
            }
            if !(-90.0..=90.0).contains(&coords[1]) {
                return Err(Error::Runtime(format!(
                    "latitude {} is outside [-90, 90]",
                    format_float(coords[1])
                )));
            }
        }
        let mut c = [0.0; 3];
        for (d, s) in c.iter_mut().zip(coords) {
            *d = canonical(*s);
        }
        Ok(Point { crs, coords: c })
    }

    pub fn crs(&self) -> Crs {
        self.crs
    }

    /// The coordinates: 2 or 3 of them.
    pub fn coords(&self) -> &[f64] {
        &self.coords[..self.crs.dims()]
    }

    pub fn x(&self) -> f64 {
        self.coords[0]
    }

    pub fn y(&self) -> f64 {
        self.coords[1]
    }

    pub fn z(&self) -> Option<f64> {
        (self.crs.dims() == 3).then_some(self.coords[2])
    }

    /// `point({…})`: `None` when a coordinate (or the map) is null.
    ///
    /// Takes `x`, `y` (and `z`) or `longitude`, `latitude` (and `height`), plus an
    /// optional `crs` name or `srid`. Without either, `x`/`y` make a Cartesian point and
    /// `longitude`/`latitude` a WGS-84 one; a third coordinate makes it 3D.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Option<Point>> {
        let bad = |m: String| Err(Error::Runtime(m));
        let mut crs_arg: Option<Crs> = None;
        let (mut x, mut y, mut z) = (None, None, None);
        let (mut cartesian_keys, mut geographic_keys) = (false, false);
        for (key, v) in map {
            let k = key.to_ascii_lowercase();
            if v.is_null() {
                return Ok(None);
            }
            match k.as_str() {
                "crs" => {
                    let Value::String(name) = v else {
                        return Err(Error::Type(format!("point crs must be a STRING, not {}", v.type_name())));
                    };
                    if crs_arg.is_some() {
                        return bad("point() takes either `crs` or `srid`, not both".into());
                    }
                    crs_arg = Some(Crs::from_name(name).ok_or_else(|| {
                        Error::Runtime(format!(
                            "unknown coordinate reference system '{name}' (expected cartesian, \
                             cartesian-3d, wgs-84 or wgs-84-3d)"
                        ))
                    })?);
                }
                "srid" => {
                    let Value::Int(srid) = v else {
                        return Err(Error::Type(format!("point srid must be an INTEGER, not {}", v.type_name())));
                    };
                    if crs_arg.is_some() {
                        return bad("point() takes either `crs` or `srid`, not both".into());
                    }
                    crs_arg = Some(Crs::from_srid(*srid).ok_or_else(|| {
                        Error::Runtime(format!(
                            "unknown SRID {srid} (expected 7203, 9157, 4326 or 4979)"
                        ))
                    })?);
                }
                "x" | "y" | "z" | "longitude" | "latitude" | "height" => {
                    let f = match v {
                        Value::Int(i) => *i as f64,
                        Value::Float(f) => *f,
                        o => {
                            return Err(Error::Type(format!(
                                "point coordinate `{key}` must be a number, not {}",
                                o.type_name()
                            )));
                        }
                    };
                    let slot = match k.as_str() {
                        "x" | "longitude" => &mut x,
                        "y" | "latitude" => &mut y,
                        _ => &mut z,
                    };
                    if slot.is_some() {
                        return bad(format!("point() got coordinate `{key}` twice (under another name)"));
                    }
                    *slot = Some(f);
                    match k.as_str() {
                        "x" | "y" => cartesian_keys = true,
                        "longitude" | "latitude" => geographic_keys = true,
                        _ => {}
                    }
                }
                _ => return bad(format!("point() does not take `{key}`")),
            }
        }
        if cartesian_keys && geographic_keys {
            return bad("point() takes `x`/`y` or `longitude`/`latitude`, not both".into());
        }
        let (Some(x), Some(y)) = (x, y) else {
            return bad("point() needs `x` and `y`, or `longitude` and `latitude`".into());
        };
        let dims = if z.is_some() { 3 } else { 2 };
        let crs = match crs_arg {
            Some(c) => {
                if c.dims() != dims {
                    return bad(format!(
                        "a {} point needs {} coordinates, got {dims}",
                        c.name(),
                        c.dims()
                    ));
                }
                if geographic_keys && !c.is_geographic() {
                    return bad(format!("a {} point takes `x` and `y`, not `longitude` and `latitude`", c.name()));
                }
                c
            }
            None if geographic_keys => Crs::Wgs84.with_dims(dims),
            None => Crs::Cartesian.with_dims(dims),
        };
        if map.keys().any(|k| k.eq_ignore_ascii_case("height")) && !crs.is_geographic() {
            return bad(format!("a {} point takes `z`, not `height`", crs.name()));
        }
        let coords = [x, y, z.unwrap_or(0.0)];
        Point::new(crs, &coords[..dims]).map(Some)
    }

    /// A field: `x`, `y`, `z`, `longitude`, `latitude`, `height`, `crs`, `srid`.
    /// `None` when the point has no such field (`z` of a 2D point, `latitude` of a
    /// Cartesian one).
    pub fn field(&self, key: &str) -> Option<Value> {
        let geo = self.crs.is_geographic();
        Some(match key.to_ascii_lowercase().as_str() {
            "x" => Value::Float(self.x()),
            "y" => Value::Float(self.y()),
            "z" => Value::Float(self.z()?),
            "longitude" if geo => Value::Float(self.x()),
            "latitude" if geo => Value::Float(self.y()),
            "height" if geo => Value::Float(self.z()?),
            "crs" => Value::String(self.crs.name().to_string()),
            "srid" => Value::Int(self.crs.srid()),
            _ => return None,
        })
    }

    /// The distance between two points of one CRS (`None` for different ones): straight
    /// line for Cartesian points; for geographic points the haversine great-circle
    /// distance in metres on a sphere of [`EARTH_RADIUS_METERS`], combined with the
    /// height difference by Pythagoras in 3D.
    pub fn distance(&self, other: &Point) -> Option<f64> {
        if self.crs != other.crs {
            return None;
        }
        if !self.crs.is_geographic() {
            let s: f64 = self.coords().iter().zip(other.coords()).map(|(a, b)| (a - b) * (a - b)).sum();
            return Some(s.sqrt());
        }
        let (lat1, lat2) = (self.y().to_radians(), other.y().to_radians());
        let dlat = lat2 - lat1;
        let dlon = (other.x() - self.x()).to_radians();
        let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
        let c = 2.0 * a.sqrt().atan2((1.0 - a).max(0.0).sqrt());
        let d = EARTH_RADIUS_METERS * c;
        Some(match (self.z(), other.z()) {
            (Some(h1), Some(h2)) => d.hypot(h2 - h1),
            _ => d,
        })
    }

    /// Whether this point lies in the box from `lower_left` to `upper_right` (bounds
    /// included); `None` if the three points are not of one CRS. A geographic box whose
    /// lower-left longitude is east of its upper-right one crosses the 180th meridian.
    pub fn within_bbox(&self, lower_left: &Point, upper_right: &Point) -> Option<bool> {
        if self.crs != lower_left.crs || self.crs != upper_right.crs {
            return None;
        }
        let (p, lo, hi) = (self.coords(), lower_left.coords(), upper_right.coords());
        let inside = |i: usize| lo[i] <= p[i] && p[i] <= hi[i];
        let x_inside = if self.crs.is_geographic() && lo[0] > hi[0] {
            p[0] >= lo[0] || p[0] <= hi[0]
        } else {
            inside(0)
        };
        Some(x_inside && (1..p.len()).all(inside))
    }

    /// The order `ORDER BY` uses: by CRS, then coordinates.
    pub fn order_cmp(&self, other: &Point) -> Ordering {
        self.crs.cmp(&other.crs).then_with(|| {
            for (a, b) in self.coords().iter().zip(other.coords()) {
                match a.total_cmp(b) {
                    Ordering::Equal => {}
                    o => return o,
                }
            }
            Ordering::Equal
        })
    }
}

impl fmt::Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "point({{x: {}, y: {}", format_float(self.x()), format_float(self.y()))?;
        if let Some(z) = self.z() {
            write!(f, ", z: {}", format_float(z))?;
        }
        write!(f, ", crs: '{}'}})", self.crs.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
        entries.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    fn point(entries: &[(&str, Value)]) -> Result<Option<Point>> {
        Point::from_map(&map(entries))
    }

    #[test]
    fn construct_and_infer_crs() {
        let p = point(&[("x", Value::Int(1)), ("y", Value::Float(2.5))]).unwrap().unwrap();
        assert_eq!(p.crs(), Crs::Cartesian);
        assert_eq!(p.to_string(), "point({x: 1.0, y: 2.5, crs: 'cartesian'})");
        let p = point(&[("longitude", Value::Int(12)), ("latitude", Value::Int(56)), ("height", Value::Int(100))])
            .unwrap()
            .unwrap();
        assert_eq!(p.crs(), Crs::Wgs84_3D);
        assert_eq!(p.field("height"), Some(Value::Float(100.0)));
        assert_eq!(p.field("srid"), Some(Value::Int(4979)));
        let p = point(&[("x", Value::Int(1)), ("y", Value::Int(2)), ("srid", Value::Int(4326))]).unwrap().unwrap();
        assert_eq!(p.crs(), Crs::Wgs84);
        assert_eq!(p.field("latitude"), Some(Value::Float(2.0)));
        let p = point(&[("x", Value::Int(1)), ("y", Value::Int(2)), ("z", Value::Int(3)), ("crs", Value::from("Cartesian-3D"))])
            .unwrap()
            .unwrap();
        assert_eq!(p.crs(), Crs::Cartesian3D);
        assert_eq!(p.field("latitude"), None);
        assert_eq!(point(&[("x", Value::Null), ("y", Value::Int(2))]).unwrap(), None);
    }

    #[test]
    fn construct_errors() {
        for bad in [
            vec![("x", Value::Int(1))],
            vec![("x", Value::Int(1)), ("latitude", Value::Int(1))],
            vec![("x", Value::Int(1)), ("y", Value::Int(1)), ("crs", Value::from("cartesian-3d"))],
            vec![("x", Value::Int(1)), ("y", Value::Int(1)), ("crs", Value::from("mars"))],
            vec![("x", Value::Int(1)), ("y", Value::Int(1)), ("srid", Value::Int(1))],
            vec![("x", Value::Int(1)), ("y", Value::Int(1)), ("srid", Value::Int(7203)), ("crs", Value::from("cartesian"))],
            vec![("longitude", Value::Int(1)), ("latitude", Value::Int(91))],
            vec![("longitude", Value::Int(181)), ("latitude", Value::Int(0))],
            vec![("x", Value::Int(1)), ("y", Value::Float(f64::NAN))],
            vec![("x", Value::Int(1)), ("y", Value::Int(1)), ("w", Value::Int(1))],
            vec![("x", Value::Int(1)), ("y", Value::from("2"))],
            vec![("x", Value::Int(1)), ("y", Value::Int(1)), ("height", Value::Int(1))],
        ] {
            assert!(point(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn distances() {
        let a = Point::new(Crs::Cartesian, &[0.0, 0.0]).unwrap();
        let b = Point::new(Crs::Cartesian, &[3.0, 4.0]).unwrap();
        assert_eq!(a.distance(&b), Some(5.0));
        let c = Point::new(Crs::Cartesian3D, &[0.0, 0.0, 0.0]).unwrap();
        assert_eq!(a.distance(&c), None);
        // One degree of longitude at the equator.
        let p = Point::new(Crs::Wgs84, &[0.0, 0.0]).unwrap();
        let q = Point::new(Crs::Wgs84, &[1.0, 0.0]).unwrap();
        let d = p.distance(&q).unwrap();
        assert!((d - EARTH_RADIUS_METERS * std::f64::consts::PI / 180.0).abs() < 1e-6, "{d}");
        // Across the antimeridian.
        let r = Point::new(Crs::Wgs84, &[179.5, 0.0]).unwrap();
        let s = Point::new(Crs::Wgs84, &[-179.5, 0.0]).unwrap();
        assert!((r.distance(&s).unwrap() - d).abs() < 1e-6);
        let p3 = Point::new(Crs::Wgs84_3D, &[0.0, 0.0, 0.0]).unwrap();
        let q3 = Point::new(Crs::Wgs84_3D, &[0.0, 0.0, 30.0]).unwrap();
        assert_eq!(p3.distance(&q3), Some(30.0));
    }

    #[test]
    fn bbox() {
        let p = |x, y| Point::new(Crs::Wgs84, &[x, y]).unwrap();
        assert_eq!(p(1.0, 1.0).within_bbox(&p(0.0, 0.0), &p(2.0, 2.0)), Some(true));
        assert_eq!(p(2.0, 2.0).within_bbox(&p(0.0, 0.0), &p(2.0, 2.0)), Some(true));
        assert_eq!(p(3.0, 1.0).within_bbox(&p(0.0, 0.0), &p(2.0, 2.0)), Some(false));
        // Crossing the 180th meridian.
        assert_eq!(p(179.0, 1.0).within_bbox(&p(170.0, 0.0), &p(-170.0, 2.0)), Some(true));
        assert_eq!(p(-175.0, 1.0).within_bbox(&p(170.0, 0.0), &p(-170.0, 2.0)), Some(true));
        assert_eq!(p(0.0, 1.0).within_bbox(&p(170.0, 0.0), &p(-170.0, 2.0)), Some(false));
        let c = Point::new(Crs::Cartesian, &[1.0, 1.0]).unwrap();
        assert_eq!(c.within_bbox(&p(0.0, 0.0), &p(2.0, 2.0)), None);
    }

    #[test]
    fn equality_ignores_negative_zero() {
        let a = Point::new(Crs::Cartesian, &[-0.0, 1.0]).unwrap();
        let b = Point::new(Crs::Cartesian, &[0.0, 1.0]).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.order_cmp(&b), Ordering::Equal);
        assert_eq!(a.coords()[0].to_bits(), 0.0f64.to_bits());
    }
}
