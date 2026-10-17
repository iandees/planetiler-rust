fn main() {
    let protos = [
        "proto/fileformat.proto",
        "proto/osmformat.proto",
        "proto/vector_tile_proto.proto",
        "proto/stream_archive_proto.proto",
    ];
    for p in &protos {
        println!("cargo:rerun-if-changed={p}");
    }
    let fds = protox::compile(protos, ["proto"]).expect("failed to parse .proto files");
    prost_build::Config::new().compile_fds(fds).expect("failed to generate protobuf code");
}
