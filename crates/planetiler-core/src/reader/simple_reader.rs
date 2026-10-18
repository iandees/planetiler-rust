//! Port of `reader/SimpleReader.java`.
//!
//! Base trait for utilities that read features from a simple data source where geometries can be read in a single
//! pass, like [`ShapefileReader`](crate::reader::shapefile_reader::ShapefileReader) but not the OSM reader which
//! requires complex multi-pass processing.
//!
//! In Java, readers emit `SimpleFeature`s directly. Here readers are *decoders* that emit a plain, reader-owned
//! [`ReaderFeature`] record (lat/lon geometry, tags, source layer and id) that the processing pipeline
//! ([`source_feature_processor`](crate::reader::source_feature_processor)) moves into a
//! [`SimpleFeature`] with [`ReaderFeature::into_simple_feature`] (Java's
//! `SimpleFeature.create(latLonGeometry, tags, sourceName, sourceLayer, id)`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use geo_types::{Coord, Geometry};

use crate::geo::crs::MathTransform;
use crate::geo::envelope::Envelope;
use crate::reader::simple_feature::SimpleFeature;
use crate::value::Tags;

/// A feature decoded by a [`SimpleReader`]: the equivalent of the arguments to Java's
/// `SimpleFeature.create(latLonGeometry, tags, source, sourceLayer, id)`.
#[derive(Clone, Debug, PartialEq)]
pub struct ReaderFeature {
    /// Geometry in latitude/longitude (EPSG:4326, x = longitude, y = latitude).
    pub geometry: Geometry<f64>,
    /// Attributes of the feature.
    pub tags: Tags,
    /// Layer within the source this feature came from (shapefile name, geopackage/sqlite table, ...). Readers share
    /// one `Arc` per layer so emitting a feature does not allocate a new string.
    pub source_layer: Option<Arc<str>>,
    /// Feature ID, unique within a single reader.
    pub id: i64,
}

impl ReaderFeature {
    pub fn new(geometry: Geometry<f64>, tags: Tags, source_layer: Option<Arc<str>>, id: i64) -> ReaderFeature {
        ReaderFeature { geometry, tags, source_layer, id }
    }

    /// Converts this record into a [`SimpleFeature`] from source `source` (Java
    /// `SimpleFeature.create(latLonGeometry, tags, source, sourceLayer, id)`). Geometry, tags and layer name are moved,
    /// not copied; `source` is a shared `Arc` so this only bumps a reference count.
    pub fn into_simple_feature(self, source: &Arc<str>) -> SimpleFeature {
        SimpleFeature::create(self.geometry, self.tags, source, self.source_layer, self.id)
    }

    /// Returns the tag value for `key`, if present.
    pub fn get_tag(&self, key: &str) -> Option<&crate::value::Value> {
        self.tags.get(key)
    }
}

impl crate::reader::with_tags::WithTags for ReaderFeature {
    fn tags(&self) -> &Tags {
        &self.tags
    }
}

/// Callback that receives each feature from a [`SimpleReader`]. Returning an error aborts reading.
pub type FeatureSink<'a> = dyn FnMut(ReaderFeature) -> anyhow::Result<()> + 'a;

/// Equivalent of Java's `SimpleReader<F>`: something that can count and then emit all of its features.
///
/// Readers may decode features in parallel internally, but always invoke the sink from the calling thread.
pub trait SimpleReader: Send + Sync {
    /// Name of the source, used in logs and stats.
    fn source_name(&self) -> &str;

    /// Returns the number of features to be read from this reader to use for displaying progress.
    fn feature_count(&self) -> anyhow::Result<u64>;

    /// Reads all features in this data provider, submitting each to `next` for further processing.
    fn read_features(&self, next: &mut FeatureSink<'_>) -> anyhow::Result<()>;

    /// Reads all features into a vector (convenience for tests and small inputs).
    fn read_all(&self) -> anyhow::Result<Vec<ReaderFeature>> {
        let mut result = Vec::new();
        self.read_features(&mut |f| {
            result.push(f);
            Ok(())
        })?;
        Ok(result)
    }
}

/// Number of threads readers use to decode features in parallel by default.
pub(crate) fn default_decode_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).max(1)
}

/// Transforms a lat/lon envelope into the source CRS of `transform` (GeoTools `CRS.transform(transform.inverse(),
/// envelope)`), sampling points along each edge. Returns `None` if any point fails to transform.
pub(crate) fn inverse_transform_envelope(transform: &MathTransform, env: &Envelope) -> Option<Envelope> {
    if env.is_null() {
        return None;
    }
    const STEPS: usize = 16;
    let mut result = Envelope::null();
    for i in 0..=STEPS {
        let f = i as f64 / STEPS as f64;
        let x = env.min_x + (env.max_x - env.min_x) * f;
        let y = env.min_y + (env.max_y - env.min_y) * f;
        for c in
            [Coord { x, y: env.min_y }, Coord { x, y: env.max_y }, Coord { x: env.min_x, y }, Coord { x: env.max_x, y }]
        {
            let t = transform.inverse_transform(c).ok()?;
            if !t.x.is_finite() || !t.y.is_finite() {
                return None;
            }
            result.expand_to_include(t.x, t.y);
        }
    }
    Some(result)
}

/// Transform every coordinate of `geom` in place.
pub(crate) fn transform_in_place(geom: &mut Geometry<f64>, transform: &MathTransform) -> anyhow::Result<()> {
    fn line(line: &mut geo_types::LineString<f64>, t: &MathTransform) -> anyhow::Result<()> {
        for c in &mut line.0 {
            *c = t.transform(*c)?;
        }
        Ok(())
    }
    fn polygon(p: &mut geo_types::Polygon<f64>, t: &MathTransform) -> anyhow::Result<()> {
        let mut result = Ok(());
        p.exterior_mut(|e| result = line(e, t));
        result?;
        let mut result = Ok(());
        p.interiors_mut(|holes| {
            for h in holes {
                if let Err(e) = line(h, t) {
                    result = Err(e);
                    return;
                }
            }
        });
        result
    }
    match geom {
        Geometry::Point(p) => p.0 = transform.transform(p.0)?,
        Geometry::Line(l) => {
            l.start = transform.transform(l.start)?;
            l.end = transform.transform(l.end)?;
        }
        Geometry::LineString(l) => line(l, transform)?,
        Geometry::Polygon(p) => polygon(p, transform)?,
        Geometry::MultiPoint(mp) => {
            for p in &mut mp.0 {
                p.0 = transform.transform(p.0)?;
            }
        }
        Geometry::MultiLineString(ml) => {
            for l in &mut ml.0 {
                line(l, transform)?;
            }
        }
        Geometry::MultiPolygon(mp) => {
            for p in &mut mp.0 {
                polygon(p, transform)?;
            }
        }
        Geometry::GeometryCollection(gc) => {
            for g in &mut gc.0 {
                transform_in_place(g, transform)?;
            }
        }
        Geometry::Rect(_) | Geometry::Triangle(_) => {
            let mut g = transform.transform_geometry(geom)?;
            std::mem::swap(geom, &mut g);
        }
    }
    Ok(())
}

/// If `input` is a path inside a zip file (as returned by `file_utils::walk_path_with_pattern`) or the zip file itself,
/// extracts the first entry matching `extension` to `tmp_dir` and returns the extracted path. Returns `None` for paths
/// that are not in zip files.
///
/// When `keep_unzipped` is true, the file is extracted to a stable name derived from the entry path and is reused if
/// it is newer than the zip file; otherwise it is extracted to a fresh temporary file that the caller should delete.
pub(crate) fn extract_from_zip(
    input: &Path,
    extension: &str,
    tmp_dir: &Path,
    keep_unzipped: bool,
) -> anyhow::Result<Option<PathBuf>> {
    use anyhow::Context;

    use crate::util::file_utils;

    let (zip_path, entry) = if file_utils::has_extension(input, "zip") && input.is_file() {
        let entries = file_utils::list_zip_entries(input)?;
        let suffix = format!(".{}", extension.to_ascii_lowercase());
        let entry = entries
            .into_iter()
            .find(|e| e.to_ascii_lowercase().ends_with(&suffix))
            .ok_or_else(|| anyhow::anyhow!("No .{extension} file found inside {}", input.display()))?;
        (input.to_path_buf(), entry)
    } else if let Some(split) = file_utils::split_zip_path(input) {
        split
    } else {
        return Ok(None);
    };
    std::fs::create_dir_all(tmp_dir)?;
    // tempfile returns absolute paths, so compare against an absolute tmp dir
    let tmp_dir_abs = std::path::absolute(tmp_dir)?;
    let tmp_dir = tmp_dir_abs.as_path();
    let extracted = if keep_unzipped {
        // like Java's URLEncoder.encode(zipEntryPath, UTF_8)
        let encoded: String = format!("/{}", entry.trim_start_matches('/'))
            .bytes()
            .map(|b| match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'*' => (b as char).to_string(),
                b' ' => "+".to_string(),
                b => format!("%{b:02X}"),
            })
            .collect();
        tmp_dir.join(encoded)
    } else {
        tempfile::Builder::new().suffix(&format!(".{extension}")).tempfile_in(tmp_dir)?.into_temp_path().keep()?
    };
    if !extracted.starts_with(tmp_dir) {
        anyhow::bail!("Zip file tried to extract child outside of folder: {entry}");
    }
    if !keep_unzipped || file_utils::is_newer(&zip_path, &extracted) {
        tracing::info!("unzipping {} to {}", zip_path.display(), extracted.display());
        let file = std::fs::File::open(&zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
        let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))?;
        let entry = zip.by_name(entry.trim_start_matches('/'))?;
        file_utils::safe_copy(std::io::BufReader::with_capacity(1 << 20, entry), &extracted)?;
    }
    Ok(Some(extracted))
}
