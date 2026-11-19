//! Host-owned UI element tree contributed by mods.
//!
//! A mod describes UI declaratively and the host renders it. The tree is a
//! **snapshot**: it is serialized across the worker boundary and owned by the
//! host, never a live JavaScript object. The host can therefore render it
//! after the mod's worker has moved on, timed out, or been unloaded.
//!
//! The tree is surface-neutral: [`crate::surface`] names the front end that
//! presents it and states what it can do. Codape registers only the terminal
//! surface; its renderer focuses a button and routes both `Enter` and mouse
//! clicks to the mod runtime's `$.ui.onActivate` handler. Another surface
//! renders the same tree for its own medium.
//!
//! There are two kinds of tree, and the difference is who owns the lifetime.
//! A hook's `$.ui.render` output is a **block**: the host appends it to the
//! transcript and forgets it. A **panel** is the other kind: the host keeps it
//! on screen beside the conversation until the mod replaces it with the same
//! slot or clears it, so it outlives the hook call that drew it. A mod asks
//! for that with [`ModPanelUpdate`]s; the slot itself is the host's, so this
//! module carries the request and not the settled panel. Panels are what a mod
//! uses for a status surface that updates; blocks are what it uses to answer
//! one event.
//!
//! [`parse_ui_elements`] is the validating boundary for the JSON that crosses
//! from the JavaScript runtime. It enforces types, non-empty button labels and
//! ids, a nesting ceiling, and a total element cap so a malformed (or
//! malicious) tree cannot exhaust the host's stack.

use serde::{Deserialize, Serialize};

/// Most deeply a UI element tree may nest.
pub const MAX_UI_DEPTH: usize = 32;

/// Most elements one hook invocation may contribute.
pub const MAX_UI_ELEMENTS: usize = 1024;

/// Most panel slots a session may hold at once.
///
/// A panel is the persistent kind of tree: it stays on screen until the mod
/// replaces or clears it, so an unbounded number would let one loop of
/// `$.ui.panel(...)` calls with fresh slot names push the conversation off the
/// screen. A slot a mod replaces does not count anew.
pub const MAX_PANEL_SLOTS: usize = 4;

/// What one button activation produced.
///
/// A handler can both append a block (the elements) and change the panels it
/// owns, so the two travel together rather than forcing a second round trip
/// through the worker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModUiActivation {
    /// The block elements the handler returned or staged.
    pub elements: Vec<UiElement>,
    /// The panel changes the handler asked for, in call order.
    pub panel_updates: Vec<ModPanelUpdate>,
}

/// One change a hook call asked the host to make to its panels.
///
/// Updates travel as a list, in call order, rather than as a whole new set of
/// panels: a hook that only knows about its own slot must not be able to erase
/// the panels another hook drew.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ModPanelUpdate {
    /// Draw `elements` in `slot`, replacing whatever that slot held.
    Set {
        /// The slot to write.
        slot: String,
        /// The panel's contents.
        elements: Vec<UiElement>,
    },
    /// Remove `slot`, if it exists.
    Clear {
        /// The slot to remove.
        slot: String,
    },
}

/// One node in a host-owned UI element tree.
///
/// The variant is tagged by `type` in JSON (`text`, `box`, `button`) so the
/// representation matches what a mod sees from `$.ui`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiElement {
    /// A run of text with an optional emphasis style.
    Text {
        /// The text to display.
        text: String,
        /// How the host should emphasize the text.
        style: UiStyle,
    },
    /// A bordered container that may carry a title.
    Box {
        /// Optional title shown in the top border.
        title: Option<String>,
        /// Nested elements, rendered top to bottom.
        children: Vec<Self>,
    },
    /// A labelled button affordance.
    ///
    /// `id` is what a focused activation sends to the mod runtime's
    /// `$.ui.onActivate` handler. Renderers show `[label]`, marking the
    /// focused one distinctly.
    Button {
        /// Text shown inside the brackets.
        label: String,
        /// Stable identifier passed to the activation handler.
        id: String,
    },
}

/// Emphasis for [`UiElement::Text`].
///
/// The variants map onto the terminal UI's existing style vocabulary: bold and
/// dim modifiers, and the warning/error colors. `Plain` adds nothing, and
/// `Notice` uses the cyan reserved for short informational notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UiStyle {
    /// Ordinary text.
    #[default]
    Plain,
    /// Emphasized text.
    Bold,
    /// De-emphasized text.
    Dim,
    /// Text that calls for attention.
    Warning,
    /// Text that reports an error.
    Error,
    /// A short notice from `$.session.notify`, shown distinctly from ordinary
    /// text but without the urgency of a warning or error.
    Notice,
}

/// Parse a JSON array of UI elements, validating every node.
///
/// The error names the offending path (for example `[0].children[1]`) and says
/// what was wrong, so a hook's mistake becomes a usable hook error rather than
/// a silently dropped tree.
///
/// # Errors
///
/// Returns an error when `value` is not an array, when an element is not an
/// object with a known `type`, when a required field has the wrong type or is
/// missing, when a button's label or id is empty, or when the tree exceeds
/// [`MAX_UI_DEPTH`] or [`MAX_UI_ELEMENTS`].
pub fn parse_ui_elements(value: &serde_json::Value) -> Result<Vec<UiElement>, String> {
    let mut budget = MAX_UI_ELEMENTS;
    parse_ui_elements_in(value, "", &mut budget)
}

/// The shared body of [`parse_ui_elements`] and [`parse_mod_panel_updates`].
///
/// `path` prefixes the error paths of the elements, and `budget` is the
/// caller's remaining element allowance — shared across a call's panel updates
/// so four of them cannot each spend the full cap.
fn parse_ui_elements_in(
    value: &serde_json::Value,
    path: &str,
    budget: &mut usize,
) -> Result<Vec<UiElement>, String> {
    let array = value.as_array().ok_or_else(|| {
        if path.is_empty() {
            "expected an array of UI elements".to_string()
        } else {
            format!("{path}: expected an array of UI elements")
        }
    })?;
    let mut elements = Vec::with_capacity(array.len().min(*budget));
    for (index, item) in array.iter().enumerate() {
        let item_path = if path.is_empty() {
            format!("[{index}]")
        } else {
            format!("{path}[{index}]")
        };
        elements.push(parse_element(item, &item_path, 1, budget)?);
    }
    Ok(elements)
}

/// Parse a JSON array of panel updates, validating every tree.
///
/// This is the wire shape of a hook's request to change a slot. There is no
/// settled `ModPanel` counterpart here on purpose: the slot belongs to the
/// host, so the protocol carries the request and the host owns what a slot
/// holds (see `codape_ui::panel::PanelStore`).
///
/// # Errors
///
/// Returns an error when `value` is not an array, when an update has an
/// unknown `op`, when `slot` is not a non-empty string, when a `set` carries
/// no `elements` array or an invalid one, or when the element budget or the
/// slot cap is exceeded.
pub fn parse_mod_panel_updates(value: &serde_json::Value) -> Result<Vec<ModPanelUpdate>, String> {
    let array = value
        .as_array()
        .ok_or_else(|| "expected an array of panel updates".to_string())?;
    if array.len() > MAX_PANEL_SLOTS {
        return Err(format!(
            "a hook may change at most {MAX_PANEL_SLOTS} panels per call"
        ));
    }
    let mut budget = MAX_UI_ELEMENTS;
    let mut updates: Vec<ModPanelUpdate> = Vec::with_capacity(array.len());
    for (index, item) in array.iter().enumerate() {
        let path = format!("[{index}]");
        let object = item
            .as_object()
            .ok_or_else(|| format!("{path}: expected a panel update object"))?;
        let slot = object
            .get("slot")
            .and_then(serde_json::Value::as_str)
            .filter(|slot| !slot.is_empty())
            .ok_or_else(|| format!("{path}: panel update needs a non-empty string `slot`"))?;
        let update = match object.get("op").and_then(serde_json::Value::as_str) {
            Some("clear") => ModPanelUpdate::Clear {
                slot: slot.to_string(),
            },
            Some("set") => ModPanelUpdate::Set {
                slot: slot.to_string(),
                elements: parse_ui_elements_in(
                    object.get("elements").ok_or_else(|| {
                        format!("{path}: a `set` panel update needs an `elements` array")
                    })?,
                    &format!("{path}.elements"),
                    &mut budget,
                )?,
            },
            Some(other) => {
                return Err(format!(
                    "{path}: unknown panel update `{other}` (expected `set` or `clear`)"
                ));
            }
            None => {
                return Err(format!(
                    "{path}: panel update needs a string `op` (`set` or `clear`)"
                ));
            }
        };
        updates.push(update);
    }
    Ok(updates)
}

fn parse_element(
    value: &serde_json::Value,
    path: &str,
    depth: usize,
    budget: &mut usize,
) -> Result<UiElement, String> {
    if depth > MAX_UI_DEPTH {
        return Err(format!(
            "{path}: UI elements may nest at most {MAX_UI_DEPTH} levels"
        ));
    }
    let Some(remaining) = budget.checked_sub(1) else {
        return Err(format!(
            "{path}: a hook may contribute at most {MAX_UI_ELEMENTS} UI elements"
        ));
    };
    *budget = remaining;

    let object = value
        .as_object()
        .ok_or_else(|| format!("{path}: expected a UI element object"))?;
    let kind = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{path}: missing string field `type`"))?;

    match kind {
        "text" => {
            let text = object
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("{path}: text element needs a string `text` field"))?;
            let style = match object.get("style") {
                None | Some(serde_json::Value::Null) => UiStyle::Plain,
                Some(serde_json::Value::String(name)) => parse_style(name)
                    .ok_or_else(|| format!("{path}: unknown text style `{name}`"))?,
                Some(_) => {
                    return Err(format!("{path}: text element's `style` must be a string"));
                }
            };
            Ok(UiElement::Text {
                text: text.to_string(),
                style,
            })
        }
        "box" => {
            let title = match object.get("title") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(title)) => Some(title.clone()),
                Some(_) => {
                    return Err(format!(
                        "{path}: box element's `title` must be a string or null"
                    ));
                }
            };
            let children = match object.get("children") {
                None | Some(serde_json::Value::Null) => Vec::new(),
                Some(serde_json::Value::Array(children)) => {
                    let mut parsed = Vec::with_capacity(children.len());
                    for (index, child) in children.iter().enumerate() {
                        let child_path = format!("{path}.children[{index}]");
                        parsed.push(parse_element(child, &child_path, depth + 1, budget)?);
                    }
                    parsed
                }
                Some(_) => {
                    return Err(format!("{path}: box element's `children` must be an array"));
                }
            };
            Ok(UiElement::Box { title, children })
        }
        "button" => {
            let label = non_empty_string(object, "label", path)?;
            let id = non_empty_string(object, "id", path)?;
            Ok(UiElement::Button { label, id })
        }
        other => Err(format!(
            "{path}: unknown element type `{other}` (expected `text`, `box` or `button`)"
        )),
    }
}

fn non_empty_string(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    path: &str,
) -> Result<String, String> {
    match object.get(field).and_then(serde_json::Value::as_str) {
        Some(value) if !value.is_empty() => Ok(value.to_string()),
        Some(_) => Err(format!(
            "{path}: button element's `{field}` must not be empty"
        )),
        None => Err(format!(
            "{path}: button element needs a string `{field}` field"
        )),
    }
}

fn parse_style(name: &str) -> Option<UiStyle> {
    match name {
        "plain" => Some(UiStyle::Plain),
        "bold" => Some(UiStyle::Bold),
        "dim" => Some(UiStyle::Dim),
        "warning" => Some(UiStyle::Warning),
        "error" => Some(UiStyle::Error),
        "notice" => Some(UiStyle::Notice),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(value: serde_json::Value) -> serde_json::Value {
        value
    }

    #[test]
    fn a_nested_tree_round_trips_through_json() {
        let value = json(serde_json::json!([
            {
                "type": "box",
                "title": "Outer",
                "children": [
                    {"type": "text", "text": "hello", "style": "bold"},
                    {
                        "type": "box",
                        "title": null,
                        "children": [
                            {"type": "button", "label": "Retry", "id": "retry"}
                        ]
                    }
                ]
            }
        ]));

        let parsed = parse_ui_elements(&value).expect("the tree is valid");
        assert_eq!(
            parsed,
            vec![UiElement::Box {
                title: Some("Outer".to_string()),
                children: vec![
                    UiElement::Text {
                        text: "hello".to_string(),
                        style: UiStyle::Bold,
                    },
                    UiElement::Box {
                        title: None,
                        children: vec![UiElement::Button {
                            label: "Retry".to_string(),
                            id: "retry".to_string(),
                        }],
                    },
                ],
            }]
        );

        // The snapshot survives a trip back through JSON.
        let encoded = serde_json::to_value(&parsed).expect("serializable");
        assert_eq!(encoded, value);
    }

    #[test]
    fn a_text_style_is_optional_and_defaults_to_plain() {
        let parsed = parse_ui_elements(&serde_json::json!([
            {"type": "text", "text": "hi"}
        ]))
        .expect("the tree is valid");
        assert_eq!(
            parsed,
            vec![UiElement::Text {
                text: "hi".to_string(),
                style: UiStyle::Plain,
            }]
        );
    }

    #[test]
    fn the_notice_style_round_trips() {
        let parsed = parse_ui_elements(&serde_json::json!([
            {"type": "text", "text": "heads up", "style": "notice"}
        ]))
        .expect("the tree is valid");
        assert_eq!(
            parsed,
            vec![UiElement::Text {
                text: "heads up".to_string(),
                style: UiStyle::Notice,
            }]
        );
    }

    #[test]
    fn unknown_types_styles_and_shapes_name_the_problem() {
        let unknown_type = parse_ui_elements(&serde_json::json!([
            {"type": "canvas"}
        ]))
        .expect_err("an unknown type is rejected");
        assert!(
            unknown_type.contains("unknown element type `canvas`"),
            "{unknown_type}"
        );

        let unknown_style = parse_ui_elements(&serde_json::json!([
            {"type": "text", "text": "hi", "style": "loud"}
        ]))
        .expect_err("an unknown style is rejected");
        assert!(
            unknown_style.contains("unknown text style `loud`"),
            "{unknown_style}"
        );

        let empty_button = parse_ui_elements(&serde_json::json!([
            {"type": "button", "label": "", "id": "retry"}
        ]))
        .expect_err("an empty label is rejected");
        assert!(
            empty_button.contains("`label` must not be empty"),
            "{empty_button}"
        );

        let missing_children = parse_ui_elements(&serde_json::json!([
            {"type": "box", "children": "nope"}
        ]))
        .expect_err("a non-array children field is rejected");
        assert!(
            missing_children.contains("`children` must be an array"),
            "{missing_children}"
        );

        let not_an_array = parse_ui_elements(&serde_json::json!({"type": "text"}))
            .expect_err("the top level must be an array");
        assert!(not_an_array.contains("expected an array"), "{not_an_array}");
    }

    #[test]
    fn nesting_beyond_the_ceiling_is_rejected() {
        let mut value = serde_json::json!({"type": "text", "text": "deep"});
        for _ in 0..MAX_UI_DEPTH {
            value = serde_json::json!({"type": "box", "children": [value]});
        }
        let error = parse_ui_elements(&serde_json::json!([value]))
            .expect_err("a tree past the depth ceiling is rejected");
        assert!(error.contains("nest at most"), "{error}");
    }

    #[test]
    fn the_element_cap_is_enforced() {
        let elements: Vec<_> = (0..=MAX_UI_ELEMENTS)
            .map(|_| serde_json::json!({"type": "text", "text": "x"}))
            .collect();
        let error = parse_ui_elements(&serde_json::json!(elements))
            .expect_err("a tree past the element cap is rejected");
        assert!(error.contains("at most"), "{error}");
    }

    #[test]
    fn panel_updates_keep_their_order_and_their_slots() {
        let value = json(serde_json::json!([
            {"op": "set", "slot": "build", "elements": [{"type": "text", "text": "green"}]},
            {"op": "clear", "slot": "watch"}
        ]));

        assert_eq!(
            parse_mod_panel_updates(&value).expect("the updates are valid"),
            vec![
                ModPanelUpdate::Set {
                    slot: "build".to_string(),
                    elements: vec![UiElement::Text {
                        text: "green".to_string(),
                        style: UiStyle::Plain,
                    }],
                },
                ModPanelUpdate::Clear {
                    slot: "watch".to_string(),
                },
            ]
        );
    }

    #[test]
    fn a_malformed_panel_update_names_the_problem() {
        for (update, expected) in [
            (
                serde_json::json!({"op": "set", "slot": "", "elements": []}),
                "non-empty",
            ),
            (
                serde_json::json!({"op": "replace", "slot": "s"}),
                "unknown panel update",
            ),
            (serde_json::json!({"slot": "s"}), "`op`"),
            (serde_json::json!({"op": "set", "slot": "s"}), "`elements`"),
        ] {
            let error = parse_mod_panel_updates(&serde_json::json!([update]))
                .expect_err("a malformed update is rejected");
            assert!(error.contains(expected), "{error}");
        }

        let error = parse_mod_panel_updates(&serde_json::json!({"op": "clear", "slot": "s"}))
            .expect_err("the array itself is required");
        assert!(error.contains("array"), "{error}");
    }

    #[test]
    fn the_panel_slot_cap_is_enforced_on_updates() {
        let updates: Vec<_> = (0..=MAX_PANEL_SLOTS)
            .map(|index| serde_json::json!({"op": "clear", "slot": format!("s{index}")}))
            .collect();
        let error = parse_mod_panel_updates(&serde_json::json!(updates))
            .expect_err("a call past the slot cap is rejected");
        assert!(error.contains("at most"), "{error}");
    }
}
