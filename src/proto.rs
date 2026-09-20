//! Generated `sdk.v1` protobuf types.
//!
//! This module is the *wire* layer. It is public so that advanced callers can
//! reach the raw contract (and so the callback servers can be implemented
//! outside this crate), but the ergonomic API in [`crate::types`] and the
//! handles in [`crate::Agent`] / [`crate::Run`] never require it.
//!
//! The protos are vendored verbatim from a `cursor/sdk-bridge` release; see
//! `proto/manifest.json` for the pinned `sdkVersion`.

#![allow(clippy::all)]
// Doc comments here are the proto comments verbatim; the protos are vendored
// and must not be edited.
#![allow(clippy::doc_markdown)]
#![allow(missing_docs)]

include!(concat!(env!("OUT_DIR"), "/sdk.v1.rs"));

/// Protobuf package name, and the first path segment of every RPC route.
pub const PACKAGE: &str = "sdk.v1";

/// The protocol version this crate's generated code was built against.
///
/// Compare against [`crate::BridgeVersion::protocol_version`] at runtime.
pub const PROTOCOL_VERSION: &str = "sdk.v1";

/// The `@cursor/sdk` version of the vendored contract (`proto/manifest.json`).
pub const CONTRACT_SDK_VERSION: &str = "1.0.31";
