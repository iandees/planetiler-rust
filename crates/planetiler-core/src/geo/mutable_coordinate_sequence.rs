//! Port of `geo/MutableCoordinateSequence.java`: a growable list of 2D coordinates that optionally translates and
//! scales coordinates on insertion.

use geo_types::{Coord, LineString};

use crate::geo::envelope::Envelope;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MutableCoordinateSequence {
    points: Vec<Coord<f64>>,
    /// `(scale, rel_x, rel_y)` for sequences created with [`MutableCoordinateSequence::new_scaling_sequence`].
    scaling: Option<(f64, f64, f64)>,
}

impl MutableCoordinateSequence {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(size: usize) -> Self {
        MutableCoordinateSequence { points: Vec::with_capacity(size), scaling: None }
    }

    /// Returns a sequence that maps `(x, y)` to `(scale * (x - rel_x), scale * (y - rel_y))` in
    /// [`MutableCoordinateSequence::add_point`].
    pub fn new_scaling_sequence(rel_x: f64, rel_y: f64, scale: f64) -> Self {
        MutableCoordinateSequence { points: Vec::new(), scaling: Some((scale, rel_x, rel_y)) }
    }

    pub fn size(&self) -> usize {
        self.points.len()
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn get_x(&self, i: usize) -> f64 {
        self.points[i].x
    }

    pub fn get_y(&self, i: usize) -> f64 {
        self.points[i].y
    }

    pub fn set_x(&mut self, i: usize, x: f64) {
        self.points[i].x = x;
    }

    pub fn set_y(&mut self, i: usize, y: f64) {
        self.points[i].y = y;
    }

    pub fn get(&self, i: usize) -> Coord<f64> {
        self.points[i]
    }

    pub fn coords(&self) -> &[Coord<f64>] {
        &self.points
    }

    /// Adds a coordinate to the sequence as long as it is different from the current endpoint.
    pub fn add_point(&mut self, x: f64, y: f64) {
        let (x, y) = match self.scaling {
            Some((scale, rel_x, rel_y)) => (scale * (x - rel_x), scale * (y - rel_y)),
            None => (x, y),
        };
        match self.points.last() {
            Some(last) if last.x == x && last.y == y => {}
            _ => self.points.push(Coord { x, y }),
        }
    }

    /// Adds a coordinate to the sequence even if it is the same as the current endpoint (never scaled).
    pub fn force_add_point(&mut self, x: f64, y: f64) {
        self.points.push(Coord { x, y });
    }

    /// Adds the starting coordinate to the end of this sequence if it has not already been added.
    pub fn close_ring(&mut self) {
        if let (Some(first), Some(last)) = (self.points.first(), self.points.last())
            && first != last
        {
            let first = *first;
            self.points.push(first);
        }
    }

    pub fn expand_envelope(&self, env: &mut Envelope) {
        for p in &self.points {
            env.expand_to_include(p.x, p.y);
        }
    }

    pub fn into_coords(self) -> Vec<Coord<f64>> {
        self.points
    }

    pub fn to_line_string(&self) -> LineString<f64> {
        LineString(self.points.clone())
    }
}

impl From<MutableCoordinateSequence> for Vec<Coord<f64>> {
    fn from(seq: MutableCoordinateSequence) -> Self {
        seq.points
    }
}

impl From<MutableCoordinateSequence> for LineString<f64> {
    fn from(seq: MutableCoordinateSequence) -> Self {
        LineString(seq.points)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_contents(seq: &MutableCoordinateSequence, expected: &[f64]) {
        let actual: Vec<f64> = (0..seq.size()).flat_map(|i| [seq.get_x(i), seq.get_y(i)]).collect();
        assert_eq!(expected, &actual[..]);
        let copy = seq.clone();
        let actual: Vec<f64> = copy.coords().iter().flat_map(|c| [c.x, c.y]).collect();
        assert_eq!(expected, &actual[..]);
    }

    #[test]
    fn empty() {
        assert_eq!(0, MutableCoordinateSequence::new().clone().size());
    }

    #[test]
    fn single() {
        let mut seq = MutableCoordinateSequence::new();
        seq.add_point(1.0, 2.0);
        assert_contents(&seq, &[1.0, 2.0]);
    }

    #[test]
    fn two_points() {
        let mut seq = MutableCoordinateSequence::new();
        seq.add_point(1.0, 2.0);
        seq.add_point(3.0, 4.0);
        assert_contents(&seq, &[1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn close() {
        let mut seq = MutableCoordinateSequence::new();
        seq.add_point(1.0, 2.0);
        seq.add_point(3.0, 4.0);
        seq.add_point(0.0, 1.0);
        seq.close_ring();
        assert_contents(&seq, &[1.0, 2.0, 3.0, 4.0, 0.0, 1.0, 1.0, 2.0]);
    }

    #[test]
    fn scaling() {
        let mut seq = MutableCoordinateSequence::new_scaling_sequence(1.0, 2.0, 3.0);
        seq.add_point(1.0, 2.0);
        seq.add_point(3.0, 4.0);
        seq.add_point(0.0, 1.0);
        seq.close_ring();
        assert_contents(&seq, &[0.0, 0.0, 6.0, 6.0, -3.0, -3.0, 0.0, 0.0]);
    }
}
