//! One agent process per agent command (spec 011), shared by every conversation Studio has open
//! with it and by the Conversations panel, as Zed keeps one connection per agent server. A
//! conversation holds its connection; the process stops when the last holder lets go, and a
//! connection whose process ended is replaced on the next use.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use agent_client_protocol::schema::v1::SessionInfo;
use futures::channel::{mpsc, oneshot};
use gpui::{App, AppContext as _, Global, Task};

use crate::host::{self, AgentLaunch, ConnectionCommand, HostCommand, HostEvent, SessionLaunch};

pub struct AgentConnection {
    commands: mpsc::UnboundedSender<ConnectionCommand>,
    /// Conversations asked for and not yet open: told why when the agent cannot start, also
    /// before it reads their request.
    opening: host::Opening,
    /// The process: dropping it stops the agent.
    _task: Task<()>,
}

impl AgentConnection {
    fn is_alive(&self) -> bool {
        !self.commands.is_closed()
    }

    /// Opens a conversation on the agent; false when the agent is gone.
    pub fn open(
        &self,
        launch: SessionLaunch,
        events: mpsc::UnboundedSender<HostEvent>,
        commands: mpsc::UnboundedReceiver<HostCommand>,
    ) -> bool {
        if let Ok(mut opening) = self.opening.lock() {
            opening.push(events.clone());
        }
        self.commands
            .unbounded_send(ConnectionCommand::Open {
                launch,
                events,
                commands,
            })
            .is_ok()
    }

    pub fn list(&self, cwd: PathBuf) -> oneshot::Receiver<Result<Vec<SessionInfo>, String>> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .unbounded_send(ConnectionCommand::List { cwd, reply })
            .ok();
        answer
    }

    pub fn delete(&self, session: String) -> oneshot::Receiver<Result<(), String>> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .unbounded_send(ConnectionCommand::Delete { session, reply })
            .ok();
        answer
    }
}

#[derive(Default)]
struct Connections(HashMap<AgentLaunch, Weak<AgentConnection>>);

impl Global for Connections {}

/// The running connection to this agent, or a new one.
pub fn connection(launch: AgentLaunch, cx: &mut App) -> Arc<AgentConnection> {
    let running = cx
        .default_global::<Connections>()
        .0
        .get(&launch)
        .and_then(Weak::upgrade)
        .filter(|connection| connection.is_alive());
    if let Some(connection) = running {
        return connection;
    }
    let (commands, receiver) = mpsc::unbounded();
    let opening = host::Opening::default();
    let task = cx.background_spawn(host::run(launch.clone(), receiver, opening.clone()));
    let connection = Arc::new(AgentConnection {
        commands,
        opening,
        _task: task,
    });
    let connections = cx.default_global::<Connections>();
    connections.0.retain(|_, weak| weak.strong_count() > 0);
    connections.0.insert(launch, Arc::downgrade(&connection));
    connection
}
