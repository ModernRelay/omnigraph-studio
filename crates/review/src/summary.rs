//! A branch summary from a model (spec 019): the changes condensed as the review groups them,
//! the shape the answer is asked in, and what the person is told goes out. No value goes but
//! a record's title (its `name`, `title` or `subject`), cut short; an update sends the names
//! of the properties that changed.

use graph_store::{FoldedChange, FoldedChanges, TypeCounts};
use serde::Deserialize;
use serde_json::{Value, json};
use studio_workspace::models::{ModelTask, Tier};

pub static TASK: ModelTask = ModelTask {
    name: "branch-summary",
    sends: "The changed records' types, ids and titles, and the names of changed properties",
    tier: Tier::Careful,
    limits: Tier::Careful.limits(),
};

pub const INSTRUCTIONS: &str = "Summarize this graph branch for someone deciding whether to \
merge it. The input lists every change by node or edge type: the operation (added, changed, \
removed), the id, a title where the record has one, and for a change the properties that \
changed. Write plainly, name the types and counts, and point out what a reviewer should check \
before merging: removals, changes to existing records, anything surprising. Answer with the \
JSON object only.";

/// How many changes of one type are listed; the rest are counted.
pub const PER_TYPE: usize = 25;
const TITLE_CHARS: usize = 80;

/// The answer's shape.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "headline": {"type": "string", "description": "One sentence, at most 120 characters, saying what the branch does."},
            "points": {"type": "array", "items": {"type": "string"}, "description": "Three to six short lines, each one group of changes, most important first."},
            "needs_attention": {"type": "array", "items": {"type": "string"}, "description": "What a reviewer should look at before merging; empty when nothing."},
        },
        "required": ["headline", "points", "needs_attention"],
        "additionalProperties": false,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Answer {
    pub headline: String,
    #[serde(default)]
    pub points: Vec<String>,
    #[serde(default)]
    pub needs_attention: Vec<String>,
}

/// The input, how many records' titles it holds, and whether types were cut at [`PER_TYPE`].
pub struct Condensed {
    pub text: String,
    pub titles: usize,
    pub cut: bool,
}

/// The changes condensed: per type in the review's order, the counts by operation, then each
/// change on a line, at most [`PER_TYPE`] per type with the rest counted.
pub fn condense(
    branch: &str,
    target: &str,
    folded: &FoldedChanges,
    counts: &[TypeCounts],
) -> Condensed {
    let mut text = format!(
        "Branch {branch} against {target}: {} changes.\n",
        folded.changes.len()
    );
    let mut titles = 0;
    let mut cut = false;
    // The changes by type, in one pass: filtered again for each type, a review of many types
    // went through every change once per type.
    let mut by_type: std::collections::HashMap<(bool, &str), Vec<&FoldedChange>> =
        std::collections::HashMap::new();
    for change in &folded.changes {
        by_type
            .entry((change.is_edge(), change.type_name.as_str()))
            .or_default()
            .push(change);
    }
    for count in counts {
        let in_type: &[&FoldedChange] = by_type
            .get(&(count.is_edge, count.type_name.as_str()))
            .map_or(&[], Vec::as_slice);
        let kind = if count.is_edge { "edge" } else { "node" };
        text.push_str(&format!(
            "\n## {} ({kind}: {} added, {} changed, {} removed)\n",
            count.type_name, count.added, count.changed, count.removed
        ));
        for change in in_type.iter().take(PER_TYPE) {
            let (op, image) = match (&change.before, &change.after) {
                (None, Some(after)) => ("added", Some(after)),
                (Some(_), None) => ("removed", change.before.as_ref()),
                (Some(_), Some(after)) => ("changed", Some(after)),
                (None, None) => continue,
            };
            text.push_str(&format!("- {op} {}", change.id));
            if let Some(title) = image.and_then(title_of) {
                titles += 1;
                text.push_str(&format!(
                    " \"{}\"",
                    studio_ui::text::cut(title, TITLE_CHARS)
                ));
            }
            if let (Some(before), Some(after)) = (&change.before, &change.after) {
                let mut changed: Vec<&String> = after
                    .properties
                    .iter()
                    .filter(|(key, value)| before.properties.get(*key) != Some(value))
                    .map(|(key, _)| key)
                    .chain(
                        before
                            .properties
                            .keys()
                            .filter(|key| !after.properties.contains_key(*key)),
                    )
                    .collect();
                changed.sort();
                changed.dedup();
                if !changed.is_empty() {
                    let names: Vec<&str> = changed.iter().map(|s| s.as_str()).collect();
                    text.push_str(&format!(" changed: {}", names.join(", ")));
                }
            }
            text.push('\n');
        }
        if in_type.len() > PER_TYPE {
            cut = true;
            text.push_str(&format!("  … and {} more\n", in_type.len() - PER_TYPE));
        }
    }
    Condensed { text, titles, cut }
}

fn title_of(image: &graph_client::api::ChangeImageOutput) -> Option<&str> {
    ["name", "title", "subject"]
        .into_iter()
        .find_map(|key| image.properties.get(key).and_then(Value::as_str))
        .filter(|title| !title.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_client::api::{ChangeImageOutput, EntityKind};

    fn image(pairs: &[(&str, &str)]) -> ChangeImageOutput {
        ChangeImageOutput {
            properties: pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), Value::from(*v)))
                .collect(),
            endpoints: None,
        }
    }

    fn change(
        type_name: &str,
        id: &str,
        before: Option<ChangeImageOutput>,
        after: Option<ChangeImageOutput>,
    ) -> FoldedChange {
        FoldedChange {
            kind: EntityKind::Node,
            type_id: type_name.to_lowercase(),
            type_name: type_name.into(),
            id: id.into(),
            before,
            after,
            commits: vec!["c1".into()],
        }
    }

    /// The input names each change's operation, id and title, and for a changed record the
    /// properties that changed, never their values; past the cap, the rest are counted.
    #[test]
    fn the_input_holds_titles_and_changed_property_names_but_no_values() {
        let mut changes = vec![
            change(
                "Task",
                "t1",
                Some(image(&[("title", "Pay the invoice"), ("status", "open")])),
                Some(image(&[("title", "Pay the invoice"), ("status", "done")])),
            ),
            change(
                "Task",
                "t2",
                None,
                Some(image(&[("title", "Reply to Roman")])),
            ),
            change("Task", "t3", Some(image(&[("title", "Old")])), None),
        ];
        for n in 0..PER_TYPE {
            changes.push(change(
                "Email",
                &format!("e{n}"),
                None,
                Some(image(&[("subject", "Hi")])),
            ));
        }
        changes.push(change("Email", "e-last", None, Some(image(&[]))));
        let folded = FoldedChanges {
            raw_changes: changes.len(),
            changes,
        };
        let counts = vec![
            TypeCounts {
                is_edge: false,
                type_name: "Task".into(),
                added: 1,
                changed: 1,
                removed: 1,
            },
            TypeCounts {
                is_edge: false,
                type_name: "Email".into(),
                added: PER_TYPE + 1,
                changed: 0,
                removed: 0,
            },
        ];
        let condensed = condense("agent/x", "main", &folded, &counts);
        let text = &condensed.text;
        assert!(text.contains("## Task (node: 1 added, 1 changed, 1 removed)"));
        assert!(text.contains("- changed t1 \"Pay the invoice\" changed: status"));
        assert!(text.contains("- added t2 \"Reply to Roman\""));
        assert!(text.contains("- removed t3 \"Old\""));
        assert!(!text.contains("done"), "no values: {text}");
        assert!(text.contains("… and 1 more"));
        assert_eq!(condensed.titles, 3 + PER_TYPE);
        assert!(condensed.cut);
        assert!(
            serde_json::from_str::<Answer>(
                r#"{"headline":"h","points":[],"needs_attention":["x"]}"#
            )
            .is_ok()
        );
    }
}
