//! Port of `reader/ShapefileOptions.java`: options to control how shapefiles are read.

/// Options to control how shapefiles are read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShapefileOptions {
    /// Coordinate Reference System of the input data if you need to override what the shapefile defines (any EPSG
    /// code, PROJ string or WKT, see [`crate::geo::crs`]), or `None` to not override.
    pub projection: Option<String>,
    /// Glob pattern pointing to a single or multiple shapefiles to include in a single source.
    pub glob_pattern: String,
    /// Convert attribute names to lowercase, since `.dbf` field names are often uppercase or mixed case.
    pub lowercase_attribute_names: bool,
    /// Leave out attributes with an empty string value so they read as null, since a `.dbf` file can't store null
    /// text.
    pub empty_strings_as_null: bool,
}

impl Default for ShapefileOptions {
    fn default() -> Self {
        ShapefileOptions {
            projection: None,
            glob_pattern: "*.shp".to_string(),
            lowercase_attribute_names: false,
            empty_strings_as_null: false,
        }
    }
}

impl ShapefileOptions {
    /// Returns a copy of these options with the projection overridden (`None` to use the one the shapefile defines).
    pub fn with_projection(mut self, projection: Option<&str>) -> Self {
        self.projection = projection.map(str::to_string);
        self
    }

    /// Returns a copy of these options that reads the files matching `glob_pattern`.
    pub fn with_glob_pattern(mut self, glob_pattern: &str) -> Self {
        self.glob_pattern = glob_pattern.to_string();
        self
    }

    /// Returns a copy of these options with `lowercase_attribute_names` set.
    pub fn with_lowercase_attribute_names(mut self, lowercase_attribute_names: bool) -> Self {
        self.lowercase_attribute_names = lowercase_attribute_names;
        self
    }

    /// Returns a copy of these options with `empty_strings_as_null` set.
    pub fn with_empty_strings_as_null(mut self, empty_strings_as_null: bool) -> Self {
        self.empty_strings_as_null = empty_strings_as_null;
        self
    }
}
