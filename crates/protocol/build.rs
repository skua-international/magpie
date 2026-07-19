fn main() {
    connectrpc_build::Config::new()
        .files(&[
            "../../proto/sync/v1/sync.proto",
            "../../proto/controller/v1/controller.proto",
            "../../proto/registry/v1/registry.proto",
        ])
        .includes(&["../../proto"])
        .include_file("_connectrpc.rs")
        .compile()
        .unwrap();
}
