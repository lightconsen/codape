//! Update check — compare local version against GitHub releases.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Name of the marker that records the last check, under the config home.
///
/// The update check asks a question about *this installation* — is the binary
/// older than the latest release — so the answer is the same in every
/// directory the user opens. Keeping the marker beside `settings.json` rather
/// than in the working directory is what makes that true, and what keeps a
/// session from creating `.codape/` in a repository that never asked for one.
const LAST_CHECK_FILE: &str = "last_version_check";

/// Current compiled version.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// GitHub repo owner for update checks.
pub const REPO_OWNER: &str = "lightconsen";
/// GitHub repo name for update checks.
pub const REPO_NAME: &str = "codape";

/// Set this to stop the once-a-day startup check (air-gapped machines, or a
/// host that wants no outbound requests it did not ask for).
pub const DISABLE_UPDATE_CHECK_ENV: &str = "CODAPE_DISABLE_UPDATE_CHECK";

/// The releases page, for messages that tell the user where to look.
#[must_use]
pub fn releases_url() -> String {
    format!("https://github.com/{REPO_OWNER}/{REPO_NAME}/releases")
}

/// Check for updates by querying GitHub's latest release API.
///
/// Returns a message if a newer version is available, `None` otherwise.
pub async fn check_for_updates() -> Option<String> {
    let release = crate::self_update::latest_release(Duration::from_secs(5))
        .await
        .ok()?;

    if is_newer(&release.version, CURRENT_VERSION) {
        Some(format!(
            "New version available: {} (current: {CURRENT_VERSION}). Run `codape update` or visit {}",
            release.version,
            releases_url()
        ))
    } else {
        None
    }
}

/// Compare two semantic version strings.
///
/// Returns `true` if `latest` is newer than `current`. Prerelease and build
/// metadata follow SemVer (`0.2.0-rc.1` sorts before `0.2.0`); tags that do
/// not parse as a version fall back to comparing dot-separated numbers, which
/// is what this used to do for everything.
#[must_use]
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (
        semver::Version::parse(latest),
        semver::Version::parse(current),
    ) {
        // `cmp_precedence`, not `>`: build metadata is part of `Ord` but not
        // of SemVer precedence, so `0.2.0+build.7` is the same release as
        // `0.2.0` and must not look newer.
        (Ok(latest), Ok(current)) => latest.cmp_precedence(&current) == std::cmp::Ordering::Greater,
        _ => dot_parts_newer(latest, current),
    }
}

/// The pre-semver comparison: more or larger dot-separated numbers wins.
fn dot_parts_newer(latest: &str, current: &str) -> bool {
    let latest_parts: Vec<u32> = latest.split('.').filter_map(|s| s.parse().ok()).collect();
    let current_parts: Vec<u32> = current.split('.').filter_map(|s| s.parse().ok()).collect();

    for (l, c) in latest_parts.iter().zip(current_parts.iter()) {
        if l > c {
            return true;
        }
        if l < c {
            return false;
        }
    }

    latest_parts.len() > current_parts.len()
}

/// Whether the given `CODAPE_DISABLE_UPDATE_CHECK` value turns checks off.
///
/// Pure so the answer can be tested without mutating the process environment.
#[must_use]
pub fn update_check_disabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Whether an update check is due, reading the marker from the config home.
///
/// See [`should_check_in`] for the testable form.
#[must_use]
pub fn should_check() -> bool {
    should_check_in(&config_home())
}

/// Whether an update check is due, with the config home passed in.
///
/// A missing or unreadable marker counts as due: the check costs one request
/// and reporting staleness matters more than saving it.
#[must_use]
pub fn should_check_in(config_home: &Path) -> bool {
    let disabled = std::env::var(DISABLE_UPDATE_CHECK_ENV).ok();
    if update_check_disabled(disabled.as_deref()) {
        return false;
    }
    let marker = config_home.join(LAST_CHECK_FILE);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();

    if !marker.exists() {
        return true;
    }

    let Ok(content) = std::fs::read_to_string(&marker) else {
        return true;
    };

    let last_check: u64 = content.trim().parse().unwrap_or(0);
    let elapsed = now.saturating_sub(last_check);
    elapsed >= Duration::from_hours(24).as_secs()
}

/// Record that an update check was performed.
pub fn record_check_time() {
    record_check_time_in(&config_home());
}

/// Record that an update check was performed, into a given config home.
///
/// Failures are ignored: a read-only or missing home must not turn a
/// once-a-day nicety into a startup error, and the next launch simply checks
/// again.
pub fn record_check_time_in(config_home: &Path) {
    let marker = config_home.join(LAST_CHECK_FILE);
    if let Err(error) = std::fs::create_dir_all(config_home) {
        tracing::debug!(%error, home = %config_home.display(), "could not create the config home");
        return;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        .to_string();
    if let Err(error) = std::fs::write(&marker, now) {
        tracing::debug!(%error, path = %marker.display(), "could not record the check time");
    }
}

/// The config home: `CLAUDE_CONFIG_DIR` when set, else `$HOME/.codape`.
fn config_home() -> PathBuf {
    codape_config::config_home()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_minor_bump() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn is_newer_patch_bump() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.1"));
    }

    #[test]
    fn is_newer_major_bump() {
        assert!(is_newer("1.0.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "1.0.0"));
    }

    #[test]
    fn is_newer_same_version() {
        assert!(!is_newer("0.1.0", "0.1.0"));
    }

    #[test]
    fn is_newer_extra_parts() {
        // More parts = newer if prefix matches
        assert!(is_newer("0.1.0.1", "0.1.0"));
    }

    #[test]
    fn is_newer_orders_a_prerelease_before_its_release() {
        assert!(!is_newer("0.2.0-rc.1", "0.2.0"));
        assert!(is_newer("0.2.0", "0.2.0-rc.1"));
    }

    #[test]
    fn is_newer_ignores_build_metadata() {
        assert!(!is_newer("0.2.0+build.7", "0.2.0"));
    }

    #[test]
    fn the_update_check_switch_accepts_the_usual_truthy_values() {
        for value in ["1", "true", "YES", " on "] {
            assert!(update_check_disabled(Some(value)), "{value}");
        }
        for value in ["0", "false", "no", ""] {
            assert!(!update_check_disabled(Some(value)), "{value}");
        }
        assert!(!update_check_disabled(None));
    }

    #[test]
    fn the_releases_url_names_the_public_repo() {
        assert_eq!(
            releases_url(),
            "https://github.com/lightconsen/codape/releases"
        );
    }

    /// The marker used to live at `<cwd>/.codape/.last_version_check`, so
    /// opening a clean repository for the first time wrote `.codape/` into it
    /// — and the first prompt's working-tree snapshot showed `?? .codape/`
    /// before the user had done anything.
    #[test]
    fn the_check_marker_lives_in_the_config_home_and_not_the_working_directory() {
        let config_home = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();

        record_check_time_in(config_home.path());

        assert!(
            config_home.path().join(LAST_CHECK_FILE).exists(),
            "the marker should be in the config home"
        );
        assert!(
            !worktree.path().join(".codape").exists(),
            "nothing should be written into the working directory"
        );
        assert_eq!(
            std::fs::read_dir(worktree.path()).unwrap().count(),
            0,
            "the working directory is left exactly as it was"
        );
        assert!(
            !should_check_in(config_home.path()),
            "the marker that was just written should suppress the next check"
        );
    }

    #[test]
    fn should_check_with_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(should_check_in(tmp.path()));
    }

    #[test]
    fn should_check_with_fresh_file() {
        let tmp = tempfile::tempdir().unwrap();
        record_check_time_in(tmp.path());
        assert!(!should_check_in(tmp.path()));
    }

    #[test]
    fn should_check_with_old_file() {
        let tmp = tempfile::tempdir().unwrap();
        // Write an old timestamp
        let old = SystemTime::now()
            .checked_sub(Duration::from_hours(48))
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(tmp.path().join(LAST_CHECK_FILE), old.to_string()).unwrap();
        assert!(should_check_in(tmp.path()));
    }

    #[test]
    fn an_unparseable_marker_is_treated_as_due() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(LAST_CHECK_FILE), "not a timestamp").unwrap();
        assert!(should_check_in(tmp.path()));
    }

    #[test]
    fn record_check_time_creates_the_home_when_it_does_not_exist_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("nested/config");
        record_check_time_in(&home);
        assert!(home.join(LAST_CHECK_FILE).exists());
    }

    #[test]
    fn record_check_time_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        record_check_time_in(tmp.path());
        assert!(tmp.path().join(LAST_CHECK_FILE).exists());
    }
}
