//! Generated protobuf types.

/// OSM PBF file format (`fileformat.proto` + `osmformat.proto`).
#[allow(clippy::all, non_snake_case, non_camel_case_types)]
pub mod osmpbf {
    include!(concat!(env!("OUT_DIR"), "/osmpbf.rs"));
}

/// Mapbox vector tile protobuf (`vector_tile_proto.proto`).
#[allow(clippy::all)]
pub mod vector_tile {
    include!(concat!(env!("OUT_DIR"), "/vector_tile.rs"));
}

/// Planetiler stream archive protobuf (`stream_archive_proto.proto`).
#[allow(clippy::all)]
pub mod stream_archive {
    include!(concat!(env!("OUT_DIR"), "/com.onthegomap.planetiler.proto.rs"));
}
