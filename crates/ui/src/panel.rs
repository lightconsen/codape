//! The host's panel slots — mod-drawn trees that stay on screen.
//!
//! A mod's `$.ui.render` block goes into the transcript and is forgotten. A
//! panel is the other contract: the host holds it in a named slot and keeps
//! drawing it, so a mod can show a status that changes without appending a
//! transcript entry per update. The slot is the identity — rendering the same
//! slot again replaces it in place.
//!
//! The store is deliberately plain data with no rendering in it. What it owns
//! is the lifetime: which slots exist, in what order they appear, and which
//! one a keypress is addressed to. Drawing it is the terminal's job, and a
//! second surface would draw the same slots for its own medium.

use codape_core::ui_protocol::{MAX_PANEL_SLOTS, ModPanelUpdate, UiElement};

/// The panels a session is showing, newest slot last.
#[derive(Debug, Clone, Default)]
pub struct PanelStore {
    slots: Vec<PanelSlot>,
}

/// One slot and what it is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelSlot {
    /// The name the mod chose. It is the slot's identity, so it is also what a
    /// later update addresses.
    pub slot: String,
    /// The current tree. Replaced wholesale by each update; there is no diffing,
    /// because a mod that wants a partial change renders the whole panel again.
    pub elements: Vec<UiElement>,
}

impl PanelStore {
    /// An empty store.
    #[must_use]
    pub const fn new() -> Self {
        Self { slots: Vec::new() }
    }

    /// Apply the changes one hook call asked for, in order.
    ///
    /// Later updates in the same list win over earlier ones, which is what
    /// makes a mod's `panel(slot, a)` followed by `panel(slot, b)` show `b`.
    /// A `Set` for a new slot when the cap is reached is dropped rather than
    /// evicting an older panel: the mod that asked most recently is the one
    /// whose request cannot be honoured, and silently replacing another mod's
    /// panel would be a worse surprise than not drawing.
    ///
    /// Returns how many updates were dropped by the cap.
    pub fn apply(&mut self, updates: &[ModPanelUpdate]) -> usize {
        let mut dropped = 0;
        for update in updates {
            match update {
                ModPanelUpdate::Clear { slot } => {
                    self.slots.retain(|existing| &existing.slot != slot);
                }
                ModPanelUpdate::Set { slot, elements } => {
                    if let Some(existing) = self.slots.iter_mut().find(|panel| &panel.slot == slot)
                    {
                        existing.elements.clone_from(elements);
                        continue;
                    }
                    if self.slots.len() >= MAX_PANEL_SLOTS {
                        dropped += 1;
                        continue;
                    }
                    self.slots.push(PanelSlot {
                        slot: slot.clone(),
                        elements: elements.clone(),
                    });
                }
            }
        }
        dropped
    }

    /// The slots in draw order, oldest first.
    #[must_use]
    pub fn slots(&self) -> &[PanelSlot] {
        &self.slots
    }

    /// Whether anything would be drawn.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(|panel| panel.elements.is_empty())
    }

    /// How many rows the panes need at `width`, including the border rows.
    ///
    /// This is what the layout asks for, so it has to agree with what
    /// [`render`](Self::render) produces: a disagreement draws a panel clipped
    /// or leaves a blank strip.
    #[must_use]
    pub fn display_height(&self, width: usize) -> u16 {
        let rows: usize = self
            .slots
            .iter()
            .map(|panel| {
                crate::mod_elements::render_mod_ui_with_hits(&panel.elements, width.max(1))
                    .0
                    .len()
            })
            .sum();
        u16::try_from(rows).unwrap_or(u16::MAX)
    }

    /// Render every slot at `width`, top to bottom.
    #[must_use]
    pub fn render(&self, width: usize) -> Vec<ratatui::text::Line<'static>> {
        let mut lines = Vec::new();
        for panel in &self.slots {
            if panel.elements.is_empty() {
                continue;
            }
            lines.extend(crate::mod_elements::render_mod_ui(
                &panel.elements,
                width.max(1),
            ));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codape_core::ui_protocol::UiStyle;

    fn text(value: &str) -> UiElement {
        UiElement::Text {
            text: value.to_string(),
            style: UiStyle::Plain,
        }
    }

    fn set(slot: &str, value: &str) -> ModPanelUpdate {
        ModPanelUpdate::Set {
            slot: slot.to_string(),
            elements: vec![text(value)],
        }
    }

    fn clear(slot: &str) -> ModPanelUpdate {
        ModPanelUpdate::Clear {
            slot: slot.to_string(),
        }
    }

    #[test]
    fn a_slot_is_replaced_in_place_rather_than_added() {
        let mut store = PanelStore::new();
        assert_eq!(store.apply(&[set("status", "one")]), 0);
        assert_eq!(store.apply(&[set("status", "two")]), 0);

        assert_eq!(store.slots().len(), 1);
        assert_eq!(store.slots()[0].elements, vec![text("two")]);
    }

    #[test]
    fn a_clear_removes_the_slot_and_a_second_clear_is_harmless() {
        let mut store = PanelStore::new();
        store.apply(&[set("status", "one"), set("other", "two")]);
        assert_eq!(store.apply(&[clear("status")]), 0);
        assert_eq!(store.slots().len(), 1);
        assert_eq!(store.slots()[0].slot, "other");

        assert_eq!(store.apply(&[clear("status")]), 0);
        assert_eq!(
            store.slots().len(),
            1,
            "clearing a missing slot changes nothing"
        );
    }

    #[test]
    fn later_updates_in_one_call_win() {
        let mut store = PanelStore::new();
        store.apply(&[set("status", "one"), set("status", "two")]);
        assert_eq!(store.slots().len(), 1);
        assert_eq!(store.slots()[0].elements, vec![text("two")]);
    }

    #[test]
    fn the_slot_cap_drops_new_slots_and_keeps_the_old_ones() {
        let mut store = PanelStore::new();
        for index in 0..MAX_PANEL_SLOTS {
            assert_eq!(store.apply(&[set(&format!("slot{index}"), "x")]), 0);
        }
        assert_eq!(store.slots().len(), MAX_PANEL_SLOTS);

        // A new slot past the cap is dropped...
        assert_eq!(store.apply(&[set("one-too-many", "x")]), 1);
        assert_eq!(store.slots().len(), MAX_PANEL_SLOTS);
        // ...but replacing an existing one still works.
        assert_eq!(store.apply(&[set("slot0", "replaced")]), 0);
        assert_eq!(store.slots()[0].elements, vec![text("replaced")]);
    }

    #[test]
    fn the_height_agrees_with_what_is_drawn() {
        let mut store = PanelStore::new();
        store.apply(&[set("status", "hello")]);
        let width = 40;
        assert_eq!(
            usize::from(store.display_height(width)),
            store.render(width).len()
        );
    }

    #[test]
    fn an_empty_tree_is_not_a_panel() {
        let mut store = PanelStore::new();
        store.apply(&[ModPanelUpdate::Set {
            slot: "blank".to_string(),
            elements: Vec::new(),
        }]);
        assert!(store.is_empty(), "a panel with no elements draws nothing");
        assert_eq!(store.display_height(40), 0);
        assert!(store.render(40).is_empty());
    }
}
