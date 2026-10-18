//! The profile example from README.md ("Writing a profile in Rust"); keep the two in sync.

use planetiler_core::config::arguments::Arguments;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::planetiler::Planetiler;
use planetiler_core::profile::Profile;
use planetiler_core::reader::source_feature::SourceFeature;

struct Toilets;

impl Profile for Toilets {
    fn process_feature(&self, feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        if feature.is_point() && feature.has_tag_str("amenity", "toilets") {
            features.point("toilets").set_zoom_range(0, 14);
        }
    }

    fn name(&self) -> String {
        "Toilets".into()
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    Planetiler::create(Arguments::from_env_or_args(&args))?
        .set_profile(Toilets)
        .add_osm_source_with_url("osm", "data/sources/monaco.osm.pbf", Some("geofabrik:monaco"))?
        .overwrite_output("data/toilets.mbtiles")?
        .run()
}
