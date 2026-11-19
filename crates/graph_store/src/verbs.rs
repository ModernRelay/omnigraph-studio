//! The graph's own verbs (spec 041): the stored mutations that act on a type's nodes, the node
//! types a stored read's columns or a stored mutation's parameters name, and the stamps a
//! graph's writes carry.
//!
//! Omnigraph's catalog says neither which type a stored mutation writes nor where a stored
//! read's columns come from (E43, E64), so each is what the person said once, per graph, in
//! the settings (`graphs."<key>".node_params`, `row_nodes`, `stamps`). Studio guesses nothing
//! it then acts on: a candidate list puts the likely types first, and the person picks.

use std::collections::BTreeMap;

use gpui::App;
use gq_language::SchemaModel;
use gq_language::builder::{Stamp, StampValue};
use graph_client::api::{ParamDescriptor, ParamKind, QueryCatalogEntry};
use studio_settings::StudioSettings;

use crate::GraphSession;

/// A stored mutation that acts on a type's nodes: `param` takes the node's id, or a list
/// holding it.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeAction {
    /// The stored mutation's name.
    pub query: String,
    pub param: String,
    /// What a menu calls it: the query's name in words ("Verify claim").
    pub label: String,
    pub description: Option<String>,
}

/// A stored query's name in words, as an action's label: `verify_claim` → "Verify claim",
/// `addMember` → "Add member".
pub fn action_label(name: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut previous_lower = false;
    for ch in name.chars() {
        if ch == '_' || ch == '-' || ch == ' ' {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            previous_lower = false;
            continue;
        }
        if ch.is_uppercase() && previous_lower && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        previous_lower = ch.is_lowercase() || ch.is_ascii_digit();
        word.extend(ch.to_lowercase());
    }
    if !word.is_empty() {
        words.push(word);
    }
    let mut label = words.join(" ");
    if let Some(first) = label.get(0..1) {
        label = first.to_uppercase() + &label[1..];
    }
    label
}

/// Whether a stored mutation's parameter can name a node: one `String`, or a list of them.
pub fn can_name_node(param: &ParamDescriptor) -> bool {
    match param.kind {
        ParamKind::String => true,
        ParamKind::List => param.item_kind == Some(ParamKind::String),
        _ => false,
    }
}

/// The words of a name, lower case: `verify_claim` → `verify`, `claim`.
fn words(name: &str) -> Vec<String> {
    action_label(name)
        .to_lowercase()
        .split(' ')
        .map(str::to_owned)
        .collect()
}

/// Whether one of `hints`' words is `type_name`, whatever the case, or its plural.
fn named_in(type_name: &str, hints: &[&str]) -> bool {
    let name = type_name.to_lowercase();
    let spaced = action_label(type_name).to_lowercase();
    hints.iter().any(|hint| {
        let hint_words = words(hint);
        let joined = hint_words.join(" ");
        hint_words
            .iter()
            .any(|word| *word == name || word.strip_suffix('s') == Some(name.as_str()))
            || (spaced.contains(' ') && joined.contains(&spaced))
    })
}

/// The node types a parameter or column can name by id (those whose key is one `String`
/// property, [`gq_language::NodeType::id_key`]), those `hints` name first, then in the schema
/// model's order. With `key` (a column `i.slug` names `slug`), only the types keyed by that property,
/// or every one for `@id`.
pub fn node_type_candidates(
    schema: &SchemaModel,
    hints: &[&str],
    key: Option<&str>,
) -> Vec<String> {
    let mut named = Vec::new();
    let mut rest = Vec::new();
    for node in &schema.nodes {
        let Some(id_key) = node.id_key() else {
            continue;
        };
        if let Some(key) = key
            && key != "@id"
            && key != id_key
        {
            continue;
        }
        if named_in(&node.name, hints) {
            named.push(node.name.clone());
        } else {
            rest.push(node.name.clone());
        }
    }
    named.extend(rest);
    named
}

/// The variable and the property a stored read's column is named by, `i` and `slug` for
/// `i.slug`: the server names a projection `$i.slug` so, unless it has an alias.
pub fn column_parts(column: &str) -> Option<(&str, &str)> {
    let (variable, property) = column.split_once('.')?;
    (!variable.is_empty() && !property.is_empty() && !property.contains('.'))
        .then_some((variable, property))
}

impl GraphSession {
    fn settings<'a>(&self, cx: &'a App) -> Option<&'a StudioSettings> {
        cx.try_global::<StudioSettings>()
    }

    /// The node type each column of the stored read `query` names by id, where the person
    /// said so (spec 041), by column.
    pub fn row_nodes(&self, query: &str, cx: &App) -> BTreeMap<String, String> {
        self.settings(cx)
            .map(|settings| settings.row_nodes(&self.overrides_key(), query))
            .unwrap_or_default()
    }

    /// The node type each parameter of the stored mutation `query` names, where the person
    /// said so and the parameter still can, by parameter.
    pub fn node_params(&self, query: &str, cx: &App) -> BTreeMap<String, String> {
        let mut params = self
            .settings(cx)
            .map(|settings| settings.node_params(&self.overrides_key()))
            .and_then(|mut all| all.remove(query))
            .unwrap_or_default();
        if let Some(entry) = self.stored_query(query) {
            params.retain(|name, _| {
                entry
                    .params
                    .iter()
                    .any(|param| param.name == *name && can_name_node(param))
            });
        }
        params
    }

    /// The stored mutations that act on `type_name`'s nodes, in the catalog's order: each
    /// whose parameter the person said names one (spec 041). One the catalog no longer lists
    /// is left out; what the person said of it stays.
    pub fn node_actions(&self, type_name: &str, cx: &App) -> Vec<NodeAction> {
        let Some(catalog) = self.stored_queries.loaded() else {
            return Vec::new();
        };
        let mapped = self
            .settings(cx)
            .map(|settings| settings.node_params(&self.overrides_key()))
            .unwrap_or_default();
        catalog
            .iter()
            .filter(|entry| entry.mutation)
            .filter_map(|entry| {
                let params = mapped.get(&entry.name)?;
                let param = entry.params.iter().find(|param| {
                    can_name_node(param)
                        && params.get(&param.name).map(String::as_str) == Some(type_name)
                })?;
                Some(NodeAction {
                    query: entry.name.clone(),
                    param: param.name.clone(),
                    label: action_label(&entry.name),
                    description: entry.description.clone(),
                })
            })
            .collect()
    }

    /// The node types a parameter of `entry` can name, the likely ones first (spec 041).
    pub fn param_type_candidates(&self, entry: &QueryCatalogEntry, param: &str) -> Vec<String> {
        let Some(schema) = self.schema_model() else {
            return Vec::new();
        };
        node_type_candidates(&schema, &[param, &entry.name], None)
    }

    /// The stamps a hand edit or a new node carries in this graph (spec 041): what the
    /// settings ask, which a write applies to the properties its type declares. The time is
    /// this Mac's, as the engine spells an instant, so a view of the write shows the value it
    /// wrote; an actor's is left out while the actor is not known.
    pub fn stamps(&self, cx: &App) -> Vec<Stamp> {
        let Some(settings) = self.settings(cx) else {
            return Vec::new();
        };
        let now = gq_language::input::engine_instant(chrono::Utc::now());
        settings
            .stamps(&self.overrides_key())
            .into_iter()
            .filter_map(|(property, stamp)| {
                let value = match stamp {
                    studio_settings::Stamp::Now => StampValue::Time(now.clone()),
                    studio_settings::Stamp::Actor => StampValue::Text(self.actor()?.to_owned()),
                };
                Some(Stamp { property, value })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_read_as_words() {
        assert_eq!(action_label("verify_claim"), "Verify claim");
        assert_eq!(action_label("addMember"), "Add member");
        assert_eq!(action_label("close"), "Close");
        assert_eq!(action_label("link-blocks"), "Link blocks");
    }

    /// The types a parameter names in its or its query's name come first; only types whose
    /// id is a one-`String` key are offered; a column offers the types keyed by its property.
    #[test]
    fn candidates_put_the_named_types_first() {
        let schema = SchemaModel::parse(
            "node Issue {\n  slug: String @key\n}\n\
             node Claim {\n  slug: String @key\n}\n\
             node ActionItem {\n  slug: String @key\n}\n\
             node Person {\n  email: String @key\n}\n\
             node Count {\n  n: I32 @key\n}\n\
             node Loose {\n  name: String\n}\n",
        )
        .unwrap();
        assert_eq!(
            node_type_candidates(&schema, &["slug", "verify_claim"], None),
            ["Claim", "ActionItem", "Issue", "Person"]
        );
        assert_eq!(
            node_type_candidates(&schema, &["items", "complete_action_item"], None)[0],
            "ActionItem"
        );
        assert_eq!(
            node_type_candidates(&schema, &["issues"], None)[0],
            "Issue",
            "a plural names its type"
        );
        assert_eq!(
            node_type_candidates(&schema, &["i"], Some("slug")),
            ["ActionItem", "Claim", "Issue"]
        );
        assert_eq!(
            node_type_candidates(&schema, &["p"], Some("email")),
            ["Person"]
        );
        assert_eq!(node_type_candidates(&schema, &[], Some("@id")).len(), 4);
        assert_eq!(column_parts("i.slug"), Some(("i", "slug")));
        assert_eq!(column_parts("i.@id"), Some(("i", "@id")));
        assert_eq!(column_parts("violated"), None);
    }
}
