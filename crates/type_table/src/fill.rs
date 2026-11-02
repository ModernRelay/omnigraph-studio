//! A column filled from other columns by a model (spec 019): one call per row, with the target
//! property's type and values, the row's source cells and the person's instruction; the
//! answer is a value as text, or none, and whether the model is sure. Each answer lands as the
//! cell's draft, which the person saves or discards as any draft.

use gq_language::schema::Property;
use serde::Deserialize;
use serde_json::{Value, json};
use studio_workspace::models::{Limits, ModelTask, Tier};

pub static TASK: ModelTask = ModelTask {
    name: "column-fill",
    sends: "The row's values in the columns you pick, the property's type and your instruction",
    tier: Tier::Fast,
    limits: Limits {
        input_bytes: 4 * 1024,
        output_tokens: 200,
        time: std::time::Duration::from_secs(10),
    },
};

pub const INSTRUCTIONS: &str = "Give the value of one property of a record from its other \
values, following the instruction. Answer with the JSON object only: `value` as text in the \
property's format, or null when the values do not say; `sure` false when you had to guess. \
Never invent what the values do not hold.";

/// How many rows fill at once, and the most one fill covers (constitution IX).
pub const AT_ONCE: usize = 8;
pub const MAX_ROWS: usize = 500;
/// A source value longer than this is cut, so a row stays under the task's cap.
const VALUE_CHARS: usize = 400;
/// And no longer than this in bytes: one character as a reader counts it can be many bytes (an
/// emoji sequence), which would carry a row past the task's cap all the same.
const VALUE_BYTES: usize = VALUE_CHARS * 4;

/// A source value as the input shows it: cut as the interface cuts text, to [`VALUE_CHARS`],
/// and then to [`VALUE_BYTES`] at a character's edge.
fn shown_value(text: &str) -> String {
    let cut = studio_ui::text::cut(text, VALUE_CHARS);
    if cut.len() <= VALUE_BYTES {
        return cut;
    }
    let mut end = VALUE_BYTES - '…'.len_utf8();
    while !cut.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &cut[..end])
}

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "value": {"type": ["string", "null"]},
            "sure": {"type": "boolean"},
        },
        "required": ["value", "sure"],
        "additionalProperties": false,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Answer {
    pub value: Option<String>,
    pub sure: bool,
}

impl Answer {
    /// Whether the value is one a cell takes: some text, not blank.
    pub fn value_fits(&self) -> bool {
        self.value
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty())
    }
}

/// One row's input: the target's type and values, the source cells, the instruction.
pub fn input(target: &Property, sources: &[(String, Value)], instruction: &str) -> String {
    let mut kind = target.base_type.clone();
    if target.list {
        kind = format!("list of {kind}, as JSON");
    }
    let mut text = format!("Property to fill: {} ({kind}", target.name);
    if let Some(values) = &target.enum_values {
        text.push_str(&format!("; exactly one of {}", values.join(", ")));
    }
    text.push_str(").\n\nThe row's other values:\n");
    for (name, value) in sources {
        let shown = match value {
            Value::Null => "(empty)".to_owned(),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        text.push_str(&format!("- {name}: {}\n", shown_value(&shown)));
    }
    text.push_str("\nInstruction: ");
    text.push_str(instruction.trim());
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use gq_language::schema::ScalarKind;

    /// The input names the target's type and enum values, each source cell, and the
    /// instruction; the answer parses with or without a value.
    #[test]
    fn a_rows_input_names_the_target_and_its_sources() {
        let target = Property {
            name: "city".into(),
            kind: ScalarKind::Text,
            base_type: "String".into(),
            nullable: true,
            list: false,
            enum_values: None,
            is_key: false,
            full_text: false,
        };
        let text = input(
            &target,
            &[
                (
                    "address".into(),
                    Value::from("12 Rue Oberkampf, 75011 Paris"),
                ),
                ("country".into(), Value::Null),
            ],
            " the city from the address ",
        );
        assert!(text.starts_with("Property to fill: city (String)."));
        assert!(text.contains("- address: 12 Rue Oberkampf, 75011 Paris\n- country: (empty)\n"));
        assert!(text.ends_with("Instruction: the city from the address"));
        // A long value is cut as the interface cuts text, marked, and an accented letter whole.
        let long = "e\u{301}".repeat(VALUE_CHARS + 10);
        let text = input(&target, &[("notes".into(), Value::from(long))], "the city");
        let shown = format!("- notes: {}…\n", "e\u{301}".repeat(VALUE_CHARS - 1));
        assert!(text.contains(&shown), "{text}");
        // Characters of many bytes each are cut to the byte bound too.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".repeat(VALUE_CHARS);
        let text = input(
            &target,
            &[("notes".into(), Value::from(family))],
            "the city",
        );
        let shown = text
            .lines()
            .find_map(|line| line.strip_prefix("- notes: "))
            .expect("the value's line");
        assert!(shown.len() <= VALUE_BYTES, "{}", shown.len());
        assert!(shown.ends_with('…'));
        assert_eq!(
            serde_json::from_str::<Answer>(r#"{"value":"Paris","sure":true}"#).unwrap(),
            Answer {
                value: Some("Paris".into()),
                sure: true
            }
        );
        assert_eq!(
            serde_json::from_str::<Answer>(r#"{"value":null,"sure":false}"#)
                .unwrap()
                .value,
            None
        );
    }
}
