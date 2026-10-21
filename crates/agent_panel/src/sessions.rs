//! The agent conversations of each graph (spec 011), as Zed keeps its threads: Studio's own
//! index, because every graph's agent works in the same directory and the agent's own
//! `session/list` cannot tell graphs apart. Per conversation: the agent's session id, its title,
//! when it started and was last active, the branch its changes go to, and whether it is pinned
//! or archived. Ids and titles only: the conversation stays with the agent, which replays it
//! when Studio loads the session. Kept in an owner-only file beside the workspaces.
//!
//! What each conversation is doing right now (running, waiting for the person, stopped with an
//! error) and whether it finished unseen live here too, in memory only.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use gpui::{App, AppContext as _, Context, Entity, Global};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One conversation with an agent on a graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// The agent's session id.
    pub session: String,
    /// The graph, as its key (`<server url>/<graph id>`, `GraphSession::overrides_key`).
    pub graph: String,
    /// The agent command, joined with spaces: a session belongs to the agent that made it.
    pub agent: String,
    /// The agent's title for it, else the start of the first message; none until one of them
    /// names it.
    pub title: Option<String>,
    /// The person's own name for it, which wins over the agent's.
    pub title_override: Option<String>,
    pub created_at: DateTime<Utc>,
    /// The last message the person sent. The list sorts by it, as Zed's sorts by the last
    /// interaction, so a conversation finishing in the background does not move its row.
    pub updated_at: DateTime<Utc>,
    /// The agent's branch, and whether it was created yet.
    pub branch: String,
    pub branch_created: bool,
    pub pinned: bool,
    pub archived: bool,
    /// The directory the agent worked in for it, where it runs again.
    pub cwd: PathBuf,
}

impl SessionRecord {
    /// The title to show.
    pub fn shown_title(&self) -> &str {
        self.title_override
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or("New conversation")
    }
}

/// What a conversation is doing, while its tab is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Live {
    Running,
    /// A change or a tool waits for the person's answer.
    Waiting,
    /// The agent stopped with an error.
    Failed,
}

/// The index, shared by every agent tab and the Agent sidebar panel.
pub struct AgentSessions {
    path: PathBuf,
    records: Vec<SessionRecord>,
    /// Whether changes are written to the file: not when it held what Studio could not read
    /// and could not be set aside, since a save would replace it. They are kept for this run.
    saving: bool,
    live: HashMap<String, Live>,
    /// Conversations whose turn ended while their tab was not the one shown.
    unread: HashSet<String>,
    /// Conversations deleted in this run: an open tab of one closes.
    deleted: HashSet<String>,
}

struct GlobalSessions(Entity<AgentSessions>);

impl Global for GlobalSessions {}

impl AgentSessions {
    /// The one index of this run of Studio.
    pub fn global(cx: &mut App) -> Entity<AgentSessions> {
        if let Some(global) = cx.try_global::<GlobalSessions>() {
            return global.0.clone();
        }
        let path = studio_settings::data_dir()
            .join("agent")
            .join("sessions.json");
        let servers = graph_store::named_keys::known_servers(cx);
        let sessions = cx.new(|_| {
            let mut sessions = AgentSessions::load(path);
            sessions.move_named_graphs(&servers);
            sessions
        });
        cx.set_global(GlobalSessions(sessions.clone()));
        sessions
    }

    fn load(path: PathBuf) -> Self {
        let Stored { value, saving } = read_records(&path);
        Self {
            path,
            records: value,
            saving,
            live: HashMap::new(),
            unread: HashSet::new(),
            deleted: HashSet::new(),
        }
    }

    /// Files the conversations a build before 2026-10-20 filed under a graph's server name
    /// under the graph's key instead, when the name is one of `servers` (name, address); each
    /// keeps its working directory, where the agent keeps its record of it. Part of the one
    /// conversion in `graph_store::named_keys`: delete it after the next release.
    fn move_named_graphs(&mut self, servers: &[(String, String)]) {
        let mut moved = false;
        for record in &mut self.records {
            if let Some(key) = graph_store::named_keys::key_for(&record.graph, servers) {
                record.graph = key;
                moved = true;
            }
        }
        if moved {
            self.save();
        }
    }

    /// The file, when Studio could neither read it nor set it aside: the list then lasts only
    /// until Studio quits, and the Conversations panel says so.
    pub fn not_saved(&self) -> Option<&Path> {
        (!self.saving).then_some(self.path.as_path())
    }

    /// Writes the list on Studio's writer thread, in order with its other writes: a write
    /// waits for the disk to sync it, too long for the main thread at every prompt.
    fn save(&self) {
        if !self.saving {
            return;
        }
        let value = serde_json::json!({ "version": VERSION, "sessions": self.records });
        if let Ok(text) = serde_json::to_string_pretty(&value) {
            studio_settings::write_atomic_in_turn(
                self.path.clone(),
                text.into_bytes(),
                studio_settings::Access::Private,
            );
        }
    }

    /// A graph's conversations that are not archived, most recently active first.
    pub fn for_graph(&self, graph: &str) -> Vec<&SessionRecord> {
        self.sorted(graph, false)
    }

    pub fn archived(&self, graph: &str) -> Vec<&SessionRecord> {
        self.sorted(graph, true)
    }

    fn sorted(&self, graph: &str, archived: bool) -> Vec<&SessionRecord> {
        let mut records: Vec<_> = self
            .records
            .iter()
            .filter(|record| record.graph == graph && record.archived == archived)
            .collect();
        records.sort_by_key(|record| std::cmp::Reverse(record.updated_at));
        records
    }

    pub fn get(&self, session: &str) -> Option<&SessionRecord> {
        self.records.iter().find(|record| record.session == session)
    }

    /// The graph's last active conversation with this agent, to come back to.
    pub fn latest(&self, graph: &str, agent: &str) -> Option<&SessionRecord> {
        self.for_graph(graph)
            .into_iter()
            .find(|record| record.agent == agent)
    }

    /// Gives the conversation recorded as `old` the agent's new session id: the agent could not
    /// load it, and a new conversation took its place on its branch.
    pub fn rebind(&mut self, old: &str, new: String, cx: &mut Context<Self>) {
        if let Some(record) = self.records.iter_mut().find(|record| record.session == old) {
            record.session = new;
            self.live.remove(old);
            self.unread.remove(old);
            self.save();
            cx.notify();
        }
    }

    /// Records a conversation, or updates its branch, keeping its title, times and marks.
    pub fn upsert(&mut self, record: SessionRecord, cx: &mut Context<Self>) {
        match self
            .records
            .iter_mut()
            .find(|existing| existing.session == record.session)
        {
            Some(existing) => {
                existing.branch = record.branch;
                existing.branch_created = record.branch_created;
                existing.archived = false;
            }
            None => self.records.push(record),
        }
        self.save();
        cx.notify();
    }

    fn update(
        &mut self,
        session: &str,
        change: impl FnOnce(&mut SessionRecord),
        cx: &mut Context<Self>,
    ) {
        if let Some(record) = self.records.iter_mut().find(|r| r.session == session) {
            let before = record.clone();
            change(record);
            // A title already set, or a pin already so, writes nothing.
            if *record != before {
                self.save();
                cx.notify();
            }
        }
    }

    /// The agent's title, which replaces any earlier one.
    pub fn set_title(&mut self, session: &str, title: String, cx: &mut Context<Self>) {
        self.update(session, |record| record.title = Some(title), cx);
    }

    /// The start of the first message, until the agent names the conversation.
    pub fn suggest_title(&mut self, session: &str, message: &str, cx: &mut Context<Self>) {
        let Some(title) = provisional_title(message) else {
            return;
        };
        self.update(
            session,
            |record| {
                record.title.get_or_insert(title);
            },
            cx,
        );
    }

    pub fn touch(&mut self, session: &str, cx: &mut Context<Self>) {
        self.update(session, |record| record.updated_at = Utc::now(), cx);
    }

    pub fn set_pinned(&mut self, session: &str, pinned: bool, cx: &mut Context<Self>) {
        self.update(session, |record| record.pinned = pinned, cx);
    }

    /// Archiving only hides it here; the agent keeps the session.
    pub fn set_archived(&mut self, session: &str, archived: bool, cx: &mut Context<Self>) {
        self.update(session, |record| record.archived = archived, cx);
    }

    /// The person's name for it; an empty one gives the agent's back.
    pub fn rename(&mut self, session: &str, title: &str, cx: &mut Context<Self>) {
        let title = title.trim();
        self.update(
            session,
            |record| {
                record.title_override = (!title.is_empty()).then(|| title.to_owned());
            },
            cx,
        );
    }

    /// Forgets a conversation; its open tab closes.
    pub fn remove(&mut self, session: &str, cx: &mut Context<Self>) {
        self.records.retain(|record| record.session != session);
        self.live.remove(session);
        self.unread.remove(session);
        self.deleted.insert(session.to_owned());
        self.save();
        cx.notify();
    }

    pub fn was_deleted(&self, session: &str) -> bool {
        self.deleted.contains(session)
    }

    /// `name`, or `name-2`, `name-3`, … when another of `graph`'s conversations writes to it:
    /// an agent branch is named after the second a conversation began, which two can share.
    pub fn unused_branch(&self, graph: &str, name: String) -> String {
        let taken = |candidate: &str| {
            self.records
                .iter()
                .any(|record| record.graph == graph && record.branch == candidate)
        };
        if !taken(&name) {
            return name;
        }
        (2..)
            .map(|n| format!("{name}-{n}"))
            .find(|candidate| !taken(candidate))
            .expect("a free name")
    }

    /// Adds conversations the agent knows and the list does not; returns how many were new.
    pub fn import(&mut self, records: Vec<SessionRecord>, cx: &mut Context<Self>) -> usize {
        let mut added = 0;
        for mut record in records {
            if self.get(&record.session).is_none() && !self.deleted.contains(&record.session) {
                record.branch = self.unused_branch(&record.graph, record.branch);
                self.records.push(record);
                added += 1;
            }
        }
        if added > 0 {
            self.save();
            cx.notify();
        }
        added
    }

    pub fn live(&self, session: &str) -> Option<Live> {
        self.live.get(session).copied()
    }

    pub fn set_live(&mut self, session: &str, live: Option<Live>, cx: &mut Context<Self>) {
        let changed = match live {
            Some(live) => self.live.insert(session.to_owned(), live) != Some(live),
            None => self.live.remove(session).is_some(),
        };
        if changed {
            cx.notify();
        }
    }

    pub fn is_unread(&self, session: &str) -> bool {
        self.unread.contains(session)
    }

    pub fn mark_unread(&mut self, session: &str, cx: &mut Context<Self>) {
        if self.unread.insert(session.to_owned()) {
            cx.notify();
        }
    }

    pub fn mark_read(&mut self, session: &str, cx: &mut Context<Self>) {
        if self.unread.remove(session) {
            cx.notify();
        }
    }
}

/// Where a conversation runs: where it ran before (`saved`), so a resumed one finds the agent's
/// own record of it whatever key its graph had then; a new one in its graph's directory.
pub fn working_dir(saved: Option<&SessionRecord>, graph: &str) -> PathBuf {
    match saved {
        Some(saved) => saved.cwd.clone(),
        None => graph_cwd(graph),
    }
}

/// The directory an agent works in for a graph: its own, so the agent's list of sessions in it
/// is the graph's. The name is the graph's key made safe, and a stable hash of the key so two
/// keys never share one.
pub fn graph_cwd(graph: &str) -> PathBuf {
    let slug: String = graph
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // FNV-1a: stable across Rust versions, unlike the standard hasher.
    let hash = graph.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    let data = studio_settings::data_dir();
    let local = if cfg!(windows) {
        let folder = |name: &str| std::env::var_os(name).map(PathBuf::from);
        local_counterpart(
            &data,
            folder("APPDATA").as_deref(),
            folder("LOCALAPPDATA").as_deref(),
        )
    } else {
        None
    };
    local
        .unwrap_or(data)
        .join("agent")
        .join("graphs")
        .join(format!("{slug}-{hash:08x}"))
}

/// Where agents work on Windows. Studio's data folder there is the roaming one (`%APPDATA%`),
/// which folder redirection can make a network path (`\\server\share\…`), and cmd cannot work
/// in a network folder: it starts in the Windows folder instead, so every batch file the agent
/// runs from its working directory, its `omnigraph.cmd` and npm's shims alike, would read
/// relative paths from there. The local application folder (`%LOCALAPPDATA%`) is never
/// redirected, and an agent keeps its own sessions on this machine anyway. So `data`'s
/// counterpart in `local`, when `data` is a folder of `roaming`; a data folder set elsewhere
/// (`OMNIGRAPH_STUDIO_DATA`) keeps its agents.
fn local_counterpart(data: &Path, roaming: Option<&Path>, local: Option<&Path>) -> Option<PathBuf> {
    let (roaming, local, name) = (roaming?, local?, data.file_name()?);
    (data.parent() == Some(roaming)).then(|| local.join(name))
}

/// Where each conversation's private CLI home is made: outside every working directory, so
/// an agent cannot change what `omnigraph_run` runs.
pub fn private_homes() -> PathBuf {
    studio_settings::data_dir().join("agent").join("homes")
}

/// The first line of a message, cut to a title's length on a character.
fn provisional_title(message: &str) -> Option<String> {
    const TITLE_CHARS: usize = 80;
    let line = message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    Some(studio_ui::text::cut(line, TITLE_CHARS))
}

/// The form of the file's records. A build that reads a file of another version sets it
/// aside whole: serde passes over fields it does not know, so a record a newer build wrote
/// with one more field would read here and be written back without it. It changes when a build
/// reading the other's records would drop or misread what they hold; a form that only accepts
/// less (every field required, which Studio always wrote) keeps it, since a record of the
/// older form that no longer reads sets its file aside on its own.
const VERSION: u64 = 2;

/// What Studio read of one of its state files, and whether it may write the file again.
struct Stored<T> {
    value: T,
    /// False when the file held what Studio could not read and could not be set aside: a save
    /// would replace it, so what changes is kept in memory for this run instead.
    saving: bool,
}

/// The text of one of Studio's state files: `Ok(None)` when there is none, `Err` when it is
/// there but cannot be read (not UTF-8, or held by another program), which is as unusable as a
/// file that does not parse.
fn read_text(path: &Path) -> Result<Option<String>, ()> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

/// Moves a state file Studio could not use out of the way of the next save, renamed beside
/// itself: whether it may be saved over now. Not when the rename fails (on Windows, another
/// program holds it open without letting it be renamed): the file then stays as it is.
fn set_aside(path: &Path) -> bool {
    studio_settings::set_aside(path).is_some()
}

/// The records on disk, each read on its own: one Studio cannot read (damaged, or written by an
/// older build without a field this one requires) leaves the others. A file with any it cannot
/// read, of another [`VERSION`], or that cannot be read at all, is set aside first, renamed
/// beside itself, so the next save, which writes only the ones read, loses nothing. One that
/// cannot be set aside is never saved over.
fn read_records(path: &Path) -> Stored<Vec<SessionRecord>> {
    let unusable = || Stored {
        value: Vec::new(),
        saving: set_aside(path),
    };
    let text = match read_text(path) {
        Ok(Some(text)) => text,
        Ok(None) => {
            return Stored {
                value: Vec::new(),
                saving: true,
            };
        }
        Err(()) => return unusable(),
    };
    let records = serde_json::from_str::<Value>(&text)
        .ok()
        .filter(|value| value.get("version").and_then(Value::as_u64) == Some(VERSION))
        .and_then(|value| match value.get("sessions") {
            Some(Value::Array(records)) => Some(records.clone()),
            _ => None,
        });
    let Some(records) = records else {
        return unusable();
    };
    let read: Vec<SessionRecord> = records
        .iter()
        .filter_map(|record| serde_json::from_value(record.clone()).ok())
        .collect();
    let saving = read.len() == records.len() || set_aside(path);
    Stored {
        value: read,
        saving,
    }
}

/// The preferences file's object. A file that exists but cannot be read or is not a JSON
/// object is set aside, so the next write does not replace every agent's choices it holds; one
/// that cannot be set aside is never written over.
fn read(path: &Path) -> Stored<Map<String, Value>> {
    let object = match read_text(path) {
        Ok(None) => Some(Map::new()),
        Ok(Some(text)) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(object)) => Some(object),
            _ => None,
        },
        Err(()) => None,
    };
    match object {
        Some(value) => Stored {
            value,
            saving: true,
        },
        None => Stored {
            value: Map::new(),
            saving: set_aside(path),
        },
    }
}

/// The option values the person last chose for an agent (its model, mode, reasoning level),
/// applied to its next new session: `{agent command: {option id: value id}}`.
fn preferences_path() -> PathBuf {
    studio_settings::data_dir()
        .join("agent")
        .join("preferences.json")
}

/// The preferences, read from their file once and then kept; each change is written in turn,
/// off the main thread, unless the file could not be read and stays where it was.
struct Preferences {
    path: PathBuf,
    records: Option<Stored<Map<String, Value>>>,
}

impl Preferences {
    fn records(&mut self) -> &mut Stored<Map<String, Value>> {
        let path = &self.path;
        self.records.get_or_insert_with(|| read(path))
    }

    fn of(&mut self, command: &[String]) -> Map<String, Value> {
        self.records()
            .value
            .get(&command.join(" "))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    fn prefer(&mut self, command: &[String], option: &str, value: Value) {
        let Stored {
            value: records,
            saving,
        } = self.records();
        let agent = records
            .entry(command.join(" "))
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(agent) = agent.as_object_mut() {
            agent.insert(option.to_owned(), value);
        }
        if !*saving {
            return;
        }
        if let Ok(text) = serde_json::to_vec_pretty(records) {
            studio_settings::write_atomic_in_turn(
                self.path.clone(),
                text,
                studio_settings::Access::Private,
            );
        }
    }
}

fn with_preferences<T>(f: impl FnOnce(&mut Preferences) -> T) -> T {
    static PREFERENCES: std::sync::Mutex<Option<Preferences>> = std::sync::Mutex::new(None);
    let mut preferences = PREFERENCES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(preferences.get_or_insert_with(|| Preferences {
        path: preferences_path(),
        records: None,
    }))
}

pub fn preferences(command: &[String]) -> Map<String, Value> {
    with_preferences(|preferences| preferences.of(command))
}

pub fn prefer(command: &[String], option: &str, value: Value) {
    with_preferences(|preferences| preferences.prefer(command, option, value));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(session: &str, graph: &str, agent: &str, minutes_ago: i64) -> SessionRecord {
        let at = Utc::now() - chrono::Duration::minutes(minutes_ago);
        SessionRecord {
            session: session.into(),
            graph: graph.into(),
            agent: agent.into(),
            title: None,
            title_override: None,
            created_at: at,
            updated_at: at,
            branch: format!("agent/{session}"),
            branch_created: false,
            pinned: false,
            archived: false,
            cwd: graph_cwd(graph),
        }
    }

    #[test]
    fn each_graph_has_a_directory_of_its_own() {
        let a = graph_cwd("server/crm");
        assert_eq!(a, graph_cwd("server/crm"));
        assert_ne!(a, graph_cwd("server-crm"));
        assert!(a.to_string_lossy().contains("server-crm-"));
    }

    /// On Windows agents work in the local application folder, never the roaming one, which
    /// folder redirection can put on a network share where cmd cannot work. A data folder set
    /// elsewhere keeps its agents.
    #[test]
    fn agents_work_in_the_local_folder_of_studios_roaming_data() {
        let profile = std::env::temp_dir().join("profile");
        let (roaming, local) = (
            profile.join("AppData").join("Roaming"),
            profile.join("AppData").join("Local"),
        );
        let redirected = PathBuf::from(r"\\server\share\ada\AppData\Roaming");
        for roaming in [roaming, redirected] {
            for name in ["OmniGraph Studio", "OmniGraph Studio Dev"] {
                assert_eq!(
                    local_counterpart(
                        &roaming.join(name),
                        Some(roaming.as_path()),
                        Some(local.as_path())
                    ),
                    Some(local.join(name))
                );
            }
            assert_eq!(
                local_counterpart(
                    &profile.join("elsewhere"),
                    Some(roaming.as_path()),
                    Some(local.as_path())
                ),
                None
            );
            assert_eq!(
                local_counterpart(
                    &roaming.join("OmniGraph Studio"),
                    Some(roaming.as_path()),
                    None
                ),
                None
            );
        }
    }

    #[gpui::test]
    fn conversations_are_kept_per_graph_newest_first(cx: &mut gpui::TestAppContext) {
        let dir =
            std::env::temp_dir().join(format!("studio-agent-sessions-{}", std::process::id()));
        let path = dir.join("sessions.json");
        let store = cx.new(|_| AgentSessions::load(path.clone()));
        store.update(cx, |store, cx| {
            store.upsert(record("old", "server/crm", "claude", 60), cx);
            store.upsert(record("new", "server/crm", "claude", 1), cx);
            store.upsert(record("codex", "server/crm", "codex", 5), cx);
            store.upsert(record("notes", "server/notes", "claude", 0), cx);
            store.suggest_title("old", "\n  How many people?\nmore", cx);
            store.suggest_title("old", "a later message", cx);
            store.set_title("new", "Count people".into(), cx);
            store.set_archived("codex", true, cx);
            store.rename("new", "  People count ", cx);
        });
        store.read_with(cx, |store, _| {
            let ids: Vec<_> = store
                .for_graph("server/crm")
                .iter()
                .map(|r| r.session.as_str())
                .collect();
            assert_eq!(ids, ["new", "old"]);
            assert_eq!(store.archived("server/crm").len(), 1);
            assert_eq!(store.get("old").unwrap().shown_title(), "How many people?");
            assert_eq!(store.get("new").unwrap().shown_title(), "People count");
            assert_eq!(store.latest("server/crm", "claude").unwrap().session, "new");
            assert!(store.latest("server/crm", "codex").is_none());
        });
        // An empty name gives the agent's back; a removed one stays out of an import.
        store.update(cx, |store, cx| {
            store.rename("new", " ", cx);
            store.remove("old", cx);
            let again = vec![
                record("old", "server/crm", "claude", 0),
                record("fresh", "server/crm", "claude", 0),
            ];
            assert_eq!(store.import(again, cx), 1);
        });
        store.read_with(cx, |store, _| {
            assert_eq!(store.get("new").unwrap().shown_title(), "Count people");
            assert!(store.get("old").is_none() && store.was_deleted("old"));
            assert!(store.get("fresh").is_some());
        });
        // What was written comes back, owner-only, once the writer thread has written it.
        studio_settings::writes_done();
        let reloaded = AgentSessions::load(path.clone());
        assert_eq!(reloaded.records.len(), 4);
        assert!(reloaded.get("old").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // Two conversations that began in the same second write to branches of their own.
        let second = "agent/20261018-101500";
        store.update(cx, |store, cx| {
            let same = |session: &str| SessionRecord {
                branch: second.into(),
                ..record(session, "server/crm", "claude", 0)
            };
            store.import(vec![same("same-a"), same("same-b")], cx);
            assert_eq!(store.get("same-a").unwrap().branch, second);
            assert_eq!(store.get("same-b").unwrap().branch, format!("{second}-2"));
            assert_eq!(
                store.unused_branch("server/crm", second.into()),
                format!("{second}-3")
            );
            assert_eq!(store.unused_branch("server/notes", second.into()), second);
        });
        studio_settings::writes_done();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Conversations a build before filed under a graph's server name are filed under the
    /// graph's key once the index loads, each keeping its working directory, so resuming one
    /// finds the agent's own record of it; a new conversation works in the key's directory. A
    /// name no server has stays as it is, and so does one two servers share, either of which
    /// could have filed it.
    #[gpui::test]
    fn conversations_filed_by_server_name_move_to_the_graphs_key(cx: &mut gpui::TestAppContext) {
        let dir = std::env::temp_dir().join(format!("studio-agent-named-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("sessions.json");
        // As a build before wrote them.
        let store = cx.new(|_| AgentSessions::load(path.clone()));
        store.update(cx, |store, cx| {
            store.upsert(record("old", "dev/crm", "claude", 5), cx);
            store.upsert(record("gone", "gone/crm", "claude", 1), cx);
            store.upsert(record("shared", "local/crm", "claude", 2), cx);
        });
        studio_settings::writes_done();

        let servers = vec![
            ("dev".to_owned(), "http://127.0.0.1:8787".to_owned()),
            ("local".to_owned(), "http://127.0.0.1:8790".to_owned()),
            ("local".to_owned(), "http://127.0.0.1:9000".to_owned()),
        ];
        let key = "http://127.0.0.1:8787/crm";
        let mut loaded = AgentSessions::load(path.clone());
        loaded.move_named_graphs(&servers);
        let old = loaded.get("old").unwrap();
        assert_eq!(old.graph, key);
        assert_eq!(old.cwd, graph_cwd("dev/crm"), "where the agent has it");
        assert_eq!(
            working_dir(Some(old), key),
            graph_cwd("dev/crm"),
            "resumed where it ran"
        );
        assert_eq!(
            working_dir(None, key),
            graph_cwd(key),
            "a new one: the key's"
        );
        assert_ne!(graph_cwd(key), graph_cwd("dev/crm"));
        assert_eq!(loaded.latest(key, "claude").unwrap().session, "old");
        assert_eq!(
            loaded.get("gone").unwrap().graph,
            "gone/crm",
            "no server has the name"
        );
        assert_eq!(
            loaded.get("shared").unwrap().graph,
            "local/crm",
            "two servers have the name"
        );

        // Saved under the key, so the next load finds it there.
        studio_settings::writes_done();
        let again = AgentSessions::load(path);
        assert_eq!(again.get("old").unwrap().graph, key);
        assert_eq!(again.get("old").unwrap().cwd, graph_cwd("dev/crm"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A conversation the agent could not load, replaced by a new one on its branch, is one
    /// record under the new session, not two on one branch.
    #[gpui::test]
    fn a_conversation_the_agent_could_not_load_follows_the_new_one(cx: &mut gpui::TestAppContext) {
        let dir = std::env::temp_dir().join(format!("studio-agent-rebind-{}", std::process::id()));
        let store = cx.new(|_| AgentSessions::load(dir.join("sessions.json")));
        store.update(cx, |store, cx| {
            store.upsert(record("old", "server/crm", "claude", 5), cx);
            store.rebind("old", "new".into(), cx);
            // As the panel records it: the new session, on the branch it resumed.
            store.upsert(
                SessionRecord {
                    branch: "agent/old".into(),
                    ..record("new", "server/crm", "claude", 0)
                },
                cx,
            );
        });
        store.read_with(cx, |store, _| {
            assert!(store.get("old").is_none());
            let records = store.for_graph("server/crm");
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].branch, "agent/old", "the branch it had");
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    /// One record Studio cannot read leaves the others, and the file is set aside before the
    /// next save writes only those. A record an older build wrote without a field this one
    /// requires is such a record, and a file of another version is set aside whole: in both,
    /// the file is renamed beside itself, not lost.
    #[test]
    fn a_record_that_cannot_be_read_leaves_the_others() {
        let dir = std::env::temp_dir().join(format!("studio-agent-records-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        let now = Utc::now();
        let record = SessionRecord {
            session: "s-1".into(),
            graph: "server/crm".into(),
            agent: "claude-code-acp".into(),
            title: None,
            title_override: None,
            created_at: now,
            updated_at: now,
            branch: "agent/x".into(),
            branch_created: true,
            pinned: false,
            archived: false,
            cwd: graph_cwd("server/crm"),
        };
        // As an older build wrote it: no working directory, and no marks.
        let mut older = serde_json::to_value(&record).unwrap();
        let older_fields = older.as_object_mut().unwrap();
        older_fields.insert("session".into(), json!("s-2"));
        older_fields.insert("cwd".into(), Value::Null);
        let mut oldest = older.clone();
        let oldest_fields = oldest.as_object_mut().unwrap();
        oldest_fields.insert("session".into(), json!("s-3"));
        for field in ["cwd", "pinned", "archived", "branch_created"] {
            oldest_fields.remove(field);
        }
        let file = json!({
            "version": VERSION,
            "sessions": [record, { "session": 3 }, older, oldest],
        });
        std::fs::write(&path, file.to_string()).unwrap();
        let Stored {
            value: records,
            saving,
        } = read_records(&path);
        assert!(saving, "set aside, so saved again");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session, "s-1");
        assert!(!path.exists(), "set aside before it is written again");
        let aside = || -> Vec<std::path::PathBuf> {
            std::fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect()
        };
        assert_eq!(aside().len(), 1);
        assert_eq!(
            std::fs::read_to_string(&aside()[0]).unwrap(),
            file.to_string(),
            "the file set aside is the file as it was"
        );

        // Every record readable, but the file of another version, or of none: nothing is read,
        // and the file is set aside.
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        for version in [json!(VERSION + 1), json!(1), Value::Null] {
            let file = json!({ "version": version, "sessions": [record] });
            std::fs::write(&path, file.to_string()).unwrap();
            assert!(read_records(&path).value.is_empty(), "version {version}");
            assert!(!path.exists(), "version {version}");
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Keeps `path` from being set aside, as another program holding it open without letting
    /// it be renamed does on Windows: a folder stands where each name it would take for the
    /// next while (`<name>.unreadable-<seconds>`), and a file cannot be renamed over a folder.
    fn block_set_aside(path: &Path) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        for seconds in now - 2..now + 60 {
            let blocker = path.with_file_name(format!("{name}.unreadable-{seconds}"));
            std::fs::create_dir_all(blocker.join("taken")).unwrap();
        }
    }

    /// A file Studio could not read is never written over. One that does not read as text is
    /// set aside as one that does not parse is, and the next save writes beside it. One that
    /// cannot be set aside stays as it was: the list is kept in memory and not saved, and says
    /// so.
    #[gpui::test]
    fn a_file_that_could_not_be_read_is_never_written_over(cx: &mut gpui::TestAppContext) {
        let dir = std::env::temp_dir().join(format!("studio-agent-unread-{}", std::process::id()));
        let fresh = || {
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
        };
        let path = dir.join("sessions.json");
        let not_text: &[u8] = b"{\"version\": 2, \"sessions\": [\xff\xfe]}";
        let others = || -> Vec<PathBuf> {
            std::fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|other| *other != path)
                .collect()
        };

        fresh();
        std::fs::write(&path, not_text).unwrap();
        let store = cx.new(|_| AgentSessions::load(path.clone()));
        assert!(!path.exists(), "set aside before it is written again");
        store.update(cx, |store, cx| {
            assert_eq!(store.not_saved(), None);
            store.upsert(record("a", "server/crm", "claude", 0), cx);
        });
        studio_settings::writes_done();
        assert_eq!(std::fs::read(&others()[0]).unwrap(), not_text);
        assert_eq!(AgentSessions::load(path.clone()).records.len(), 1);

        // Neither one that does not read as text nor one that does not parse is written over
        // when it cannot be set aside.
        let other_version = serde_json::to_vec(&json!({ "version": VERSION + 1, "sessions": [] }));
        for held in [not_text.to_vec(), other_version.unwrap()] {
            fresh();
            std::fs::write(&path, &held).unwrap();
            block_set_aside(&path);
            let store = cx.new(|_| AgentSessions::load(path.clone()));
            store.update(cx, |store, cx| {
                store.upsert(record("a", "server/crm", "claude", 0), cx);
                store.set_pinned("a", true, cx);
            });
            studio_settings::writes_done();
            assert_eq!(std::fs::read(&path).unwrap(), held, "written over");
            store.read_with(cx, |store, _| {
                assert!(
                    store.get("a").is_some_and(|record| record.pinned),
                    "kept in memory"
                );
                assert_eq!(store.not_saved(), Some(path.as_path()));
            });
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The person's option choices are kept the same way: a preferences file that could not be
    /// read and cannot be set aside is not written over.
    #[test]
    fn a_preferences_file_that_could_not_be_read_is_never_written_over() {
        let dir = std::env::temp_dir().join(format!("studio-agent-prefs-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("preferences.json");
        let claude = vec!["claude-code-acp".to_owned()];
        for held in [&b"{\"a\": \xff}"[..], b"[1, 2]"] {
            std::fs::write(&path, held).unwrap();
            block_set_aside(&path);
            let mut preferences = Preferences {
                path: path.clone(),
                records: None,
            };
            preferences.prefer(&claude, "model", json!("opus"));
            studio_settings::writes_done();
            assert_eq!(std::fs::read(&path).unwrap(), held, "written over");
            assert_eq!(preferences.of(&claude).get("model"), Some(&json!("opus")));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// On Windows another program can hold the file open without letting it be renamed, or
    /// read at all. Studio can then neither set it aside nor read it, and does not save over
    /// it, even once the other program lets it go.
    #[cfg(windows)]
    #[gpui::test]
    fn a_file_another_program_holds_is_never_written_over(cx: &mut gpui::TestAppContext) {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        let dir = std::env::temp_dir().join(format!("studio-agent-held-{}", std::process::id()));
        let path = dir.join("sessions.json");
        let held = serde_json::to_vec(&json!({ "version": VERSION + 1, "sessions": [] })).unwrap();
        // Readable but not to be renamed, then not even readable.
        for share in [FILE_SHARE_READ, 0] {
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, &held).unwrap();
            let holder = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(share)
                .open(&path)
                .unwrap();
            let store = cx.new(|_| AgentSessions::load(path.clone()));
            drop(holder);
            store.update(cx, |store, cx| {
                store.upsert(record("a", "server/crm", "claude", 0), cx);
            });
            studio_settings::writes_done();
            assert_eq!(std::fs::read(&path).unwrap(), held, "share {share}");
            store.read_with(cx, |store, _| {
                assert_eq!(store.not_saved(), Some(path.as_path()));
            });
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_long_first_line_is_cut_on_a_character() {
        let long = "word ".repeat(40);
        let title = provisional_title(&long).unwrap();
        assert!(title.chars().count() <= 80 && title.ends_with('…'));
        assert_eq!(provisional_title("  \n "), None);
    }

    #[test]
    fn option_choices_are_kept_per_agent_and_the_latest_wins() {
        let dir =
            std::env::temp_dir().join(format!("studio-agent-preferences-{}", std::process::id()));
        let path = dir.join("preferences.json");
        let claude = vec!["claude-code-acp".to_owned()];
        let other = vec!["other-agent".to_owned()];
        let kept = |path: &Path| Preferences {
            path: path.to_owned(),
            records: None,
        };
        let mut preferences = kept(&path);
        preferences.prefer(&claude, "model", json!("sonnet"));
        preferences.prefer(&claude, "model", json!("opus"));
        preferences.prefer(&claude, "fast", json!(true));
        studio_settings::writes_done();
        // As kept, and as the next start reads them.
        for mut preferences in [preferences, kept(&path)] {
            let chosen = preferences.of(&claude);
            assert_eq!(chosen.get("model"), Some(&json!("opus")));
            assert_eq!(chosen.get("fast"), Some(&json!(true)));
            // Another agent's options are its own.
            assert!(preferences.of(&other).is_empty());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
