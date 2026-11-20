//! The rows behind each modal list.
//!
//! [`codape_ui::picker::Picker`] owns the interaction — filtering, the
//! highlight, accept-or-refuse. This module owns what each list is *made of*,
//! which is the part that needs the session: the agent roster, the rewind
//! targets, the themes, the plugins, the marketplaces.
//!
//! Split out of `repl.rs` for two reasons. The key handler there is already
//! over the lint's line limit, and a list-building function is not a key
//! branch. And the rows are worth testing without a terminal: every function
//! here is a pure mapping from session facts to [`PickerItem`] rows, except
//! the two that take a lock.

use codape_ui::picker::{Picker, PickerItem};

/// What an accepted row of the agent list means.
///
/// The picker's `key` is a string, so the interpretation lives here: prefix a
/// row's key so the session can tell "focus this agent" from "open its log"
/// without a parallel enum threaded through the UI crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentChoice {
    /// Point the observable subscription at this agent.
    Follow(String),
    /// Show this agent's NDJSON event log.
    Logs(String),
    /// Show or hide this agent's task in the roster.
    TogglePin(String),
    /// Give this agent a name of its own.
    Rename(String),
    /// Put this agent in a group, or move it to another one.
    Group(String),
}

impl AgentChoice {
    /// The key [`PickerItem`] carries for this choice.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Follow(id) => format!("follow:{id}"),
            Self::Logs(id) => format!("logs:{id}"),
            Self::TogglePin(id) => format!("pin:{id}"),
            Self::Rename(id) => format!("rename:{id}"),
            Self::Group(id) => format!("group:{id}"),
        }
    }

    /// Read a key back, or say why it is not one we wrote.
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        let (kind, id) = key.split_once(':')?;
        if id.is_empty() {
            return None;
        }
        match kind {
            "follow" => Some(Self::Follow(id.to_string())),
            "logs" => Some(Self::Logs(id.to_string())),
            "pin" => Some(Self::TogglePin(id.to_string())),
            "rename" => Some(Self::Rename(id.to_string())),
            "group" => Some(Self::Group(id.to_string())),
            _ => None,
        }
    }
}

/// One agent as the roster needs it.
///
/// A plain struct rather than `&AgentInstance` so the row builder can be
/// tested without a live manager holding a lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    /// The task id — what `/agents logs` and `/agents follow` take.
    pub id: String,
    /// The agent type, as `Debug` spells it.
    pub kind: String,
    /// Lowercased status, as the `/agents list` text path prints it.
    pub status: String,
    /// The first line of the agent's prompt.
    pub prompt: String,
    /// Whether the agent has a transcript to continue.
    ///
    /// Drives whether the "follow" row is offered: following an agent whose
    /// conversation is over subscribes to a channel nobody will write to, and
    /// the row would look live while doing nothing.
    pub resumable: bool,
    /// Whether this agent is pinned.
    ///
    /// Pinning is a display preference, not a lifecycle fact: a roster of
    /// thirty agents is mostly history, and the two or three being watched
    /// should not have to be found by scrolling past the rest each time.
    pub pinned: bool,
    /// The name this agent was given, when it has one.
    ///
    /// Kept beside the id rather than replacing it: the id is what the engine,
    /// the logs and `/agents logs` all address the agent by, and a display
    /// name that took over the key would rename the row out of reach of the
    /// commands that act on it.
    pub name: Option<String>,
    /// The group this agent was put in, when it has one.
    pub group: Option<String>,
}

/// Build the agent roster.
///
/// Five rows per agent — follow, logs, pin, rename, group — because those are
/// the things `/agents` can do with one, and a list that only did some of them
/// would leave the rest reachable only by typing the id from memory.
///
/// Pinned agents come first, because the point of pinning is that the row is
/// where the eye starts. Within the pinned and the unpinned half, agents that
/// share a group sit together and the groups are ordered by name, with the
/// ungrouped ones ahead of them — a group is only worth naming if naming it
/// moves the rows, otherwise it is a label that leaves a ten-agent roster as
/// hard to scan as it was. The sort is stable, so members of one group keep
/// the order the manager listed them in.
///
/// The rows are still built per agent rather than per section: a person who
/// pins or groups an agent should not have to learn a new key to follow it.
#[must_use]
pub fn agent_rows(agents: &[AgentRow]) -> Picker {
    if agents.is_empty() {
        return Picker::new(
            "Agents",
            vec![
                PickerItem::new("", "No agents have been spawned in this session")
                    .disabled("spawn one with the Agent tool"),
            ],
        );
    }

    let mut ordered: Vec<&AgentRow> = agents.iter().collect();
    ordered.sort_by(|left, right| {
        right
            .pinned
            .cmp(&left.pinned)
            .then_with(|| left.group.is_some().cmp(&right.group.is_some()))
            .then_with(|| left.group.cmp(&right.group))
    });

    let mut items = Vec::with_capacity(agents.len() * 5);
    for agent in ordered {
        let pin = if agent.pinned { "unpin" } else { "pin" };
        // The name when there is one, the id when there is not: a person who
        // named an agent did so to recognise it, and the id is still one row
        // away on every row that names it explicitly.
        let shown = agent.name.as_deref().unwrap_or(&agent.id);
        let group = agent.group.as_deref().map_or_else(
            || format!("{id} · group", id = agent.id),
            |group| format!("{id} · group: {group}", id = agent.id),
        );
        items.push(
            PickerItem::new(
                AgentChoice::Follow(agent.id.clone()).key(),
                format!(
                    "{marker}{shown} · {kind} · follow",
                    marker = if agent.pinned { "★ " } else { "" },
                    shown = shown,
                    kind = agent.kind
                ),
            )
            .with_detail(agent.status.clone()),
        );

        let logs = PickerItem::new(
            AgentChoice::Logs(agent.id.clone()).key(),
            format!("{id} · logs", id = agent.id),
        )
        .with_detail(if agent.resumable {
            "transcript available".to_string()
        } else {
            agent.prompt.clone()
        });
        items.push(logs);

        items.push(
            PickerItem::new(
                AgentChoice::TogglePin(agent.id.clone()).key(),
                format!("{id} · {pin}", id = agent.id),
            )
            .with_detail(if agent.pinned {
                "keep at the top"
            } else {
                "move to the top"
            }),
        );

        items.push(
            PickerItem::new(
                AgentChoice::Rename(agent.id.clone()).key(),
                format!("{shown} · rename"),
            )
            .with_detail("give it a name of its own"),
        );

        items.push(
            PickerItem::new(AgentChoice::Group(agent.id.clone()).key(), group)
                .with_detail("file it with the others doing the same job"),
        );
    }
    Picker::new("Agents", items)
}

/// Put a pinned id at the front of a list of ids, or take it off.
///
/// Order is the pin order, so the last agent pinned is the last of the pinned
/// group rather than the first: a set is not enough, because "top" for two
/// pins has to mean something.
#[must_use]
pub fn toggle_pin(pinned: &mut Vec<String>, id: &str) -> bool {
    if let Some(index) = pinned.iter().position(|pinned| pinned == id) {
        pinned.remove(index);
        false
    } else {
        pinned.push(id.to_string());
        true
    }
}

/// A roster name: the text a `rename` or `group` dialog keeps.
///
/// Trimmed, and empty means "none" rather than a name that is the empty
/// string — clearing the field is how a name or group is taken off, and a
/// label that reads `a1 · group: ` is not a group.
#[must_use]
pub fn clean_name(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// One rewritable point in the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindRow {
    /// One-based message number, the same one `/rewind <n>` takes.
    pub number: usize,
    /// The role, for the row's left half.
    pub role: String,
    /// The message's first line, truncated for the row.
    pub preview: String,
}

/// Build the rewind list.
///
/// The same one-based numbering `/rewind <n>` prints, so a person who learned
/// the number from the command finds the same row here. Newest first: the
/// reason to rewind is usually recent.
#[must_use]
pub fn rewind_rows(rows: &[RewindRow]) -> Picker {
    if rows.is_empty() {
        return Picker::new(
            "Rewind",
            vec![
                PickerItem::new("", "Nothing to rewind to").disabled("the session has no messages"),
            ],
        );
    }
    let items = rows
        .iter()
        .rev()
        .map(|row| {
            PickerItem::new(
                row.number.to_string(),
                format!(
                    "{n} · {role} · {preview}",
                    n = row.number,
                    role = row.role,
                    preview = row.preview
                ),
            )
        })
        .collect();
    Picker::new("Rewind to message", items)
}

/// Build the theme list.
///
/// `current` is annotated rather than omitted, so the list always says what the
/// session is on.
#[must_use]
pub fn theme_rows(names: &[String], current: &str) -> Picker {
    if names.is_empty() {
        return Picker::new(
            "Theme",
            vec![PickerItem::new("", "No themes available").disabled("")],
        );
    }
    let items = names
        .iter()
        .map(|name| {
            let detail = if name == current { "current" } else { "" };
            PickerItem::new(name.clone(), name.clone()).with_detail(detail)
        })
        .collect();
    Picker::new("Theme", items)
}

/// One earlier session of this workspace.
///
/// A plain struct rather than `SessionMeta` so the row builder can be tested
/// without a transcript store or an index: the mapping is what the session
/// provides, and the row is what it looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// The id the transcript is filed under — what `/resume <id>` takes.
    pub id: String,
    /// The title taken from the session's first prompt, when it has one.
    pub title: Option<String>,
    /// How long ago it was last active, already humanised.
    pub age: String,
    /// Total tokens, when the index knows them.
    pub tokens: i64,
}

/// The key a session row accepts with.
///
/// Prefixed so [`Picker`]'s single key string can say which list a row came
/// from, the same way `plugin:` and `follow:` do.
#[must_use]
pub fn session_key(id: &str) -> String {
    format!("session:{id}")
}

/// Read [`session_key`] back.
#[must_use]
pub fn session_id_from_key(key: &str) -> Option<&str> {
    key.strip_prefix("session:").filter(|id| !id.is_empty())
}

/// Build the session picker.
///
/// Newest first, which is the order [`codape_memory::session_index`] hands
/// back: the session someone wants is nearly always the one they were last in.
/// The id is in the row because it is also reachable to a script and to
/// `/resume <id>`, and a title alone does not let a person go and look at the
/// file.
#[must_use]
pub fn session_rows(rows: &[SessionRow]) -> Picker {
    if rows.is_empty() {
        return Picker::new(
            "Sessions",
            vec![
                PickerItem::new("", "No earlier sessions in this workspace")
                    .disabled("this is the first one"),
            ],
        );
    }
    let items = rows
        .iter()
        .map(|row| {
            let label = row
                .title
                .as_deref()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or("(untitled)");
            let mut detail = format!("{} · {}", row.age, row.id);
            if row.tokens > 0 {
                use std::fmt::Write as _;
                let _ = write!(detail, " · {} tokens", row.tokens);
            }
            PickerItem::new(session_key(&row.id), label.to_string()).with_detail(detail)
        })
        .collect();
    Picker::new("Sessions", items)
}

/// One installed plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRow {
    /// The plugin's name.
    pub name: String,
    /// What the browser says about it: version, state and where it was loaded
    /// from, when the session knows them.
    pub detail: String,
    /// Whether the plugin loads in a session.
    pub enabled: bool,
    /// Whether the row is pinned to the top.
    pub favorite: bool,
}

/// What an accepted row of the plugin browser means.
///
/// The browser has more than one answer per row — a plugin can be inspected,
/// switched off, or pinned — so the row carries all three as separate keys
/// rather than the picker growing a mode. Every key for one plugin sorts
/// together because they share the name, and the filter matches the key, so
/// typing a plugin's name narrows to exactly its own verbs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginChoice {
    /// Show what this plugin contributes and where it lives.
    Inspect(String),
    /// Switch the plugin on or off for this machine.
    Toggle(String),
    /// Pin or unpin the plugin.
    Favorite(String),
    /// Open a field for the source to install from.
    ///
    /// The only row in this list that is not about a plugin that is already
    /// installed: an install needs a source typed out (a path, a git remote, a
    /// marketplace name), and neither a highlighted row nor a keypress can
    /// carry one. It is first, because adding is what a person opens an empty
    /// or short list to do.
    InstallFrom,
}

impl PluginChoice {
    /// The key [`PickerItem`] carries for this choice.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Inspect(name) => format!("plugin:{name}"),
            Self::Toggle(name) => format!("plugin-toggle:{name}"),
            Self::Favorite(name) => format!("plugin-favorite:{name}"),
            Self::InstallFrom => PLUGIN_INSTALL_KEY.to_string(),
        }
    }

    /// Read a key back, or `None` when it is not one this browser wrote.
    ///
    /// `plugin:` is matched last so the longer prefixes win: a plugin
    /// literally named `toggle:x` is not reachable as a verb, and that is the
    /// right trade — the alternative is a name that can shadow an action.
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        if key == PLUGIN_INSTALL_KEY {
            return Some(Self::InstallFrom);
        }
        let (kind, name) = key.split_once(':')?;
        if name.is_empty() {
            return None;
        }
        match kind {
            "plugin-toggle" => Some(Self::Toggle(name.to_string())),
            "plugin-favorite" => Some(Self::Favorite(name.to_string())),
            "plugin" => Some(Self::Inspect(name.to_string())),
            _ => None,
        }
    }
}

/// The install row's key.
///
/// Not `plugin-install:<name>`: this row installs something that is not in the
/// list yet, so there is no name to carry.
pub const PLUGIN_INSTALL_KEY: &str = "plugin-install";

/// Build the plugin browser.
///
/// Three rows per installed plugin — open, switch, pin — because the browser
/// used to be read-only: accepting a row said "Installed plugin: name", which
/// is what the row already said. The verbs exist on the row rather than as a
/// key handler because the picker owns the keyboard while it is up, so a modal
/// list cannot also answer `d` and `f`.
///
/// A switched-off plugin keeps its rows. A browser that hid it would leave no
/// way back, and "off" is a preference, not an uninstall — the directory, the
/// manifest and `codape plugin list` all still know about it.
///
/// The first row is the one that is not about a plugin in the list: it opens a
/// field for the source to install from. That is the one verb the rows cannot
/// carry themselves — a source is a path, a remote or a marketplace name, and
/// a highlighted row has nothing to put in it. It is present on an empty list
/// too, which is exactly where it is wanted; the old empty state only named
/// the command instead.
#[must_use]
pub fn plugin_rows(plugins: &[PluginRow], marketplaces: &[(String, String)]) -> Picker {
    // Pinned plugins first, then alphabetical. Sorted here rather than by the
    // caller because the browser is the only reader of the order, and the
    // manager's own listing comes out of a `DashMap`, whose order is not
    // stable between openings — a row that moves between openings is a row
    // people type the wrong name into.
    let mut plugins: Vec<&PluginRow> = plugins.iter().collect();
    plugins.sort_by(|a, b| {
        b.favorite
            .cmp(&a.favorite)
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut items: Vec<PickerItem> = Vec::with_capacity(plugins.len() * 3 + marketplaces.len() + 1);
    items.push(
        PickerItem::new(
            PluginChoice::InstallFrom.key(),
            "install a plugin…".to_string(),
        )
        .with_detail("a directory, a git remote, an archive URL, or a marketplace name"),
    );
    for plugin in plugins {
        let state = if plugin.enabled { "" } else { " (off)" };
        let pin = if plugin.favorite { " ★" } else { "" };
        let detail = if plugin.detail.is_empty() {
            String::new()
        } else {
            plugin.detail.clone()
        };
        items.push(
            PickerItem::new(
                PluginChoice::Inspect(plugin.name.clone()).key(),
                format!("{}{state}{pin}", plugin.name),
            )
            .with_detail(detail),
        );
        items.push(
            PickerItem::new(
                PluginChoice::Toggle(plugin.name.clone()).key(),
                format!(
                    "{} · {}",
                    plugin.name,
                    if plugin.enabled {
                        "turn off"
                    } else {
                        "turn on"
                    }
                ),
            )
            .with_detail(off_note()),
        );
        items.push(
            PickerItem::new(
                PluginChoice::Favorite(plugin.name.clone()).key(),
                format!(
                    "{} · {}",
                    plugin.name,
                    if plugin.favorite { "unpin" } else { "pin" }
                ),
            )
            .with_detail("pinned rows sort first"),
        );
    }
    for (name, url) in marketplaces {
        items.push(
            PickerItem::new(format!("marketplace:{name}"), name.clone())
                .with_detail(format!("marketplace · {url}")),
        );
    }
    Picker::new("Plugins", items)
}

/// The annotation on a switch row: what "off" actually does.
///
/// It says the effect on the *next* session because that is the truth: the
/// plugin's commands, skills and hooks were registered at startup and the
/// manager cannot take them back mid-session, so a switch that claimed to take
/// effect immediately would be a lie the person only discovers by using a
/// command that still works.
fn off_note() -> String {
    "loads in new sessions".to_string()
}

/// What an accepted row of the attachment list means.
///
/// Indexes because an [`AttachmentRow`] carries no name of its own: an image
/// pasted from the clipboard has a media type and some bytes, nothing that
/// identifies it to a person. The index is stable because the list is a
/// snapshot taken when the picker opened, and picking a row closes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentChoice {
    /// Draw this picture in the transcript, where it can be seen.
    Show(usize),
    /// Take this picture off the next turn.
    Remove(usize),
    /// Take every picture off the next turn.
    RemoveAll,
}

impl AttachmentChoice {
    /// The key [`PickerItem`] carries for this choice.
    #[must_use]
    pub fn key(self) -> String {
        match self {
            Self::Show(index) => format!("image-show:{index}"),
            Self::Remove(index) => format!("image-remove:{index}"),
            Self::RemoveAll => "image-remove-all".to_string(),
        }
    }

    /// Read a key back, or say why it is not one we wrote.
    ///
    /// The range is the caller's to check: an index is a position in the list
    /// the rows were built from, and only the session knows how long it is now.
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        if key == Self::RemoveAll.key() {
            return Some(Self::RemoveAll);
        }
        let (kind, index) = key.split_once(':')?;
        let index: usize = index.parse().ok()?;
        match kind {
            "image-show" => Some(Self::Show(index)),
            "image-remove" => Some(Self::Remove(index)),
            _ => None,
        }
    }
}

/// One image waiting to ride on the next turn, as the browser needs it.
///
/// A plain struct rather than [`codape_core::types::ImageBlock`] so the row
/// builder does not carry the base64 payload around to render a size, and so
/// these tests need no image data at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRow {
    /// The MIME type, e.g. `image/png`.
    pub media_type: String,
    /// Decoded size in bytes — the number the base64 payload stands for, not
    /// the length of the payload.
    pub bytes: usize,
}

/// Build the list of images attached to the next turn.
///
/// Two rows per image — show and remove — because those are the two things a
/// person wants to do with one they are about to send, and a list that only
/// offered one would leave the other unreachable. Show comes first: the
/// common question is "which screenshot is this", and it is answered without
/// changing anything.
#[must_use]
pub fn attachment_rows(images: &[AttachmentRow]) -> Picker {
    if images.is_empty() {
        return Picker::new(
            "Attached images",
            vec![
                PickerItem::new("", "No images attached")
                    .disabled("paste one with Ctrl+V or @path"),
            ],
        );
    }

    let mut items = Vec::with_capacity(images.len() * 2 + 1);
    for (index, image) in images.iter().enumerate() {
        let number = index + 1;
        let size = human_size(image.bytes);
        items.push(
            PickerItem::new(
                AttachmentChoice::Show(index).key(),
                format!("{number}. {}", image.media_type),
            )
            .with_detail(format!("{size} · show")),
        );
        items.push(
            PickerItem::new(
                AttachmentChoice::Remove(index).key(),
                format!("{number}. {} · remove", image.media_type),
            )
            .with_detail(size),
        );
    }
    if images.len() > 1 {
        items.push(
            PickerItem::new(
                AttachmentChoice::RemoveAll.key(),
                format!("Remove all {} images", images.len()),
            )
            .with_detail("the next turn sends none"),
        );
    }
    Picker::new(format!("Attached images ({})", images.len()), items)
}

/// A byte count in the unit a person reads it in.
///
/// Screenshots are the common case and they are hundreds of kilobytes, so the
/// unit is chosen from the magnitude rather than fixed: "412 KB" says what the
/// request will cost in a way "421888" does not.
fn human_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;
    if bytes >= MB {
        // Tenths of a megabyte in integer arithmetic: the value is a label for
        // a person, and `bytes as f64` is a lossy cast clippy is right to flag
        // for a number that only ever needs one decimal place.
        let tenths = bytes.saturating_mul(10) / MB;
        format!("{}.{} MB", tenths / 10, tenths % 10)
    } else if bytes >= KB {
        format!("{} KB", (bytes + KB / 2) / KB)
    } else {
        format!("{bytes} B")
    }
}

/// One background job, as the job panel needs it.
///
/// A plain struct rather than a `JobRecord` so the row builder is testable
/// without a store on disk, and so the panel does not have to agree with the
/// record's field names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    /// The job's id, which is also how `JobOutput` and `JobKill` name it.
    pub id: String,
    /// `agent`, `shell`, `workflow` or `cron`.
    pub kind: String,
    /// `running`, `done`, `failed` or `interrupted`.
    pub state: String,
    /// What the job was asked to do.
    pub description: String,
    /// Whether the job has an output file to open.
    ///
    /// A cron definition never writes one, so `Enter` on its row says so
    /// rather than opening an empty pane as though the job had said nothing.
    pub has_output: bool,
}

/// What an accepted row of the job panel means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobChoice {
    /// Show the job's output in the detail overlay.
    Output(String),
    /// Stop the job's process group.
    Kill(String),
    /// Remove a finished job's record.
    Forget(String),
}

impl JobChoice {
    /// The key [`PickerItem`] carries for this choice.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Output(id) => format!("job-out:{id}"),
            Self::Kill(id) => format!("job-kill:{id}"),
            Self::Forget(id) => format!("job-forget:{id}"),
        }
    }

    /// Read a key back, or say why it is not one we wrote.
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        let (kind, id) = key.split_once(':')?;
        if id.is_empty() {
            return None;
        }
        match kind {
            "job-out" => Some(Self::Output(id.to_string())),
            "job-kill" => Some(Self::Kill(id.to_string())),
            "job-forget" => Some(Self::Forget(id.to_string())),
            _ => None,
        }
    }
}

/// Build the background-job panel.
///
/// Three rows per job — output, stop, forget — because those are the three
/// things a person wants from work that is already running or already over,
/// and the list is the only place all of them are reachable without typing.
/// Only what *can* be done is offered: a finished job has nothing to stop, and
/// a running one has nothing to forget.
#[must_use]
pub fn job_rows(rows: &[JobRow]) -> Picker {
    if rows.is_empty() {
        return Picker::new(
            "Background jobs",
            vec![
                PickerItem::new("", "No background jobs in this workspace")
                    .disabled("Bash run_in_background and background agents write them here"),
            ],
        );
    }

    let mut items = Vec::with_capacity(rows.len() * 3);
    for job in rows {
        let running = job.state == "running";
        items.push(
            PickerItem::new(
                JobChoice::Output(job.id.clone()).key(),
                format!("{} · {} · {}", job.id, job.kind, job.description),
            )
            .with_detail(if job.has_output {
                format!("{} · output", job.state)
            } else {
                job.state.clone()
            }),
        );
        if running {
            items.push(
                PickerItem::new(
                    JobChoice::Kill(job.id.clone()).key(),
                    format!("{} · stop", job.id),
                )
                .with_detail(job.kind.clone()),
            );
        } else {
            items.push(
                PickerItem::new(
                    JobChoice::Forget(job.id.clone()).key(),
                    format!("{} · forget", job.id),
                )
                .with_detail("delete the record"),
            );
        }
    }
    Picker::new(format!("Background jobs ({})", rows.len()), items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, status: &str) -> AgentRow {
        AgentRow {
            id: id.to_string(),
            kind: "Explore".into(),
            status: status.into(),
            prompt: "find the auth flow".into(),
            resumable: true,
            pinned: false,
            name: None,
            group: None,
        }
    }

    #[test]
    fn the_roster_offers_follow_logs_and_pin_per_agent() {
        let picker = agent_rows(&[agent("a1", "running"), agent("a2", "completed")]);
        assert_eq!(picker.items.len(), 10, "five rows each");
        let keys: Vec<&str> = picker.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "follow:a1",
                "logs:a1",
                "pin:a1",
                "rename:a1",
                "group:a1",
                "follow:a2",
                "logs:a2",
                "pin:a2",
                "rename:a2",
                "group:a2",
            ]
        );
        assert_eq!(picker.accept(), Some("follow:a1"));
    }

    /// A name is a label on the roster, not a new address: every row that acts
    /// on the agent still carries its id, because that is what the engine, the
    /// logs and `/agents logs` take.
    #[test]
    fn a_named_agent_is_called_by_its_name_and_addressed_by_its_id() {
        let mut named = agent("a1", "running");
        named.name = Some("auth scout".into());
        named.group = Some("investigation".into());
        let picker = agent_rows(&[named]);
        assert!(
            picker.items[0].label.contains("auth scout"),
            "the follow row does not use the name: {label}",
            label = picker.items[0].label
        );
        assert!(
            picker.items[3].label.contains("auth scout · rename"),
            "the rename row does not use the name: {label}",
            label = picker.items[3].label
        );
        assert!(
            picker.items[4].label.contains("group: investigation"),
            "the group row does not name the group: {label}",
            label = picker.items[4].label
        );
        let keys: Vec<&str> = picker.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys[0], "follow:a1", "the key is still the id");
        assert_eq!(keys[3], "rename:a1");
        assert_eq!(keys[4], "group:a1");
    }

    /// A pinned agent keeps all five of its rows — pinning moves the agent,
    /// it does not replace what can be done with it.
    #[test]
    fn a_pinned_agent_moves_to_the_top_with_its_rows() {
        let mut second = agent("a2", "running");
        second.pinned = true;
        let picker = agent_rows(&[agent("a1", "running"), second]);
        let keys: Vec<&str> = picker.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "follow:a2",
                "logs:a2",
                "pin:a2",
                "rename:a2",
                "group:a2",
                "follow:a1",
                "logs:a1",
                "pin:a1",
                "rename:a1",
                "group:a1",
            ],
            "the pinned agent is first and still has all five rows"
        );
        assert!(
            picker.items[0].label.contains('★'),
            "no pin mark on the row"
        );
        assert!(
            picker.items.iter().any(|item| item.label == "a1 · pin"),
            "the unpinned agent lost its pin row"
        );
    }

    /// A group is a row label *and* a place on the roster. Naming a group that
    /// moves nothing is a label a person reads once and then stops trusting.
    #[test]
    fn agents_sharing_a_group_sit_together() {
        let mut second = agent("a2", "running");
        second.group = Some("bug hunt".into());
        let mut third = agent("a3", "running");
        third.group = Some("bug hunt".into());
        let mut fourth = agent("a4", "running");
        fourth.group = Some("docs".into());

        let picker = agent_rows(&[third, agent("a1", "running"), fourth, second]);
        let keys: Vec<&str> = picker
            .items
            .iter()
            .filter(|item| item.key.starts_with("follow:"))
            .map(|item| item.key.as_str())
            .collect();
        assert_eq!(
            keys,
            ["follow:a1", "follow:a3", "follow:a2", "follow:a4"],
            "ungrouped first, then groups by name, members in input order"
        );
    }

    /// Pinning outranks grouping: the row a person pinned is the row at the
    /// top even when a group it does not belong to sorts earlier.
    #[test]
    fn a_pin_still_beats_a_group() {
        let mut pinned = agent("a4", "running");
        pinned.pinned = true;
        let mut grouped = agent("a1", "running");
        grouped.group = Some("aaa first alphabetically".into());

        let picker = agent_rows(&[grouped, pinned]);
        assert_eq!(
            picker.items[0].key, "follow:a4",
            "the pinned agent sank below a group"
        );
    }

    #[test]
    fn pinning_is_a_toggle_and_keeps_pin_order() {
        let mut pinned = Vec::new();
        assert!(toggle_pin(&mut pinned, "a1"), "first pin reports pinned");
        assert!(toggle_pin(&mut pinned, "a2"));
        assert_eq!(pinned, ["a1", "a2"], "later pins go last, not first");
        assert!(!toggle_pin(&mut pinned, "a1"), "the second call unpins");
        assert_eq!(pinned, ["a2"]);
        // The row for an agent nobody has pinned yet is the pin action, so
        // the same call has to add it rather than report that it did nothing.
        assert!(toggle_pin(&mut pinned, "a3"));
        assert_eq!(pinned, ["a2", "a3"]);
    }

    /// Emptying the field is how a name comes off, so whitespace is not a
    /// name — a label reading `a1 · group: ` is a group nobody can join.
    #[test]
    fn an_empty_name_is_no_name() {
        assert_eq!(clean_name(""), None);
        assert_eq!(clean_name("   "), None);
        assert_eq!(clean_name("\t\n"), None);
        assert_eq!(clean_name("  auth scout  ").as_deref(), Some("auth scout"));
    }

    #[test]
    fn a_row_key_round_trips_through_the_choice() {
        let choice = AgentChoice::Follow("a1".into());
        assert_eq!(AgentChoice::parse(&choice.key()), Some(choice));
        let choice = AgentChoice::TogglePin("a2".into());
        assert_eq!(AgentChoice::parse(&choice.key()), Some(choice));
        let choice = AgentChoice::Logs("weird:id".into());
        assert_eq!(
            AgentChoice::parse(&choice.key()),
            Some(AgentChoice::Logs("weird:id".into())),
            "an id containing a colon survives, because only the first is split"
        );
        assert_eq!(AgentChoice::parse("nonsense"), None);
        assert_eq!(
            AgentChoice::parse("follow:"),
            None,
            "an empty id is not a row"
        );
    }

    #[test]
    fn an_empty_roster_says_how_to_fill_it() {
        let picker = agent_rows(&[]);
        assert_eq!(picker.accept(), None, "nothing to choose");
        assert!(
            picker.refusal().is_some_and(|r| r.contains("unavailable")),
            "{:?}",
            picker.refusal()
        );
    }

    #[test]
    fn the_rewind_list_is_newest_first_and_keeps_command_numbering() {
        let rows = vec![
            RewindRow {
                number: 1,
                role: "User".into(),
                preview: "first".into(),
            },
            RewindRow {
                number: 2,
                role: "Assistant".into(),
                preview: "second".into(),
            },
        ];
        let picker = rewind_rows(&rows);
        assert_eq!(picker.accept(), Some("2"), "newest first");
        assert!(picker.items[0].label.contains("Assistant"));
    }

    #[test]
    fn an_empty_rewind_list_is_disabled_rather_than_absent() {
        let picker = rewind_rows(&[]);
        assert_eq!(picker.accept(), None);
        assert_eq!(picker.items.len(), 1);
    }

    #[test]
    fn the_theme_list_marks_the_current_one() {
        let picker = theme_rows(&["dark".to_string(), "light".to_string()], "light");
        let light = picker
            .items
            .iter()
            .find(|i| i.key == "light")
            .expect("light");
        assert_eq!(light.detail, "current");
        let dark = picker.items.iter().find(|i| i.key == "dark").expect("dark");
        assert!(dark.detail.is_empty());
    }

    #[test]
    fn the_plugin_list_covers_plugins_and_marketplaces() {
        let picker = plugin_rows(
            &[PluginRow {
                name: "fmt".into(),
                detail: "1.2.3 · active".into(),
                enabled: true,
                favorite: false,
            }],
            &[("acme".to_string(), "https://example.com/m.json".to_string())],
        );
        assert_eq!(
            picker.items.len(),
            5,
            "install row, three verbs and a marketplace"
        );
        assert!(picker.items.iter().any(|i| i.key == "plugin:fmt"));
        assert!(picker.items.iter().any(|i| i.key == "plugin-toggle:fmt"));
        assert!(picker.items.iter().any(|i| i.key == "plugin-favorite:fmt"));
        assert!(picker.items.iter().any(|i| i.key == "marketplace:acme"));
        let marketplace = picker
            .items
            .iter()
            .find(|i| i.key == "marketplace:acme")
            .expect("marketplace row");
        assert!(
            marketplace.detail.contains("https://example.com/m.json"),
            "the row must name the URL it resolves against: {:?}",
            marketplace.detail
        );
    }

    /// The browser used to be read-only: accepting a row answered "Installed
    /// plugin: name", which the row already said. Each row now offers an
    /// action, and the row says which one it will take.
    #[test]
    fn a_plugin_row_offers_the_verb_it_will_perform() {
        let picker = plugin_rows(
            &[PluginRow {
                name: "fmt".into(),
                detail: String::new(),
                enabled: true,
                favorite: false,
            }],
            &[],
        );
        let label = |key: &str| {
            picker
                .items
                .iter()
                .find(|item| item.key == key)
                .map(|item| item.label.clone())
                .unwrap_or_default()
        };
        assert!(label("plugin-toggle:fmt").contains("turn off"));
        assert!(label("plugin-favorite:fmt").contains("pin"));
        assert_eq!(
            picker.accept(),
            Some(PLUGIN_INSTALL_KEY),
            "the install row is first, because adding is what a short list is opened for"
        );
    }

    /// The one row that is not about a plugin in the list opens a field; it
    /// carries no name because there is nothing to install yet.
    #[test]
    fn the_browser_can_start_an_install() {
        let picker = plugin_rows(&[], &[]);
        assert_eq!(picker.items.len(), 1, "an empty list still offers it");
        assert_eq!(picker.items[0].key, PLUGIN_INSTALL_KEY);
        assert!(picker.items[0].label.contains("install"));
        assert!(
            picker.items[0].detail.contains("marketplace name"),
            "the row does not say what a source can be: {:?}",
            picker.items[0].detail
        );
        assert_eq!(
            PluginChoice::parse(PLUGIN_INSTALL_KEY),
            Some(PluginChoice::InstallFrom)
        );
    }

    #[test]
    fn an_off_plugin_is_still_listed_and_says_so() {
        let picker = plugin_rows(
            &[PluginRow {
                name: "fmt".into(),
                detail: String::new(),
                enabled: false,
                favorite: true,
            }],
            &[],
        );
        // A browser that hid it would leave no way back.
        assert_eq!(picker.items.len(), 4, "install row plus three");
        assert!(
            picker.items[1].label.contains("(off)"),
            "{:?}",
            picker.items[1]
        );
        assert!(picker.items[1].label.contains('★'), "pinned");
        assert!(
            picker
                .items
                .iter()
                .any(|item| item.key == "plugin-toggle:fmt" && item.label.contains("turn on")),
            "the switch offers the way back"
        );
    }

    #[test]
    fn pinned_plugins_sort_first() {
        let rows: Vec<PluginRow> = ["alpha", "zeta", "beta"]
            .into_iter()
            .map(|name| PluginRow {
                name: name.into(),
                detail: String::new(),
                enabled: true,
                favorite: name == "zeta",
            })
            .collect();
        let picker = plugin_rows(&rows, &[]);
        let names: Vec<&str> = picker
            .items
            .iter()
            .filter(|item| item.key.starts_with("plugin:"))
            .map(|item| item.label.as_str())
            .collect();
        assert_eq!(names[0], "zeta ★", "pinned first");
        assert_eq!(names[1], "alpha");
        assert_eq!(names[2], "beta");
    }

    /// The three prefixes have to survive the round trip unchanged, and
    /// `plugin:` must not swallow the two longer ones.
    #[test]
    fn plugin_choice_keys_round_trip_and_longer_prefixes_win() {
        for choice in [
            PluginChoice::Inspect("fmt".into()),
            PluginChoice::Toggle("fmt".into()),
            PluginChoice::Favorite("fmt".into()),
            PluginChoice::InstallFrom,
        ] {
            assert_eq!(PluginChoice::parse(&choice.key()), Some(choice));
        }
        assert_eq!(
            PluginChoice::parse("plugin-toggle:weird:name"),
            Some(PluginChoice::Toggle("weird:name".into()))
        );
        assert_eq!(PluginChoice::parse("plugin:"), None);
        assert_eq!(PluginChoice::parse("marketplace:acme"), None);
    }

    /// An empty list is not a dead end: its one row starts an install.
    #[test]
    fn an_empty_plugin_list_offers_the_install_row() {
        let picker = plugin_rows(&[], &[]);
        assert_eq!(picker.accept(), Some(PLUGIN_INSTALL_KEY));
    }

    #[test]
    fn a_session_row_carries_its_id_out_of_band_from_its_title() {
        let picker = session_rows(&[SessionRow {
            id: "abc123".into(),
            title: Some("fix the flaky test".into()),
            age: "2h ago".into(),
            tokens: 1234,
        }]);
        assert_eq!(picker.accept(), Some("session:abc123"));
        assert_eq!(picker.items[0].label, "fix the flaky test");
        // The id is on the row as well as in the key: a person reusing it in
        // `/resume <id>` needs to read it, and the key is not drawn.
        assert!(picker.items[0].detail.contains("abc123"));
        assert!(picker.items[0].detail.contains("1234 tokens"));
    }

    #[test]
    fn an_untitled_session_is_labelled_rather_than_left_blank() {
        let picker = session_rows(&[SessionRow {
            id: "s1".into(),
            title: Some("   ".into()),
            age: "just now".into(),
            tokens: 0,
        }]);
        assert_eq!(picker.items[0].label, "(untitled)");
        assert!(
            !picker.items[0].detail.contains("tokens"),
            "an unknown count must not be drawn as zero: {:?}",
            picker.items[0].detail
        );
    }

    #[test]
    fn a_session_key_round_trips_and_rejects_other_lists() {
        assert_eq!(
            session_id_from_key(&session_key("abc")),
            Some("abc"),
            "a key written by this module is read by it"
        );
        assert_eq!(session_id_from_key("session:"), None);
        // Every other list's key must be refused, or accepting a theme row
        // would try to resume a session named after it.
        assert_eq!(session_id_from_key("plugin:fmt"), None);
        assert_eq!(session_id_from_key("abc"), None);
    }

    #[test]
    fn an_empty_session_list_says_this_is_the_first_one() {
        let picker = session_rows(&[]);
        assert_eq!(picker.accept(), None);
        assert_eq!(picker.items.len(), 1);
    }

    fn attachment(media_type: &str, bytes: usize) -> AttachmentRow {
        AttachmentRow {
            media_type: media_type.into(),
            bytes,
        }
    }

    #[test]
    fn the_attachment_browser_shows_and_removes_each_picture() {
        let picker = attachment_rows(&[
            attachment("image/png", 421_888),
            attachment("image/jpeg", 2_097_152),
        ]);
        let keys: Vec<&str> = picker.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "image-show:0",
                "image-remove:0",
                "image-show:1",
                "image-remove:1",
                "image-remove-all",
            ]
        );
        assert_eq!(picker.items[0].label, "1. image/png");
        assert_eq!(picker.items[0].detail, "412 KB · show");
        assert_eq!(picker.items[3].detail, "2.0 MB");
        assert_eq!(picker.items[4].label, "Remove all 2 images");
    }

    #[test]
    fn a_lone_attachment_offers_no_remove_all() {
        // "Remove all" beside one row is a second way to say the row above it.
        let picker = attachment_rows(&[attachment("image/png", 1024)]);
        assert_eq!(picker.items.len(), 2);
        assert!(
            !picker.items.iter().any(|i| i.key == "image-remove-all"),
            "{:?}",
            picker.items
        );
    }

    #[test]
    fn an_empty_attachment_list_names_how_to_attach_one() {
        let picker = attachment_rows(&[]);
        assert_eq!(picker.accept(), None);
        assert_eq!(picker.items.len(), 1);
        assert_eq!(
            picker.refusal(),
            picker.refusal(),
            "asking twice gives the same answer"
        );
        assert!(
            picker.items[0].label.contains("No images attached"),
            "{:?}",
            picker.items[0]
        );
    }

    #[test]
    fn an_attachment_key_round_trips_and_rejects_other_lists() {
        for choice in [
            AttachmentChoice::Show(0),
            AttachmentChoice::Show(12),
            AttachmentChoice::Remove(3),
            AttachmentChoice::RemoveAll,
        ] {
            // RemoveAll parses out of its own key, and Show parses out of a
            // key that names an index — so a round trip keeps the variant.
            assert_eq!(AttachmentChoice::parse(&choice.key()), Some(choice));
        }
        assert_eq!(AttachmentChoice::parse("image-show:"), None);
        assert_eq!(AttachmentChoice::parse("image-show:x"), None);
        assert_eq!(AttachmentChoice::parse("logs:0"), None);
        assert_eq!(
            AttachmentChoice::parse("plugin:fmt"),
            None,
            "a plugin row must not be read as an image"
        );
    }

    #[test]
    fn a_size_is_written_in_the_unit_it_is_read_in() {
        assert_eq!(super::human_size(0), "0 B");
        assert_eq!(super::human_size(512), "512 B");
        assert_eq!(super::human_size(1024), "1 KB");
        assert_eq!(super::human_size(421_888), "412 KB");
        assert_eq!(super::human_size(1024 * 1024), "1.0 MB");
        assert_eq!(super::human_size(1024 * 1024 + 512 * 1024), "1.5 MB");
    }

    fn running_job() -> JobRow {
        JobRow {
            id: "job-1".into(),
            kind: "shell".into(),
            state: "running".into(),
            description: "npm run dev".into(),
            has_output: true,
        }
    }

    fn finished_job() -> JobRow {
        JobRow {
            id: "job-2".into(),
            kind: "agent".into(),
            state: "done".into(),
            description: "review the diff".into(),
            has_output: true,
        }
    }

    #[test]
    fn a_running_job_offers_stop_and_a_finished_one_offers_forget() {
        let picker = job_rows(&[running_job(), finished_job()]);
        let keys: Vec<&str> = picker.items.iter().map(|item| item.key.as_str()).collect();
        assert!(keys.contains(&"job-out:job-1"), "{keys:?}");
        assert!(keys.contains(&"job-kill:job-1"), "{keys:?}");
        assert!(
            !keys.contains(&"job-forget:job-1"),
            "a running job's record must not be offered for deletion: {keys:?}"
        );
        assert!(keys.contains(&"job-out:job-2"), "{keys:?}");
        assert!(keys.contains(&"job-forget:job-2"), "{keys:?}");
        assert!(
            !keys.contains(&"job-kill:job-2"),
            "a finished job has nothing to stop: {keys:?}"
        );
    }

    #[test]
    fn a_job_without_output_says_its_state_instead_of_offering_a_pane() {
        let picker = job_rows(&[JobRow {
            id: "cron-1".into(),
            kind: "cron".into(),
            state: "running".into(),
            description: "every 5m".into(),
            has_output: false,
        }]);
        assert_eq!(picker.items[0].detail, "running");
    }

    #[test]
    fn an_empty_job_list_says_where_jobs_come_from() {
        let picker = job_rows(&[]);
        assert_eq!(picker.items.len(), 1);
        assert!(picker.items[0].label.contains("No background jobs"));
    }

    #[test]
    fn a_job_key_round_trips_and_rejects_other_lists() {
        for choice in [
            JobChoice::Output("j1".into()),
            JobChoice::Kill("j1".into()),
            JobChoice::Forget("j1".into()),
        ] {
            assert_eq!(JobChoice::parse(&choice.key()), Some(choice));
        }
        assert_eq!(JobChoice::parse("job-out:"), None);
        assert_eq!(JobChoice::parse("logs:j1"), None);
        assert_eq!(
            JobChoice::parse("job-out:kill:j1"),
            Some(JobChoice::Output("kill:j1".into())),
            "the first colon splits, so an id may contain one"
        );
    }
}
