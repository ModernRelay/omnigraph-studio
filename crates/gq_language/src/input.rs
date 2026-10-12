//! What a person typed into a cell or form field, as a property's JSON value, and back (spec
//! 004 cell editing, spec 008 review edits). No GPUI.

use serde_json::Value;

use crate::schema::{Property, ScalarKind};

/// The text an editor starts with for a value.
pub fn edit_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(edit_text).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
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
        ScalarKind::Integer => trimmed
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| "Enter a whole number".into()),
        ScalarKind::Float => trimmed
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| "Enter a number".into()),
        ScalarKind::Bool => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "yes" => Ok(Value::Bool(true)),
            "false" | "no" => Ok(Value::Bool(false)),
            _ => Err("Enter true or false".into()),
        },
        ScalarKind::Date => {
            if is_date(trimmed) {
                Ok(Value::String(trimmed.to_owned()))
            } else {
                Err("Enter a date as YYYY-MM-DD".into())
            }
        }
        ScalarKind::DateTime => {
            if trimmed.len() >= 16 && is_date(&trimmed[..10]) && trimmed.as_bytes()[10] == b'T' {
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

pub fn is_date(text: &str) -> bool {
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

/// Parses what someone typed into a cell or form field into the property's JSON value. Empty
/// input is null for a nullable property. Lists are comma-separated.
pub fn parse_input(property: &Property, text: &str) -> Result<Value, String> {
    if text.trim().is_empty() {
        return if property.nullable {
            Ok(Value::Null)
        } else {
            Err(format!("{} is required", property.name))
        };
    }
    if property.list {
        let element = Property {
            list: false,
            nullable: false,
            ..property.clone()
        };
        return text
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| parse_scalar(&element, item))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array);
    }
    parse_scalar(property, text)
}
