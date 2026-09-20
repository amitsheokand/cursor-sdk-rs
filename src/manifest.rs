//! The bridge archive's `manifest.json`, and the preflight check on it.
//!
//! A standalone bridge archive unpacks flat:
//!
//! ```text
//! bin/cursor-sdk-bridge      # the executable
//! proto/sdk/v1/              # the contract this binary implements
//! manifest.json              # what this archive is
//! ```
//!
//! The manifest says which platform the binary was built for and which
//! protocol it speaks, so a mismatch can be caught *before* spawning rather
//! than as a confusing exec failure or a handshake error afterwards.
//!
//! The check is deliberately soft: a bridge installed some other way — the
//! copy `pip install cursor-sdk` puts on `PATH`, for instance — has no
//! manifest beside it, and that is not a problem. Only a manifest that is
//! present *and* says something wrong is an error.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{BridgeError, Result};

/// Set to `1` to skip the preflight entirely.
const SKIP_ENV: &str = "CURSOR_SDK_BRIDGE_SKIP_MANIFEST_CHECK";

/// What a bridge archive says about itself.
///
/// Every field is optional: manifests gain fields over time, and this struct
/// tolerates both older and newer ones.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct BridgeManifest {
    /// Version of the bridge build.
    pub bridge_version: Option<String>,
    /// The `@cursor/sdk` version this bridge embeds.
    pub sdk_version: Option<String>,
    /// Platform the binary was built for: `linux`, `darwin`, or `win32`.
    pub os: Option<String>,
    /// Architecture the binary was built for: `x64` or `arm64`.
    pub arch: Option<String>,
    /// Contract the bridge implements. Expected to be `sdk.v1`.
    pub protocol: Option<String>,
    /// Executable path relative to the archive root.
    pub entrypoint: Option<String>,
    /// How the archive was packaged, for example `standalone`.
    pub distribution: Option<String>,
    /// The JavaScript runtime baked into the binary.
    pub runtime: Option<String>,
}

impl BridgeManifest {
    /// Where the manifest would live for a binary at `binary`.
    ///
    /// The executable sits at `<root>/bin/cursor-sdk-bridge`, so the manifest
    /// is two levels up. Returns `None` when the path has no such ancestor.
    pub fn path_for_binary(binary: &Path) -> Option<PathBuf> {
        Some(binary.parent()?.parent()?.join("manifest.json"))
    }

    /// Read the manifest beside a bridge binary, if there is one.
    ///
    /// `Ok(None)` means no manifest was found, which is normal for a bridge
    /// that did not come from a standalone archive. Unreadable or malformed
    /// JSON is also `Ok(None)`: the manifest is a diagnostic aid, and refusing
    /// to launch a working bridge over it would be worse than ignoring it.
    pub async fn for_binary(binary: &Path) -> Option<Self> {
        let path = Self::path_for_binary(binary)?;
        let text = tokio::fs::read_to_string(&path).await.ok()?;
        match serde_json::from_str(&text) {
            Ok(manifest) => Some(manifest),
            Err(error) => {
                tracing::debug!(
                    target: "cursor_sdk::bridge",
                    path = %path.display(),
                    %error,
                    "ignoring an unreadable bridge manifest"
                );
                None
            }
        }
    }

    /// Whether this archive advertises the contract this crate was built from.
    pub fn speaks_supported_protocol(&self) -> bool {
        match &self.protocol {
            Some(protocol) => protocol == crate::proto::PROTOCOL_VERSION,
            None => true,
        }
    }
}

/// The platform token the bridge uses for this host's operating system.
///
/// The archives use Node's vocabulary (`darwin`, `win32`), not Rust's
/// (`macos`, `windows`).
pub fn host_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// The platform token the bridge uses for this host's architecture.
///
/// The archives use `x64` / `arm64`, not `x86_64` / `aarch64`.
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Check a bridge binary's manifest before spawning it.
///
/// Errors on a mismatch that cannot work; warns on one that merely might not
/// be what you intended.
///
/// | Field | On mismatch |
/// | --- | --- |
/// | `protocol` | Error. A different contract is not something this crate can speak. |
/// | `os` | Error. A Linux binary cannot exec on macOS. |
/// | `arch` | Warning only. Rosetta and qemu make this legitimately common. |
/// | `sdkVersion` | Warning only. `sdk.v1` changes additively. |
pub(crate) async fn preflight(binary: &Path) -> Result<Option<BridgeManifest>> {
    if std::env::var_os(SKIP_ENV).is_some_and(|value| value == "1") {
        tracing::debug!(target: "cursor_sdk::bridge", "{SKIP_ENV}=1; skipping the manifest check");
        return Ok(None);
    }

    let Some(manifest) = BridgeManifest::for_binary(binary).await else {
        return Ok(None);
    };

    let fail = |problem: String| BridgeError::Manifest {
        path: binary.display().to_string(),
        problem,
    };

    if let Some(protocol) = manifest.protocol.as_deref() {
        if protocol != crate::proto::PROTOCOL_VERSION {
            return Err(fail(format!(
                "it implements protocol {protocol:?}, but this SDK speaks {:?}. Install a bridge \
                 for the matching contract, or upgrade the cursor-sdk-rs crate",
                crate::proto::PROTOCOL_VERSION
            ))
            .into());
        }
    }

    if let Some(os) = manifest.os.as_deref() {
        if os != host_os() {
            return Err(fail(format!(
                "it was built for {os:?}, but this host is {:?}. Download the \
                 cursor-sdk-bridge-standalone-{}-{} archive, or run `cursor-sdk-bridge-fetch`",
                host_os(),
                host_os(),
                host_arch(),
            ))
            .into());
        }
    }

    // An architecture mismatch is survivable and sometimes deliberate: an
    // x64 bridge runs fine on Apple Silicon under Rosetta, and under qemu on
    // Linux. Say something, then get out of the way.
    if let Some(arch) = manifest.arch.as_deref() {
        if arch != host_arch() {
            tracing::warn!(
                target: "cursor_sdk::bridge",
                bridge_arch = %arch,
                host_arch = %host_arch(),
                "the bridge was built for a different architecture; it will run through emulation \
                 if this host supports it"
            );
        }
    }

    if let Some(sdk_version) = manifest.sdk_version.as_deref() {
        if sdk_version != crate::proto::CONTRACT_SDK_VERSION {
            tracing::debug!(
                target: "cursor_sdk::bridge",
                bridge_sdk_version = %sdk_version,
                contract_sdk_version = %crate::proto::CONTRACT_SDK_VERSION,
                "the bridge embeds a different SDK version than this crate's vendored contract; \
                 sdk.v1 changes additively, so this is usually fine"
            );
        }
    }

    Ok(Some(manifest))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn manifest_dir(name: &str, body: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("cursor-sdk-manifest-{}-{name}", std::process::id()));
        tokio::fs::create_dir_all(root.join("bin")).await.unwrap();
        tokio::fs::write(root.join("manifest.json"), body)
            .await
            .unwrap();
        tokio::fs::write(root.join("bin").join("cursor-sdk-bridge"), b"stub")
            .await
            .unwrap();
        root
    }

    fn manifest_json(overrides: serde_json::Value) -> String {
        let mut base = serde_json::json!({
            "bridgeVersion": "1.0.0",
            "sdkVersion": crate::proto::CONTRACT_SDK_VERSION,
            "os": host_os(),
            "arch": host_arch(),
            "entrypoint": "bin/cursor-sdk-bridge",
            "protocol": "sdk.v1",
            "distribution": "standalone",
            "runtime": "bun-1.3.9",
        });
        for (key, value) in overrides.as_object().unwrap() {
            base[key] = value.clone();
        }
        base.to_string()
    }

    #[tokio::test]
    async fn a_matching_manifest_passes_and_is_returned() {
        let root = manifest_dir("ok", &manifest_json(serde_json::json!({}))).await;
        let binary = root.join("bin").join("cursor-sdk-bridge");

        let manifest = preflight(&binary).await.unwrap().expect("a manifest");
        assert_eq!(manifest.bridge_version.as_deref(), Some("1.0.0"));
        assert_eq!(manifest.runtime.as_deref(), Some("bun-1.3.9"));
        assert!(manifest.speaks_supported_protocol());

        tokio::fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn a_different_protocol_is_an_error() {
        let root = manifest_dir(
            "proto",
            &manifest_json(serde_json::json!({"protocol": "sdk.v2"})),
        )
        .await;
        let binary = root.join("bin").join("cursor-sdk-bridge");

        let error = preflight(&binary).await.unwrap_err();
        assert!(matches!(
            error,
            crate::Error::Bridge(BridgeError::Manifest { .. })
        ));
        let text = error.to_string();
        assert!(text.contains("sdk.v2"), "{text}");
        assert!(text.contains("sdk.v1"), "{text}");

        tokio::fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn the_wrong_operating_system_is_an_error() {
        let wrong = if host_os() == "linux" {
            "darwin"
        } else {
            "linux"
        };
        let root = manifest_dir("os", &manifest_json(serde_json::json!({"os": wrong}))).await;
        let binary = root.join("bin").join("cursor-sdk-bridge");

        let error = preflight(&binary).await.unwrap_err();
        let text = error.to_string();
        assert!(text.contains(wrong), "{text}");
        assert!(text.contains("cursor-sdk-bridge-fetch"), "{text}");

        tokio::fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn a_different_architecture_only_warns() {
        // An x64 bridge on Apple Silicon runs under Rosetta; rejecting it
        // would break a setup that genuinely works.
        let wrong = if host_arch() == "arm64" {
            "x64"
        } else {
            "arm64"
        };
        let root = manifest_dir("arch", &manifest_json(serde_json::json!({"arch": wrong}))).await;
        let binary = root.join("bin").join("cursor-sdk-bridge");

        assert!(preflight(&binary).await.is_ok());

        tokio::fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn a_different_sdk_version_only_warns() {
        let root = manifest_dir(
            "sdk",
            &manifest_json(serde_json::json!({"sdkVersion": "99.0.0"})),
        )
        .await;
        let binary = root.join("bin").join("cursor-sdk-bridge");
        assert!(preflight(&binary).await.is_ok());
        tokio::fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn no_manifest_is_fine() {
        // This is the `pip install cursor-sdk` case: a bare binary on PATH.
        let root = std::env::temp_dir().join(format!("cursor-sdk-bare-{}", std::process::id()));
        tokio::fs::create_dir_all(root.join("bin")).await.unwrap();
        let binary = root.join("bin").join("cursor-sdk-bridge");
        tokio::fs::write(&binary, b"stub").await.unwrap();

        assert!(preflight(&binary).await.unwrap().is_none());

        tokio::fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn a_malformed_manifest_is_ignored_rather_than_fatal() {
        let root = manifest_dir("bad", "{ not json").await;
        let binary = root.join("bin").join("cursor-sdk-bridge");
        assert!(preflight(&binary).await.unwrap().is_none());
        tokio::fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn an_unknown_field_does_not_break_parsing() {
        let root = manifest_dir(
            "future",
            &manifest_json(serde_json::json!({"somethingNew": true})),
        )
        .await;
        let binary = root.join("bin").join("cursor-sdk-bridge");
        assert!(preflight(&binary).await.unwrap().is_some());
        tokio::fs::remove_dir_all(&root).await.unwrap();
    }

    #[test]
    fn host_tokens_use_the_bridge_vocabulary() {
        assert!(matches!(host_os(), "darwin" | "linux" | "win32"));
        assert!(!host_arch().contains('_'), "x86_64 must map to x64");
    }
}
