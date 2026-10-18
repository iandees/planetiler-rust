//! Port of `geo/GeometryPipeline.java`: composable geometry transformations.

use std::sync::Arc;

use geo_types::Geometry;

use crate::geo::douglas_peucker_simplifier::DouglasPeuckerSimplifier;
use crate::geo::dual_midpoint_smoother::DualMidpointSmoother;
use crate::geo::midpoint_smoother::MidpointSmoother;
use crate::geo::simplify_method::SimplifyMethod;
use crate::geo::vw_simplifier::VWSimplifier;

/// A function that transforms a geometry (Java's `GeometryPipeline`).
pub trait GeometryPipeline: Send + Sync {
    fn apply(&self, input: &Geometry<f64>) -> Geometry<f64>;
}

/// Shared reference to a pipeline.
pub type GeometryPipelineRef = Arc<dyn GeometryPipeline>;

/// A pipeline that varies by zoom level (Java's `ZoomFunction<GeometryPipeline>`); `None` means no-op at that zoom.
pub type ZoomPipeline = Arc<dyn Fn(u32) -> Option<GeometryPipelineRef> + Send + Sync>;

impl<F: Fn(&Geometry<f64>) -> Geometry<f64> + Send + Sync> GeometryPipeline for F {
    fn apply(&self, input: &Geometry<f64>) -> Geometry<f64> {
        self(input)
    }
}

/// Wrap a closure as a [`GeometryPipelineRef`].
pub fn pipeline(f: impl Fn(&Geometry<f64>) -> Geometry<f64> + Send + Sync + 'static) -> GeometryPipelineRef {
    Arc::new(f)
}

/// The no-op pipeline.
pub fn noop() -> GeometryPipelineRef {
    pipeline(|g| g.clone())
}

/// Returns a pipeline equivalent to `b(a(geom))`.
pub fn compose(a: GeometryPipelineRef, b: GeometryPipelineRef) -> GeometryPipelineRef {
    pipeline(move |g| b.apply(&a.apply(g)))
}

/// Returns a pipeline that applies each element sequentially.
pub fn compose_all(pipelines: Vec<GeometryPipelineRef>) -> GeometryPipelineRef {
    pipeline(move |g| {
        let mut result = g.clone();
        for p in &pipelines {
            result = p.apply(&result);
        }
        result
    })
}

/// A zoom pipeline that returns `p` at every zoom (Java's `GeometryPipeline.apply(int zoom)`).
pub fn at_all_zooms(p: GeometryPipelineRef) -> ZoomPipeline {
    Arc::new(move |_| Some(p.clone()))
}

fn apply_or_noop(f: &ZoomPipeline, zoom: u32) -> GeometryPipelineRef {
    f(zoom).unwrap_or_else(noop)
}

/// Returns a function equivalent to `b(a(geom))` at each zoom.
pub fn compose_by_zoom(a: ZoomPipeline, b: ZoomPipeline) -> ZoomPipeline {
    Arc::new(move |zoom| Some(compose(apply_or_noop(&a, zoom), apply_or_noop(&b, zoom))))
}

/// Returns a function that applies each argument sequentially to the input geometry at each zoom.
pub fn compose_all_by_zoom(fns: Vec<ZoomPipeline>) -> ZoomPipeline {
    Arc::new(move |zoom| Some(compose_all(fns.iter().map(|f| apply_or_noop(f, zoom)).collect())))
}

/// The normal simplification planetiler applies at each zoom without a custom pipeline (Java's `defaultSimplify`),
/// given the pixel tolerance and simplify method for a zoom.
pub fn default_simplify(pixel_tolerance: f64, simplify_method: SimplifyMethod) -> GeometryPipelineRef {
    let tolerance = pixel_tolerance / 256.0;
    match simplify_method {
        SimplifyMethod::RetainImportantPoints => {
            pipeline(move |g| crate::geo::douglas_peucker_simplifier::simplify(g, tolerance))
        }
        // DP tolerance is displacement, and VW tolerance is area, so square what the user entered to convert from
        // DP to VW tolerance
        SimplifyMethod::RetainEffectiveAreas => Arc::new(VWSimplifier::new().set_tolerance(tolerance * tolerance)),
        SimplifyMethod::RetainWeightedEffectiveAreas => {
            Arc::new(VWSimplifier::new().set_weight(0.7).set_tolerance(tolerance * tolerance))
        }
    }
}

/// Returns a pipeline that simplifies input geometries using the Visvalingam Whyatt algorithm.
pub fn simplify_vw(tolerance: f64) -> VWSimplifier {
    VWSimplifier::new().set_tolerance(tolerance)
}

/// Returns a pipeline that simplifies input geometries using the Douglas Peucker algorithm.
pub fn simplify_dp(tolerance: f64) -> DouglasPeuckerSimplifier {
    DouglasPeuckerSimplifier::new(tolerance)
}

/// Returns a pipeline that smooths an input geometry by joining the midpoint of each edge.
pub fn smooth_midpoint(iters: usize) -> MidpointSmoother {
    MidpointSmoother::new().set_iters(iters)
}

/// Returns a pipeline that applies Chaikin smoothing `iters` times.
pub fn smooth_chaikin(iters: usize) -> DualMidpointSmoother {
    DualMidpointSmoother::chaikin(iters)
}

/// Chaikin smoothing that stops when vertices get within `tolerance` of the line between their neighbors.
pub fn smooth_chaikin_to_tolerance(tolerance: f64) -> DualMidpointSmoother {
    DualMidpointSmoother::chaikin_to_tolerance(tolerance)
}

/// Chaikin smoothing that stops when the triangle formed by a vertex and its neighbors is below `min_area`.
pub fn smooth_chaikin_to_min_area(min_area: f64) -> DualMidpointSmoother {
    DualMidpointSmoother::chaikin_to_min_area(min_area)
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Translate;
    use geo_types::Point;

    fn mv(i: i32) -> GeometryPipelineRef {
        pipeline(move |g| g.translate(i as f64, i as f64))
    }

    fn move_by_zoom() -> ZoomPipeline {
        Arc::new(|z| Some(mv(z as i32)))
    }

    fn point(i: i32) -> Geometry<f64> {
        Geometry::Point(Point::new(i as f64, i as f64))
    }

    #[test]
    fn single() {
        assert_eq!(point(2), mv(1).apply(&point(1)));
        assert_eq!(point(2), at_all_zooms(mv(1))(3).unwrap().apply(&point(1)));
    }

    #[test]
    fn single_by_zoom() {
        assert_eq!(point(4), move_by_zoom()(3).unwrap().apply(&point(1)));
    }

    #[test]
    fn compose2() {
        assert_eq!(point(4), compose(mv(1), mv(2)).apply(&point(1)));
        assert_eq!(point(5), compose_by_zoom(at_all_zooms(mv(1)), move_by_zoom())(3).unwrap().apply(&point(1)));
        assert_eq!(point(5), compose_by_zoom(move_by_zoom(), at_all_zooms(mv(1)))(3).unwrap().apply(&point(1)));
        assert_eq!(point(7), compose_by_zoom(move_by_zoom(), move_by_zoom())(3).unwrap().apply(&point(1)));
    }

    #[test]
    fn compose_many() {
        let m1 = || at_all_zooms(mv(1));
        assert_eq!(
            point(8),
            compose_all_by_zoom(vec![m1(), move_by_zoom(), move_by_zoom()])(3).unwrap().apply(&point(1))
        );
        assert_eq!(
            point(12),
            compose_all_by_zoom(vec![m1(), move_by_zoom(), move_by_zoom(), m1(), move_by_zoom()])(3)
                .unwrap()
                .apply(&point(1))
        );
    }
}
