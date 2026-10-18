//! Port of `reader/parquet/CoordinateSequenceBuilder.java`: a mutable packed coordinate sequence that grows to fit
//! new elements when [`CoordinateSequenceBuilder::set_ordinate`] is called.
//!
//! geo-types coordinates are 2D, so `z`/`m` ordinates are stored but dropped by [`CoordinateSequenceBuilder::coords`].

use geo_types::Coord;

/// A growable packed sequence of coordinates with `components` ordinates each (2 = xy, 3 = xyz, 4 = xyzm).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CoordinateSequenceBuilder {
    points: Vec<f64>,
    components: usize,
}

impl CoordinateSequenceBuilder {
    pub fn new(components: usize) -> CoordinateSequenceBuilder {
        CoordinateSequenceBuilder { points: Vec::new(), components: components.max(2) }
    }

    pub fn with_capacity(components: usize, capacity: usize) -> CoordinateSequenceBuilder {
        let components = components.max(2);
        CoordinateSequenceBuilder { points: Vec::with_capacity(capacity * components), components }
    }

    /// Number of ordinates per coordinate.
    pub fn dimension(&self) -> usize {
        self.components
    }

    /// Number of measures (`m` ordinates) per coordinate.
    pub fn measures(&self) -> usize {
        usize::from(self.components > 3)
    }

    pub fn size(&self) -> usize {
        self.points.len() / self.components
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn get_ordinate(&self, index: usize, ordinate: usize) -> f64 {
        self.points[index * self.components + ordinate]
    }

    /// Sets an ordinate, growing the sequence to fit `index` (new ordinates are 0).
    pub fn set_ordinate(&mut self, index: usize, ordinate: usize, value: f64) {
        let idx = index * self.components + ordinate;
        let count = (index + 1) * self.components;
        if self.points.len() < count {
            self.points.resize(count, 0.0);
        }
        self.points[idx] = value;
    }

    /// Appends an x/y coordinate.
    pub fn push_xy(&mut self, x: f64, y: f64) {
        let index = self.size();
        self.set_ordinate(index, 0, x);
        self.set_ordinate(index, 1, y);
    }

    pub fn get_x(&self, index: usize) -> f64 {
        self.get_ordinate(index, 0)
    }

    pub fn get_y(&self, index: usize) -> f64 {
        self.get_ordinate(index, 1)
    }

    /// The x/y coordinates of this sequence.
    pub fn coords(&self) -> Vec<Coord<f64>> {
        self.points.chunks_exact(self.components).map(|c| Coord { x: c[0], y: c[1] }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        for (components, dims, measures) in [(2, 2, 0), (3, 3, 0), (4, 4, 1)] {
            let cs = CoordinateSequenceBuilder::new(components);
            assert_eq!(dims, cs.dimension());
            assert_eq!(measures, cs.measures());
            assert_eq!(0, cs.size());
        }
    }

    #[test]
    fn single_xy() {
        let mut cs = CoordinateSequenceBuilder::new(2);
        cs.set_ordinate(0, 0, 1.0);
        cs.set_ordinate(0, 1, 2.0);
        assert_eq!(vec![Coord { x: 1.0, y: 2.0 }], cs.coords());
    }

    #[test]
    fn double_xyz() {
        let mut cs = CoordinateSequenceBuilder::new(3);
        cs.set_ordinate(0, 0, 1.0);
        cs.set_ordinate(0, 1, 2.0);
        cs.set_ordinate(1, 0, 1.0);
        cs.set_ordinate(1, 1, 2.0);
        cs.set_ordinate(1, 2, 3.0);
        assert_eq!(2, cs.size());
        assert_eq!(0.0, cs.get_ordinate(0, 2));
        assert_eq!(3.0, cs.get_ordinate(1, 2));
    }

    #[test]
    fn triple_xyzm() {
        let mut cs = CoordinateSequenceBuilder::new(4);
        cs.set_ordinate(0, 0, 1.0);
        cs.set_ordinate(0, 1, 2.0);
        cs.set_ordinate(1, 0, 1.0);
        cs.set_ordinate(1, 1, 2.0);
        cs.set_ordinate(1, 2, 3.0);
        cs.set_ordinate(2, 0, 1.0);
        cs.set_ordinate(2, 1, 2.0);
        cs.set_ordinate(2, 2, 3.0);
        cs.set_ordinate(2, 3, 4.0);
        assert_eq!(3, cs.size());
        assert_eq!(1, cs.measures());
        assert_eq!([0.0, 0.0], [cs.get_ordinate(0, 2), cs.get_ordinate(0, 3)]);
        assert_eq!([3.0, 0.0], [cs.get_ordinate(1, 2), cs.get_ordinate(1, 3)]);
        assert_eq!([3.0, 4.0], [cs.get_ordinate(2, 2), cs.get_ordinate(2, 3)]);
    }
}
