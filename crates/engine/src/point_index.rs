//! Point indexes: the points of one property of the nodes with a label (or the
//! relationships of a type), kept in memory per committed state like the full-text and
//! vector indexes (see [`IndexCache`]), and used by index seeks for
//! `point.distance(n.p, c) < r` (or `<=`) and `point.withinBBox(n.p, lowerLeft,
//! upperRight)`.
//!
//! Each coordinate reference system has a uniform grid over its first two coordinates
//! (longitude and latitude for WGS-84), with cells sized to the points when it is built.
//! A seek returns the entities in the cells its box overlaps: a superset of the matches,
//! which the `WHERE` then checks exactly.

use std::collections::{BTreeMap, HashMap};

use graphbuff_core::spatial::EARTH_RADIUS_METERS;
use graphbuff_core::{Changes, Crs, Error, GraphStore, IndexDef, IndexEntity, IndexKind, NodeId, Point, RelId, Result, Value};

use crate::fulltext::for_each_entity;
use crate::index_cache::{Cached, IndexCache};

/// The settings `CREATE POINT INDEX … OPTIONS {indexConfig: {…}}` takes: Neo4j's
/// `spatial.<crs>.min` and `spatial.<crs>.max` bounds (lists of numbers), accepted and
/// ignored, since the grid sizes itself to the points.
pub fn point_kind(config: &BTreeMap<String, Value>) -> Result<IndexKind> {
    for (k, v) in config {
        let Some(rest) = k.strip_prefix("spatial.") else {
            return Err(Error::Semantic(format!("unknown index setting `{k}` for a point index")));
        };
        let crs = rest.strip_suffix(".min").or_else(|| rest.strip_suffix(".max"));
        let Some(crs) = crs.filter(|c| Crs::ALL.iter().any(|x| x.name() == *c)) else {
            return Err(Error::Semantic(format!("unknown point index setting `{k}`")));
        };
        let dims = Crs::ALL.iter().find(|x| x.name() == crs).map_or(2, |c| c.dims());
        match v {
            Value::List(items)
                if items.len() == dims && items.iter().all(|x| matches!(x, Value::Int(_) | Value::Float(_))) => {}
            o => {
                return Err(Error::Semantic(format!("`{k}` needs a list of {dims} numbers, not {o}")));
            }
        }
    }
    Ok(IndexKind::Point)
}

/// The points of one coordinate reference system.
#[derive(Clone, Default)]
struct Layer {
    /// The side of a grid cell.
    cell: f64,
    /// The entities in each cell.
    cells: HashMap<(i64, i64), Vec<u64>>,
    /// The first two coordinates of each entity.
    points: HashMap<u64, (f64, f64)>,
    /// The number of points when the cells were laid out.
    laid_out: usize,
}

impl Layer {
    fn key(&self, x: f64, y: f64) -> (i64, i64) {
        // Float-to-int casts saturate, so far-out coordinates land in the edge cells.
        ((x / self.cell).floor() as i64, (y / self.cell).floor() as i64)
    }

    fn insert(&mut self, id: u64, x: f64, y: f64) {
        self.points.insert(id, (x, y));
        if self.cell == 0.0 || self.points.len() > 4 * self.laid_out + 64 {
            self.lay_out();
        } else {
            let k = self.key(x, y);
            self.cells.entry(k).or_default().push(id);
        }
    }

    fn remove(&mut self, id: u64) {
        let Some((x, y)) = self.points.remove(&id) else { return };
        let k = self.key(x, y);
        if let Some(c) = self.cells.get_mut(&k) {
            c.retain(|&e| e != id);
            if c.is_empty() {
                self.cells.remove(&k);
            }
        }
    }

    /// Sizes the cells to the points (about one per cell, were they spread evenly) and
    /// files the points into them.
    fn lay_out(&mut self) {
        let n = self.points.len();
        let (mut x0, mut x1, mut y0, mut y1) = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
        for &(x, y) in self.points.values() {
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
        let extent = (x1 - x0).max(y1 - y0);
        let cell = extent / (n as f64).sqrt().max(1.0);
        self.cell = if cell.is_finite() && cell > 0.0 { cell } else { 1.0 };
        self.laid_out = n;
        self.cells.clear();
        let entries: Vec<(u64, (f64, f64))> = self.points.iter().map(|(&id, &p)| (id, p)).collect();
        for (id, (x, y)) in entries {
            let k = self.key(x, y);
            self.cells.entry(k).or_default().push(id);
        }
    }

    /// The entities whose point lies in `[x0, x1] × [y0, y1]`.
    fn in_box(&self, (x0, x1): (f64, f64), (y0, y1): (f64, f64), out: &mut Vec<u64>) {
        if self.points.is_empty() || x0.is_nan() || y0.is_nan() || x1.is_nan() || y1.is_nan() || x0 > x1 || y0 > y1 {
            return;
        }
        let inside = |&(x, y): &(f64, f64)| x0 <= x && x <= x1 && y0 <= y && y <= y1;
        let (lo, hi) = (self.key(x0, y0), self.key(x1, y1));
        let span = |a: i64, b: i64| (b as i128 - a as i128 + 1) as u128;
        let cells = span(lo.0, hi.0).saturating_mul(span(lo.1, hi.1));
        if cells > self.cells.len() as u128 {
            // A box over more cells than are in use: look at every point instead.
            out.extend(self.points.iter().filter(|(_, p)| inside(p)).map(|(&id, _)| id));
            return;
        }
        for cx in lo.0..=hi.0 {
            for cy in lo.1..=hi.1 {
                if let Some(ids) = self.cells.get(&(cx, cy)) {
                    out.extend(ids.iter().filter(|id| self.points.get(id).is_some_and(inside)));
                }
            }
        }
    }
}

/// A point index's points in one committed state.
#[derive(Clone)]
pub(crate) struct PointIndex {
    def: IndexDef,
    layers: HashMap<Crs, Layer>,
    /// The coordinate reference system of each indexed entity's point.
    crs_of: HashMap<u64, Crs>,
}

impl PointIndex {
    /// The indexed point of entity `id` in `store`, if it is covered and has one.
    fn current(&self, store: &dyn GraphStore, id: u64) -> Result<Option<Point>> {
        let key = &self.def.properties[0];
        let value = match self.def.entity {
            IndexEntity::Node => {
                let n = NodeId(id);
                if !store.node_exists(n)? || !store.node_has_label(n, &self.def.label)? {
                    return Ok(None);
                }
                store.node_property(n, key)?
            }
            IndexEntity::Relationship => {
                let r = RelId(id);
                if !store.relationship_exists(r)? || store.relationship_type(r)?.as_deref() != Some(self.def.label.as_str())
                {
                    return Ok(None);
                }
                store.relationship_property(r, key)?
            }
        };
        Ok(match value {
            Some(Value::Point(p)) => Some(p),
            _ => None,
        })
    }

    fn add(&mut self, id: u64, p: Point) {
        self.crs_of.insert(id, p.crs());
        self.layers.entry(p.crs()).or_default().insert(id, p.x(), p.y());
    }

    fn remove(&mut self, id: u64) {
        if let Some(crs) = self.crs_of.remove(&id)
            && let Some(layer) = self.layers.get_mut(&crs)
        {
            layer.remove(id);
        }
    }

    /// The entities whose point may lie within `radius` of `center` (a superset of
    /// those `point.distance` puts there), sorted.
    pub(crate) fn near(&self, center: &Point, radius: f64) -> Vec<u64> {
        let mut out = Vec::new();
        let Some(layer) = self.layers.get(&center.crs()) else {
            return out;
        };
        if radius.is_nan() || radius < 0.0 {
            return out;
        }
        let (cx, cy) = (center.x(), center.y());
        if !center.crs().is_geographic() {
            // Some slack for rounding in the distance's square root.
            let r = radius + 1e-9 * (1.0 + radius + cx.abs() + cy.abs());
            layer.in_box((cx - r, cx + r), (cy - r, cy + r), &mut out);
        } else {
            // The box around the spherical cap of angular radius `theta` (a slightly
            // larger one, for rounding); the height of a 3D point only adds distance.
            let theta = radius / EARTH_RADIUS_METERS * (1.0 + 1e-9) + 1e-12;
            let dlat = theta.to_degrees();
            let (lat0, lat1) = (cy - dlat, cy + dlat);
            let full = (-180.0, 180.0);
            if theta >= std::f64::consts::PI || lat0 <= -90.0 || lat1 >= 90.0 {
                // A cap that reaches a pole: every longitude.
                layer.in_box(full, (lat0.max(-90.0), lat1.min(90.0)), &mut out);
            } else {
                let s = theta.sin() / cy.to_radians().cos();
                if s >= 1.0 {
                    layer.in_box(full, (lat0, lat1), &mut out);
                } else {
                    let dlon = s.asin().to_degrees() * (1.0 + 1e-9) + 1e-9;
                    for lon in lon_ranges(cx - dlon, cx + dlon) {
                        layer.in_box(lon, (lat0, lat1), &mut out);
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The entities whose point may lie in the box from `lower_left` to `upper_right`
    /// (a superset of those `point.withinBBox` accepts), sorted.
    pub(crate) fn in_bbox(&self, lower_left: &Point, upper_right: &Point) -> Vec<u64> {
        let mut out = Vec::new();
        if lower_left.crs() != upper_right.crs() {
            return out;
        }
        let Some(layer) = self.layers.get(&lower_left.crs()) else {
            return out;
        };
        let ys = (lower_left.y(), upper_right.y());
        let (x0, x1) = (lower_left.x(), upper_right.x());
        if lower_left.crs().is_geographic() && x0 > x1 {
            // Across the 180th meridian.
            layer.in_box((x0, 180.0), ys, &mut out);
            layer.in_box((-180.0, x1), ys, &mut out);
        } else {
            layer.in_box((x0, x1), ys, &mut out);
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Longitude ranges within `[-180, 180]` covering `[lo, hi]` (a range wider than the
/// circle is split at the 180th meridian).
fn lon_ranges(lo: f64, hi: f64) -> Vec<(f64, f64)> {
    if hi - lo >= 360.0 {
        vec![(-180.0, 180.0)]
    } else if lo < -180.0 {
        vec![(lo + 360.0, 180.0), (-180.0, hi)]
    } else if hi > 180.0 {
        vec![(lo, 180.0), (-180.0, hi - 360.0)]
    } else {
        vec![(lo, hi)]
    }
}

impl Cached for PointIndex {
    fn def(&self) -> &IndexDef {
        &self.def
    }

    fn build(store: &dyn GraphStore, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<PointIndex> {
        let mut ix = PointIndex {
            def: def.clone(),
            layers: HashMap::new(),
            crs_of: HashMap::new(),
        };
        for_each_entity(store, def, interrupt, |id| {
            if let Some(p) = ix.current(store, id)? {
                ix.crs_of.insert(id, p.crs());
                ix.layers.entry(p.crs()).or_default().points.insert(id, (p.x(), p.y()));
            }
            Ok(())
        })?;
        for layer in ix.layers.values_mut() {
            layer.lay_out();
        }
        Ok(ix)
    }

    fn apply(&mut self, store: &dyn GraphStore, changes: &Changes, interrupt: &dyn Fn() -> Result<()>) -> Result<()> {
        let changed = match self.def.entity {
            IndexEntity::Node => &changes.nodes,
            IndexEntity::Relationship => &changes.rels,
        };
        for (n, &id) in changed.iter().enumerate() {
            if n % 4096 == 4095 {
                interrupt()?;
            }
            let p = self.current(store, id)?;
            self.remove(id);
            if let Some(p) = p {
                self.add(id, p);
            }
        }
        Ok(())
    }
}

/// The point index contents of a committed state.
pub(crate) type PointCache = IndexCache<PointIndex>;

#[cfg(test)]
mod tests {
    use super::*;
    use graphbuff_core::MemoryGraph;

    fn p(crs: Crs, c: &[f64]) -> Point {
        Point::new(crs, c).unwrap()
    }

    /// An index over `points` (entity ids are their positions), and brute force.
    fn index(points: &[Point]) -> PointIndex {
        let def = IndexDef::point("ix", IndexEntity::Node, "L", "p");
        let mut ix = PointIndex::build(&MemoryGraph::new(), &def, &|| Ok(())).unwrap();
        for (i, q) in points.iter().enumerate() {
            ix.add(i as u64, *q);
        }
        ix
    }

    fn brute_near(points: &[Point], c: &Point, r: f64) -> Vec<u64> {
        (0..points.len() as u64).filter(|&i| points[i as usize].distance(c).is_some_and(|d| d <= r)).collect()
    }

    fn brute_bbox(points: &[Point], lo: &Point, hi: &Point) -> Vec<u64> {
        (0..points.len() as u64).filter(|&i| points[i as usize].within_bbox(lo, hi) == Some(true)).collect()
    }

    fn subset(a: &[u64], b: &[u64]) -> bool {
        a.iter().all(|x| b.binary_search(x).is_ok())
    }

    /// A deterministic pseudo-random sequence in [0, 1).
    fn rng(seed: &mut u64) -> f64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        (*seed >> 11) as f64 / (1u64 << 53) as f64
    }

    #[test]
    fn cartesian_seeks_find_every_match() {
        let mut seed = 42;
        let pts: Vec<Point> = (0..3000)
            .map(|_| p(Crs::Cartesian, &[rng(&mut seed) * 1000.0 - 500.0, rng(&mut seed) * 10.0]))
            .collect();
        let ix = index(&pts);
        for _ in 0..200 {
            let c = p(Crs::Cartesian, &[rng(&mut seed) * 1200.0 - 600.0, rng(&mut seed) * 12.0 - 1.0]);
            let r = rng(&mut seed) * 50.0;
            let found = ix.near(&c, r);
            assert!(subset(&brute_near(&pts, &c, r), &found));
            assert!(found.len() <= pts.len());
            let hi = p(Crs::Cartesian, &[c.x() + r, c.y() + r / 10.0]);
            assert!(subset(&brute_bbox(&pts, &c, &hi), &ix.in_bbox(&c, &hi)));
        }
        // A point exactly on the circle and on the box edges.
        let ix = index(&[p(Crs::Cartesian, &[3.0, 4.0])]);
        assert_eq!(ix.near(&p(Crs::Cartesian, &[0.0, 0.0]), 5.0), vec![0]);
        assert_eq!(ix.in_bbox(&p(Crs::Cartesian, &[3.0, 4.0]), &p(Crs::Cartesian, &[3.0, 4.0])), vec![0]);
        assert!(ix.in_bbox(&p(Crs::Cartesian, &[4.0, 0.0]), &p(Crs::Cartesian, &[0.0, 9.0])).is_empty());
        // Another CRS finds nothing.
        assert!(ix.near(&p(Crs::Wgs84, &[3.0, 4.0]), 10.0).is_empty());
        assert!(ix.near(&p(Crs::Cartesian, &[3.0, 4.0]), -1.0).is_empty());
    }

    #[test]
    fn geographic_seeks_find_every_match() {
        let mut seed = 7;
        let pts: Vec<Point> = (0..3000)
            .map(|_| p(Crs::Wgs84, &[rng(&mut seed) * 360.0 - 180.0, rng(&mut seed) * 180.0 - 90.0]))
            .collect();
        let ix = index(&pts);
        let mut centers: Vec<Point> = (0..200)
            .map(|_| p(Crs::Wgs84, &[rng(&mut seed) * 360.0 - 180.0, rng(&mut seed) * 180.0 - 90.0]))
            .collect();
        // Near the poles and the 180th meridian.
        centers.extend([[179.9, 0.0], [-179.9, 10.0], [0.0, 89.9], [45.0, -89.95], [180.0, 0.0]].map(|c| p(Crs::Wgs84, &c)));
        for c in &centers {
            for r in [1_000.0, 100_000.0, 1_500_000.0, 25_000_000.0] {
                let found = ix.near(c, r);
                assert!(subset(&brute_near(&pts, c, r), &found), "{c:?} {r}");
            }
        }
        // A box across the 180th meridian.
        let (lo, hi) = (p(Crs::Wgs84, &[170.0, -20.0]), p(Crs::Wgs84, &[-170.0, 20.0]));
        let found = ix.in_bbox(&lo, &hi);
        assert_eq!(found, brute_bbox(&pts, &lo, &hi));
        assert!(!found.is_empty());
    }

    #[test]
    fn updates_keep_the_grid_right() {
        let mut ix = index(&[]);
        let mut seed = 3;
        let mut pts = Vec::new();
        // Grows far beyond the first layout, then moves and removes points.
        for i in 0..5000u64 {
            let q = p(Crs::Cartesian3D, &[rng(&mut seed) * 1e6, rng(&mut seed) * 1e6, 5.0]);
            ix.add(i, q);
            pts.push(q);
        }
        for i in (0..5000u64).step_by(3) {
            ix.remove(i);
            let q = p(Crs::Cartesian3D, &[rng(&mut seed) * 10.0, rng(&mut seed) * 10.0, 0.0]);
            ix.add(i, q);
            pts[i as usize] = q;
        }
        let c = p(Crs::Cartesian3D, &[5.0, 5.0, 0.0]);
        let found = ix.near(&c, 3.0);
        assert!(subset(&brute_near(&pts, &c, 3.0), &found));
        assert!(!found.is_empty());
        let big = p(Crs::Cartesian3D, &[5e5, 5e5, 5.0]);
        assert!(subset(&brute_near(&pts, &big, 2e5), &ix.near(&big, 2e5)));
    }

    #[test]
    fn options() {
        let cfg = |k: &str, v: Value| BTreeMap::from([(k.to_string(), v)]);
        let nums = |n: &[f64]| Value::List(n.iter().map(|x| Value::Float(*x)).collect());
        assert_eq!(point_kind(&BTreeMap::new()).unwrap(), IndexKind::Point);
        assert!(point_kind(&cfg("spatial.cartesian.min", nums(&[-100.0, -100.0]))).is_ok());
        assert!(point_kind(&cfg("spatial.wgs-84-3d.max", nums(&[180.0, 90.0, 1e6]))).is_ok());
        assert!(point_kind(&cfg("spatial.cartesian.min", nums(&[1.0]))).is_err());
        assert!(point_kind(&cfg("spatial.mars.min", nums(&[1.0, 2.0]))).is_err());
        assert!(point_kind(&cfg("vector.dimensions", Value::Int(3))).is_err());
    }
}
