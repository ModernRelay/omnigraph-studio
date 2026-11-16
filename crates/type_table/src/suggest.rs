//! Values a filter's value field offers while it is typed (spec 004, filters): an enum's
//! values or `true`/`false` from the schema, and for "related to" the other type's nodes,
//! found by id or name (spec 031).

use std::time::Duration;

use gpui::{App, Entity, Task};
use gq_language::input::{edit_text, list_items};
use graph_store::GraphSession;
use graph_store::node_search::TypeHits;
use serde_json::Value;

/// One offered value: what goes in the field, and a label shown beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub value: String,
    pub label: Option<String>,
}

/// How many nodes a field that names a node offers.
pub const NODE_SUGGESTIONS: usize = 8;

/// The pause in typing a field that names a node waits for before it searches.
pub const NODE_SEARCH_DELAY: Duration = Duration::from_millis(120);

/// The nodes of `type_name` that `text` names, best first (`GraphSession::find_nodes`, spec
/// 031), at `snapshot` once typing pauses: what a field that names a node offers.
pub fn search_nodes(
    session: &Entity<GraphSession>,
    type_name: &str,
    text: &str,
    snapshot: Option<String>,
    cx: &App,
) -> Task<Result<TypeHits, String>> {
    let session = session.downgrade();
    let (type_name, text) = (type_name.to_owned(), text.to_owned());
    cx.spawn(async move |cx| {
        cx.background_executor().timer(NODE_SEARCH_DELAY).await;
        let find = session
            .update(cx, |session, cx| {
                session.find_nodes(&type_name, &text, snapshot, None, NODE_SUGGESTIONS, cx)
            })
            .map_err(|_| "The graph was closed.".to_owned())?;
        find.await.map_err(|error| error.to_string())
    })
}

/// A node search's hits as offers, best first: the id goes in the field, the name beside it.
pub fn from_hits(hits: &TypeHits) -> Vec<Suggestion> {
    hits.hits
        .iter()
        .map(|hit| Suggestion {
            value: hit.id.clone(),
            label: hit.label.clone(),
        })
        .collect()
}

/// Whether suggestions complete the field: always, but for a list typed as JSON (a list pasted
/// as it is copied), which a pick would rewrite as items, its JSON lost.
pub fn completes(text: &str, list: bool) -> bool {
    !list || !text.trim_start().starts_with('[')
}

/// The part of the field a suggestion completes: all of it, or for a list (`in`) its last item
/// as typed so far, read as [`list_items`] reads one: unquoted, a doubled quote standing for
/// one, and a quote not closed yet allowed.
pub fn needle(text: &str, list: bool) -> String {
    if list {
        last_item(text).1
    } else {
        text.trim().to_owned()
    }
}

/// Where a list's last item starts in `text`, past the comma before it, and that item as typed
/// so far. A comma inside a quoted item is part of it, as [`list_items`] reads it.
fn last_item(text: &str) -> (usize, String) {
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    loop {
        while chars.next_if(|(_, c)| c.is_whitespace()).is_some() {}
        let mut item = String::new();
        if chars.next_if(|(_, c)| *c == '"').is_some() {
            loop {
                match chars.next() {
                    Some((_, '"')) if chars.next_if(|(_, c)| *c == '"').is_some() => item.push('"'),
                    Some((_, '"')) => break,
                    Some((_, c)) => item.push(c),
                    None => return (start, item),
                }
            }
            // Past the closing quote, up to the next comma; text there is the list's error.
            loop {
                match chars.next() {
                    None => return (start, item),
                    Some((at, ',')) => {
                        start = at + 1;
                        break;
                    }
                    Some(_) => {}
                }
            }
            continue;
        }
        loop {
            match chars.next() {
                None => return (start, item.trim().to_owned()),
                Some((at, ',')) => {
                    start = at + 1;
                    break;
                }
                Some((_, c)) => item.push(c),
            }
        }
    }
}

/// The items given before a list's last one, as [`list_items`] reads them; none while they do
/// not read.
fn given_items(text: &str) -> Vec<String> {
    let (start, _) = last_item(text);
    list_items(&text[..start]).unwrap_or_default()
}

/// The values that hold the needle, those starting with it first, in their own order
/// otherwise. In a list, values already given are left out.
pub fn from_values(values: &[String], text: &str, list: bool) -> Vec<Suggestion> {
    let needle = needle(text, list).to_lowercase();
    let given: Vec<String> = if list {
        given_items(text)
            .iter()
            .map(|item| item.to_lowercase())
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
/// value, and a separator for the next one, each written as a list cell writes its items
/// ([`edit_text`]): one holding a comma or a quote is quoted, so it reads back whole.
pub fn complete(text: &str, value: &str, list: bool) -> String {
    if !list {
        return value.to_owned();
    }
    let (start, _) = last_item(text);
    let before = &text[..start];
    let picked = Value::String(value.to_owned());
    match list_items(before) {
        Ok(items) => {
            let mut items: Vec<Value> = items.into_iter().map(Value::String).collect();
            items.push(picked);
            format!("{}, ", edit_text(&Value::Array(items)))
        }
        // Items that do not read stay as typed, for the filter to say why.
        Err(_) => format!("{before} {}, ", edit_text(&Value::Array(vec![picked]))),
    }
}

/// Whether an offered value is no more than what is typed, so ↵ needs no pick. Exactly as
/// typed: an enum's members and a node's id are matched as written, so `active` typed with
/// `Active` offered takes the offer.
pub fn already_typed(text: &str, value: &str, list: bool) -> bool {
    needle(text, list) == value
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_store::node_search::{MatchRank, NodeHit};

    fn values() -> Vec<String> {
        ["active", "paused", "done", "inactive"]
            .map(str::to_owned)
            .to_vec()
    }

    fn shown(suggestions: &[Suggestion]) -> Vec<&str> {
        suggestions.iter().map(|s| s.value.as_str()).collect()
    }

    fn hit(id: &str, label: &str, rank: MatchRank) -> NodeHit {
        NodeHit {
            id: id.into(),
            label: Some(label.into()),
            matched: None,
            rank,
        }
    }

    fn hits(hits: Vec<NodeHit>, more: bool) -> TypeHits {
        TypeHits {
            hits,
            more,
            held: true,
        }
    }

    #[test]
    fn a_node_search_offers_ids_with_their_names_best_first() {
        let found = hits(
            vec![
                hit("o-2", "Acme", MatchRank::Exact),
                hit("o-1", "Acme Analytics", MatchRank::Prefix),
            ],
            false,
        );
        assert_eq!(
            from_hits(&found),
            [
                Suggestion {
                    value: "o-2".into(),
                    label: Some("Acme".into()),
                },
                Suggestion {
                    value: "o-1".into(),
                    label: Some("Acme Analytics".into()),
                },
            ]
        );
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

    /// A list pasted as JSON is not completed: a pick would rewrite it as items. Before, a
    /// pick in `["Founder", "Sm` wrote `["Founder"` back as a quoted item.
    #[test]
    fn a_list_typed_as_json_is_not_completed() {
        assert!(!completes(r#"["Founder", "Sm"#, true));
        assert!(completes(r#"Founder, "Sm"#, true));
        assert!(
            completes("[draft", false),
            "a single value is completed whatever it holds"
        );
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
        assert!(already_typed("active, done", "done", true));
        assert!(
            !already_typed("active, done", "Done", true),
            "another case is a pick"
        );
        assert!(!already_typed("do", "done", false));
    }

    /// A list's items are read and written as a list cell's: a comma or a quote inside a
    /// quoted item is part of it, and a picked value that holds one is quoted, so the filter
    /// reads every item back whole.
    #[test]
    fn a_list_reads_and_writes_quoted_items_whole() {
        let values: Vec<String> = ["Smith, John", "Doe", "Say \"hi\""]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(needle("Doe, \"Smith, J", true), "Smith, J");
        assert_eq!(needle("\"a \"\"b\"\"\"", true), "a \"b\"");
        assert_eq!(
            shown(&from_values(&values, "\"smith, j", true)),
            ["Smith, John"]
        );
        assert_eq!(
            shown(&from_values(&values, "\"Smith, John\", ", true)),
            ["Doe", "Say \"hi\""],
            "a quoted item given is left out"
        );
        assert!(already_typed("Doe, \"Smith, John\"", "Smith, John", true));

        let picked = complete("Doe, Smi", "Smith, John", true);
        assert_eq!(picked, "Doe, \"Smith, John\", ");
        assert_eq!(list_items(&picked).unwrap(), ["Doe", "Smith, John"]);
        let picked = complete("\"a,b\", Sa", "Say \"hi\"", true);
        assert_eq!(
            picked, "\"a,b\", \"Say \"\"hi\"\"\", ",
            "the item given stays whole"
        );
        assert_eq!(list_items(&picked).unwrap(), ["a,b", "Say \"hi\""]);
    }
}
