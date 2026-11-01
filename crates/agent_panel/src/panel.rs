//! The agent panel's view and state: the thread, approval cards, the composer, and Studio's
//! tools, which run the omnigraph CLI against the conversation's gateway.

use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use agent_client_protocol::schema::MaybeUndefined;
use agent_client_protocol::schema::v1::{
    ContentBlock, PermissionOption, PermissionOptionKind, PlanEntryStatus,
    RequestPermissionOutcome, SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectOptions,
    SessionUpdate, StopReason, ToolCallStatus,
};
use editor::{Editor, EditorEvent};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FollowMode, Hsla,
    ListAlignment, ListState, Render, SharedString, Subscription, Task, WeakEntity, Window, list,
};
use graph_client::api::CommitKind;
use graph_store::{GraphSession, OutcomeKind};
use serde_json::Value;
use studio_actions::agent::{Allow, AllowAlways, ClearQueue, Reject, Restart, Send, SendNow, Stop};
use studio_actions::graph::ReviewBranch;
use studio_settings::{AgentSettings, StudioSettings};
use studio_ui::ds;
use studio_ui::ds::{Dotted as _, Prose as _};
use studio_ui::menu::ContextMenu;
use studio_workspace::{Item, ItemEvent, SavedItem, Workspace};
use ui::prelude::*;
use ui::{CircularProgress, KeyBinding, PopoverMenu, Severity, Tooltip};

use crate::cli_home::{self, CliHome};
use crate::gateway::{Gateway, GatewayEvent, OnOutcome, Seen};
use crate::gateway_rules::{Creates, Sandbox, WriteKind, WriteSummary};
use crate::host::{AgentLaunch, HostCommand, HostEvent, Opened, SessionLaunch};
use crate::sessions::{AgentSessions, Live, SessionRecord};

/// Presets offered when no agent is set: the ACP crate's own preset commands.
pub const PRESETS: &[(&str, &[&str])] = &[
    (
        "Claude Agent",
        &["npx", "-y", "@agentclientprotocol/claude-agent-acp@latest"],
    ),
    (
        "Codex",
        &["npx", "-y", "@agentclientprotocol/codex-acp@latest"],
    ),
];

const UNKNOWN_OUTCOME: &str = "The server did not confirm this change. It was not retried; \
check the branch history before the agent tries it again.";

/// What the card of a write still waiting for the person says when the agent stops.
const STOPPED_UNSENT: &str = "Not sent: the agent stopped before you decided.";

/// How long the agent may take to start before the thread says it is slow and offers to start
/// it again. A first start fetches the adapter (`npx`) and starts its MCP servers, which can
/// take seconds; one past this has most likely stalled.
const SLOW_START: std::time::Duration = std::time::Duration::from_secs(20);

/// What a forwarded write's outcome is applied to: its card, what it is, and the agent's branch
/// it went to with that branch's base.
#[derive(Debug, Clone)]
struct SentWrite {
    card: u64,
    kind: WriteKind,
    creates: Creates,
    branch: String,
    base: String,
}

/// Whether a write that may create the agent's branch made it: a branch create that went
/// through, or a load given `from` whose answer says it did.
fn makes_branch(creates: Creates, seen: &Seen) -> bool {
    match creates {
        Creates::Always => matches!(seen, Seen::Written { .. }),
        Creates::IfMissing => matches!(
            seen,
            Seen::Written {
                branch_created: true,
                ..
            }
        ),
        Creates::Never => false,
    }
}

/// Whether the agent's branch exists after a write of `kind` that ended so, when the outcome
/// says: one that went through on the branch leaves it there, made now or before, and a delete
/// that went through takes it away.
fn branch_after(kind: WriteKind, seen: &Seen) -> Option<bool> {
    matches!(seen, Seen::Written { .. }).then_some(kind != WriteKind::BranchDelete)
}

/// What a write's card shows once it ended: its commit on the branch, a commit no branch shows,
/// one of a kind this build does not know (whether the branch moved is not known), no commit
/// (a branch made or deleted, a mutation that matched nothing), a stored query that read, the
/// server's refusal, or no confirmation.
fn card_state(seen: &Seen, made_branch: bool) -> WriteState {
    match seen {
        Seen::Written {
            commit: Some((commit, CommitKind::Published)),
            nodes,
            edges,
            ..
        } => WriteState::Committed {
            commit: commit.clone(),
            nodes: *nodes,
            edges: *edges,
        },
        Seen::Written {
            commit: Some((commit, kind @ (CommitKind::Unpublished | CommitKind::Unattached))),
            ..
        } => WriteState::Unpublished {
            commit: commit.clone(),
            kind: *kind,
        },
        Seen::Written {
            commit: Some((commit, _)),
            ..
        } => WriteState::Unknown {
            commit: Some(commit.clone()),
        },
        Seen::Written { commit: None, .. } => WriteState::Done(if made_branch {
            "Branch created".into()
        } else {
            "Done".into()
        }),
        Seen::Read => WriteState::Done("Read; nothing written".into()),
        Seen::Refused(message) => WriteState::Refused(message.clone()),
        Seen::Unknown(_) => WriteState::Unknown { commit: None },
    }
}

/// Hands the gateway its decision. An approval it can no longer take, the CLI having given up
/// meanwhile, records that nothing was sent, rather than being dropped and taken for a write
/// whose outcome is unknown.
fn hand_over(decide: oneshot::Sender<Option<OnOutcome>>, approval: Option<OnOutcome>) {
    if let Err(Some(approval)) = decide.send(approval) {
        approval(&Seen::Refused(crate::gateway::NOT_SENT.into()));
    }
}

/// A gateway write's outcome, as Studio's activity records it: by its commit's kind, as Studio's
/// own writes are ([`OutcomeKind::written`]). A commit on the agent's branch is committed there;
/// one written `as commit` or `as unattached branch` is held, with its commit, and moves no
/// branch; one of a kind this build does not know leaves whether a branch moved unknown. A
/// stored query that read wrote nothing, as a mutation that matched nothing.
fn outcome_kind(seen: &Seen) -> OutcomeKind {
    match seen {
        Seen::Written { commit, .. } => OutcomeKind::written(commit.clone()),
        Seen::Read => OutcomeKind::Committed { commit: None },
        Seen::Refused(message) => OutcomeKind::Refused {
            message: message.clone(),
        },
        Seen::Unknown(reason) => OutcomeKind::Unknown {
            reason: reason.clone(),
        },
    }
}

/// Opens the agent for the workspace's graph: an agent tab already open (the shown one first),
/// else the graph's last conversation with the agent, else a new one.
pub fn open(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<AgentPanel>> {
    let shown = workspace
        .active_item()
        .and_then(|item| item.to_any().downcast::<AgentPanel>().ok());
    let open = shown.or_else(|| {
        workspace
            .all_items()
            .find_map(|item| item.to_any().downcast::<AgentPanel>().ok())
    });
    if let Some(panel) = open {
        workspace.activate_item_by_id(panel.entity_id(), window, cx);
        return Some(panel);
    }
    let graph = workspace.session()?.read(cx).overrides_key();
    let agent = StudioSettings::get(cx).agent.command.join(" ");
    let sessions = AgentSessions::global(cx);
    let latest = sessions
        .read(cx)
        .latest(&graph, &agent)
        .map(|record| record.session.clone());
    match latest {
        Some(id) => open_session(workspace, id, window, cx),
        None => new_conversation(workspace, window, cx),
    }
}

/// Opens one of the graph's conversations in its own tab, or shows the tab it is in; the agent
/// starts and replays it once the tab is shown ([`AgentPanel::start_when_shown`]).
///
/// A conversation has one tab, in whichever pane it is, also while a split or a restore fills
/// a pane with copies: a second tab would load the session again and take the first one's
/// replies, permission prompts and CLI credentials.
pub fn open_session(
    workspace: &mut Workspace,
    id: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<AgentPanel>> {
    let open = workspace.all_items().find_map(|item| {
        let panel = item.to_any().downcast::<AgentPanel>().ok()?;
        (panel.read(cx).session_id.as_deref() == Some(id.as_str())).then_some(panel)
    });
    if let Some(panel) = open {
        workspace.activate_item_by_id(panel.entity_id(), window, cx);
        return Some(panel);
    }
    let session = workspace.session()?.clone();
    let handle = cx.entity().downgrade();
    let panel = workspace.open_item(format!("agent:{id}"), window, cx, move |window, cx| {
        AgentPanel::new(session, handle, Some(id), window, cx)
    })?;
    panel.update(cx, |panel, cx| panel.start_when_shown(cx));
    Some(panel)
}

/// Opens a new conversation in a tab of its own, with the default agent, which starts when the
/// tab is shown. It joins the graph's list when the first message is sent.
pub fn new_conversation(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<AgentPanel>> {
    let session = workspace.session()?.clone();
    let handle = cx.entity().downgrade();
    let panel = workspace.add_item(window, cx, move |window, cx| {
        AgentPanel::new(session, handle, None, window, cx)
    });
    panel.update(cx, |panel, cx| panel.start_when_shown(cx));
    Some(panel)
}

enum State {
    /// No agent chosen yet.
    Setup,
    /// An agent is chosen but not running.
    Idle,
    Starting,
    Ready {
        agent: String,
    },
    Prompting {
        agent: String,
    },
    Exited {
        message: String,
        stderr: String,
    },
}

/// The agent's branch for this agent session.
struct AgentBranch {
    name: String,
    base: String,
    /// Whether the branch exists, as the conversation's record keeps it.
    created: bool,
    /// The changes the agent committed on it since the conversation opened in this tab.
    changes: usize,
}

impl AgentBranch {
    /// The branch of a conversation coming back: its record's, with whether it exists.
    fn resumed(saved: &SessionRecord, base: String) -> Self {
        Self {
            name: saved.branch.clone(),
            base,
            created: saved.branch_created,
            changes: 0,
        }
    }

    /// Whether the composer's notice offers Review: once the branch exists, made in this tab
    /// or before. How many changes it shows counts only those made since the conversation
    /// opened here.
    fn offers_review(&self) -> bool {
        self.created
    }
}

enum Decision {
    Allow,
    AllowAlways,
    Reject,
}

/// Where a write the gateway holds is: waiting for the person's answer, sent, or how it ended.
enum WriteState {
    Waiting(oneshot::Sender<Decision>),
    Running,
    /// The commit it made, and what changed. A write that made none is `Done`.
    Committed {
        commit: String,
        nodes: i64,
        edges: i64,
    },
    Refused(String),
    /// Whether it went through is not known: the server's answer was lost or confirms nothing,
    /// or it names `commit`, of a kind this build does not know, so whether the branch moved is
    /// not known.
    Unknown {
        commit: Option<String>,
    },
    Rejected,
    /// A write that made no commit of its own went through, such as creating a branch.
    Done(String),
    /// The server held the write as this commit, of this kind, which no branch shows: the
    /// agent's branch did not move.
    Unpublished {
        commit: String,
        kind: CommitKind,
    },
}

/// The card of a write the agent's CLI sends through the gateway (spec 013): what it does, the
/// branch it writes and the base it creates that branch from, and where it is. `id` is the
/// panel's own for it, which its outcome finds it by.
struct WriteCard {
    id: u64,
    summary: String,
    query: String,
    params: Vec<(String, String)>,
    branch: String,
    creates_from: Option<String>,
    state: WriteState,
}

struct PermissionCard {
    title: String,
    options: Vec<PermissionOption>,
    reply: Option<oneshot::Sender<RequestPermissionOutcome>>,
    answer: Option<String>,
}

impl PermissionCard {
    /// Answers "cancelled" if the card still waits, and says so on it; whether it waited.
    fn cancel(&mut self) -> bool {
        let Some(reply) = self.reply.take() else {
            return false;
        };
        reply.send(RequestPermissionOutcome::Cancelled).ok();
        self.answer = Some("Cancelled".into());
        true
    }
}

enum Entry {
    User(String),
    Agent(String),
    Thought(String),
    Tool {
        id: String,
        title: String,
        status: ToolCallStatus,
    },
    Plan(Vec<(String, PlanEntryStatus)>),
    Permission(PermissionCard),
    Write(WriteCard),
    Notice {
        text: String,
        error: bool,
    },
}

impl Entry {
    /// A card waiting for the person's answer.
    fn waits(&self) -> bool {
        match self {
            Entry::Permission(card) => card.reply.is_some(),
            Entry::Write(card) => matches!(card.state, WriteState::Waiting(_)),
            _ => false,
        }
    }
}

/// One item of the thread's list: an entry, a run of steps folded to one line, the message sent before the agent was ready with how its start goes, or the
/// callout of an agent that stopped. `lead` carries the agent's mark; `ends`
/// marks the last reply of a finished turn, which carries the turn's actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Block {
    Entry {
        index: usize,
        lead: bool,
        ends: bool,
    },
    Activity {
        start: usize,
        end: usize,
        lead: bool,
    },
    /// The agent starting, under the message sent before it was ready
    /// ([`AgentPanel::opening`]), or that message after the agent stopped before it went.
    Waiting,
    Exited,
}

/// What a thread was brought up to: its entries' version, its folds' count, how the agent
/// stopped, and whether it is working on a turn, which holds back the turn's actions
/// ([`AgentPanel::thread_key`]).
type ThreadKey = (u64, u64, Option<u64>, bool, Option<u64>);

/// The thread's entries, and what each shows folded to a number, kept as they change: a
/// streamed token rehashes the entry it lands in, not the whole conversation (a reply can be
/// can hold a whole file). Every way to change an entry marks it, so none goes stale.
#[derive(Default)]
struct Entries {
    list: Vec<Entry>,
    hashes: Vec<u64>,
    /// The first entry whose hash may be stale.
    stale_from: usize,
    /// Counts the changes: a draw that finds it as it last was has nothing to bring up to date.
    version: u64,
    /// No entry before this one waits for the person ([`Entries::first_waiting`]).
    answered_before: std::cell::Cell<usize>,
}

impl std::ops::Deref for Entries {
    type Target = [Entry];

    fn deref(&self) -> &[Entry] {
        &self.list
    }
}

impl Entries {
    fn push(&mut self, entry: Entry) {
        self.version += 1;
        self.list.push(entry);
    }

    fn get_mut(&mut self, index: usize) -> Option<&mut Entry> {
        self.version += 1;
        self.stale_from = self.stale_from.min(index);
        self.list.get_mut(index)
    }

    fn last_mut(&mut self) -> Option<&mut Entry> {
        let last = self.list.len().checked_sub(1)?;
        self.get_mut(last)
    }

    /// Offers every entry to `change`, which says whether it changed it: the hashes go stale
    /// from the first changed only, so settling a turn's open tools leaves the rest of a long
    /// conversation as hashed.
    fn change_each(&mut self, mut change: impl FnMut(&mut Entry) -> bool) {
        for (index, entry) in self.list.iter_mut().enumerate() {
            if change(entry) {
                self.stale_from = self.stale_from.min(index);
                self.version += 1;
            }
        }
    }

    /// Answers every permission request still waiting "cancelled", as ACP asks of a client
    /// that cancels the turn: their cards say so, and no key answers them after.
    fn cancel_permissions(&mut self) {
        self.change_each(|entry| match entry {
            Entry::Permission(card) => card.cancel(),
            _ => false,
        });
    }

    fn clear(&mut self) {
        self.version += 1;
        self.list.clear();
        self.hashes.clear();
        self.stale_from = 0;
        self.answered_before.set(0);
    }

    /// Takes the entries from `at` on out of the thread.
    fn take_from(&mut self, at: usize) -> Vec<Entry> {
        let at = at.min(self.list.len());
        self.version += 1;
        self.hashes.truncate(at);
        self.stale_from = self.stale_from.min(at);
        // Put back, a card taken may wait again where it was.
        self.answered_before.set(self.answered_before.get().min(at));
        self.list.split_off(at)
    }

    /// The first card waiting for the person that `also` holds for. A card waits from when it
    /// is added, at the end, until it is answered, and never again, so the search starts past
    /// the cards found answered before: asked on every change, as each streamed token makes
    /// one, it looks at the cards still waiting, not the whole conversation.
    fn first_waiting(&self, also: impl Fn(&Entry) -> bool) -> Option<usize> {
        let from = self.answered_before.get().min(self.list.len());
        let first = self.list[from..]
            .iter()
            .position(Entry::waits)
            .map_or(self.list.len(), |at| from + at);
        self.answered_before.set(first);
        self.list[first..]
            .iter()
            .position(|entry| entry.waits() && also(entry))
            .map(|at| first + at)
    }

    /// The write card the approval keys answer: the one waiting longest. A permission request
    /// of the agent's own is answered only by its buttons: there the agent's "always" option is
    /// a lasting rule of its own, which ⌘⌥Y, Always on this branch for a write, must not pick
    /// unseen.
    fn keyed_write(&self) -> Option<usize> {
        self.first_waiting(|entry| matches!(entry, Entry::Write(_)))
    }

    fn extend(&mut self, entries: Vec<Entry>) {
        if !entries.is_empty() {
            self.version += 1;
            self.list.extend(entries);
        }
    }

    /// Brings the hashes up to the entries. Returns the first entry that is new or changed since
    /// the last time: every entry before it is as it was then.
    fn rehash(&mut self) -> usize {
        self.hashes.truncate(self.stale_from.min(self.list.len()));
        let changed = self.hashes.len();
        for entry in &self.list[changed..] {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            hash_entry(entry, &mut hasher);
            self.hashes.push(std::hash::Hasher::finish(&hasher));
        }
        self.stale_from = self.list.len();
        changed
    }

    /// The card of write `card`, wherever the thread holds it now; none once the thread was
    /// shown again without it (the agent replayed the conversation after a restart).
    fn write_card(&mut self, card: u64) -> Option<&mut WriteCard> {
        let index = self
            .list
            .iter()
            .position(|entry| matches!(entry, Entry::Write(write) if write.id == card))?;
        match self.get_mut(index) {
            Some(Entry::Write(write)) => Some(write),
            _ => None,
        }
    }

    /// Settles the write cards still waiting for the person as the agent stops: the gateway
    /// that would send them went with it, so nothing was sent. A write already sent runs to its
    /// end, and its card hears how ([`AgentPanel::record_outcome`]).
    fn stop_writes(&mut self) {
        self.change_each(|entry| match entry {
            Entry::Write(card) if matches!(card.state, WriteState::Waiting(_)) => {
                card.state = WriteState::Refused(STOPPED_UNSENT.into());
                true
            }
            _ => false,
        });
    }

    /// What entry `index` shows, as of the last [`Entries::rehash`].
    fn hash(&self, index: usize) -> u64 {
        self.hashes.get(index).copied().unwrap_or_default()
    }
}

/// Feeds what an entry shows to `hasher` (`AgentPanel::fingerprint`).
fn hash_entry(entry: &Entry, hasher: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    std::mem::discriminant(entry).hash(hasher);
    match entry {
        Entry::User(text) | Entry::Agent(text) | Entry::Thought(text) => text.hash(hasher),
        Entry::Tool { title, status, .. } => {
            title.hash(hasher);
            format!("{status:?}").hash(hasher);
        }
        Entry::Plan(items) => {
            for (content, status) in items {
                content.hash(hasher);
                format!("{status:?}").hash(hasher);
            }
        }
        Entry::Permission(card) => {
            card.title.hash(hasher);
            card.options.len().hash(hasher);
            card.answer.hash(hasher);
        }
        Entry::Write(card) => {
            card.summary.hash(hasher);
            card.query.hash(hasher);
            card.branch.hash(hasher);
            card.creates_from.hash(hasher);
            card.params.hash(hasher);
            match &card.state {
                WriteState::Waiting(_) => 0u8.hash(hasher),
                WriteState::Running => 1u8.hash(hasher),
                WriteState::Committed {
                    commit,
                    nodes,
                    edges,
                } => {
                    2u8.hash(hasher);
                    commit.hash(hasher);
                    nodes.hash(hasher);
                    edges.hash(hasher);
                }
                WriteState::Refused(reason) => {
                    3u8.hash(hasher);
                    reason.hash(hasher);
                }
                WriteState::Unknown { commit } => {
                    4u8.hash(hasher);
                    commit.hash(hasher);
                }
                WriteState::Rejected => 5u8.hash(hasher),
                WriteState::Done(label) => {
                    6u8.hash(hasher);
                    label.hash(hasher);
                }
                WriteState::Unpublished { commit, kind } => {
                    7u8.hash(hasher);
                    commit.hash(hasher);
                    std::mem::discriminant(kind).hash(hasher);
                }
            }
        }
        Entry::Notice { text, error } => {
            text.hash(hasher);
            error.hash(hasher);
        }
    }
}

/// How full the agent's context window is, and what the session has cost so far, as the agent
/// last reported them.
struct Usage {
    used: u64,
    size: u64,
    cost: Option<(f64, String)>,
}

impl Usage {
    /// How much of the window is used, from 0 to 1, of a window whose size is known.
    fn ratio(&self) -> f32 {
        (self.used as f32 / self.size as f32).clamp(0., 1.)
    }

    /// The tokens used of the window's, as "12k of 200k tokens".
    fn tokens(&self) -> String {
        format!(
            "{} of {} tokens",
            humanize_tokens(self.used),
            humanize_tokens(self.size)
        )
    }

    /// The window as a ring filled to how much is used, in `color`, over a visible track, so it
    /// reads as a gauge, not a spinner.
    fn gauge(&self, color: Hsla, cx: &App) -> CircularProgress {
        CircularProgress::new(self.used as f32, self.size as f32, px(ds::size::GAUGE), cx)
            .stroke_width(px(ds::size::GAUGE_STROKE))
            .bg_color(ds::surface::control_border(cx))
            .progress_color(color)
    }
}

/// A message sent while the agent was working, waiting for its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Queued {
    id: usize,
    text: String,
}

/// Messages sent while the agent works (spec 011), as Zed queues them on the client: ACP runs one
/// turn at a time and has no queue of its own. The first goes out as each turn ends; Stop
/// pauses the queue until the person sends again; "Send now" stops the turn and sends one at
/// once, without the queue going on by itself.
#[derive(Debug, Default)]
struct MessageQueue {
    entries: Vec<Queued>,
    next_id: usize,
    paused: bool,
    /// Sent when the stopped turn ends, ahead of the queue.
    after_cancel: Option<String>,
}

impl MessageQueue {
    fn push(&mut self, text: String) {
        self.entries.push(Queued {
            id: self.next_id,
            text,
        });
        self.next_id += 1;
        // Queuing is the person engaging again.
        self.paused = false;
    }

    fn take(&mut self, id: usize) -> Option<String> {
        let index = self.entries.iter().position(|entry| entry.id == id)?;
        Some(self.entries.remove(index).text)
    }

    /// What to send now that a turn ended: the message "Send now" stopped it for, else the
    /// next in line unless the queue is paused.
    fn on_turn_ended(&mut self) -> Option<String> {
        if let Some(text) = self.after_cancel.take() {
            return Some(text);
        }
        if self.paused || self.entries.is_empty() {
            return None;
        }
        Some(self.entries.remove(0).text)
    }

    /// "Send now": `text` goes once the turn stops. A message "Send now" asked for before it,
    /// not sent yet, goes back first in line rather than being lost.
    fn send_after_cancel(&mut self, text: String) {
        if let Some(earlier) = self.after_cancel.replace(text) {
            self.put_back(earlier);
        }
    }

    /// A message taken to send that could not go: first in line again.
    fn put_back(&mut self, text: String) {
        self.entries.insert(
            0,
            Queued {
                id: self.next_id,
                text,
            },
        );
        self.next_id += 1;
    }

    /// Puts `text` in line: first, as "Send now" asks, or after the rest.
    fn enqueue(&mut self, text: String, first: bool) {
        if first {
            self.put_back(text);
        } else {
            self.push(text);
        }
    }

    /// Something is waiting to be sent, which closing the tab would drop.
    fn holds_messages(&self) -> bool {
        !self.entries.is_empty() || self.after_cancel.is_some()
    }
}

struct Host {
    commands: mpsc::UnboundedSender<HostCommand>,
    /// The conversation's events, closed as the host is dropped: the connection then sees at
    /// once that this panel let go, before the panel asks for the session again, and the new
    /// conversation waits for this one to free the session.
    events: mpsc::UnboundedSender<HostEvent>,
    /// Keeps the agent's process running while this conversation is open.
    _connection: std::sync::Arc<crate::connection::AgentConnection>,
    /// The conversation's CLI home (spec 013); its session token goes with it.
    _cli_home: Option<std::sync::Arc<CliHome>>,
    _tasks: Vec<Task<()>>,
}

impl Drop for Host {
    fn drop(&mut self) {
        self.events.close_channel();
    }
}

/// What a conversation's agent needs before it is launched, made off the main thread: its
/// working directory, and the omnigraph CLI's gateway and home with `AGENTS.md` and the skill,
/// which it can start without.
struct Prepared {
    gateway: std::io::Result<(Gateway, CliHome)>,
    /// Why `AGENTS.md` and the skill could not be written.
    rules: Option<std::io::Error>,
}

impl Prepared {
    /// Makes them for a conversation in `cwd` on `graph`, writing to `branch` off `base`; a
    /// start fails, with its message, when the directory cannot be made.
    fn make(cwd: &Path, graph: &str, branch: &str, base: &str) -> Result<Self, String> {
        std::fs::create_dir_all(cwd)
            .map_err(|error| format!("Studio could not create {}: {error}", cwd.display()))?;
        let gateway = Gateway::bind().and_then(|gateway| {
            let home = CliHome::write(cwd, gateway.access(), graph)?;
            Ok((gateway, home))
        });
        let rules = cli_home::write_agents_md(cwd, graph, branch, base)
            .and_then(|()| crate::skill::write(cwd))
            .err();
        Ok(Self { gateway, rules })
    }
}

pub struct AgentPanel {
    session: Entity<GraphSession>,
    workspace: WeakEntity<Workspace>,
    /// The graph's conversations, which this tab records itself in.
    sessions: Entity<AgentSessions>,
    /// Whether the tab last said it holds unsaved work ([`AgentPanel::holds_work`]): the tab
    /// is told when that changes, not at every change.
    dirty: bool,
    /// The agent's replies as rendered markdown, by entry index, with how much of the text each
    /// has taken in; a streaming reply appends its new tail.
    markdown: std::collections::HashMap<usize, (Entity<Markdown>, usize)>,
    focus_handle: FocusHandle,
    composer: Entity<Editor>,
    /// The composer holds something to send: read once an edit, not at every frame the Send
    /// button is drawn.
    has_draft: bool,
    custom_command: Entity<Editor>,
    /// The thread as a list that lays out only what shows (as Zed's agent thread does), and
    /// the blocks it last drew with their fingerprints, to remeasure only what changed.
    thread: ListState,
    shown_blocks: Vec<(Block, u64)>,
    /// What the thread was last brought up to ([`AgentPanel::thread_key`]): a frame with
    /// nothing new skips the walk of every entry.
    synced_thread: Option<ThreadKey>,
    /// Each reply's markdown is watched: its parse finishes after the text arrives, and the
    /// block grows then.
    _markdown_watch: std::collections::HashMap<usize, Subscription>,
    state: State,
    entries: Entries,
    /// The message sent before the agent was ready for it: shown in the thread at once, as the
    /// person's, and sent as the first turn once the agent is ready. Later ones wait in the
    /// queue behind it, as they would behind a running turn.
    opening: Option<String>,
    /// The agent has been starting for [`SLOW_START`] without answering: the thread says so
    /// and offers to start it again.
    slow_start: bool,
    _slow_start: Option<Task<()>>,
    branch: Option<AgentBranch>,
    always_allow: bool,
    host: Option<Host>,
    /// Making what a start needs on disk, off the main thread; dropped, the start is not made.
    preparing: Option<Task<()>>,
    /// The agent comes back after it stopped, with the conversation on screen, this many entries
    /// long: when it replays the conversation (`session/load`), the replay replaces them
    /// instead of adding to them.
    replace_on_replay: Option<usize>,
    /// What was said while the agent came back to a conversation (a setup problem, the CLI it
    /// found), kept aside while the agent replays the conversation and shown after it.
    said_while_starting: Vec<Entry>,
    /// The conversation this tab holds: the one it loads before the agent answers, then the one
    /// the agent opened. `None` starts a new one.
    session_id: Option<String>,
    /// The agent this conversation runs: the one it was held with, else the default when it
    /// opened. Empty until an agent is chosen.
    agent: Vec<String>,
    /// The agent starts when the tab is next drawn ([`AgentPanel::start_when_shown`]): a
    /// conversation restored with the workspace behind another tab starts no agent until it is
    /// shown.
    start_owed: bool,
    /// The agent stopped with an error.
    failed: bool,
    /// The directory the agent works in for this conversation.
    cwd: Option<PathBuf>,
    _focus: Option<gpui::Subscription>,
    /// The options the agent offers for this session (its model, mode, reasoning level), in its
    /// order and as it last confirmed them.
    config_options: Vec<SessionConfigOption>,
    /// A value chosen and sent but not yet confirmed, shown in the meantime.
    setting_option: Option<(String, SessionConfigOptionValue)>,
    /// The options as the composer shows them, made when they or the value being set change,
    /// not at every frame the composer is drawn: a model picker holds every model's name.
    option_views: Vec<Rc<OptionView>>,
    usage: Option<Usage>,
    /// Runs of tool calls and thoughts opened to their steps, by the run's first entry.
    expanded_activity: std::collections::HashSet<usize>,
    /// Counts the changes to which runs are open.
    folds: u64,
    queue: MessageQueue,
    /// The card of each write the gateway holds, by the gateway's id: the card's own id, for a
    /// write the CLI gave up before the person decided.
    gateway_cards: std::collections::HashMap<u64, u64>,
    /// The id the next write card takes; never reused, so a write's outcome finds its own card
    /// after a restart showed the conversation again.
    next_card: u64,
    /// The fold count the thread's blocks were last measured with: a fold changes what every
    /// block of it shows.
    synced_folds: Option<u64>,
}

impl EventEmitter<ItemEvent> for AgentPanel {}

impl Focusable for AgentPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for AgentPanel {
    /// A conversation is the agent's, kept on this machine: its requests to the server say
    /// their own refusals, and the conversation stays readable while a sign-in is asked for.
    fn reads_graph(&self) -> bool {
        false
    }

    /// The conversation's title, as the sidebar lists it.
    fn tab_title(&self, cx: &App) -> SharedString {
        self.session_id
            .as_ref()
            .and_then(|id| self.sessions.read(cx).get(id))
            .and_then(|record| record.title.clone())
            .map(|title| studio_ui::text::cut(&title, TAB_TITLE_CHARS).into())
            .unwrap_or_else(|| "New conversation".into())
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::ZedAssistant)
    }

    /// A conversation is one tab; a new one has no key until the agent names its session.
    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        self.session_id
            .as_ref()
            .map(|id| format!("agent:{id}").into())
    }

    /// Closing drops a draft, the messages waiting their turn and the cards waiting for an
    /// answer, so it asks first.
    fn is_dirty(&self, _cx: &App) -> bool {
        self.holds_work()
    }

    /// The tab comes back after a restart with its conversation.
    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "agent".into(),
            data: match &self.session_id {
                Some(id) => serde_json::json!({ "session": id }),
                None => Value::Null,
            },
        })
    }
}

impl AgentPanel {
    pub fn new(
        session: Entity<GraphSession>,
        workspace: WeakEntity<Workspace>,
        session_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 10, window, cx);
            editor.set_placeholder_text("Ask about the graph, or ask for a change…", window, cx);
            editor
        });
        let custom_command = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(
                "Program and arguments, for example my-agent --acp",
                window,
                cx,
            );
            editor
        });
        cx.observe_global::<StudioSettings>(|this, cx| {
            let was_setup = matches!(this.state, State::Setup);
            if this.agent.is_empty() {
                this.agent = StudioSettings::get(cx).agent.command.clone();
            }
            this.sync_setup_state(cx);
            // What was sent before an agent was chosen goes to the one chosen.
            if was_setup
                && matches!(this.state, State::Idle)
                && (this.opening.is_some() || this.queue.holds_messages())
            {
                this.start(cx);
            }
            cx.notify();
        })
        .detach();
        cx.subscribe(
            &composer,
            |this: &mut Self, composer, event: &EditorEvent, cx| {
                if let EditorEvent::BufferEdited = event {
                    let has_draft = !composer.read(cx).text(cx).trim().is_empty();
                    if has_draft != this.has_draft {
                        this.has_draft = has_draft;
                        cx.notify();
                    }
                }
            },
        )
        .detach();
        let sessions = AgentSessions::global(cx);
        // A tab title follows the conversation's title; a deleted conversation's tab closes.
        cx.observe(&sessions, |this: &mut Self, sessions, cx| {
            let deleted = this
                .session_id
                .as_ref()
                .is_some_and(|id| sessions.read(cx).was_deleted(id));
            cx.emit(if deleted {
                ItemEvent::Close
            } else {
                ItemEvent::UpdateTab
            });
        })
        .detach();
        // Whatever changes the conversation (a turn, an approval card, a decision), the graph's
        // list learns what it is doing now, and the tab whether it holds unsaved work; a closed
        // tab's agent stops with it.
        cx.observe_self(|this, cx| {
            this.sync_live(cx);
            let dirty = this.holds_work();
            if dirty != this.dirty {
                this.dirty = dirty;
                cx.emit(ItemEvent::UpdateTab);
            }
        })
        .detach();
        cx.on_release(|this, cx| {
            if let Some(id) = this.session_id.clone() {
                this.sessions
                    .update(cx, |sessions, cx| sessions.set_live(&id, None, cx));
            }
            this.discard_unused_folder(cx);
        })
        .detach();
        let focus_handle = cx.focus_handle();
        // Seeing a conversation reads what it finished while unseen.
        let focus = cx.on_focus_in(&focus_handle, window, |this: &mut Self, _, cx| {
            if let Some(id) = this.session_id.clone() {
                this.sessions
                    .update(cx, |sessions, cx| sessions.mark_read(&id, cx));
            }
        });
        let agent = agent_for(session_id.as_deref(), &sessions, cx);
        let mut panel = Self {
            session,
            workspace,
            sessions,
            dirty: false,
            focus_handle,
            composer,
            has_draft: false,
            custom_command,
            thread: {
                let thread = ListState::new(0, ListAlignment::Top, px(800.));
                thread.set_follow_mode(FollowMode::Tail);
                thread
            },
            shown_blocks: Vec::new(),
            synced_thread: None,
            _markdown_watch: std::collections::HashMap::new(),
            state: State::Setup,
            entries: Entries::default(),
            opening: None,
            slow_start: false,
            _slow_start: None,
            markdown: std::collections::HashMap::new(),
            branch: None,
            always_allow: false,
            host: None,
            preparing: None,
            replace_on_replay: None,
            said_while_starting: Vec::new(),
            session_id,
            agent,
            start_owed: false,
            failed: false,
            cwd: None,
            _focus: Some(focus),
            config_options: Vec::new(),
            setting_option: None,
            option_views: Vec::new(),
            usage: None,
            expanded_activity: std::collections::HashSet::new(),
            folds: 0,
            queue: MessageQueue::default(),
            gateway_cards: std::collections::HashMap::new(),
            next_card: 0,
            synced_folds: None,
        };
        panel.sync_setup_state(cx);
        panel
    }

    fn sync_setup_state(&mut self, _cx: &App) {
        let has_command = !self.agent.is_empty();
        match self.state {
            State::Setup if has_command => self.state = State::Idle,
            State::Idle if !has_command => self.state = State::Setup,
            _ => {}
        }
    }

    // ---- lifecycle ----

    /// Starts the agent when the tab is next drawn, as Zed does when a thread opens: the agent is
    /// ready by the time the first message is written, and a tab not shown runs no agent.
    pub fn start_when_shown(&mut self, cx: &mut Context<Self>) {
        self.start_owed = true;
        cx.notify();
    }

    fn start_if_idle(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, State::Idle) {
            self.start(cx);
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        if self.agent.is_empty() {
            self.state = State::Setup;
            return;
        }
        let mut settings = StudioSettings::get(cx).agent.clone();
        settings.command = self.agent.clone();
        // The conversation this tab holds, to load with the agent it was held with (spec 011:
        // resuming a session).
        let saved = self
            .session_id
            .as_ref()
            .and_then(|id| self.sessions.read(cx).get(id).cloned());
        let graph_key = self.session.read(cx).overrides_key();
        let base = self.session.read(cx).branch.clone();
        // A resumed conversation keeps writing to its branch.
        let branch = match &saved {
            Some(saved) => AgentBranch::resumed(saved, base),
            None => AgentBranch {
                name: self.sessions.update(cx, |sessions, _| {
                    sessions.claim_branch(
                        &graph_key,
                        format!("agent/{}", chrono::Local::now().format("%Y%m%d-%H%M%S")),
                    )
                }),
                base,
                created: false,
                changes: 0,
            },
        };
        let cwd = crate::sessions::working_dir(saved.as_ref(), &graph_key, &branch.name);
        let graph = self.session.read(cx).graph_id.clone();
        // The directory, the gateway and the files the agent needs are made off the main thread
        // (constitution IX); the agent is launched once they are there.
        let prepare = {
            let (cwd, graph) = (cwd.clone(), graph.clone());
            let (name, base) = (branch.name.clone(), branch.base.clone());
            cx.background_spawn(async move { Prepared::make(&cwd, &graph, &name, &base) })
        };
        self.enter_starting(cx);
        self.preparing = Some(cx.spawn(async move |this, cx| {
            let prepared = prepare.await;
            this.update(cx, |this, cx| match prepared {
                Ok(prepared) => this.launch(prepared, settings, cwd, branch, graph, cx),
                Err(message) => this.fail_start(message, cx),
            })
            .ok();
        }));
        cx.notify();
    }

    /// Launches the agent on what [`Prepared::make`] made for it.
    fn launch(
        &mut self,
        prepared: Prepared,
        settings: AgentSettings,
        cwd: PathBuf,
        branch: AgentBranch,
        graph: String,
        cx: &mut Context<Self>,
    ) {
        let Prepared { gateway, rules } = prepared;
        self.failed = false;
        let (agent_branch, agent_base) = (branch.name.clone(), branch.base.clone());
        self.branch = Some(branch);
        self.always_allow = false;

        // The agent's omnigraph CLI (spec 013): a gateway for this conversation, and a home that
        // points the CLI at it. Without them the agent still has the tools.
        let (gateway_tx, mut gateway_rx) = mpsc::unbounded::<GatewayEvent>();
        self.gateway_cards.clear();
        let (cli_home, gateway_serve) = match gateway {
            Ok((mut gateway, home)) => {
                let serve = gateway.serve(
                    self.session.read(cx).gateway_client(cx),
                    Sandbox {
                        graph: graph.clone(),
                        branch: agent_branch,
                        base: agent_base,
                    },
                    gateway_tx,
                );
                (Some(std::sync::Arc::new(home)), Some(serve))
            }
            Err(error) => {
                self.say(
                    format!(
                        "Studio could not set up the omnigraph CLI for this conversation \
                         ({error}), so the agent cannot reach the graph."
                    ),
                    true,
                );
                (None, None)
            }
        };
        // `AGENTS.md` and the skill tell the agent how to use the CLI; without them it can still
        // run it.
        if let Some(error) = rules {
            self.say(
                format!(
                    "Studio could not write AGENTS.md and the omnigraph-studio skill for the \
                     agent ({error}); it can still reach the graph."
                ),
                true,
            );
        }
        self.cwd = Some(cwd.clone());
        let launch = SessionLaunch {
            cwd,
            resume: self.session_id.clone(),
        };
        let (events_tx, mut events_rx) = mpsc::unbounded();
        let (commands_tx, commands_rx) = mpsc::unbounded();
        // One process per agent, shared with the graph's other conversations.
        let connection =
            crate::connection::connection(AgentLaunch::new(settings.command, settings.env), cx);
        if !connection.open(launch, events_tx.clone(), commands_rx) {
            self.fail_start("The agent is not running.".into(), cx);
            return;
        }

        let mut tasks = Vec::new();
        if let Some(gateway_serve) = gateway_serve {
            tasks.push(cx.background_spawn(gateway_serve));
        }
        tasks.push(cx.spawn(async move |this, cx| {
            while let Some(event) = gateway_rx.next().await {
                if this
                    .update(cx, |this, cx| this.apply_gateway(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        if let Some(home) = cli_home.clone() {
            // Where Studio looks for its own CLI, the one the settings name first
            // (`omnigraph_cli`), so the agent and Studio take the same CLI.
            let configured = graph_store::cli::configured();
            tasks.push(cx.spawn(async move |this, cx| {
                let (chosen, passed_over) = cx
                    .background_spawn(async move {
                        let chosen =
                            cli_home::choose_cli(&home, graph_store::cli::candidates()).await;
                        let passed_over = configured.filter(|configured| match &chosen {
                            cli_home::CliChoice::Chosen { cli, .. } => {
                                std::fs::canonicalize(configured).ok()
                                    != std::fs::canonicalize(cli).ok()
                            }
                            _ => false,
                        });
                        (chosen, passed_over)
                    })
                    .await;
                this.update(cx, |this, cx| this.cli_chosen(chosen, passed_over, cx))
                    .ok();
            }));
        }
        let event_pump = cx.spawn(async move |this, cx| {
            while let Some(event) = events_rx.next().await {
                if this
                    .update(cx, |this, cx| this.apply_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        tasks.push(event_pump);
        self.host = Some(Host {
            commands: commands_tx,
            events: events_tx,
            _connection: connection,
            _cli_home: cli_home,
            _tasks: tasks,
        });
        self.enter_starting(cx);
        cx.notify();
    }

    /// The agent starts. One that has not answered [`SLOW_START`] later is said to be slow, with
    /// an offer to start it again; a start already under way keeps its clock.
    fn enter_starting(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, State::Starting) && self._slow_start.is_some() {
            return;
        }
        self.state = State::Starting;
        self.slow_start = false;
        self._slow_start = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SLOW_START).await;
            this.update(cx, |this, cx| {
                if matches!(this.state, State::Starting) {
                    this.slow_start = true;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn fail_start(&mut self, message: String, cx: &mut Context<Self>) {
        self.show_held();
        self.state = State::Exited {
            message,
            stderr: String::new(),
        };
        cx.notify();
    }

    /// Shows a notice of Studio's own in the thread. While the agent comes back to a
    /// conversation it replays, the notice waits with what else was said meanwhile
    /// ([`AgentPanel::said_while_starting`]) until the agent is ready or stopped: landing in
    /// the replay, it would split a replayed message in two.
    fn say(&mut self, text: String, error: bool) {
        let notice = Entry::Notice { text, error };
        if matches!(self.state, State::Starting) && self.session_id.is_some() {
            self.said_while_starting.push(notice);
        } else {
            self.entries.push(notice);
        }
    }

    /// Shows what was said while the agent came back, after the conversation it replayed.
    fn show_held(&mut self) {
        self.replace_on_replay = None;
        self.entries
            .extend(std::mem::take(&mut self.said_while_starting));
    }

    fn stop_host(&mut self) {
        self.host = None;
        self.preparing = None;
        // Options can only be set on a running session.
        self.config_options.clear();
        self.setting_option = None;
        self.options_changed();
        self.entries.cancel_permissions();
        self.entries.stop_writes();
        self.gateway_cards.clear();
    }

    /// Starts the agent again on this conversation, after it stopped: it replays the
    /// conversation when it can load it, else a new one begins in this tab.
    fn restart(&mut self, _: &Restart, _window: &mut Window, cx: &mut Context<Self>) {
        self.stop_host();
        self._slow_start = None;
        self.clear_thread();
        // What the start given up said is said again by this one.
        self.said_while_starting.clear();
        // A conversation nothing was sent in starts in a folder of its own again, as when
        // another agent is chosen: the one it ran in goes.
        self.discard_unused_folder(cx);
        self.start(cx);
    }

    /// Forgets the thread on screen, before the agent shows it again.
    fn clear_thread(&mut self) {
        self.entries.clear();
        self.markdown.clear();
        self.expanded_activity.clear();
        self.folds += 1;
        self.usage = None;
    }

    /// Records the open session, to come back to it at the next start.
    fn remember_session(&self, cx: &mut Context<Self>) {
        // All three are set once the agent was launched, which is before it opens a session.
        let (Some(session), Some(branch), Some(cwd)) = (&self.session_id, &self.branch, &self.cwd)
        else {
            return;
        };
        let now = chrono::Utc::now();
        let record = SessionRecord {
            session: session.clone(),
            graph: self.session.read(cx).overrides_key(),
            agent: self.agent.join(" "),
            title: None,
            title_override: None,
            created_at: now,
            updated_at: now,
            branch: branch.name.clone(),
            branch_created: branch.created,
            pinned: false,
            archived: false,
            cwd: cwd.clone(),
        };
        self.sessions
            .update(cx, |sessions, cx| sessions.upsert(record, cx));
    }

    /// Whether the graph's list holds this conversation: something was sent in it.
    fn is_listed(&self, cx: &App) -> bool {
        self.session_id
            .as_ref()
            .is_some_and(|id| self.sessions.read(cx).get(id).is_some())
    }

    /// Removes the folder of a conversation the list never held: nothing was sent in it, so
    /// nothing in it is kept. Only a folder of its own goes, never the graph's shared one.
    fn discard_unused_folder(&mut self, cx: &mut App) {
        if self.is_listed(cx) {
            return;
        }
        let Some(cwd) = self.cwd.take() else {
            return;
        };
        let graph = crate::sessions::graph_cwd(&self.session.read(cx).overrides_key());
        if cwd.parent() == Some(graph.as_path()) {
            cx.background_executor()
                .spawn(async move {
                    std::fs::remove_dir_all(cwd).ok();
                })
                .detach();
        }
    }

    /// Runs another agent, which becomes the default too, as Zed remembers the agent chosen
    /// last. A conversation nothing was sent in yet changes agent in place; one that holds
    /// messages keeps its agent, and the one chosen starts a new conversation.
    fn choose_agent(&mut self, command: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let default = command.clone();
        studio_settings::update(cx, move |settings| settings.agent.command = default);
        if self.is_listed(cx) {
            window.dispatch_action(Box::new(studio_actions::agent::NewConversation), cx);
            return;
        }
        self.stop_host();
        self._slow_start = None;
        self.clear_thread();
        self.said_while_starting.clear();
        self.discard_unused_folder(cx);
        self.session_id = None;
        self.agent = command;
        self.state = State::Idle;
        self.start(cx);
        cx.notify();
    }

    /// Runs `command` with `model` (its model option and value): remembered for the agent's next
    /// session, which takes it as it opens. Another agent is chosen ([`Self::choose_agent`]);
    /// this conversation's agent switches model when it is running.
    fn choose_model(
        &mut self,
        command: Vec<String>,
        model: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((option, value)) = &model {
            crate::sessions::prefer(&command, option, Value::String(value.clone()));
        }
        if command != self.agent {
            self.choose_agent(command, window, cx);
        } else if let Some((option, value)) = model {
            self.set_option(option, SessionConfigOptionValue::value_id(value), cx);
        }
    }

    /// The name the agent menu gives this conversation's agent.
    fn agent_label(&self, cx: &App) -> String {
        agents(cx)
            .into_iter()
            .find(|(_, command)| *command == self.agent)
            .map_or_else(|| "Agent".to_owned(), |(name, _)| name)
    }

    /// Whether this tab is the one the workspace shows.
    fn is_shown(&self, cx: &Context<Self>) -> bool {
        let this = cx.entity_id();
        self.workspace.upgrade().is_some_and(|workspace| {
            workspace
                .read(cx)
                .active_item()
                .is_some_and(|item| item.item_id() == this)
        })
    }

    /// Tells the graph's list what this conversation is doing.
    fn sync_live(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.session_id.clone() else {
            return;
        };
        let live = if self.pending_index().is_some() {
            Some(Live::Waiting)
        } else if matches!(self.state, State::Prompting { .. }) {
            Some(Live::Running)
        } else if self.failed {
            Some(Live::Failed)
        } else {
            None
        };
        self.sessions
            .update(cx, |sessions, cx| sessions.set_live(&id, live, cx));
    }

    fn apply_event(&mut self, event: HostEvent, cx: &mut Context<Self>) {
        match event {
            HostEvent::Ready {
                agent,
                session,
                opened,
            } => {
                // The agent could not load the conversation asked for and opened a new one on
                // its branch: the record follows the new one, rather than two conversations
                // sharing a branch, and the thread says the agent lacks what came before.
                let mut lost = false;
                if opened == Opened::New
                    && let Some(asked) = self.session_id.take().filter(|asked| *asked != session)
                {
                    let new = session.clone();
                    self.sessions
                        .update(cx, |sessions, cx| sessions.rebind(&asked, new, cx));
                    lost = true;
                }
                self.session_id = Some(session);
                // A new conversation joins the list when something is sent in it.
                if self.is_listed(cx) {
                    self.remember_session(cx);
                }
                cx.emit(ItemEvent::UpdateTab);
                // A loaded conversation replays itself and keeps its own options; a new one
                // starts from the ones chosen last for this agent.
                match opened {
                    Opened::New => {
                        self.apply_preferences();
                        if lost {
                            self.entries.push(Entry::Notice {
                                text: "The agent could not load this conversation, so a new \
                                       one starts on its branch without the earlier messages."
                                    .into(),
                                error: false,
                            });
                        }
                    }
                    Opened::Loaded => {}
                    Opened::Resumed => self.entries.push(Entry::Notice {
                        text: "Resumed: the agent has the earlier messages but cannot show them."
                            .into(),
                        error: false,
                    }),
                }
                self.state = State::Ready { agent };
                self._slow_start = None;
                self.slow_start = false;
                self.show_held();
                // What was sent before the agent was ready goes now: the message on screen as
                // the first turn, the queue after it as turns end.
                match self.opening.take() {
                    Some(text) => {
                        if !self.prompt(text.clone(), cx) {
                            self.opening = Some(text);
                        }
                    }
                    None => self.send_queued(cx),
                }
            }
            HostEvent::Update(update) => {
                // What was said before the replay began waits with what is said during it.
                // Only the conversation itself replays: a title, an option or the usage the
                // agent sends first leaves the thread on screen, which stays if no replay
                // comes.
                if replays(&update)
                    && let Some(shown) = self.replace_on_replay.take()
                {
                    let mut said = self.entries.take_from(shown);
                    said.append(&mut self.said_while_starting);
                    self.said_while_starting = said;
                    self.clear_thread();
                }
                self.apply_update(update, cx)
            }
            HostEvent::ConfigOptions(options) => {
                self.config_options = options;
                self.setting_option = None;
                self.options_changed();
            }
            HostEvent::ConfigOptionFailed { message } => {
                self.setting_option = None;
                self.options_changed();
                self.entries.push(Entry::Notice {
                    text: format!("The agent did not change the option: {message}"),
                    error: true,
                });
            }
            HostEvent::Permission { request, reply } => {
                self.entries.push(Entry::Permission(PermissionCard {
                    title: request
                        .tool_call
                        .fields
                        .title
                        .clone()
                        .unwrap_or_else(|| "The agent asks to use a tool".into()),
                    options: request.options,
                    reply: Some(reply),
                    answer: None,
                }));
            }
            HostEvent::TurnEnded { stop } => {
                // A tool still shown running when the turn ends is not running: the turn
                // finished it, or stopping, failing or leaving cut it short. Either way its
                // spinner stops, so an idle thread never animates.
                let finished = matches!(stop, Ok(StopReason::EndTurn));
                self.settle_open_tools(if finished {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                });
                // Nor does a permission request of the turn still wait: the agent no longer
                // asks it.
                self.entries.cancel_permissions();
                if let State::Prompting { agent } = &self.state {
                    self.state = State::Ready {
                        agent: agent.clone(),
                    };
                }
                if let Some(id) = self.session_id.clone()
                    && !self.is_shown(cx)
                {
                    self.sessions
                        .update(cx, |sessions, cx| sessions.mark_unread(&id, cx));
                }
                match stop {
                    Ok(StopReason::EndTurn) => {}
                    // "Send now" stopped it; the next message says why.
                    Ok(StopReason::Cancelled) if self.queue.after_cancel.is_some() => {}
                    Ok(StopReason::Cancelled) => self.entries.push(Entry::Notice {
                        text: "Stopped.".into(),
                        error: false,
                    }),
                    Ok(other) => self.entries.push(Entry::Notice {
                        text: format!("The agent ended its turn: {other:?}."),
                        error: false,
                    }),
                    Err(error) => self.entries.push(Entry::Notice {
                        text: format!("The turn failed: {error}"),
                        error: true,
                    }),
                }
                self.send_queued(cx);
            }
            HostEvent::Exited {
                message,
                stderr,
                failed,
            } => {
                self.stop_host();
                self.settle_open_tools(ToolCallStatus::Failed);
                self.show_held();
                self.failed = failed;
                self.state = State::Exited { message, stderr };
            }
        }
        // The thread follows the newest reply while it is scrolled to its end (the list's tail
        // mode); a person reading further up is left where they are.
        cx.notify();
    }

    /// Gives every tool still pending or running the status it ended with.
    fn settle_open_tools(&mut self, ended: ToolCallStatus) {
        self.entries.change_each(|entry| match entry {
            Entry::Tool { status, .. }
                if matches!(status, ToolCallStatus::Pending | ToolCallStatus::InProgress) =>
            {
                *status = ended;
                true
            }
            _ => false,
        });
    }

    fn apply_update(&mut self, update: SessionUpdate, cx: &mut Context<Self>) {
        match update {
            // The agent's own title for the conversation.
            SessionUpdate::SessionInfoUpdate(info) => {
                if let (MaybeUndefined::Value(title), Some(id)) = (info.title, &self.session_id)
                    && !title.trim().is_empty()
                {
                    let id = id.clone();
                    self.sessions
                        .update(cx, |sessions, cx| sessions.set_title(&id, title, cx));
                }
            }
            // A resumed session replays the person's own messages too.
            SessionUpdate::UserMessageChunk(chunk) => {
                let text = content_text(&chunk.content);
                match self.entries.last_mut() {
                    Some(Entry::User(existing)) => existing.push_str(&text),
                    _ => self.entries.push(Entry::User(text)),
                }
            }
            SessionUpdate::AgentMessageChunk(chunk) => {
                let text = content_text(&chunk.content);
                match self.entries.last_mut() {
                    Some(Entry::Agent(existing)) => existing.push_str(&text),
                    _ => self.entries.push(Entry::Agent(text)),
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                let text = content_text(&chunk.content);
                match self.entries.last_mut() {
                    Some(Entry::Thought(existing)) => existing.push_str(&text),
                    _ => self.entries.push(Entry::Thought(text)),
                }
            }
            SessionUpdate::ToolCall(call) => {
                self.entries.push(Entry::Tool {
                    id: call.tool_call_id.to_string(),
                    title: call.title,
                    status: call.status,
                });
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let id = update.tool_call_id.to_string();
                let at = self.entries.iter().rposition(
                    |entry| matches!(entry, Entry::Tool { id: existing, .. } if *existing == id),
                );
                if let Some(Entry::Tool { title, status, .. }) =
                    at.and_then(|at| self.entries.get_mut(at))
                {
                    if let Some(new_title) = update.fields.title {
                        *title = new_title;
                    }
                    if let Some(new_status) = update.fields.status {
                        *status = new_status;
                    }
                }
            }
            SessionUpdate::ConfigOptionUpdate(update) => {
                self.config_options = update.config_options;
                self.options_changed();
            }
            SessionUpdate::UsageUpdate(update) => {
                self.usage = Some(Usage {
                    used: update.used,
                    size: update.size,
                    cost: update.cost.map(|cost| (cost.amount, cost.currency)),
                });
            }
            SessionUpdate::Plan(plan) => {
                let entries = plan
                    .entries
                    .into_iter()
                    .map(|entry| (entry.content, entry.status))
                    .collect();
                match self.entries.last_mut() {
                    Some(Entry::Plan(existing)) => *existing = entries,
                    _ => self.entries.push(Entry::Plan(entries)),
                }
            }
            _ => {}
        }
    }

    // ---- session options ----

    /// Sets one of the session's options and remembers the choice for this agent's next new
    /// session.
    fn set_option(&mut self, id: String, value: SessionConfigOptionValue, cx: &mut Context<Self>) {
        let Some(host) = &self.host else {
            return;
        };
        let remembered = match &value {
            SessionConfigOptionValue::Boolean { value } => Value::Bool(*value),
            other => match other.as_value_id() {
                Some(id) => Value::String(id.0.to_string()),
                None => return,
            },
        };
        crate::sessions::prefer(&self.agent, &id, remembered);
        if host
            .commands
            .unbounded_send(HostCommand::SetConfigOption {
                id: id.clone(),
                value: value.clone(),
            })
            .is_ok()
        {
            self.setting_option = Some((id, value));
            self.options_changed();
        }
        cx.notify();
    }

    /// Makes the composer's options again from the agent's and the value being set.
    fn options_changed(&mut self) {
        self.option_views = self
            .config_options
            .iter()
            .filter_map(|option| OptionView::new(option, self.setting_option.as_ref()))
            .map(Rc::new)
            .collect();
        // Kept, so the model menu lists this agent's models while another one runs.
        if let Some(model) = self.option_views.iter().find(|option| option.is_model())
            && let Choice::Select { sections, .. } = &model.choice
        {
            let values = sections.iter().flat_map(|(_, values)| values.clone());
            crate::sessions::remember_models(&self.agent, &model.id, values.collect());
        }
    }

    /// Sets the options chosen last for this agent where the new session differs and still
    /// offers the value.
    fn apply_preferences(&mut self) {
        let preferences = crate::sessions::preferences(&self.agent);
        let Some(host) = &self.host else {
            return;
        };
        for option in &self.config_options {
            let Some(preferred) = preferences.get(option.id.0.as_ref()) else {
                continue;
            };
            let value = match (&option.kind, preferred) {
                (SessionConfigKind::Select(select), Value::String(preferred))
                    if select.current_value.0.as_ref() != preferred
                        && select_values(&select.options).any(|(value, _)| value == preferred) =>
                {
                    SessionConfigOptionValue::value_id(preferred.clone())
                }
                (SessionConfigKind::Boolean(boolean), Value::Bool(preferred))
                    if boolean.current_value != *preferred =>
                {
                    SessionConfigOptionValue::boolean(*preferred)
                }
                _ => continue,
            };
            host.commands
                .unbounded_send(HostCommand::SetConfigOption {
                    id: option.id.0.to_string(),
                    value,
                })
                .ok();
        }
    }

    // ---- prompting ----

    /// Sends `text`, or queues it after the messages waiting before it.
    pub fn send_text(&mut self, text: String, _window: &mut Window, cx: &mut Context<Self>) {
        self.send_or_queue(text, false, cx);
    }

    /// Sends `text` as the next turn when the agent is ready for one. A message sent while the
    /// agent works, starts or is still to be chosen waits its turn in the queue: `first` in
    /// line, as "Send now" asks, or after any sent before it.
    fn send_or_queue(&mut self, text: String, first: bool, cx: &mut Context<Self>) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        match &self.state {
            // One that cannot go (the agent just stopped) waits in the queue, not lost.
            State::Ready { .. } => {
                if !self.prompt(text.clone(), cx) {
                    self.queue.enqueue(text, first);
                }
            }
            State::Prompting { .. } => self.queue.enqueue(text, first),
            State::Starting => self.hold_until_ready(text, first),
            State::Setup => {
                self.hold_until_ready(text, first);
                self.entries.push(Entry::Notice {
                    text: "Choose an agent to send this to.".into(),
                    error: false,
                });
            }
            State::Idle | State::Exited { .. } => {
                self.hold_until_ready(text, first);
                self.replace_on_replay = (!self.entries.is_empty()).then_some(self.entries.len());
                self.start(cx);
            }
        }
        cx.notify();
    }

    /// A message sent before the agent is ready for it. With nothing waiting already, neither an
    /// opening nor messages kept in the queue across a stop, it is the opening
    /// ([`Self::opening`]): shown in the thread at once and sent as soon as the agent is ready.
    /// Any other waits in the queue, behind it, so the messages go in the order they were sent.
    fn hold_until_ready(&mut self, text: String, first: bool) {
        if self.opening.is_none() && !self.queue.holds_messages() {
            self.opening = Some(text);
            self.thread.scroll_to_end();
        } else {
            self.queue.enqueue(text, first);
        }
    }

    /// Sends `text` as the next turn; whether it went.
    fn prompt(&mut self, text: String, cx: &mut Context<Self>) -> bool {
        let Some(host) = &self.host else {
            return false;
        };
        let agent = match &self.state {
            State::Ready { agent } => agent.clone(),
            _ => return false,
        };
        let sent = host
            .commands
            .unbounded_send(HostCommand::Prompt(text.clone()))
            .is_ok();
        if sent {
            if !self.is_listed(cx) {
                self.remember_session(cx);
            }
            if let Some(id) = self.session_id.clone() {
                self.sessions.update(cx, |sessions, cx| {
                    sessions.suggest_title(&id, &text, cx);
                    sessions.touch(&id, cx);
                });
            }
            self.entries.push(Entry::User(text));
            self.state = State::Prompting { agent };
            self.thread.scroll_to_end();
            cx.notify();
        }
        sent
    }

    /// Sends the next queued message as a turn ends; one that cannot go stays first in line.
    fn send_queued(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.queue.on_turn_ended()
            && !self.prompt(text.clone(), cx)
        {
            self.queue.put_back(text);
        }
    }

    fn send(&mut self, _: &Send, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }
        self.composer
            .update(cx, |editor, cx| editor.clear(window, cx));
        // Sending is the person engaging again: a paused queue goes on.
        self.queue.paused = false;
        self.send_text(text, window, cx);
    }

    /// Stops the turn and sends the composer's message as soon as it has stopped.
    fn send_now(&mut self, _: &SendNow, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).text(cx).trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.composer
            .update(cx, |editor, cx| editor.clear(window, cx));
        self.send_at_once(text, cx);
    }

    /// Stops the turn and sends `text` once it has stopped; with no turn running it goes now,
    /// or first when the agent is not ready for it yet.
    fn send_at_once(&mut self, text: String, cx: &mut Context<Self>) {
        if matches!(self.state, State::Prompting { .. })
            && let Some(host) = &self.host
        {
            self.queue.send_after_cancel(text);
            host.commands.unbounded_send(HostCommand::Cancel).ok();
            self.entries.cancel_permissions();
            cx.notify();
        } else {
            self.queue.paused = false;
            self.send_or_queue(text, true, cx);
        }
    }

    /// Sends a queued message at once, stopping the turn if one runs.
    fn send_queued_now(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(text) = self.queue.take(id) {
            self.send_at_once(text, cx);
        }
    }

    /// Puts a queued message back in the composer to edit, ahead of what is there.
    fn edit_queued(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.queue.take(id) {
            self.put_back(text, window, cx);
        }
    }

    /// Takes back the message waiting for the agent to start ([`Self::opening`]): it goes to
    /// the composer, to be changed or not sent at all, and nothing is sent when the agent starts.
    fn edit_opening(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.opening.take() {
            self.put_back(text, window, cx);
        }
    }

    /// Puts an unsent message in the composer, ahead of what is there.
    fn put_back(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |editor, cx| {
            let draft = editor.text(cx);
            let text = if draft.trim().is_empty() {
                text
            } else {
                format!("{text}\n{draft}")
            };
            editor.set_text(text, window, cx);
            editor.move_to_end(&Default::default(), window, cx);
        });
        window.focus(&self.composer.focus_handle(cx), cx);
        cx.notify();
    }

    fn clear_queue(&mut self, _: &ClearQueue, _window: &mut Window, cx: &mut Context<Self>) {
        self.queue.entries.clear();
        cx.notify();
    }

    /// Stops the turn and answers its waiting permission requests; the queue waits until the
    /// person sends again.
    fn stop(&mut self, _: &Stop, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(host) = &self.host {
            host.commands.unbounded_send(HostCommand::Cancel).ok();
            self.entries.cancel_permissions();
            if !self.queue.entries.is_empty() {
                self.queue.paused = true;
            }
            cx.notify();
        }
    }

    // ---- approvals ----

    /// The oldest card still waiting for the person.
    fn pending_index(&self) -> Option<usize> {
        self.entries.first_waiting(|_| true)
    }

    /// What closing the tab would drop: a draft in the composer, a message waiting its turn,
    /// or a card waiting for the person's answer.
    fn holds_work(&self) -> bool {
        self.has_draft
            || self.opening.is_some()
            || self.queue.holds_messages()
            || self.pending_index().is_some()
    }

    /// Answers the write card at `index`, which its buttons and keys do.
    fn decide(&mut self, index: usize, decision: Decision, cx: &mut Context<Self>) {
        if let Some(Entry::Write(card)) = self.entries.get_mut(index)
            && let WriteState::Waiting(_) = card.state
            && let WriteState::Waiting(reply) =
                std::mem::replace(&mut card.state, WriteState::Running)
        {
            reply.send(decision).ok();
        }
        cx.notify();
    }

    fn choose_option(&mut self, index: usize, option_index: usize, cx: &mut Context<Self>) {
        if let Some(Entry::Permission(card)) = self.entries.get_mut(index)
            && let (Some(reply), Some(option)) = (card.reply.take(), card.options.get(option_index))
        {
            card.answer = Some(option.name.clone());
            reply
                .send(RequestPermissionOutcome::Selected(
                    SelectedPermissionOutcome::new(option.option_id.clone()),
                ))
                .ok();
        }
        cx.notify();
    }

    fn allow(&mut self, _: &Allow, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.entries.keyed_write() {
            self.decide(index, Decision::Allow, cx);
        }
    }

    fn allow_always(&mut self, _: &AllowAlways, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.entries.keyed_write() {
            self.decide(index, Decision::AllowAlways, cx);
        }
    }

    fn reject(&mut self, _: &Reject, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.entries.keyed_write() {
            self.decide(index, Decision::Reject, cx);
        }
    }

    fn apply_gateway(&mut self, event: GatewayEvent, cx: &mut Context<Self>) {
        match event {
            GatewayEvent::Write { id, write, decide } => self.gateway_write(id, write, decide, cx),
            GatewayEvent::TokenRefused => self
                .session
                .update(cx, |session, cx| session.token_refused(cx)),
            GatewayEvent::Abandoned { id } => {
                if let Some(card) = self.gateway_cards.remove(&id) {
                    self.set_write_state(
                        card,
                        WriteState::Refused(
                            "Not sent: the CLI stopped waiting before you decided.".into(),
                        ),
                        cx,
                    );
                }
            }
        }
    }

    /// A write the agent's CLI sent to its own branch (spec 013): a card in the thread. It goes
    /// through at once, or waits for Allow when the person asked to approve each write.
    fn gateway_write(
        &mut self,
        id: u64,
        write: WriteSummary,
        decide: oneshot::Sender<Option<OnOutcome>>,
        cx: &mut Context<Self>,
    ) {
        let Some(branch) = &self.branch else {
            decide.send(None).ok();
            return;
        };
        let ask = StudioSettings::get(cx).agent.ask_before_writes && !self.always_allow;
        let card = self.next_card;
        self.next_card += 1;
        let (state, waiting) = if ask {
            let (reply, decision) = oneshot::channel();
            (WriteState::Waiting(reply), Some(decision))
        } else {
            (WriteState::Running, None)
        };
        self.entries.push(Entry::Write(WriteCard {
            id: card,
            summary: write.summary.clone(),
            query: write.query.clone().unwrap_or_default(),
            params: write.params.clone(),
            branch: branch.name.clone(),
            // A load given `from` is marked new only once its answer says it made the branch.
            creates_from: (write.creates_branch == Creates::Always).then(|| branch.base.clone()),
            state,
        }));
        self.gateway_cards.insert(id, card);
        let sent = SentWrite {
            card,
            kind: write.kind,
            creates: write.creates_branch,
            branch: branch.name.clone(),
            base: branch.base.clone(),
        };
        let description = format!("Agent CLI: {}", write.summary);
        match waiting {
            None => {
                let approval = self.record_outcome(sent, description, cx);
                hand_over(decide, Some(approval));
            }
            Some(decision) => {
                cx.spawn(async move |this, cx| {
                    // No decision when the card was settled otherwise (the CLI gave up): the
                    // write is not sent, and the card keeps what it says.
                    let decision = decision.await.ok();
                    let allow = matches!(decision, Some(Decision::Allow | Decision::AllowAlways));
                    let approval = this
                        .update(cx, |this, cx| {
                            if let Some(Decision::AllowAlways) = decision {
                                this.always_allow = true;
                            }
                            if let Some(Decision::Reject) = decision {
                                // Nothing is sent, so no outcome comes for the card.
                                this.gateway_cards.remove(&id);
                                this.set_write_state(sent.card, WriteState::Rejected, cx);
                            }
                            allow.then(|| this.record_outcome(sent, description, cx))
                        })
                        .ok()
                        .flatten();
                    hand_over(decide, approval);
                })
                .detach();
            }
        }
        self.thread.scroll_to_end();
        cx.notify();
    }

    /// What records a write the person let the agent's CLI send, from the moment it is sent:
    /// the session counts it among the writes in flight and logs its outcome with Studio's own,
    /// and the write's card and the agent's branch learn how it ended. The gateway runs a sent
    /// write to its end though the agent stops or the conversation restarts meanwhile, so the
    /// outcome reaches them then too; with the tab closed, the conversation's record still
    /// learns whether the branch exists.
    fn record_outcome(
        &self,
        sent: SentWrite,
        description: String,
        cx: &mut Context<Self>,
    ) -> OnOutcome {
        // A write that makes or deletes the agent's branch reads the branch list again, and so
        // does a load that may make it: the session takes that from the request, as only the
        // session can drop the history it holds for a branch made again under its name.
        let moves_branches = sent.creates != Creates::Never || sent.kind == WriteKind::BranchDelete;
        let write = self.session.update(cx, |session, cx| {
            session.external_write(sent.branch.clone(), description, moves_branches, cx)
        });
        let (heard, outcome) = oneshot::channel::<Seen>();
        let (sessions, conversation) = (self.sessions.clone(), self.session_id.clone());
        cx.spawn(async move |this, cx| {
            // Dropped unheard only as Studio quits with the write on its way, which the session
            // records as unknown too.
            let seen = outcome.await.unwrap_or_else(|_| {
                Seen::Unknown("Studio stopped before the server answered.".into())
            });
            let shown = this.update(cx, |this, cx| this.write_finished(&sent, &seen, cx));
            if shown.is_err()
                && let (Some(conversation), Some(exists)) =
                    (conversation, branch_after(sent.kind, &seen))
            {
                sessions.update(cx, |sessions, cx| {
                    sessions.set_branch_created(&conversation, &sent.branch, exists, cx)
                });
            }
        })
        .detach();
        Box::new(move |seen: &Seen| {
            write.finish(outcome_kind(seen));
            heard.send(seen.clone()).ok();
        })
    }

    /// A write the agent's CLI sent has ended: its card shows how, while the thread holds it,
    /// and the agent's branch learns whether it exists now and what it holds, while this tab
    /// still writes to it (a tab that started a new conversation with another agent writes to
    /// another branch).
    fn write_finished(&mut self, sent: &SentWrite, seen: &Seen, cx: &mut Context<Self>) {
        let made_branch = makes_branch(sent.creates, seen);
        if let Some(card) = self.entries.write_card(sent.card) {
            if made_branch {
                card.creates_from = Some(sent.base.clone());
            }
            card.state = card_state(seen, made_branch);
        }
        if let Some(branch) = self
            .branch
            .as_mut()
            .filter(|branch| branch.name == sent.branch)
        {
            let was = branch.created;
            if let Some(exists) = branch_after(sent.kind, seen) {
                branch.created = exists;
                // Deleted, there is nothing to review, and the next write makes it again.
                if !exists {
                    branch.changes = 0;
                }
            }
            if let Seen::Written {
                commit: Some((_, CommitKind::Published)),
                ..
            } = seen
            {
                branch.changes += 1;
            }
            if branch.created != was {
                self.remember_session(cx);
            }
        }
        cx.notify();
    }

    /// Studio found (or did not find) a CLI that speaks the graph's API for this conversation.
    /// The configured CLI it passed over for another (`passed_over`), every CLI it found when
    /// none fits, and one the agent's shell cannot run through the wrapper, are named.
    fn cli_chosen(
        &mut self,
        choice: cli_home::CliChoice,
        passed_over: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let contract = graph_client::CONTRACT;
        match choice {
            cli_home::CliChoice::Chosen {
                cli,
                version,
                wrapper,
            } => {
                if let Some(configured) = passed_over {
                    self.say(
                        format!(
                            "`omnigraph_cli` ({}) does not speak this graph's HTTP API \
                             ({contract}) with Omnigraph's version-1 operator configuration, so \
                             the agent uses {} ({version}).",
                            configured.display(),
                            cli.display(),
                        ),
                        false,
                    );
                }
                if !wrapper {
                    self.say(
                        format!(
                            "The agent cannot run {}: its path is not plain ASCII, which a \
                             batch file cannot hold. Set `omnigraph_cli` in Studio's settings to \
                             a CLI at a plain ASCII path.",
                            cli.display()
                        ),
                        false,
                    );
                }
            }
            refused => {
                let why = refused.refusal().unwrap_or_default();
                self.say(
                    format!("{why} The agent cannot reach the graph until one is."),
                    false,
                );
            }
        }
        cx.notify();
    }

    fn set_write_state(&mut self, card: u64, state: WriteState, cx: &mut Context<Self>) {
        if let Some(card) = self.entries.write_card(card) {
            card.state = state;
        }
        cx.notify();
    }

    fn review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(branch) = self.branch.as_ref().filter(|b| b.offers_review()) else {
            return;
        };
        let name = branch.name.clone();
        self.session
            .update(cx, |session, cx| session.set_branch(name, cx));
        window.dispatch_action(Box::new(ReviewBranch), cx);
    }

    fn use_command(&mut self, command: Vec<String>, cx: &mut Context<Self>) {
        self.agent = command.clone();
        studio_settings::update(cx, move |settings| settings.agent.command = command);
        self.sync_setup_state(cx);
        // A question asked before an agent was chosen is sent to it now, as the setup said.
        if matches!(self.state, State::Idle) && !self.queue.entries.is_empty() {
            self.start(cx);
        }
        cx.notify();
    }
}

/// One of the agent's session options as the composer shows it, with a value chosen but not yet
/// confirmed standing in for the agent's.
#[derive(Clone)]
struct OptionView {
    id: String,
    name: String,
    description: Option<SharedString>,
    category: Option<SessionConfigOptionCategory>,
    /// A choice was sent and the agent has not answered yet.
    pending: bool,
    choice: Choice,
}

#[derive(Clone)]
enum Choice {
    /// The current value, its name, and the menu's sections: the agent's groups, or one
    /// section under the option's name.
    Select {
        current: String,
        current_name: String,
        sections: Vec<(String, Vec<(String, String)>)>,
    },
    Switch(bool),
}

impl OptionView {
    fn new(
        option: &SessionConfigOption,
        setting: Option<&(String, SessionConfigOptionValue)>,
    ) -> Option<Self> {
        let id = option.id.0.to_string();
        let pending = setting
            .filter(|(pending, _)| *pending == id)
            .map(|(_, value)| value);
        let choice = match &option.kind {
            SessionConfigKind::Select(select) => {
                let current = pending
                    .and_then(SessionConfigOptionValue::as_value_id)
                    .map(|value| value.0.to_string())
                    .unwrap_or_else(|| select.current_value.0.to_string());
                let current_name = select_values(&select.options)
                    .find(|(value, _)| *value == current)
                    .map(|(_, name)| name.to_owned())
                    .unwrap_or_else(|| current.clone());
                let sections = match &select.options {
                    SessionConfigSelectOptions::Grouped(groups) => groups
                        .iter()
                        .map(|group| {
                            (
                                group.name.clone(),
                                group
                                    .options
                                    .iter()
                                    .map(|o| (o.value.0.to_string(), o.name.clone()))
                                    .collect(),
                            )
                        })
                        .collect(),
                    options => vec![(
                        option.name.clone(),
                        select_values(options)
                            .map(|(value, name)| (value.to_owned(), name.to_owned()))
                            .collect(),
                    )],
                };
                Choice::Select {
                    current,
                    current_name,
                    sections,
                }
            }
            SessionConfigKind::Boolean(boolean) => Choice::Switch(match pending {
                Some(SessionConfigOptionValue::Boolean { value }) => *value,
                _ => boolean.current_value,
            }),
            _ => return None,
        };
        Some(Self {
            id,
            name: option.name.clone(),
            description: option.description.clone().map(SharedString::from),
            category: option.category.clone(),
            pending: pending.is_some(),
            choice,
        })
    }

    /// The icon a composer picker shows before the value, or `None` for an option that belongs
    /// under More options. The model's icon is its provider's, chosen by the caller.
    fn picker_icon(&self) -> Option<IconName> {
        let Choice::Select { .. } = &self.choice else {
            return None;
        };
        match self.category.as_ref()? {
            SessionConfigOptionCategory::Mode => Some(IconName::UserCheck),
            SessionConfigOptionCategory::Model => Some(IconName::ZedAssistant),
            SessionConfigOptionCategory::ThoughtLevel => Some(IconName::ThinkingMode),
            _ => None,
        }
    }

    fn is_model(&self) -> bool {
        matches!(self.category, Some(SessionConfigOptionCategory::Model))
    }

    fn current_name(&self) -> String {
        match &self.choice {
            Choice::Select { current_name, .. } => current_name.clone(),
            Choice::Switch(on) => if *on { "On" } else { "Off" }.to_owned(),
        }
    }

    /// "Fast mode: Off", for tooltips.
    fn summary(&self) -> String {
        format!("{}: {}", self.name, self.current_name())
    }

    /// The option's entries in a menu: its values with the current one checked, or one entry
    /// that switches it.
    /// A select option's values in the menu, the current one checked; another is set when chosen.
    fn add_values(
        &self,
        mut menu: ContextMenu,
        values: &[(String, String)],
        current: &str,
        panel: &WeakEntity<AgentPanel>,
    ) -> ContextMenu {
        for (value, name) in values {
            let checked = value == current;
            let (panel, id, value) = (panel.clone(), self.id.clone(), value.clone());
            menu = menu.toggleable_entry(
                name.clone(),
                checked,
                IconPosition::Start,
                None,
                move |_, cx| {
                    if checked {
                        return;
                    }
                    panel
                        .update(cx, |panel, cx| {
                            panel.set_option(
                                id.clone(),
                                SessionConfigOptionValue::value_id(value.clone()),
                                cx,
                            )
                        })
                        .ok();
                },
            );
        }
        menu
    }

    fn add_to(&self, mut menu: ContextMenu, panel: &WeakEntity<AgentPanel>) -> ContextMenu {
        match &self.choice {
            Choice::Select {
                current, sections, ..
            } => {
                for (index, (header, values)) in sections.iter().enumerate() {
                    if index > 0 {
                        menu = menu.separator();
                    }
                    menu = self.add_values(menu.header(header.clone()), values, current, panel);
                }
                menu
            }
            Choice::Switch(on) => {
                let (panel, id, next) = (panel.clone(), self.id.clone(), !on);
                menu.toggleable_entry(
                    self.name.clone(),
                    *on,
                    IconPosition::Start,
                    None,
                    move |_, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.set_option(
                                    id.clone(),
                                    SessionConfigOptionValue::boolean(next),
                                    cx,
                                )
                            })
                            .ok();
                    },
                )
            }
        }
    }
}

/// A composer picker: the option's icon and current value, borderless, opening its values. The
/// model's menu also holds the agent: a new conversation, and choosing another agent.
fn option_picker(
    option: Rc<OptionView>,
    icon: Icon,
    panel: &WeakEntity<AgentPanel>,
    agents: Option<AgentMenu>,
) -> AnyElement {
    let id = option.id.clone();
    let title = option.summary();
    let meta = option.description.clone();
    let panel = panel.clone();
    PopoverMenu::new(SharedString::from(format!("option-{id}")))
        .trigger(
            ds::button(
                SharedString::from(format!("option-{id}-button")),
                studio_ui::text::cut(&option.current_name(), OPTION_NAME_CHARS),
            )
            .style(ButtonStyle::Subtle)
            .color(Color::Muted)
            .disabled(option.pending)
            .start_icon(icon)
            .tooltip(move |_, cx| match &meta {
                Some(meta) => Tooltip::with_meta(title.clone(), None, meta.clone(), cx),
                None => Tooltip::simple(title.clone(), cx),
            }),
        )
        .menu(move |window, cx| {
            let option = option.clone();
            let panel = panel.clone();
            let agents = agents.clone();
            Some(ContextMenu::build(
                window,
                cx,
                move |menu, _, _| match &agents {
                    Some(agents) => agents.add_to(menu, &panel),
                    None => option.add_to(menu, &panel),
                },
            ))
        })
        .anchor(gpui::Anchor::BottomRight)
        .into_any_element()
}

/// The agents a conversation can run: the presets, then the configured agent when it is none
/// of them.
fn agents(cx: &App) -> Vec<(String, Vec<String>)> {
    let mut agents: Vec<(String, Vec<String>)> = PRESETS
        .iter()
        .map(|(name, command)| {
            let command = command.iter().map(|word| (*word).to_owned()).collect();
            ((*name).to_owned(), command)
        })
        .collect();
    let configured = &StudioSettings::get(cx).agent.command;
    if !configured.is_empty() && !agents.iter().any(|(_, command)| command == configured) {
        let name = crate::sessions_panel::agent_name(&configured.join(" "));
        agents.push((name, configured.clone()));
    }
    agents
}

/// The command of the agent a conversation was held with, else the default agent's.
fn agent_for(session_id: Option<&str>, sessions: &Entity<AgentSessions>, cx: &App) -> Vec<String> {
    let held = session_id.and_then(|id| Some(sessions.read(cx).get(id)?.agent.clone()));
    let Some(held) = held else {
        return StudioSettings::get(cx).agent.command.clone();
    };
    agents(cx)
        .into_iter()
        .map(|(_, command)| command)
        .find(|command| command.join(" ") == held)
        .unwrap_or_else(|| held.split_whitespace().map(str::to_owned).collect())
}

/// The model menu, or the menu in its place before the agent offers a model: a section of
/// models per agent. The running agent's are its model option's, the current one checked; another
/// agent's are the ones it offered when it last ran, or its default before it ever has. A model
/// of another agent runs that agent with it ([`AgentPanel::choose_model`]).
#[derive(Clone)]
struct AgentMenu {
    agents: Vec<(String, Vec<String>)>,
    current: Vec<String>,
    /// The running agent's model option.
    model: Option<Rc<OptionView>>,
}

impl AgentMenu {
    fn add_to(&self, mut menu: ContextMenu, panel: &WeakEntity<AgentPanel>) -> ContextMenu {
        for (index, (name, command)) in self.agents.iter().enumerate() {
            if index > 0 {
                menu = menu.separator();
            }
            menu = menu.header(name.clone());
            let live = self.model.as_ref().filter(|_| *command == self.current);
            if let Some(model) = live
                && let Choice::Select {
                    current, sections, ..
                } = &model.choice
            {
                for (_, values) in sections {
                    menu = model.add_values(menu, values, current, panel);
                }
                continue;
            }
            let models = crate::sessions::models(command)
                .map(|(option, values)| {
                    values
                        .into_iter()
                        .map(|(value, name)| (name, Some((option.clone(), value))))
                        .collect()
                })
                .filter(|models: &Vec<_>| !models.is_empty())
                .unwrap_or_else(|| vec![("Default model".to_owned(), None)]);
            for (label, model) in models {
                let (panel, command) = (panel.clone(), command.clone());
                // Unchecked, in the checked column, so every section's models line up.
                menu = menu.toggleable_entry(
                    label,
                    false,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.choose_model(command.clone(), model.clone(), window, cx)
                            })
                            .ok();
                    },
                );
            }
        }
        menu.separator()
            .entry("Custom agent…", None, |window, cx| {
                window.dispatch_action(Box::new(studio_actions::workspace::OpenSettings), cx)
            })
            .separator()
            .entry("New conversation", None, |window, cx| {
                window.dispatch_action(Box::new(studio_actions::agent::NewConversation), cx)
            })
    }
}

/// The model provider's mark, when the agent or model names one.
fn provider_icon(names: &str) -> Option<IconName> {
    let names = names.to_lowercase();
    let any = |words: &[&str]| words.iter().any(|word| names.contains(word));
    if any(&["claude", "anthropic", "opus", "sonnet", "haiku"]) {
        Some(IconName::AiAnthropic)
    } else if any(&["codex", "openai", "gpt"]) {
        Some(IconName::AiOpenAi)
    } else if any(&["gemini"]) {
        Some(IconName::AiGemini)
    } else {
        None
    }
}

/// A select option's values and names, across its groups.
fn select_values(
    options: &SessionConfigSelectOptions,
) -> Box<dyn Iterator<Item = (&str, &str)> + '_> {
    match options {
        SessionConfigSelectOptions::Ungrouped(options) => Box::new(
            options
                .iter()
                .map(|option| (option.value.0.as_ref(), option.name.as_str())),
        ),
        SessionConfigSelectOptions::Grouped(groups) => Box::new(groups.iter().flat_map(|group| {
            group
                .options
                .iter()
                .map(|option| (option.value.0.as_ref(), option.name.as_str()))
        })),
        _ => Box::new(std::iter::empty()),
    }
}

/// The longest value name a composer picker shows before it is cut.
const OPTION_NAME_CHARS: usize = 32;
/// How full the context window is when the composer's ring turns to the warning colour and its
/// notice strip says so.
const CONTEXT_NEARLY_FULL: f32 = 0.85;
/// The longest conversation title a tab shows.
const TAB_TITLE_CHARS: usize = 28;

/// Token counts as people read them: 999, 1.2k, 84k, 1.2M.
fn humanize_tokens(count: u64) -> String {
    match count {
        0..=999 => count.to_string(),
        1_000..=9_999 => trim_point(format!("{:.1}", count as f64 / 1_000.)) + "k",
        10_000..=999_999 => format!("{}k", (count as f64 / 1_000.).round() as u64),
        _ => trim_point(format!("{:.1}", count as f64 / 1_000_000.)) + "M",
    }
}

fn trim_point(number: String) -> String {
    number
        .strip_suffix(".0")
        .map(str::to_owned)
        .unwrap_or(number)
}

/// A session's cost: four decimals under a cent, else two.
fn format_cost(amount: f64, currency: &str) -> String {
    let precision = if amount > 0. && amount < 0.01 { 4 } else { 2 };
    format!("{amount:.precision$} {currency}")
}

/// Whether `update` is part of the conversation an agent replays when it loads one: its
/// messages, thoughts, tool calls and plan.
fn replays(update: &SessionUpdate) -> bool {
    matches!(
        update,
        SessionUpdate::UserMessageChunk(_)
            | SessionUpdate::AgentMessageChunk(_)
            | SessionUpdate::AgentThoughtChunk(_)
            | SessionUpdate::ToolCall(_)
            | SessionUpdate::ToolCallUpdate(_)
            | SessionUpdate::Plan(_)
    )
}

/// The last reply of each finished turn in `entries`: a turn runs from a message of the
/// person's to the next, and the last one has finished unless the agent is still `working` on
/// it. A turn without a reply has none.
fn turn_ends(entries: &[Entry], working: bool) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut last_reply = None;
    for (index, entry) in entries.iter().enumerate() {
        match entry {
            Entry::User(_) => ends.extend(last_reply.take()),
            Entry::Agent(_) => last_reply = Some(index),
            _ => {}
        }
    }
    if !working {
        ends.extend(last_reply);
    }
    ends
}

fn content_text(content: &ContentBlock) -> String {
    match content {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::ResourceLink(link) => format!("[{}]({})", link.name, link.uri),
        _ => String::new(),
    }
}

// ---- rendering ----
//
// Layout, not sentences (spec 002): Delta's thread, a gutter icon beside each entry and one-line
// activity rows; Zed's tool cards, a tinted header and a footer of icon buttons with key caps.
// Explanations go in tooltips.

/// Width of the thread's gutter, where each entry's icon sits.
const GUTTER: f32 = ds::size::GUTTER;

/// A thread row: an icon in the gutter, the content beside it.
fn gutter_row(icon: Option<Icon>, content: impl IntoElement) -> Div {
    h_flex()
        .w_full()
        .items_start()
        .gap(px(ds::space::XS))
        .child(
            div()
                .w(px(GUTTER))
                .flex_none()
                // Centers a small icon on the first line of `text_sm` content.
                .pt(px(ds::space::XS))
                .flex()
                .justify_center()
                .children(icon),
        )
        .child(div().flex_1().min_w_0().child(content))
}

/// The thread's blocks for `entries`, made again from the turn the first new or changed entry,
/// `changed`, falls in; the turns before keep their blocks from `shown`, which was made from
/// the entries as they still are there. Returns the blocks and how many were kept.
///
/// That holds because a turn's blocks read only its own entries and whether a message of the
/// person's follows it: no block spans a message of the person's, and only the last turn can
/// still be under way. The person's last
/// message before `changed` is unchanged and was there before, so every turn before it was
/// followed by it then and now.
fn thread_blocks(
    shown: &[Block],
    entries: &[Entry],
    changed: usize,
    working: bool,
    waiting: bool,
    exited: bool,
) -> (Vec<Block>, usize) {
    let cut = entries[..changed.min(entries.len())]
        .iter()
        .rposition(|entry| matches!(entry, Entry::User(_)))
        .unwrap_or(0);
    let kept = shown
        .iter()
        .take_while(|block| match block {
            Block::Entry { index, .. } => *index < cut,
            Block::Activity { start, .. } => *start < cut,
            Block::Waiting | Block::Exited => false,
        })
        .count();
    let mut blocks = shown[..kept].to_vec();
    blocks.extend(blocks_from(entries, cut, working));
    if waiting {
        blocks.push(Block::Waiting);
    }
    if exited {
        blocks.push(Block::Exited);
    }
    (blocks, kept)
}

/// The blocks of `entries` from `start` on, where a turn begins (a message of the person's, or
/// the first entry). As in Delta: the agent's mark once per turn, and each run of tool calls
/// and thoughts as one line that opens to its steps.
fn blocks_from(entries: &[Entry], start: usize, working: bool) -> Vec<Block> {
    let ends: std::collections::HashSet<usize> = turn_ends(&entries[start..], working)
        .into_iter()
        .map(|end| start + end)
        .collect();
    let mut blocks = Vec::new();
    let mut marked = false;
    let mut index = start;
    while index < entries.len() {
        let entry = &entries[index];
        match entry {
            Entry::Tool { .. } | Entry::Thought(_) => {
                let start = index;
                while index < entries.len()
                    && matches!(entries[index], Entry::Tool { .. } | Entry::Thought(_))
                {
                    index += 1;
                }
                blocks.push(Block::Activity {
                    start,
                    end: index,
                    lead: !marked,
                });
                marked = true;
                continue;
            }
            Entry::User(_) => marked = false,
            _ => {}
        }
        let lead = matches!(entry, Entry::Agent(_)) && !marked;
        if matches!(entry, Entry::Agent(_)) {
            marked = true;
        }
        blocks.push(Block::Entry {
            index,
            lead,
            ends: ends.contains(&index),
        });
        index += 1;
    }
    blocks
}

/// A run's summary, as Delta writes it: "Thought 1 time, used 2 tools".
fn activity_summary(thoughts: usize, tools: usize) -> String {
    let mut parts = Vec::new();
    if thoughts > 0 {
        parts.push(format!(
            "thought {}",
            studio_ui::plural(thoughts, "time", "times")
        ));
    }
    if tools > 0 {
        parts.push(format!(
            "used {}",
            studio_ui::plural(tools, "tool", "tools")
        ));
    }
    let text = parts.join(", ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

impl Render for AgentPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.start_owed) {
            cx.defer_in(window, |this, _, cx| this.start_if_idle(cx));
        }
        let editor_bg = ds::surface::document(cx);
        let body = match &self.state {
            State::Setup => self.render_setup(cx).into_any_element(),
            _ => {
                let key = self.thread_key();
                if self.synced_thread != Some(key) {
                    let changed = self.entries.rehash();
                    self.sync_markdown(changed, cx);
                    self.sync_thread(changed);
                    self.synced_thread = Some(key);
                }
                self.render_thread(window, cx).into_any_element()
            }
        };
        let (graph, branch) = {
            let session = self.session.read(cx);
            (session.graph_id.clone(), session.branch.clone())
        };
        // The composer, after the reference chat's: a card holding the message and the
        // controls that send it, under the thread's text column with the card's inset outside
        // it; what holds the conversation up or follows from it on a strip attached to its top;
        // and under it, what the agent reaches.
        let card = ds::composer_card(cx)
            .child(div().prose().child(self.composer.clone()))
            .child(self.render_composer_controls(cx));
        let notices = self.render_notices(cx);
        let reach = div().w_full().flex().justify_center().child(ds::meta_row([
            ds::text::meta(format!("Reads {graph} at {branch}"))
                .single_line()
                .into_any_element(),
            ds::text::meta("writes only to a branch of its own")
                .single_line()
                .into_any_element(),
        ]));
        let composer = v_flex()
            .id("composer")
            .key_context("AgentComposer")
            .flex_none()
            .pl(px(ds::space::MD + GUTTER + ds::space::XS - ds::space::LG))
            .pr(px(ds::space::XL))
            .pt(px(ds::space::SM))
            .pb(px(ds::space::SM))
            .gap(px(ds::space::SM))
            .children(self.render_queue(cx))
            .child(match notices {
                Some(strip) => ds::attach_notice(strip, card, cx),
                None => card,
            })
            .child(reach);

        v_flex()
            .key_context("AgentPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::send))
            .on_action(cx.listener(Self::send_now))
            .on_action(cx.listener(Self::clear_queue))
            .on_action(cx.listener(Self::stop))
            .on_action(cx.listener(Self::allow))
            .on_action(cx.listener(Self::allow_always))
            .on_action(cx.listener(Self::reject))
            .on_action(cx.listener(Self::restart))
            .size_full()
            .bg(editor_bg)
            .child(body)
            .child(composer)
    }
}

impl AgentPanel {
    /// The composer's controls under the prompt, as Delta's composer footer and Zed's message
    /// editor keep them with the text they send: the agent at work, how full its context is,
    /// its options (mode, effort, more), the model it runs, then send or stop. Send fills in
    /// only when there is something to send. They are the composer's, not the status bar's,
    /// which holds facts about the open tab.
    fn render_composer_controls(&self, cx: &mut Context<Self>) -> Div {
        let running = matches!(self.state, State::Prompting { .. });
        let can_send = matches!(
            self.state,
            State::Ready { .. } | State::Idle | State::Exited { .. } | State::Starting
        ) && self.has_draft;
        let send_or_stop = if running {
            ds::composer_button("stop", IconName::Stop, false, cx)
                .tooltip(|_, cx| Tooltip::for_action("Stop", &Stop, cx))
                .on_click(cx.listener(|this, _, window, cx| this.stop(&Stop, window, cx)))
        } else {
            ds::composer_button("send", IconName::Send, can_send, cx)
                .tooltip(|_, cx| Tooltip::for_action("Send", &Send, cx))
                .on_click(cx.listener(|this, _, window, cx| this.send(&Send, window, cx)))
        };
        let mut end = Vec::new();
        if running {
            end.push(
                h_flex()
                    .gap(px(ds::space::XS))
                    .items_center()
                    .child(studio_ui::ds::spinner())
                    .child(ds::text::meta("Working"))
                    .into_any_element(),
            );
        }
        end.extend(self.render_usage(cx));
        end.extend(self.render_options(cx));
        // A slow start too: an agent that never answers (a program waiting on its input, one
        // without its ACP flag) is not waited for forever. A start that is merely under way
        // offers nothing to cancel it.
        if matches!(self.state, State::Exited { .. })
            || (matches!(self.state, State::Starting) && self.slow_start)
        {
            end.push(
                ds::icon_button("restart", IconName::RotateCw)
                    .icon_color(Color::Muted)
                    .tooltip(|_, cx| Tooltip::for_action("Start the agent again", &Restart, cx))
                    .on_click(cx.listener(|this, _, window, cx| this.restart(&Restart, window, cx)))
                    .into_any_element(),
            );
        }
        end.push(send_or_stop.into_any_element());
        ds::composer_controls().children(end)
    }

    /// The agent's name, a dot for its state and what the state means.
    fn status(&self) -> (String, Color, &'static str) {
        match &self.state {
            State::Setup => ("Agent".to_owned(), Color::Muted, "No agent chosen"),
            State::Idle => ("Agent".to_owned(), Color::Muted, "Not running"),
            State::Starting => ("Agent".to_owned(), Color::Warning, "Starting"),
            State::Ready { agent } => (agent.clone(), Color::Success, "Ready"),
            State::Prompting { agent } => (agent.clone(), Color::Accent, "Working"),
            State::Exited { .. } => ("Agent".to_owned(), Color::Error, "Stopped"),
        }
    }

    fn render_setup(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("agent-setup")
            .overflow_y_scroll()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .p(px(ds::space::XXL))
            .child(
                v_flex()
                    .w(px(ds::size::FIELD_WIDE))
                    // Outlined choices a step apart, so their edges and shadows do not touch.
                    .gap(px(ds::space::MD))
                    .child(
                        h_flex()
                            .gap(px(ds::space::SM))
                            .mb(px(ds::space::MD))
                            .justify_center()
                            .child(studio_ui::ds::text::state("Choose an agent"))
                            .child(
                                div()
                                    .id("agent-info")
                                    .tooltip(Tooltip::text(
                                        "Studio runs the agent on this computer over the Agent \
                                         Client Protocol and gives it tools for this graph. It \
                                         never sees your token, and its changes land on a branch \
                                         of its own, for you to review.",
                                    ))
                                    .child(studio_ui::ds::icon::meta(IconName::Info)),
                            ),
                    )
                    // A question asked before an agent was chosen waits, and says so.
                    .children(
                        self.queue
                            .entries
                            .first()
                            .map(|queued| queued.text.clone())
                            .map(|question| {
                                div()
                                    .mb(px(ds::space::MD))
                                    .child(studio_ui::ds::text::muted(format!(
                                        "“{question}” is sent once you choose an agent."
                                    )))
                            }),
                    )
                    .children(PRESETS.iter().enumerate().map(|(index, (name, command))| {
                        let command: Vec<String> =
                            command.iter().map(|s| (*s).to_owned()).collect();
                        let shown = command.join(" ");
                        studio_ui::ds::button(("preset", index), *name)
                            .full_width()
                            .style(ButtonStyle::Outlined)
                            .start_icon(Icon::new(IconName::ZedAssistant))
                            .tooltip(Tooltip::text(format!(
                                "Runs {shown}; npx downloads it the first time you send a message"
                            )))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.use_command(command.clone(), cx)
                            }))
                    }))
                    .child(
                        h_flex()
                            .my(px(ds::space::MD))
                            .gap(px(ds::space::MD))
                            .items_center()
                            .child(studio_ui::ds::rule_h(cx))
                            .child(studio_ui::ds::text::meta("or"))
                            .child(studio_ui::ds::rule_h(cx)),
                    )
                    .child(
                        h_flex()
                            .gap(px(ds::space::XS))
                            .child(
                                studio_ui::ds::field_frame(false, cx)
                                    .flex_1()
                                    .child(studio_ui::ds::field_input(self.custom_command.clone())),
                            )
                            .child(
                                studio_ui::ds::icon_button("use-custom", IconName::ArrowRight)
                                    .tooltip(Tooltip::text("Use this command"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let text = this.custom_command.read(cx).text(cx);
                                        let command: Vec<String> =
                                            text.split_whitespace().map(str::to_owned).collect();
                                        if !command.is_empty() {
                                            this.use_command(command, cx);
                                        }
                                    })),
                            ),
                    ),
            )
    }

    /// What the thread's blocks are made from: its entries, which runs are open and which
    /// queries folded, and how the agent stopped, if it did.
    fn thread_key(&self) -> ThreadKey {
        use std::hash::{Hash, Hasher};
        let exited = match &self.state {
            State::Exited { message, stderr } => {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                (message, stderr).hash(&mut hasher);
                Some(hasher.finish())
            }
            _ => None,
        };
        let working = matches!(self.state, State::Prompting { .. });
        let waiting = self.shows_waiting().then(|| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (
                &self.opening,
                matches!(self.state, State::Starting),
                self.slow_start,
            )
                .hash(&mut hasher);
            hasher.finish()
        });
        (self.entries.version, self.folds, exited, working, waiting)
    }

    /// Whether the thread ends with [`Block::Waiting`]: a message sent before the agent was
    /// ready that has not gone, or a start under way after the conversation on screen.
    fn shows_waiting(&self) -> bool {
        self.opening.is_some()
            || (matches!(self.state, State::Starting) && !self.entries.is_empty())
    }

    /// Brings each agent reply's markdown up to its text: new replies get an entity, a growing
    /// one appends what streamed in since. The entries before `changed` are as they were when
    /// their markdown last took them in.
    fn sync_markdown(&mut self, changed: usize, cx: &mut Context<Self>) {
        self.markdown.retain(|index, _| {
            *index < changed || matches!(self.entries.get(*index), Some(Entry::Agent(_)))
        });
        self._markdown_watch
            .retain(|index, _| self.markdown.contains_key(index));
        for (index, entry) in self.entries.iter().enumerate().skip(changed) {
            let Entry::Agent(text) = entry else { continue };
            match self.markdown.get_mut(&index) {
                Some((markdown, taken)) if text.len() >= *taken => {
                    if text.len() > *taken {
                        let tail = text[*taken..].to_owned();
                        markdown.update(cx, |markdown, cx| markdown.append(&tail, cx));
                        *taken = text.len();
                    }
                }
                _ => {
                    let source: SharedString = text.clone().into();
                    let markdown = cx.new(|cx| Markdown::new(source, None, None, cx));
                    self._markdown_watch.insert(
                        index,
                        cx.observe(&markdown, move |this, _, cx| {
                            this.remeasure_entry(index, cx)
                        }),
                    );
                    self.markdown.insert(index, (markdown, text.len()));
                }
            }
        }
    }

    /// The turn entry `index` is in: from the person's message before it to the next one.
    fn turn_of(&self, index: usize) -> std::ops::Range<usize> {
        let is_user = |entry: &Entry| matches!(entry, Entry::User(_));
        let start = self.entries[..index]
            .iter()
            .rposition(is_user)
            .map_or(0, |at| at + 1);
        let end = self.entries[index..]
            .iter()
            .position(is_user)
            .map_or(self.entries.len(), |at| index + at);
        start..end
    }

    /// The reply of the turn entry `index` is in: its agent messages, each trimmed, a blank line
    /// apart.
    fn turn_reply(&self, index: usize) -> String {
        self.entries[self.turn_of(index)]
            .iter()
            .filter_map(|entry| match entry {
                Entry::Agent(text) => Some(text.trim()),
                _ => None,
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// A finished turn's actions under its last reply, after the reference chat's: copy the
    /// turn's reply, and how many tools the agent called for it, muted. There is no retry or
    /// rating: the agent offers neither. The reply is put together when Copy is clicked, not
    /// on every frame the turn's end is drawn.
    fn render_turn_actions(&self, index: usize, cx: &Context<Self>) -> AnyElement {
        let tools = self.entries[self.turn_of(index)]
            .iter()
            .filter(|entry| matches!(entry, Entry::Tool { .. }))
            .count();
        let panel = cx.entity().downgrade();
        h_flex()
            .gap(px(ds::space::SM))
            .items_center()
            .child(
                ds::row_icon_button(("copy-reply", index), IconName::Copy)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Copy the reply"))
                    .on_click(move |_, _, cx| {
                        if let Some(panel) = panel.upgrade() {
                            let reply = panel.read(cx).turn_reply(index);
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(reply));
                        }
                    }),
            )
            .when(tools > 0, |row| {
                row.child(ds::text::meta(if tools == 1 {
                    "1 tool call".to_owned()
                } else {
                    format!("{tools} tool calls")
                }))
            })
            .into_any_element()
    }

    /// What a block shows, folded to a number: a block whose number is unchanged keeps its
    /// measured height. Markdown that finishes parsing remeasures its block by itself. The
    /// `first` block carries the thread's top margin (`render_block`), so one that becomes
    /// first, or stops being so, is measured again.
    fn fingerprint(&self, block: &Block, first: bool) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        block.hash(&mut hasher);
        first.hash(&mut hasher);
        let entries = match *block {
            Block::Entry { index, .. } => index..index + 1,
            Block::Activity { start, end, .. } => {
                self.expanded_activity.contains(&start).hash(&mut hasher);
                start..end
            }
            Block::Waiting => {
                (
                    &self.opening,
                    matches!(self.state, State::Starting),
                    matches!(self.state, State::Exited { .. }),
                    self.slow_start,
                )
                    .hash(&mut hasher);
                0..0
            }
            Block::Exited => {
                if let State::Exited { message, stderr } = &self.state {
                    message.hash(&mut hasher);
                    stderr.hash(&mut hasher);
                }
                0..0
            }
        };
        for index in entries {
            self.entries.hash(index).hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Brings the list up to the thread, whose entries from `changed` on are new or changed: the
    /// blocks are made again from that entry's turn on ([`thread_blocks`]), and the blocks
    /// between those the list starts and ends with as last drawn are measured again; the others
    /// keep their heights, and the scroll stays. A streamed token costs its own turn, not the
    /// whole conversation.
    fn sync_thread(&mut self, changed: usize) {
        let shown: Vec<Block> = self.shown_blocks.iter().map(|(block, _)| *block).collect();
        let (blocks, kept) = thread_blocks(
            &shown,
            &self.entries,
            changed,
            matches!(self.state, State::Prompting { .. }),
            self.shows_waiting(),
            matches!(self.state, State::Exited { .. }),
        );
        // A kept block shows what it showed, unless a fold changed since.
        let refold = self.synced_folds != Some(self.folds);
        let blocks: Vec<(Block, u64)> = blocks
            .into_iter()
            .enumerate()
            .map(|(position, block)| {
                let fingerprint = match self.shown_blocks.get(position) {
                    Some((_, fingerprint)) if position < kept && !refold => *fingerprint,
                    _ => self.fingerprint(&block, position == 0),
                };
                (block, fingerprint)
            })
            .collect();
        self.synced_folds = Some(self.folds);
        studio_ui::rows::sync(&self.thread, &mut self.shown_blocks, blocks);
    }

    /// Measures again the block that shows entry `index`, whose markdown finished parsing.
    fn remeasure_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(position) = self.shown_blocks.iter().position(
            |(block, _)| matches!(block, Block::Entry { index: shown, .. } if *shown == index),
        ) {
            self.thread.remeasure_items(position..position + 1);
        }
        cx.notify();
    }

    fn render_thread(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.entries.is_empty()
            && self.opening.is_none()
            && !matches!(self.state, State::Exited { .. })
        {
            return v_flex()
                .flex_1()
                .min_h_0()
                .py(px(ds::space::XL))
                .pr(px(ds::space::XL))
                .pl(px(ds::space::MD))
                .child(studio_ui::ds::empty_state(
                    IconName::ZedAssistant,
                    "Ask about the graph",
                ))
                .into_any_element();
        }
        list(
            self.thread.clone(),
            cx.processor(|this, ix: usize, window, cx| this.render_block(ix, window, cx)),
        )
        .flex_1()
        .min_h_0()
        .size_full()
        .into_any_element()
    }

    /// One block of the thread, as the list asks for it. Steps of a turn sit close; a new
    /// question opens a new group (`render_entry`).
    fn render_block(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some((block, _)) = self.shown_blocks.get(ix).copied() else {
            return div().into_any_element();
        };
        let body = match block {
            Block::Entry { index, lead, ends } => match self.entries.get(index) {
                // The actions sit a step under the reply, apart from its text.
                Some(entry) if ends => v_flex()
                    .gap(px(ds::space::SM))
                    .child(self.render_entry(index, entry, lead, window, cx))
                    .child(gutter_row(None, self.render_turn_actions(index, cx)))
                    .into_any_element(),
                Some(entry) => self.render_entry(index, entry, lead, window, cx),
                None => div().into_any_element(),
            },
            Block::Activity { start, end, lead } => {
                if end <= self.entries.len() {
                    self.render_activity(start..end, lead, cx)
                } else {
                    div().into_any_element()
                }
            }
            Block::Waiting => self.render_waiting(cx),
            Block::Exited => match &self.state {
                State::Exited { message, stderr } => {
                    let mut callout = studio_ui::ds::callout(Severity::Error, message.clone());
                    if !stderr.is_empty() {
                        callout =
                            callout.description_slot(studio_ui::ds::code_block(stderr.clone(), cx));
                    }
                    gutter_row(None, callout).into_any_element()
                }
                _ => div().into_any_element(),
            },
        };
        div()
            .pl(px(ds::space::MD))
            .pr(px(ds::space::XL))
            .pb(px(ds::space::LG))
            .when(ix == 0, |block| block.pt(px(ds::space::XL)))
            .child(body)
            .into_any_element()
    }

    /// Edit for the message waiting for the agent ([`Self::edit_opening`]), offered once the
    /// agent is slow to start or has stopped.
    fn edit_opening_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.opening.as_ref()?;
        Some(
            ds::row_button("edit-opening", "Edit")
                .on_click(cx.listener(|this, _, window, cx| this.edit_opening(window, cx)))
                .into_any_element(),
        )
    }

    /// The message sent before the agent was ready, as the person's, with how the agent's
    /// start goes under it: starting, slow to answer (with Start again), or stopped before
    /// the message went. Slow or stopped, Edit takes it back.
    fn render_waiting(&self, cx: &mut Context<Self>) -> AnyElement {
        let status = match &self.state {
            State::Starting if self.slow_start => h_flex()
                .gap(px(ds::space::SM))
                .items_center()
                .child(ds::text::muted("The agent has not answered yet."))
                .child(
                    ds::row_button("restart-slow", "Start again")
                        .start_icon(Icon::new(IconName::RotateCw))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.restart(&Restart, window, cx)),
                        ),
                )
                .children(self.edit_opening_button(cx))
                .into_any_element(),
            State::Starting => h_flex()
                .gap(px(ds::space::XS))
                .items_center()
                .child(studio_ui::ds::spinner())
                .child(ds::text::meta("Starting the agent…"))
                .into_any_element(),
            State::Exited { .. } => h_flex()
                .gap(px(ds::space::SM))
                .items_center()
                .child(ds::text::meta(
                    "Not sent yet: it goes when the agent starts again.",
                ))
                .children(self.edit_opening_button(cx))
                .into_any_element(),
            _ => ds::text::meta("Waiting for the agent").into_any_element(),
        };
        v_flex()
            .gap(px(ds::space::SM))
            .when(!self.entries.is_empty(), |column| {
                column.mt(px(ds::space::XL))
            })
            .children(self.opening.as_ref().map(|text| {
                gutter_row(
                    Some(studio_ui::ds::icon::lead(IconName::Person)),
                    div().prose().child(text.clone()),
                )
            }))
            .child(gutter_row(None, status))
            .into_any_element()
    }

    /// One entry of the thread; `lead` marks the agent's first entry in a turn, which carries
    /// its mark.
    fn render_entry(
        &self,
        index: usize,
        entry: &Entry,
        lead: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match entry {
            Entry::User(text) => gutter_row(
                Some(studio_ui::ds::icon::lead(IconName::Person)),
                div().prose().child(text.clone()),
            )
            .when(index > 0, |row| row.mt(px(ds::space::XL)))
            .into_any_element(),
            Entry::Agent(text) => gutter_row(
                lead.then(|| studio_ui::ds::icon::lead(IconName::ZedAssistant)),
                match self.markdown.get(&index) {
                    // Zed's markdown, as Zed's agent panel renders a reply.
                    Some((markdown, _)) => div()
                        .prose()
                        .child(MarkdownElement::new(
                            markdown.clone(),
                            MarkdownStyle::themed(MarkdownFont::Agent, window, cx),
                        ))
                        .into_any_element(),
                    None => div()
                        .prose()
                        .child(text.trim().to_owned())
                        .into_any_element(),
                },
            )
            .into_any_element(),
            Entry::Thought(_) | Entry::Tool { .. } => {
                gutter_row(None, self.activity_step(index, entry, cx)).into_any_element()
            }
            Entry::Plan(items) => gutter_row(
                Some(studio_ui::ds::icon::meta(IconName::ListTodo)),
                v_flex()
                    .gap(px(ds::space::XXS))
                    .children(items.iter().map(|(content, status)| {
                        let icon = match status {
                            PlanEntryStatus::Completed => {
                                ds::icon::meta(IconName::Check).color(Color::Success)
                            }
                            PlanEntryStatus::InProgress => {
                                ds::icon::meta(IconName::ArrowRight).color(Color::Accent)
                            }
                            _ => ds::icon::meta(IconName::Circle).color(Color::Disabled),
                        };
                        h_flex()
                            .gap(px(ds::space::MD))
                            .child(icon)
                            .child(studio_ui::ds::text::body(content.clone()))
                    })),
            )
            .into_any_element(),
            Entry::Notice { text, error } => gutter_row(
                Some(
                    studio_ui::ds::icon::meta(if *error {
                        IconName::Warning
                    } else {
                        IconName::Info
                    })
                    .color(if *error { Color::Error } else { Color::Muted }),
                ),
                studio_ui::ds::text::body(text.clone()).color(if *error {
                    Color::Error
                } else {
                    Color::Muted
                }),
            )
            .into_any_element(),
            Entry::Permission(card) => {
                gutter_row(None, self.render_permission(index, card, cx)).into_any_element()
            }
            Entry::Write(card) => {
                gutter_row(None, self.render_write(index, card, cx)).into_any_element()
            }
        }
    }

    /// A run of tool calls and thoughts as one muted line ("Thought 1 time, ran 2 queries"),
    /// with a turning arrow while a step runs and the failures named; it opens to the steps.
    fn render_activity(
        &self,
        range: std::ops::Range<usize>,
        lead: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let start = range.start;
        let steps = &self.entries[range.clone()];
        let thoughts = steps
            .iter()
            .filter(|entry| matches!(entry, Entry::Thought(_)))
            .count();
        let tools: Vec<&ToolCallStatus> = steps
            .iter()
            .filter_map(|entry| match entry {
                Entry::Tool { status, .. } => Some(status),
                _ => None,
            })
            .collect();
        let running = tools
            .iter()
            .any(|status| matches!(status, ToolCallStatus::Pending | ToolCallStatus::InProgress));
        let failed = tools
            .iter()
            .filter(|status| matches!(status, ToolCallStatus::Failed))
            .count();
        let expanded = self.expanded_activity.contains(&start);
        let header = h_flex()
            .id(("activity", start))
            .gap(px(ds::space::XS))
            .items_center()
            .cursor_pointer()
            .child(studio_ui::ds::text::muted(activity_summary(
                thoughts,
                tools.len(),
            )))
            .when(failed > 0, |row| {
                row.child(studio_ui::ds::text::error(format!("· {failed} failed")))
            })
            .child(if running {
                studio_ui::ds::spinner().into_any_element()
            } else {
                studio_ui::ds::icon::meta(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .into_any_element()
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.expanded_activity.remove(&start) {
                    this.expanded_activity.insert(start);
                }
                this.folds += 1;
                cx.notify();
            }));
        gutter_row(
            lead.then(|| studio_ui::ds::icon::lead(IconName::ZedAssistant)),
            v_flex()
                .gap(px(ds::space::XS))
                .child(header)
                .when(expanded, |list| {
                    list.child(
                        v_flex()
                            .gap(px(ds::space::XS))
                            .pl(px(ds::space::SM))
                            .border_l_1()
                            .border_color(ds::surface::rule(cx))
                            .children(
                                range.map(|index| {
                                    self.activity_step(index, &self.entries[index], cx)
                                }),
                            ),
                    )
                }),
        )
        .into_any_element()
    }

    /// One step of a run, as text: a tool call by name, a thought by its words. No marks for
    /// steps that went well; a running call turns an arrow, a failed one reads in the error
    /// colour.
    fn activity_step(&self, index: usize, entry: &Entry, cx: &App) -> AnyElement {
        match entry {
            Entry::Thought(text) => div()
                .id(("thought", index))
                .quiet_prose(cx)
                .child(text.trim().to_owned())
                .into_any_element(),
            Entry::Tool { title, status, .. } => {
                let failed = matches!(status, ToolCallStatus::Failed);
                let running =
                    matches!(status, ToolCallStatus::Pending | ToolCallStatus::InProgress);
                h_flex()
                    .id(("tool", index))
                    .gap(px(ds::space::XS))
                    .tooltip(Tooltip::text(title.clone()))
                    .child(
                        studio_ui::ds::text::body(if failed {
                            format!("{title} failed")
                        } else {
                            title.clone()
                        })
                        .color(if failed { Color::Error } else { Color::Muted })
                        .single_line()
                        .truncate(),
                    )
                    .when(running, |row| row.child(studio_ui::ds::spinner()))
                    .into_any_element()
            }
            _ => div().into_any_element(),
        }
    }

    fn render_permission(
        &self,
        index: usize,
        card: &PermissionCard,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let header = studio_ui::ds::card_header(cx)
            .child(studio_ui::ds::icon::meta(IconName::Warning).color(Color::Warning))
            // Whole, wrapped: the end of a long command is where a second one would hide.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(studio_ui::ds::text::body(card.title.clone())),
            );
        let footer = match &card.answer {
            Some(answer) => {
                studio_ui::ds::card_footer(cx).child(studio_ui::ds::text::meta(answer.clone()))
            }
            None => studio_ui::ds::card_footer(cx).children(card.options.iter().enumerate().map(
                |(option_index, option)| {
                    let icon = match option.kind {
                        PermissionOptionKind::AllowOnce => IconName::Check,
                        PermissionOptionKind::AllowAlways => IconName::CheckDouble,
                        _ => IconName::Close,
                    };
                    studio_ui::ds::button(
                        ("permission", index * 16 + option_index),
                        option.name.clone(),
                    )
                    .start_icon(Icon::new(icon))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.choose_option(index, option_index, cx)
                    }))
                },
            )),
        };
        studio_ui::ds::card(cx)
            .child(header)
            .child(footer)
            .into_any_element()
    }

    fn render_write(&self, index: usize, card: &WriteCard, cx: &mut Context<Self>) -> AnyElement {
        let focus = self.focus_handle.clone();
        let header = studio_ui::ds::card_header(cx)
            .child(studio_ui::ds::icon::meta(IconName::Pencil))
            .child(
                studio_ui::ds::text::body(card.summary.clone())
                    .single_line()
                    .truncate(),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id(("card-branch", index))
                    .tooltip(Tooltip::text(match &card.creates_from {
                        Some(base) => format!(
                            "Runs on {}, which Studio creates from {base} first",
                            card.branch
                        ),
                        None => format!("Runs on {}", card.branch),
                    }))
                    .child(studio_ui::ds::meta(
                        IconName::GitBranch,
                        card.branch.clone(),
                    )),
            )
            .when(card.creates_from.is_some(), |header| {
                header.child(studio_ui::ds::tag("new", cx))
            });
        let params = (!card.params.is_empty()).then(|| {
            ds::fields().children(card.params.iter().map(|(key, value)| {
                ds::field(
                    format!("${key}"),
                    ds::text::code(value.clone(), cx).single_line().truncate(),
                )
            }))
        });
        // A write with no query or parameters (a branch created through the CLI) has no body.
        let body = (!card.query.is_empty() || params.is_some()).then(|| {
            ds::card_body()
                .when(!card.query.is_empty(), |body| {
                    body.child(ds::code_block(card.query.clone(), cx))
                })
                .children(params)
        });
        // The keys answer the write card waiting longest; only that card shows them, so a key
        // never answers a card other than the one it is shown on ([`Entries::keyed_write`]).
        let keyed = self.entries.keyed_write() == Some(index);
        let key = |action: &dyn gpui::Action, cx: &App| {
            keyed.then(|| KeyBinding::for_action_in(action, &focus, cx))
        };
        let footer = match &card.state {
            WriteState::Waiting(_) => studio_ui::ds::card_footer(cx)
                .child(
                    studio_ui::ds::button(("allow", index), "Allow")
                        .start_icon(Icon::new(IconName::Check))
                        .when_some(key(&Allow, cx), |button, key| button.key_binding(key))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.decide(index, Decision::Allow, cx)
                        })),
                )
                .child(
                    studio_ui::ds::button(("always", index), "Always on this branch")
                        .start_icon(Icon::new(IconName::CheckDouble))
                        .when_some(key(&AllowAlways, cx), |button, key| button.key_binding(key))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.decide(index, Decision::AllowAlways, cx)
                        })),
                )
                .child(div().flex_1())
                .child(
                    studio_ui::ds::button(("reject", index), "Reject")
                        .start_icon(Icon::new(IconName::Close))
                        .when_some(key(&Reject, cx), |button, key| button.key_binding(key))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.decide(index, Decision::Reject, cx)
                        })),
                ),
            state => {
                // A running card turns the spinner; the others name their outcome's icon.
                let (icon, color, label, tooltip): (
                    Option<IconName>,
                    Color,
                    String,
                    Option<String>,
                ) = match state {
                    WriteState::Running => (None, Color::Accent, "Running".into(), None),
                    WriteState::Committed {
                        commit,
                        nodes,
                        edges,
                    } => (
                        Some(IconName::Check),
                        Color::Success,
                        "Committed".into(),
                        Some(format!(
                            "Commit {commit}: {nodes} nodes and {edges} edges changed"
                        )),
                    ),
                    WriteState::Refused(message) => (
                        Some(IconName::XCircle),
                        Color::Error,
                        "Refused".into(),
                        Some(message.clone()),
                    ),
                    WriteState::Unknown { commit } => (
                        Some(IconName::Warning),
                        Color::Warning,
                        "Outcome unknown".into(),
                        Some(match commit {
                            None => UNKNOWN_OUTCOME.into(),
                            Some(commit) => format!(
                                "The server wrote commit {commit}, of a kind Studio does not \
                                 know, so whether {} moved is not known. Check its history \
                                 before the agent tries it again.",
                                card.branch
                            ),
                        }),
                    ),
                    WriteState::Done(label) => {
                        (Some(IconName::Check), Color::Success, label.clone(), None)
                    }
                    WriteState::Unpublished { commit, kind } => (
                        Some(IconName::Warning),
                        Color::Warning,
                        "Not on the branch".into(),
                        Some(match kind {
                            CommitKind::Unattached => format!(
                                "The server saved this write as snapshot {commit}, which no \
                                 branch shows and which never publishes, so {} did not change.",
                                card.branch
                            ),
                            _ => format!(
                                "The server held this write as commit {commit}, which no \
                                 branch shows, so {} did not change. Publishing it is yours, \
                                 outside Studio.",
                                card.branch
                            ),
                        }),
                    ),
                    _ => (Some(IconName::Close), Color::Muted, "Rejected".into(), None),
                };
                let commit = match state {
                    WriteState::Committed {
                        commit,
                        nodes,
                        edges,
                    } => Some((studio_ui::short_commit_id(commit), *nodes + *edges)),
                    _ => None,
                };
                studio_ui::ds::card_footer(cx)
                    .px(px(ds::space::MD))
                    .gap(px(ds::space::LG))
                    .child(
                        h_flex()
                            .id(("outcome", index))
                            .gap(px(ds::space::XS))
                            .when_some(tooltip, |d, tooltip| d.tooltip(Tooltip::text(tooltip)))
                            .child(match icon {
                                Some(icon) => studio_ui::ds::icon::meta(icon)
                                    .color(color)
                                    .into_any_element(),
                                None => studio_ui::ds::spinner().into_any_element(),
                            })
                            .child(studio_ui::ds::text::body(label).color(color)),
                    )
                    .when_some(commit, |d, (commit, changed)| {
                        d.child(studio_ui::ds::meta_code(IconName::GitCommit, commit, cx))
                            .child(studio_ui::ds::meta(IconName::Pencil, changed.to_string()))
                    })
            }
        };
        studio_ui::ds::card(cx)
            .child(header)
            .children(body)
            .child(footer)
            .into_any_element()
    }

    /// The agent's options in the composer (spec 011), as icon and value with no chrome: its mode
    /// and reasoning level in the agent's order, then everything else (switches, profiles)
    /// behind one More options menu, then the model, last as in Delta, whose menu also holds
    /// the agent. Before the agent offers a model, the agent's name and state stand there.
    fn render_options(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let panel = cx.entity().downgrade();
        let options: Vec<Rc<OptionView>> = if self.host.is_some() {
            self.option_views.clone()
        } else {
            Vec::new()
        };
        let (shown, more): (Vec<_>, Vec<_>) = options
            .into_iter()
            .partition(|option| option.picker_icon().is_some());
        let (models, shown): (Vec<_>, Vec<_>) =
            shown.into_iter().partition(|option| option.is_model());
        let (agent_name, dot, state) = self.status();
        let agent_menu = AgentMenu {
            agents: agents(cx),
            current: self.agent.clone(),
            model: models.first().cloned(),
        };

        let mut pickers: Vec<AnyElement> = shown
            .into_iter()
            .map(|option| {
                let icon = option.picker_icon().unwrap_or(IconName::Circle);
                option_picker(option, Icon::new(icon), &panel, None)
            })
            .collect();
        if !more.is_empty() {
            let summary = more
                .iter()
                .map(|option| option.summary())
                .collect::<Vec<_>>()
                .join(" · ");
            let panel = panel.clone();
            pickers.push(
                PopoverMenu::new("more-options")
                    .trigger(
                        ds::icon_button("more-options-button", IconName::Ellipsis)
                            .icon_color(Color::Muted)
                            .tooltip(move |_, cx| {
                                Tooltip::with_meta("More options", None, summary.clone(), cx)
                            }),
                    )
                    .menu(move |window, cx| {
                        let more = more.clone();
                        let panel = panel.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for (index, option) in more.iter().enumerate() {
                                if index > 0 {
                                    menu = menu.separator();
                                }
                                menu = option.add_to(menu, &panel);
                            }
                            menu
                        }))
                    })
                    .anchor(gpui::Anchor::BottomRight)
                    .into_any_element(),
            );
        }
        match models.into_iter().next() {
            Some(model) => {
                let icon = match provider_icon(&format!("{agent_name} {}", model.current_name())) {
                    Some(icon) => Icon::new(icon),
                    None => Icon::new(IconName::ZedAssistant),
                };
                pickers.push(option_picker(model, icon, &panel, Some(agent_menu)));
            }
            None => {
                let agent_name = match self.state {
                    State::Ready { .. } | State::Prompting { .. } => agent_name,
                    State::Starting => format!("Starting {}…", self.agent_label(cx)),
                    _ => self.agent_label(cx),
                };
                let command = self.agent.join(" ");
                let about = if command.is_empty() {
                    format!("{state}. Choose an agent.")
                } else {
                    format!("{state} · {command}")
                };
                pickers.push(
                    PopoverMenu::new("agent-menu")
                        .trigger(
                            ui::ButtonLike::new("agent")
                                .style(ButtonStyle::Subtle)
                                .child(
                                    h_flex()
                                        .px(px(ds::space::XS))
                                        .gap(px(ds::space::XS))
                                        .items_center()
                                        .child(studio_ui::ds::dot(dot))
                                        .child(studio_ui::ds::text::muted(agent_name)),
                                )
                                .tooltip(Tooltip::text(about)),
                        )
                        .menu(move |window, cx| {
                            let (agent_menu, panel) = (agent_menu.clone(), panel.clone());
                            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                                agent_menu.add_to(menu, &panel)
                            }))
                        })
                        .anchor(gpui::Anchor::BottomRight)
                        .into_any_element(),
                );
            }
        }
        pickers
    }

    /// How full the context window is, as a ring that turns to the warning colour near the end,
    /// with the percentage then; the share, token counts and cost so far on hover.
    fn render_usage(&self, cx: &App) -> Option<AnyElement> {
        let usage = self.usage.as_ref().filter(|usage| usage.size > 0)?;
        let ratio = usage.ratio();
        let nearly_full = ratio >= CONTEXT_NEARLY_FULL;
        let color = if nearly_full {
            Color::Warning.color(cx)
        } else {
            Color::Muted.color(cx)
        };
        let title = format!("{}% of the context window", (ratio * 100.).round() as u32);
        let mut meta = usage.tokens();
        if let Some((amount, currency)) = &usage.cost {
            meta.push_str(&format!(" · {} so far", format_cost(*amount, currency)));
        }
        Some(
            h_flex()
                .id("context-usage")
                .flex_none()
                .gap(px(ds::space::XS))
                .items_center()
                .child(usage.gauge(color, cx))
                // Nearly empty, a ring still reads as a spinner; its number says it is a gauge.
                .child(
                    ds::text::meta(format!("{}%", (ratio * 100.).round() as u32)).color(
                        if nearly_full {
                            Color::Warning
                        } else {
                            Color::Muted
                        },
                    ),
                )
                .tooltip(move |_, cx| Tooltip::with_meta(title.clone(), None, meta.clone(), cx))
                .into_any_element(),
        )
    }

    /// The messages waiting for the agent, above the composer as Zed shows its queue: how many
    /// and whether the queue is paused, Clear all, then one line each, the next one marked;
    /// Remove and Edit on hover, Send now always on the next one and on hover on the rest.
    fn render_queue(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.queue.entries.is_empty() {
            return None;
        }
        let count = self.queue.entries.len();
        let mut summary = if count == 1 {
            "1 queued message".to_owned()
        } else {
            format!("{count} queued messages")
        };
        if self.queue.paused {
            summary.push_str(" · paused until you send");
        }
        let focus = self.focus_handle.clone();
        let header = h_flex()
            .h(px(ds::size::ROW))
            .px(px(ds::space::MD))
            .gap(px(ds::space::SM))
            .items_center()
            .child(ds::text::meta(summary))
            .child(div().flex_1())
            .child(
                ds::button("clear-queue", "Clear all")
                    .style(ButtonStyle::Subtle)
                    .color(Color::Muted)
                    .tooltip(move |_, cx| {
                        Tooltip::for_action_in("Drop the queued messages", &ClearQueue, &focus, cx)
                    })
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.clear_queue(&ClearQueue, window, cx)
                        }),
                    ),
            );
        let border = ds::surface::rule(cx);
        let rows: Vec<AnyElement> = self
            .queue
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let id = entry.id;
                let next = index == 0;
                let line = entry.text.lines().next().unwrap_or_default().to_owned();
                h_flex()
                    .id(("queued", id))
                    .group("queued")
                    .h(px(ds::size::ROW))
                    .px(px(ds::space::MD))
                    .gap(px(ds::space::SM))
                    .items_center()
                    .border_t_1()
                    .dotted(cx)
                    .tooltip(Tooltip::text(entry.text.clone()))
                    .child(studio_ui::ds::dot(if next {
                        Color::Accent
                    } else {
                        Color::Muted
                    }))
                    .child(
                        div().flex_1().min_w_0().child(
                            studio_ui::ds::text::body(line)
                                .color(if next { Color::Default } else { Color::Muted })
                                .single_line()
                                .truncate(),
                        ),
                    )
                    .child(
                        h_flex()
                            .gap(px(ds::space::XXS))
                            .items_center()
                            .visible_on_hover("queued")
                            .child(
                                ds::row_icon_button(("remove-queued", id), IconName::Trash)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Remove from the queue"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.queue.take(id);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                ds::row_icon_button(("edit-queued", id), IconName::Pencil)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Edit in the composer"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.edit_queued(id, window, cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .when(!next, |send| send.visible_on_hover("queued"))
                            .child(
                                ds::button(("send-queued", id), "Send now")
                                    .style(if next {
                                        ButtonStyle::Outlined
                                    } else {
                                        ButtonStyle::Subtle
                                    })
                                    .tooltip(Tooltip::text(
                                        "Stop the agent's turn and send this at once",
                                    ))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.send_queued_now(id, cx)
                                    })),
                            ),
                    )
                    .into_any_element()
            })
            .collect();
        // Above the composer's card, on its edges and its corner.
        Some(
            v_flex()
                .rounded(px(ds::radii(cx).md))
                .border_1()
                .border_color(border)
                .bg(ds::surface::sunken(cx))
                .overflow_hidden()
                .child(header)
                .children(rows)
                .into_any_element(),
        )
    }

    /// The notice strip on the composer's card (spec 011), one line for each state that holds
    /// the conversation up or follows from it, or none: the context window nearly full (the
    /// gauge, how full and how many tokens), then the agent's branch once it exists, though it
    /// was made before the conversation came back (its name, how many changes the agent
    /// committed on it since, when any, and Review).
    fn render_notices(&self, cx: &mut Context<Self>) -> Option<Div> {
        let mut lines = Vec::new();
        if let Some(usage) = self.usage.as_ref().filter(|usage| usage.size > 0) {
            let ratio = usage.ratio();
            if ratio >= CONTEXT_NEARLY_FULL {
                lines.push(
                    ds::notice_line()
                        .child(usage.gauge(Color::Warning.color(cx), cx))
                        .child(ds::text::body(format!(
                            "{}% of the context window used",
                            (ratio * 100.).round() as u32
                        )))
                        .child(ds::text::muted(usage.tokens())),
                );
            }
        }
        if let Some(branch) = self.branch.as_ref().filter(|b| b.offers_review()) {
            lines.push(
                ds::notice_line()
                    .child(ds::meta(IconName::GitBranch, branch.name.clone()))
                    .when(branch.changes > 0, |line| {
                        line.child(
                            div()
                                .id("agent-changes")
                                .tooltip(Tooltip::text(
                                    "Changes the agent committed on its branch since this \
                                     conversation opened",
                                ))
                                .child(ds::meta(IconName::Pencil, branch.changes.to_string())),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        ds::button("review", "Review")
                            .start_icon(Icon::new(IconName::Eye))
                            .on_click(cx.listener(|this, _, window, cx| this.review(window, cx))),
                    ),
            );
        }
        (!lines.is_empty()).then(|| ds::notice_strip(cx).children(lines))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A conversation runs the agent it was held with, whatever the default is now; a new one
    /// runs the default. A configured agent that is no preset is offered beside the presets.
    #[gpui::test]
    fn a_conversation_runs_the_agent_it_was_held_with(cx: &mut gpui::TestAppContext) {
        let words = |command: &[&str]| -> Vec<String> {
            command.iter().map(|word| (*word).to_owned()).collect()
        };
        let codex = words(PRESETS[1].1);
        let custom = words(&["my-agent", "--acp"]);
        cx.update(|cx| {
            studio_settings::init_test(cx);
            let mut settings = StudioSettings::get(cx).clone();
            settings.agent.command = custom.clone();
            cx.set_global(settings);
        });
        let path =
            std::env::temp_dir().join(format!("studio-agent-for-{}.json", std::process::id()));
        let sessions = cx.new(|_| AgentSessions::load(path.clone()));
        let now = chrono::Utc::now();
        sessions.update(cx, |sessions, cx| {
            sessions.upsert(
                SessionRecord {
                    session: "held".into(),
                    graph: "server/crm".into(),
                    agent: codex.join(" "),
                    title: None,
                    title_override: None,
                    created_at: now,
                    updated_at: now,
                    branch: "agent/held".into(),
                    branch_created: false,
                    pinned: false,
                    archived: false,
                    cwd: std::env::temp_dir(),
                },
                cx,
            )
        });
        cx.update(|cx| {
            assert_eq!(agent_for(Some("held"), &sessions, cx), codex);
            assert_eq!(agent_for(Some("unknown"), &sessions, cx), custom);
            assert_eq!(agent_for(None, &sessions, cx), custom);
            let names: Vec<String> = agents(cx).into_iter().map(|(name, _)| name).collect();
            assert_eq!(names, ["Claude Agent", "Codex", "my-agent"]);
        });
        std::fs::remove_file(&path).ok();
    }

    /// A conversation restored with the workspace behind another tab starts no agent: its start
    /// waits for the tab's first draw, which takes it. Before, every restored conversation
    /// started its agent at launch, a process and a replay each. (A drawn tab here has no agent
    /// to start, so the test starts no process.)
    /// Starting a conversation nothing was sent in again (Restart, or Start again after a slow
    /// start) removes the folder it ran in, as choosing another agent does: the start takes a
    /// folder of its own. Before, the old folder, with its `AGENTS.md` and skill, stayed.
    #[gpui::test]
    async fn starting_an_unsent_conversation_again_removes_its_folder(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
            editor::init(cx);
            let mut settings = StudioSettings::get(cx).clone();
            settings.agent.command = vec!["an-agent".to_owned()];
            cx.set_global(settings);
        });
        let (session, _) = graph_store::test_server::followed_session(
            "node Person {\n  slug: String @key\n}\n",
            |_| serde_json::json!([]),
            cx,
        );
        cx.run_until_parked();
        let (_root, cx) = cx.add_window_view(|_, _| gpui::EmptyView);
        let panel = cx.update(|window, cx| {
            cx.new(|cx| {
                AgentPanel::new(
                    session,
                    WeakEntity::new_invalid(),
                    Some("s1".into()),
                    window,
                    cx,
                )
            })
        });
        let old = panel.read_with(cx, |panel, cx| {
            crate::sessions::graph_cwd(&panel.session.read(cx).overrides_key()).join("main-old")
        });
        std::fs::create_dir_all(&old).unwrap();
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.cwd = Some(old.clone());
                panel.restart(&Restart, window, cx);
            })
        });
        cx.run_until_parked();
        assert!(!old.exists(), "the unused folder removed");
    }

    #[gpui::test]
    async fn a_restored_conversation_starts_its_agent_when_shown(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
            editor::init(cx);
        });
        let agent = |command: &[&str], cx: &mut gpui::TestAppContext| {
            cx.update(|cx| {
                let mut settings = StudioSettings::get(cx).clone();
                settings.agent.command = command.iter().map(|part| (*part).to_owned()).collect();
                cx.set_global(settings);
            })
        };
        agent(&["an-agent"], cx);
        let (session, _) = graph_store::test_server::followed_session(
            "node Person {\n  slug: String @key\n}\n",
            |_| serde_json::json!([]),
            cx,
        );
        cx.run_until_parked();
        let (_root, cx) = cx.add_window_view(|_, _| gpui::EmptyView);
        let hidden = cx.update(|window, cx| {
            let session = session.clone();
            cx.new(|cx| {
                let mut panel = AgentPanel::new(
                    session,
                    WeakEntity::new_invalid(),
                    Some("s1".into()),
                    window,
                    cx,
                );
                panel.start_when_shown(cx);
                panel
            })
        });
        cx.run_until_parked();
        hidden.read_with(cx, |panel, _| {
            assert!(matches!(panel.state, State::Idle), "no agent started");
            assert!(panel.start_owed, "it starts when drawn");
        });

        agent(&[], &mut cx.cx);
        let (shown, cx) = cx.add_window_view(|window, cx| {
            let mut panel = AgentPanel::new(
                session,
                WeakEntity::new_invalid(),
                Some("s2".into()),
                window,
                cx,
            );
            panel.start_when_shown(cx);
            panel
        });
        cx.run_until_parked();
        shown.read_with(cx, |panel, _| {
            assert!(!panel.start_owed, "the draw took it")
        });
    }

    /// Rendering asks [`shows_query`] of every tool call; it agrees with [`shown_query`].
    /// A turn's actions go under its last reply once it has finished: not under a turn the
    /// agent still works on, nor in a turn with no reply.
    #[test]
    fn a_turn_ends_at_its_last_reply() {
        let user = || Entry::User("q".into());
        let agent = || Entry::Agent("a".into());
        let thought = || Entry::Thought("t".into());
        let entries = [
            user(),
            agent(),
            thought(),
            agent(),
            user(),
            thought(),
            user(),
            agent(),
        ];
        assert_eq!(turn_ends(&entries, false), [3, 7]);
        assert_eq!(
            turn_ends(&entries, true),
            [3],
            "the last turn is still going"
        );
        assert!(turn_ends(&[], false).is_empty());
    }

    #[test]
    fn token_counts_and_costs_read_like_zeds() {
        assert_eq!(humanize_tokens(999), "999");
        assert_eq!(humanize_tokens(1_000), "1k");
        assert_eq!(humanize_tokens(1_234), "1.2k");
        assert_eq!(humanize_tokens(83_600), "84k");
        assert_eq!(humanize_tokens(200_000), "200k");
        assert_eq!(humanize_tokens(1_000_000), "1M");
        assert_eq!(humanize_tokens(1_250_000), "1.2M");
        assert_eq!(format_cost(0.0042, "USD"), "0.0042 USD");
        assert_eq!(format_cost(0.411, "USD"), "0.41 USD");
        assert_eq!(format_cost(0., "USD"), "0.00 USD");
    }

    #[test]
    fn an_approval_the_gateway_cannot_take_records_that_nothing_was_sent() {
        let (decide, decision) = oneshot::channel::<Option<OnOutcome>>();
        drop(decision);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let heard = seen.clone();
        hand_over(
            decide,
            Some(Box::new(move |outcome: &Seen| {
                *heard.lock().unwrap() = Some(outcome.clone());
            })),
        );
        assert_eq!(
            *seen.lock().unwrap(),
            Some(Seen::Refused(crate::gateway::NOT_SENT.into()))
        );
    }

    /// An agent's write is logged as Studio's own writes are, by its commit's kind: one written
    /// `as commit` is held as a proposal and one written `as unattached branch` as a snapshot,
    /// each with its commit and no branch moved; one on the agent's branch is committed there;
    /// one of a kind this build does not know leaves its outcome unknown, and its card says so.
    /// A stored query that read wrote nothing, and is not logged as committed.
    #[test]
    fn an_agents_held_write_is_logged_as_held() {
        use graph_store::HeldAs;
        let written = |commit: Option<(&str, CommitKind)>| Seen::Written {
            commit: commit.map(|(id, kind)| (id.to_owned(), kind)),
            nodes: 1,
            edges: 0,
            branch_created: false,
        };
        let held = |kind| outcome_kind(&written(Some(("c3", kind))));
        assert_eq!(
            held(CommitKind::Unpublished),
            OutcomeKind::Held {
                commit: "c3".into(),
                held: HeldAs::Proposal,
            }
        );
        assert_eq!(
            held(CommitKind::Unattached),
            OutcomeKind::Held {
                commit: "c3".into(),
                held: HeldAs::Snapshot,
            }
        );
        assert!(
            matches!(held(CommitKind::Other), OutcomeKind::Unknown { reason } if reason.contains("c3")),
            "a kind Studio does not know leaves the branch's move unknown"
        );
        assert!(matches!(
            card_state(&written(Some(("c3", CommitKind::Other))), false),
            WriteState::Unknown { commit: Some(commit) } if commit == "c3"
        ));
        assert_eq!(
            outcome_kind(&written(Some(("c2", CommitKind::Published)))),
            OutcomeKind::Committed {
                commit: Some("c2".into())
            }
        );
        assert_eq!(
            outcome_kind(&written(None)),
            OutcomeKind::Committed { commit: None }
        );
        assert_eq!(
            outcome_kind(&Seen::Read),
            OutcomeKind::Committed { commit: None },
            "logged as writing nothing, never as a commit"
        );
        assert!(
            matches!(card_state(&Seen::Read, false), WriteState::Done(label) if label.contains("nothing written"))
        );
    }

    fn write_card(id: u64, state: WriteState) -> Entry {
        Entry::Write(WriteCard {
            id,
            summary: "Mutation".into(),
            query: String::new(),
            params: Vec::new(),
            branch: "agent/x".into(),
            creates_from: None,
            state,
        })
    }

    /// The approval keys answer only a write card, the one waiting longest, which shows them: a
    /// permission request of the agent's own waiting above it is answered by its own buttons,
    /// so ⌘⌥Y never picks the agent's lasting rule unseen.
    #[test]
    fn the_keys_answer_only_the_write_card_that_shows_them() {
        let (reply, _asked) = oneshot::channel();
        let permission = Entry::Permission(PermissionCard {
            title: "rm -rf build".into(),
            options: Vec::new(),
            reply: Some(reply),
            answer: None,
        });
        let waiting = || WriteState::Waiting(oneshot::channel().0);
        let mut entries = Entries::default();
        for entry in [
            Entry::User("change it".into()),
            permission,
            write_card(0, WriteState::Running),
        ] {
            entries.push(entry);
        }
        assert!(entries[1].waits(), "the permission card waits first");
        assert_eq!(entries.first_waiting(|_| true), Some(1));
        assert_eq!(
            entries.keyed_write(),
            None,
            "a permission card alone takes no key"
        );
        entries.push(write_card(1, waiting()));
        entries.push(write_card(2, waiting()));
        assert_eq!(entries.keyed_write(), Some(3));
        // Answered, a card is passed over, and the search starts past it from then on.
        if let Some(Entry::Write(card)) = entries.get_mut(3) {
            card.state = WriteState::Running;
        }
        assert_eq!(entries.keyed_write(), Some(4));
        if let Some(Entry::Permission(card)) = entries.get_mut(1) {
            card.cancel();
        }
        assert_eq!(entries.first_waiting(|_| true), Some(4));
        assert_eq!(entries.answered_before.get(), 4);
        // Taken out and put back, as a replay does, the cards are found again.
        let said = entries.take_from(2);
        assert_eq!(entries.first_waiting(|_| true), None);
        entries.extend(said);
        assert_eq!(entries.keyed_write(), Some(4));
    }

    /// A write sent before the agent stopped runs on, and its card waits to hear how rather than
    /// saying its outcome is unknown; one still waiting for the person was never sent. The
    /// outcome finds its card by its own id after the thread changed around it, and the agent's
    /// branch learns whether it exists.
    #[test]
    fn a_write_sent_before_the_agent_stopped_still_shows_its_outcome() {
        let mut entries = Entries::default();
        entries.push(Entry::User("change it".into()));
        entries.push(write_card(7, WriteState::Running));
        entries.push(write_card(8, WriteState::Waiting(oneshot::channel().0)));
        entries.stop_writes();
        let state = |entries: &Entries, at: usize| match &entries[at] {
            Entry::Write(card) => match &card.state {
                WriteState::Running => "running".to_owned(),
                WriteState::Refused(text) => text.clone(),
                WriteState::Committed { commit, .. } => format!("committed {commit}"),
                WriteState::Done(label) => label.clone(),
                _ => "other".to_owned(),
            },
            _ => "not a card".to_owned(),
        };
        assert_eq!(state(&entries, 1), "running");
        assert_eq!(state(&entries, 2), STOPPED_UNSENT);
        // The conversation went on; the outcome lands on its own card.
        let said = entries.take_from(0);
        entries.push(Entry::Notice {
            text: "Resumed".into(),
            error: false,
        });
        entries.extend(said);
        let committed = Seen::Written {
            commit: Some(("c9".into(), CommitKind::Published)),
            nodes: 1,
            edges: 0,
            branch_created: false,
        };
        let card = entries.write_card(7).expect("the card, moved");
        card.state = card_state(&committed, false);
        assert_eq!(state(&entries, 2), "committed c9");
        assert!(entries.write_card(9).is_none());
        // What it says of the agent's branch.
        assert_eq!(branch_after(WriteKind::Mutate, &committed), Some(true));
        assert_eq!(
            branch_after(WriteKind::BranchDelete, &committed),
            Some(false)
        );
        assert_eq!(
            branch_after(WriteKind::Load, &Seen::Unknown("lost".into())),
            None
        );
        let created = |branch_created| Seen::Written {
            commit: None,
            nodes: 0,
            edges: 0,
            branch_created,
        };
        assert!(makes_branch(Creates::Always, &created(false)));
        assert!(makes_branch(Creates::IfMissing, &created(true)));
        assert!(!makes_branch(Creates::IfMissing, &created(false)));
        assert!(!makes_branch(
            Creates::Always,
            &Seen::Refused("exists".into())
        ));
        assert!(
            matches!(card_state(&created(false), true), WriteState::Done(label) if label == "Branch created")
        );
    }

    /// A conversation that comes back offers Review of its branch when its record says the
    /// branch exists, though no change was committed since it opened here.
    #[test]
    fn a_resumed_conversation_offers_review_of_a_branch_it_made_before() {
        let record = |branch_created| SessionRecord {
            session: "s1".into(),
            graph: "server/crm".into(),
            agent: "claude".into(),
            title: None,
            title_override: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            branch: "agent/20261021-101500".into(),
            branch_created,
            pinned: false,
            archived: false,
            cwd: PathBuf::from("/tmp/agent"),
        };
        let resumed = AgentBranch::resumed(&record(true), "main".into());
        assert_eq!(resumed.name, "agent/20261021-101500");
        assert_eq!(resumed.changes, 0);
        assert!(resumed.offers_review());
        assert!(!AgentBranch::resumed(&record(false), "main".into()).offers_review());
    }

    /// A streamed token lands in the last turn, and only that turn's blocks are made again:
    /// the earlier turns keep theirs, and the blocks are those a rebuild of the whole thread
    /// makes. A change in an earlier turn makes its turn's blocks and the later ones again. A
    /// message waiting for the agent ends the thread, before the callout of an agent that
    /// stopped, and is never kept as a turn's block.
    #[test]
    fn streaming_into_the_last_turn_keeps_the_earlier_turns_blocks() {
        let query = || Entry::Tool {
            id: "q".into(),
            title: String::new(),
            status: ToolCallStatus::Completed,
        };
        let mut entries = Entries::default();
        for entry in [
            Entry::User("first".into()),
            Entry::Agent("one".into()),
            query(),
            Entry::Thought("hm".into()),
            Entry::User("second".into()),
            Entry::Agent("tw".into()),
        ] {
            entries.push(entry);
        }
        let whole =
            |entries: &Entries, working| thread_blocks(&[], entries, 0, working, false, false).0;
        let changed = entries.rehash();
        assert_eq!(changed, 0);
        let (blocks, kept) = thread_blocks(&[], &entries, changed, true, false, false);
        assert_eq!(kept, 0);
        let first_turn = blocks
            .iter()
            .take_while(|block| !matches!(block, Block::Entry { index: 4, .. }))
            .count();
        assert_eq!(
            first_turn, 3,
            "the message, the reply, and the query with the thought"
        );

        if let Some(Entry::Agent(text)) = entries.last_mut() {
            text.push('o');
        }
        let changed = entries.rehash();
        assert_eq!(changed, 5);
        let (again, kept) = thread_blocks(&blocks, &entries, changed, true, false, false);
        assert_eq!(kept, first_turn, "the first turn's blocks are kept");
        assert_eq!(again, whole(&entries, true));

        // The turn ends: still only the last turn is made again, now with its actions.
        let (ended, kept) = thread_blocks(&again, &entries, entries.len(), false, false, true);
        assert_eq!(kept, first_turn);
        assert_eq!(ended[..ended.len() - 1], whole(&entries, false)[..]);
        assert_eq!(ended.last(), Some(&Block::Exited));

        // A message sent while the agent comes back ends the thread, before the callout.
        let (waiting, kept) = thread_blocks(&ended, &entries, entries.len(), false, true, true);
        assert_eq!(kept, first_turn);
        assert_eq!(
            waiting[waiting.len() - 2..],
            [Block::Waiting, Block::Exited]
        );
        let (alone, _) = thread_blocks(&[], &Entries::default(), 0, false, true, false);
        assert_eq!(
            alone,
            [Block::Waiting],
            "a first message, on an empty thread"
        );

        // A change in the first turn (a write card's outcome, a tool's status) makes it again.
        entries.change_each(|entry| match entry {
            Entry::Thought(text) => {
                text.push('m');
                true
            }
            _ => false,
        });
        let changed = entries.rehash();
        let (rebuilt, kept) = thread_blocks(&ended, &entries, changed, false, false, false);
        assert_eq!(kept, 0);
        assert_eq!(rebuilt, whole(&entries, false));
    }

    #[test]
    fn the_queue_sends_in_order_pauses_on_stop_and_lets_send_now_jump_it() {
        let mut queue = MessageQueue::default();
        queue.push("first".into());
        queue.push("second".into());
        assert_eq!(queue.on_turn_ended().as_deref(), Some("first"));
        // Stop pauses it; queuing again counts as sending again and resumes it.
        queue.paused = true;
        assert_eq!(queue.on_turn_ended(), None);
        queue.push("third".into());
        assert_eq!(queue.on_turn_ended().as_deref(), Some("second"));
        // "Send now" goes ahead of the line, once.
        queue.send_after_cancel("urgent".into());
        assert_eq!(queue.on_turn_ended().as_deref(), Some("urgent"));
        assert_eq!(queue.on_turn_ended().as_deref(), Some("third"));
        assert_eq!(queue.on_turn_ended(), None);
        // "Send now" twice before the turn stops: the later goes at once, the earlier is next.
        queue.send_after_cancel("first now".into());
        queue.send_after_cancel("second now".into());
        assert_eq!(queue.on_turn_ended().as_deref(), Some("second now"));
        assert_eq!(queue.on_turn_ended().as_deref(), Some("first now"));
        assert_eq!(queue.on_turn_ended(), None);
        // Taking one out by id leaves the rest in order.
        queue.push("a".into());
        queue.push("b".into());
        let b = queue.entries[1].id;
        assert_eq!(queue.take(b).as_deref(), Some("b"));
        assert_eq!(queue.entries.len(), 1);
        // One taken to send that could not go is first in line again.
        let taken = queue.on_turn_ended().unwrap();
        queue.push("c".into());
        queue.put_back(taken);
        assert_eq!(queue.on_turn_ended().as_deref(), Some("a"));
        assert_eq!(queue.on_turn_ended().as_deref(), Some("c"));
        // "Send now" on the last in line while no turn runs (the agent starting) puts it
        // first, not back at the end.
        assert!(!queue.holds_messages(), "nothing for a close to drop");
        queue.enqueue("d".into(), false);
        queue.enqueue("e".into(), false);
        let e = queue.entries[1].id;
        let now = queue.take(e).unwrap();
        queue.enqueue(now, true);
        assert_eq!(queue.on_turn_ended().as_deref(), Some("e"));
        assert_eq!(queue.on_turn_ended().as_deref(), Some("d"));
        // A message waiting for the stopped turn is work a close would drop too.
        queue.send_after_cancel("after".into());
        assert!(queue.holds_messages());
        assert_eq!(queue.on_turn_ended().as_deref(), Some("after"));
        assert!(!queue.holds_messages());
    }

    /// Stopping a turn, "Send now" and a turn's end answer each permission request still
    /// waiting "cancelled", as ACP asks of a client: the card says so, and no longer waits for
    /// a key that would answer a request nobody asks any more.
    #[test]
    fn a_cancelled_turn_answers_its_waiting_permission_requests() {
        let mut entries = Entries::default();
        let card = |reply, answer: Option<&str>| {
            Entry::Permission(PermissionCard {
                title: "Run a command".into(),
                options: Vec::new(),
                reply,
                answer: answer.map(str::to_owned),
            })
        };
        let (reply, mut asked) = oneshot::channel();
        entries.push(card(Some(reply), None));
        entries.push(card(None, Some("Allow once")));
        entries.push(Entry::Agent("Done.".into()));
        assert!(entries[0].waits());
        entries.cancel_permissions();
        assert!(matches!(
            asked.try_recv(),
            Ok(Some(RequestPermissionOutcome::Cancelled))
        ));
        assert!(!entries.iter().any(Entry::waits));
        let answer = |entry: &Entry| match entry {
            Entry::Permission(card) => card.answer.clone(),
            _ => None,
        };
        assert_eq!(answer(&entries[0]).as_deref(), Some("Cancelled"));
        assert_eq!(
            answer(&entries[1]).as_deref(),
            Some("Allow once"),
            "an answered card keeps its answer"
        );
    }

    /// An entry changed by any way there is changes its hash; the others keep theirs, so the
    /// thread measures again only what changed.
    #[test]
    fn a_changed_entry_changes_its_hash_and_no_other() {
        let mut entries = Entries::default();
        entries.push(Entry::User("question".into()));
        entries.push(Entry::Agent("Ans".into()));
        entries.rehash();
        let (first, second) = (entries.hash(0), entries.hash(1));
        if let Some(Entry::Agent(text)) = entries.last_mut() {
            text.push_str("wer");
        }
        entries.rehash();
        assert_eq!(entries.hash(0), first);
        assert_ne!(entries.hash(1), second);
        let second = entries.hash(1);
        entries.change_each(|entry| match entry {
            Entry::User(text) => {
                text.push('?');
                true
            }
            _ => false,
        });
        assert_eq!(entries.stale_from, 0, "stale from the entry changed");
        entries.rehash();
        assert_ne!(entries.hash(0), first);
        assert_eq!(entries.hash(1), second);
        // Changing nothing leaves every hash as it is.
        entries.change_each(|_| false);
        assert_eq!(entries.stale_from, entries.len());
        entries.clear();
        entries.rehash();
        assert!(entries.is_empty() && entries.hash(0) == 0);
    }

    #[test]
    fn a_run_of_steps_reads_as_one_sentence() {
        assert_eq!(activity_summary(1, 1), "Thought 1 time, used 1 tool");
        assert_eq!(activity_summary(0, 2), "Used 2 tools");
        assert_eq!(activity_summary(2, 0), "Thought 2 times");
    }
}
