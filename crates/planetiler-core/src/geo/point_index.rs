//! Port of `geo/PointIndex.java`: index to efficiently query points within a radius from a point.
//!
//! Writes and reads are thread-safe, but all writes must occur before reads.

use std::sync::OnceLock;

use geo_types::{Coord, Geometry, Point};
use parking_lot::Mutex;
use rstar::primitives::GeomWithData;
use rstar::{AABB, RTree};

type Entry = GeomWithData<[f64; 2], usize>;
type Built<T> = (RTree<Entry>, Vec<(Coord<f64>, T)>);

pub struct PointIndex<T> {
    items: Mutex<Vec<(Coord<f64>, T)>>,
    built: OnceLock<Built<T>>,
}

impl<T: Clone> Default for PointIndex<T> {
    fn default() -> Self {
        Self::create()
    }
}

impl<T: Clone> PointIndex<T> {
    pub fn create() -> Self {
        PointIndex { items: Mutex::new(Vec::new()), built: OnceLock::new() }
    }

    fn index(&self) -> &Built<T> {
        self.built.get_or_init(|| {
            let items = std::mem::take(&mut *self.items.lock());
            let entries = items.iter().enumerate().map(|(i, (c, _))| GeomWithData::new([c.x, c.y], i)).collect();
            (RTree::bulk_load(entries), items)
        })
    }

    fn query(&self, point: &Point<f64>, threshold: f64) -> Vec<usize> {
        let (tree, _) = self.index();
        let aabb = AABB::from_corners(
            [point.x() - threshold, point.y() - threshold],
            [point.x() + threshold, point.y() + threshold],
        );
        let mut result: Vec<usize> = tree.locate_in_envelope(aabb).map(|e| e.data).collect();
        result.sort_unstable();
        result
    }

    /// Returns the data associated with all indexed points within a radius from `point`.
    pub fn get_within(&self, point: &Point<f64>, threshold: f64) -> Vec<T> {
        let items = self.query(point, threshold);
        let (_, all) = self.index();
        items
            .into_iter()
            .filter(|i| {
                let c = all[*i].0;
                (c.x - point.x()).hypot(c.y - point.y()) <= threshold
            })
            .map(|i| all[i].1.clone())
            .collect()
    }

    /// Returns the data associated with the nearest indexed point to `point`, up to a certain distance.
    pub fn get_nearest(&self, point: &Point<f64>, threshold: f64) -> Option<T> {
        let items = self.query(point, threshold);
        let (_, all) = self.index();
        let mut nearest_distance = f64::MAX;
        let mut nearest = None;
        for i in items {
            let c = all[i].0;
            let distance = (c.x - point.x()).hypot(c.y - point.y());
            if distance < nearest_distance {
                nearest_distance = distance;
                nearest = Some(all[i].1.clone());
            }
        }
        nearest
    }

    /// Indexes `item` for points contained in `geom`.
    pub fn put(&self, geom: &Geometry<f64>, item: T) {
        debug_assert!(self.built.get().is_none(), "PointIndex written to after reads started");
        match geom {
            Geometry::Point(p) => self.items.lock().push((p.0, item)),
            Geometry::MultiPoint(mp) => {
                for p in &mp.0 {
                    self.items.lock().push((p.0, item.clone()));
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
        let index: PointIndex<i32> = PointIndex::create();
        assert!(index.get_within(&new_point(0.5, 0.5), 1.0).is_empty());
        assert_eq!(None, index.get_nearest(&new_point(0.5, 0.5), 1.0));
    }

    #[test]
    fn single() {
        let index = PointIndex::create();
        index.put(&new_point(0.5, 1.0).into(), 1);
        assert_eq!(vec![1], index.get_within(&new_point(0.5, 0.5), 0.6));
        assert!(index.get_within(&new_point(0.5, 0.5), 0.4).is_empty());
        assert_eq!(None, index.get_nearest(&new_point(0.5, 0.5), 0.4));
        assert_eq!(Some(1), index.get_nearest(&new_point(0.5, 0.5), 0.6));
    }

    #[test]
    fn multiple_identical() {
        let index = PointIndex::create();
        index.put(&new_point(1.0, 1.0).into(), 1);
        index.put(&new_point(1.0, 1.0).into(), 2);
        assert_eq!(vec![1, 2], sorted(index.get_within(&new_point(0.5, 1.0), 0.6)));
        assert!(index.get_within(&new_point(0.5, 1.0), 0.4).is_empty());
    }

    #[test]
    fn multiple_different() {
        let index = PointIndex::create();
        index.put(&new_point(0.0, 1.0).into(), 1);
        index.put(&new_point(1.0, 1.0).into(), 2);
        assert_eq!(vec![1, 2], sorted(index.get_within(&new_point(0.5, 1.0), 0.6)));
        assert_eq!(vec![1], index.get_within(&new_point(0.4, 1.0), 0.4));
        assert_eq!(vec![2], index.get_within(&new_point(0.6, 1.0), 0.4));
        assert!(index.get_within(&new_point(0.5, 1.0), 0.4).is_empty());

        assert_eq!(Some(1), index.get_nearest(&new_point(0.4, 1.0), 0.5));
        assert_eq!(Some(1), index.get_nearest(&new_point(0.4, 1.0), 1.0));
        assert_eq!(None, index.get_nearest(&new_point(0.5, 1.0), 0.4));
        assert_eq!(Some(2), index.get_nearest(&new_point(0.6, 1.0), 0.5));
        assert_eq!(Some(2), index.get_nearest(&new_point(0.6, 1.0), 1.0));
    }
}
