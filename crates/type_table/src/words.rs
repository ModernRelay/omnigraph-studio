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
other operator takes one value. Prefer an enum's exact value. Filters are joined with and. \
When the request needs or, not, a count, two tests on one related record or more than one \
hop, put that part in `predicate` as a GQ condition on the row variable $n and leave it out \
of the filters; otherwise predicate is null. When the request names nothing the table has, \
answer with no filters and a null predicate.";

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
    /// For a relationship key, the GQ that names it in a predicate's block, from the row `$n`
    /// to the related record `$r`: `$n attended $r`, or `$r knows $n` for an incoming key
    /// ([`gq_language::builder::relationship_traversal`]).
    pub traversal: Option<String>,
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
            "predicate": {
                "type": ["string", "null"],
                "description": "A GQ condition on $n for what the filters cannot say: or, not, a count, a further hop. Null when the filters say it all.",
            },
        },
        "required": ["filters", "predicate"],
        "additionalProperties": false,
    })
}

/// What the model is told: the type, each key with its kind, values and operators, and the
/// sentence.
pub fn input(type_name: &str, choices: &[Choice], today: &str, sentence: &str) -> String {
    let mut text = format!("Table: rows of type {type_name}. Today is {today}.\n\nKeys:\n");
    // The type's own properties, one line each; then each relationship on a line of its own,
    // their operators said once for all; then each related type's properties once, however
    // many relationships reach it. A wide type is reached by hundreds of relationships, and
    // repeating either for each would pass the task's cap.
    for choice in choices
        .iter()
        .filter(|c| c.property.is_some() && !c.key.contains('.'))
    {
        let property = choice.property.as_ref().expect("filtered");
        text.push_str(&format!(
            "- {}: {}; operators {}\n",
            choice.key,
            kind_of(property),
            op_keys(&choice.ops)
        ));
    }
    let relationships: Vec<&Choice> = choices.iter().filter(|c| c.property.is_none()).collect();
    let common = relationships
        .first()
        .map(|relationship| relationship.ops.clone())
        .unwrap_or_default();
    if !relationships.is_empty() {
        text.push_str(&format!(
            "\nRelationships, each with operators {}, its GQ in a predicate from the row $n to \
             the related record $r, and the related type; `key.<property>` tests the related \
             record's property, with that property's operators:\n",
            op_keys(&common)
        ));
    }
    let mut related: Vec<(&str, Vec<String>)> = Vec::new();
    for relationship in relationships {
        // The GQ names the edge and the related type its end: the menu's label says no more.
        match (&relationship.traversal, &relationship.other_type) {
            (Some(traversal), Some(other)) => {
                text.push_str(&format!("- {}: `{traversal}`, {other}", relationship.key));
            }
            _ => text.push_str(&format!("- {} ({})", relationship.key, relationship.label)),
        }
        if relationship.ops != common {
            text.push_str(&format!("; operators {}", op_keys(&relationship.ops)));
        }
        text.push('\n');
        let Some(other) = relationship.other_type.as_deref() else {
            continue;
        };
        if related.iter().any(|(seen, _)| *seen == other) {
            continue;
        }
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
            related.push((other, nested));
        }
    }
    let mut listed = String::new();
    if !related.is_empty() {
        listed.push_str("\nProperties of related types:\n");
        for (other, nested) in related {
            listed.push_str(&format!("- {other}: {}\n", nested.join(", ")));
        }
    }
    let rest = format!(
        "\nOperators: eq, ne, lt, le, gt, ge compare one value; contains matches text or a \
         list member; starts_with and search (the words, in any order) match text; in takes \
         several values; is_empty and is_not_empty, today, this_week and last_30_days take no \
         value; has_any and has_none take no value and test a relationship; a key with a dot \
         tests the related records' property, which is how to filter by a related record's \
         name or kind; related_to takes one value, the related record's name, only for one \
         exact record.\n\nGQ for `predicate`, on the row $n: a property is $n.<key>; compare \
         with =, !=, <, <=, >, >=, contains, starts_with, in [..]; `is null`; combine with and, \
         or, not, and parentheses. Or, as the whole predicate, one block on a related record: \
         `exists {{ $r: <Type>  <its GQ>  $r.<property> = \"x\"  $r.<other> > 3 }}` or \
         `not {{ ... }}`, with the relationship's GQ as given above, in its direction; a \
         block cannot sit inside or, and holds no count.\n\nRequest: {}",
        sentence.trim()
    );
    // The related types' properties are what a wide type cannot always fit: left out, and said
    // to be, when they would pass the task's cap, so the request is still sent.
    if text.len() + listed.len() + rest.len() > TASK.limits.input_bytes {
        listed = "\nThe related types' properties are left out: there are too many to list.\n"
            .to_owned();
    }
    text + &listed + &rest
}

/// Operators by their keys, as the answer names them: `eq, ne, contains`.
fn op_keys(ops: &[FilterOp]) -> String {
    ops.iter().map(|op| op.key()).collect::<Vec<_>>().join(", ")
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
    /// A GQ condition for what the filters cannot say; the table judges it with the compiler.
    #[serde(default)]
    pub predicate: Option<String>,
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
    if let Some(predicate) = answer.predicate.map(|p| p.trim().to_owned())
        && !predicate.is_empty()
    {
        // The table judges it with the compiler before it is applied.
        filters.push(Filter {
            property: gq_language::builder::WHERE_FILTER.to_owned(),
            op: FilterOp::Expr,
            value: Value::String(predicate),
        });
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
            full_text: false,
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
                traversal: None,
            })
            .collect();
        out.push(Choice {
            key: "out:Attended".into(),
            label: "attended Person".into(),
            ops: FilterOp::for_relationship(),
            property: None,
            other_type: Some("Person".into()),
            traversal: Some("$n attended $r".into()),
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
            predicate: Some(" $n.kind = \"dinner\" or $n.kind = \"party\" ".into()),
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
        assert_eq!(filters.len(), 4);
        assert_eq!(filters[3].op, FilterOp::Expr);
        assert_eq!(
            filters[3].value,
            json!("$n.kind = \"dinner\" or $n.kind = \"party\"")
        );
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
                "Relationships, each with operators has_any, has_none, related_to, its GQ"
            ),
            "{text}"
        );
        assert!(
            text.contains("- out:Attended: `$n attended $r`, Person\n"),
            "{text}"
        );
        assert!(text.ends_with("Request: dinners last month"));
    }

    /// A related type's properties are said once, however many relationships reach it, and a
    /// type too wide for the task's cap leaves them out, says so, and is still asked. Before,
    /// every relationship repeated its operators and the related type's properties, so the
    /// personal graph's widest types (1,122 keys for Person) passed the cap and the request
    /// was refused.
    #[test]
    fn the_input_says_each_related_type_once_and_fits_the_cap() {
        let relationship = |key: &str, other: &str| Choice {
            key: key.into(),
            label: format!("{key} {other}"),
            ops: FilterOp::for_relationship(),
            property: None,
            other_type: Some(other.into()),
            traversal: gq_language::builder::relationship_traversal(key, "$n", "$r"),
        };
        let nested = |key: &str, name: &str, values: &[&str]| {
            let property = property(name, ScalarKind::Text, "String", Some(values));
            Choice {
                key: format!("{key}.{name}"),
                label: name.into(),
                ops: FilterOp::for_property(&property),
                property: Some(property),
                other_type: None,
                traversal: None,
            }
        };
        let mut choices = choices();
        for key in ["out:Attended", "in:Invited"] {
            if key != "out:Attended" {
                choices.push(relationship(key, "Person"));
            }
            choices.push(nested(key, "role", &["host", "guest"]));
        }
        let text = input("Event", &choices, "2026-10-29", "dinners Ada hosted");
        assert_eq!(
            text.matches("- Person: role enum(host, guest)\n").count(),
            1,
            "{text}"
        );
        assert!(
            text.contains("- in:Invited: `$r invited $n`, Person\n"),
            "{text}"
        );

        let values: Vec<String> = (0..12).map(|n| format!("value-{n:02}")).collect();
        let values: Vec<&str> = values.iter().map(String::as_str).collect();
        let mut wide = choices.clone();
        for n in 0..400 {
            let key = format!("out:Rel{n:03}");
            wide.push(relationship(&key, &format!("Type{n:03}")));
            wide.push(nested(&key, "status", &values));
        }
        let text = input("Event", &wide, "2026-10-29", "dinners Ada hosted");
        assert!(
            text.len() <= TASK.limits.input_bytes,
            "{} bytes",
            text.len()
        );
        assert!(
            text.contains("related types' properties are left out"),
            "{text}"
        );
        assert!(!text.contains("- Type000: status"), "{text}");
        assert!(text.contains("- out:Rel399: `$n rel399 $r`, Type399\n"));
        assert!(text.ends_with("Request: dinners Ada hosted"));
    }
}
