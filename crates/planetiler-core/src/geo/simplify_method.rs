//! Port of `geo/SimplifyMethod.java`.

/// The algorithm used to simplify geometries at each zoom level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum SimplifyMethod {
    /// Douglas-Peucker simplification.
    #[default]
    RetainImportantPoints,
    /// Visvalingam-Whyatt simplification.
    RetainEffectiveAreas,
    /// Visvalingam-Whyatt simplification with a penalty for sharp corners.
    RetainWeightedEffectiveAreas,
}

impl SimplifyMethod {
    pub const DOUGLAS_PEUCKER: SimplifyMethod = SimplifyMethod::RetainImportantPoints;
    pub const VISVALINGAM_WHYATT: SimplifyMethod = SimplifyMethod::RetainEffectiveAreas;
}
