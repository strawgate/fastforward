fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc path");
    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc);
    config
        .compile_protos(&["proto/otap.proto"], &["proto"])
        .expect("compile OTAP protobuf schema");
}
