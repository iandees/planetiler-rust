//! Port of `layers/Housenumber.java`: defines the logic for generating map elements in the `housenumber` layer from
//! source features.
//!
//! This is ported from the
//! [OpenMapTiles housenumber sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/housenumber).

use std::borrow::Cow;
use std::sync::Arc;

use indexmap::IndexMap;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::forwarding_profile::{Handler, LayerPostProcessor};
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::translations::Translations;
use planetiler_core::value::Value;
use planetiler_core::vector_tile;

use crate::feature_merge;
use crate::generated::open_map_tiles_schema::housenumber::*;
use crate::generated::tables::*;
use crate::layer::Layer;

const OSM_SEPARATOR: char = ';';
const DISPLAY_SEPARATOR: &str = "–";
const TEMP_PARTITION: &str = "_partition";
const TEMP_HAS_NAME: &str = "_has_name";

pub struct Housenumber {
    stats: StatsRef,
}

/// Returned when a numeric house number range cannot be parsed.
#[derive(Debug)]
pub struct NumberFormatError;

impl Housenumber {
    pub fn new(_translations: &Arc<Translations>, _config: &Arc<PlanetilerConfig>, stats: &StatsRef) -> Housenumber {
        Housenumber { stats: stats.clone() }
    }

    /// Converts `;`-separated house numbers into a `first–last` (or numeric `min–max`) display range.
    pub fn display_housenumber(housenumber: &str) -> Result<Cow<'_, str>, NumberFormatError> {
        if !housenumber.contains(OSM_SEPARATOR) {
            return Ok(Cow::Borrowed(housenumber));
        }
        let numbers: Vec<&str> = housenumber
            .split(OSM_SEPARATOR)
            .map(|s| s.trim_matches(|c: char| c <= ' '))
            .filter(|s| !s.is_empty())
            .collect();
        if numbers.is_empty() {
            // not much to do with strange/invalid entries like "3;" or ";" etc.
            return Ok(Cow::Borrowed(housenumber));
        }
        if housenumber.chars().any(|c| !(c.is_ascii_digit() || c == ';')) {
            return Ok(Cow::Owned(format!("{}{DISPLAY_SEPARATOR}{}", numbers[0], numbers[numbers.len() - 1])));
        }
        // numeric display house number
        let mut min = u64::MAX;
        let mut max = u64::MIN;
        for n in numbers {
            let v: u64 = n.parse().map_err(|_| NumberFormatError)?;
            min = min.min(v);
            max = max.max(v);
        }
        // Java summarizes as signed longs
        let (min, max) = (min as i64, max as i64);
        Ok(Cow::Owned(format!("{min}{DISPLAY_SEPARATOR}{max}")))
    }
}

impl OsmHousenumberPointHandler for Housenumber {
    fn process_osm_housenumber_point(&self, element: &OsmHousenumberPoint<'_>, features: &mut FeatureCollector<'_>) {
        let raw = element.housenumber();
        let raw = raw.as_deref().unwrap_or("");
        let housenumber = match Housenumber::display_housenumber(raw) {
            Ok(h) => h,
            Err(_) => {
                // should not be happening (thanks to the digit check) but ...
                self.stats.data_error("housenumber_range");
                tracing::warn!("Failed to convert housenumber range: {raw}");
                Cow::Borrowed(raw)
            }
        };
        let street = element.street();
        let block_number = element.block_number();
        let mut partition = String::with_capacity(
            street.as_deref().map_or(0, str::len) + block_number.as_deref().map_or(0, str::len) + housenumber.len(),
        );
        partition.push_str(street.as_deref().unwrap_or(""));
        partition.push_str(block_number.as_deref().unwrap_or(""));
        partition.push_str(&housenumber);
        let has_name = element.has_name().is_some_and(|n| !n.is_empty());

        features
            .centroid_if_convex(LAYER_NAME)
            .set_buffer_pixels(BUFFER_SIZE)
            .set_attr(fields::HOUSENUMBER, &*housenumber)
            .set_attr(TEMP_PARTITION, partition)
            .set_attr(TEMP_HAS_NAME, has_name)
            .set_min_zoom(14);
    }
}

impl LayerPostProcessor for Housenumber {
    fn post_process(&self, _zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()> {
        // remove duplicate house numbers, features without name tag are prioritized
        let mut groups: IndexMap<Option<Value>, vector_tile::Feature> = IndexMap::with_capacity(items.len());
        let has_name = |f: &vector_tile::Feature| f.tags.get(TEMP_HAS_NAME).and_then(Value::as_bool).unwrap_or(false);
        for item in items.drain(..) {
            let key = item.tags.get(TEMP_PARTITION).cloned();
            match groups.entry(key) {
                indexmap::map::Entry::Vacant(e) => {
                    e.insert(item);
                }
                indexmap::map::Entry::Occupied(mut e) => {
                    // keep the first minimum (false < true)
                    if !has_name(&item) && has_name(e.get()) {
                        e.insert(item);
                    }
                }
            }
        }
        // Java iterates the `groupingBy` HashMap, so emit the groups in Java HashMap order (this decides which
        // feature's ID the merged multipoint keeps)
        let keys: Vec<String> =
            groups.keys().map(|k| k.as_ref().map(Value::to_string_lossy).unwrap_or_default().into_owned()).collect();
        let order = java_hash_map_order(&keys);
        let mut slots: Vec<Option<vector_tile::Feature>> = groups.into_values().map(Some).collect();
        let mut result: Vec<vector_tile::Feature> = order.into_iter().filter_map(|i| slots[i].take()).collect();
        // remove temporary attributes
        for item in &mut result {
            item.tags.shift_remove(TEMP_HAS_NAME);
            item.tags.shift_remove(TEMP_PARTITION);
        }
        // reduces the size of some heavy z14 tiles with many repeated housenumber values by 60% or more
        *items = feature_merge::merge_multi_point(result);
        Ok(())
    }
}

impl Handler for Housenumber {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }

    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        Some(self)
    }
}

impl RowHandlers for Housenumber {
    fn as_osm_housenumber_point_handler(&self) -> Option<&dyn OsmHousenumberPointHandler> {
        Some(self)
    }
}

impl Layer for Housenumber {}

#[cfg(test)]
mod tests {
    use planetiler_core::tags;

    use super::*;
    use crate::test_support::*;

    #[test]
    fn test_housenumber() {
        let t = LayerTest::new();
        let expected =
            tags! {"_layer" => "housenumber", "_type" => "point", "_minzoom" => 14, "_maxzoom" => 14, "_buffer" => 8.0};
        assert_features(
            14,
            std::slice::from_ref(&expected),
            &t.process(&point_feature(tags! {"addr:housenumber" => "10"})),
        );
        assert_features(14, &[expected], &t.process(&polygon_feature(tags! {"addr:housenumber" => "10"})));
    }

    #[test]
    fn test_display_housenumber() {
        for (outlier, expected) in [
            ("1", "1"),
            ("1;1a;2;2/b;20;3", "1–3"),
            ("1;1a;2;2/b;20;3;", "1–3"),
            ("1;2;20;3", "1–20"),
            ("1;2;20;3;", "1–20"),
            (";", ";"),
            (";;", ";;"),
            ("2712;935803935803", "2712–935803935803"),
        ] {
            assert_eq!(expected, Housenumber::display_housenumber(outlier).unwrap(), "{outlier}");
        }
    }

    #[test]
    fn test_temp_attrs() {
        let t = LayerTest::new();
        assert_features(
            14,
            &[tags! {"_has_name" => true, "_partition" => "streetX765/6"}],
            &t.process(&polygon_feature(tags! {
                "addr:housenumber" => "765/6",
                "addr:block_number" => "X",
                "addr:street" => "street",
                "name" => "name",
            })),
        );
    }

    #[test]
    fn test_nonduplicate_housenumber() {
        let t = LayerTest::new();
        let hn1 = vt_point_feature(LAYER_NAME, tags! {"housenumber" => "764/2", "_partition" => "764/2"}, 1);
        let hn2 = vt_point_feature(LAYER_NAME, tags! {"housenumber" => "765/6", "_partition" => "765/6"}, 1);
        assert_eq!(2, t.post_process(LAYER_NAME, 14, vec![hn1, hn2]).len());
    }

    #[test]
    fn test_nonduplicate_street() {
        let t = LayerTest::new();
        let housenumber = "765/6";
        let hn1 = vt_point_feature(
            LAYER_NAME,
            tags! {"housenumber" => housenumber, "_partition" => format!("street 1{housenumber}")},
            1,
        );
        let hn2 = vt_point_feature(
            LAYER_NAME,
            tags! {"housenumber" => housenumber, "_partition" => format!("street 2{housenumber}")},
            1,
        );
        let result = t.post_process(LAYER_NAME, 14, vec![hn1, hn2]);
        // same housenumber => two points merged into one multipoint
        assert_eq!(1, result.len());
        // two point in multipoint => 5 commands
        assert_eq!(5, result[0].geometry.commands().len());
    }

    #[test]
    fn test_duplicate_housenumber() {
        let t = LayerTest::new();
        let housenumber = "765/6";
        let hn1 = vt_point_feature(
            LAYER_NAME,
            tags! {"housenumber" => format!("{housenumber} (no name)"), "_has_name" => false, "_partition" => housenumber},
            1,
        );
        let hn2 = vt_point_feature(
            LAYER_NAME,
            tags! {"housenumber" => format!("{housenumber} (with name)"), "_has_name" => true, "_partition" => housenumber},
            1,
        );
        let result = t.post_process(LAYER_NAME, 14, vec![hn1, hn2]);
        assert_eq!(vec![vt_point_feature(LAYER_NAME, tags! {"housenumber" => "765/6 (no name)"}, 1)], result);
        // only one point in multipoint => 3 commands
        assert_eq!(3, result[0].geometry.commands().len());
    }
}

/// Java `String.hashCode` (over UTF-16 code units).
fn java_string_hash(s: &str) -> i32 {
    s.encode_utf16().fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32))
}

/// The order a `java.util.HashMap<String, _>` (default capacity 16, load factor 0.75) iterates `keys` inserted in
/// order: by bucket of the final table, then insertion order within a bucket (ignoring treeified bins, which need 8+
/// colliding keys).
fn java_hash_map_order(keys: &[String]) -> Vec<usize> {
    let mut capacity = 16usize;
    while keys.len() > capacity * 3 / 4 {
        capacity *= 2;
    }
    let bucket = |k: &str| {
        let h = java_string_hash(k) as u32;
        ((h ^ (h >> 16)) as usize) & (capacity - 1)
    };
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by_key(|&i| bucket(&keys[i]));
    order
}

#[cfg(test)]
mod java_hash_map_order_tests {
    use super::*;

    #[test]
    fn java_string_hash_matches() {
        assert_eq!(0, java_string_hash(""));
        assert_eq!(97, java_string_hash("a"));
        assert_eq!(99162322, java_string_hash("hello"));
    }

    #[test]
    fn hash_map_order() {
        // java: new HashMap<>() with "b", "a", "c" iterates a, b, c (buckets 1, 2, 3)
        let keys: Vec<String> = ["b", "a", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(vec![1, 0, 2], java_hash_map_order(&keys));
    }
}
