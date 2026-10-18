//! Port of `planetiler-dist` `Main.java`: main entry-point for the planetiler binary, which delegates to individual
//! tasks based on the first argument (`openmaptiles` by default).
//!
//! ```text
//! planetiler [task] [--option=value ...]
//! planetiler --area=monaco --download                  # openmaptiles
//! planetiler custom --schema=path/to/schema.yml
//! planetiler path/to/schema.yml                         # same as custom --schema=...
//! planetiler example-toilets --osm-path=monaco.osm.pbf
//! planetiler compare a.mbtiles b.mbtiles
//! ```

// see the `mimalloc` feature in Cargo.toml
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::process::ExitCode;

use anyhow::bail;

type EntryPoint = fn(&[String]) -> anyhow::Result<ExitCode>;

fn ok(result: anyhow::Result<()>) -> anyhow::Result<ExitCode> {
    result.map(|()| ExitCode::SUCCESS)
}

fn openmaptiles(args: &[String]) -> anyhow::Result<ExitCode> {
    // override default sources to fix download issues with maptiler URLs
    let mut with_default_args = vec![
        format!("--{}-url", planetiler_openmaptiles::open_map_tiles_profile::LAKE_CENTERLINE_SOURCE),
        "https://github.com/acalcutt/osm-lakelines/releases/download/v12/lake_centerline.shp.zip".to_string(),
        format!("--{}-url", planetiler_openmaptiles::open_map_tiles_profile::NATURAL_EARTH_SOURCE),
        "https://naciscdn.org/naturalearth/packages/natural_earth_vector.sqlite.zip".to_string(),
    ];
    with_default_args.extend(args.iter().cloned());
    ok(planetiler_openmaptiles::open_map_tiles_main::main(&with_default_args))
}

fn custom(args: &[String]) -> anyhow::Result<ExitCode> {
    ok(planetiler_custommap::configured_map_main::main(args))
}

fn shortbread(args: &[String]) -> anyhow::Result<ExitCode> {
    let mut with_schema = vec!["--schema=shortbread.yml".to_string()];
    with_schema.extend(args.iter().cloned());
    custom(&with_schema)
}

fn verify_schema(_args: &[String]) -> anyhow::Result<ExitCode> {
    // TODO(port): custommap `SchemaValidator.main` (CLI + --watch) is not ported yet.
    bail!("verify-schema is not ported yet (custommap SchemaValidator)")
}

fn example_bikeroutes(args: &[String]) -> anyhow::Result<ExitCode> {
    ok(planetiler_examples::bike_route_overlay::main(args))
}

fn example_toilets(args: &[String]) -> anyhow::Result<ExitCode> {
    ok(planetiler_examples::toilets_overlay::main(args))
}

fn example_toilets_lowlevel(args: &[String]) -> anyhow::Result<ExitCode> {
    ok(planetiler_examples::toilets_overlay_low_level_api::main(args))
}

fn example_overture(args: &[String]) -> anyhow::Result<ExitCode> {
    ok(planetiler_examples::overture::main(args))
}

fn example_qa(args: &[String]) -> anyhow::Result<ExitCode> {
    ok(planetiler_examples::osm_qa_tiles::main(args))
}

fn benchmark_mapping(_args: &[String]) -> anyhow::Result<ExitCode> {
    // TODO(port): `OpenMapTilesMapping` benchmark is not ported.
    bail!("benchmark-mapping is not ported yet")
}

fn benchmark_longlongmap(_args: &[String]) -> anyhow::Result<ExitCode> {
    bail!("run `cargo run --release -p planetiler-benchmarks --bin long_long_map_bench -- [args]` instead")
}

fn first_path(args: &[String], usage: &str) -> anyhow::Result<std::path::PathBuf> {
    match args.iter().find(|a| !a.starts_with('-')) {
        Some(path) => Ok(path.into()),
        None => bail!("usage: {usage}"),
    }
}

fn verify_mbtiles(args: &[String]) -> anyhow::Result<ExitCode> {
    let path = first_path(args, "verify-mbtiles path/to/output.mbtiles")?;
    let errors = planetiler_core::mbtiles::verify::Verify::main(&path)?;
    Ok(if errors > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

fn verify_monaco(args: &[String]) -> anyhow::Result<ExitCode> {
    let path = first_path(args, "verify-monaco path/to/output.mbtiles")?;
    ok(planetiler_openmaptiles::util::verify_monaco::main(&path))
}

fn stats(args: &[String]) -> anyhow::Result<ExitCode> {
    ok(planetiler_core::util::tile_size_stats::main(args))
}

fn top_osm_tiles(_args: &[String]) -> anyhow::Result<ExitCode> {
    // TODO(port): `TopOsmTiles.main` needs an xz decoder for the openstreetmap.org tile logs.
    bail!("top-osm-tiles is not ported yet (needs xz decompression of tile logs)")
}

fn compare(args: &[String]) -> anyhow::Result<ExitCode> {
    let code = planetiler_core::util::compare_archives::main(args)?;
    Ok(ExitCode::from(code as u8))
}

/// Task names, mirroring Java's `ENTRY_POINTS`.
const ENTRY_POINTS: &[(&str, EntryPoint)] = &[
    ("generate-openmaptiles", openmaptiles),
    ("openmaptiles", openmaptiles),
    ("generate-custom", custom),
    ("custom", custom),
    ("generate-shortbread", shortbread),
    ("shortbread", shortbread),
    ("verify", verify_schema),
    ("verify-custom", verify_schema),
    ("verify-schema", verify_schema),
    ("example-bikeroutes", example_bikeroutes),
    ("example-toilets", example_toilets),
    ("example-toilets-lowlevel", example_toilets_lowlevel),
    ("example-overture", example_overture),
    ("overture", example_overture),
    ("example-qa", example_qa),
    ("osm-qa", example_qa),
    ("benchmark-mapping", benchmark_mapping),
    ("benchmark-longlongmap", benchmark_longlongmap),
    ("verify-mbtiles", verify_mbtiles),
    ("verify-monaco", verify_monaco),
    ("stats", stats),
    ("top-osm-tiles", top_osm_tiles),
    ("compare", compare),
];

const DEFAULT_TASK: EntryPoint = openmaptiles;

/// Picks the task for `args` (Java `Main.main` dispatch) and returns it with the remaining arguments.
fn dispatch(mut args: Vec<String>) -> Result<(EntryPoint, Vec<String>), String> {
    let mut task = DEFAULT_TASK;
    if let Some(first) = args.first() {
        let maybe_task = first.trim().to_lowercase();
        if maybe_task.ends_with(".yml") || maybe_task.ends_with(".yaml") {
            task = custom;
            args[0] = format!("--schema={}", args[0]);
        } else if let Some((_, entry)) = ENTRY_POINTS.iter().find(|(name, _)| *name == maybe_task) {
            args.remove(0);
            task = *entry;
        } else if !maybe_task.contains('=') && !maybe_task.starts_with('-') {
            let names: Vec<&str> = ENTRY_POINTS.iter().map(|(n, _)| *n).collect();
            return Err(format!("Unrecognized task: {maybe_task}\npossibilities: {names:?}"));
        }
    }
    Ok((task, args))
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        // write messages verbatim: progress logs embed ANSI colors, which the default field formatter escapes
        .fmt_fields(tracing_subscriber::fmt::format::debug_fn(|writer, field, value| {
            if field.name() == "message" { write!(writer, "{value:?}") } else { write!(writer, "{field}={value:?}") }
        }))
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (task, args) = match dispatch(args) {
        Ok(result) => result,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    match task(&args) {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn dispatch_default_task() {
        let (task, rest) = dispatch(args(&["--area=monaco"])).unwrap();
        assert_eq!(task as usize, openmaptiles as EntryPoint as usize);
        assert_eq!(args(&["--area=monaco"]), rest);
    }

    #[test]
    fn dispatch_named_task() {
        let (task, rest) = dispatch(args(&["Example-Toilets", "--osm-path=x"])).unwrap();
        assert_eq!(task as usize, example_toilets as EntryPoint as usize);
        assert_eq!(args(&["--osm-path=x"]), rest);
    }

    #[test]
    fn dispatch_yaml() {
        let (task, rest) = dispatch(args(&["schema.yml", "--x=y"])).unwrap();
        assert_eq!(task as usize, custom as EntryPoint as usize);
        assert_eq!(args(&["--schema=schema.yml", "--x=y"]), rest);
    }

    #[test]
    fn dispatch_unknown() {
        assert!(dispatch(args(&["nope"])).unwrap_err().contains("Unrecognized task: nope"));
        // key=value args are passed through to the default task
        assert!(dispatch(args(&["area=monaco"])).is_ok());
    }
}
