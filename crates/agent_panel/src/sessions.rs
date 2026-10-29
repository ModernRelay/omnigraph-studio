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
use studio_workspace::{ToastKind, Workspace};

/// One conversation with an agent on a graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// The agent's session id.
    pub session: String,
    /// The graph, as its key (`<target>@<address>/<graph id>`, `GraphSession::overrides_key`).
    pub graph: String,
    /// The agent command, joined with spaces: a session belongs to the agent that made it.
    pub agent: String,
    /// The agent's title for it, else the start of the first message; none until one of them
    /// names it.
    pub title: Option<String>,
    /// The person's own name for it, which wins over the agent's.
    pub title_override: Option<String>,
    pub created_at: DateTime<Utc>,
    /// When the person last wrote in it, or it last opened ready (or was imported, at the
    /// agent's last activity). The list sorts by it, as Zed's sorts by the last interaction, so
    /// a conversation finishing in the background does not move its row.
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
    /// What the person is told, once, about a file Studio could not read and set aside: which
    /// file, why, and where it is kept ([`Self::tell_pending`]). One that could not be set
    /// aside is said in the Conversations panel instead ([`Self::not_saved`]).
    notice: Option<String>,
    live: HashMap<String, Live>,
    /// Conversations whose turn ended while their tab was not the one shown.
    unread: HashSet<String>,
    /// Conversations deleted in this run: an open tab of one closes.
    deleted: HashSet<String>,
    /// Agent branches given to conversations begun in this run, by graph, before their records
    /// are written at Ready ([`Self::claim_branch`]).
    claimed: HashSet<(String, String)>,
    /// The ids conversations were rebound from in this run, oldest first, each with the id it
    /// moved to ([`Self::rebind`]), [`REBOUND_KEPT`] at most: a rename or a delete begun on a
    /// conversation's old id finishes on its new one ([`Self::current`]).
    rebound: Vec<(String, String)>,
}

/// How many rebinds [`AgentSessions`] keeps: one happens when an agent could not load a
/// conversation it was asked for, so a run sees few.
const REBOUND_KEPT: usize = 64;

struct GlobalSessions(Entity<AgentSessions>);

impl Global for GlobalSessions {}

impl AgentSessions {
    /// The one index of this run of Studio.
    pub fn global(cx: &mut App) -> Entity<AgentSessions> {
        let sessions = match cx.try_global::<GlobalSessions>() {
            Some(global) => global.0.clone(),
            None => {
                let path = studio_settings::data_dir()
                    .join("agent")
                    .join("sessions.json");
                let sessions = cx.new(|_| AgentSessions::load(path));
                cx.set_global(GlobalSessions(sessions.clone()));
                sessions
            }
        };
        Self::tell_pending(&sessions, cx);
        sessions
    }

    fn load(path: PathBuf) -> Self {
        let (Stored { value, saving }, notice) = read_records(&path);
        Self {
            path,
            records: value,
            saving,
            notice,
            live: HashMap::new(),
            unread: HashSet::new(),
            deleted: HashSet::new(),
            claimed: HashSet::new(),
            rebound: Vec::new(),
        }
    }

    /// Shows the notice about the file set aside ([`Self::notice`]) in a toast that stays until
    /// dismissed, in the active window, else any: once, at the first look at the index with a
    /// window open. Deferred, since the index is first looked at while a window is being made.
    fn tell_pending(sessions: &Entity<AgentSessions>, cx: &mut App) {
        if sessions.read(cx).notice.is_none() {
            return;
        }
        let sessions = sessions.clone();
        cx.defer(move |cx| {
            let workspace = |window: gpui::AnyWindowHandle| window.downcast::<Workspace>();
            let Some(window) = cx
                .active_window()
                .and_then(workspace)
                .or_else(|| cx.windows().into_iter().find_map(workspace))
            else {
                return;
            };
            let Some(notice) = sessions.update(cx, |sessions, _| sessions.notice.take()) else {
                return;
            };
            window
                .update(cx, |workspace, _, cx| {
                    workspace.show_toast(ToastKind::Error, notice, cx)
                })
                .ok();
        });
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
            record.session = new.clone();
            self.live.remove(old);
            self.unread.remove(old);
            if self.rebound.len() == REBOUND_KEPT {
                self.rebound.remove(0);
            }
            self.rebound.push((old.to_owned(), new));
            self.save();
            cx.notify();
        }
    }

    /// The id conversation `id` has now: the one it was rebound to, if it was, through each
    /// rebind since.
    pub fn current(&self, id: &str) -> String {
        let mut id = id.to_owned();
        // Each rebind is looked through once, so a cycle cannot hold it.
        for (old, new) in &self.rebound {
            if *old == id {
                id = new.clone();
            }
        }
        id
    }

    /// Records a conversation, or updates its branch, keeping its title, times and pin; one
    /// opened again leaves the archive.
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

    /// Whether the conversation's agent branch, `branch`, exists, as a write that ended after
    /// its tab closed found: the record learns it only while it still writes to that branch.
    pub fn set_branch_created(
        &mut self,
        session: &str,
        branch: &str,
        created: bool,
        cx: &mut Context<Self>,
    ) {
        self.update(
            session,
            |record| {
                if record.branch == branch {
                    record.branch_created = created;
                }
            },
            cx,
        );
    }

    /// The agent's title, which replaces any earlier one: one line, cut ([`title_of`]), so it
    /// fits a row and can be typed into the delete confirmation. One without text is not taken.
    pub fn set_title(&mut self, session: &str, title: String, cx: &mut Context<Self>) {
        let Some(title) = title_of(&title) else {
            return;
        };
        self.update(session, |record| record.title = Some(title), cx);
    }

    /// The start of the first message, until the agent names the conversation.
    pub fn suggest_title(&mut self, session: &str, message: &str, cx: &mut Context<Self>) {
        let Some(title) = title_of(message) else {
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

    /// `name`, or `name-2`, `name-3`, … when another of `graph`'s conversations writes to it,
    /// recorded or begun in this run, and now this one's: an agent branch is named after the
    /// second a conversation began, which two can share.
    pub fn claim_branch(&mut self, graph: &str, name: String) -> String {
        let taken = |candidate: &str| {
            self.claimed
                .contains(&(graph.to_owned(), candidate.to_owned()))
                || self
                    .records
                    .iter()
                    .any(|record| record.graph == graph && record.branch == candidate)
        };
        let name = if taken(&name) {
            (2..)
                .map(|n| format!("{name}-{n}"))
                .find(|candidate| !taken(candidate))
                .expect("a free name")
        } else {
            name
        };
        self.claimed.insert((graph.to_owned(), name.clone()));
        name
    }

    /// Adds conversations the agent knows and the list does not; returns how many were new.
    pub fn import(&mut self, records: Vec<SessionRecord>, cx: &mut Context<Self>) -> usize {
        let mut added = 0;
        for mut record in records {
            if self.get(&record.session).is_none() && !self.deleted.contains(&record.session) {
                record.branch = self.claim_branch(&record.graph, record.branch);
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
/// own record of it whatever key its graph had then; a new one in a folder of its own in its
/// graph's directory, named for its `branch`, which holds its CLI and `AGENTS.md`
/// ([`crate::cli_home`]).
pub fn working_dir(saved: Option<&SessionRecord>, graph: &str, branch: &str) -> PathBuf {
    match saved {
        Some(saved) => saved.cwd.clone(),
        None => {
            // A tab that starts again on the same branch gets another folder.
            let random = crate::confined::random_hex(4).unwrap_or_default();
            graph_cwd(graph).join(format!("{}-{random}", branch.replace('/', "-")))
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
    agent_dir()
        .join("graphs")
        .join(format!("{slug}-{hash:08x}"))
}

/// The folder of what agents keep on this machine, their working directories with their
/// conversations' CLI homes: `agent` in Studio's data folder, or on Windows in its
/// local counterpart ([`local_counterpart`]).
fn agent_dir() -> PathBuf {
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
    local.unwrap_or(data).join("agent")
}

/// Where agents work, and their conversations' CLI homes are kept, on Windows. Studio's data
/// folder there is the roaming one (`%APPDATA%`), which folder redirection can make a network
/// path (`\\server\share\…`) and a roaming profile copies to a server. cmd cannot work in a
/// network folder: it starts in the Windows folder instead, so every batch file the agent runs
/// from its working directory, its `omnigraph.cmd` and npm's shims alike, would read relative
/// paths from there. And each CLI home holds a gateway's session token and certificate, which
/// would travel to the share and stay there. The local application folder (`%LOCALAPPDATA%`)
/// is never redirected or roamed, and an agent keeps its own sessions on this machine anyway.
/// So `data`'s counterpart in `local`, when `data` is a folder of `roaming`; a data folder set
/// elsewhere (`OMNIGRAPH_STUDIO_DATA`) keeps its agents.
fn local_counterpart(data: &Path, roaming: Option<&Path>, local: Option<&Path>) -> Option<PathBuf> {
    let (roaming, local, name) = (roaming?, local?, data.file_name()?);
    (data.parent() == Some(roaming)).then(|| local.join(name))
}

/// A title as Studio keeps one: the first line with text, cut to a title's length on a
/// character; `None` for text without any. The start of the first message is one until the
/// agent names the conversation, and the agent's titles and imported ones are taken so too.
pub(crate) fn title_of(message: &str) -> Option<String> {
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
/// less (every field required, which Studio always wrote) keeps it, since a file holding a
/// record of the older form no longer parses, and is set aside on its own.
const VERSION: u64 = 2;

/// The sessions file as Studio writes it ([`AgentSessions::save`]).
#[derive(Deserialize)]
struct SessionsFile {
    version: u64,
    sessions: Vec<SessionRecord>,
}

/// What Studio read of one of its state files, and whether it may write the file again.
struct Stored<T> {
    value: T,
    /// False when the file held what Studio could not read and could not be set aside: a save
    /// would replace it, so what changes is kept in memory for this run instead.
    saving: bool,
}

/// The text of one of Studio's state files: `Ok(None)` when there is none, `Err` with why when
/// it is there but cannot be read (not UTF-8, or held by another program), which is as unusable
/// as a file that does not parse.
fn read_text(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// The records on disk, and what the person is told when the file was set aside. A file that
/// does not parse as this build's form ([`SessionsFile`]), one of another [`VERSION`], or one
/// that cannot be read at all, is set aside whole, renamed beside itself, as Studio's other
/// state files are, so the next save, which writes only what this run holds, loses nothing; one
/// record of an older form is enough, since no record is read on its own. The notice names the
/// file, why it was not read, and where it is kept. One that cannot be set aside is never saved
/// over, and the Conversations panel says so ([`AgentSessions::not_saved`]).
fn read_records(path: &Path) -> (Stored<Vec<SessionRecord>>, Option<String>) {
    let unread = match read_text(path) {
        Ok(Some(text)) => match serde_json::from_str::<SessionsFile>(&text) {
            Ok(file) if file.version == VERSION => {
                let stored = Stored {
                    value: file.sessions,
                    saving: true,
                };
                return (stored, None);
            }
            Ok(file) => format!(
                "it is of another build of Studio, version {} where this one reads {VERSION}",
                file.version
            ),
            Err(error) => error.to_string(),
        },
        Ok(None) => {
            let stored = Stored {
                value: Vec::new(),
                saving: true,
            };
            return (stored, None);
        }
        Err(error) => error,
    };
    let Some(aside) = studio_settings::set_aside(path) else {
        let stored = Stored {
            value: Vec::new(),
            saving: false,
        };
        return (stored, None);
    };
    let notice = format!(
        "The agent conversations list {} could not be read ({unread}), so it starts empty; the \
         file is kept as {}.",
        path.display(),
        aside.display()
    );
    let stored = Stored {
        value: Vec::new(),
        saving: true,
    };
    (stored, Some(notice))
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
        Err(_) => None,
    };
    match object {
        Some(value) => Stored {
            value,
            saving: true,
        },
        // A file Studio could not use is moved out of the way of the next save, renamed beside
        // itself; when that fails (on Windows, another program holds it open), it stays as it
        // is and is not saved over.
        None => Stored {
            value: Map::new(),
            saving: studio_settings::set_aside(path).is_some(),
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

    /// A conversation rebound in this run is found by an id it had before, through each rebind
    /// since, and its old ids keep none of its state.
    #[gpui::test]
    fn a_rebound_conversation_is_found_by_its_old_id(cx: &mut gpui::TestAppContext) {
        let path = std::env::temp_dir().join(format!("studio-rebind-{}.json", std::process::id()));
        let sessions = cx.new(|_| AgentSessions::load(path.clone()));
        sessions.update(cx, |sessions, cx| {
            sessions.upsert(record("a", "server/crm", "claude", 0), cx);
            sessions.rebind("a", "b".into(), cx);
            sessions.rebind("b", "c".into(), cx);
            assert_eq!(sessions.current("a"), "c");
            assert_eq!(sessions.current("b"), "c");
            assert_eq!(sessions.current("c"), "c");
            assert_eq!(sessions.current("other"), "other");
            // A rebind of an id no record has changes nothing.
            sessions.rebind("gone", "d".into(), cx);
            assert_eq!(sessions.current("gone"), "gone");
        });
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn each_graph_has_a_directory_of_its_own() {
        let a = graph_cwd("server/crm");
        assert_eq!(a, graph_cwd("server/crm"));
        assert_ne!(a, graph_cwd("server-crm"));
        assert!(a.to_string_lossy().contains("server-crm-"));
    }

    /// A new conversation works in a folder of its own in its graph's directory, named for its
    /// branch, another each time; a saved one where it ran before.
    #[test]
    fn each_conversation_has_a_folder_of_its_own() {
        let first = working_dir(None, "server/crm", "agent/20261028-101500");
        let again = working_dir(None, "server/crm", "agent/20261028-101500");
        assert_eq!(first.parent(), Some(graph_cwd("server/crm").as_path()));
        assert_ne!(first, again);
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("agent-20261028-101500-"), "{name}");
        let saved = record("s1", "server/crm", "agent", 1);
        assert_eq!(working_dir(Some(&saved), "other", "agent/x"), saved.cwd);
    }

    /// On Windows agents work, with their conversations' CLI homes holding a session token and
    /// a certificate, in the local application folder, never the roaming one, which
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
            for name in ["Omnigraph Studio", "Omnigraph Studio Dev"] {
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
                    &roaming.join("Omnigraph Studio"),
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
                store.claim_branch("server/crm", second.into()),
                format!("{second}-3")
            );
            // Claimed before its record is written: the next one begun that second is told
            // apart too.
            assert_eq!(
                store.claim_branch("server/crm", second.into()),
                format!("{second}-4")
            );
            assert_eq!(store.claim_branch("server/notes", second.into()), second);
        });
        // A write that ended after its tab closed records that its branch exists, only while
        // the conversation still writes to that branch.
        store.update(cx, |store, cx| {
            store.set_branch_created("same-a", second, true, cx);
            store.set_branch_created("same-b", second, true, cx);
            assert!(store.get("same-a").unwrap().branch_created);
            assert!(
                !store.get("same-b").unwrap().branch_created,
                "another branch"
            );
            store.set_branch_created("same-a", second, false, cx);
            assert!(!store.get("same-a").unwrap().branch_created, "deleted");
        });
        studio_settings::writes_done();
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

    /// A file that does not parse as this build writes it is set aside whole, renamed beside
    /// itself byte for byte, and nothing in it is read: one holding a record Studio cannot read
    /// (a field missing or of another type) as one of another version. A file that parses is
    /// read whole.
    #[test]
    fn a_file_that_does_not_parse_is_set_aside_whole() {
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
        let fresh = || {
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
        };
        let aside = || -> Vec<std::path::PathBuf> {
            std::fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect()
        };

        // What the person is told of a file set aside names it and where it is kept.
        let told = |notice: Option<String>, aside: &[PathBuf]| {
            let notice = notice.expect("the person is told");
            assert!(notice.contains(&path.display().to_string()), "{notice}");
            assert!(notice.contains(&aside[0].display().to_string()), "{notice}");
            notice
        };

        // A file as Studio writes it is read whole, and stays, with nothing to tell.
        let file = json!({ "version": VERSION, "sessions": [record] });
        std::fs::write(&path, file.to_string()).unwrap();
        let (
            Stored {
                value: records,
                saving,
            },
            notice,
        ) = read_records(&path);
        assert!(saving);
        assert_eq!(records, std::slice::from_ref(&record));
        assert!(path.exists());
        assert_eq!(notice, None);

        // A record without its working directory, with none, without its marks, or of another
        // shape sets the whole file aside, and nothing in it is read.
        let mut without = serde_json::to_value(&record).unwrap();
        without.as_object_mut().unwrap().remove("cwd");
        let mut null = serde_json::to_value(&record).unwrap();
        null.as_object_mut()
            .unwrap()
            .insert("cwd".into(), Value::Null);
        let mut unmarked = serde_json::to_value(&record).unwrap();
        for field in ["pinned", "archived", "branch_created"] {
            unmarked.as_object_mut().unwrap().remove(field);
        }
        for unread in [without, null, unmarked, json!({ "session": 3 })] {
            fresh();
            let file = json!({ "version": VERSION, "sessions": [record, unread] });
            std::fs::write(&path, file.to_string()).unwrap();
            let (
                Stored {
                    value: records,
                    saving,
                },
                notice,
            ) = read_records(&path);
            assert!(saving, "set aside, so saved again: {unread}");
            assert!(records.is_empty(), "{unread}");
            assert!(!path.exists(), "set aside before it is written again");
            assert_eq!(aside().len(), 1);
            assert_eq!(
                std::fs::read_to_string(&aside()[0]).unwrap(),
                file.to_string(),
                "the file set aside is the file as it was"
            );
            told(notice, &aside());
        }

        // Every record readable, but the file of another version, or of none: nothing is read,
        // and the file is set aside.
        for version in [json!(VERSION + 1), json!(1), Value::Null] {
            fresh();
            let file = json!({ "version": version, "sessions": [record] });
            std::fs::write(&path, file.to_string()).unwrap();
            let (stored, notice) = read_records(&path);
            assert!(stored.value.is_empty(), "version {version}");
            assert!(!path.exists(), "version {version}");
            let notice = told(notice, &aside());
            if version == json!(VERSION + 1) {
                assert!(notice.contains("another build of Studio"), "{notice}");
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A conversations file set aside is told once: the index holds the notice from its load
    /// until a window shows it, and a file read, or one that could not be set aside (which the
    /// Conversations panel says), holds none. Before, one record of an older form emptied the
    /// list and nothing said so, nor where the file went.
    #[gpui::test]
    fn a_file_set_aside_is_told_once(cx: &mut gpui::TestAppContext) {
        let dir = std::env::temp_dir().join(format!("studio-agent-told-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        let mut legacy = serde_json::to_value(record("a", "server/crm", "claude", 0)).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .insert("cwd".into(), Value::Null);
        std::fs::write(
            &path,
            json!({ "version": VERSION, "sessions": [legacy] }).to_string(),
        )
        .unwrap();
        let store = cx.new(|_| AgentSessions::load(path.clone()));
        let notice = store.update(cx, |store, _| store.notice.take());
        let notice = notice.expect("told");
        assert!(
            notice.starts_with(&format!(
                "The agent conversations list {} could not be read (",
                path.display()
            )),
            "{notice}"
        );
        assert!(notice.contains(".unreadable-"), "{notice}");
        assert_eq!(store.read_with(cx, |store, _| store.notice.clone()), None);
        // The file written again in its place is read, and tells nothing.
        store.update(cx, |store, cx| {
            store.upsert(record("b", "server/crm", "claude", 0), cx)
        });
        studio_settings::writes_done();
        assert_eq!(AgentSessions::load(path.clone()).notice, None);
        // One that cannot be set aside tells nothing here: the panel says it is not saved.
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "not json").unwrap();
        block_set_aside(&path);
        let held = AgentSessions::load(path.clone());
        assert_eq!(held.notice, None);
        assert_eq!(held.not_saved(), Some(path.as_path()));
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
        let title = title_of(&long).unwrap();
        assert!(title.chars().count() <= 80 && title.ends_with('…'));
        assert_eq!(title_of("  \n "), None);
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
