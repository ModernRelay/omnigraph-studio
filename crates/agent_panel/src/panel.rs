//! The agent panel's view and state: the thread, approval cards, the composer, and the tools
//! run on the person's session.

use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use std::path::PathBuf;

use agent_client_protocol::schema::MaybeUndefined;
use agent_client_protocol::schema::v1::{
    ContentBlock, PermissionOption, PermissionOptionKind, PlanEntryStatus,
    RequestPermissionOutcome, SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectOptions,
    SessionUpdate, StopReason, ToolCallStatus,
};
use editor::Editor;
use futures::channel::{mpsc, oneshot};
use futures::future::Shared;
use futures::{FutureExt as _, StreamExt as _};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FollowMode,
    ListAlignment, ListState, Render, SharedString, Subscription, Task, WeakEntity, Window, list,
};
use graph_store::{GraphSession, OutcomeKind};
use serde_json::{Map, Value};
use studio_actions::agent::{Allow, AllowAlways, ClearQueue, Reject, Restart, Send, SendNow, Stop};
use studio_actions::graph::ReviewBranch;
use studio_settings::StudioSettings;
use studio_ui::ds;
use studio_workspace::{Item, ItemEvent, SavedItem, Workspace};
use ui::prelude::*;
use ui::{
    Callout, CircularProgress, CommonAnimationExt as _, ContextMenu, Indicator, KeyBinding,
    PopoverMenu, Severity, Tooltip,
};

use crate::cli_home::{self, CliHome};
use crate::gateway::{Gateway, GatewayEvent, Seen};
use crate::gateway_rules::{Sandbox, WriteSummary};
use crate::host::{AgentLaunch, HostCommand, HostEvent, Opened, SessionLaunch};
use crate::sessions::{AgentSessions, Live, SessionRecord};
use crate::socket::{ToolRequest, ToolSocket};
use crate::tools::{Adapter, StudioTool, ToolCall};

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
    let graph = workspace.session()?.read(cx).settings_key(cx);
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
/// starts and replays it.
pub fn open_session(
    workspace: &mut Workspace,
    id: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<AgentPanel>> {
    let session = workspace.session()?.clone();
    let handle = cx.entity().downgrade();
    let panel = workspace.open_item(format!("agent:{id}"), window, cx, move |window, cx| {
        AgentPanel::new(session, handle, Some(id), window, cx)
    })?;
    panel.update(cx, |panel, cx| panel.start_if_idle(cx));
    Some(panel)
}

/// Opens a new conversation in a tab of its own; the agent starts when the first message is
/// sent.
pub fn new_conversation(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<AgentPanel>> {
    let session = workspace.session()?.clone();
    let handle = cx.entity().downgrade();
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let key = format!(
        "agent:new:{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    workspace.open_item(key, window, cx, move |window, cx| {
        AgentPanel::new(session, handle, None, window, cx)
    })
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
    created: bool,
    changes: usize,
}

enum Decision {
    Allow,
    AllowAlways,
    Reject,
}

enum ProposalState {
    Waiting(oneshot::Sender<Decision>),
    Running,
    /// The commit it made, and what changed. A write that made none is `Done`.
    Committed {
        commit: String,
        nodes: i64,
        edges: i64,
    },
    Refused(String),
    Unknown,
    Rejected,
    /// A write that made no commit of its own went through, such as creating a branch.
    Done(String),
}

struct Proposal {
    summary: String,
    query: String,
    params: Option<Map<String, Value>>,
    branch: String,
    creates_from: Option<String>,
    state: ProposalState,
}

/// An `omnigraph_run` under way or done, and the stop that kills its command.
struct CliRun {
    task: Shared<Task<(String, bool)>>,
    stop: Option<oneshot::Sender<()>>,
}

struct PermissionCard {
    title: String,
    options: Vec<PermissionOption>,
    reply: Option<oneshot::Sender<RequestPermissionOutcome>>,
    answer: Option<String>,
}

enum Entry {
    User(String),
    Agent(String),
    Thought(String),
    Tool {
        id: String,
        title: String,
        status: ToolCallStatus,
        /// The arguments the agent called it with, as it reported them.
        input: Option<Value>,
        /// Which of Studio's tools it is, by what the agent's adapter said when it was called.
        studio: Option<StudioTool>,
    },
    Plan(Vec<(String, PlanEntryStatus)>),
    Permission(PermissionCard),
    Proposal(Proposal),
    Notice {
        text: String,
        error: bool,
    },
}

/// One item of the thread's list: an entry, a query the agent ran, a run of steps folded to
/// one line, or the callout of an agent that stopped. `lead` carries the agent's mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Block {
    Entry {
        index: usize,
        lead: bool,
    },
    Query {
        index: usize,
        lead: bool,
    },
    Activity {
        start: usize,
        end: usize,
        lead: bool,
    },
    Exited,
}

/// The thread's entries, and what each shows folded to a number, kept as they change: a
/// streamed token rehashes the entry it lands in, not the whole conversation (a tool's input
/// can hold a whole file). Every way to change an entry marks it, so none goes stale.
#[derive(Default)]
struct Entries {
    list: Vec<Entry>,
    hashes: Vec<u64>,
    /// The first entry whose hash may be stale.
    stale_from: usize,
}

impl std::ops::Deref for Entries {
    type Target = [Entry];

    fn deref(&self) -> &[Entry] {
        &self.list
    }
}

impl Entries {
    fn push(&mut self, entry: Entry) {
        self.list.push(entry);
    }

    fn get_mut(&mut self, index: usize) -> Option<&mut Entry> {
        self.stale_from = self.stale_from.min(index);
        self.list.get_mut(index)
    }

    fn last_mut(&mut self) -> Option<&mut Entry> {
        let last = self.list.len().checked_sub(1)?;
        self.get_mut(last)
    }

    /// Every entry, to change any of them.
    fn iter_mut(&mut self) -> std::slice::IterMut<'_, Entry> {
        self.stale_from = 0;
        self.list.iter_mut()
    }

    fn clear(&mut self) {
        self.list.clear();
        self.hashes.clear();
        self.stale_from = 0;
    }

    /// Brings the hashes up to the entries.
    fn rehash(&mut self) {
        self.hashes.truncate(self.stale_from.min(self.list.len()));
        for entry in &self.list[self.hashes.len()..] {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            hash_entry(entry, &mut hasher);
            self.hashes.push(std::hash::Hasher::finish(&hasher));
        }
        self.stale_from = self.list.len();
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
        Entry::Tool {
            title,
            status,
            input,
            ..
        } => {
            title.hash(hasher);
            format!("{status:?}").hash(hasher);
            input.as_ref().map(Value::to_string).hash(hasher);
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
        Entry::Proposal(proposal) => {
            proposal.summary.hash(hasher);
            proposal.query.hash(hasher);
            proposal.branch.hash(hasher);
            proposal.creates_from.hash(hasher);
            proposal
                .params
                .as_ref()
                .map(|params| Value::Object(params.clone()).to_string())
                .hash(hasher);
            match &proposal.state {
                ProposalState::Waiting(_) => 0u8.hash(hasher),
                ProposalState::Running => 1u8.hash(hasher),
                ProposalState::Committed {
                    commit,
                    nodes,
                    edges,
                } => {
                    2u8.hash(hasher);
                    commit.hash(hasher);
                    nodes.hash(hasher);
                    edges.hash(hasher);
                }
                ProposalState::Refused(reason) => {
                    3u8.hash(hasher);
                    reason.hash(hasher);
                }
                ProposalState::Unknown => 4u8.hash(hasher),
                ProposalState::Rejected => 5u8.hash(hasher),
                ProposalState::Done(label) => {
                    6u8.hash(hasher);
                    label.hash(hasher);
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
}

struct Host {
    commands: mpsc::UnboundedSender<HostCommand>,
    /// Keeps the agent's process running while this conversation is open.
    _connection: std::sync::Arc<crate::connection::AgentConnection>,
    _socket: ToolSocket,
    /// The conversation's CLI home (spec 013); its session token goes with it.
    cli_home: Option<std::sync::Arc<CliHome>>,
    _tasks: Vec<Task<()>>,
}

pub struct AgentPanel {
    session: Entity<GraphSession>,
    workspace: WeakEntity<Workspace>,
    /// The graph's conversations, which this tab records itself in.
    sessions: Entity<AgentSessions>,
    /// The tab's key until the agent names the session.
    new_key: SharedString,
    /// The agent's replies as rendered markdown, by entry index, with how much of the text each
    /// has taken in; a streaming reply appends its new tail.
    markdown: std::collections::HashMap<usize, (Entity<Markdown>, usize)>,
    focus_handle: FocusHandle,
    composer: Entity<Editor>,
    custom_command: Entity<Editor>,
    /// The thread as a list that lays out only what shows (as Zed's agent thread does), and
    /// the blocks it last drew with their fingerprints, to remeasure only what changed.
    thread: ListState,
    shown_blocks: Vec<(Block, u64)>,
    /// Each reply's markdown is watched: its parse finishes after the text arrives, and the
    /// block grows then.
    _markdown_watch: std::collections::HashMap<usize, Subscription>,
    state: State,
    entries: Entries,
    branch: Option<AgentBranch>,
    always_allow: bool,
    /// The adapter the agent runs as, which says which of its tool calls are Studio's.
    adapter: Adapter,
    host: Option<Host>,
    /// The agent comes back after it stopped, with the conversation on screen: when it replays
    /// the conversation (`session/load`), the replay replaces what is shown instead of adding
    /// to it.
    replace_on_replay: bool,
    /// The conversation this tab holds: the one it loads before the agent answers, then the one
    /// the agent opened. `None` starts a new one.
    session_id: Option<String>,
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
    usage: Option<Usage>,
    /// Runs of tool calls and thoughts opened to their steps, by the run's first entry.
    expanded_activity: std::collections::HashSet<usize>,
    /// Query calls whose card is folded away, by entry.
    folded_queries: std::collections::HashSet<usize>,
    queue: MessageQueue,
    /// Whether a CLI that speaks the graph's API was found for this conversation (spec 013);
    /// `None` while Studio looks.
    cli_ready: Option<bool>,
    /// `omnigraph_run` calls made while Studio looks, answered once it knows.
    cli_waiting: Vec<oneshot::Sender<()>>,
    /// CLI runs whose outcome has not reached the agent yet, by run number. A run outlives the
    /// call that started it when it takes longer than `RUN_ANSWER_WITHIN`. A run stops (its
    /// command killed) when its call is cancelled, at Stop and with a new session.
    runs: std::collections::HashMap<u64, CliRun>,
    next_run: u64,
    /// The card of each write the gateway forwards, by the gateway's id: the entry, whether
    /// the write creates the agent's branch, and its summary.
    gateway_cards: std::collections::HashMap<u64, (usize, bool, String)>,
}

impl EventEmitter<ItemEvent> for AgentPanel {}

impl Focusable for AgentPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for AgentPanel {
    /// The conversation's title, as the sidebar lists it.
    fn tab_title(&self, cx: &App) -> SharedString {
        self.session_id
            .as_ref()
            .and_then(|id| self.sessions.read(cx).get(id))
            .and_then(|record| record.title.clone())
            .map(|title| cut(&title, TAB_TITLE_CHARS).into())
            .unwrap_or_else(|| "New conversation".into())
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::ZedAssistant)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(match &self.session_id {
            Some(id) => format!("agent:{id}").into(),
            None => self.new_key.clone(),
        })
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
            editor.set_placeholder_text("program and arguments, e.g. my-agent --acp", window, cx);
            editor
        });
        cx.observe_global::<StudioSettings>(|this, cx| {
            this.sync_setup_state(cx);
            cx.notify();
        })
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
        // list learns what it is doing now; a closed tab's agent stops with it.
        cx.observe_self(|this, cx| this.sync_live(cx)).detach();
        cx.on_release(|this, cx| {
            if let Some(id) = this.session_id.clone() {
                this.sessions
                    .update(cx, |sessions, cx| sessions.set_live(&id, None, cx));
            }
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
        let mut panel = Self {
            session,
            workspace,
            sessions,
            new_key: format!("agent:new:{}", cx.entity_id()).into(),
            focus_handle,
            composer,
            custom_command,
            thread: {
                let thread = ListState::new(0, ListAlignment::Top, px(800.));
                thread.set_follow_mode(FollowMode::Tail);
                thread
            },
            shown_blocks: Vec::new(),
            _markdown_watch: std::collections::HashMap::new(),
            state: State::Setup,
            entries: Entries::default(),
            markdown: std::collections::HashMap::new(),
            branch: None,
            always_allow: false,
            adapter: Adapter::Other,
            host: None,
            replace_on_replay: false,
            session_id,
            failed: false,
            cwd: None,
            _focus: Some(focus),
            config_options: Vec::new(),
            setting_option: None,
            usage: None,
            expanded_activity: std::collections::HashSet::new(),
            folded_queries: std::collections::HashSet::new(),
            queue: MessageQueue::default(),
            cli_ready: None,
            cli_waiting: Vec::new(),
            runs: std::collections::HashMap::new(),
            next_run: 1,
            gateway_cards: std::collections::HashMap::new(),
        };
        panel.sync_setup_state(cx);
        panel
    }

    fn command(cx: &App) -> Vec<String> {
        StudioSettings::get(cx).agent.command.clone()
    }

    fn sync_setup_state(&mut self, cx: &App) {
        let has_command = !Self::command(cx).is_empty();
        match self.state {
            State::Setup if has_command => self.state = State::Idle,
            State::Idle if !has_command => self.state = State::Setup,
            _ => {}
        }
    }

    // ---- lifecycle ----

    /// Starts the agent at once for a conversation to come back to, so it is on screen
    /// without sending anything first.
    pub fn start_if_idle(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, State::Idle) && self.session_id.is_some() {
            self.start(cx);
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        // The agent's `omnigraph` wrapper is a shell script (`cli_home`); Windows needs one of
        // its own, with its own quoting, before an agent may run there.
        if cfg!(windows) {
            self.fail_start(
                "The agent panel is not available on Windows yet; the rest of Studio is.".into(),
                cx,
            );
            return;
        }
        let settings = StudioSettings::get(cx).agent.clone();
        if settings.command.is_empty() {
            self.state = State::Setup;
            return;
        }
        // The conversation this tab holds, to load (spec 011: resuming a session).
        let saved = self
            .session_id
            .as_ref()
            .and_then(|id| self.sessions.read(cx).get(id).cloned());
        // A conversation runs where it ran before; a new one in the graph's own directory.
        let cwd = match (&saved, &self.session_id) {
            (Some(saved), _) => saved
                .cwd
                .clone()
                .unwrap_or_else(crate::sessions::legacy_cwd),
            (None, Some(_)) => crate::sessions::legacy_cwd(),
            (None, None) => crate::sessions::graph_cwd(&self.session.read(cx).settings_key(cx)),
        };
        if let Err(error) = std::fs::create_dir_all(&cwd) {
            self.fail_start(
                format!("Studio could not create {}: {error}", cwd.display()),
                cx,
            );
            return;
        }
        let mut socket = match ToolSocket::bind(&crate::socket::socket_dir()) {
            Ok(socket) => socket,
            Err(error) => {
                self.fail_start(
                    format!("Studio could not open the tool socket: {error}"),
                    cx,
                );
                return;
            }
        };
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                self.fail_start(
                    format!("Studio could not find its own executable: {error}"),
                    cx,
                );
                return;
            }
        };
        let base = self.session.read(cx).branch.clone();
        let graph_key = self.session.read(cx).settings_key(cx);
        // A resumed conversation keeps writing to its branch.
        self.failed = false;
        self.branch = Some(match &saved {
            Some(saved) => AgentBranch {
                name: saved.branch.clone(),
                base,
                created: saved.branch_created,
                changes: 0,
            },
            None => AgentBranch {
                name: self.sessions.read(cx).unused_branch(
                    &graph_key,
                    format!("agent/{}", chrono::Local::now().format("%Y%m%d-%H%M%S")),
                ),
                base,
                created: false,
                changes: 0,
            },
        });
        self.always_allow = false;

        // The agent's omnigraph CLI (spec 013): a gateway for this conversation, and a home that
        // points the CLI at it. Without them the agent still has the tools.
        let (agent_branch, agent_base) = self
            .branch
            .as_ref()
            .map(|branch| (branch.name.clone(), branch.base.clone()))
            .unwrap_or_default();
        let graph = self.session.read(cx).graph_id.clone();
        let (gateway_tx, mut gateway_rx) = mpsc::unbounded::<GatewayEvent>();
        self.cli_ready = None;
        self.cli_waiting.clear();
        self.stop_runs();
        self.runs.clear();
        self.gateway_cards.clear();
        let gateway = Gateway::bind().and_then(|mut gateway| {
            let home = CliHome::write(
                &cwd,
                &crate::sessions::private_homes(),
                gateway.port,
                &gateway.token,
                &graph,
                &agent_branch,
                &agent_base,
            )?;
            let serve = gateway.serve(
                self.session.read(cx).gateway_client(cx),
                Sandbox {
                    graph: graph.clone(),
                    branch: agent_branch.clone(),
                    base: agent_base.clone(),
                },
                gateway_tx,
            );
            Ok((std::sync::Arc::new(home), serve))
        });
        let (cli_home, gateway_serve) = match gateway {
            Ok((home, serve)) => (Some(home), Some(serve)),
            Err(error) => {
                self.cli_ready = Some(false);
                self.entries.push(Entry::Notice {
                    text: format!(
                        "Studio could not set up the omnigraph CLI for this conversation \
                         ({error}), so the agent cannot reach the graph."
                    ),
                    error: true,
                });
                (None, None)
            }
        };
        // The shared rules and the skill help the agent; without them it still has the CLI.
        if let Err(error) =
            cli_home::write_agents_md(&cwd, &graph).and_then(|()| crate::skill::write(&cwd))
        {
            self.entries.push(Entry::Notice {
                text: format!(
                    "Studio could not write AGENTS.md and the omnigraph-studio skill for the \
                     agent ({error}); it can still reach the graph."
                ),
                error: true,
            });
        }
        let instructions = cli_home
            .as_ref()
            .map(|home| home.instructions(None))
            .unwrap_or_default();

        self.cwd = Some(cwd.clone());
        let launch = SessionLaunch {
            cwd,
            mcp_command: proxy_executable(executable),
            mcp_args: vec![
                crate::PROXY_COMMAND.to_owned(),
                socket.path.to_string_lossy().into_owned(),
            ],
            resume: self.session_id.clone(),
        };
        let (events_tx, mut events_rx) = mpsc::unbounded();
        let (commands_tx, commands_rx) = mpsc::unbounded();
        let (tools_tx, mut tools_rx) = mpsc::unbounded::<ToolRequest>();
        // One process per agent, shared with the graph's other conversations.
        let connection = crate::connection::connection(
            AgentLaunch {
                command: settings.command,
                env: settings.env,
            },
            cx,
        );
        if !connection.open(launch, events_tx, commands_rx) {
            self.fail_start("The agent is not running.".into(), cx);
            return;
        }

        let serve = cx.background_spawn(socket.serve(tools_tx, instructions));
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
            let configured = settings.omnigraph_cli;
            tasks.push(cx.spawn(async move |this, cx| {
                let wanted = configured.clone();
                let chosen = cx
                    .background_spawn(async move {
                        let candidates = cli_home::candidates(wanted.as_deref());
                        let contract = graph_client::SUPPORTED_CONTRACTS[0];
                        cli_home::choose_cli(&home, candidates, contract).await
                    })
                    .await;
                this.update(cx, |this, cx| this.cli_chosen(chosen, configured, cx))
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
        let tool_pump = cx.spawn(async move |this, cx| {
            while let Some(request) = tools_rx.next().await {
                if this
                    .update(cx, |this, cx| this.run_tool(request, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        tasks.extend([serve, event_pump, tool_pump]);
        self.host = Some(Host {
            commands: commands_tx,
            _connection: connection,
            _socket: socket,
            cli_home,
            _tasks: tasks,
        });
        self.state = State::Starting;
        cx.notify();
    }

    fn fail_start(&mut self, message: String, cx: &mut Context<Self>) {
        self.state = State::Exited {
            message,
            stderr: String::new(),
        };
        cx.notify();
    }

    fn stop_host(&mut self) {
        self.host = None;
        // No agent is left to take a run's outcome.
        self.stop_runs();
        // Options can only be set on a running session.
        self.config_options.clear();
        self.setting_option = None;
        for entry in self.entries.iter_mut() {
            match entry {
                Entry::Permission(card) if card.reply.is_some() => {
                    card.reply = None;
                    card.answer = Some("Cancelled".into());
                }
                Entry::Proposal(proposal)
                    if matches!(proposal.state, ProposalState::Waiting(_)) =>
                {
                    proposal.state = ProposalState::Rejected;
                }
                _ => {}
            }
        }
    }

    /// Starts the agent again on this conversation, after it stopped: it replays the
    /// conversation when it can load it, else a new one begins in this tab.
    fn restart(&mut self, _: &Restart, _window: &mut Window, cx: &mut Context<Self>) {
        self.stop_host();
        self.clear_thread();
        self.start(cx);
    }

    /// Forgets the thread on screen, before the agent shows it again.
    fn clear_thread(&mut self) {
        self.entries.clear();
        self.markdown.clear();
        self.expanded_activity.clear();
        self.folded_queries.clear();
        self.usage = None;
    }

    /// Records the open session, to come back to it at the next start.
    fn remember_session(&self, cx: &mut Context<Self>) {
        let (Some(session), Some(branch)) = (&self.session_id, &self.branch) else {
            return;
        };
        let now = chrono::Utc::now();
        let record = SessionRecord {
            session: session.clone(),
            graph: self.session.read(cx).settings_key(cx),
            agent: Self::command(cx).join(" "),
            title: None,
            title_override: None,
            created_at: now,
            updated_at: now,
            branch: branch.name.clone(),
            branch_created: branch.created,
            pinned: false,
            archived: false,
            cwd: self.cwd.clone(),
        };
        self.sessions
            .update(cx, |sessions, cx| sessions.upsert(record, cx));
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
            HostEvent::Adapter(adapter) => self.adapter = adapter,
            HostEvent::Ready {
                agent,
                session,
                opened,
            } => {
                self.session_id = Some(session);
                self.remember_session(cx);
                cx.emit(ItemEvent::UpdateTab);
                // A loaded conversation replays itself and keeps its own options; a new one
                // starts from the ones chosen last for this agent.
                match opened {
                    Opened::New => self.apply_preferences(cx),
                    Opened::Loaded => {}
                    Opened::Resumed => self.entries.push(Entry::Notice {
                        text: "Resumed: the agent has the earlier messages but cannot show them."
                            .into(),
                        error: false,
                    }),
                }
                self.state = State::Ready { agent };
                self.replace_on_replay = false;
                // What was sent before the agent was ready goes now, in order.
                if let Some(text) = self.queue.on_turn_ended() {
                    self.prompt(text, cx);
                }
            }
            HostEvent::Update(update) => {
                if std::mem::take(&mut self.replace_on_replay) {
                    self.clear_thread();
                }
                self.apply_update(update, cx)
            }
            HostEvent::ConfigOptions(options) => {
                self.config_options = options;
                self.setting_option = None;
            }
            HostEvent::ConfigOptionFailed { message } => {
                self.setting_option = None;
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
                if let Some(next) = self.queue.on_turn_ended() {
                    self.prompt(next, cx);
                }
            }
            HostEvent::Exited {
                message,
                stderr,
                failed,
            } => {
                self.stop_host();
                self.settle_open_tools(ToolCallStatus::Failed);
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
        for entry in self.entries.iter_mut() {
            if let Entry::Tool { status, .. } = entry
                && matches!(status, ToolCallStatus::Pending | ToolCallStatus::InProgress)
            {
                *status = ended;
            }
        }
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
                let studio =
                    StudioTool::of(self.adapter, call.meta.as_ref(), call.raw_input.as_ref());
                self.entries.push(Entry::Tool {
                    id: call.tool_call_id.to_string(),
                    title: call.title,
                    status: call.status,
                    input: call.raw_input,
                    studio,
                });
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let id = update.tool_call_id.to_string();
                let adapter = self.adapter;
                let at = self.entries.iter().rposition(
                    |entry| matches!(entry, Entry::Tool { id: existing, .. } if *existing == id),
                );
                if let Some(Entry::Tool {
                    title,
                    status,
                    input,
                    studio,
                    ..
                }) = at.and_then(|at| self.entries.get_mut(at))
                {
                    if let Some(new_title) = update.fields.title.clone() {
                        *title = new_title;
                    }
                    if let Some(new_status) = update.fields.status {
                        *status = new_status;
                    }
                    if studio.is_none() {
                        *studio = StudioTool::of(
                            adapter,
                            update.meta.as_ref(),
                            update.fields.raw_input.as_ref(),
                        );
                    }
                    if let Some(new_input) = update.fields.raw_input {
                        *input = Some(new_input);
                    }
                }
            }
            SessionUpdate::ConfigOptionUpdate(update) => {
                self.config_options = update.config_options;
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
        crate::sessions::prefer(&Self::command(cx), &id, remembered);
        if host
            .commands
            .unbounded_send(HostCommand::SetConfigOption {
                id: id.clone(),
                value: value.clone(),
            })
            .is_ok()
        {
            self.setting_option = Some((id, value));
        }
        cx.notify();
    }

    /// Sets the options chosen last for this agent where the new session differs and still
    /// offers the value.
    fn apply_preferences(&mut self, cx: &mut Context<Self>) {
        let preferences = crate::sessions::preferences(&Self::command(cx));
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

    pub fn send_text(&mut self, text: String, _window: &mut Window, cx: &mut Context<Self>) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        // A message sent while the agent works, starts or is still to be chosen waits its
        // turn in the queue, with any sent before it.
        match &self.state {
            State::Ready { .. } => self.prompt(text, cx),
            State::Prompting { .. } | State::Starting => self.queue.push(text),
            State::Setup => {
                self.queue.push(text);
                self.entries.push(Entry::Notice {
                    text: "Choose an agent to send this to.".into(),
                    error: false,
                });
            }
            State::Idle | State::Exited { .. } => {
                self.queue.push(text);
                self.replace_on_replay = !self.entries.is_empty();
                self.start(cx);
            }
        }
        cx.notify();
    }

    fn prompt(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(host) = &self.host else { return };
        let agent = match &self.state {
            State::Ready { agent } => agent.clone(),
            _ => return,
        };
        if host
            .commands
            .unbounded_send(HostCommand::Prompt(text.clone()))
            .is_ok()
        {
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
        self.send_at_once(text, window, cx);
    }

    fn send_at_once(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.state, State::Prompting { .. })
            && let Some(host) = &self.host
        {
            self.queue.after_cancel = Some(text);
            host.commands.unbounded_send(HostCommand::Cancel).ok();
            cx.notify();
        } else {
            self.queue.paused = false;
            self.send_text(text, window, cx);
        }
    }

    /// Sends a queued message at once, stopping the turn if one runs.
    fn send_queued_now(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.queue.take(id) {
            self.send_at_once(text, window, cx);
        }
    }

    /// Puts a queued message back in the composer to edit, ahead of what is there.
    fn edit_queued(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.queue.take(id) else {
            return;
        };
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

    /// Stops the turn and the CLI runs it started; the queue waits until the person sends
    /// again.
    fn stop(&mut self, _: &Stop, _window: &mut Window, cx: &mut Context<Self>) {
        self.stop_runs();
        if let Some(host) = &self.host {
            host.commands.unbounded_send(HostCommand::Cancel).ok();
            if !self.queue.entries.is_empty() {
                self.queue.paused = true;
                cx.notify();
            }
        }
    }

    // ---- approvals ----

    /// The oldest card still waiting for the person.
    fn pending_index(&self) -> Option<usize> {
        self.entries.iter().position(|entry| match entry {
            Entry::Permission(card) => card.reply.is_some(),
            Entry::Proposal(proposal) => matches!(proposal.state, ProposalState::Waiting(_)),
            _ => false,
        })
    }

    fn decide(&mut self, index: usize, decision: Decision, cx: &mut Context<Self>) {
        match self.entries.get_mut(index) {
            Some(Entry::Proposal(proposal)) => {
                if let ProposalState::Waiting(_) = proposal.state {
                    let ProposalState::Waiting(reply) =
                        std::mem::replace(&mut proposal.state, ProposalState::Running)
                    else {
                        return;
                    };
                    reply.send(decision).ok();
                }
            }
            Some(Entry::Permission(card)) => {
                let Some(reply) = card.reply.take() else {
                    return;
                };
                let wanted: &[PermissionOptionKind] = match decision {
                    Decision::Allow => &[PermissionOptionKind::AllowOnce],
                    Decision::AllowAlways => &[
                        PermissionOptionKind::AllowAlways,
                        PermissionOptionKind::AllowOnce,
                    ],
                    Decision::Reject => &[
                        PermissionOptionKind::RejectOnce,
                        PermissionOptionKind::RejectAlways,
                    ],
                };
                let option = wanted
                    .iter()
                    .find_map(|kind| card.options.iter().find(|o| o.kind == *kind));
                match option {
                    Some(option) => {
                        card.answer = Some(option.name.clone());
                        reply
                            .send(RequestPermissionOutcome::Selected(
                                SelectedPermissionOutcome::new(option.option_id.clone()),
                            ))
                            .ok();
                    }
                    None => {
                        card.answer = Some("Cancelled".into());
                        reply.send(RequestPermissionOutcome::Cancelled).ok();
                    }
                }
            }
            _ => {}
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
        if let Some(index) = self.pending_index() {
            self.decide(index, Decision::Allow, cx);
        }
    }

    fn allow_always(&mut self, _: &AllowAlways, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.pending_index() {
            self.decide(index, Decision::AllowAlways, cx);
        }
    }

    fn reject(&mut self, _: &Reject, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.pending_index() {
            self.decide(index, Decision::Reject, cx);
        }
    }

    // ---- tools ----

    fn run_tool(&mut self, request: ToolRequest, cx: &mut Context<Self>) {
        let ToolRequest {
            name,
            arguments,
            reply,
            cancelled,
        } = request;
        match ToolCall::parse(&name, &arguments) {
            Ok(ToolCall::Cli) => {
                reply.send(self.cli_instructions()).ok();
            }
            Ok(ToolCall::Run { args, save_to }) => {
                self.run_cli(&args, save_to.as_deref(), reply, cancelled, cx);
            }
            Ok(ToolCall::Wait { run }) => {
                self.answer_run(run, reply, cx);
            }
            Err(error) => {
                reply.send((error, true)).ok();
            }
        }
    }

    fn apply_gateway(&mut self, event: GatewayEvent, cx: &mut Context<Self>) {
        match event {
            GatewayEvent::Write { id, write, decide } => self.gateway_write(id, write, decide, cx),
            GatewayEvent::Finished { id, outcome } => self.gateway_finished(id, outcome, cx),
            GatewayEvent::Abandoned { id } => {
                if let Some((index, ..)) = self.gateway_cards.remove(&id) {
                    self.set_proposal_state(
                        index,
                        ProposalState::Refused(
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
        decide: oneshot::Sender<bool>,
        cx: &mut Context<Self>,
    ) {
        let Some(branch) = &self.branch else {
            decide.send(false).ok();
            return;
        };
        let ask = StudioSettings::get(cx).agent.ask_before_writes && !self.always_allow;
        let index = self.entries.len();
        let (state, waiting) = if ask {
            let (reply, decision) = oneshot::channel();
            (ProposalState::Waiting(reply), Some(decision))
        } else {
            (ProposalState::Running, None)
        };
        self.entries.push(Entry::Proposal(Proposal {
            summary: write.summary.clone(),
            query: write.query.clone().unwrap_or_default(),
            params: write.params.clone(),
            branch: branch.name.clone(),
            creates_from: write.creates_branch.then(|| branch.base.clone()),
            state,
        }));
        self.gateway_cards
            .insert(id, (index, write.creates_branch, write.summary));
        match waiting {
            None => {
                decide.send(true).ok();
            }
            Some(decision) => {
                cx.spawn(async move |this, cx| {
                    // No decision when the card was settled otherwise (the CLI gave up): the
                    // write is not sent, and the card keeps what it says.
                    let decision = decision.await.ok();
                    let allow = matches!(decision, Some(Decision::Allow | Decision::AllowAlways));
                    this.update(cx, |this, cx| {
                        if let Some(Decision::AllowAlways) = decision {
                            this.always_allow = true;
                        }
                        if let Some(Decision::Reject) = decision {
                            // Nothing is sent, so no outcome comes for the card.
                            this.gateway_cards.remove(&id);
                            this.set_proposal_state(index, ProposalState::Rejected, cx);
                        }
                    })
                    .ok();
                    decide.send(allow).ok();
                })
                .detach();
            }
        }
        self.thread.scroll_to_end();
        cx.notify();
    }

    fn gateway_finished(&mut self, id: u64, outcome: Seen, cx: &mut Context<Self>) {
        let Some((index, creates_branch, summary)) = self.gateway_cards.remove(&id) else {
            return;
        };
        let (state, kind) = match &outcome {
            Seen::Committed {
                commit: Some(commit),
                nodes,
                edges,
            } => (
                ProposalState::Committed {
                    commit: commit.clone(),
                    nodes: *nodes,
                    edges: *edges,
                },
                OutcomeKind::Committed {
                    commit: Some(commit.clone()),
                },
            ),
            Seen::Committed { commit: None, .. } => (
                ProposalState::Done(if creates_branch {
                    "Branch created".into()
                } else {
                    "Done".into()
                }),
                OutcomeKind::Committed { commit: None },
            ),
            Seen::Refused(message) => (
                ProposalState::Refused(message.clone()),
                OutcomeKind::Refused {
                    message: message.clone(),
                },
            ),
            Seen::Unknown(reason) => (
                ProposalState::Unknown,
                OutcomeKind::Unknown {
                    reason: reason.clone(),
                },
            ),
        };
        let went_through = matches!(outcome, Seen::Committed { .. });
        let mut created = false;
        let mut branch_name = String::new();
        if let Some(branch) = &mut self.branch {
            branch_name = branch.name.clone();
            if went_through && creates_branch && !branch.created {
                branch.created = true;
                created = true;
            }
            if matches!(
                outcome,
                Seen::Committed {
                    commit: Some(_),
                    ..
                }
            ) {
                branch.changes += 1;
            }
        }
        if created {
            self.remember_session(cx);
        }
        self.session.update(cx, |session, cx| {
            session.note_write(
                branch_name,
                format!("Agent CLI: {summary}"),
                kind,
                created,
                cx,
            )
        });
        self.set_proposal_state(index, state, cx);
    }

    /// Studio found (or did not find) a CLI that speaks the graph's API for this conversation.
    /// A configured CLI it passed over, and every CLI it found when none fits, are named.
    fn cli_chosen(
        &mut self,
        choice: cli_home::CliChoice,
        configured: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let contract = graph_client::SUPPORTED_CONTRACTS[0];
        self.cli_ready = Some(matches!(choice, cli_home::CliChoice::Chosen { .. }));
        for waiting in self.cli_waiting.drain(..) {
            waiting.send(()).ok();
        }
        match choice {
            cli_home::CliChoice::Chosen { cli, version } => {
                let configured = configured
                    .filter(|path| !path.is_empty())
                    .map(|path| cli_home::expand_home(&path));
                if let Some(configured) = configured
                    && std::fs::canonicalize(&configured).ok() != std::fs::canonicalize(&cli).ok()
                {
                    self.entries.push(Entry::Notice {
                        text: format!(
                            "`agent.omnigraph_cli` ({}) does not speak this graph's HTTP API \
                             ({contract}), so the agent uses {}{}.",
                            configured.display(),
                            cli.display(),
                            version.map(|v| format!(" ({v})")).unwrap_or_default()
                        ),
                        error: false,
                    });
                }
            }
            cli_home::CliChoice::NoMatch { found } => {
                self.entries.push(Entry::Notice {
                    text: format!(
                        "{} The agent cannot reach the graph until one is.",
                        cli_home::no_cli_text(contract, &found)
                    ),
                    error: false,
                });
            }
        }
        cx.notify();
    }

    /// Runs the conversation's CLI for the agent from Studio (`omnigraph_run`, spec 013), for an
    /// agent whose sandbox cannot reach the gateway. The arguments are checked first, since the
    /// CLI then runs outside the agent's sandbox.
    fn run_cli(
        &mut self,
        args: &[String],
        save_to: Option<&str>,
        reply: oneshot::Sender<(String, bool)>,
        cancelled: oneshot::Receiver<()>,
        cx: &mut Context<Self>,
    ) {
        let (Some(home), Some(cwd)) = (
            self.host.as_ref().and_then(|host| host.cli_home.clone()),
            self.cwd.clone(),
        ) else {
            reply.send(self.cli_instructions()).ok();
            return;
        };
        match self.cli_ready {
            Some(true) => {}
            None => {
                // The call waits until Studio has found (or not found) the CLI.
                let (ready, known) = oneshot::channel();
                self.cli_waiting.push(ready);
                let (args, save_to) = (args.to_vec(), save_to.map(str::to_owned));
                cx.spawn(async move |this, cx| {
                    if known.await.is_err() {
                        reply
                            .send((
                                "The conversation ended before the command ran.".into(),
                                true,
                            ))
                            .ok();
                        return;
                    }
                    this.update(cx, |this, cx| {
                        this.run_cli(&args, save_to.as_deref(), reply, cancelled, cx)
                    })
                    .ok();
                })
                .detach();
                return;
            }
            Some(false) => {
                reply.send(self.cli_instructions()).ok();
                return;
            }
        }
        let checked = cli_home::check_run_args(args, &cwd, &home.graph).and_then(|mut plan| {
            plan.save_to = save_to
                .map(|path| {
                    cli_home::inside(&cwd, path).map(|parts| cli_home::RunFile {
                        parts,
                        output: true,
                    })
                })
                .transpose()?;
            Ok(plan)
        });
        let plan = match checked {
            Ok(plan) => plan,
            Err(message) => {
                reply.send((message, true)).ok();
                return;
            }
        };
        let run = self.next_run;
        self.next_run += 1;
        let (stop, stopped) = oneshot::channel::<()>();
        let task = cx
            .background_spawn(async move {
                let ran = cli_home::run(&home, &cwd, plan).fuse();
                // Only a stop sent stops it, never one dropped with its entry.
                let stopped = async {
                    if stopped.await.is_err() {
                        futures::future::pending::<()>().await;
                    }
                }
                .fuse();
                futures::pin_mut!(ran, stopped);
                // Dropping the run kills its command.
                futures::select! {
                    done = ran => done,
                    () = stopped => (
                        "The run was stopped before it finished; what it had already written \
                         stays."
                            .into(),
                        true,
                    ),
                }
            })
            .shared();
        self.runs.insert(
            run,
            CliRun {
                task,
                stop: Some(stop),
            },
        );
        // The agent cancelling the call while it waits for the run stops the run.
        cx.spawn(async move |this, cx| {
            if cancelled.await.is_ok() {
                this.update(cx, |this, _| this.stop_run(run)).ok();
            }
        })
        .detach();
        self.answer_run(run, reply, cx);
    }

    fn stop_run(&mut self, run: u64) {
        if let Some(stop) = self.runs.get_mut(&run).and_then(|run| run.stop.take()) {
            stop.send(()).ok();
        }
    }

    /// Stops every run still going: at Stop, when the agent stops and when a new session
    /// starts.
    fn stop_runs(&mut self) {
        for run in self.runs.values_mut() {
            if let Some(stop) = run.stop.take() {
                stop.send(()).ok();
            }
        }
    }

    /// Answers with run `run`'s outcome once it finishes, or with its number if it is still
    /// going after `RUN_ANSWER_WITHIN`, so the agent can wait for it again.
    fn answer_run(
        &mut self,
        run: u64,
        reply: oneshot::Sender<(String, bool)>,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self.runs.get(&run).map(|run| run.task.clone()) else {
            reply
                .send((
                    format!(
                        "There is no run {run} waiting in this conversation: its outcome was \
                         already returned, or the conversation restarted."
                    ),
                    true,
                ))
                .ok();
            return;
        };
        cx.spawn(async move |this, cx| {
            match cli_home::answer_within(task, cli_home::RUN_ANSWER_WITHIN).await {
                Some(answer) => {
                    this.update(cx, |this, _| this.runs.remove(&run)).ok();
                    reply.send(answer).ok();
                }
                None => {
                    reply.send(cli_home::still_running(run)).ok();
                }
            }
        })
        .detach();
    }

    /// What the `omnigraph_cli` tool answers: this conversation's CLI, or why it has none.
    fn cli_instructions(&self) -> (String, bool) {
        match self.host.as_ref().and_then(|host| host.cli_home.as_ref()) {
            Some(home) => match self.cli_ready {
                Some(false) => (home.instructions(Some(false)), true),
                ready => (home.instructions(ready), false),
            },
            None => (
                "The omnigraph CLI is not available in this conversation; tell the person.".into(),
                true,
            ),
        }
    }

    fn set_proposal_state(&mut self, index: usize, state: ProposalState, cx: &mut Context<Self>) {
        if let Some(Entry::Proposal(proposal)) = self.entries.get_mut(index) {
            proposal.state = state;
        }
        cx.notify();
    }

    fn review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(branch) = self.branch.as_ref().filter(|b| b.created) else {
            return;
        };
        let name = branch.name.clone();
        self.session
            .update(cx, |session, cx| session.set_branch(name, cx));
        window.dispatch_action(Box::new(ReviewBranch), cx);
    }

    fn use_command(&mut self, command: Vec<String>, cx: &mut Context<Self>) {
        studio_settings::update(cx, move |settings| settings.agent.command = command);
        self.sync_setup_state(cx);
        // A question asked before an agent was chosen is sent to it now, as the setup said.
        if matches!(self.state, State::Idle) && !self.queue.entries.is_empty() {
            self.start(cx);
        }
        cx.notify();
    }
}

/// The proxy runs Studio's own executable. Test binaries and examples are not Studio, so they
/// name the built `omnigraph-studio` next to them when it exists.
fn proxy_executable(current: PathBuf) -> PathBuf {
    if let Ok(path) = std::env::var("OMNIGRAPH_STUDIO_PROXY")
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    current
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
    fn add_to(&self, mut menu: ContextMenu, panel: &WeakEntity<AgentPanel>) -> ContextMenu {
        match &self.choice {
            Choice::Select {
                current, sections, ..
            } => {
                for (index, (header, values)) in sections.iter().enumerate() {
                    if index > 0 {
                        menu = menu.separator();
                    }
                    menu = menu.header(header.clone());
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
    option: OptionView,
    icon: Icon,
    panel: &WeakEntity<AgentPanel>,
    agent: Option<String>,
) -> AnyElement {
    let id = option.id.clone();
    let title = option.summary();
    let meta = option.description.clone();
    let panel = panel.clone();
    PopoverMenu::new(SharedString::from(format!("option-{id}")))
        .trigger(
            ds::button(
                SharedString::from(format!("option-{id}-button")),
                cut(&option.current_name(), OPTION_NAME_CHARS),
            )
            .style(ButtonStyle::Subtle)
            .color(Color::Muted)
            .disabled(option.pending)
            .start_icon(icon.size(IconSize::XSmall).color(Color::Muted))
            .tooltip(move |_, cx| match &meta {
                Some(meta) => Tooltip::with_meta(title.clone(), None, meta.clone(), cx),
                None => Tooltip::simple(title.clone(), cx),
            }),
        )
        .menu(move |window, cx| {
            let option = option.clone();
            let panel = panel.clone();
            let agent = agent.clone();
            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                let mut menu = option.add_to(menu, &panel);
                if let Some(agent) = agent {
                    menu = menu
                        .separator()
                        .header(agent)
                        .entry("New conversation", None, |window, cx| {
                            window.dispatch_action(
                                Box::new(studio_actions::agent::NewConversation),
                                cx,
                            )
                        })
                        .entry("Choose another agent…", None, |window, cx| {
                            window.dispatch_action(
                                Box::new(studio_actions::workspace::OpenSettings),
                                cx,
                            )
                        });
                }
                menu
            }))
        })
        .anchor(gpui::Anchor::BottomRight)
        .into_any_element()
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
/// The longest conversation title a tab shows.
const TAB_TITLE_CHARS: usize = 28;

fn cut(name: &str, chars: usize) -> String {
    if name.chars().count() <= chars {
        name.to_owned()
    } else {
        let kept: String = name.chars().take(chars.saturating_sub(1)).collect();
        format!("{}…", kept.trim_end())
    }
}

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
const GUTTER: f32 = 24.;

/// A thread row: an icon in the gutter, the content beside it.
fn gutter_row(icon: Option<Icon>, content: impl IntoElement) -> Div {
    h_flex()
        .w_full()
        .items_start()
        .gap_1()
        .child(
            div()
                .w(px(GUTTER))
                .flex_none()
                // Centers a small icon on the first line of `text_sm` content.
                .pt(px(4.))
                .flex()
                .justify_center()
                .children(icon),
        )
        .child(div().flex_1().min_w_0().child(content))
}

/// An `omnigraph_run` call shown with its query: the subcommand ("query", "mutate"), the
/// query text given with `-e`, and the other arguments on one line.
fn shown_query(entry: &Entry) -> Option<(String, String, String)> {
    let args: Vec<&str> = run_args(entry)?.iter().filter_map(Value::as_str).collect();
    let at = args.iter().position(|arg| *arg == "-e")?;
    let text = (*args.get(at + 1)?).to_owned();
    let command = (*args.first()?).to_owned();
    let rest = args
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 0 && *index != at && *index != at + 1)
        .map(|(_, arg)| *arg)
        .collect::<Vec<_>>()
        .join(" ");
    Some((command, text, rest))
}

/// An `omnigraph_run` call's arguments.
fn run_args(entry: &Entry) -> Option<&Vec<Value>> {
    let Entry::Tool { input, studio, .. } = entry else {
        return None;
    };
    if *studio != Some(StudioTool::Run) {
        return None;
    }
    input.as_ref()?.get("args")?.as_array()
}

/// Whether [`shown_query`] shows the entry, read without copying its text: on every render,
/// for every tool call in the thread.
fn shows_query(entry: &Entry) -> bool {
    let mut args = run_args(entry)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    args.by_ref().any(|arg| arg == "-e") && args.next().is_some()
}

/// Whether an entry folds into a run's summary line: thoughts, and tool calls other than the
/// queries shown with their text.
fn is_folded_step(entry: &Entry) -> bool {
    match entry {
        Entry::Thought(_) => true,
        Entry::Tool { .. } => !shows_query(entry),
        _ => false,
    }
}

/// What a tool call did, for a run's summary line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    Cli,
    Run,
    Other,
}

impl ToolKind {
    fn of(studio: Option<StudioTool>) -> Self {
        match studio {
            Some(StudioTool::Cli) => Self::Cli,
            Some(StudioTool::Run) => Self::Run,
            None => Self::Other,
        }
    }

    fn phrase(self, count: usize) -> String {
        let plural = |one: &str, many: &str| if count == 1 { one } else { many }.to_owned();
        match self {
            Self::Cli => "looked up the CLI".into(),
            Self::Run => format!("ran {count} omnigraph {}", plural("command", "commands")),
            Self::Other => format!("used {count} {}", plural("tool", "tools")),
        }
    }
}

/// A run's summary, as Delta writes it: "Thought 1 time, read the schema, ran 2 queries", kinds
/// in the order they first ran.
fn activity_summary(thoughts: usize, tools: &[ToolKind]) -> String {
    let mut parts = Vec::new();
    if thoughts > 0 {
        parts.push(format!(
            "thought {thoughts} {}",
            if thoughts == 1 { "time" } else { "times" }
        ));
    }
    let mut seen: Vec<ToolKind> = Vec::new();
    for kind in tools {
        if !seen.contains(kind) {
            seen.push(*kind);
        }
    }
    for kind in seen {
        let count = tools.iter().filter(|other| **other == kind).count();
        parts.push(kind.phrase(count));
    }
    let text = parts.join(", ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

fn tool_verb(title: &str, studio: Option<StudioTool>) -> String {
    match ToolKind::of(studio) {
        ToolKind::Cli => "Look up the CLI".into(),
        ToolKind::Run => "Run omnigraph".into(),
        ToolKind::Other => title.to_owned(),
    }
}

impl Render for AgentPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_bg = cx.theme().colors().editor_background;
        let body = match &self.state {
            State::Setup => self.render_setup(cx).into_any_element(),
            _ => {
                self.sync_markdown(cx);
                self.sync_thread(cx);
                self.render_thread(window, cx).into_any_element()
            }
        };
        let banner = self.render_branch_banner(cx);
        let (graph, branch) = {
            let session = self.session.read(cx);
            (session.graph_id.clone(), session.branch.clone())
        };
        // The prompt, inline under the thread as Delta writes it: the next message's gutter and
        // the editor, then the controls that send it.
        let composer = div()
            .id("composer")
            .key_context("AgentComposer")
            .flex_none()
            .pl_2()
            .pr_4()
            .pt(px(ds::space::MD))
            .pb(px(ds::space::LG))
            .border_t_1()
            .border_color(ds::surface::rule(cx))
            .when(ds::is_glass(cx), |row| row.border_dashed())
            .tooltip(Tooltip::text(format!(
                "The agent reads {graph} at {branch}; its changes go to a branch of its own"
            )))
            .children(self.render_queue(cx))
            .child(gutter_row(
                Some(
                    Icon::new(IconName::Person)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                ),
                div().text_sm().child(self.composer.clone()),
            ))
            .child(self.render_composer_controls(cx));

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
            .children(banner)
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
        ) && !self.composer.read(cx).text(cx).trim().is_empty();
        let send_or_stop = if running {
            ds::round_button("stop", IconName::Stop, false, cx)
                .tooltip(|_, cx| Tooltip::for_action("Stop", &Stop, cx))
                .on_click(cx.listener(|this, _, window, cx| this.stop(&Stop, window, cx)))
        } else {
            ds::round_button("send", IconName::Send, can_send, cx)
                .tooltip(|_, cx| Tooltip::for_action("Send", &Send, cx))
                .on_click(cx.listener(|this, _, window, cx| this.send(&Send, window, cx)))
        };
        let mut end = Vec::new();
        if running {
            end.push(
                h_flex()
                    .gap(px(ds::space::XS))
                    .items_center()
                    .child(
                        Icon::new(IconName::ArrowCircle)
                            .size(IconSize::XSmall)
                            .color(Color::Muted)
                            .with_rotate_animation(2),
                    )
                    .child(ds::text::meta("Working"))
                    .into_any_element(),
            );
        }
        end.extend(self.render_usage(cx));
        end.extend(self.render_options(cx));
        if matches!(self.state, State::Exited { .. }) {
            end.push(
                ds::icon_button("restart", IconName::RotateCw)
                    .icon_color(Color::Muted)
                    .tooltip(|_, cx| Tooltip::for_action("Start the agent again", &Restart, cx))
                    .on_click(cx.listener(|this, _, window, cx| this.restart(&Restart, window, cx)))
                    .into_any_element(),
            );
        }
        end.push(send_or_stop.into_any_element());
        h_flex()
            .justify_end()
            .items_center()
            .gap(px(ds::space::XS))
            .pt(px(ds::space::SM))
            .children(end)
    }

    /// The agent's name, a dot for its state and what the state means.
    fn status(&self) -> (String, Color, &'static str) {
        match &self.state {
            State::Setup => ("Agent".to_owned(), Color::Muted, "No agent chosen"),
            State::Idle => (
                "Agent".to_owned(),
                Color::Muted,
                "Starts when you send a message",
            ),
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
            .p_6()
            .child(
                v_flex()
                    .w(px(320.))
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .mb_2()
                            .justify_center()
                            .child(Label::new("Choose an agent").color(Color::Muted))
                            .child(
                                div()
                                    .id("agent-info")
                                    .tooltip(Tooltip::text(
                                        "Studio runs the agent on this computer over the Agent \
                                         Client Protocol and gives it tools for this graph. It \
                                         never sees your token, and its changes land on a branch \
                                         of its own, for you to review.",
                                    ))
                                    .child(
                                        Icon::new(IconName::Info)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            ),
                    )
                    // A question asked before an agent was chosen waits, and says so.
                    .children(
                        self.queue
                            .entries
                            .first()
                            .map(|queued| queued.text.clone())
                            .map(|question| {
                                div().mb_2().child(
                                    Label::new(format!(
                                        "“{question}” is sent once you choose an agent."
                                    ))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                )
                            }),
                    )
                    .children(PRESETS.iter().enumerate().map(|(index, (name, command))| {
                        let command: Vec<String> =
                            command.iter().map(|s| (*s).to_owned()).collect();
                        let shown = command.join(" ");
                        studio_ui::ds::button(("preset", index), *name)
                            .full_width()
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .start_icon(
                                Icon::new(IconName::ZedAssistant)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .tooltip(Tooltip::text(format!(
                                "Runs {shown}; npx downloads it the first time you send a message"
                            )))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.use_command(command.clone(), cx)
                            }))
                    }))
                    .child(
                        h_flex()
                            .my_2()
                            .gap_2()
                            .child(ui::Divider::horizontal())
                            .child(Label::new("or").size(LabelSize::XSmall).color(Color::Muted))
                            .child(ui::Divider::horizontal()),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                studio_ui::ds::field_frame(false, cx)
                                    .flex_1()
                                    .child(studio_ui::ds::field_input(self.custom_command.clone())),
                            )
                            .child(
                                studio_ui::ds::icon_button("use-custom", IconName::ArrowRight)
                                    .icon_size(IconSize::Small)
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

    /// Brings each agent reply's markdown up to its text: new replies get an entity, a growing
    /// one appends what streamed in since.
    fn sync_markdown(&mut self, cx: &mut Context<Self>) {
        self.markdown
            .retain(|index, _| matches!(self.entries.get(*index), Some(Entry::Agent(_))));
        self._markdown_watch
            .retain(|index, _| self.markdown.contains_key(index));
        for (index, entry) in self.entries.iter().enumerate() {
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

    /// The thread's blocks in order. As in Delta: the agent's mark once per turn, and each run
    /// of tool calls and thoughts as one line that opens to its steps.
    fn blocks(&self) -> Vec<Block> {
        let mut blocks = Vec::new();
        let mut marked = false;
        let mut index = 0;
        while index < self.entries.len() {
            let entry = &self.entries[index];
            match entry {
                Entry::Tool { .. } if shows_query(entry) => {
                    blocks.push(Block::Query {
                        index,
                        lead: !marked,
                    });
                    marked = true;
                    index += 1;
                    continue;
                }
                Entry::Tool { .. } | Entry::Thought(_) => {
                    let start = index;
                    while index < self.entries.len() && is_folded_step(&self.entries[index]) {
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
            blocks.push(Block::Entry { index, lead });
            index += 1;
        }
        if matches!(self.state, State::Exited { .. }) {
            blocks.push(Block::Exited);
        }
        blocks
    }

    /// What a block shows, folded to a number: a block whose number is unchanged keeps its
    /// measured height. Markdown that finishes parsing remeasures its block by itself.
    fn fingerprint(&self, block: &Block) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        block.hash(&mut hasher);
        let entries = match *block {
            Block::Entry { index, .. } => index..index + 1,
            Block::Query { index, .. } => {
                self.folded_queries.contains(&index).hash(&mut hasher);
                index..index + 1
            }
            Block::Activity { start, end, .. } => {
                self.expanded_activity.contains(&start).hash(&mut hasher);
                start..end
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

    /// Brings the list up to the thread: from the first block that differs from what it last
    /// drew, the blocks are measured again; those above keep their heights and the scroll.
    fn sync_thread(&mut self, _cx: &mut Context<Self>) {
        self.entries.rehash();
        let blocks: Vec<(Block, u64)> = self
            .blocks()
            .into_iter()
            .map(|block| (block, self.fingerprint(&block)))
            .collect();
        let same = self
            .shown_blocks
            .iter()
            .zip(&blocks)
            .take_while(|(shown, now)| shown == now)
            .count();
        if same == blocks.len() && same == self.shown_blocks.len() {
            return;
        }
        self.thread
            .splice(same..self.shown_blocks.len(), blocks.len() - same);
        self.shown_blocks = blocks;
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
        if self.entries.is_empty() && !matches!(self.state, State::Exited { .. }) {
            return v_flex()
                .flex_1()
                .min_h_0()
                .py_4()
                .pr_4()
                .pl_2()
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
            Block::Entry { index, lead } => match self.entries.get(index) {
                Some(entry) => self.render_entry(index, entry, lead, window, cx),
                None => div().into_any_element(),
            },
            Block::Query { index, lead } => match self.entries.get(index) {
                Some(entry) => self.render_query_call(index, entry, lead, cx),
                None => div().into_any_element(),
            },
            Block::Activity { start, end, lead } => {
                if end <= self.entries.len() {
                    self.render_activity(start..end, lead, cx)
                } else {
                    div().into_any_element()
                }
            }
            Block::Exited => match &self.state {
                State::Exited { message, stderr } => {
                    let mut callout = Callout::new()
                        .severity(Severity::Error)
                        .icon(IconName::XCircle)
                        .title(message.clone());
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
            .pl_2()
            .pr_4()
            .pb(px(ds::space::LG))
            .when(ix == 0, |block| block.pt_4())
            .child(body)
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
                Some(
                    Icon::new(IconName::Person)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                ),
                div().text_sm().child(text.clone()),
            )
            .when(index > 0, |row| row.mt(px(ds::space::XL)))
            .into_any_element(),
            Entry::Agent(text) => gutter_row(
                lead.then(|| {
                    Icon::new(IconName::ZedAssistant)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                }),
                match self.markdown.get(&index) {
                    // Zed's markdown, as Zed's agent panel renders a reply.
                    Some((markdown, _)) => div()
                        .text_sm()
                        .child(MarkdownElement::new(
                            markdown.clone(),
                            MarkdownStyle::themed(MarkdownFont::Agent, window, cx),
                        ))
                        .into_any_element(),
                    None => div()
                        .text_sm()
                        .child(text.trim().to_owned())
                        .into_any_element(),
                },
            )
            .into_any_element(),
            Entry::Thought(_) | Entry::Tool { .. } => {
                gutter_row(None, self.activity_step(index, entry, cx)).into_any_element()
            }
            Entry::Plan(items) => gutter_row(
                Some(
                    Icon::new(IconName::ListTodo)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                ),
                v_flex()
                    .gap_0p5()
                    .children(items.iter().map(|(content, status)| {
                        let icon = match status {
                            PlanEntryStatus::Completed => {
                                Icon::new(IconName::Check).color(Color::Success)
                            }
                            PlanEntryStatus::InProgress => {
                                Icon::new(IconName::ArrowRight).color(Color::Accent)
                            }
                            _ => Icon::new(IconName::Circle).color(Color::Disabled),
                        };
                        h_flex()
                            .gap_2()
                            .child(icon.size(IconSize::XSmall))
                            .child(Label::new(content.clone()).size(LabelSize::Small))
                    })),
            )
            .into_any_element(),
            Entry::Notice { text, error } => gutter_row(
                Some(
                    Icon::new(if *error {
                        IconName::Warning
                    } else {
                        IconName::Info
                    })
                    .size(IconSize::XSmall)
                    .color(if *error { Color::Error } else { Color::Muted }),
                ),
                Label::new(text.clone())
                    .size(LabelSize::Small)
                    .color(if *error { Color::Error } else { Color::Muted }),
            )
            .into_any_element(),
            Entry::Permission(card) => {
                gutter_row(None, self.render_permission(index, card, cx)).into_any_element()
            }
            Entry::Proposal(proposal) => {
                gutter_row(None, self.render_proposal(index, proposal, cx)).into_any_element()
            }
        }
    }

    /// A query the agent ran, as Delta shows an edit: "Query people" over the query in a code
    /// card with the editor's colours, and its parameters under it. The line folds the card.
    fn render_query_call(
        &self,
        index: usize,
        entry: &Entry,
        lead: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Entry::Tool { status, .. } = entry else {
            return div().into_any_element();
        };
        let Some((command, text, params)) = shown_query(entry) else {
            return div().into_any_element();
        };
        let tokens = gq_language::tokenize(&text);
        let name = tokens
            .iter()
            .find(|token| token.kind == gq_language::TokenKind::QueryName)
            .map(|token| text[token.range.clone()].to_owned());
        let running = matches!(status, ToolCallStatus::Pending | ToolCallStatus::InProgress);
        let failed = matches!(status, ToolCallStatus::Failed);
        let folded = self.folded_queries.contains(&index);
        let header = h_flex()
            .id(("query-call", index))
            .gap(px(ds::space::XS))
            .items_center()
            .cursor_pointer()
            .child(
                Label::new(if running {
                    "Running".to_owned()
                } else {
                    // "Query people", "Mutate retitle": the CLI's subcommand, capitalized.
                    let mut chars = command.chars();
                    chars
                        .next()
                        .map(|first| first.to_uppercase().chain(chars).collect())
                        .unwrap_or_default()
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .children(name.map(|name| {
                Label::new(name)
                    .size(LabelSize::Small)
                    .color(Color::Default)
                    .buffer_font(cx)
            }))
            .when(failed, |row| {
                row.child(
                    Label::new("· failed")
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
            })
            .child(if running {
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .with_rotate_animation(2)
                    .into_any_element()
            } else {
                Icon::new(if folded {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted)
                .into_any_element()
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.folded_queries.remove(&index) {
                    this.folded_queries.insert(index);
                }
                cx.notify();
            }));
        let highlights = tokens.into_iter().filter_map(|token| {
            token
                .kind
                .syntax_name()
                .map(|name| (token.range.clone(), name))
        });
        let card = ds::code_block_highlighted(text.trim_end().to_owned(), highlights, cx);
        gutter_row(
            lead.then(|| {
                Icon::new(IconName::ZedAssistant)
                    .size(IconSize::Small)
                    .color(Color::Muted)
            }),
            v_flex()
                .w_full()
                .gap(px(ds::space::SM))
                .child(header)
                .when(!folded, |column| {
                    column.child(card).when(!params.is_empty(), |column| {
                        column.child(ds::text::meta(params))
                    })
                }),
        )
        .into_any_element()
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
        let tools: Vec<(ToolKind, &ToolCallStatus)> = steps
            .iter()
            .filter_map(|entry| match entry {
                Entry::Tool { studio, status, .. } => Some((ToolKind::of(*studio), status)),
                _ => None,
            })
            .collect();
        let kinds: Vec<ToolKind> = tools.iter().map(|(kind, _)| *kind).collect();
        let running = tools.iter().any(|(_, status)| {
            matches!(status, ToolCallStatus::Pending | ToolCallStatus::InProgress)
        });
        let failed = tools
            .iter()
            .filter(|(_, status)| matches!(status, ToolCallStatus::Failed))
            .count();
        let expanded = self.expanded_activity.contains(&start);
        let header = h_flex()
            .id(("activity", start))
            .gap(px(ds::space::XS))
            .items_center()
            .cursor_pointer()
            .child(
                Label::new(activity_summary(thoughts, &kinds))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when(failed > 0, |row| {
                row.child(
                    Label::new(format!("· {failed} failed"))
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
            })
            .child(if running {
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .with_rotate_animation(2)
                    .into_any_element()
            } else {
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(IconSize::XSmall)
                .color(Color::Muted)
                .into_any_element()
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.expanded_activity.remove(&start) {
                    this.expanded_activity.insert(start);
                }
                cx.notify();
            }));
        gutter_row(
            lead.then(|| {
                Icon::new(IconName::ZedAssistant)
                    .size(IconSize::Small)
                    .color(Color::Muted)
            }),
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
                .text_xs()
                .text_color(cx.theme().colors().text_muted)
                .child(text.trim().to_owned())
                .into_any_element(),
            Entry::Tool {
                title,
                status,
                studio,
                ..
            } => {
                let failed = matches!(status, ToolCallStatus::Failed);
                let running =
                    matches!(status, ToolCallStatus::Pending | ToolCallStatus::InProgress);
                h_flex()
                    .id(("tool", index))
                    .gap_1()
                    .tooltip(Tooltip::text(title.clone()))
                    .child(
                        Label::new(if failed {
                            format!("{} failed", tool_verb(title, *studio))
                        } else {
                            tool_verb(title, *studio)
                        })
                        .size(LabelSize::Small)
                        .color(if failed { Color::Error } else { Color::Muted })
                        .single_line()
                        .truncate(),
                    )
                    .when(running, |row| {
                        row.child(
                            Icon::new(IconName::ArrowCircle)
                                .size(IconSize::XSmall)
                                .color(Color::Muted)
                                .with_rotate_animation(2),
                        )
                    })
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
            .child(
                Icon::new(IconName::Warning)
                    .size(IconSize::XSmall)
                    .color(Color::Warning),
            )
            // Whole, wrapped: the end of a long command is where a second one would hide.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(card.title.clone()).size(LabelSize::Small)),
            );
        let footer = match &card.answer {
            Some(answer) => studio_ui::ds::card_footer(cx).child(
                Label::new(answer.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            ),
            None => studio_ui::ds::card_footer(cx).children(card.options.iter().enumerate().map(
                |(option_index, option)| {
                    let (icon, color) = match option.kind {
                        PermissionOptionKind::AllowOnce => (IconName::Check, Color::Success),
                        PermissionOptionKind::AllowAlways => {
                            (IconName::CheckDouble, Color::Success)
                        }
                        _ => (IconName::Close, Color::Error),
                    };
                    studio_ui::ds::button(
                        ("permission", index * 16 + option_index),
                        option.name.clone(),
                    )
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(icon).size(IconSize::XSmall).color(color))
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

    fn render_proposal(
        &self,
        index: usize,
        proposal: &Proposal,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focus = self.focus_handle.clone();
        let header = studio_ui::ds::card_header(cx)
            .child(
                Icon::new(IconName::Pencil)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(proposal.summary.clone())
                    .size(LabelSize::Small)
                    .single_line()
                    .truncate(),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id(("proposal-branch", index))
                    .tooltip(Tooltip::text(match &proposal.creates_from {
                        Some(base) => format!(
                            "Runs on {}, which Studio creates from {base} first",
                            proposal.branch
                        ),
                        None => format!("Runs on {}", proposal.branch),
                    }))
                    .child(studio_ui::ds::meta(
                        IconName::GitBranch,
                        proposal.branch.clone(),
                    )),
            )
            .when(proposal.creates_from.is_some(), |header| {
                header.child(ui::Chip::new("new").label_size(LabelSize::XSmall))
            });
        let params = proposal.params.as_ref().map(|params| {
            ds::fields().children(params.iter().map(|(key, value)| {
                ds::field(
                    format!("${key}"),
                    ds::text::code(
                        match value {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        },
                        cx,
                    )
                    .single_line()
                    .truncate(),
                )
            }))
        });
        // A write with no query or parameters (a branch created through the CLI) has no body.
        let body = (!proposal.query.is_empty() || params.is_some()).then(|| {
            ds::card_body()
                .when(!proposal.query.is_empty(), |body| {
                    body.child(ds::code_block(proposal.query.clone(), cx))
                })
                .children(params)
        });
        // The keys answer the card waiting longest; only that card shows them, so a key never
        // answers a card other than the one it is shown on.
        let keyed = self.pending_index() == Some(index);
        let key = |action: &dyn gpui::Action, cx: &App| {
            keyed.then(|| KeyBinding::for_action_in(action, &focus, cx))
        };
        let footer = match &proposal.state {
            ProposalState::Waiting(_) => studio_ui::ds::card_footer(cx)
                .child(
                    studio_ui::ds::button(("allow", index), "Allow")
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(IconName::Check)
                                .size(IconSize::XSmall)
                                .color(Color::Success),
                        )
                        .when_some(key(&Allow, cx), |button, key| button.key_binding(key))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.decide(index, Decision::Allow, cx)
                        })),
                )
                .child(
                    studio_ui::ds::button(("always", index), "Always on this branch")
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(IconName::CheckDouble)
                                .size(IconSize::XSmall)
                                .color(Color::Success),
                        )
                        .when_some(key(&AllowAlways, cx), |button, key| button.key_binding(key))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.decide(index, Decision::AllowAlways, cx)
                        })),
                )
                .child(div().flex_1())
                .child(
                    studio_ui::ds::button(("reject", index), "Reject")
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(IconName::Close)
                                .size(IconSize::XSmall)
                                .color(Color::Error),
                        )
                        .when_some(key(&Reject, cx), |button, key| button.key_binding(key))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.decide(index, Decision::Reject, cx)
                        })),
                ),
            state => {
                let (icon, color, label, tooltip): (IconName, Color, String, Option<String>) =
                    match state {
                        ProposalState::Running => {
                            (IconName::ArrowCircle, Color::Accent, "Running".into(), None)
                        }
                        ProposalState::Committed {
                            commit,
                            nodes,
                            edges,
                        } => (
                            IconName::Check,
                            Color::Success,
                            "Committed".into(),
                            Some(format!(
                                "Commit {commit}: {nodes} nodes and {edges} edges changed"
                            )),
                        ),
                        ProposalState::Refused(message) => (
                            IconName::XCircle,
                            Color::Error,
                            "Refused".into(),
                            Some(message.clone()),
                        ),
                        ProposalState::Unknown => (
                            IconName::Warning,
                            Color::Warning,
                            "Outcome unknown".into(),
                            Some(UNKNOWN_OUTCOME.into()),
                        ),
                        ProposalState::Done(label) => {
                            (IconName::Check, Color::Success, label.clone(), None)
                        }
                        _ => (IconName::Close, Color::Muted, "Rejected".into(), None),
                    };
                let commit = match state {
                    ProposalState::Committed {
                        commit,
                        nodes,
                        edges,
                    } => Some((studio_ui::short_commit_id(commit), *nodes + *edges)),
                    _ => None,
                };
                studio_ui::ds::card_footer(cx)
                    .px_2()
                    .gap_3()
                    .child(
                        h_flex()
                            .id(("outcome", index))
                            .gap_1()
                            .when_some(tooltip, |d, tooltip| d.tooltip(Tooltip::text(tooltip)))
                            .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                            .child(Label::new(label).size(LabelSize::Small).color(color)),
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
        let options: Vec<OptionView> = if self.host.is_some() {
            self.config_options
                .iter()
                .filter_map(|option| OptionView::new(option, self.setting_option.as_ref()))
                .collect()
        } else {
            Vec::new()
        };
        let (shown, more): (Vec<_>, Vec<_>) = options
            .into_iter()
            .partition(|option| option.picker_icon().is_some());
        let (models, shown): (Vec<_>, Vec<_>) = shown.into_iter().partition(OptionView::is_model);
        let (agent_name, dot, state) = self.status();

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
                .map(OptionView::summary)
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
                pickers.push(option_picker(model, icon, &panel, Some(agent_name)));
            }
            None => {
                let command = Self::command(cx).join(" ");
                let about = if command.is_empty() {
                    format!("{state}. Choose the agent in Settings.")
                } else {
                    format!("{state} · {command}. Choose the agent in Settings.")
                };
                pickers.push(
                    h_flex()
                        .id("agent")
                        .h(px(ds::size::ROW))
                        .px(px(ds::space::SM))
                        .gap(px(ds::space::XS))
                        .items_center()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                        .tooltip(Tooltip::text(about))
                        .child(Indicator::dot().color(dot))
                        .child(
                            Label::new(agent_name)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(
                                Box::new(studio_actions::workspace::OpenSettings),
                                cx,
                            )
                        })
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
        let ratio = (usage.used as f32 / usage.size as f32).clamp(0., 1.);
        let nearly_full = ratio >= 0.85;
        let color = if nearly_full {
            cx.theme().status().warning
        } else {
            cx.theme().colors().text_muted
        };
        let percent = format!("{}%", (ratio * 100.).round() as u32);
        let title = format!("{percent} of the context window");
        let mut meta = format!(
            "{} of {} tokens",
            humanize_tokens(usage.used),
            humanize_tokens(usage.size)
        );
        if let Some((amount, currency)) = &usage.cost {
            meta.push_str(&format!(" · {} so far", format_cost(*amount, currency)));
        }
        Some(
            h_flex()
                .id("context-usage")
                .flex_none()
                .gap(px(ds::space::XS))
                .items_center()
                .child(
                    CircularProgress::new(usage.used as f32, usage.size as f32, px(14.), cx)
                        .stroke_width(px(2.))
                        // A visible track, so the ring reads as a gauge, not a spinner.
                        .bg_color(cx.theme().colors().border)
                        .progress_color(color),
                )
                // The number only when it matters.
                .when(nearly_full, |row| {
                    row.child(ds::text::meta(percent).color(Color::Warning))
                })
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
        let border = cx.theme().colors().border_variant;
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
                    .border_color(border)
                    .tooltip(Tooltip::text(entry.text.clone()))
                    .child(Indicator::dot().color(if next { Color::Accent } else { Color::Muted }))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(line)
                                .size(LabelSize::Small)
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
                                IconButton::new(("remove-queued", id), IconName::Trash)
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Remove from the queue"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.queue.take(id);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                IconButton::new(("edit-queued", id), IconName::Pencil)
                                    .icon_size(IconSize::XSmall)
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
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.send_queued_now(id, window, cx)
                                    })),
                            ),
                    )
                    .into_any_element()
            })
            .collect();
        Some(
            v_flex()
                .mb(px(ds::space::MD))
                .ml(px(ds::space::SM))
                .rounded(px(ds::radius::SM))
                .border_1()
                .border_color(border)
                .bg(ds::surface::sunken(cx))
                .overflow_hidden()
                .child(header)
                .children(rows)
                .into_any_element(),
        )
    }

    fn render_branch_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let branch = self
            .branch
            .as_ref()
            .filter(|b| b.created && b.changes > 0)?;
        Some(
            h_flex()
                .flex_none()
                .h(px(studio_ui::ds::size::STRIP))
                .px_3()
                .gap_3()
                .border_t_1()
                .border_color(studio_ui::ds::surface::rule(cx))
                .child(studio_ui::ds::meta(
                    IconName::GitBranch,
                    branch.name.clone(),
                ))
                .child(
                    div()
                        .id("agent-changes")
                        .tooltip(Tooltip::text("Changes committed on the agent's branch"))
                        .child(studio_ui::ds::meta(
                            IconName::Pencil,
                            branch.changes.to_string(),
                        )),
                )
                .child(div().flex_1())
                .child(
                    studio_ui::ds::button("review", "Review")
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::Eye).size(IconSize::XSmall))
                        .on_click(cx.listener(|this, _, window, cx| this.review(window, cx))),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rendering asks [`shows_query`] of every tool call; it agrees with [`shown_query`].
    #[test]
    fn a_run_shows_its_query_only_when_it_gives_one() {
        let tool = |studio, input: Value| Entry::Tool {
            id: "t".into(),
            title: String::new(),
            status: ToolCallStatus::Completed,
            input: Some(input),
            studio,
        };
        let run = Some(StudioTool::Run);
        for (entry, shown) in [
            (
                tool(
                    run,
                    serde_json::json!({"args": ["query", "-e", "q", "--json"]}),
                ),
                true,
            ),
            (
                tool(run, serde_json::json!({"args": ["query", 1, "-e", 2, "q"]})),
                true,
            ),
            (
                tool(run, serde_json::json!({"args": ["query", "-e"]})),
                false,
            ),
            (
                tool(run, serde_json::json!({"args": ["load", "--data", "x"]})),
                false,
            ),
            (tool(run, serde_json::json!({})), false),
            (
                tool(None, serde_json::json!({"args": ["query", "-e", "q"]})),
                false,
            ),
            (Entry::Thought("-e".into()), false),
        ] {
            assert_eq!(shows_query(&entry), shown, "{:?}", shown_query(&entry));
            assert_eq!(shown_query(&entry).is_some(), shown);
        }
        assert_eq!(
            shown_query(&tool(
                run,
                serde_json::json!({"args": ["query", "-e", "q", "--json"]})
            )),
            Some(("query".into(), "q".into(), "--json".into()))
        );
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
        queue.after_cancel = Some("urgent".into());
        assert_eq!(queue.on_turn_ended().as_deref(), Some("urgent"));
        assert_eq!(queue.on_turn_ended().as_deref(), Some("third"));
        assert_eq!(queue.on_turn_ended(), None);
        // Taking one out by id leaves the rest in order.
        queue.push("a".into());
        queue.push("b".into());
        let b = queue.entries[1].id;
        assert_eq!(queue.take(b).as_deref(), Some("b"));
        assert_eq!(queue.entries.len(), 1);
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
        for entry in entries.iter_mut() {
            if let Entry::User(text) = entry {
                text.push('?');
            }
        }
        entries.rehash();
        assert_ne!(entries.hash(0), first);
        assert_eq!(entries.hash(1), second);
        entries.clear();
        entries.rehash();
        assert!(entries.is_empty() && entries.hash(0) == 0);
    }

    #[test]
    fn a_run_of_steps_reads_as_one_sentence() {
        use ToolKind::*;
        assert_eq!(
            activity_summary(1, &[Run]),
            "Thought 1 time, ran 1 omnigraph command"
        );
        assert_eq!(
            activity_summary(0, &[Cli, Run, Run, Other]),
            "Looked up the CLI, ran 2 omnigraph commands, used 1 tool"
        );
        assert_eq!(activity_summary(2, &[]), "Thought 2 times");
        assert_eq!(ToolKind::of(Some(StudioTool::Run)), Run);
    }

    #[test]
    fn long_option_names_are_cut_on_a_character() {
        assert_eq!(cut("Claude Sonnet", OPTION_NAME_CHARS), "Claude Sonnet");
        let long = "Claude Sonnet with an extended thinking budget";
        // No space is left before the ellipsis.
        assert_eq!(
            cut(long, OPTION_NAME_CHARS),
            "Claude Sonnet with an extended…"
        );
        assert_eq!(cut("模型模型模型", 4), "模型模…");
    }
}
