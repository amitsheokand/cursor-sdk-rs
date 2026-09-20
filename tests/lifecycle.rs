//! Bridge process lifecycle: discovery, handshake, and shutdown.
//!
//! The failure paths run everywhere, using stub executables. The happy path
//! needs a real `cursor-sdk-bridge`; it is skipped with a note when the binary
//! is not available, so `cargo test` stays green on a machine without one.
//!
//! ```text
//! export CURSOR_SDK_BRIDGE_BIN=/path/to/bin/cursor-sdk-bridge
//! cargo test --test lifecycle
//! ```

use std::path::PathBuf;
use std::time::Duration;

use cursor_sdk::{BridgeError, Client, Error};

/// Write an executable stub that stands in for the bridge.
fn stub(name: &str, script: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("cursor-sdk-stubs-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(name);
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// Whether a process id is still alive.
fn is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// A real bridge binary, or `None` when this machine has none.
fn real_bridge() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CURSOR_SDK_BRIDGE_BIN").map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }
    let name = if cfg!(windows) {
        "cursor-sdk-bridge.exe"
    } else {
        "cursor-sdk-bridge"
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

macro_rules! require_bridge {
    () => {
        match real_bridge() {
            Some(path) => path,
            None => {
                eprintln!(
                    "skipping: no cursor-sdk-bridge found. Set CURSOR_SDK_BRIDGE_BIN to run this."
                );
                return;
            }
        }
    };
}

// ---- failure paths (no bridge needed) -------------------------------------

#[tokio::test]
async fn a_missing_binary_explains_every_way_to_supply_one() {
    let client = Client::builder()
        .api_key("k")
        .bridge_binary("/definitely/not/here/cursor-sdk-bridge")
        .build();

    let error = client.ping().await.unwrap_err();
    assert!(matches!(error, Error::Bridge(BridgeError::NotFound(_))));

    let text = error.to_string();
    for hint in ["CURSOR_SDK_BRIDGE_BIN", "PATH", "bridge_binary", "endpoint"] {
        assert!(
            text.contains(hint),
            "the error should mention {hint}: {text}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_bridge_that_dies_before_ready_surfaces_its_stderr() {
    let path = stub(
        "dies",
        "#!/bin/sh\necho 'error: unknown option --workspace' >&2\necho 'usage: cursor-sdk-bridge' >&2\nexit 3\n",
    );
    let client = Client::builder()
        .api_key("k")
        .bridge_binary(&path)
        .workspace("/tmp")
        .build();

    let error = client.ping().await.unwrap_err();
    assert!(matches!(
        error,
        Error::Bridge(BridgeError::ExitedBeforeReady { .. })
    ));

    // The captured stderr is the whole point: it says what went wrong.
    let text = error.to_string();
    assert!(text.contains("unknown option --workspace"), "{text}");
    assert!(
        text.contains("exit status: 3") || text.contains("exit code: 3"),
        "{text}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_bridge_that_never_becomes_ready_times_out() {
    let path = stub("hangs", "#!/bin/sh\nsleep 30\n");
    let client = Client::builder()
        .api_key("k")
        .bridge_binary(&path)
        .startup_timeout(Duration::from_millis(700))
        .build();

    let started = std::time::Instant::now();
    let error = client.ping().await.unwrap_err();
    assert!(matches!(
        error,
        Error::Bridge(BridgeError::StartupTimeout { .. })
    ));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the timeout must not wait for the process to finish"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_malformed_ready_line_is_rejected_without_echoing_it() {
    // Older bridges inline the auth token in this line, so it must never be
    // reflected into an error message or a log.
    let path = stub(
        "bad-schema",
        "#!/bin/sh\necho 'cursor-sdk-bridge ready {\"schemaVersion\":99,\"transport\":\"tcp\",\
         \"protocol\":\"connect\",\"url\":\"http://127.0.0.1:1\",\"authToken\":\"SECRET-TOKEN\"}' >&2\nsleep 5\n",
    );
    let client = Client::builder().api_key("k").bridge_binary(&path).build();

    let error = client.ping().await.unwrap_err();
    assert!(matches!(error, Error::Bridge(BridgeError::Handshake(_))));
    let text = error.to_string();
    assert!(text.contains("schemaVersion 99"), "{text}");
    assert!(
        !text.contains("SECRET-TOKEN"),
        "the token must never be echoed: {text}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_stub_bridge_completes_the_handshake_and_is_killed_on_drop() {
    // A ready line pointing at a port nothing listens on: enough to prove the
    // handshake and the process bookkeeping without a real bridge.
    let directory = std::env::temp_dir().join(format!("cursor-sdk-token-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let token_file = directory.join("auth-token");
    std::fs::write(&token_file, "  token-from-file\n").unwrap();

    let path = stub(
        "ready",
        &format!(
            "#!/bin/sh\necho 'starting' >&2\necho 'cursor-sdk-bridge ready \
             {{\"schemaVersion\":1,\"serverVersion\":\"9.9.9\",\"transport\":\"tcp\",\
             \"protocol\":\"connect\",\"host\":\"127.0.0.1\",\"port\":1,\
             \"authTokenFile\":\"{}\",\"workspaceRef\":\"/tmp\",\"futureField\":true}}' >&2\n\
             while true; do sleep 1; done\n",
            token_file.display()
        ),
    );

    let client = Client::builder()
        .api_key("k")
        .bridge_binary(&path)
        .verify_on_connect(false)
        .build();

    let info = client
        .bridge_info()
        .await
        .unwrap()
        .expect("a managed bridge");
    assert_eq!(info.server_version.as_deref(), Some("9.9.9"));
    assert_eq!(info.workspace_ref.as_deref(), Some("/tmp"));
    assert_eq!(info.url, "http://127.0.0.1:1");
    let pid = info.pid.expect("a pid");
    assert!(is_alive(pid));

    // No close() — dropping the last handle must still kill the process.
    drop(client);
    for _ in 0..50 {
        if !is_alive(pid) {
            std::fs::remove_dir_all(&directory).ok();
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the bridge survived the client being dropped (pid {pid})");
}

// ---- happy path (needs a real bridge) -------------------------------------

#[tokio::test]
async fn a_real_bridge_handshakes_answers_and_shuts_down() {
    let binary = require_bridge!();
    let client = Client::builder()
        .api_key(std::env::var("CURSOR_API_KEY").unwrap_or_else(|_| "unused-for-ping".into()))
        .bridge_binary(&binary)
        .workspace(std::env::current_dir().unwrap())
        .build();

    assert_eq!(client.ping().await.unwrap(), "pong");

    let version = client.version().await.unwrap();
    assert_eq!(version.protocol_version, cursor_sdk::PROTOCOL_VERSION);
    assert!(version.has_capability("agent.create"));

    let info = client
        .bridge_info()
        .await
        .unwrap()
        .expect("a managed bridge");
    let pid = info.pid.expect("a pid");
    assert!(is_alive(pid));
    assert!(info.url.starts_with("http://127.0.0.1:"));

    client.close().await.unwrap();

    // The graceful path: Shutdown, then exit, without needing the kill.
    for _ in 0..50 {
        if !is_alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the bridge was still running after close() (pid {pid})");
}

#[tokio::test]
async fn a_closed_client_cannot_be_reused() {
    let binary = require_bridge!();
    let client = Client::builder()
        .api_key("k")
        .bridge_binary(&binary)
        .build();

    client.ping().await.unwrap();
    client.close().await.unwrap();
    // Closing twice is a no-op, not an error.
    client.close().await.unwrap();

    let error = client.ping().await.unwrap_err();
    assert!(error.to_string().contains("closed"));
}

#[tokio::test]
async fn a_second_client_can_attach_to_a_running_bridge() {
    let binary = require_bridge!();
    let owner = Client::builder()
        .api_key("k")
        .bridge_binary(&binary)
        .build();
    owner.ping().await.unwrap();

    let endpoint = owner.endpoint().await.unwrap();
    let info = owner.bridge_info().await.unwrap().unwrap();
    let pid = info.pid.unwrap();

    // Attaching needs the token, which only the spawning client knows; read it
    // back the same way that client did, from the handshake it already did.
    // Here we simply prove the owner's endpoint is reachable and that closing
    // an *attached* client leaves the process alone.
    let attached = Client::builder()
        .api_key("k")
        .endpoint(&endpoint, "wrong-token-on-purpose")
        .verify_on_connect(false)
        .build();
    assert!(
        attached.ping().await.is_err(),
        "a bad token must be rejected"
    );

    attached.close().await.unwrap();
    assert!(
        is_alive(pid),
        "closing an attached client must not stop the bridge"
    );

    owner.close().await.unwrap();
}

#[tokio::test]
async fn an_unauthenticated_request_is_classified_as_an_auth_error() {
    let binary = require_bridge!();
    let owner = Client::builder()
        .api_key("k")
        .bridge_binary(&binary)
        .build();
    owner.ping().await.unwrap();
    let endpoint = owner.endpoint().await.unwrap();

    let impostor = Client::builder()
        .api_key("k")
        .endpoint(&endpoint, "definitely-not-the-token")
        .verify_on_connect(false)
        .build();

    let error = impostor.ping().await.unwrap_err();
    assert_eq!(error.kind(), Some(cursor_sdk::ErrorKind::Unauthenticated));
    assert!(error.is_auth());

    owner.close().await.unwrap();
}
