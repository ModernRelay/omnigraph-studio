//! A request's route as the contract names it (`/graphs/{graph_id}/query`), which is what
//! Studio counts and traces a request by (`studio_perf`): never its path, which holds a graph,
//! branch or commit.

use http_client::http::Method;
use studio_perf::{Counter, Unit};
use url::Url;

/// The method as a static name.
fn method_name(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::PATCH => "PATCH",
        Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    }
}

/// A request's counters: how many were sent on the route with the method, and the bytes of their
/// answers.
pub struct Route {
    pub method: &'static str,
    pub template: &'static str,
    pub requests: Counter,
    pub bytes: Counter,
}

macro_rules! route {
    ($method:literal, $template:literal) => {
        Route {
            method: $method,
            template: $template,
            requests: Counter::new($method, $template, Unit::Count),
            bytes: Counter::new("received", concat!($method, " ", $template), Unit::Bytes),
        }
    };
}

/// Every route of the contract, by method.
static ROUTES: [Route; 41] = [
    route!("GET", "/.well-known/oauth-protected-resource"),
    route!(
        "GET",
        "/.well-known/oauth-protected-resource/{resource_path}"
    ),
    route!("GET", "/cluster/deployments"),
    route!("POST", "/cluster/deployments"),
    route!("GET", "/cluster/deployments/{deployment_id}"),
    route!("POST", "/cluster/plan"),
    route!("GET", "/graphs"),
    route!("GET", "/graphs/discovery"),
    route!("GET", "/graphs/{graph_id}/access"),
    route!("GET", "/graphs/{graph_id}/blob"),
    route!("PUT", "/graphs/{graph_id}/blob"),
    route!("DELETE", "/graphs/{graph_id}/blob"),
    route!("HEAD", "/graphs/{graph_id}/blob"),
    route!("GET", "/graphs/{graph_id}/branches"),
    route!("POST", "/graphs/{graph_id}/branches"),
    route!("GET", "/graphs/{graph_id}/branches/diff"),
    route!("POST", "/graphs/{graph_id}/branches/merge"),
    route!("GET", "/graphs/{graph_id}/branches/merge-base"),
    route!("DELETE", "/graphs/{graph_id}/branches/{branch_name}"),
    route!("GET", "/graphs/{graph_id}/changes"),
    route!("POST", "/graphs/{graph_id}/changes/baseline"),
    route!("GET", "/graphs/{graph_id}/commits"),
    route!("GET", "/graphs/{graph_id}/commits/{commit_id}"),
    route!("DELETE", "/graphs/{graph_id}/commits/{commit_id}"),
    route!("GET", "/graphs/{graph_id}/commits/{commit_id}/changes"),
    route!("POST", "/graphs/{graph_id}/commits/{commit_id}/publish"),
    route!("GET", "/graphs/{graph_id}/diff"),
    route!("POST", "/graphs/{graph_id}/export"),
    route!("POST", "/graphs/{graph_id}/load"),
    route!("POST", "/graphs/{graph_id}/load/ndjson"),
    route!("POST", "/graphs/{graph_id}/mutate"),
    route!("POST", "/graphs/{graph_id}/mutate/if-graph-commit"),
    route!("GET", "/graphs/{graph_id}/queries"),
    route!("POST", "/graphs/{graph_id}/queries/{query_name}"),
    route!(
        "POST",
        "/graphs/{graph_id}/queries/{query_name}/if-graph-commit"
    ),
    route!("POST", "/graphs/{graph_id}/query"),
    route!("GET", "/graphs/{graph_id}/schema"),
    route!("GET", "/graphs/{graph_id}/snapshot"),
    route!("GET", "/healthz"),
    route!("HEAD", "/healthz"),
    route!("GET", "/readyz"),
];

/// A request to a path the contract does not name, or with a method it does not take there.
static OTHER: Route = route!("OTHER", "other");

/// The counters of a request with `method` to `url` below `base` (the server root).
pub fn counters(base: &Url, method: &Method, url: &Url) -> &'static Route {
    let (method, template) = (method_name(method), route(base, url));
    ROUTES
        .iter()
        .find(|route| route.method == method && route.template == template)
        .unwrap_or(&OTHER)
}

/// The queries Studio builds (`gq_language`'s builders name theirs `studio_…`), each counted by
/// its name.
static QUERIES: [Counter; 28] = [
    Counter::new("query", "studio_count", Unit::Count),
    Counter::new("query", "studio_delete", Unit::Count),
    Counter::new("query", "studio_delete_edge", Unit::Count),
    Counter::new("query", "studio_delete_edges", Unit::Count),
    Counter::new("query", "studio_edge_count", Unit::Count),
    Counter::new("query", "studio_edge_degree", Unit::Count),
    Counter::new("query", "studio_edge_page", Unit::Count),
    Counter::new("query", "studio_find", Unit::Count),
    Counter::new("query", "studio_head", Unit::Count),
    Counter::new("query", "studio_insert", Unit::Count),
    Counter::new("query", "studio_insert_edge", Unit::Count),
    Counter::new("query", "studio_neighbour_count", Unit::Count),
    Counter::new("query", "studio_neighbour_values", Unit::Count),
    Counter::new("query", "studio_neighbours", Unit::Count),
    Counter::new("query", "studio_neighbours_without", Unit::Count),
    Counter::new("query", "studio_node", Unit::Count),
    Counter::new("query", "studio_node_fields", Unit::Count),
    Counter::new("query", "studio_node_labels", Unit::Count),
    Counter::new("query", "studio_nodes_fields", Unit::Count),
    Counter::new("query", "studio_page", Unit::Count),
    Counter::new("query", "studio_page_degrees", Unit::Count),
    Counter::new("query", "studio_page_fields", Unit::Count),
    Counter::new("query", "studio_page_neighbours", Unit::Count),
    Counter::new("query", "studio_required_dependents", Unit::Count),
    Counter::new("query", "studio_save_rows", Unit::Count),
    Counter::new("query", "studio_set_rows", Unit::Count),
    Counter::new("query", "studio_undo_delete", Unit::Count),
    Counter::new("query", "studio_value_counts", Unit::Count),
];

/// A person's own query, its name unsaid.
static QUERY: Counter = Counter::new("query", "query", Unit::Count);

/// What a query request is counted as: by the name Studio gave the query it built, and any
/// other as `query`.
pub fn query_counter(name: Option<&str>) -> &'static Counter {
    name.and_then(|name| QUERIES.iter().find(|counter| counter.name == name))
        .unwrap_or(&QUERY)
}

/// The contract's template for `url`'s path below `base` (the server root), or `other` for a
/// path the contract does not name.
pub fn route(base: &Url, url: &Url) -> &'static str {
    let base: Vec<&str> = base
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let path: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let below = path.strip_prefix(base.as_slice()).unwrap_or(&path);
    template(below)
}

fn template(segments: &[&str]) -> &'static str {
    match segments {
        ["healthz"] => "/healthz",
        ["readyz"] => "/readyz",
        [".well-known", "oauth-protected-resource"] => "/.well-known/oauth-protected-resource",
        [".well-known", "oauth-protected-resource", ..] => {
            "/.well-known/oauth-protected-resource/{resource_path}"
        }
        ["cluster", "deployments"] => "/cluster/deployments",
        ["cluster", "deployments", _] => "/cluster/deployments/{deployment_id}",
        ["cluster", "plan"] => "/cluster/plan",
        ["graphs"] => "/graphs",
        ["graphs", "discovery"] => "/graphs/discovery",
        ["graphs", _, rest @ ..] => match rest {
            ["access"] => "/graphs/{graph_id}/access",
            ["blob"] => "/graphs/{graph_id}/blob",
            ["branches"] => "/graphs/{graph_id}/branches",
            ["branches", "diff"] => "/graphs/{graph_id}/branches/diff",
            ["branches", "merge"] => "/graphs/{graph_id}/branches/merge",
            ["branches", "merge-base"] => "/graphs/{graph_id}/branches/merge-base",
            ["branches", _] => "/graphs/{graph_id}/branches/{branch_name}",
            ["changes"] => "/graphs/{graph_id}/changes",
            ["changes", "baseline"] => "/graphs/{graph_id}/changes/baseline",
            ["commits"] => "/graphs/{graph_id}/commits",
            ["commits", _] => "/graphs/{graph_id}/commits/{commit_id}",
            ["commits", _, "changes"] => "/graphs/{graph_id}/commits/{commit_id}/changes",
            ["commits", _, "publish"] => "/graphs/{graph_id}/commits/{commit_id}/publish",
            ["diff"] => "/graphs/{graph_id}/diff",
            ["export"] => "/graphs/{graph_id}/export",
            ["load"] => "/graphs/{graph_id}/load",
            ["load", "ndjson"] => "/graphs/{graph_id}/load/ndjson",
            ["mutate"] => "/graphs/{graph_id}/mutate",
            ["mutate", "if-graph-commit"] => "/graphs/{graph_id}/mutate/if-graph-commit",
            ["queries"] => "/graphs/{graph_id}/queries",
            ["queries", _] => "/graphs/{graph_id}/queries/{query_name}",
            ["queries", _, "if-graph-commit"] => {
                "/graphs/{graph_id}/queries/{query_name}/if-graph-commit"
            }
            ["query"] => "/graphs/{graph_id}/query",
            ["schema"] => "/graphs/{graph_id}/schema",
            ["snapshot"] => "/graphs/{graph_id}/snapshot",
            _ => "other",
        },
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every path in the contract is its own template, so no request is counted as another's.
    #[test]
    fn every_contract_path_is_its_own_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../openapi/omnigraph-0.14.0.json")).unwrap();
        let paths = contract["paths"].as_object().unwrap();
        assert!(paths.len() > 20);
        let base = Url::parse("https://example.test/prefix/").unwrap();
        for path in paths.keys() {
            // A path with its parameters filled in as a request would have them.
            let filled = path
                .split('/')
                .map(|s| if s.starts_with('{') { "x-1" } else { s })
                .collect::<Vec<_>>()
                .join("/");
            let url = base.join(filled.trim_start_matches('/')).unwrap();
            assert_eq!(route(&base, &url), path, "{filled}");
        }
    }

    #[test]
    fn a_query_is_counted_by_studios_name_only() {
        assert_eq!(query_counter(Some("studio_page")).name, "studio_page");
        assert_eq!(query_counter(Some("my_private_query")).name, "query");
        assert_eq!(query_counter(None).name, "query");
    }

    /// Every method of every path in the contract has counters of its own.
    #[test]
    fn every_contract_operation_has_its_counters() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../openapi/omnigraph-0.14.0.json")).unwrap();
        let base = Url::parse("https://example.test/prefix/").unwrap();
        let mut operations = 0;
        for (path, item) in contract["paths"].as_object().unwrap() {
            let filled = path
                .split('/')
                .map(|s| if s.starts_with('{') { "x-1" } else { s })
                .collect::<Vec<_>>()
                .join("/");
            let url = base.join(filled.trim_start_matches('/')).unwrap();
            for method in item.as_object().unwrap().keys() {
                let Ok(method) = method.to_uppercase().parse::<Method>() else {
                    continue;
                };
                if !["GET", "POST", "PUT", "DELETE", "HEAD"].contains(&method.as_str()) {
                    continue;
                }
                operations += 1;
                let route = counters(&base, &method, &url);
                assert_eq!(
                    (route.method, route.template),
                    (method.as_str(), path.as_str())
                );
            }
        }
        assert_eq!(operations, ROUTES.len());
    }

    #[test]
    fn a_path_outside_the_contract_is_other() {
        let base = Url::parse("http://127.0.0.1:8787").unwrap();
        let url = Url::parse("http://127.0.0.1:8787/graphs/g/nothing?x=1").unwrap();
        assert_eq!(route(&base, &url), "other");
    }
}
