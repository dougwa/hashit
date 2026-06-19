//! Compile the Search gRPC service. Uses a vendored `protoc` so the build
//! doesn't depend on a system protobuf compiler being installed.

fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    std::env::set_var("PROTOC", protoc);
    tonic_build::configure()
        .build_client(false)
        .compile_protos(&["../../proto/search.proto"], &["../../proto"])
        .expect("compiling proto/search.proto");
}
