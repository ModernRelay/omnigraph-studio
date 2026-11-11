//! What a graph's window shows in its centre when the graph would not open (spec 029): one
//! page per answer the schema read got, saying what happened and offering what helps, in place of
//! an empty centre and a one-line error in the Data panel.

use gpui::{AnyElement, App, Entity};
use graph_store::{Connection, GraphListing, GraphSession, SchemaFailure};
use ui::prelude::*;

/// How many of a server's graphs the page for a missing one offers, as buttons.
const GRAPHS_OFFERED: usize = 5;

/// The page for `session`'s graph, when its schema read failed and no sign-in is asked for
/// instead ([`crate::sign_in_prompt`]). `None` while the schema is read or held.
pub fn open_failure(session: &Entity<GraphSession>, cx: &App) -> Option<AnyElement> {
    let s = session.read(cx);
    let failure = s.schema_failure?;
    if !matches!(s.schema, graph_store::Loadable::Failed(_)) {
        return None;
    }
    let connection = s.connection.read(cx);
    if connection.sign_in_prompt().is_some() {
        return None;
    }
    let graph = s.graph_id.clone();
    let server = connection.server.name.clone();
    let loopback = graph_client::is_loopback(&connection.server.url);
    let (icon, title, detail) = says(failure, &graph, &server, connection, loopback);
    let open_another = studio_ui::ds::button("open-another-graph", "Open a graph…")
        .style(ButtonStyle::Outlined)
        .on_click(|_, window, cx| {
            window.dispatch_action(Box::new(studio_actions::workspace::OpenGraph), cx)
        });
    let session = session.clone();
    let retry = matches!(failure, SchemaFailure::Unreachable | SchemaFailure::Other).then(|| {
        studio_ui::ds::button("retry-open", "Try again")
            .style(ButtonStyle::Filled)
            .on_click(move |_, _, cx| {
                session.update(cx, |session, cx| {
                    session.connection.update(cx, |c, cx| c.connect(cx));
                    session.reload_schema(cx)
                })
            })
    });
    let graphs = (failure == SchemaFailure::NotFound)
        .then(|| listed(connection))
        .flatten()
        .unwrap_or_default();
    let url = connection.server.url.clone();
    let offered = graphs
        .into_iter()
        .take(GRAPHS_OFFERED)
        .enumerate()
        .map(|(index, id)| {
            let link = graph_link(&url, &id);
            studio_ui::ds::button(("open-listed-graph", index), id)
                .style(ButtonStyle::Subtle)
                .start_icon(Icon::new(IconName::Folder))
                .on_click(move |_, window, cx| {
                    window.dispatch_action(
                        Box::new(studio_actions::workspace::OpenLink { url: link.clone() }),
                        cx,
                    )
                })
        });
    Some(
        studio_ui::ds::empty_state(icon, title)
            .debug_selector(|| "open-failure".into())
            .child(studio_ui::ds::state_detail(detail))
            .child(
                h_flex()
                    .flex_wrap()
                    .justify_center()
                    .gap(px(studio_ui::ds::space::SM))
                    .children(offered),
            )
            .child(
                h_flex()
                    .gap(px(studio_ui::ds::space::SM))
                    .children(retry)
                    .child(open_another),
            )
            .into_any_element(),
    )
}

/// The page's icon, title and line for `failure` of `graph` on `server`.
fn says(
    failure: SchemaFailure,
    graph: &str,
    server: &str,
    connection: &Connection,
    loopback: bool,
) -> (IconName, String, String) {
    match failure {
        SchemaFailure::NotFound => (
            IconName::Folder,
            format!("No graph named “{graph}” on {server}"),
            match listed(connection) {
                Some(ids) if ids.is_empty() => "This server lists no graphs.".to_owned(),
                Some(ids) => format!("It has: {}.", ids.join(", ")),
                None => "This server does not list its graphs. Check the id, or open another \
                         graph."
                    .to_owned(),
            },
        ),
        SchemaFailure::Unauthorized => (
            IconName::UserCheck,
            format!("{server} refused the token"),
            format!(
                "{} Open a graph… and give the server a new token with Update token.",
                connection
                    .refusal
                    .unwrap_or(graph_client::Unauthorized::TokenNeeded)
                    .reason()
            ),
        ),
        SchemaFailure::Forbidden => (
            IconName::UserCheck,
            format!("This token may not read “{graph}”"),
            "The server's policy refuses this credential the graph. Open another graph, or ask \
             for access."
                .to_owned(),
        ),
        SchemaFailure::Unreachable => (
            IconName::Server,
            format!("Could not reach {server}"),
            if loopback {
                format!(
                    "Is `omnigraph-server` running at {}? Start it, then Try again.",
                    connection.server.url
                )
            } else {
                format!(
                    "{} did not answer. Check the address and your network, then Try again.",
                    connection.server.url
                )
            },
        ),
        SchemaFailure::Other => (
            IconName::Server,
            format!("Studio could not read the schema of “{graph}”"),
            "The server's answer could not be read. Try again, or open another graph.".to_owned(),
        ),
    }
}

/// The graphs the server says the token reaches, when it says.
fn listed(connection: &Connection) -> Option<Vec<String>> {
    match &connection.graphs {
        GraphListing::Listed(_) | GraphListing::Discovered(_) => Some(connection.graph_ids()),
        _ => None,
    }
}

/// An `omnigraph-studio://graph` link to `graph` on the server at `url`, which opens it as a
/// link does.
fn graph_link(url: &str, graph: &str) -> String {
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("server", url)
        .append_pair("graph", graph)
        .finish();
    format!("omnigraph-studio://graph?{query}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_graph_link_carries_its_server_encoded() {
        assert_eq!(
            graph_link("http://127.0.0.1:8787", "knowledge"),
            "omnigraph-studio://graph?server=http%3A%2F%2F127.0.0.1%3A8787&graph=knowledge"
        );
    }
}
