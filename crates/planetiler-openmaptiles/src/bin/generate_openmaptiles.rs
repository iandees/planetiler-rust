//! Port of `Generate.main`: regenerates `src/generated/{open_map_tiles_schema,tables}.rs` from the vendored
//! OpenMapTiles schema.
//!
//! ```text
//! cargo run -p planetiler-openmaptiles --bin generate_openmaptiles -- \
//!   [--schema-dir=crates/planetiler-openmaptiles/schema] [--output=crates/planetiler-openmaptiles/src/generated] \
//!   [--tag=<ref used in doc links, defaults to schema/REF>]
//! ```
//!
//! To update the vendored schema first run
//! `crates/planetiler-openmaptiles/scripts/fetch-schema.sh <tag-or-sha> crates/planetiler-openmaptiles/schema`.

use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;
use planetiler_openmaptiles::generate;

fn main() -> anyhow::Result<()> {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut schema_dir = crate_dir.join("schema");
    let mut output = crate_dir.join("src").join("generated");
    let mut tag = None;
    for arg in std::env::args().skip(1) {
        let arg = arg.trim_start_matches('-');
        let (key, value) = arg.split_once('=').with_context(|| format!("expected key=value: {arg}"))?;
        match key.replace('-', "_").as_str() {
            "schema_dir" => schema_dir = PathBuf::from(value),
            "output" => output = PathBuf::from(value),
            "tag" => tag = Some(value.to_string()),
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    let tag = match tag {
        Some(tag) => tag,
        None => {
            std::fs::read_to_string(schema_dir.join("REF")).map(|s| s.trim().to_string()).unwrap_or("master".into())
        }
    };
    let generated = generate::generate(&schema_dir, &tag)?;
    std::fs::create_dir_all(&output)?;
    let schema_path = output.join("open_map_tiles_schema.rs");
    let tables_path = output.join("tables.rs");
    std::fs::write(&schema_path, generated.open_map_tiles_schema)?;
    std::fs::write(&tables_path, generated.tables)?;
    // best-effort formatting
    let status = Command::new("rustfmt").arg("--edition=2024").arg(&schema_path).arg(&tables_path).status();
    if !matches!(status, Ok(s) if s.success()) {
        eprintln!("warning: rustfmt failed, run cargo fmt");
    }
    println!("Done! wrote {} and {}", schema_path.display(), tables_path.display());
    Ok(())
}
