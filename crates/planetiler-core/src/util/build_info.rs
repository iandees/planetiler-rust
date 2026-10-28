//! Port of `util/BuildInfo.java`: accessor for build-time information.
//!
//! Java reads a `buildinfo.properties` file that maven fills in. Here the version comes from the optional compile-time
//! environment variable `PLANETILER_VERSION` (CI sets it from the release tag), falling back to `CARGO_PKG_VERSION`.
//! The git hash and build timestamp come from `PLANETILER_GITHASH` and `PLANETILER_BUILD_TIMESTAMP` (milliseconds since
//! the epoch), which a build script or CI can set.

use std::sync::LazyLock;

/// Information about this build of planetiler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildInfo {
    pub githash: Option<String>,
    pub version: Option<String>,
    /// Build time in milliseconds since the epoch.
    pub build_time: Option<i64>,
}

static INSTANCE: LazyLock<BuildInfo> = LazyLock::new(|| {
    BuildInfo::from_parts(
        option_env!("PLANETILER_GITHASH"),
        option_env!("PLANETILER_VERSION").or(Some(env!("CARGO_PKG_VERSION"))),
        option_env!("PLANETILER_BUILD_TIMESTAMP"),
    )
});

impl BuildInfo {
    fn from_parts(githash: Option<&str>, version: Option<&str>, epoch_ms: Option<&str>) -> BuildInfo {
        let non_blank = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        BuildInfo {
            githash: non_blank(githash),
            version: non_blank(version),
            build_time: epoch_ms
                .map(str::trim)
                .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|s| s.parse().ok()),
        }
    }

    /// Returns the build time as an ISO-8601 instant string like `2023-01-01T00:00:00Z`.
    pub fn build_time_string(&self) -> Option<String> {
        let ms = self.build_time?;
        chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
    }

    /// Returns info about this build.
    pub fn get() -> &'static BuildInfo {
        &INSTANCE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_info() {
        let info = BuildInfo::get();
        let expected = option_env!("PLANETILER_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
        assert_eq!(Some(expected), info.version.as_deref());
        let parsed = BuildInfo::from_parts(Some("abc"), Some("1.0"), Some("1672531200000"));
        assert_eq!(Some("2023-01-01T00:00:00Z".to_string()), parsed.build_time_string());
        assert_eq!(None, BuildInfo::from_parts(None, None, Some("x1")).build_time);
    }
}
