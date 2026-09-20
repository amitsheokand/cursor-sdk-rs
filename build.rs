//! Codegen for the vendored `sdk.v1` contract.
//!
//! Uses `protox` (a pure-Rust protobuf compiler) so building this crate does
//! not require a `protoc` binary on the host. The protos under `proto/` are
//! copied verbatim from a `cursor/sdk-bridge` release tag and must not be
//! edited; see `proto/manifest.json` for the pinned version.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let files = [
        "sdk/v1/sdk_messages.proto",
        "sdk/v1/sdk_errors.proto",
        "sdk/v1/sdk_agent_service.proto",
        "sdk/v1/sdk_cursor_service.proto",
        "sdk/v1/sdk_bridge_control_service.proto",
        "sdk/v1/sdk_custom_tool_callback_service.proto",
        "sdk/v1/sdk_store_callback_service.proto",
    ];
    let include = PathBuf::from("proto");

    for file in &files {
        println!("cargo:rerun-if-changed=proto/{file}");
    }

    let descriptors = protox::compile(files, [&include])?;

    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let mut config = prost_build::Config::new();
    config
        .out_dir(&out_dir)
        .skip_protoc_run()
        .bytes([".sdk.v1.DownloadArtifactChunk.data"])
        .compile_well_known_types()
        .extern_path(".google.protobuf.Struct", "::prost_types::Struct")
        .extern_path(".google.protobuf.Value", "::prost_types::Value")
        .extern_path(".google.protobuf.ListValue", "::prost_types::ListValue")
        .extern_path(".google.protobuf.NullValue", "::prost_types::NullValue")
        .extern_path(".google.protobuf.Timestamp", "::prost_types::Timestamp")
        .extern_path(".google.protobuf.Duration", "::prost_types::Duration")
        .type_attribute(".", "#[allow(clippy::doc_overindented_list_items)]");
    config.compile_fds(descriptors)?;
    Ok(())
}
