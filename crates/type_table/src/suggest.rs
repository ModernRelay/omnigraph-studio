//! Values a filter's value field offers while it is typed (spec 004, filters): an enum's
//! values or `true`/`false` from the schema, and for "related to" the other type's nodes,
//! searched on the server by id or label.

use std::time::Duration;

use gpui::{App, Entity, Task};
use gq_language::{SchemaModel, builder};
use graph_store::GraphSession;

/// One offered value: what goes in the field, and a label shown beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub value: String,
    pub label: Option<String>,
}

/// How many nodes a "related to" search offers.
pub const NODE_SUGGESTIONS: u32 = 8;

/// The pause in typing a field that names a node waits for before it searches.
pub const NODE_SEARCH_DELAY: Duration = Duration::from_millis(120);

/// The rows of `type_name`'s nodes that `text` names (`builder::node_search`), read at
/// `snapshot` once typing pauses: what a field that names a node offers. The search is built at
/// once, so one that cannot be built fails without the wait.
pub fn search_nodes(
    session: &Entity<GraphSession>,
    schema: &SchemaModel,
    type_name: &str,
    text: &str,
    snapshot: Option<String>,
    cx: &App,
) -> Task<Result<serde_json::Value, String>> {
    let query = match builder::node_search(schema, type_name, text, NODE_SUGGESTIONS) {
        Ok(query) => query,
        Err(error) => return Task::ready(Err(error.to_string())),
    };
    let session = session.downgrade();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(NODE_SEARCH_DELAY).await;
        let read = session
            .update(cx, |session, cx| session.read(&query, snapshot, cx))
            .map_err(|_| "The graph was closed.".to_owned())?;
        read.await
            .map(|output| output.rows)
            .map_err(|error| error.to_string())
    })
}

/// A node search's rows (`builder::node_search`) as offers: the id goes in the field, the label
/// beside it.
pub fn from_node_rows(rows: &serde_json::Value) -> Vec<Suggestion> {
    rows.as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let id = row.get("id")?.as_str()?.to_owned();
            let label = row
                .get("label")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            Some(Suggestion { value: id, label })
        })
        .collect()
}

/// The part of the field a suggestion completes: all of it, or for a list (`in`) the text after
/// the last comma.
pub fn needle(text: &str, list: bool) -> &str {
    let needle = if list {
        text.rsplit(',').next().unwrap_or(text)
    } else {
        text
    };
    needle.trim()
}

/// The values that hold the needle, those starting with it first, in their own order
/// otherwise. In a list, values already given are left out.
pub fn from_values(values: &[String], text: &str, list: bool) -> Vec<Suggestion> {
    let needle = needle(text, list).to_lowercase();
    let given: Vec<String> = if list {
        let mut parts: Vec<&str> = text.split(',').collect();
        parts.pop();
        parts
            .iter()
            .map(|part| part.trim().to_lowercase())
            .collect()
    } else {
        Vec::new()
    };
    let mut matching: Vec<&String> = values
        .iter()
        .filter(|value| !given.contains(&value.to_lowercase()))
        .filter(|value| value.to_lowercase().contains(&needle))
        .collect();
    matching.sort_by_key(|value| !value.to_lowercase().starts_with(&needle));
    matching
        .into_iter()
        .map(|value| Suggestion {
            value: value.clone(),
            label: None,
        })
        .collect()
}

/// The field after picking `value`: the value itself, or in a list the values before it, the
/// value, and a separator for the next one.
pub fn complete(text: &str, value: &str, list: bool) -> String {
    if !list {
        return value.to_owned();
    }
    let mut parts: Vec<&str> = text.split(',').map(str::trim).collect();
    parts.pop();
    parts.retain(|part| !part.is_empty());
    parts.push(value);
    format!("{}, ", parts.join(", "))
}

/// Whether an offered value is no more than what is typed, so ↵ needs no pick.
pub fn already_typed(text: &str, value: &str, list: bool) -> bool {
    needle(text, list).eq_ignore_ascii_case(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Vec<String> {
        ["active", "paused", "done", "inactive"]
            .map(str::to_owned)
            .to_vec()
    }

    fn shown(suggestions: &[Suggestion]) -> Vec<&str> {
        suggestions.iter().map(|s| s.value.as_str()).collect()
    }

    #[test]
    fn a_node_search_offers_ids_with_their_labels() {
        let rows = serde_json::json!([
            {"id": "person-000000", "label": "Radia Knuth"},
            {"id": "person-000004"},
            {"label": "no id"},
        ]);
        assert_eq!(
            from_node_rows(&rows),
            [
                Suggestion {
                    value: "person-000000".into(),
                    label: Some("Radia Knuth".into()),
                },
                Suggestion {
                    value: "person-000004".into(),
                    label: None,
                },
            ]
        );
        assert!(from_node_rows(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn values_narrow_as_text_is_typed_with_prefixes_first() {
        assert_eq!(
            shown(&from_values(&values(), "", false)),
            ["active", "paused", "done", "inactive"]
        );
        assert_eq!(
            shown(&from_values(&values(), "act", false)),
            ["active", "inactive"]
        );
        assert_eq!(
            shown(&from_values(&values(), "ACTIVE", false)),
            ["active", "inactive"]
        );
        assert!(from_values(&values(), "zzz", false).is_empty());
    }

    #[test]
    fn a_list_completes_its_last_value_and_skips_those_given() {
        assert_eq!(needle("active, pa", true), "pa");
        assert_eq!(
            shown(&from_values(&values(), "active, ", true)),
            ["paused", "done", "inactive"]
        );
        assert_eq!(complete("active, pa", "paused", true), "active, paused, ");
        assert_eq!(complete("", "done", true), "done, ");
        assert_eq!(complete("ac", "active", false), "active");
        assert!(already_typed("active, done", "Done", true));
        assert!(!already_typed("do", "done", false));
    }
}
