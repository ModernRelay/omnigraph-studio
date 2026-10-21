//! The one conversion from the key builds before 2026-10-20 filed a graph's things under, its
//! server's name and its id (`<server name>/<graph id>`), to the graph's one key, its server's
//! address and its id ([`studio_settings::overrides_key`]). A server renamed in the CLI's config
//! lost everything filed under its old name; its address does not change with the name.
//!
//! What was filed under a name that resolves to a known server moves once, and never over what
//! the key holds already: a graph's settings and its saved queries' folder as the graph opens
//! ([`move_named`], from `GraphSession::new`), and the agent's conversations as their index
//! loads (`agent_panel::sessions`, through [`key_for`]). A name resolves only when exactly one
//! known server has it ([`address_of`]): a name no server has, or one two servers share (a CLI
//! target and a server added in Studio, each called `local`), is left as it is on every path,
//! since either server could be the one that filed it.
//! A one-release conversion: delete this module and its two callers after the next release.

use std::path::Path;

use gpui::{App, Task};
use studio_settings::StudioSettings;

use crate::{ConnectionRegistry, ServerEntry};

/// A graph's key as builds before 2026-10-20 made it: its server's name and its id.
fn named_key(server_name: &str, graph: &str) -> String {
    format!("{server_name}/{graph}")
}

/// The servers Studio knows, each by its name and its address.
pub fn known_servers(cx: &App) -> Vec<(String, String)> {
    let Some(registry) = ConnectionRegistry::try_global(cx) else {
        return Vec::new();
    };
    registry
        .read(cx)
        .connections
        .iter()
        .map(|connection| {
            let server = &connection.read(cx).server;
            (server.name.clone(), server.url.clone())
        })
        .collect()
}

/// The address of the one server among `servers` (name, address) named `name`; `None` when no
/// server has the name, or more than one does.
fn address_of<'a>(name: &str, servers: &'a [(String, String)]) -> Option<&'a str> {
    let mut named = servers.iter().filter(|(server, _)| server == name);
    let (_, url) = named.next()?;
    named.next().is_none().then_some(url.as_str())
}

/// The graph key for `named`, a key made of a server's name, when exactly one of `servers`
/// (name, address) has the name; `None` for a key that is an address's already, or a name no
/// server or more than one has.
pub fn key_for(named: &str, servers: &[(String, String)]) -> Option<String> {
    let (name, graph) = named.rsplit_once('/')?;
    if graph.is_empty() {
        return None;
    }
    let url = address_of(name, servers)?;
    Some(studio_settings::overrides_key(url, graph))
}

/// Moves what `graph` on `server` has under the server's name: its settings, saved to the
/// settings file at once, and its saved queries' folder, off the main thread; the task says
/// whether any query moved. Nothing moves when the name is not this server's alone among the
/// servers Studio knows ([`address_of`]); `None` then.
pub(crate) fn move_named(server: &ServerEntry, graph: &str, cx: &mut App) -> Option<Task<bool>> {
    let mut servers = known_servers(cx);
    if !servers.iter().any(|(_, url)| *url == server.url) {
        servers.push((server.name.clone(), server.url.clone()));
    }
    if address_of(&server.name, &servers) != Some(server.url.as_str()) {
        return None;
    }
    let named = named_key(&server.name, graph);
    let key = studio_settings::overrides_key(&server.url, graph);
    let in_settings = cx.try_global::<StudioSettings>().is_some_and(|settings| {
        settings.graphs.contains_key(&named) && !settings.graphs.contains_key(&key)
    });
    if in_settings {
        studio_settings::update(cx, |settings| {
            move_overrides(settings, &named, &key);
        });
    }
    let (from, to) = (
        studio_settings::queries_dir(&named),
        studio_settings::queries_dir(&key),
    );
    Some(
        cx.background_executor()
            .spawn(async move { move_queries(&from, &to) }),
    )
}

/// Moves the overrides under `named` to `key`, unless `key` holds some already. Whether
/// anything moved.
fn move_overrides(settings: &mut StudioSettings, named: &str, key: &str) -> bool {
    if named == key || settings.graphs.contains_key(key) {
        return false;
    }
    match settings.graphs.remove(named) {
        Some(overrides) => {
            settings.graphs.insert(key.to_owned(), overrides);
            true
        }
        None => false,
    }
}

/// Moves a folder of saved queries to the key's: renamed whole when the key has none, else each
/// `.gq` file the key's folder has no file of that name for. The rest stay where they are, and
/// the folder with them. A file is never written over, and one that cannot move stays.
/// Whether anything moved.
fn move_queries(from: &Path, to: &Path) -> bool {
    if from == to || !from.is_dir() {
        return false;
    }
    if !to.exists() {
        return std::fs::rename(from, to).is_ok();
    }
    let Ok(entries) = std::fs::read_dir(from) else {
        return false;
    };
    let mut moved = false;
    for path in entries.flatten().map(|entry| entry.path()) {
        let (Some(name), true) = (
            path.file_name(),
            path.extension().is_some_and(|ext| ext == "gq"),
        ) else {
            continue;
        };
        // A link fails when its name is taken, so nothing is written over.
        if std::fs::hard_link(&path, to.join(name)).is_ok() {
            std::fs::remove_file(&path).ok();
            moved = true;
        }
    }
    moved
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const DEV: &str = "http://127.0.0.1:8787";

    fn servers() -> Vec<(String, String)> {
        vec![("dev".into(), DEV.into())]
    }

    /// A key made of a known server's name names that server's address; an address's key, or
    /// a name no server has, is left as it is.
    #[test]
    fn a_name_resolves_only_to_a_known_server() {
        assert_eq!(
            key_for("dev/crm", &servers()).as_deref(),
            Some("http://127.0.0.1:8787/crm")
        );
        assert_eq!(key_for("http://127.0.0.1:8787/crm", &servers()), None);
        assert_eq!(key_for("gone/crm", &servers()), None);
        assert_eq!(key_for("dev/", &servers()), None);
        assert_eq!(key_for("devx/crm", &servers()), None);

        // A name two servers share (a CLI target and a server added in Studio) is either's,
        // so it moves nowhere; the address's own key is left as well.
        let shared = vec![
            ("local".to_owned(), DEV.to_owned()),
            ("local".to_owned(), "http://127.0.0.1:9000".to_owned()),
        ];
        assert_eq!(key_for("local/crm", &shared), None);
        assert_eq!(address_of("local", &shared), None);
        assert_eq!(
            address_of("local", &shared[1..]),
            Some("http://127.0.0.1:9000")
        );
    }

    /// Settings kept under a server's name move to its address once, and never over settings
    /// the address already has.
    #[test]
    fn settings_kept_under_a_servers_name_move_to_its_address_once() {
        let mut settings = StudioSettings::parse(
            r#"{"graphs": {
                "dev/knowledge": {"tree_page_size": 10, "labels": {"Person": "title"}},
                "dev/notes": {"tree_page_size": 20},
                "http://127.0.0.1:8787/notes": {"tree_page_size": 30}
            }}"#,
        )
        .unwrap();
        let knowledge = studio_settings::overrides_key(DEV, "knowledge");
        assert_eq!(knowledge, "http://127.0.0.1:8787/knowledge");
        let named = named_key("dev", "knowledge");
        assert!(move_overrides(&mut settings, &named, &knowledge));
        assert_eq!(settings.for_graph(DEV, "knowledge").tree_page_size, 10);
        assert_eq!(settings.labels_for(&knowledge)["Person"], "title");
        assert!(!settings.graphs.contains_key(&named), "moved, not copied");
        assert!(!move_overrides(&mut settings, &named, &knowledge), "once");
        let notes = studio_settings::overrides_key(DEV, "notes");
        assert!(!move_overrides(&mut settings, "dev/notes", &notes));
        assert_eq!(
            settings.for_graph(DEV, "notes").tree_page_size,
            30,
            "the address's own settings stand"
        );
        assert!(
            settings.graphs.contains_key("dev/notes"),
            "and the old ones stay"
        );
    }

    fn scratch(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "studio-named-queries-{}-{case}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// A graph's saved queries follow it to its key: the folder is renamed when the key has
    /// none; when both have one, each file the key's lacks moves and the rest stay, the old
    /// folder with them; with neither, nothing is made. No file is written over.
    #[test]
    fn saved_queries_move_to_the_graphs_key_without_writing_over_one() {
        // Neither folder.
        let root = scratch("neither");
        let (from, to) = (root.join("dev~crm"), root.join("http_~~dev~crm"));
        assert!(!move_queries(&from, &to));
        assert!(!from.exists() && !to.exists(), "nothing made");

        // Only the name's: renamed whole.
        let root = scratch("one");
        let (from, to) = (root.join("dev~crm"), root.join("http_~~dev~crm"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("a.gq"), "query a() {}").unwrap();
        assert!(move_queries(&from, &to));
        assert!(!from.exists(), "renamed");
        assert_eq!(names(&to), ["a.gq"]);

        // Both: what the key's folder lacks moves, a name it has stays, and so does the rest.
        let root = scratch("both");
        let (from, to) = (root.join("dev~crm"), root.join("http_~~dev~crm"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("a.gq"), "old a").unwrap();
        std::fs::write(from.join("b.gq"), "old b").unwrap();
        std::fs::write(from.join("notes.txt"), "not a query").unwrap();
        std::fs::write(to.join("b.gq"), "new b").unwrap();
        assert!(move_queries(&from, &to));
        assert_eq!(names(&to), ["a.gq", "b.gq"]);
        assert_eq!(std::fs::read_to_string(to.join("a.gq")).unwrap(), "old a");
        assert_eq!(
            std::fs::read_to_string(to.join("b.gq")).unwrap(),
            "new b",
            "not written over"
        );
        assert_eq!(names(&from), ["b.gq", "notes.txt"], "the rest stays");
        assert_eq!(std::fs::read_to_string(from.join("b.gq")).unwrap(), "old b");

        for case in ["neither", "one", "both"] {
            std::fs::remove_dir_all(scratch(case)).ok();
        }
    }

    /// Opening a graph moves its saved queries from its server's name to its key, off the main
    /// thread, and says so once they have moved, so the folder is listed again.
    #[gpui::test]
    fn a_graph_opening_takes_its_saved_queries_along(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        // A graph of its own: other tests open `dev`'s `knowledge` in this process.
        let graph = "moved-queries";
        let named = studio_settings::queries_dir(&named_key("dev", graph));
        let key = studio_settings::queries_dir(&studio_settings::overrides_key(DEV, graph));
        std::fs::remove_dir_all(&key).ok();
        std::fs::create_dir_all(&named).unwrap();
        std::fs::write(named.join("people.gq"), "query people() {}").unwrap();
        let http = http_client::FakeHttpClient::create(|_| async {
            Ok(http_client::http::Response::builder()
                .status(404)
                .body(http_client::AsyncBody::default())
                .unwrap())
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: DEV.into(),
            source: crate::ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| crate::Connection::new(server, None, http).unwrap());
        let session =
            cx.new(|cx| crate::GraphSession::new(connection, graph.into(), "main".into(), cx));
        let moved = std::rc::Rc::new(std::cell::Cell::new(false));
        let heard = moved.clone();
        let _events = cx.update(|cx| {
            cx.subscribe(&session, move |_, event: &crate::SessionEvent, _| {
                if *event == crate::SessionEvent::SavedQueriesMoved {
                    heard.set(true);
                }
            })
        });
        cx.run_until_parked();
        assert!(moved.get(), "said once moved");
        assert!(key.join("people.gq").is_file(), "moved to the graph's key");
        assert!(!named.exists());
        std::fs::remove_dir_all(&key).ok();
    }
}
