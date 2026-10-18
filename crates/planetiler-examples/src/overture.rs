//! Port of `overture/OvertureBasemap.java`: example basemap using [Overture Maps](https://overturemaps.org/) data.

use std::path::Path;

use planetiler_core::config::arguments::Arguments;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::planetiler::Planetiler;
use planetiler_core::profile::Profile;
use planetiler_core::reader::source_feature::SourceFeature;

/// Overture basemap profile.
#[derive(Debug, Default)]
pub struct OvertureBasemap;

impl Profile for OvertureBasemap {
    fn process_feature(&self, source: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        if source.source_layer() == Some("building") {
            features
                .polygon("building")
                .set_min_zoom(13)
                .inherit_attr_from_source("height")
                .inherit_attr_from_source("roof_color");
        }
        // ignore other layers for now
    }

    fn name(&self) -> String {
        "Overture".into()
    }

    fn description(&self) -> Option<String> {
        Some("A basemap generated from Overture data".into())
    }

    fn attribution(&self) -> Option<String> {
        Some(
            r#"<a href="https://www.openstreetmap.org/copyright" target="_blank">&copy; OpenStreetMap</a> <a href="https://docs.overturemaps.org/attribution" target="_blank">&copy; Overture Maps Foundation</a>"#
                .into(),
        )
    }
}

/// Main entrypoint for the example program.
pub fn main<S: AsRef<str>>(args: &[S]) -> anyhow::Result<()> {
    run(crate::parse_args(args)?)
}

/// Runs the example with `args`.
pub fn run(args: Arguments) -> anyhow::Result<()> {
    let base = args.file("base", "overture base directory", Path::new("data").join("overture"));
    Planetiler::create(args)?
        .set_profile(OvertureBasemap)
        .add_overture_source("overture-buildings", "buildings", "building", base)?
        .overwrite_output(Path::new("data").join("overture.pmtiles"))?
        .run()
}

#[cfg(test)]
mod tests {
    use planetiler_core::geo::test_utils::new_polygon;
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;

    use super::*;
    use crate::test_support::process_source_feature;

    #[test]
    fn test_building() {
        let polygon = SimpleFeature::create(
            new_polygon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]),
            tags! {"height" => 10, "other" => "x"},
            "overture",
            "building",
            1,
        );
        process_source_feature(&polygon, &OvertureBasemap, |features| {
            assert_eq!(1, features.len());
            assert_eq!("building", features[0].get_layer());
            assert_eq!(13, features[0].get_min_zoom());
            assert_eq!(&tags! {"height" => 10}, &**features[0].get_attrs_at_zoom(14));
        });
        let other = SimpleFeature::create(
            new_polygon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]),
            tags! {},
            "overture",
            "place",
            1,
        );
        process_source_feature(&other, &OvertureBasemap, |features| assert!(features.is_empty()));
    }
}
