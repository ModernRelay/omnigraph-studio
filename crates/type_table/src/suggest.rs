//! Values a filter's value field offers while it is typed (spec 004, filters): an enum's
//! values or `true`/`false` from the schema, and for "related to" the other type's nodes,
//! searched on the server by id or label.

/// One offered value: what goes in the field, and a label shown beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub value: String,
    pub label: Option<String>,
}

/// How many nodes a "related to" search offers.
pub const NODE_SUGGESTIONS: u32 = 8;

/// The part of the field a suggestion completes: all of it, or for a list (`in`) the text after
/// the last comma.
pub fn needle(text: &str, list: bool) -> &str {
    let needle = if list {
        text.rsplit(',').next().unwrap_or(text)
    } else {
        text
    };
    needle.trim()
}

/// The values that hold the needle, those starting with it first, in their own order
/// otherwise. In a list, values already given are left out.
pub fn from_values(values: &[String], text: &str, list: bool) -> Vec<Suggestion> {
    let needle = needle(text, list).to_lowercase();
    let given: Vec<String> = if list {
        let mut parts: Vec<&str> = text.split(',').collect();
        parts.pop();
        parts
            .iter()
            .map(|part| part.trim().to_lowercase())
            .collect()
    } else {
        Vec::new()
    };
    let mut matching: Vec<&String> = values
        .iter()
        .filter(|value| !given.contains(&value.to_lowercase()))
        .filter(|value| value.to_lowercase().contains(&needle))
        .collect();
    matching.sort_by_key(|value| !value.to_lowercase().starts_with(&needle));
    matching
        .into_iter()
        .map(|value| Suggestion {
            value: value.clone(),
            label: None,
        })
        .collect()
}

/// The field after picking `value`: the value itself, or in a list the values before it, the
/// value, and a separator for the next one.
pub fn complete(text: &str, value: &str, list: bool) -> String {
    if !list {
        return value.to_owned();
    }
    let mut parts: Vec<&str> = text.split(',').map(str::trim).collect();
    parts.pop();
    parts.retain(|part| !part.is_empty());
    parts.push(value);
    format!("{}, ", parts.join(", "))
}

/// Whether an offered value is no more than what is typed, so ↵ needs no pick.
pub fn already_typed(text: &str, value: &str, list: bool) -> bool {
    needle(text, list).eq_ignore_ascii_case(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Vec<String> {
        ["active", "paused", "done", "inactive"]
            .map(str::to_owned)
            .to_vec()
    }

    fn shown(suggestions: &[Suggestion]) -> Vec<&str> {
        suggestions.iter().map(|s| s.value.as_str()).collect()
    }

    #[test]
    fn values_narrow_as_text_is_typed_with_prefixes_first() {
        assert_eq!(
            shown(&from_values(&values(), "", false)),
            ["active", "paused", "done", "inactive"]
        );
        assert_eq!(
            shown(&from_values(&values(), "act", false)),
            ["active", "inactive"]
        );
        assert_eq!(
            shown(&from_values(&values(), "ACTIVE", false)),
            ["active", "inactive"]
        );
        assert!(from_values(&values(), "zzz", false).is_empty());
    }

    #[test]
    fn a_list_completes_its_last_value_and_skips_those_given() {
        assert_eq!(needle("active, pa", true), "pa");
        assert_eq!(
            shown(&from_values(&values(), "active, ", true)),
            ["paused", "done", "inactive"]
        );
        assert_eq!(complete("active, pa", "paused", true), "active, paused, ");
        assert_eq!(complete("", "done", true), "done, ");
        assert_eq!(complete("ac", "active", false), "active");
        assert!(already_typed("active, done", "Done", true));
        assert!(!already_typed("do", "done", false));
    }
}
