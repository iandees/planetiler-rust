//! Port of JTS `BufferParameters`.

use crate::geo::cg_algorithms::angle::PI_OVER_2;

/// How the ends of buffered lines are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapStyle {
    Round,
    Flat,
    Square,
}

/// How the corners of buffered geometries are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinStyle {
    Round,
    Mitre,
    Bevel,
}

/// Parameters for a buffer operation (JTS `BufferParameters`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BufferParameters {
    pub quadrant_segments: i32,
    pub end_cap_style: CapStyle,
    pub join_style: JoinStyle,
    pub mitre_limit: f64,
    pub is_single_sided: bool,
    simplify_factor: f64,
}

impl BufferParameters {
    pub const DEFAULT_QUADRANT_SEGMENTS: i32 = 8;
    pub const DEFAULT_MITRE_LIMIT: f64 = 5.0;
    pub const DEFAULT_SIMPLIFY_FACTOR: f64 = 0.01;

    pub const fn new() -> BufferParameters {
        BufferParameters {
            quadrant_segments: Self::DEFAULT_QUADRANT_SEGMENTS,
            end_cap_style: CapStyle::Round,
            join_style: JoinStyle::Round,
            mitre_limit: Self::DEFAULT_MITRE_LIMIT,
            is_single_sided: false,
            simplify_factor: Self::DEFAULT_SIMPLIFY_FACTOR,
        }
    }

    /// Default parameters with mitre joins (what planetiler's `FeatureMerge` uses).
    pub const fn mitre() -> BufferParameters {
        let mut p = Self::new();
        p.join_style = JoinStyle::Mitre;
        p
    }

    pub const fn with_quadrant_segments(mut self, quadrant_segments: i32) -> BufferParameters {
        self.quadrant_segments = quadrant_segments;
        self
    }

    pub const fn with_end_cap_style(mut self, style: CapStyle) -> BufferParameters {
        self.end_cap_style = style;
        self
    }

    pub const fn with_join_style(mut self, style: JoinStyle) -> BufferParameters {
        self.join_style = style;
        self
    }

    pub const fn with_mitre_limit(mut self, mitre_limit: f64) -> BufferParameters {
        self.mitre_limit = mitre_limit;
        self
    }

    pub const fn with_single_sided(mut self, single_sided: bool) -> BufferParameters {
        self.is_single_sided = single_sided;
        self
    }

    /// Sets the factor used to determine the simplify distance tolerance for input simplification (negative values
    /// are treated as 0).
    pub fn with_simplify_factor(mut self, simplify_factor: f64) -> BufferParameters {
        self.simplify_factor = if simplify_factor < 0.0 { 0.0 } else { simplify_factor };
        self
    }

    pub fn simplify_factor(&self) -> f64 {
        self.simplify_factor
    }

    /// JTS `BufferParameters.bufferDistanceError`: the maximum error (as a fraction of the buffer distance) of the
    /// approximation of a circular arc with `quad_segs` segments per quadrant.
    pub fn buffer_distance_error(quad_segs: i32) -> f64 {
        let alpha = PI_OVER_2 / quad_segs as f64;
        1.0 - (alpha / 2.0).cos()
    }
}

impl Default for BufferParameters {
    fn default() -> Self {
        Self::new()
    }
}
