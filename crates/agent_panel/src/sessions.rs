//! The agent conversations Studio can come back to (spec 011): for each graph and agent command,
//! the last session's id and the branch its changes go to, in an owner-only file beside the
//! workspaces. Ids only: the conversation itself stays with the agent, which replays it when
//! Studio loads the session.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// The conversation to come back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSession {
    pub session: String,
    /// The agent's branch, and whether it was created yet.
    pub branch: String,
    pub branch_created: bool,
}

fn path() -> PathBuf {
    studio_settings::data_dir()
        .join("agent")
        .join("sessions.json")
}

/// One record per graph and agent: a session belongs to the agent that made it.
fn key(graph: &str, command: &[String]) -> String {
    format!("{graph}\n{}", command.join(" "))
}

fn read(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn write(path: &Path, records: &Map<String, Value>) {
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_err()
    {
        return;
    }
    let Ok(text) = serde_json::to_string_pretty(&Value::Object(records.clone())) else {
        return;
    };
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    if let Ok(mut file) = options.open(path) {
        std::io::Write::write_all(&mut file, text.as_bytes()).ok();
    }
}

pub fn load(graph: &str, command: &[String]) -> Option<SavedSession> {
    load_from(&path(), graph, command)
}

pub fn save(graph: &str, command: &[String], saved: &SavedSession) {
    save_to(&path(), graph, command, saved);
}

pub fn forget(graph: &str, command: &[String]) {
    let path = path();
    let mut records = read(&path);
    if records.remove(&key(graph, command)).is_some() {
        write(&path, &records);
    }
}

fn load_from(path: &Path, graph: &str, command: &[String]) -> Option<SavedSession> {
    let records = read(path);
    let record = records.get(&key(graph, command))?;
    Some(SavedSession {
        session: record.get("session")?.as_str()?.to_owned(),
        branch: record.get("branch")?.as_str()?.to_owned(),
        branch_created: record
            .get("branch_created")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn save_to(path: &Path, graph: &str, command: &[String], saved: &SavedSession) {
    let mut records = read(path);
    records.insert(
        key(graph, command),
        json!({
            "session": saved.session,
            "branch": saved.branch,
            "branch_created": saved.branch_created,
        }),
    );
    write(path, &records);
}

/// The option values the person last chose for an agent (its model, mode, reasoning level),
/// applied to its next new session: `{agent command: {option id: value id}}`.
fn preferences_path() -> PathBuf {
    studio_settings::data_dir()
        .join("agent")
        .join("preferences.json")
}

pub fn preferences(command: &[String]) -> Map<String, Value> {
    preferences_from(&preferences_path(), command)
}

pub fn prefer(command: &[String], option: &str, value: Value) {
    prefer_to(&preferences_path(), command, option, value);
}

fn preferences_from(path: &Path, command: &[String]) -> Map<String, Value> {
    read(path)
        .get(&command.join(" "))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn prefer_to(path: &Path, command: &[String], option: &str, value: Value) {
    let mut records = read(path);
    let agent = records
        .entry(command.join(" "))
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(agent) = agent.as_object_mut() {
        agent.insert(option.to_owned(), value);
    }
    write(path, &records);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_kept_per_graph_and_agent() {
        let dir =
            std::env::temp_dir().join(format!("studio-agent-sessions-{}", std::process::id()));
        let path = dir.join("sessions.json");
        let claude = vec!["claude-code-acp".to_owned()];
        let other = vec!["other-agent".to_owned()];
        let saved = SavedSession {
            session: "s-1".into(),
            branch: "agent/20261012-101010".into(),
            branch_created: true,
        };
        save_to(&path, "server/crm", &claude, &saved);
        assert_eq!(load_from(&path, "server/crm", &claude), Some(saved));
        // Another graph, or another agent, has no session to resume.
        assert_eq!(load_from(&path, "server/notes", &claude), None);
        assert_eq!(load_from(&path, "server/crm", &other), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn option_choices_are_kept_per_agent_and_the_latest_wins() {
        let dir =
            std::env::temp_dir().join(format!("studio-agent-preferences-{}", std::process::id()));
        let path = dir.join("preferences.json");
        let claude = vec!["claude-code-acp".to_owned()];
        let other = vec!["other-agent".to_owned()];
        prefer_to(&path, &claude, "model", json!("sonnet"));
        prefer_to(&path, &claude, "model", json!("opus"));
        prefer_to(&path, &claude, "fast", json!(true));
        let chosen = preferences_from(&path, &claude);
        assert_eq!(chosen.get("model"), Some(&json!("opus")));
        assert_eq!(chosen.get("fast"), Some(&json!(true)));
        // Another agent's options are its own.
        assert!(preferences_from(&path, &other).is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
