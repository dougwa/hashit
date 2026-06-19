//! Compile the FileOps gRPC service (server only) for `watch --serve`. Uses a
//! vendored `protoc` so the build doesn't require a system protobuf compiler.

fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    std::env::set_var("PROTOC", protoc);
    // The client is generated too so the in-process round-trip test can call the
    // server; the binary itself only uses the server side.
    tonic_build::configure()
        .compile_protos(&["../../proto/fileops.proto"], &["../../proto"])
        .expect("compiling proto/fileops.proto");
}
