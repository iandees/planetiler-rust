//! Sample schemas bundled with the crate (Java's `/samples/*.yml` jar resources), and the JSON schemas describing the
//! schema and spec file formats.

/// JSON schema for schema YAML files (`planetiler.schema.json`).
pub const SCHEMA_JSON_SCHEMA: &str = include_str!("../resources/planetiler.schema.json");

/// JSON schema for schema spec/example YAML files (`planetilerspec.schema.json`).
pub const SPEC_JSON_SCHEMA: &str = include_str!("../resources/planetilerspec.schema.json");

/// `(file name, contents)` of every bundled sample.
pub const SAMPLES: &[(&str, &str)] = &[
    ("highway_areas.yml", include_str!("../resources/samples/highway_areas.yml")),
    ("manholes.yml", include_str!("../resources/samples/manholes.yml")),
    ("natural_earth.yml", include_str!("../resources/samples/natural_earth.yml")),
    ("owg_simple.yml", include_str!("../resources/samples/owg_simple.yml")),
    ("power.yml", include_str!("../resources/samples/power.yml")),
    ("railways.yml", include_str!("../resources/samples/railways.yml")),
    ("shortbread.spec.yml", include_str!("../resources/samples/shortbread.spec.yml")),
    ("shortbread.yml", include_str!("../resources/samples/shortbread.yml")),
];

/// Returns the contents of a bundled sample by name (`shortbread.yml` or `/samples/shortbread.yml`).
pub fn get(name: &str) -> Option<&'static str> {
    let name = name.strip_prefix("/samples/").unwrap_or(name);
    SAMPLES.iter().find(|(n, _)| *n == name).map(|(_, contents)| *contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup() {
        assert!(get("shortbread.yml").is_some());
        assert!(get("/samples/power.yml").is_some());
        assert!(get("missing.yml").is_none());
        assert!(SCHEMA_JSON_SCHEMA.contains("\"layers\""));
        assert!(!SPEC_JSON_SCHEMA.is_empty());
    }
}
