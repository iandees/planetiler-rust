//! Port of `geo/PolygonIndex.java`: index to efficiently query which polygons contain a point.
//!
//! Writes and reads are thread-safe, but all writes must occur before reads.

use std::sync::OnceLock;

use geo::{BoundingRect, Contains, Distance, Euclidean, Intersects};
use geo_types::{Geometry, Point, Polygon};
use parking_lot::Mutex;
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{AABB, RTree};

use crate::geo::envelope::Envelope;

type Entry = GeomWithData<Rectangle<[f64; 2]>, usize>;
type Built<T> = (RTree<Entry>, Vec<(Polygon<f64>, T)>);

pub struct PolygonIndex<T> {
    items: Mutex<Vec<(Polygon<f64>, T)>>,
    built: OnceLock<Built<T>>,
}

impl<T: Clone> Default for PolygonIndex<T> {
    fn default() -> Self {
        Self::create()
    }
}

impl<T: Clone> PolygonIndex<T> {
    pub fn create() -> Self {
        PolygonIndex { items: Mutex::new(Vec::new()), built: OnceLock::new() }
    }

    fn index(&self) -> &Built<T> {
        self.built.get_or_init(|| {
            let items = std::mem::take(&mut *self.items.lock());
            let entries = items
                .iter()
                .enumerate()
                .filter_map(|(i, (poly, _))| {
                    poly.bounding_rect().map(|r| {
                        GeomWithData::new(Rectangle::from_corners([r.min().x, r.min().y], [r.max().x, r.max().y]), i)
                    })
                })
                .collect();
            (RTree::bulk_load(entries), items)
        })
    }

    fn query(&self, env: &Envelope) -> Vec<usize> {
        if env.is_null() {
            return Vec::new();
        }
        let (tree, _) = self.index();
        let aabb = AABB::from_corners([env.min_x, env.min_y], [env.max_x, env.max_y]);
        let mut result: Vec<usize> = tree.locate_in_envelope_intersecting(aabb).map(|e| e.data).collect();
        // keep insertion order for deterministic results
        result.sort_unstable();
        result
    }

    /// Returns the data associated with the first polygon containing `point`.
    pub fn get_only_containing(&self, point: &Point<f64>) -> Option<T> {
        self.get_containing(point).into_iter().next()
    }

    /// Returns the data associated with all polygons containing `point`.
    pub fn get_containing(&self, point: &Point<f64>) -> Vec<T> {
        let items = self.query(&Envelope::of_point(point.x(), point.y()));
        self.post_filter_containing(point, &items)
    }

    /// Returns the data associated with all polygons intersecting `geom`.
    pub fn get_intersecting(&self, geom: &Geometry<f64>) -> Vec<T> {
        let items = self.query(&Envelope::of_geometry(geom));
        let (_, all) = self.index();
        items.into_iter().filter(|i| all[*i].0.intersects(geom)).map(|i| all[i].1.clone()).collect()
    }

    fn post_filter_containing(&self, point: &Point<f64>, items: &[usize]) -> Vec<T> {
        let (_, all) = self.index();
        items.iter().filter(|i| all[**i].0.contains(point)).map(|i| all[*i].1.clone()).collect()
    }

    /// Returns the data associated with either the polygons that contain `point` or if none are found then the
    /// nearest polygon to `point` with an envelope that contains point.
    pub fn get_containing_or_nearest(&self, point: &Point<f64>) -> Vec<T> {
        let items = self.query(&Envelope::of_point(point.x(), point.y()));
        let (_, all) = self.index();
        // optimization: if there's only one then skip checking contains/distance
        if items.len() == 1 {
            return vec![all[items[0]].1.clone()];
        }
        let mut result = self.post_filter_containing(point, &items);
        if result.is_empty() {
            let mut nearest = f64::MAX;
            let mut nearest_value = None;
            for i in items {
                let distance = Euclidean.distance(&all[i].0, point);
                if distance < nearest {
                    nearest = distance;
                    nearest_value = Some(all[i].1.clone());
                }
            }
            result.extend(nearest_value);
        }
        result
    }

    /// Returns the data associated with a polygon that contains `point` or nearest polygon if none are found.
    pub fn get(&self, point: &Point<f64>) -> Option<T> {
        self.get_containing_or_nearest(point).into_iter().next()
    }

    /// Indexes `item` for all polygons contained in `geom`.
    pub fn put(&self, geom: &Geometry<f64>, item: T) {
        debug_assert!(self.built.get().is_none(), "PolygonIndex written to after reads started");
        match geom {
            Geometry::Polygon(poly) => self.items.lock().push((poly.clone(), item)),
            Geometry::MultiPolygon(mp) => {
                for p in &mp.0 {
                    self.items.lock().push((p.clone(), item.clone()));
                }
            }
            Geometry::GeometryCollection(gc) => {
                for g in &gc.0 {
                    self.put(g, item.clone());
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::*;

    fn sorted(mut v: Vec<i32>) -> Vec<i32> {
        v.sort();
        v
    }

    #[test]
    fn empty() {
        let index: PolygonIndex<i32> = PolygonIndex::create();
        assert!(index.get_containing(&new_point(0.5, 0.5)).is_empty());
        assert!(index.get_containing_or_nearest(&new_point(0.5, 0.5)).is_empty());
    }

    #[test]
    fn single() {
        let index = PolygonIndex::create();
        index.put(&rectangle(0.0, 1.0).into(), 1);
        assert_eq!(vec![1], index.get_containing(&new_point(0.5, 0.5)));
        assert_eq!(vec![1], index.get_intersecting(&new_point(0.5, 0.5).into()));
        assert_eq!(vec![1], index.get_intersecting(&rectangle(1.0, 2.0).into()));
        assert_eq!(vec![1], index.get_containing_or_nearest(&new_point(0.5, 0.5)));

        assert!(index.get_containing(&new_point(1.5, 1.5)).is_empty());
        assert!(index.get_containing_or_nearest(&new_point(1.5, 1.5)).is_empty());
        assert!(index.get_intersecting(&rectangle(2.0, 3.0).into()).is_empty());
    }

    #[test]
    fn multiple_identical() {
        let index = PolygonIndex::create();
        index.put(&rectangle(0.0, 1.0).into(), 1);
        index.put(&rectangle(0.0, 1.0).into(), 2);
        assert_eq!(vec![1, 2], sorted(index.get_containing(&new_point(0.5, 0.5))));
        assert_eq!(vec![1, 2], sorted(index.get_containing_or_nearest(&new_point(0.5, 0.5))));
        assert_eq!(vec![1, 2], sorted(index.get_intersecting(&rectangle(0.5, 1.5).into())));
        assert_eq!(vec![1, 2], sorted(index.get_intersecting(&rectangle(1.0, 2.0).into())));
        assert!(index.get_intersecting(&rectangle(2.0, 3.0).into()).is_empty());
    }

    #[test]
    fn multiple_different() {
        let index = PolygonIndex::create();
        index.put(&rectangle(0.25, 1.0).into(), 1);
        // rectangle(0, 1).difference(rectangle(0, 0.5))
        index.put(&new_polygon(&[0.5, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.5, 0.5, 0.5, 0.5, 0.0]).into(), 2);

        assert!(index.get_containing(&new_point(0.0, 0.0)).is_empty());
        assert_eq!(vec![2], index.get_containing_or_nearest(&new_point(0.0, 0.0)));

        assert_eq!(vec![1, 2], sorted(index.get_containing(&new_point(0.75, 0.75))));
        assert_eq!(vec![1, 2], sorted(index.get_containing_or_nearest(&new_point(0.75, 0.75))));
    }
}
