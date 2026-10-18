//! Port of JTS `PrecisionModel` (only the `FLOATING` and `FIXED` model types).

use geo_types::Coord;

/// Rounds coordinates to a fixed grid, equivalent to JTS `PrecisionModel`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PrecisionModel {
    /// Full double precision: [`PrecisionModel::make_precise`] is a no-op.
    Floating,
    /// Coordinates are rounded to the nearest `1 / scale`.
    Fixed { scale: f64 },
}

/// Java's `Math.round(double)`: round half up (towards positive infinity).
#[inline]
pub fn java_round(value: f64) -> f64 {
    let floor = value.floor();
    if value - floor >= 0.5 { floor + 1.0 } else { floor }
}

impl PrecisionModel {
    /// Equivalent to `new PrecisionModel(scale)`.
    pub fn fixed(scale: f64) -> PrecisionModel {
        PrecisionModel::Fixed { scale: scale.abs() }
    }

    pub fn is_floating(&self) -> bool {
        matches!(self, PrecisionModel::Floating)
    }

    pub fn scale(&self) -> f64 {
        match self {
            PrecisionModel::Floating => 0.0,
            PrecisionModel::Fixed { scale } => *scale,
        }
    }

    /// Size of a grid cell (`1 / scale`), or 0 for floating precision.
    pub fn grid_size(&self) -> f64 {
        match self {
            PrecisionModel::Floating => 0.0,
            PrecisionModel::Fixed { scale } => 1.0 / scale,
        }
    }

    /// Round `value` to this precision model.
    #[inline]
    pub fn make_precise(&self, value: f64) -> f64 {
        match self {
            PrecisionModel::Floating => value,
            PrecisionModel::Fixed { scale } => {
                if value.is_nan() {
                    value
                } else if *scale < 1.0 {
                    let grid_size = 1.0 / scale;
                    java_round(value / grid_size) * grid_size
                } else {
                    java_round(value * scale) / scale
                }
            }
        }
    }

    #[inline]
    pub fn make_precise_coord(&self, coord: Coord<f64>) -> Coord<f64> {
        Coord { x: self.make_precise(coord.x), y: self.make_precise(coord.y) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding() {
        let pm = PrecisionModel::fixed(16.0);
        assert_eq!(0.0625, pm.make_precise(0.07));
        assert_eq!(0.0, pm.make_precise(-1.0 / 32.0 + 1e-12));
        // Java rounds half up
        assert_eq!(0.0, pm.make_precise(-1.0 / 32.0));
        assert_eq!(1.0 / 16.0, pm.make_precise(1.0 / 32.0));
        assert_eq!(0.0, java_round(0.49999999999999994));
        assert_eq!(-1.0, java_round(-1.5));
        assert_eq!(1.234, PrecisionModel::Floating.make_precise(1.234));
        assert_eq!(1.0 / 16.0, pm.grid_size());
    }
}
