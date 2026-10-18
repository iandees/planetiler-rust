//! Port of `util/Imposm3Parsers.java`: parse utilities ported from
//! [omniscale/imposm3:mapping/columns.go](https://github.com/omniscale/imposm3/blob/master/mapping/columns.go).

use crate::value::{Tags, Value};

fn default_rank(highway: &str) -> Option<i64> {
    Some(match highway {
        "minor" | "road" | "unclassified" | "residential" | "tertiary_link" | "secondary_link" | "primary_link"
        | "trunk_link" | "motorway_link" => 3,
        "tertiary" => 4,
        "secondary" => 5,
        "primary" => 6,
        "trunk" => 8,
        "motorway" => 9,
        _ => return None,
    })
}

/// Returns a z-order for an OSM road based on the tags that are present. Bridges are above roads appear above tunnels
/// and major roads appear above minor.
pub fn wayzorder(tags: &Tags) -> i32 {
    let tag = |k: &str| tags.get(k).unwrap_or(&Value::Null);
    let highway = tags.get("highway").filter(|v| !v.is_null()).map(|v| v.to_string_lossy().into_owned());
    let rank = highway.as_deref().and_then(default_rank).unwrap_or(if tags.contains_key("railway") { 7 } else { 0 });
    let z = crate::util::parse::parse_long(tag("layer")).wrapping_mul(10)
        + rank
        + (bool_int(tag("tunnel")) as i64 * -10)
        + (bool_int(tag("bridge")) as i64 * 10);
    if z.abs() < 10_000 { z as i32 } else { 0 }
}

/// Returns the direction value for an input string -1 is reverse, 1 is forward ("1" "yes" or "true"), and 0 is other.
///
/// See [OSM one-way](https://wiki.openstreetmap.org/wiki/Key:oneway).
pub fn direction(obj: &Value) -> i32 {
    if obj.is_null() {
        return 0;
    }
    match obj.to_string_lossy().as_ref() {
        "1" | "yes" | "true" => 1,
        "-1" => -1,
        _ => 0,
    }
}

/// Returns `false` if `tag` is null, empty, "0", "false", or "no" and `true` otherwise.
pub fn bool(tag: &Value) -> bool {
    !(tag.is_null() || matches!(tag.to_string_lossy().as_ref(), "" | "0" | "false" | "no"))
}

/// Returns 1 if [`bool`] is `true`, 0 otherwise.
pub fn bool_int(tag: &Value) -> i32 {
    if bool(tag) { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags;

    #[test]
    fn test_wayzorder() {
        assert_eq!(0, wayzorder(&tags! {}));
        assert_eq!(10, wayzorder(&tags! {"layer" => 1}));
        assert_eq!(-30, wayzorder(&tags! {"layer" => -3}));
        assert_eq!(9, wayzorder(&tags! {"highway" => "motorway"}));
        assert_eq!(7, wayzorder(&tags! {"railway" => "anything"}));
        assert_eq!(-3, wayzorder(&tags! {"railway" => "anything", "tunnel" => "1"}));
        assert_eq!(17, wayzorder(&tags! {"railway" => "anything", "bridge" => "1"}));
    }

    #[test]
    fn test_direction() {
        for (input, out) in [("1", 1), ("yes", 1), ("true", 1), ("-1", -1), ("2", 0), ("0", 0)] {
            assert_eq!(out, direction(&Value::str(input)), "{input}");
        }
        assert_eq!(0, direction(&Value::Null));
    }
}
