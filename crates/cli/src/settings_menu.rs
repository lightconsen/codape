//! The settings menu: the same `settings.json`, with the schema on screen.
//!
//! Settings live in files, and that stays true — the menu is a *view* of one
//! of them, not a second store. Every accept writes `settings.json` through
//! the same read-modify-write the slash commands use, so a key changed here is
//! the key `/config get` reads back and the key the next session starts from.
//! Anything the menu cannot see stays exactly as it was.
//!
//! Only keys this program acts on are listed. A row for a key nobody reads
//! would be a switch that looks like it does something. Each row says whether
//! the running session adopts the change or only the next one does: several of
//! these are read once at startup, and a menu that let them look instant would
//! be lying about when they take effect.
//!
//! Deliberately rows of *values*, not of free text. A setting whose value is
//! one of a few choices is a row you cycle. The ones needing a string (an API
//! key, a command) stay `/config set`'s business, because a one-line field for
//! them would be a worse editor than the file itself.

use codape_ui::picker::{Picker, PickerItem};

/// The prefix a settings row's key carries.
///
/// Same convention as `agent:`/`plugin:`/`session:` — [`Picker`] hands back
/// one opaque string, and the prefix says which list it came from.
const PREFIX: &str = "setting:";

/// One setting the menu offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingRow {
    /// The dotted key in `settings.json`, e.g. `statusline.theme`.
    pub key: &'static str,
    /// How the row is named on screen.
    pub label: &'static str,
    /// Every value the key may hold, in cycling order.
    ///
    /// Empty for the theme row, whose values are the install's theme list
    /// rather than a fixed set this crate could name.
    pub values: &'static [&'static str],
    /// Whether this row is the on/off kind, which flips rather than steps.
    pub boolean: bool,
    /// Whether the running session adopts a change immediately.
    pub live: bool,
}

/// The settings the menu offers, in the order they are drawn.
///
/// A `const` table rather than a walk over `settings.json`: the schema is this
/// program's, and a file-driven menu would offer whatever a hand-edit left
/// behind — including keys nothing reads.
pub const ROWS: &[SettingRow] = &[
    SettingRow {
        key: "model",
        label: "Model",
        // Empty for the same reason the theme row is: the values come from
        // `ModelId`'s alias table rather than being this table's to spell.
        // They have to be the *canonical* ids, not the aliases a person types.
        // `/model` persists the resolved id, and startup hands a settings
        // `model` straight to the engine, so writing `"sonnet"` here would send
        // `"model": "sonnet"` to the provider at the next start.
        //
        // "default" is deliberately not a choice either: a missing `model` key
        // *is* the default, and the literal string would defeat the startup
        // check that recognises "nothing was chosen".
        values: &[],
        boolean: false,
        live: true,
    },
    SettingRow {
        key: "permission_mode",
        label: "Permission mode",
        values: &[
            "default",
            "accept-edits",
            "bypass-permissions",
            "dont-ask",
            "plan",
            "auto",
        ],
        boolean: false,
        live: true,
    },
    SettingRow {
        key: "vim_mode",
        label: "Vim mode",
        values: &["true", "false"],
        boolean: true,
        live: true,
    },
    SettingRow {
        key: "statusline.theme",
        label: "Theme",
        values: &[],
        boolean: false,
        live: true,
    },
    SettingRow {
        key: "prompt_caching",
        label: "Prompt caching",
        values: &["true", "false"],
        boolean: true,
        live: false,
    },
    SettingRow {
        key: "telemetry_enabled",
        label: "Telemetry",
        values: &["true", "false"],
        boolean: true,
        live: false,
    },
];

/// Read a dotted path out of a settings document, as a display string.
///
/// Missing is the empty string: a key that was never written has no value, and
/// a row that printed "none" would read like a value it could hold.
#[must_use]
pub fn value_at(settings: &serde_json::Value, key: &str) -> String {
    let mut node = settings;
    for segment in key.split('.') {
        let Some(next) = node.get(segment) else {
            return String::new();
        };
        node = next;
    }
    match node {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Bool(flag) => flag.to_string(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The value a row moves to when one of `choices` is the row's value set.
///
/// Two shapes, because the file has two. A boolean flips. An enumerated value
/// steps to the next one and wraps, which is what lets a row with six choices
/// change with one key instead of six.
#[must_use]
pub fn next_value(row: &SettingRow, choices: &[String], current: &str) -> Option<String> {
    if row.boolean {
        return Some((current != "true").to_string());
    }
    if choices.is_empty() {
        return None;
    }
    let position = choices.iter().position(|value| value == current);
    // An unrecognised current value — a hand-edit, or a theme since deleted —
    // lands on the first choice rather than refusing to move: the row shows
    // what the file says, and the next press has to mean something.
    let next = position.map_or(0, |index| (index + 1) % choices.len());
    choices.get(next).cloned()
}

/// The full value set of a row, given the themes the install has.
///
/// Two rows take their values from outside this table: the theme row from the
/// install's theme list, and the model row from the alias table. Every caller
/// that wants to step a row asks here rather than matching on the key.
#[must_use]
pub fn choices_for(row: &SettingRow, themes: &[String]) -> Vec<String> {
    if row.key == "statusline.theme" {
        return themes.to_vec();
    }
    if row.key == "model" {
        return model_choices();
    }
    row.values
        .iter()
        .map(|value| (*value).to_string())
        .collect()
}

/// The models the model row steps through, as canonical ids.
///
/// Built from [`codape_core::types::ModelId`] rather than spelled out, so a
/// release that moves an
/// alias moves this list with it — and so the row cannot write a name the
/// provider does not know. The aliases are only the labels a person types;
/// what reaches `settings.json` is what `/model` writes.
#[must_use]
pub fn model_choices() -> Vec<String> {
    vec![
        codape_core::types::ModelId::opus().0,
        codape_core::types::ModelId::sonnet().0,
        codape_core::types::ModelId::haiku().0,
    ]
}

/// Parse a value string into the JSON the file should hold.
///
/// `true`/`false` become booleans and a bare integer becomes a number. Every
/// other string stays a string: a theme named `2024` is still a name.
#[must_use]
pub fn value_to_json(text: &str) -> serde_json::Value {
    match text {
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        "" => serde_json::Value::Null,
        other => other
            .parse::<i64>()
            .map_or_else(|_| serde_json::Value::String(other.to_string()), Into::into),
    }
}

/// Write one dotted key into a settings document, creating what it needs.
///
/// A non-object on the way down (`statusline` holding a string, say) is
/// replaced rather than skipped: the alternative is a menu press that silently
/// does nothing, and the file is a settings document before it is anything
/// else.
pub fn set_at(document: &mut serde_json::Value, key: &str, value: serde_json::Value) {
    if !document.is_object() {
        *document = serde_json::json!({});
    }
    let mut segments = key.split('.').peekable();
    let mut node = document;
    while let Some(segment) = segments.next() {
        let last = segments.peek().is_none();
        if last {
            if let Some(object) = node.as_object_mut() {
                object.insert(segment.to_string(), value);
            }
            return;
        }
        if !node.is_object() {
            *node = serde_json::json!({});
        }
        let Some(object) = node.as_object_mut() else {
            return;
        };
        let child = object
            .entry(segment.to_string())
            .or_insert_with(|| serde_json::json!({}));
        if !child.is_object() {
            *child = serde_json::json!({});
        }
        node = child;
    }
}

/// The short name of a model id, for display only.
///
/// `claude-sonnet-5-5` reads as `sonnet` in the menu because that is the word a
/// person uses; the file keeps the full id. Anything that is not one of the
/// aliases this binary knows is shown as written, so a provider-specific id
/// appears as itself rather than being mangled into a family name.
#[must_use]
pub fn short_model_name(id: &str) -> String {
    let known = [
        (codape_core::types::ModelId::opus().0, "opus"),
        (codape_core::types::ModelId::sonnet().0, "sonnet"),
        (codape_core::types::ModelId::haiku().0, "haiku"),
    ];
    known
        .iter()
        .find(|(full, _)| full == id)
        .map_or_else(|| id.to_string(), |(_, short)| (*short).to_string())
}

/// Build the menu as a [`Picker`].
///
/// `themes` is the install's theme names, used for the one row whose values
/// are not this crate's to know. `settings` is the parsed file. A row that
/// cannot move — the theme row on an install with no themes — is drawn
/// disabled with the reason, rather than offered and then refusing.
#[must_use]
pub fn rows(settings: &serde_json::Value, themes: &[String]) -> Picker {
    let items = ROWS
        .iter()
        .map(|row| {
            let stored = value_at(settings, row.key);
            let current = if stored.is_empty() {
                "unset".to_string()
            } else {
                stored
            };
            // The model row stores a canonical id and shows the short name
            // beside it, so the row reads the way a person names a model
            // without the file losing the id the provider needs.
            let current = if row.key == "model" {
                short_model_name(&current)
            } else {
                current
            };
            let detail = if row.live {
                current
            } else {
                format!("{current} · next session")
            };
            let item = PickerItem::new(row_key(row.key), row.label).with_detail(detail);
            if row.key == "statusline.theme" && themes.is_empty() {
                return item.disabled("no themes installed");
            }
            item
        })
        .collect();
    Picker::new("Settings", items)
}

/// The key a settings row accepts with.
#[must_use]
pub fn row_key(key: &str) -> String {
    format!("{PREFIX}{key}")
}

/// Read a settings row key back.
#[must_use]
pub fn key_from_row(key: &str) -> Option<&str> {
    key.strip_prefix(PREFIX).filter(|key| !key.is_empty())
}

/// The row a dotted key names, if the menu offers it.
#[must_use]
pub fn row_for(key: &str) -> Option<&'static SettingRow> {
    ROWS.iter().find(|row| row.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn a_boolean_flips() {
        let row = row_for("vim_mode").expect("vim_mode is offered");
        let choices = choices_for(row, &[]);
        assert_eq!(next_value(row, &choices, "false").as_deref(), Some("true"));
        assert_eq!(next_value(row, &choices, "true").as_deref(), Some("false"));
        // An unset key is off, so the first press turns it on.
        assert_eq!(next_value(row, &choices, "").as_deref(), Some("true"));
    }

    #[test]
    fn an_enumerated_row_steps_and_wraps() {
        let row = row_for("permission_mode").expect("permission mode is offered");
        let choices = choices_for(row, &[]);
        assert_eq!(
            next_value(row, &choices, "default").as_deref(),
            Some("accept-edits")
        );
        assert_eq!(
            next_value(row, &choices, "auto").as_deref(),
            Some("default")
        );
        // An unknown value lands on the first choice rather than refusing.
        assert_eq!(
            next_value(row, &choices, "nonsense").as_deref(),
            Some("default")
        );
    }

    #[test]
    fn the_theme_row_steps_through_the_installs_themes() {
        let row = row_for("statusline.theme").expect("theme is offered");
        let themes = strings(&["dark", "light", "nord"]);
        let choices = choices_for(row, &themes);
        assert_eq!(choices, themes);
        assert_eq!(next_value(row, &choices, "dark").as_deref(), Some("light"));
        assert_eq!(next_value(row, &choices, "nord").as_deref(), Some("dark"));
        // With no themes there is nothing to step to.
        assert!(next_value(row, &choices_for(row, &[]), "dark").is_none());
    }

    #[test]
    fn value_spellings_round_trip_through_json() {
        assert_eq!(value_to_json("true"), serde_json::Value::Bool(true));
        assert_eq!(value_to_json("false"), serde_json::Value::Bool(false));
        assert_eq!(value_to_json("2024"), serde_json::json!(2024));
        assert_eq!(
            value_to_json("accept-edits"),
            serde_json::json!("accept-edits")
        );
        assert_eq!(value_to_json(""), serde_json::Value::Null);
    }

    #[test]
    fn a_dotted_key_writes_into_its_block() {
        let mut document = serde_json::json!({"model": "opus"});
        set_at(&mut document, "statusline.theme", serde_json::json!("dark"));
        assert_eq!(document["statusline"]["theme"], "dark");
        // The neighbour is untouched, which is the whole point of reading and
        // writing the document rather than one key.
        assert_eq!(document["model"], "opus");
    }

    #[test]
    fn a_block_holding_something_else_is_replaced_rather_than_skipped() {
        let mut document = serde_json::json!({"statusline": "dark"});
        set_at(&mut document, "statusline.theme", serde_json::json!("nord"));
        assert_eq!(document["statusline"]["theme"], "nord");
    }

    #[test]
    fn a_value_is_read_through_its_path() {
        let settings = serde_json::json!({
            "vim_mode": true,
            "statusline": { "theme": "nord" }
        });
        assert_eq!(value_at(&settings, "vim_mode"), "true");
        assert_eq!(value_at(&settings, "statusline.theme"), "nord");
        assert_eq!(value_at(&settings, "absent"), "");
        assert_eq!(value_at(&settings, "statusline.absent"), "");
    }

    #[test]
    fn the_model_row_writes_canonical_ids_not_aliases() {
        // `/model` persists the resolved id and startup hands a settings
        // `model` straight to the engine, so an alias written here would be
        // sent to the provider as a model name at the next start.
        let row = row_for("model").expect("model is offered");
        let choices = choices_for(row, &[]);
        assert!(!choices.is_empty(), "{choices:?}");
        for (choice, expected) in choices.iter().zip([
            codape_core::types::ModelId::opus().0,
            codape_core::types::ModelId::sonnet().0,
            codape_core::types::ModelId::haiku().0,
        ]) {
            assert_eq!(*choice, expected);
        }
        // The alias is not a valid choice, and neither is "default": a missing
        // key is the default, and the literal string defeats the startup check
        // that recognises "nothing was chosen".
        assert!(
            !choices.iter().any(|choice| choice == "sonnet"),
            "{choices:?}"
        );
        assert!(
            !choices.iter().any(|choice| choice == "default"),
            "{choices:?}"
        );
    }

    #[test]
    fn the_model_row_steps_between_canonical_ids() {
        let row = row_for("model").expect("model is offered");
        let choices = choices_for(row, &[]);
        let current = &choices[0];
        assert_eq!(
            next_value(row, &choices, current).as_deref(),
            choices.get(1).map(String::as_str)
        );
        // A hand-written alias is not a choice, so the row lands on the first
        // one rather than refusing to move.
        assert_eq!(
            next_value(row, &choices, "sonnet").as_deref(),
            Some(choices[0].as_str())
        );
    }

    #[test]
    fn the_menu_shows_a_short_model_name_and_keeps_the_id() {
        // The row reads as the word a person uses; `value_at` still returns the
        // id the file holds, so display and storage do not have to agree.
        let settings = serde_json::json!({"model": codape_core::types::ModelId::sonnet().0});
        let picker = rows(&settings, &strings(&["dark"]));
        let model = picker
            .items
            .iter()
            .find(|item| item.key == row_key("model"))
            .expect("model row");
        assert_eq!(model.detail, "sonnet");
        assert_eq!(
            value_at(&settings, "model"),
            codape_core::types::ModelId::sonnet().0
        );
        // A model this binary does not know is shown as written.
        assert_eq!(short_model_name("my-gateway-model"), "my-gateway-model");
    }

    #[test]
    fn every_row_key_round_trips() {
        for row in ROWS {
            let key = row_key(row.key);
            assert_eq!(key_from_row(&key), Some(row.key));
        }
        assert!(key_from_row("agent:1").is_none());
        assert!(key_from_row(PREFIX).is_none());
    }

    #[test]
    fn the_menu_shows_the_stored_value_and_flags_startup_only_rows() {
        let settings = serde_json::json!({"vim_mode": true});
        let picker = rows(&settings, &strings(&["dark"]));
        let vim = picker
            .items
            .iter()
            .find(|item| item.key == row_key("vim_mode"))
            .expect("vim row");
        assert_eq!(vim.detail, "true");
        let telemetry = picker
            .items
            .iter()
            .find(|item| item.key == row_key("telemetry_enabled"))
            .expect("telemetry row");
        assert!(
            telemetry.detail.contains("next session"),
            "{}",
            telemetry.detail
        );
        assert!(
            telemetry.detail.starts_with("unset"),
            "{}",
            telemetry.detail
        );
    }

    #[test]
    fn the_theme_row_is_disabled_when_the_install_has_no_themes() {
        let picker = rows(&serde_json::json!({}), &[]);
        let theme = picker
            .items
            .iter()
            .find(|item| item.key == row_key("statusline.theme"))
            .expect("theme row");
        assert!(!theme.enabled, "{theme:?}");
        assert_eq!(theme.detail, "no themes installed");
    }
}
