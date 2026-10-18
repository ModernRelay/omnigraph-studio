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
    /// The graph, as its settings key (`server/graph`).
    pub graph: String,
    /// The agent command, joined with spaces: a session belongs to the agent that made it.
    pub agent: String,
    /// The agent's title for it, else the start of the first message.
    #[serde(default)]
    pub title: Option<String>,
    /// The person's own name for it, which wins over the agent's.
    #[serde(default)]
    pub title_override: Option<String>,
    pub created_at: DateTime<Utc>,
    /// The last message the person sent. The list sorts by it, as Zed's sorts by the last
    /// interaction, so a conversation finishing in the background does not move its row.
    pub updated_at: DateTime<Utc>,
    /// The agent's branch, and whether it was created yet.
    pub branch: String,
    #[serde(default)]
    pub branch_created: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    /// The directory the agent worked in for it; none for conversations from before each graph
    /// had its own, which ran in [`legacy_cwd`].
    #[serde(default)]
    pub cwd: Option<PathBuf>,
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
        let sessions = cx.new(|_| AgentSessions::load(path));
        cx.set_global(GlobalSessions(sessions.clone()));
        sessions
    }

    fn load(path: PathBuf) -> Self {
        let records = read_records(&path);
        Self {
            path,
            records,
            live: HashMap::new(),
            unread: HashSet::new(),
            deleted: HashSet::new(),
        }
    }

    /// Writes the list on Studio's writer thread, in order with its other writes: a write
    /// waits for the disk to sync it, too long for the main thread at every prompt.
    fn save(&self) {
        let value = serde_json::json!({ "version": 2, "sessions": self.records });
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
                if existing.cwd.is_none() {
                    existing.cwd = record.cwd;
                }
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

    /// Adds conversations the agent knows and the list does not; returns how many were new.
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
    studio_settings::data_dir()
        .join("agent")
        .join("graphs")
        .join(format!("{slug}-{hash:08x}"))
}

/// Where each conversation's private CLI home is made: outside every working directory, so
/// an agent cannot change what `omnigraph_run` runs.
pub fn private_homes() -> PathBuf {
    studio_settings::data_dir().join("agent").join("homes")
}

/// Where conversations ran before each graph had a directory of its own.
pub fn legacy_cwd() -> PathBuf {
    studio_settings::data_dir().join("agent").join("workspace")
}

/// The first line of a message, cut to a title's length on a character.
fn provisional_title(message: &str) -> Option<String> {
    const TITLE_CHARS: usize = 80;
    let line = message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    Some(if line.chars().count() > TITLE_CHARS {
        let kept: String = line.chars().take(TITLE_CHARS - 1).collect();
        format!("{}…", kept.trim_end())
    } else {
        line.to_owned()
    })
}

/// The records on disk, each read on its own: one Studio cannot read (damaged, or written by a
/// newer Studio) leaves the others. A file with any it cannot read is set aside first, so the
/// next save, which writes only the ones read, loses nothing.
fn read_records(path: &Path) -> Vec<SessionRecord> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let records =
        serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| match value.get("sessions") {
                Some(Value::Array(records)) => Some(records.clone()),
                _ => None,
            });
    let Some(records) = records else {
        studio_settings::set_aside(path);
        return Vec::new();
    };
    let read: Vec<SessionRecord> = records
        .iter()
        .filter_map(|record| serde_json::from_value(record.clone()).ok())
        .collect();
    if read.len() < records.len() {
        studio_settings::set_aside(path);
    }
    read
}

/// The file's object. A file that exists but is not a JSON object is set aside, so the next
/// write does not replace every conversation it lists.
fn read(path: &Path) -> Map<String, Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(object)) => object,
        _ => {
            studio_settings::set_aside(path);
            Map::new()
        }
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
/// off the main thread.
struct Preferences {
    path: PathBuf,
    records: Option<Map<String, Value>>,
}

impl Preferences {
    fn records(&mut self) -> &mut Map<String, Value> {
        let path = &self.path;
        self.records.get_or_insert_with(|| read(path))
    }

    fn of(&mut self, command: &[String]) -> Map<String, Value> {
        self.records()
            .get(&command.join(" "))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    fn prefer(&mut self, command: &[String], option: &str, value: Value) {
        let records = self.records();
        let agent = records
            .entry(command.join(" "))
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(agent) = agent.as_object_mut() {
            agent.insert(option.to_owned(), value);
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
            cwd: None,
        }
    }

    #[test]
    fn each_graph_has_a_directory_of_its_own() {
        let a = graph_cwd("server/crm");
        assert_eq!(a, graph_cwd("server/crm"));
        assert_ne!(a, graph_cwd("server-crm"));
        assert!(a.to_string_lossy().contains("server-crm-"));
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

    /// One record Studio cannot read leaves the others, and the file is set aside before the
    /// next save writes only those.
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
            cwd: None,
        };
        let file = json!({ "version": 2, "sessions": [record, { "session": 3 }] });
        std::fs::write(&path, file.to_string()).unwrap();
        let records = read_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session, "s-1");
        assert!(!path.exists(), "set aside before it is written again");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
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
