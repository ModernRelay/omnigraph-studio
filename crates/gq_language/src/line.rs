//! What a row of a node shows after its name (spec 044): the values of the properties its
//! type's layout names, as one short line, `24.5 kg · estimated` or `due 2026-11-20`.

use serde_json::Value;

use crate::schema::{NodeType, Property, ScalarKind};

/// The most characters a text value gives a line: a note's first sentence, cut with "…".
pub const TEXT_ROOM: usize = 60;

/// The line of a node of `node` holding `values`, by property, in the order given: each value
/// as short text, a number and the unit after it (a text property named `unit`, `units`, or
/// ending in `_unit` or `Unit`) as one piece, `·` between pieces. A value with nothing to say
/// (none, false, an empty list or text) is left out, and so is a property the type does not
/// hold. `None` when nothing is left.
pub fn line_text(node: &NodeType, values: &[(String, Value)]) -> Option<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut last_number = false;
    for (name, value) in values {
        let Some(property) = node.property(name) else {
            continue;
        };
        let Some(text) = piece(property, value) else {
            last_number = false;
            continue;
        };
        if last_number && is_unit(property) {
            if let Some(number) = pieces.last_mut() {
                number.push(' ');
                number.push_str(&text);
            }
            last_number = false;
            continue;
        }
        last_number =
            matches!(property.kind, ScalarKind::Integer | ScalarKind::Float) && !property.list;
        pieces.push(text);
    }
    (!pieces.is_empty()).then(|| pieces.join(" · "))
}

/// Whether `property` reads as the unit of a number before it.
fn is_unit(property: &Property) -> bool {
    let name = property.name.as_str();
    matches!(property.kind, ScalarKind::Text)
        && !property.list
        && (matches!(name, "unit" | "units") || name.ends_with("_unit") || name.ends_with("Unit"))
}

/// One value as a line shows it, or `None` when it says nothing.
fn piece(property: &Property, value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        // A truth names its property when it holds, and says nothing when it does not.
        Value::Bool(true) => Some(property.name.clone()),
        Value::Bool(false) => None,
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) => match property.kind {
            // A time reads to the minute; midnight reads as its day.
            ScalarKind::DateTime => Some(short_time(text)),
            ScalarKind::Date => Some(text.clone()),
            _ => first_sentence(text),
        },
        Value::Array(items) => {
            let shown: Vec<String> = items
                .iter()
                .take(3)
                .filter_map(|item| piece(property, item))
                .collect();
            if shown.is_empty() {
                return None;
            }
            let mut text = shown.join(", ");
            if items.len() > 3 {
                text.push_str(&format!(" +{}", items.len() - 3));
            }
            Some(text)
        }
        Value::Object(_) => None,
    }
}

/// `2026-04-04T09:30:00Z` as `2026-04-04 09:30`, and `2026-04-04T00:00:00Z` as `2026-04-04`.
fn short_time(text: &str) -> String {
    let Some((day, time)) = text.split_once('T') else {
        return text.to_owned();
    };
    let minute: String = time.chars().take(5).collect();
    if minute == "00:00" || minute.len() < 5 {
        day.to_owned()
    } else {
        format!("{day} {minute}")
    }
}

/// A text's first sentence on its first line, its markdown marks taken off the line's start,
/// at most [`TEXT_ROOM`] characters, then "…"; `None` for blank text.
fn first_sentence(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let line = line.trim_start_matches(['#', '>', '-', '*', ' ']);
    let end = line
        .char_indices()
        .find(|(at, ch)| matches!(ch, '.' | '!' | '?') && line[at + 1..].starts_with(' '))
        .map_or(line.len(), |(at, ch)| at + ch.len_utf8());
    let sentence = line[..end].trim();
    if sentence.is_empty() {
        return None;
    }
    if sentence.chars().count() <= TEXT_ROOM {
        return Some(sentence.to_owned());
    }
    let cut: String = sentence.chars().take(TEXT_ROOM).collect();
    Some(format!("{}…", cut.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SchemaModel;
    use serde_json::json;

    fn program() -> SchemaModel {
        SchemaModel::parse(
            "node Parameter {\n  slug: String @key\n  name: String\n  value: F64\n  unit: String\n  basis: enum(assumed, estimated, calculated, measured)\n  critical: Bool?\n  due: DateTime?\n  tags: [String]?\n  body: String?\n}\n",
        )
        .unwrap()
    }

    #[test]
    fn a_number_reads_with_its_unit_and_the_rest_apart() {
        let schema = program();
        let node = schema.node("Parameter").unwrap();
        let line = |values: Vec<(&str, Value)>| {
            let values: Vec<(String, Value)> = values
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect();
            line_text(node, &values)
        };
        assert_eq!(
            line(vec![
                ("value", json!(24.5)),
                ("unit", json!("kg")),
                ("basis", json!("estimated"))
            ])
            .as_deref(),
            Some("24.5 kg · estimated")
        );
        // A unit with no number before it is a value of its own.
        assert_eq!(
            line(vec![("unit", json!("kg")), ("value", json!(3))]).as_deref(),
            Some("kg · 3")
        );
        // Nothing to say is left out; a truth names its property.
        assert_eq!(
            line(vec![
                ("critical", json!(true)),
                ("due", json!("2026-11-20T00:00:00Z")),
                ("tags", json!([])),
                ("basis", Value::Null)
            ])
            .as_deref(),
            Some("critical · 2026-11-20")
        );
        assert_eq!(
            line(vec![("due", json!("2026-11-20T09:30:00Z"))]).as_deref(),
            Some("2026-11-20 09:30")
        );
        assert_eq!(line(vec![("critical", json!(false))]), None);
        // A text gives its first sentence, cut when long.
        assert_eq!(
            line(vec![(
                "body",
                json!("## Loves single malt. Lives 15 min away.\nMore")
            )])
            .as_deref(),
            Some("Loves single malt.")
        );
        let long = "word ".repeat(30);
        let shown = line(vec![("body", json!(long))]).unwrap();
        assert!(
            shown.ends_with('…') && shown.chars().count() <= TEXT_ROOM + 1,
            "{shown}"
        );
        assert_eq!(
            line(vec![("tags", json!(["a", "b", "c", "d"]))]).as_deref(),
            Some("a, b, c +1")
        );
    }
}
