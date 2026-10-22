//! A test's check of a JSON value against a JSON Schema (draft 2020-12), for the keywords
//! OmniGraph's operator-config schema uses, so the CLI home Studio writes is held to the schema
//! OmniGraph publishes (`fixtures/omnigraph-operator-config.schema.json`, a copy of
//! `docs/user/cli/operator-config.schema.json` at OmniGraph main `97788d37`). A keyword the
//! check does not know fails it rather than passing unread; annotations are read as such.

use serde_json::Value;

/// What in `value` the schema `root` refuses, each place with why; empty when it fits. `$ref`s
/// resolve in `root` (`#/$defs/…`).
pub fn errors(root: &Value, value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    check(root, root, value, "", &mut found);
    found
}

fn fits(root: &Value, schema: &Value, value: &Value) -> bool {
    let mut found = Vec::new();
    check(root, schema, value, "", &mut found);
    found.is_empty()
}

fn has_type(value: &Value, name: &str) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
        }
        _ => false,
    }
}

/// An ECMA-262 pattern as the `regex` crate reads it: the schema ends its anchored patterns with
/// `$(?![\s\S])`, the end of the text, which is `\z` there.
fn pattern(source: &str) -> Result<regex::Regex, regex::Error> {
    regex::Regex::new(&source.replace(r"$(?![\s\S])", r"\z"))
}

fn check(root: &Value, schema: &Value, value: &Value, at: &str, found: &mut Vec<String>) {
    let place = if at.is_empty() { "/" } else { at };
    let Some(rules) = schema.as_object() else {
        if *schema == Value::Bool(false) {
            found.push(format!("{place}: nothing is allowed here"));
        }
        return;
    };
    let mut fail = |why: String| found.push(format!("{place}: {why}"));
    let mut nested = Vec::new();
    for (keyword, rule) in rules {
        match keyword.as_str() {
            "$schema" | "title" | "description" | "$comment" | "default" | "format" | "$defs"
            | "then" | "else" => {}
            "$ref" => match rule
                .as_str()
                .and_then(|target| target.strip_prefix('#'))
                .and_then(|pointer| root.pointer(pointer))
            {
                Some(target) => nested.push((target, value, at.to_owned())),
                None => fail(format!("unknown $ref {rule}")),
            },
            "type" => {
                if !has_type(value, rule.as_str().unwrap_or_default()) {
                    fail(format!("is not of type {rule}"));
                }
            }
            "const" => {
                if value != rule {
                    fail(format!("is not {rule}"));
                }
            }
            "enum" => {
                if !rule.as_array().is_some_and(|all| all.contains(value)) {
                    fail(format!("is not one of {rule}"));
                }
            }
            "pattern" => {
                if let Some(text) = value.as_str() {
                    match pattern(rule.as_str().unwrap_or_default()) {
                        Ok(pattern) if pattern.is_match(text) => {}
                        Ok(_) => fail(format!("does not match {rule}")),
                        Err(error) => fail(format!("the check cannot read {rule}: {error}")),
                    }
                }
            }
            "minLength" => {
                if let (Some(text), Some(least)) = (value.as_str(), rule.as_u64())
                    && (text.chars().count() as u64) < least
                {
                    fail(format!("is shorter than {least}"));
                }
            }
            "minItems" => {
                if let (Some(items), Some(least)) = (value.as_array(), rule.as_u64())
                    && (items.len() as u64) < least
                {
                    fail(format!("has fewer than {least} items"));
                }
            }
            "required" => {
                if let Some(object) = value.as_object() {
                    for name in rule.as_array().into_iter().flatten() {
                        if let Some(name) = name.as_str()
                            && !object.contains_key(name)
                        {
                            fail(format!("lacks {name}"));
                        }
                    }
                }
            }
            "properties" => {
                if let (Some(object), Some(properties)) = (value.as_object(), rule.as_object()) {
                    for (name, property) in properties {
                        if let Some(field) = object.get(name) {
                            nested.push((property, field, format!("{at}/{name}")));
                        }
                    }
                }
            }
            "additionalProperties" => {
                if let Some(object) = value.as_object() {
                    let known = rules.get("properties").and_then(Value::as_object);
                    for (name, field) in object {
                        if known.is_some_and(|known| known.contains_key(name)) {
                            continue;
                        }
                        if *rule == Value::Bool(false) {
                            fail(format!("does not allow {name}"));
                        } else {
                            nested.push((rule, field, format!("{at}/{name}")));
                        }
                    }
                }
            }
            "propertyNames" => {
                if let Some(object) = value.as_object() {
                    for name in object.keys() {
                        if !fits(root, rule, &Value::String(name.clone())) {
                            fail(format!("does not allow the name {name}"));
                        }
                    }
                }
            }
            "prefixItems" => {
                if let (Some(items), Some(prefix)) = (value.as_array(), rule.as_array()) {
                    for (index, (item, schema)) in items.iter().zip(prefix).enumerate() {
                        nested.push((schema, item, format!("{at}/{index}")));
                    }
                }
            }
            "items" => {
                if let Some(items) = value.as_array() {
                    let skip = rules
                        .get("prefixItems")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    for (index, item) in items.iter().enumerate().skip(skip) {
                        nested.push((rule, item, format!("{at}/{index}")));
                    }
                }
            }
            "allOf" => {
                for schema in rule.as_array().into_iter().flatten() {
                    nested.push((schema, value, at.to_owned()));
                }
            }
            "oneOf" => {
                let matching = rule
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|schema| fits(root, schema, value))
                    .count();
                if matching != 1 {
                    fail(format!("matches {matching} of oneOf, not one"));
                }
            }
            "not" => {
                if fits(root, rule, value) {
                    fail(format!("matches what `not` refuses: {rule}"));
                }
            }
            "if" => {
                let branch = if fits(root, rule, value) {
                    rules.get("then")
                } else {
                    rules.get("else")
                };
                if let Some(branch) = branch {
                    nested.push((branch, value, at.to_owned()));
                }
            }
            other => fail(format!("the check does not know the keyword {other}")),
        }
    }
    for (schema, value, at) in nested {
        check(root, schema, value, &at, found);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    /// The check reads each keyword OmniGraph's schema uses as JSON Schema does, and refuses one
    /// it does not know.
    #[test]
    fn the_check_reads_the_keywords_it_knows_and_refuses_others() {
        let root = json!({
            "type": "object",
            "properties": {
                "name": { "$ref": "#/$defs/name" },
                "list": { "type": "array", "minItems": 1, "prefixItems": [{ "const": "a" }], "items": { "type": "string" } },
                "either": { "oneOf": [{ "type": "integer" }, { "enum": [1, "x"] }] },
                "link": { "type": "string" },
            },
            "required": ["name"],
            "additionalProperties": false,
            "propertyNames": { "pattern": "^[a-z]+$(?![\\s\\S])" },
            "allOf": [{ "if": { "required": ["link"] }, "then": { "not": { "required": ["either"] } } }],
            "$defs": { "name": { "type": "string", "minLength": 2, "pattern": "^[a-z]+$(?![\\s\\S])" } },
        });
        let errors = |value: serde_json::Value| super::errors(&root, &value);
        assert!(errors(json!({ "name": "ab", "list": ["a", "b"], "either": "x" })).is_empty());
        for refused in [
            json!({ "list": ["a"] }),
            json!({ "name": "a" }),
            json!({ "name": "ab", "list": [] }),
            json!({ "name": "ab", "list": ["b"] }),
            json!({ "name": "ab", "list": ["a", 1] }),
            json!({ "name": "ab", "either": 1 }),
            json!({ "name": "ab", "either": true }),
            json!({ "name": "ab", "other": 1 }),
            json!({ "name": "ab", "link": "l", "either": 2 }),
            json!({ "name": "ab\n" }),
        ] {
            assert!(!errors(refused.clone()).is_empty(), "{refused}");
        }
        let unknown = super::errors(&json!({ "maxLength": 1 }), &json!("x"));
        assert!(
            unknown[0].contains("does not know the keyword maxLength"),
            "{unknown:?}"
        );
    }
}
