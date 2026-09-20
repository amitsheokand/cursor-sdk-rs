//! Installs the `cursor-sdk-bridge` binary the `cursor-sdk-rs` crate drives.
//!
//! Provisioning lives in this tool rather than in the library so that the
//! library keeps a dependency tree with no TLS stack in it. The install path
//! is the one `cursor-sdk-rs` already searches, so a successful run needs no
//! follow-up configuration.

use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sha2::{Digest, Sha256};

/// The bridge release this tool installs unless told otherwise. It matches the
/// `sdk.v1` contract vendored by the `cursor-sdk-rs` crate of the same version.
const DEFAULT_VERSION: &str = "1.0.31";

/// SHA-256 of every release archive, keyed by version then `<os>-<arch>`.
///
/// Compiled in on purpose. See the security note in this crate's README: a
/// checksum fetched from the release it validates only detects corruption,
/// because it arrives over the same connection from the same origin as the
/// archive it is vouching for.
const PINNED: &[(&str, &[(&str, &str)])] = &[(
    "1.0.31",
    &[
        (
            "darwin-arm64",
            "6978358fa72aa511108d682aecaf29940df0708711f838cddc51e6049b2031ba",
        ),
        (
            "darwin-x64",
            "13f383cf4be71c981bfc32c1216521d64928b7e7d0939f213469cce633601fed",
        ),
        (
            "linux-arm64",
            "c5b3dce52ba01f60b152861f008e8d2c931c0ba9fbf79a110ddea375ae40717b",
        ),
        (
            "linux-x64",
            "527cbebdc6aad4ea7d3026f49b4879e3e7f3d6e907c0598241863802b022c838",
        ),
        (
            "win32-x64",
            "7121271f4dc4802d16530e25446df60361a5432adf69db454785131139e63ce9",
        ),
    ],
)];

const HELP: &str = "\
Installs the cursor-sdk-bridge binary that the cursor-sdk-rs crate drives.

USAGE:
    cursor-sdk-bridge-fetch [OPTIONS]

OPTIONS:
    --version <TAG>    Bridge release to install (default: {DEFAULT})
    --dest <DIR>       Install root (default: ~/.cursor/sdk-bridge)
    --force            Reinstall even when a bridge is already present
    --print-path       Print the executable path and exit without downloading
    --list             List the releases and platforms with pinned checksums
    --allow-unpinned   Install a release with no pinned checksum (unverified)
    --quiet            Only print errors
    -h, --help         Print this help

The install location is one the cursor-sdk-rs crate already searches, so a
successful run needs no further configuration.
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

struct Options {
    version: String,
    dest: PathBuf,
    force: bool,
    print_path: bool,
    list: bool,
    allow_unpinned: bool,
    quiet: bool,
}

fn run() -> Result<ExitCode, String> {
    let options = match parse_args()? {
        Some(options) => options,
        // --help printed already.
        None => return Ok(ExitCode::SUCCESS),
    };

    if options.list {
        print_pinned();
        return Ok(ExitCode::SUCCESS);
    }

    let platform = host_platform()?;
    let executable = executable_path(&options.dest);

    if options.print_path {
        println!("{}", executable.display());
        return Ok(ExitCode::SUCCESS);
    }

    if executable.is_file() && !options.force {
        if !options.quiet {
            println!(
                "cursor-sdk-bridge is already installed at {}",
                executable.display()
            );
            if let Some(version) = installed_version(&options.dest) {
                println!("  installed version: {version}");
            }
            println!("  pass --force to reinstall");
        }
        return Ok(ExitCode::SUCCESS);
    }

    install(&options, &platform, &executable)?;
    Ok(ExitCode::SUCCESS)
}

fn parse_args() -> Result<Option<Options>, String> {
    let mut options = Options {
        version: DEFAULT_VERSION.to_string(),
        dest: default_dest()?,
        force: false,
        print_path: false,
        list: false,
        allow_unpinned: false,
        quiet: false,
    };

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| -> Result<String, String> {
            args.next()
                .ok_or_else(|| format!("{flag} needs a value. Try --help"))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{}", HELP.replace("{DEFAULT}", DEFAULT_VERSION));
                return Ok(None);
            }
            "--version" => {
                options.version = value("--version")?.trim_start_matches('v').to_string()
            }
            "--dest" => options.dest = PathBuf::from(value("--dest")?),
            "--force" => options.force = true,
            "--print-path" => options.print_path = true,
            "--list" => options.list = true,
            "--allow-unpinned" => options.allow_unpinned = true,
            "--quiet" => options.quiet = true,
            other => {
                return Err(format!("unrecognized argument {other:?}. Try --help"));
            }
        }
    }
    Ok(Some(options))
}

/// The platform token pair the release archives are named with.
///
/// The archives use Node's vocabulary, not Rust's: `darwin` rather than
/// `macos`, `x64` rather than `x86_64`.
fn host_platform() -> Result<String, String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        "linux" => "linux",
        other => return Err(format!("no cursor-sdk-bridge build exists for {other}")),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => {
            return Err(format!(
                "no cursor-sdk-bridge build exists for {os} on {other}"
            ))
        }
    };
    if os == "win32" && arch != "x64" {
        return Err("Windows builds of cursor-sdk-bridge are x64 only".to_string());
    }
    Ok(format!("{os}-{arch}"))
}

fn default_dest() -> Result<PathBuf, String> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|value| !value.is_empty())
        .ok_or("could not determine the home directory; pass --dest")?;
    Ok(PathBuf::from(home).join(".cursor").join("sdk-bridge"))
}

fn executable_path(dest: &Path) -> PathBuf {
    let name = if cfg!(windows) {
        "cursor-sdk-bridge.exe"
    } else {
        "cursor-sdk-bridge"
    };
    dest.join("bin").join(name)
}

/// Read `sdkVersion` out of an installed archive's manifest, for reporting.
fn installed_version(dest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dest.join("manifest.json")).ok()?;
    // A deliberate pinhole rather than a JSON dependency: this is a cosmetic
    // line in a status message.
    let (_, rest) = text.split_once("\"sdkVersion\"")?;
    let (_, rest) = rest.split_once('"')?;
    let (version, _) = rest.split_once('"')?;
    Some(version.to_string())
}

fn pinned_checksum(version: &str, platform: &str) -> Option<&'static str> {
    PINNED
        .iter()
        .find(|(tag, _)| *tag == version)?
        .1
        .iter()
        .find(|(name, _)| *name == platform)
        .map(|(_, digest)| *digest)
}

fn print_pinned() {
    println!("Releases with checksums pinned in this tool:\n");
    for (version, platforms) in PINNED {
        println!("  {version}");
        for (platform, digest) in *platforms {
            println!("    {platform:<14} {digest}");
        }
    }
    println!("\nAnything else needs --allow-unpinned, which verifies nothing.");
}

fn install(options: &Options, platform: &str, executable: &Path) -> Result<(), String> {
    let archive_name = format!("cursor-sdk-bridge-standalone-{platform}.tar.gz");
    let url = format!(
        "https://github.com/cursor/sdk-bridge/releases/download/v{}/{archive_name}",
        options.version
    );

    let expected = match pinned_checksum(&options.version, platform) {
        Some(digest) => Some(digest),
        None if options.allow_unpinned => {
            eprintln!(
                "warning: no checksum is pinned for {} on {platform}; the download will NOT be \
                 verified.\n         Run --list to see which releases are pinned.",
                options.version
            );
            None
        }
        None => {
            return Err(format!(
                "no checksum is pinned for version {} on {platform}.\n       This tool pins {}. \
                 Install that instead, upgrade cursor-sdk-bridge-fetch, or pass --allow-unpinned \
                 to install without verification.",
                options.version, DEFAULT_VERSION
            ));
        }
    };

    // Stage next to the destination so the final move is a rename on the same
    // filesystem, not a copy that can half-finish.
    let parent = options
        .dest
        .parent()
        .ok_or("the destination has no parent directory")?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;

    let staging = parent.join(format!(".cursor-sdk-bridge-staging-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|error| format!("could not create {}: {error}", staging.display()))?;

    let result = (|| -> Result<(), String> {
        let archive = staging.join(&archive_name);
        let digest = download(&url, &archive, options.quiet)?;

        if let Some(expected) = expected {
            if !digest.eq_ignore_ascii_case(expected) {
                return Err(format!(
                    "checksum mismatch for {archive_name}\n       expected {expected}\n       \
                     got      {digest}\n       The download was discarded. This is either a \
                     corrupted transfer or a tampered asset; retry, and if it persists do not \
                     use the file."
                ));
            }
            if !options.quiet {
                println!("  checksum verified");
            }
        }

        extract(&archive, &staging)?;
        std::fs::remove_file(&archive).ok();

        let staged_executable = executable_path(&staging);
        if !staged_executable.is_file() {
            return Err(format!(
                "the archive did not contain {}",
                staged_executable
                    .strip_prefix(&staging)
                    .unwrap_or(&staged_executable)
                    .display()
            ));
        }
        make_executable(&staged_executable)?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }

    swap_into_place(&staging, &options.dest)?;

    if !options.quiet {
        println!("\nInstalled cursor-sdk-bridge {} to", options.version);
        println!("  {}", executable.display());
        println!("\nThe cursor-sdk-rs crate searches this location, so nothing else is needed.");
    }
    Ok(())
}

fn download(url: &str, destination: &Path, quiet: bool) -> Result<String, String> {
    if !quiet {
        println!("Downloading {url}");
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!(
            "cursor-sdk-bridge-fetch/",
            env!("CARGO_PKG_VERSION")
        ))
        // Release assets redirect to a CDN host.
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(None)
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| format!("could not build an HTTP client: {error}"))?;

    let mut response = client
        .get(url)
        .send()
        .map_err(|error| format!("the download failed: {error}"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(format!(
            "no such release asset (404).\n       {url}\n       Check the version with --list."
        ));
    }
    if !response.status().is_success() {
        return Err(format!(
            "the download failed with HTTP {}",
            response.status()
        ));
    }

    // A carriage-return progress bar is unreadable once it is piped into a
    // file or a CI log, so only draw one for a terminal.
    let progress = !quiet && io::stdout().is_terminal();
    let total = response.content_length();
    let mut file = File::create(destination)
        .map_err(|error| format!("could not write the download: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    let mut last_report = std::time::Instant::now();

    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|error| format!("the download was interrupted: {error}"))?;
        if read == 0 {
            break;
        }
        // Hash the same bytes that land on disk, so the digest cannot drift
        // from the file it is vouching for.
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|error| format!("could not write the download: {error}"))?;
        written += read as u64;

        if progress && last_report.elapsed() >= std::time::Duration::from_millis(250) {
            report_progress(written, total);
            last_report = std::time::Instant::now();
        }
    }
    file.flush()
        .map_err(|error| format!("could not flush the download: {error}"))?;

    if let Some(total) = total {
        if written != total {
            return Err(format!(
                "the download ended early: got {written} of {total} bytes"
            ));
        }
    }
    if progress {
        report_progress(written, total);
        println!();
    } else if !quiet {
        println!("  downloaded {:.1} MiB", written as f64 / (1024.0 * 1024.0));
    }

    Ok(format!("{:x}", hasher.finalize()))
}

fn report_progress(written: u64, total: Option<u64>) {
    let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
    match total {
        Some(total) if total > 0 => {
            let percent = (written as f64 / total as f64 * 100.0).min(100.0);
            print!(
                "\r  {:.1} / {:.1} MiB  ({percent:.0}%)   ",
                mib(written),
                mib(total)
            );
        }
        _ => print!("\r  {:.1} MiB   ", mib(written)),
    }
    let _ = io::stdout().flush();
}

fn extract(archive: &Path, into: &Path) -> Result<(), String> {
    let file =
        File::open(archive).map_err(|error| format!("could not open the archive: {error}"))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);

    for entry in tar
        .entries()
        .map_err(|error| format!("the archive could not be read: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("the archive could not be read: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("the archive has an unreadable entry name: {error}"))?
            .into_owned();

        // Refuse absolute paths and `..` traversal rather than trusting the
        // archive to stay inside the staging directory.
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(format!(
                "the archive contains an unsafe path: {}",
                path.display()
            ));
        }

        entry
            .unpack(into.join(&path))
            .map_err(|error| format!("could not extract {}: {error}", path.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .map_err(|error| format!("could not stat {}: {error}", path.display()))?
        .permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    std::fs::set_permissions(path, permissions)
        .map_err(|error| format!("could not make {} executable: {error}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Move the staged tree into place, keeping any previous install recoverable
/// until the new one is committed.
fn swap_into_place(staging: &Path, dest: &Path) -> Result<(), String> {
    let parent = dest.parent().ok_or("the destination has no parent")?;
    let retired = parent.join(format!(".cursor-sdk-bridge-old-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&retired);

    let had_previous = dest.exists();
    if had_previous {
        std::fs::rename(dest, &retired).map_err(|error| {
            format!(
                "could not move the existing install aside from {}: {error}",
                dest.display()
            )
        })?;
    }

    match std::fs::rename(staging, dest) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&retired);
            Ok(())
        }
        Err(error) => {
            // Put the old install back rather than leaving nothing behind.
            if had_previous {
                let _ = std::fs::rename(&retired, dest);
            }
            let _ = std::fs::remove_dir_all(staging);
            Err(format!("could not install to {}: {error}", dest.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_version_is_pinned_for_every_platform() {
        let platforms = PINNED
            .iter()
            .find(|(tag, _)| *tag == DEFAULT_VERSION)
            .expect("the default version must be pinned")
            .1;
        for expected in [
            "darwin-arm64",
            "darwin-x64",
            "linux-arm64",
            "linux-x64",
            "win32-x64",
        ] {
            assert!(
                platforms.iter().any(|(name, _)| *name == expected),
                "missing a checksum for {expected}"
            );
        }
    }

    #[test]
    fn every_pinned_checksum_is_a_sha256() {
        for (version, platforms) in PINNED {
            for (platform, digest) in *platforms {
                assert_eq!(digest.len(), 64, "{version} {platform} is not 64 hex chars");
                assert!(
                    digest.chars().all(|c| c.is_ascii_hexdigit()),
                    "{version} {platform} is not hex"
                );
            }
        }
    }

    #[test]
    fn the_host_platform_uses_the_release_vocabulary() {
        let platform = host_platform().expect("this host has a bridge build");
        assert!(
            pinned_checksum(DEFAULT_VERSION, &platform).is_some(),
            "no pinned checksum for this host's platform ({platform})"
        );
        assert!(!platform.contains("x86_64"), "x86_64 must map to x64");
        assert!(!platform.contains("macos"), "macos must map to darwin");
    }

    #[test]
    fn an_unpinned_version_has_no_checksum() {
        assert!(pinned_checksum("0.0.1", "linux-x64").is_none());
    }

    #[test]
    fn the_executable_path_matches_what_the_sdk_searches() {
        let path = executable_path(Path::new("/home/me/.cursor/sdk-bridge"));
        assert!(path.ends_with(if cfg!(windows) {
            "bin/cursor-sdk-bridge.exe"
        } else {
            "bin/cursor-sdk-bridge"
        }));
    }

    #[test]
    fn the_installed_version_is_read_from_a_manifest() {
        let directory = std::env::temp_dir().join(format!("cs-fetch-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("manifest.json"),
            r#"{"bridgeVersion":"1.0.0","sdkVersion":"1.0.31","os":"linux"}"#,
        )
        .unwrap();
        assert_eq!(installed_version(&directory).as_deref(), Some("1.0.31"));
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
