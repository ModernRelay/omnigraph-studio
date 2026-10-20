//! Parameter values typed by a person, read as the JSON a query parameter takes (spec 007).
//!
//! A parameter's type is its `.gq` spelling: `String`, `I32`, `Bool`, `Date`, `[String]`,
//! `Vector(3)`. Values always travel as parameters, never spliced into query text.

use serde_json::Value;

use crate::input::{FloatError, IntegerError};

/// Reads `text` as a value of the parameter type `type_name`. An empty text is `None` (the
/// parameter is sent as null) when the parameter is nullable, and an error otherwise.
pub fn param_value(
    name: &str,
    type_name: &str,
    nullable: bool,
    text: &str,
) -> Result<Option<Value>, String> {
    let type_name = type_name.trim();
    // Spaces are a string value; for every other type they are no value at all.
    let empty = if type_name == "String" {
        text.is_empty()
    } else {
        text.trim().is_empty()
    };
    if empty {
        return if nullable {
            Ok(None)
        } else {
            Err(format!("${name} needs a {type_name} value"))
        };
    }
    scalar_or_list(name, type_name, text).map(Some)
}

fn scalar_or_list(name: &str, type_name: &str, text: &str) -> Result<Value, String> {
    if let Some(inner) = type_name
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return list(name, inner.trim(), text);
    }
    if type_name.starts_with("Vector") {
        return list(name, "F32", text);
    }
    scalar(name, type_name, text)
}

/// A list is JSON (`["a", "b"]`) or comma-separated items (`a, b`), quoted as a cell's are
/// when one holds a comma (`"Smith, John", x`).
fn list(name: &str, item_type: &str, text: &str) -> Result<Value, String> {
    let trimmed = text.trim();
    if trimmed.starts_with('[') {
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|error| format!("${name} is not a JSON list: {error}"))?;
        let Value::Array(items) = value else {
            return Err(format!("${name} must be a list"));
        };
        return items
            .into_iter()
            .map(|item| match item {
                Value::String(s) => scalar(name, item_type, &s),
                Value::Null => Err(format!("${name} cannot hold a null item")),
                other => scalar(name, item_type, &other.to_string()),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array);
    }
    crate::input::list_items(trimmed)
        .map_err(|error| format!("${name}: {error}"))?
        .iter()
        .map(|item| scalar(name, item_type, item))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

fn scalar(name: &str, type_name: &str, text: &str) -> Result<Value, String> {
    let trimmed = text.trim();
    match type_name {
        "String" | "Blob" => Ok(Value::String(text.to_owned())),
        // As a cell's: the engine reads a date or a time in more spellings than one.
        "Date" | "DateTime" if crate::input::begins_a_date(trimmed) => {
            Ok(Value::String(trimmed.to_owned()))
        }
        "Date" => Err(format!("${name} must be a date as YYYY-MM-DD")),
        "DateTime" => Err(format!(
            "${name} must be a date and time as YYYY-MM-DD HH:MM:SS"
        )),
        "Bool" => {
            crate::input::boolean(trimmed).ok_or_else(|| format!("${name} must be true or false"))
        }
        "I32" | "I64" | "U32" | "U64" => {
            crate::input::integer(type_name, trimmed).map_err(|error| match error {
                IntegerError::NotWhole => format!("${name} must be a whole number"),
                IntegerError::Negative => format!("${name} must be a whole number of at least 0"),
                IntegerError::OutOfRange => format!("${name} does not fit in {type_name}"),
            })
        }
        "F32" | "F64" => crate::input::float(type_name, trimmed).map_err(|error| match error {
            FloatError::NotANumber => format!("${name} must be a finite number"),
            FloatError::OutOfRange => format!("${name} does not fit in {type_name}"),
        }),
        // An unknown spelling: send the text and let the server decide.
        _ => Ok(Value::String(text.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn scalars_are_typed() {
        assert_eq!(param_value("n", "I32", false, " 42 "), Ok(Some(json!(42))));
        assert!(param_value("n", "I32", false, "5000000000").is_err());
        assert_eq!(
            param_value("n", "I64", false, "5000000000"),
            Ok(Some(json!(5_000_000_000_i64)))
        );
        assert!(param_value("n", "U32", false, "-1").is_err());
        assert_eq!(param_value("f", "F64", false, "1.5"), Ok(Some(json!(1.5))));
        assert_eq!(
            param_value("b", "Bool", false, "true"),
            Ok(Some(json!(true)))
        );
        assert!(param_value("b", "Bool", false, "maybe").is_err());
        assert_eq!(
            param_value("d", "Date", false, "2024-01-02"),
            Ok(Some(json!("2024-01-02")))
        );
        assert_eq!(
            param_value("t", "DateTime", false, "2024-01-02 10:00:00"),
            Ok(Some(json!("2024-01-02 10:00:00")))
        );
        // As the engine takes them: U64 up to i64's maximum, and an F32 that stays finite.
        assert!(param_value("n", "U64", false, &u64::MAX.to_string()).is_err());
        assert!(param_value("f", "F32", false, "1e39").is_err());
        assert_eq!(
            param_value("f", "F64", false, "1e39"),
            Ok(Some(json!(1e39)))
        );
        assert_eq!(
            param_value("s", "String", false, " Ada "),
            Ok(Some(json!(" Ada ")))
        );
    }

    #[test]
    fn empty_values_are_null_only_when_nullable() {
        assert_eq!(param_value("n", "I32", true, ""), Ok(None));
        assert!(param_value("n", "I32", false, " ").is_err());
        assert_eq!(param_value("s", "String", true, ""), Ok(None));
        assert!(param_value("s", "String", false, "").is_err());
        // A non-empty run of spaces is a string value.
        assert_eq!(
            param_value("s", "String", false, "  "),
            Ok(Some(json!("  ")))
        );
    }

    #[test]
    fn lists_take_json_or_commas() {
        assert_eq!(
            param_value("t", "[String]", false, "a, b"),
            Ok(Some(json!(["a", "b"])))
        );
        assert_eq!(
            param_value("t", "[I32]", false, "[1, 2]"),
            Ok(Some(json!([1, 2])))
        );
        assert_eq!(
            param_value("v", "Vector(3)", false, "0.5, 1, 2"),
            Ok(Some(json!([0.5, 1.0, 2.0])))
        );
        assert!(param_value("t", "[I32]", false, "[1, \"x\"]").is_err());
        // Quoted as a cell's list is: a comma inside an item stays in it.
        assert_eq!(
            param_value("t", "[String]", false, "\"Smith, John\", x"),
            Ok(Some(json!(["Smith, John", "x"])))
        );
        // A list parameter holds no null: the item is refused, not sent as the text "null".
        assert!(param_value("t", "[String]", false, "[\"a\", null]").is_err());
    }
}
