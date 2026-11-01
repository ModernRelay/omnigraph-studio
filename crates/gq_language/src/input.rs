//! What a person typed into a cell or form field, as a property's JSON value, and back (spec
//! 004 cell editing, spec 008 review edits). No GPUI.

use std::cmp::Ordering;

use serde_json::Value;

use crate::schema::{Property, ScalarKind};

/// How two values of a property order, as the engine compares them; `None` for values of
/// different kinds, or not ordered.
pub fn compare_values(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        // Whole numbers exactly: an I64 or U64 past 2^53 has no exact f64, so two that differ
        // would compare equal and an edit between them would read as no change.
        (Value::Number(a), Value::Number(b)) => match (whole(a), whole(b)) {
            (Some(a), Some(b)) => Some(a.cmp(&b)),
            _ => a.as_f64()?.partial_cmp(&b.as_f64()?),
        },
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// A whole number exactly, as an I64 or a U64 holds it; `None` for one with a fraction.
fn whole(number: &serde_json::Number) -> Option<i128> {
    number
        .as_i64()
        .map(i128::from)
        .or_else(|| number.as_u64().map(i128::from))
}

/// Whether two values of a property are the same value (`1` and `1.0` are).
pub fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_value(a, b))
        }
        _ => a == b || compare_values(a, b) == Some(Ordering::Equal),
    }
}

/// The text a field that edits a value starts with: a cell, a form field, a filter or a query
/// parameter. A list is its items separated by commas, an item that holds a comma or a quote,
/// starts or ends with a space, starts with `[` or is empty quoted as in CSV (its quotes
/// doubled); an empty list is `[]`. [`parse_input`] reads each back as it was, and so does
/// [`crate::param_value`] for a parameter of the list's type, which takes text starting with `[`
/// for JSON. A copied value is `studio_ui::value_text` instead, which writes a list as JSON.
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
                    || text.starts_with('[')
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

/// The items of a list as typed, each as text for its type to read: JSON (`["a", "b"]`, as a
/// list is copied) when it starts with `[`, else [`list_items`]. A cell, a form field and a list
/// parameter read one alike.
pub fn list_texts(text: &str) -> Result<Vec<String>, String> {
    let trimmed = text.trim();
    if !trimmed.starts_with('[') {
        return list_items(trimmed);
    }
    let value: Value =
        serde_json::from_str(trimmed).map_err(|error| format!("Not a JSON list: {error}"))?;
    let Value::Array(items) = value else {
        return Err("Not a list".into());
    };
    items
        .into_iter()
        .map(|item| match item {
            Value::String(text) => Ok(text),
            Value::Null => Err("A list cannot hold an empty item".into()),
            other => Ok(other.to_string()),
        })
        .collect()
}

/// The items of a list as typed: separated by commas, each trimmed unless quoted; a quoted item
/// keeps its text, a doubled quote standing for one. Items left empty between commas are
/// dropped. [`list_texts`] reads text that is not JSON so, and a list filter's suggestions read
/// the items typed so far so.
pub fn list_items(text: &str) -> Result<Vec<String>, String> {
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
        ScalarKind::Float => float(&property.base_type, trimmed).map_err(|error| match error {
            FloatError::NotANumber => "Enter a number".to_owned(),
            FloatError::OutOfRange => format!("Enter a number that fits in {}", property.base_type),
        }),
        ScalarKind::Bool => boolean(trimmed).ok_or_else(|| "Enter true or false".into()),
        ScalarKind::Date if begins_a_date(trimmed) => Ok(Value::String(trimmed.to_owned())),
        ScalarKind::Date => Err("Enter a date as YYYY-MM-DD".into()),
        ScalarKind::DateTime if begins_a_date(trimmed) => Ok(Value::String(date_time(trimmed))),
        ScalarKind::DateTime => Err("Enter a date and time as YYYY-MM-DD HH:MM:SS".into()),
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
/// parameter both read one, and as the engine takes it: its `U64` stops at `i64::MAX` for now.
pub(crate) fn integer(base_type: &str, text: &str) -> Result<Value, IntegerError> {
    if base_type.starts_with('U') {
        if text.starts_with('-') && text.parse::<i64>().is_ok() {
            return Err(IntegerError::Negative);
        }
        let value: u64 = text.parse().map_err(|_| IntegerError::NotWhole)?;
        let fits = match base_type {
            "U32" => u32::try_from(value).is_ok(),
            _ => i64::try_from(value).is_ok(),
        };
        if !fits {
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

/// Why text is not a number of a float type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FloatError {
    NotANumber,
    /// Infinite as an `F32`, which the engine refuses: a parameter when it binds the query, a
    /// value when it writes the column.
    OutOfRange,
}

/// A finite number of a float type (`F32`, `F64`), as a cell and a query parameter both read
/// one: an `F32` must stay finite once narrowed.
pub(crate) fn float(base_type: &str, text: &str) -> Result<Value, FloatError> {
    let value: f64 = text.parse().map_err(|_| FloatError::NotANumber)?;
    // `NaN` and `inf` parse, but are no number JSON holds.
    let number = serde_json::Number::from_f64(value).ok_or(FloatError::NotANumber)?;
    if base_type == "F32" && !(value as f32).is_finite() {
        return Err(FloatError::OutOfRange);
    }
    Ok(Value::Number(number))
}

/// What a cell's or field's text asks for: nothing while it is the text its editor was filled
/// with ([`edit_text`] of `old`), so a value left alone stays as it is, even one its text cannot
/// say again (an empty text, a null in a list, a number past what an input parses); else the
/// value typed, or why the text is not one.
pub fn typed(property: &Property, old: &Value, text: &str) -> Option<Result<Value, String>> {
    (text != edit_text(old)).then(|| parse_input(property, text))
}

/// Whether text can be a date or a time the engine reads: every spelling it takes
/// (`2024-01-02`, `20240102`, `2024-01-02 10:00:00`, `+10000-01-01T…`) begins with a digit, or
/// a sign and a digit. Which of those it takes is the engine's to say; a word is refused here.
pub(crate) fn begins_a_date(text: &str) -> bool {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    digits.starts_with(|c: char| c.is_ascii_digit())
}

/// A time typed in one of the engine's spellings spelled as the engine writes it: in UTC with
/// no zone, and with milliseconds only when they are not zero (`2024-01-02T08:00:00` for
/// `2024-01-02 10:00:00+02:00`). The spellings rewritten are `YYYY-MM-DD`, alone or followed
/// by `T`, `t` or a space and `HH:MM:SS`, a fraction, and `Z` or an offset `±HH:MM`. The
/// engine reads both alike, and values compared here as text then order as the engine orders
/// them. Any other text goes as typed, for the server to judge: one with a digit missing
/// (`24-01-02`), which a lenient reading would take for the year 24; a leap second; digits
/// past the millisecond, which the engine refuses when they are not zero; and spellings the
/// engine reads that are not rewritten here (`20240102`, `+0200`).
pub(crate) fn date_time(text: &str) -> String {
    let bytes = text.as_bytes();
    let dated = bytes.len() > 10
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[7] == b'-';
    let spaced = match bytes.get(10) {
        Some(b' ' | b't') if dated => format!("{}T{}", &text[..10], text[11..].trim_start()),
        _ => text.to_owned(),
    };
    engine_date_time(&spaced).unwrap_or(spaced)
}

/// `text` as the engine writes the instant it names, when it is one of the shapes
/// [`date_time`] rewrites and names an instant to the millisecond.
fn engine_date_time(text: &str) -> Option<String> {
    use chrono::{DateTime, NaiveDate, NaiveDateTime, Timelike as _, Utc};
    if !engine_shape(text) {
        return None;
    }
    if let Some((_, fraction)) = text.split_once('.') {
        let past_millis = fraction
            .bytes()
            .take_while(u8::is_ascii_digit)
            .skip(3)
            .any(|digit| digit != b'0');
        if past_millis {
            return None;
        }
    }
    let at = if let Ok(zoned) = DateTime::parse_from_rfc3339(text) {
        zoned.with_timezone(&Utc).naive_utc()
    } else if let Ok(naive) = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f") {
        naive
    } else {
        NaiveDate::parse_from_str(text, "%Y-%m-%d")
            .ok()?
            .and_hms_opt(0, 0, 0)?
    };
    // A leap second names its instant past the second's end, which this spelling cannot.
    if at.nanosecond() >= 1_000_000_000 {
        return None;
    }
    let seconds = at.format("%Y-%m-%dT%H:%M:%S");
    Some(match at.and_utc().timestamp_subsec_millis() {
        0 => seconds.to_string(),
        millis => format!("{seconds}.{millis:03}"),
    })
}

/// Whether `text` is `YYYY-MM-DD`, alone or with `THH:MM:SS` (after `T`, `t` or a space,
/// already spelled `T` by [`date_time`]), a fraction, and `Z`, `z` or `±HH:MM`: every field its
/// full width, as the engine's own parser takes them.
fn engine_shape(text: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let width = |s: &str, n: usize| s.len() == n && digits(s);
    let Some(date) = text.get(..10) else {
        return false;
    };
    let [year, month, day] = date.splitn(3, '-').collect::<Vec<_>>()[..] else {
        return false;
    };
    if !(width(year, 4) && width(month, 2) && width(day, 2)) {
        return false;
    }
    let Some(rest) = text.get(10..) else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    let Some(rest) = rest.strip_prefix('T') else {
        return false;
    };
    let Some(clock) = rest.get(..8) else {
        return false;
    };
    let [hour, minute, second] = clock.splitn(3, ':').collect::<Vec<_>>()[..] else {
        return false;
    };
    if !(width(hour, 2) && width(minute, 2) && width(second, 2)) {
        return false;
    }
    let mut rest = &rest[8..];
    if let Some(after_point) = rest.strip_prefix('.') {
        let end = after_point
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after_point.len());
        if end == 0 {
            return false;
        }
        rest = &after_point[end..];
    }
    match rest {
        "" | "Z" | "z" => true,
        offset => {
            let Some(offset) = offset.strip_prefix(['+', '-']) else {
                return false;
            };
            let [hours, minutes] = offset.splitn(2, ':').collect::<Vec<_>>()[..] else {
                return false;
            };
            width(hours, 2) && width(minutes, 2)
        }
    }
}

/// `true`, `yes` or `1`, and `false`, `no` or `0`, in any case.
pub(crate) fn boolean(text: &str) -> Option<Value> {
    match text.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Some(Value::Bool(true)),
        "false" | "no" | "0" => Some(Value::Bool(false)),
        _ => None,
    }
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
        let element = Property {
            list: false,
            nullable: false,
            ..property.clone()
        };
        return list_texts(text)?
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
            indexed: false,
        }
    }

    /// A list reads back as it was written: an item holding a comma or a quote, an empty one,
    /// one starting with `[`, and the empty list itself, apart from no value. A list parameter
    /// reads the same text back, and both take a list as it is copied, in JSON. Before, a cell
    /// refused the JSON a parameter took.
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
            json!(["[draft] notes", "b"]),
            json!([]),
            Value::Null,
        ] {
            let text = edit_text(&value);
            assert_eq!(parse_input(&tags, &text), Ok(value.clone()), "{text}");
            if !value.is_null() {
                assert_eq!(
                    crate::param_value("tags", "[String]", false, &text),
                    Ok(Some(value.clone())),
                    "{text}"
                );
            }
        }
        assert_eq!(
            crate::param_value("n", "[I64]", false, &edit_text(&json!([1, 2]))),
            Ok(Some(json!([1, 2])))
        );
        assert_eq!(
            edit_text(&json!(["Smith, John", "x"])),
            "\"Smith, John\", x"
        );
        // Typed by hand: spaces around unquoted items go, empty ones are dropped.
        assert_eq!(parse_input(&tags, " a ,, b ,"), Ok(json!(["a", "b"])));
        // Pasted as it is copied, in JSON.
        assert_eq!(parse_input(&tags, r#"["a", "b"]"#), Ok(json!(["a", "b"])));
        assert!(parse_input(&tags, r#"["a", null]"#).is_err());
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
    fn values_compare_exactly() {
        assert!(same_value(&json!(1), &json!(1.0)));
        assert!(!same_value(&json!(["a"]), &json!(["b"])));
        // Whole numbers past 2^53 compare exactly: an edit between two of them is a change.
        assert!(!same_value(
            &json!(9_007_199_254_740_993_i64),
            &json!(9_007_199_254_740_992_i64)
        ));
        assert!(!same_value(&json!(u64::MAX), &json!(u64::MAX - 1)));
    }

    /// Every spelling the engine reads a time in is spelled as the engine writes it, in UTC
    /// with no zone, so text compared here orders as the engine orders the times. What it
    /// cannot read as one goes as typed, and the engine refuses it.
    #[test]
    fn dates_and_times_are_spelled_as_the_engine_writes_them() {
        let at = property(ScalarKind::DateTime, "DateTime");
        for (typed, written) in [
            ("2024-01-02T10:00:00Z", "2024-01-02T10:00:00"),
            ("2024-01-02T10:00:00+02:00", "2024-01-02T08:00:00"),
            ("2024-01-02 10:00:00", "2024-01-02T10:00:00"),
            ("2024-01-02t10:00:00.250Z", "2024-01-02T10:00:00.250"),
            ("2024-01-02T10:00:00.5", "2024-01-02T10:00:00.500"),
            ("2024-01-02T10:00:00.000", "2024-01-02T10:00:00"),
            ("2024-01-02", "2024-01-02T00:00:00"),
            // Past the millisecond, which the engine refuses, and without seconds, which it
            // does not read: as typed.
            ("2024-01-02T10:00:00.1234", "2024-01-02T10:00:00.1234"),
            ("2024-01-02T10:00", "2024-01-02T10:00"),
            // A digit missing is no year 24 or 202, nor hour 1; a leap second keeps its
            // spelling; one the engine reads but this does not rewrite goes as typed.
            ("24-01-02", "24-01-02"),
            ("202-01-02T10:00:00", "202-01-02T10:00:00"),
            ("2024-01-02T1:00:00", "2024-01-02T1:00:00"),
            ("2024-06-30T23:59:60", "2024-06-30T23:59:60"),
            ("2024-01-02T10:00:00+0200", "2024-01-02T10:00:00+0200"),
        ] {
            assert_eq!(parse_scalar(&at, typed), Ok(json!(written)), "{typed}");
        }
        let on = property(ScalarKind::Date, "Date");
        assert_eq!(parse_scalar(&on, " 2024-01-02 "), Ok(json!("2024-01-02")));
        // A word is no date in any spelling.
        assert!(parse_scalar(&on, "yesterday").is_err());
        assert!(parse_scalar(&at, "soon").is_err());
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
        // The engine's U64 stops at i64's maximum for now.
        assert_eq!(
            parse_scalar(&of("U64"), &i64::MAX.to_string()),
            Ok(json!(i64::MAX))
        );
        assert!(parse_scalar(&of("U64"), "18446744073709551615").is_err());
        let float = |base_type: &str| property(ScalarKind::Float, base_type);
        assert!(parse_scalar(&float("F32"), "1e39").is_err());
        assert_eq!(parse_scalar(&float("F64"), "1e39"), Ok(json!(1e39)));
        let on = property(ScalarKind::Bool, "Bool");
        assert_eq!(parse_scalar(&on, "1"), Ok(json!(true)));
    }
}
