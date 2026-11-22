//! `codape update` against a fake release served over loopback.
//!
//! These run the real binary — a copy of it, so a successful update replaces
//! the copy and never the test build — against a tiny HTTP server that
//! publishes the same layout GitHub does: a `/releases/latest` document, the
//! archives, and `SHA256SUMS`.

use std::io::{BufRead, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use codape_cli::self_update;

/// What the fake release publishes.
struct FakeRelease {
    tag: String,
    asset_name: String,
    asset: Vec<u8>,
    /// `None` makes the release omit `SHA256SUMS` entirely.
    checksums: Option<String>,
    /// The release body, which is what `/release-notes` prints.
    notes: Option<String>,
}

impl FakeRelease {
    fn new(tag: &str, asset_name: &str, asset: Vec<u8>) -> Self {
        let digest = self::sha256(&asset);
        Self {
            tag: tag.to_string(),
            asset_name: asset_name.to_string(),
            asset,
            checksums: Some(format!("{digest}  {asset_name}\n")),
            notes: None,
        }
    }

    fn without_checksums(mut self) -> Self {
        self.checksums = None;
        self
    }

    fn with_checksum(mut self, digest: &str) -> Self {
        self.checksums = Some(format!("{digest}  {}\n", self.asset_name));
        self
    }

    fn with_notes(mut self, notes: &str) -> Self {
        self.notes = Some(notes.to_string());
        self
    }
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Serve `release` on loopback until the returned handle is dropped.
struct FakeServer {
    base: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FakeServer {
    fn start(release: FakeRelease) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let port = listener.local_addr().expect("local address").port();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_flag = std::sync::Arc::clone(&stop);

        thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut request_line = String::new();
                let _ = reader.read_line(&mut request_line);
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");

                let (status, body) = route(&release, path, port);
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });

        Self {
            base: format!("http://127.0.0.1:{port}"),
            stop,
        }
    }

    fn asset_base(&self) -> String {
        format!("{}/releases/download", self.base)
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // Unblock `incoming()` so the thread notices the flag.
        let _ = std::net::TcpStream::connect(self.base.trim_start_matches("http://"));
    }
}

fn route(release: &FakeRelease, path: &str, port: u16) -> (&'static str, Vec<u8>) {
    if path.ends_with("/releases/latest") {
        let asset_url = format!(
            "http://127.0.0.1:{port}/releases/download/{}/{}",
            release.tag, release.asset_name
        );
        let mut body = serde_json::json!({
            "tag_name": release.tag,
            "assets": [
                {"name": release.asset_name, "browser_download_url": asset_url},
                {"name": "SHA256SUMS",
                 "browser_download_url": format!("http://127.0.0.1:{port}/releases/download/{}/SHA256SUMS", release.tag)},
            ],
        });
        // Omitted when there are no notes, the way GitHub omits an empty
        // body — `/release-notes` has to handle both shapes.
        if let Some(notes) = &release.notes {
            body["body"] = serde_json::Value::String(notes.clone());
        }
        return ("200 OK", serde_json::to_vec(&body).expect("serialize"));
    }
    let suffix = format!("/{}/{}", release.tag, release.asset_name);
    if path.ends_with(&suffix) {
        return ("200 OK", release.asset.clone());
    }
    if path.ends_with(&format!("/{}/SHA256SUMS", release.tag)) {
        return match &release.checksums {
            Some(text) => ("200 OK", text.clone().into_bytes()),
            None => ("404 Not Found", b"missing".to_vec()),
        };
    }
    ("404 Not Found", b"missing".to_vec())
}

/// A copy of the built binary in a scratch directory, so an update replaces
/// the copy instead of the artifact under test.
fn copied_binary() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = PathBuf::from(env!("CARGO_BIN_EXE_codape"));
    let name = if cfg!(windows) {
        "codape.exe"
    } else {
        "codape"
    };
    let copied = dir.path().join(name);
    std::fs::copy(&source, &copied).expect("copy the binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&copied, std::fs::Permissions::from_mode(0o755))
            .expect("make the copy executable");
    }
    (dir, copied)
}

/// The bytes a release ships for this platform.
fn payload() -> (String, Vec<u8>) {
    payload_named(self_update::asset_name, self_update::archive_member)
}

/// The bytes a pre-rename release ships: `coderex-*` assets holding a
/// `coderex-*` member.
fn legacy_payload() -> (String, Vec<u8>) {
    payload_named(self_update::legacy_asset_name, |target| {
        format!("coderex-{target}")
    })
}

fn payload_named(asset: fn(&str) -> String, member: fn(&str) -> String) -> (String, Vec<u8>) {
    let target = self_update::release_target().expect("a supported platform");
    let name = asset(target);
    #[cfg(unix)]
    {
        // The archive is `codape-<target>.tar.gz` and holds
        // `codape-<target>`: the extension is on the archive, not the binary.
        // The release workflow packages it that way (`cp … dist/codape-$target`
        // then `tar czf codape-$target.tar.gz -C dist codape-$target`), and
        // building the fixture with the extension inside the archive is what
        // let a real "holds X, not X.tar.gz" failure through once.
        let script = b"#!/bin/sh\necho fake-new-version\n";
        let archive = tarball(&member(target), script);
        (name, archive)
    }
    #[cfg(windows)]
    {
        let _ = member;
        let mut bytes = std::fs::read(env!("CARGO_BIN_EXE_codape")).expect("read the binary");
        bytes.extend_from_slice(b"\nFAKE-NEW-VERSION");
        (name, bytes)
    }
}

#[cfg(unix)]
fn tarball(member: &str, body: &[u8]) -> Vec<u8> {
    use std::io::Write as _;

    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_data(&mut header, member, std::io::Cursor::new(body))
        .expect("append the member");
    let tar = builder.into_inner().expect("finish the tar");
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&tar).expect("gzip the tar");
    encoder.finish().expect("finish gzip")
}

fn run_update(binary: &Path, server: &FakeServer, args: &[&str]) -> std::process::Output {
    let config_home = tempfile::tempdir().expect("config home");
    Command::new(binary)
        .arg("update")
        .args(args)
        .env("CODAPE_API_BASE_URL", &server.base)
        .env("CODAPE_RELEASE_BASE_URL", server.asset_base())
        .env("CODAPE_DISABLE_UPDATE_CHECK", "1")
        .env("CLAUDE_CONFIG_DIR", config_home.path())
        .env("CODAPE_LOG", "off")
        .output()
        .expect("run codape update")
}

/// The check-time marker belongs to the installation, not to a directory.
///
/// It used to be written to `<cwd>/.codape/.last_version_check`, so running
/// `codape update` inside a repository created a `.codape/` there — and the
/// next session's working-tree snapshot reported it as uncommitted work.
#[test]
fn update_records_its_check_time_outside_the_working_directory() {
    let (tag_name, asset) = payload();
    let release = FakeRelease::new("v99.0.0", &tag_name, asset);
    let server = FakeServer::start(release);
    let (_dir, binary) = copied_binary();
    let worktree = tempfile::tempdir().expect("worktree");
    let config_home = worktree.path().join("config-home");

    // The switch is *not* set: this test is about the path the check writes.
    let output = Command::new(&binary)
        .arg("update")
        .arg("--check")
        .current_dir(worktree.path())
        .env("CODAPE_API_BASE_URL", &server.base)
        .env("CODAPE_RELEASE_BASE_URL", server.asset_base())
        .env("CLAUDE_CONFIG_DIR", &config_home)
        .env("CODAPE_LOG", "off")
        .output()
        .expect("run codape update");

    assert!(
        output.status.success(),
        "update --check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let written: Vec<_> = std::fs::read_dir(worktree.path())
        .expect("read the worktree")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(
        written,
        vec![std::ffi::OsString::from("config-home")],
        "the working directory gained files other than the config home"
    );
    assert!(
        config_home.join("last_version_check").exists(),
        "the check time should be in the config home"
    );
}

#[test]
fn update_check_reports_a_newer_release_without_replacing_the_binary() {
    let (tag_name, asset) = payload();
    let release = FakeRelease::new("v99.0.0", &tag_name, asset);
    let server = FakeServer::start(release);
    let (_dir, binary) = copied_binary();
    let original = std::fs::read(&binary).expect("read the copy");

    let output = run_update(&binary, &server, &["--check"]);

    assert!(
        output.status.success(),
        "update --check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("99.0.0"), "{stdout}");
    assert_eq!(
        std::fs::read(&binary).expect("read the copy"),
        original,
        "--check replaced the binary"
    );
}

#[test]
fn update_downloads_verifies_and_replaces_the_binary() {
    let (tag_name, asset) = payload();
    let release = FakeRelease::new("v99.0.0", &tag_name, asset);
    let server = FakeServer::start(release);
    let (_dir, binary) = copied_binary();

    let output = run_update(&binary, &server, &[]);

    assert!(
        output.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Updated"), "{stdout}");

    #[cfg(unix)]
    {
        let replaced = std::fs::read(&binary).expect("read the replaced binary");
        assert_eq!(replaced, b"#!/bin/sh\necho fake-new-version\n");
        let run = Command::new(&binary).output().expect("run the new binary");
        assert_eq!(
            String::from_utf8_lossy(&run.stdout).trim(),
            "fake-new-version"
        );
    }
    #[cfg(windows)]
    {
        let replaced = std::fs::read(&binary).expect("read the replaced binary");
        assert!(replaced.ends_with(b"\nFAKE-NEW-VERSION"));
    }
}

/// A release published before the rename ships `coderex-*` assets. A binary
/// built from this tree must still be able to install it.
#[test]
fn update_installs_a_release_published_under_the_legacy_names() {
    let (tag_name, asset) = legacy_payload();
    let release = FakeRelease::new("v99.0.0", &tag_name, asset);
    let server = FakeServer::start(release);
    let (_dir, binary) = copied_binary();

    let output = run_update(&binary, &server, &[]);

    assert!(
        output.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Updated"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    #[cfg(unix)]
    assert_eq!(
        std::fs::read(&binary).expect("read the replaced binary"),
        b"#!/bin/sh\necho fake-new-version\n"
    );
}

/// Up to date means up to date: when the latest release is not newer, the
/// check must not say one is available.
#[test]
fn update_check_says_up_to_date_when_the_latest_is_the_same() {
    let (tag_name, asset) = payload();
    let release = FakeRelease::new("v0.1.0", &tag_name, asset);
    let server = FakeServer::start(release);
    let (_dir, binary) = copied_binary();

    let output = run_update(&binary, &server, &["--check"]);

    assert!(
        output.status.success(),
        "update --check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("up to date"), "{stdout}");
    assert!(
        !stdout.contains("New version available"),
        "it claimed a newer release for the same version:\n{stdout}"
    );
}

#[test]
fn a_bad_checksum_leaves_the_binary_untouched() {
    let (tag_name, asset) = payload();
    let release = FakeRelease::new("v99.0.0", &tag_name, asset).with_checksum(&"0".repeat(64));
    let server = FakeServer::start(release);
    let (_dir, binary) = copied_binary();
    let original = std::fs::read(&binary).expect("read the copy");

    let output = run_update(&binary, &server, &[]);

    assert!(!output.status.success(), "a bad checksum was accepted");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("checksum mismatch"), "{stderr}");
    assert_eq!(
        std::fs::read(&binary).expect("read the copy"),
        original,
        "a bad checksum replaced the binary"
    );
}

#[test]
fn a_release_without_sha256sums_is_refused() {
    let (tag_name, asset) = payload();
    let release = FakeRelease::new("v99.0.0", &tag_name, asset).without_checksums();
    let server = FakeServer::start(release);
    let (_dir, binary) = copied_binary();
    let original = std::fs::read(&binary).expect("read the copy");

    let output = run_update(&binary, &server, &[]);

    assert!(
        !output.status.success(),
        "an unverifiable release was installed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("SHA256SUMS"), "{stderr}");
    assert_eq!(std::fs::read(&binary).expect("read the copy"), original);
}

/// Run a slash command through the binary against the fake release server.
///
/// `/upgrade` and `/release-notes` are commands, not the `update` subcommand,
/// so they go through the same entry point a person types into: `-p /<name>`.
/// The provider is pointed at a closed port — neither command touches the
/// model, and a sweep that quietly needed one would be testing the wrong
/// thing.
fn run_slash_command(binary: &Path, server: &FakeServer, command: &str) -> std::process::Output {
    Command::new(binary)
        .args(["-p", &format!("/{command}")])
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:1")
        .env("CODAPE_API_BASE_URL", &server.base)
        .env("CODAPE_RELEASE_BASE_URL", server.asset_base())
        .env("CODAPE_DISABLE_UPDATE_CHECK", "1")
        .env("CODAPE_LOG", "off")
        .env("CODAPE_DISABLE_ADOPT", "1")
        .output()
        .expect("run a slash command")
}

/// `/upgrade` replaces the running binary and says to restart.
///
/// The command is the same updater as `codape update`, reached from inside a
/// session; the difference under test is that the deferred work runs and its
/// report reaches the output rather than being dropped.
#[test]
fn the_upgrade_command_replaces_the_binary() {
    let (tag_name, asset) = payload();
    let release = FakeRelease::new("v99.0.0", &tag_name, asset);
    let server = FakeServer::start(release);
    let (_dir, binary) = copied_binary();
    let before = std::fs::read(&binary).expect("read the copy");

    let output = run_slash_command(&binary, &server, "upgrade");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "/upgrade failed: {text}");
    assert!(text.contains("99.0.0"), "{text}");
    assert!(
        text.contains("Restart the session"),
        "the report did not tell the person to restart: {text}"
    );
    // The payload is a shell script holding `fake-new-version`, so a copy
    // that changed to exactly that is the release, not the build.
    let after = std::fs::read(&binary).expect("read the copy");
    #[cfg(unix)]
    {
        const NEEDLE: &[u8] = b"fake-new-version";
        assert_ne!(
            after, before,
            "/upgrade reported an install but wrote nothing"
        );
        assert!(
            after.windows(NEEDLE.len()).any(|window| window == NEEDLE),
            "the installed file is not the release's payload"
        );
    }
    #[cfg(not(unix))]
    let _ = (after, before);
}

/// `/release-notes` prints the release body the server published.
#[test]
fn the_release_notes_command_prints_the_published_body() {
    let (tag_name, asset) = payload();
    let release =
        FakeRelease::new("v99.0.0", &tag_name, asset).with_notes("- the thing\n- the other");
    let server = FakeServer::start(release);
    let (_dir, binary) = copied_binary();

    let output = run_slash_command(&binary, &server, "release-notes");
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(output.status.success(), "/release-notes failed: {text}");
    assert!(text.contains("v99.0.0"), "{text}");
    assert!(text.contains("- the thing"), "{text}");
    assert!(text.contains("- the other"), "{text}");
}

/// A release with no body is a fact to report, not a blank answer.
#[test]
fn release_notes_says_a_release_without_a_body_has_none() {
    let (tag_name, asset) = payload();
    let release = FakeRelease::new("v99.0.0", &tag_name, asset);
    let server = FakeServer::start(release);
    let (_dir, binary) = copied_binary();

    let output = run_slash_command(&binary, &server, "release-notes");
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(output.status.success(), "/release-notes failed: {text}");
    assert!(text.contains("v99.0.0"), "{text}");
    assert!(text.contains("no notes"), "{text}");
}
