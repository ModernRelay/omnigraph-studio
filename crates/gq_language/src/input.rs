//! What a person typed into a cell or form field, as a property's JSON value, and back (spec
//! 004 cell editing, spec 008 review edits). No GPUI.

use serde_json::Value;

use crate::schema::{Property, ScalarKind};

/// The text an editor starts with for a value. A list is its items separated by commas, an item
/// that holds a comma or a quote, starts or ends with a space, or is empty or `[]` quoted as in
/// CSV (its quotes doubled); an empty list is `[]`. [`parse_input`] reads each back as it was.
pub fn edit_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) if items.is_empty() => EMPTY_LIST.to_owned(),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                let text = edit_text(item);
                let quoted = text.is_empty()
                    || text == EMPTY_LIST
                    || text.contains([',', '"'])
                    || text.trim() != text;
                if quoted {
                    format!("\"{}\"", text.replace('"', "\"\""))
                } else {
                    text
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

/// How an empty list is written.
const EMPTY_LIST: &str = "[]";

/// The items of a list as typed: separated by commas, each trimmed unless quoted; a quoted item
/// keeps its text, a doubled quote standing for one. Items left empty between commas are
/// dropped.
fn list_items(text: &str) -> Result<Vec<String>, String> {
    let mut items = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.next_if_eq(&'"').is_some() {
            let mut item = String::new();
            loop {
                match chars.next() {
                    Some('"') if chars.next_if_eq(&'"').is_some() => item.push('"'),
                    Some('"') => break,
                    Some(c) => item.push(c),
                    None => return Err("A quoted item has no closing quote".into()),
                }
            }
            items.push(item);
            while chars.next_if(|c| c.is_whitespace()).is_some() {}
            match chars.next() {
                None => return Ok(items),
                Some(',') => continue,
                Some(_) => {
                    return Err(
                        "Separate items with commas, and quote an item that holds one".into(),
                    );
                }
            }
        }
        let mut item = String::new();
        let ended = loop {
            match chars.next() {
                None => break true,
                Some(',') => break false,
                Some(c) => item.push(c),
            }
        };
        let item = item.trim();
        if !item.is_empty() {
            items.push(item.to_owned());
        }
        if ended {
            return Ok(items);
        }
    }
}

pub fn parse_scalar(property: &Property, text: &str) -> Result<Value, String> {
    let trimmed = text.trim();
    if let Some(values) = &property.enum_values {
        return if values.iter().any(|v| v == trimmed) {
            Ok(Value::String(trimmed.to_owned()))
        } else {
            Err(format!("Use one of: {}", values.join(", ")))
        };
    }
    match property.kind {
        ScalarKind::Text => Ok(Value::String(text.to_owned())),
        ScalarKind::Integer => integer(&property.base_type, trimmed).map_err(|error| match error {
            IntegerError::NotWhole => "Enter a whole number".to_owned(),
            IntegerError::Negative => "Enter a whole number of at least 0".to_owned(),
            IntegerError::OutOfRange => {
                format!("Enter a whole number that fits in {}", property.base_type)
            }
        }),
        ScalarKind::Float => trimmed
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| "Enter a number".into()),
        ScalarKind::Bool => boolean(trimmed).ok_or_else(|| "Enter true or false".into()),
        ScalarKind::Date => {
            if is_date(trimmed) {
                Ok(Value::String(trimmed.to_owned()))
            } else {
                Err("Enter a date as YYYY-MM-DD".into())
            }
        }
        ScalarKind::DateTime => {
            // `get`, not slicing: a wide character may straddle the tenth byte.
            if trimmed.len() >= 16
                && trimmed.get(..10).is_some_and(is_date)
                && trimmed.as_bytes().get(10) == Some(&b'T')
            {
                Ok(Value::String(trimmed.to_owned()))
            } else {
                Err("Enter a date and time as YYYY-MM-DDTHH:MM:SSZ".into())
            }
        }
        ScalarKind::Vector | ScalarKind::Blob => {
            Err(format!("{} cannot be edited here", property.name))
        }
    }
}

/// Why text is not a whole number of an integer type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntegerError {
    NotWhole,
    /// Below zero, for `U32` or `U64`.
    Negative,
    OutOfRange,
}

/// A whole number in its type's range (`I32`, `I64`, `U32`, `U64`), as a cell and a query
/// parameter both read one.
pub(crate) fn integer(base_type: &str, text: &str) -> Result<Value, IntegerError> {
    if base_type.starts_with('U') {
        if text.starts_with('-') && text.parse::<i64>().is_ok() {
            return Err(IntegerError::Negative);
        }
        let value: u64 = text.parse().map_err(|_| IntegerError::NotWhole)?;
        if base_type == "U32" && u32::try_from(value).is_err() {
            return Err(IntegerError::OutOfRange);
        }
        return Ok(Value::from(value));
    }
    let value: i64 = text.parse().map_err(|_| IntegerError::NotWhole)?;
    if base_type == "I32" && i32::try_from(value).is_err() {
        return Err(IntegerError::OutOfRange);
    }
    Ok(Value::from(value))
}

/// `true`, `yes` or `1`, and `false`, `no` or `0`, in any case.
pub(crate) fn boolean(text: &str) -> Option<Value> {
    match text.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Some(Value::Bool(true)),
        "false" | "no" | "0" => Some(Value::Bool(false)),
        _ => None,
    }
}

pub(crate) fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
        && (1..=12).contains(&text[5..7].parse::<u32>().unwrap_or(0))
        && (1..=31).contains(&text[8..10].parse::<u32>().unwrap_or(0))
}

/// Parses what someone typed into a cell or form field into the property's JSON value, as
/// [`edit_text`] writes it. Empty input is null for a nullable property, and an empty list for a
/// list that must hold one; `[]` is an empty list.
pub fn parse_input(property: &Property, text: &str) -> Result<Value, String> {
    if text.trim().is_empty() {
        return if property.nullable {
            Ok(Value::Null)
        } else if property.list {
            Ok(Value::Array(Vec::new()))
        } else {
            Err(format!("{} is required", property.name))
        };
    }
    if property.list {
        if text.trim() == EMPTY_LIST {
            return Ok(Value::Array(Vec::new()));
        }
        let element = Property {
            list: false,
            nullable: false,
            ..property.clone()
        };
        return list_items(text)?
            .iter()
            .map(|item| parse_scalar(&element, item))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array);
    }
    parse_scalar(property, text)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn property(kind: ScalarKind, base_type: &str) -> Property {
        Property {
            name: "at".into(),
            kind,
            base_type: base_type.into(),
            nullable: true,
            list: false,
            enum_values: None,
            is_key: false,
        }
    }

    /// A list reads back as it was written: an item holding a comma or a quote, an empty one,
    /// and the empty list itself, apart from no value.
    #[test]
    fn a_list_reads_back_as_it_was_written() {
        let tags = Property {
            list: true,
            ..property(ScalarKind::Text, "String")
        };
        for value in [
            json!(["Smith, John", "x"]),
            json!(["say \"hi\"", " padded ", ""]),
            json!(["[]"]),
            json!([]),
            Value::Null,
        ] {
            let text = edit_text(&value);
            assert_eq!(parse_input(&tags, &text), Ok(value.clone()), "{text}");
        }
        assert_eq!(
            edit_text(&json!(["Smith, John", "x"])),
            "\"Smith, John\", x"
        );
        // Typed by hand: spaces around unquoted items go, empty ones are dropped.
        assert_eq!(parse_input(&tags, " a ,, b ,"), Ok(json!(["a", "b"])));
        assert!(parse_input(&tags, "\"open").is_err());
        assert!(parse_input(&tags, "\"a\" b").is_err());
        // A list that must hold one is emptied by clearing it.
        let required = Property {
            nullable: false,
            ..tags.clone()
        };
        assert_eq!(parse_input(&required, ""), Ok(json!([])));
        let numbers = Property {
            list: true,
            ..property(ScalarKind::Integer, "I64")
        };
        assert_eq!(parse_input(&numbers, "1, 2"), Ok(json!([1, 2])));
        assert_eq!(edit_text(&json!([1, 2])), "1, 2");
    }

    #[test]
    fn a_date_time_with_a_wide_character_is_refused_not_a_panic() {
        let at = property(ScalarKind::DateTime, "DateTime");
        assert_eq!(
            parse_scalar(&at, "2024-01-02T10:00:00Z"),
            Ok(Value::String("2024-01-02T10:00:00Z".into()))
        );
        // A full-width digit, as an input method types it, puts the tenth byte inside it.
        assert!(parse_scalar(&at, "2024-01-0１T10:00:00Z").is_err());
        assert!(parse_scalar(&at, "日付日付日付日付").is_err());
    }

    /// A cell takes a whole number in its type's range, as a query parameter does.
    #[test]
    fn a_whole_number_is_read_in_its_types_range() {
        let of = |base_type: &str| property(ScalarKind::Integer, base_type);
        assert!(parse_scalar(&of("I32"), "3000000000").is_err());
        assert_eq!(
            parse_scalar(&of("I64"), "3000000000"),
            Ok(json!(3_000_000_000_i64))
        );
        assert!(parse_scalar(&of("U32"), "-1").is_err());
        assert_eq!(
            parse_scalar(&of("U64"), "18446744073709551615"),
            Ok(json!(u64::MAX))
        );
        let on = property(ScalarKind::Bool, "Bool");
        assert_eq!(parse_scalar(&on, "1"), Ok(json!(true)));
    }
}
