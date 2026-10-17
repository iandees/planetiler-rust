//! Port of JTS `Envelope`: an axis-aligned bounding box that may be "null" (empty).

use geo_types::{Coord, Rect};

/// Axis-aligned rectangle, equivalent to JTS `Envelope`. A "null" envelope (see [`Envelope::null`]) contains nothing
/// and becomes non-null when expanded to include a point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Envelope {
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
}

impl Default for Envelope {
    fn default() -> Self {
        Self::null()
    }
}

impl Envelope {
    /// Creates an envelope from 2 x and 2 y values in any order (same argument order as JTS: `x1, x2, y1, y2`).
    pub fn new(x1: f64, x2: f64, y1: f64, y2: f64) -> Self {
        Envelope { min_x: x1.min(x2), max_x: x1.max(x2), min_y: y1.min(y2), max_y: y1.max(y2) }
    }

    pub fn null() -> Self {
        Envelope { min_x: 0.0, max_x: -1.0, min_y: 0.0, max_y: -1.0 }
    }

    pub fn of_point(x: f64, y: f64) -> Self {
        Envelope { min_x: x, max_x: x, min_y: y, max_y: y }
    }

    pub fn is_null(&self) -> bool {
        self.max_x < self.min_x
    }

    pub fn width(&self) -> f64 {
        if self.is_null() { 0.0 } else { self.max_x - self.min_x }
    }

    pub fn height(&self) -> f64 {
        if self.is_null() { 0.0 } else { self.max_y - self.min_y }
    }

    pub fn area(&self) -> f64 {
        self.width() * self.height()
    }

    pub fn diameter(&self) -> f64 {
        if self.is_null() { 0.0 } else { self.width().hypot(self.height()) }
    }

    pub fn centre(&self) -> Option<Coord<f64>> {
        if self.is_null() {
            None
        } else {
            Some(Coord { x: (self.min_x + self.max_x) / 2.0, y: (self.min_y + self.max_y) / 2.0 })
        }
    }

    pub fn expand_to_include(&mut self, x: f64, y: f64) {
        if self.is_null() {
            *self = Self::of_point(x, y);
        } else {
            self.min_x = self.min_x.min(x);
            self.max_x = self.max_x.max(x);
            self.min_y = self.min_y.min(y);
            self.max_y = self.max_y.max(y);
        }
    }

    pub fn expand_to_include_env(&mut self, other: &Envelope) {
        if other.is_null() {
            return;
        }
        if self.is_null() {
            *self = *other;
        } else {
            self.min_x = self.min_x.min(other.min_x);
            self.max_x = self.max_x.max(other.max_x);
            self.min_y = self.min_y.min(other.min_y);
            self.max_y = self.max_y.max(other.max_y);
        }
    }

    /// Grow (or shrink if negative) the envelope by `distance` on every side.
    pub fn expand_by(&mut self, distance: f64) {
        self.expand_by_xy(distance, distance);
    }

    pub fn expand_by_xy(&mut self, dx: f64, dy: f64) {
        if self.is_null() {
            return;
        }
        self.min_x -= dx;
        self.max_x += dx;
        self.min_y -= dy;
        self.max_y += dy;
        if self.min_x > self.max_x || self.min_y > self.max_y {
            *self = Self::null();
        }
    }

    pub fn intersects(&self, other: &Envelope) -> bool {
        !self.is_null()
            && !other.is_null()
            && other.min_x <= self.max_x
            && other.max_x >= self.min_x
            && other.min_y <= self.max_y
            && other.max_y >= self.min_y
    }

    pub fn intersects_point(&self, x: f64, y: f64) -> bool {
        !self.is_null() && x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }

    /// True if `other` lies entirely within (or on the boundary of) this envelope.
    pub fn contains(&self, other: &Envelope) -> bool {
        !self.is_null()
            && !other.is_null()
            && other.min_x >= self.min_x
            && other.max_x <= self.max_x
            && other.min_y >= self.min_y
            && other.max_y <= self.max_y
    }

    pub fn contains_point(&self, x: f64, y: f64) -> bool {
        self.intersects_point(x, y)
    }

    pub fn intersection(&self, other: &Envelope) -> Envelope {
        if !self.intersects(other) {
            return Envelope::null();
        }
        Envelope {
            min_x: self.min_x.max(other.min_x),
            max_x: self.max_x.min(other.max_x),
            min_y: self.min_y.max(other.min_y),
            max_y: self.max_y.min(other.max_y),
        }
    }

    pub fn to_rect(&self) -> Option<Rect<f64>> {
        if self.is_null() {
            None
        } else {
            Some(Rect::new(Coord { x: self.min_x, y: self.min_y }, Coord { x: self.max_x, y: self.max_y }))
        }
    }

    pub fn from_rect(rect: &Rect<f64>) -> Envelope {
        Envelope::new(rect.min().x, rect.max().x, rect.min().y, rect.max().y)
    }

    /// Compute the envelope of any geometry.
    pub fn of_geometry(geom: &geo_types::Geometry<f64>) -> Envelope {
        use geo::BoundingRect;
        geom.bounding_rect().map_or_else(Envelope::null, |r| Envelope::from_rect(&r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_envelope() {
        let mut e = Envelope::null();
        assert!(e.is_null());
        assert_eq!(0.0, e.width());
        e.expand_to_include(1.0, 2.0);
        assert!(!e.is_null());
        e.expand_to_include(3.0, -1.0);
        assert_eq!(Envelope::new(1.0, 3.0, -1.0, 2.0), e);
    }

    #[test]
    fn intersection() {
        let a = Envelope::new(0.0, 2.0, 0.0, 2.0);
        let b = Envelope::new(1.0, 3.0, 1.0, 3.0);
        assert_eq!(Envelope::new(1.0, 2.0, 1.0, 2.0), a.intersection(&b));
        assert!(a.intersection(&Envelope::new(5.0, 6.0, 5.0, 6.0)).is_null());
    }
}
