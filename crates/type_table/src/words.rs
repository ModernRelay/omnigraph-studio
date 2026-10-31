//! A table filter from a sentence (spec 019): what the table sends the model (the type's
//! properties, enums, relationships and operators, and the sentence), the shape it asks the
//! answer in, and the answer checked against what the table offers. Every filter that comes
//! back goes through the same keys, operators and value parsing a filter typed by hand does;
//! one the type cannot take is dropped and named.

use gq_language::builder::{Filter, FilterOp};
use gq_language::schema::Property;
use serde::Deserialize;
use serde_json::{Value, json};
use studio_workspace::models::{Limits, ModelTask, Tier};

use crate::model::parse_filter_value;

pub static TASK: ModelTask = ModelTask {
    name: "table-filter",
    sends: "The type's property names, its enum values and your sentence",
    tier: Tier::Fast,
    // A wide type with many relationships describes itself in more than the tier's cap; the
    // call is still one short answer.
    limits: Limits {
        input_bytes: 32 * 1024,
        ..Tier::Fast.limits()
    },
};

pub const INSTRUCTIONS: &str = "Turn the request into filters on a table of rows of one type. \
Use only the properties and operators listed. Answer with the JSON object only. A date is \
ISO 8601. An operator that takes no value gets an empty list; `in` lists every member; every \
other operator takes one value. Prefer an enum's exact value. When the request names nothing \
the table has, answer with no filters.";

/// A key the table's filter menu offers: a property, or a relationship key.
#[derive(Debug, Clone)]
pub struct Choice {
    pub key: String,
    /// How the menu names it ("attended Event" for a relationship).
    pub label: String,
    pub ops: Vec<FilterOp>,
    pub property: Option<Property>,
    /// For a relationship key, the type at its other end, whose records `related_to` names.
    pub other_type: Option<String>,
}

/// The answer's shape: filters whose property and operator are each one of the keys offered.
pub fn schema(choices: &[Choice]) -> Value {
    let keys: Vec<&str> = choices.iter().map(|choice| choice.key.as_str()).collect();
    let mut ops: Vec<&str> = choices
        .iter()
        .flat_map(|choice| choice.ops.iter().map(|op| op.key()))
        .collect();
    ops.sort_unstable();
    ops.dedup();
    json!({
        "type": "object",
        "properties": {
            "filters": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "property": {"type": "string", "enum": keys},
                        "op": {"type": "string", "enum": ops},
                        "value": {"type": "array", "items": {"type": "string"}},
                    },
                    "required": ["property", "op", "value"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["filters"],
        "additionalProperties": false,
    })
}

/// What the model is told: the type, each key with its kind, values and operators, and the
/// sentence.
pub fn input(type_name: &str, choices: &[Choice], today: &str, sentence: &str) -> String {
    let mut text = format!("Table: rows of type {type_name}. Today is {today}.\n\nKeys:\n");
    // The type's own properties, one line each; then each relationship on one line with the
    // related type's properties after it, so a wide graph stays within the task's cap.
    for choice in choices
        .iter()
        .filter(|c| c.property.is_some() && !c.key.contains('.'))
    {
        let property = choice.property.as_ref().expect("filtered");
        let ops: Vec<&str> = choice.ops.iter().map(|op| op.key()).collect();
        text.push_str(&format!(
            "- {}: {}; operators {}\n",
            choice.key,
            kind_of(property),
            ops.join(", ")
        ));
    }
    for relationship in choices.iter().filter(|c| c.property.is_none()) {
        let ops: Vec<&str> = relationship.ops.iter().map(|op| op.key()).collect();
        text.push_str(&format!(
            "- {} ({}): operators {}",
            relationship.key,
            relationship.label,
            ops.join(", ")
        ));
        let prefix = format!("{}.", relationship.key);
        let nested: Vec<String> = choices
            .iter()
            .filter(|c| c.key.starts_with(&prefix))
            .filter_map(|c| {
                let property = c.property.as_ref()?;
                Some(format!("{} {}", property.name, kind_of(property)))
            })
            .collect();
        if !nested.is_empty() {
            text.push_str(&format!(
                "; or test the related record's property as {prefix}<property>, with that \
                 property's operators: {}",
                nested.join(", ")
            ));
        }
        text.push('\n');
    }
    text.push_str(
        "\nOperators: eq, ne, lt, le, gt, ge compare one value; contains matches text or a \
         list member; starts_with and search (the words, in any order) match text; in takes \
         several values; is_empty and is_not_empty, today, this_week and last_30_days take no \
         value; has_any and has_none take no value and test a relationship; a key with a dot \
         tests the related records' property, which is how to filter by a related record's \
         name or kind; related_to takes one value, the related record's name, only for one \
         exact record.\n\nRequest: ",
    );
    text.push_str(sentence.trim());
    text
}

/// A property's kind in a few words: `String`, `list of String`, `enum(a, b, …)`.
fn kind_of(property: &Property) -> String {
    const ENUM_VALUES_SHOWN: usize = 12;
    let base = match &property.enum_values {
        Some(values) if values.len() > ENUM_VALUES_SHOWN => format!(
            "enum({}, … {} more)",
            values[..ENUM_VALUES_SHOWN].join(", "),
            values.len() - ENUM_VALUES_SHOWN
        ),
        Some(values) => format!("enum({})", values.join(", ")),
        None => property.base_type.clone(),
    };
    if property.list {
        format!("list of {base}")
    } else {
        base
    }
}

#[derive(Debug, Deserialize)]
pub struct Answer {
    pub filters: Vec<Item>,
}

#[derive(Debug, Deserialize)]
pub struct Item {
    pub property: String,
    pub op: String,
    #[serde(default)]
    pub value: Vec<String>,
}

/// The answer as filters the table takes, and the keys it could not use, each with why.
pub fn filters(answer: Answer, choices: &[Choice]) -> (Vec<Filter>, Vec<String>) {
    let mut filters = Vec::new();
    let mut refused = Vec::new();
    for item in answer.filters {
        match filter(&item, choices) {
            Ok(filter) => filters.push(filter),
            Err(why) => refused.push(why),
        }
    }
    (filters, refused)
}

fn filter(item: &Item, choices: &[Choice]) -> Result<Filter, String> {
    let choice = choices
        .iter()
        .find(|choice| choice.key == item.property)
        .ok_or_else(|| format!("{} is not a key of this type", item.property))?;
    let op = FilterOp::from_key(&item.op)
        .filter(|op| choice.ops.contains(op))
        .ok_or_else(|| format!("{} cannot be tested with {}", item.property, item.op))?;
    let value = if !op.takes_value() {
        Value::Null
    } else {
        let first = item
            .value
            .first()
            .ok_or_else(|| format!("{} {} needs a value", item.property, item.op))?;
        match (&choice.property, op) {
            (Some(property), FilterOp::In) => Value::Array(
                item.value
                    .iter()
                    .map(|text| parse_filter_value(property, text))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|why| format!("{}: {why}", item.property))?,
            ),
            (Some(property), _) => parse_filter_value(property, first)
                .map_err(|why| format!("{}: {why}", item.property))?,
            (None, _) => Value::String(first.clone()),
        }
    };
    Ok(Filter {
        property: choice.key.clone(),
        op,
        value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gq_language::schema::ScalarKind;

    fn property(name: &str, kind: ScalarKind, base: &str, values: Option<&[&str]>) -> Property {
        Property {
            name: name.into(),
            kind,
            base_type: base.into(),
            nullable: true,
            list: false,
            enum_values: values.map(|values| values.iter().map(|v| (*v).to_owned()).collect()),
            is_key: false,
            indexed: false,
        }
    }

    fn choices() -> Vec<Choice> {
        let kind = property(
            "kind",
            ScalarKind::Text,
            "String",
            Some(&["dinner", "trip"]),
        );
        let date = property("date", ScalarKind::DateTime, "DateTime", None);
        let mut out: Vec<Choice> = [kind, date]
            .into_iter()
            .map(|property| Choice {
                key: property.name.clone(),
                label: property.name.clone(),
                ops: FilterOp::for_property(&property),
                property: Some(property),
                other_type: None,
            })
            .collect();
        out.push(Choice {
            key: "out:Attended".into(),
            label: "attended Person".into(),
            ops: FilterOp::for_relationship(),
            property: None,
            other_type: Some("Person".into()),
        });
        out
    }

    /// The schema allows only the keys and operators offered, and the answer's filters are
    /// parsed as typed ones would be; one the type cannot take is named, not kept.
    #[test]
    fn an_answer_becomes_filters_the_type_takes() {
        let choices = choices();
        let schema = schema(&choices);
        assert_eq!(
            schema["properties"]["filters"]["items"]["properties"]["property"]["enum"],
            json!(["kind", "date", "out:Attended"])
        );
        let answer = Answer {
            filters: vec![
                Item {
                    property: "kind".into(),
                    op: "in".into(),
                    value: vec!["dinner".into(), "trip".into()],
                },
                Item {
                    property: "date".into(),
                    op: "last_30_days".into(),
                    value: vec![],
                },
                Item {
                    property: "out:Attended".into(),
                    op: "has_any".into(),
                    value: vec![],
                },
                Item {
                    property: "mood".into(),
                    op: "eq".into(),
                    value: vec!["good".into()],
                },
                Item {
                    property: "kind".into(),
                    op: "gt".into(),
                    value: vec!["dinner".into()],
                },
            ],
        };
        let (filters, refused) = filters(answer, &choices);
        assert_eq!(filters.len(), 3);
        assert_eq!(filters[0].op, FilterOp::In);
        assert_eq!(filters[0].value, json!(["dinner", "trip"]));
        assert_eq!(filters[1].op, FilterOp::Last30Days);
        assert_eq!(filters[1].value, Value::Null);
        assert_eq!(filters[2].property, "out:Attended");
        assert_eq!(
            refused,
            [
                "mood is not a key of this type",
                "kind cannot be tested with gt"
            ]
        );
        let text = input("Event", &choices, "2026-10-29", " dinners last month ");
        assert!(
            text.contains("- kind: enum(dinner, trip); operators eq"),
            "{text}"
        );
        assert!(
            text.contains(
                "- out:Attended (attended Person): operators has_any, has_none, related_to"
            ),
            "{text}"
        );
        assert!(text.ends_with("Request: dinners last month"));
    }
}
