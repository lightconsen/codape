//! Which plugins this person turned off, and which they pinned to the top.
//!
//! Both are preferences rather than facts about the install: the plugin
//! directory is still there, `codape plugin list` still names it, and
//! `plugin.json` still declares what it contributes. Turning a plugin off is
//! therefore enforced where the session *loads* plugins, not by deleting
//! anything — which is what makes it reversible from the browser that turned
//! it off.
//!
//! The record lives in the user's `settings.json` under
//! `plugins.<name>.enabled` / `plugins.<name>.favorite`. That is the object
//! the plugin host already uses for per-plugin configuration
//! (`plugins.<name>.options`), so the file gains keys rather than a second
//! place to keep plugin state. [`codape_plugins`]' `apply_options_from_settings`
//! reads only `options`, so the extra keys are inert to it.
//!
//! Deliberately not written to the project's `.codape/settings.json`: being
//! switched off is a property of this person's machine, not of the repository.

use std::collections::BTreeSet;
use std::path::Path;

/// The per-plugin preferences a session reads at startup and the browser edits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginPrefs {
    /// Plugins switched off on this machine.
    disabled: BTreeSet<String>,
    /// Plugins pinned to the top of the browser.
    favorites: BTreeSet<String>,
}

impl PluginPrefs {
    /// Read the preferences out of a whole settings document.
    ///
    /// Takes the parsed document rather than a path so the shape rules can be
    /// tested without a file. A `plugins` value that is not an object, or a
    /// per-plugin value that is not one, is ignored: a hand-edited settings
    /// file must not stop a session from starting.
    #[must_use]
    pub fn from_settings(settings: &serde_json::Value) -> Self {
        let mut prefs = Self::default();
        let Some(plugins) = settings.get("plugins").and_then(serde_json::Value::as_object) else {
            return prefs;
        };
        for (name, entry) in plugins {
            let Some(entry) = entry.as_object() else {
                continue;
            };
            if entry.get("enabled").and_then(serde_json::Value::as_bool) == Some(false) {
                prefs.disabled.insert(name.clone());
            }
            if entry.get("favorite").and_then(serde_json::Value::as_bool) == Some(true) {
                prefs.favorites.insert(name.clone());
            }
        }
        prefs
    }

    /// Read the preferences from a settings file, treating a missing or
    /// unparsable file as "no preferences".
    #[must_use]
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .map(|value| Self::from_settings(&value))
            .unwrap_or_default()
    }

    /// Whether this plugin should load in a new session.
    ///
    /// Unknown means enabled: a plugin with no preference is one the person
    /// has not switched off, and the manifest's own default is the only other
    /// opinion in play.
    #[must_use]
    pub fn is_enabled(&self, name: &str) -> bool {
        !self.disabled.contains(name)
    }

    /// Whether this plugin is pinned.
    #[must_use]
    pub fn is_favorite(&self, name: &str) -> bool {
        self.favorites.contains(name)
    }

    /// Record a switch and report the state that was written.
    ///
    /// Returns the new value so the caller announces what happened rather than
    /// what it asked for: the write can refuse (a settings file that is not a
    /// JSON object), and a message saying "disabled" after a failed write is
    /// the kind of lie that gets discovered a session later.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        if enabled {
            self.disabled.remove(name);
        } else {
            self.disabled.insert(name.to_string());
        }
    }

    /// Pin or unpin a plugin.
    pub fn set_favorite(&mut self, name: &str, favorite: bool) {
        if favorite {
            self.favorites.insert(name.to_string());
        } else {
            self.favorites.remove(name);
        }
    }
}

/// Write one plugin's `enabled` or `favorite` bit into a settings file.
///
/// The document is edited as raw JSON rather than through
/// [`codape_config::settings::Settings`]: `plugins` is not a field of that
/// struct, and round-tripping the whole file through it would drop every key
/// the struct does not carry. Written through a temporary file and renamed so
/// a crash mid-write cannot leave a settings file the next startup reads as
/// empty.
///
/// # Errors
///
/// When the file exists but is not a JSON object, or the write fails.
pub fn write_flag(path: &Path, name: &str, key: &str, value: bool) -> std::io::Result<()> {
    let mut document: serde_json::Value = match std::fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => serde_json::json!({}),
        Ok(content) => serde_json::from_str(&content).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not valid JSON: {error}", path.display()),
            )
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(error),
    };
    if document.as_object().is_none() {
        document = serde_json::json!({});
    }
    let object = document
        .as_object_mut()
        .ok_or_else(|| std::io::Error::other("settings document is not an object"))?;
    let plugins = object
        .entry("plugins")
        .or_insert_with(|| serde_json::json!({}));
    if plugins.as_object().is_none() {
        *plugins = serde_json::json!({});
    }
    let Some(plugins) = plugins.as_object_mut() else {
        return Err(std::io::Error::other("plugins is not an object"));
    };
    let entry = plugins
        .entry(name.to_string())
        .or_insert_with(|| serde_json::json!({}));
    if entry.as_object().is_none() {
        *entry = serde_json::json!({});
    }
    let Some(entry) = entry.as_object_mut() else {
        return Err(std::io::Error::other("plugin entry is not an object"));
    };
    entry.insert(key.to_string(), serde_json::Value::Bool(value));

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(
        &temporary,
        serde_json::to_string_pretty(&document)
            .map_err(|error| std::io::Error::other(error.to_string()))?,
    )?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(json: serde_json::Value) -> PluginPrefs {
        PluginPrefs::from_settings(&json)
    }

    #[test]
    fn a_plugin_without_a_preference_is_enabled() {
        let prefs = settings(serde_json::json!({}));
        assert!(prefs.is_enabled("anything"));
        assert!(!prefs.is_favorite("anything"));
    }

    #[test]
    fn the_flags_are_read_from_the_per_plugin_object() {
        let prefs = settings(serde_json::json!({
            "plugins": {
                "acme": {"enabled": false, "favorite": true},
                "other": {"options": {"token": "t"}}
            }
        }));
        assert!(!prefs.is_enabled("acme"));
        assert!(prefs.is_favorite("acme"));
        assert!(prefs.is_enabled("other"), "options alone is not a switch");
        assert!(!prefs.is_favorite("other"));
    }

    /// `enabled: true` is the default, so writing it must not count as a
    /// preference — otherwise re-enabling a plugin leaves a row behind that
    /// the next version has to keep understanding.
    #[test]
    fn an_explicit_enable_is_not_a_disable() {
        let prefs = settings(serde_json::json!({"plugins": {"acme": {"enabled": true}}}));
        assert!(prefs.is_enabled("acme"));
    }

    #[test]
    fn a_malformed_plugins_value_is_ignored_rather_than_fatal() {
        for value in [
            serde_json::json!({"plugins": "nonsense"}),
            serde_json::json!({"plugins": {"acme": 7}}),
            serde_json::json!({"plugins": null}),
        ] {
            let prefs = settings(value);
            assert!(prefs.is_enabled("acme"));
        }
    }

    #[test]
    fn a_flag_round_trips_through_a_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"config_version": 4, "plugins": {"acme": {"options": {"token": "t"}}}}"#,
        )
        .unwrap();

        write_flag(&path, "acme", "enabled", false).unwrap();
        write_flag(&path, "acme", "favorite", true).unwrap();

        let prefs = PluginPrefs::load(&path);
        assert!(!prefs.is_enabled("acme"));
        assert!(prefs.is_favorite("acme"));

        // The options the host wrote are still there: this edits a document,
        // it does not rebuild one.
        let document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(document["plugins"]["acme"]["options"]["token"], "t");
        assert_eq!(document["config_version"], 4);
    }

    #[test]
    fn writing_a_flag_creates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("settings.json");
        write_flag(&path, "acme", "enabled", false).unwrap();
        assert!(!PluginPrefs::load(&path).is_enabled("acme"));
    }

    #[test]
    fn a_file_that_is_not_json_is_an_error_rather_than_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "not json at all").unwrap();
        let error = write_flag(&path, "acme", "enabled", false).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json at all");
    }
}
