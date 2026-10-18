//! `settings.json` as text the person owns (spec 002). Reading accepts comments and trailing
//! commas, applies every value that fits, and reports each one that does not and each key that
//! does nothing. Writing edits the text in place: only the settings that changed are rewritten,
//! a setting put back to its default is removed, and comments, order and every other key stay.

use serde_json::{Map, Value};

use crate::StudioSettings;

/// What reading `settings.json` found.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// The file's settings: each value that fits, a default where one does not.
    pub settings: StudioSettings,
    /// Each value that was not applied and each key that does nothing, by line.
    pub problems: Vec<String>,
}

/// A step from the document's root: an object's key or a list's position.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(usize),
}

/// The most values the reader sets aside in one file before it gives up on the file.
const MOST_PROBLEMS: usize = 100;

/// Reads a settings document. A value that does not fit is set aside and its setting takes the
/// default; the problem is reported with every other one. An error means the text is not a JSON
/// object at all (its syntax), so nothing in it can be used.
pub fn read(text: &str) -> Result<Reading, String> {
    let mut document = parse(text)?;
    // (line, problem), so they read top to bottom.
    let mut problems: Vec<(usize, String)> = Vec::new();
    for _ in 0..MOST_PROBLEMS {
        let mut ignored = Vec::new();
        let attempt = serde_path_to_error::deserialize::<_, StudioSettings>(
            serde_ignored::Deserializer::new(&document, &mut |path| {
                ignored.push(ignored_steps(&path))
            }),
        );
        match attempt {
            Ok(settings) => {
                problems.extend(ignored.into_iter().map(|steps| {
                    let (line, at) = locate(text, &steps);
                    (line, format!("{at} is not a setting, so it does nothing."))
                }));
                problems.sort_by_key(|(line, _)| *line);
                let problems = problems.into_iter().map(|(_, problem)| problem).collect();
                return Ok(Reading { settings, problems });
            }
            Err(error) => {
                let failed = error_steps(error.path());
                let Some(dropped) = drop_target(&failed) else {
                    return Err(error.into_inner().to_string());
                };
                let reason = plain(&error.into_inner().to_string());
                let (line, at) = locate(text, &dropped);
                problems.push((
                    line,
                    if dropped == failed {
                        format!("{at} was not applied: {reason}.")
                    } else {
                        let part = display(&failed[dropped.len()..]);
                        format!("{at} was not applied: `{part}`: {reason}.")
                    },
                ));
                if !remove(&mut document, &dropped) {
                    return Err(reason);
                }
            }
        }
    }
    Err(format!("more than {MOST_PROBLEMS} values do not fit"))
}

/// The document as JSON with comments and trailing commas allowed. Empty text is an empty
/// document; anything else must be one object.
fn parse(text: &str) -> Result<Value, String> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let value: Value = settings::parse_json_with_comments(text).map_err(|e| e.to_string())?;
    if !value.is_object() {
        return Err("the file must hold one object, `{ … }`".into());
    }
    Ok(value)
}

/// What to set aside for a value that does not fit at `failed`. A list element with a part that
/// does not fit (one saved view) is set aside alone; a list whose element is itself wrong (a
/// number in the agent's command) is set aside whole, so no setting runs with part of its list
/// missing. `None` when the failure is the document itself.
fn drop_target(failed: &[Step]) -> Option<Vec<Step>> {
    match failed
        .iter()
        .position(|step| matches!(step, Step::Index(_)))
    {
        Some(index) if index + 1 < failed.len() => Some(failed[..=index].to_vec()),
        Some(index) => (index > 0).then(|| failed[..index].to_vec()),
        None => (!failed.is_empty()).then(|| failed.to_vec()),
    }
}

/// A deserializer's message in the words of a settings file: a number, a whole number, a list,
/// an object, not Rust's type names.
fn plain(reason: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut rest = reason.split(' ').peekable();
    while let Some(word) = rest.next() {
        let (name, tail) = split_punctuation(word);
        let plain = match name {
            "f32" | "f64" => Some("a number"),
            "u8" | "u16" | "u32" | "u64" | "usize" => Some("a whole number, 0 or more"),
            "i8" | "i16" | "i32" | "i64" | "isize" => Some("a whole number"),
            _ => None,
        };
        match (plain, words.last().map(String::as_str)) {
            (Some(plain), Some("expected")) => words.push(format!("{plain}{tail}")),
            _ if word == "struct" && words.last().map(String::as_str) == Some("expected") => {
                // `expected struct ThemeChoice`: the name is Rust's, the shape an object.
                let tail = rest
                    .next()
                    .map(|name| split_punctuation(name).1)
                    .unwrap_or("");
                words.push(format!("an object{tail}"));
            }
            _ => words.push(match word {
                "sequence" | "sequence," => word.replace("sequence", "list"),
                "map" | "map," => word.replace("map", "object"),
                _ => word.to_owned(),
            }),
        }
    }
    words.join(" ")
}

/// A word and the punctuation it ends with.
fn split_punctuation(word: &str) -> (&str, &str) {
    let end = word.trim_end_matches([',', '.', ';']).len();
    word.split_at(end)
}

/// Removes the value at `path` from the document; false when nothing is there.
fn remove(document: &mut Value, path: &[Step]) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let mut value = document;
    for step in parents {
        let next = match step {
            Step::Key(key) => value.get_mut(key.as_str()),
            Step::Index(index) => value.get_mut(*index),
        };
        let Some(next) = next else {
            return false;
        };
        value = next;
    }
    match (last, value) {
        (Step::Key(key), Value::Object(object)) => object.remove(key).is_some(),
        (Step::Index(index), Value::Array(list)) if *index < list.len() => {
            list.remove(*index);
            true
        }
        _ => false,
    }
}

fn error_steps(path: &serde_path_to_error::Path) -> Vec<Step> {
    let mut steps = Vec::new();
    for segment in path.iter() {
        match segment {
            serde_path_to_error::Segment::Map { key } => steps.push(Step::Key(key.clone())),
            serde_path_to_error::Segment::Seq { index } => steps.push(Step::Index(*index)),
            serde_path_to_error::Segment::Enum { .. } => {}
            // The path ends where the deserializer lost track of it.
            serde_path_to_error::Segment::Unknown => break,
        }
    }
    steps
}

fn ignored_steps(path: &serde_ignored::Path) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut at = path;
    loop {
        at = match at {
            serde_ignored::Path::Root => break,
            serde_ignored::Path::Seq { parent, index } => {
                steps.push(Step::Index(*index));
                parent
            }
            serde_ignored::Path::Map { parent, key } => {
                steps.push(Step::Key(key.clone()));
                parent
            }
            serde_ignored::Path::Some { parent }
            | serde_ignored::Path::NewtypeStruct { parent }
            | serde_ignored::Path::NewtypeVariant { parent } => parent,
        };
    }
    steps.reverse();
    steps
}

/// A path as the person would write it: `studio.tree_page_size`, `graphs."dev/knowledge"`,
/// `agent.command[1]`.
fn display(path: &[Step]) -> String {
    let mut out = String::new();
    for step in path {
        match step {
            Step::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                if !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    out.push_str(key);
                } else {
                    out.push_str(&serde_json::to_string(key).unwrap_or_default());
                }
            }
            Step::Index(index) => out.push_str(&format!("[{index}]")),
        }
    }
    out
}

/// "`path` (line n)", and the line: where the value begins, or the list holding it. A path the
/// text does not show sorts first.
fn locate(text: &str, path: &[Step]) -> (usize, String) {
    let keys: Vec<&str> = path
        .iter()
        .map_while(|step| match step {
            Step::Key(key) => Some(key.as_str()),
            Step::Index(_) => None,
        })
        .collect();
    match settings::find_value_range_in_json_text(text, &keys) {
        Some(range) if !keys.is_empty() => {
            let line = text[..range.start].matches('\n').count() + 1;
            (line, format!("`{}` (line {line})", display(path)))
        }
        _ => (0, format!("`{}`", display(path))),
    }
}

/// The text with every setting that differs between `old` and `new` written in place. A
/// setting put back to its default is removed, and so is an object the removal left empty.
/// Everything else in the text (comments, order, unknown keys, values that did not fit) stays.
/// An error means the text is not a settings document, so it is left alone.
pub fn edit(text: &str, old: &StudioSettings, new: &StudioSettings) -> Result<String, String> {
    parse(text)?;
    // Serializing the settings types to a `Value` cannot fail: every map key is a string.
    let as_value = |settings: &StudioSettings| serde_json::to_value(settings).unwrap_or_default();
    let mut changes = Vec::new();
    diff(
        &as_value(old),
        &as_value(new),
        &as_value(&StudioSettings::default()),
        &mut Vec::new(),
        &mut changes,
    );
    let mut text = if text.trim().is_empty() {
        "{\n}\n".to_owned()
    } else {
        text.to_owned()
    };
    let tab = settings::infer_json_indent_size(&text);
    for (path, value) in &changes {
        match value.clone() {
            None => remove_in_text(&mut text, &path),
            Some(value) => {
                let present = settings::find_value_range_in_json_text(&text, &path).is_some();
                if present || !add_in_text(&mut text, &path, &value, tab) {
                    let (range, replacement) =
                        settings::replace_value_in_json_text(&text, &path, tab, Some(&value), None);
                    text.replace_range(range, &replacement);
                }
            }
        }
    }
    // The edit must leave a document that still reads; otherwise the file stays as it was.
    let edited =
        parse(&text).map_err(|error| format!("the change would break the file ({error})"))?;
    // And one that reads as changed: a key the file sets twice is edited where it first
    // appears, while the last one applies, so the change would be lost without a word.
    for (path, value) in &changes {
        let read = path.iter().try_fold(&edited, |at, key| at.get(key));
        if read != value.as_ref() {
            let at = display(&path.iter().cloned().map(Step::Key).collect::<Vec<_>>());
            return Err(format!(
                "{at} is set more than once, and only the last one applies; remove the others"
            ));
        }
    }
    Ok(text)
}

/// Each setting whose value differs, with its new value, or `None` to remove it: a setting
/// back at its default, or a map entry that is gone. Objects are compared key by key; any
/// other value, a list among them, is replaced whole.
fn diff(
    old: &Value,
    new: &Value,
    default: &Value,
    path: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, Option<Value>)>,
) {
    match (old, new) {
        (Value::Object(old), Value::Object(new)) => {
            let added = new.keys().filter(|key| !old.contains_key(*key));
            for key in old.keys().chain(added) {
                path.push(key.clone());
                let default = default.get(key).unwrap_or(&Value::Null);
                match (old.get(key), new.get(key)) {
                    (Some(old), Some(new)) => diff(old, new, default, path, out),
                    (_, new) => out.push((path.clone(), new.cloned())),
                }
                path.pop();
            }
        }
        _ if old != new => out.push((path.clone(), (new != default).then(|| new.clone()))),
        _ => {}
    }
}

/// Adds a key the text does not have yet as the first key of the deepest object on its path
/// that exists, just after the brace, so a comment above the object's first key stays with that
/// key. False when that object is empty or is not an object: Zed's replacement then writes it.
fn add_in_text(text: &mut String, path: &[String], value: &Value, tab: usize) -> bool {
    let Some(depth) = (0..path.len())
        .rev()
        .find(|depth| settings::find_value_range_in_json_text(text, &path[..*depth]).is_some())
    else {
        return false;
    };
    let (parent, key, rest) = (&path[..depth], &path[depth], &path[depth + 1..]);
    let Some(range) = settings::find_value_range_in_json_text(text, parent) else {
        return false;
    };
    let object = &text[range.clone()];
    let has_keys = parse(object)
        .ok()
        .and_then(|value| value.as_object().map(|object| !object.is_empty()))
        .unwrap_or(false);
    if !object.starts_with('{') || !has_keys {
        return false;
    }
    let value = rest.iter().rev().fold(value.clone(), |value, key| {
        Value::Object(Map::from_iter([(key.clone(), value)]))
    });
    let key = serde_json::to_string(key).unwrap_or_default();
    let after_brace = &object[1..];
    let insert = if after_brace
        .trim_start_matches([' ', '\t'])
        .starts_with(['\n', '\r'])
    {
        let indent = tab * (parent.len() + 1);
        let value = settings::to_pretty_json(&value, tab, indent);
        format!("\n{}{key}: {value},", " ".repeat(indent))
    } else {
        let value = serde_json::to_string(&value).unwrap_or_default();
        if after_brace.starts_with(' ') {
            format!(" {key}: {value},")
        } else {
            format!("{key}: {value}, ")
        }
    };
    text.insert_str(range.start + 1, &insert);
    true
}

/// Removes the key at `path` when the text has it, then each enclosing object the removal left
/// empty. A key the text does not have is left alone: Zed's replacement would write it as null.
fn remove_in_text(text: &mut String, path: &[String]) {
    let mut path = path;
    while !path.is_empty() && settings::find_value_range_in_json_text(text, path).is_some() {
        let (range, replacement) = settings::replace_value_in_json_text(text, path, 0, None, None);
        text.replace_range(range, &replacement);
        path = &path[..path.len() - 1];
        let empty = !path.is_empty()
            && settings::find_value_range_in_json_text(text, path).is_some_and(|range| {
                text[range]
                    .strip_prefix('{')
                    .and_then(|inner| inner.strip_suffix('}'))
                    .is_some_and(|inner| inner.trim().is_empty())
            });
        if !empty {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GraphOverrides, SavedView, ThemeMode};

    fn edited(text: &str, f: impl FnOnce(&mut StudioSettings)) -> String {
        let old = read(text).unwrap().settings;
        let mut new = old.clone();
        f(&mut new);
        edit(text, &old, &new).unwrap()
    }

    #[test]
    fn comments_and_trailing_commas_are_allowed() {
        let reading = read(
            r#"// Studio
            {
                /* the shared page size */
                "studio": { "tree_page_size": 20, },
            }"#,
        )
        .unwrap();
        assert_eq!(reading.settings.studio.tree_page_size, 20);
        assert!(reading.problems.is_empty(), "{:?}", reading.problems);
        assert_eq!(read("").unwrap().settings, StudioSettings::default());
        assert_eq!(read(" \n").unwrap().settings, StudioSettings::default());
    }

    #[test]
    fn broken_syntax_is_one_error_with_its_line() {
        let error =
            read("{\n  \"ui_font_size\": 13\n  \"ui_density\": \"compact\"\n}").unwrap_err();
        assert!(error.contains("line 3"), "{error}");
        assert!(read("[1, 2]").unwrap_err().contains("one object"));
        assert!(read("12").is_err());
    }

    /// One value that does not fit costs only that setting, and every problem is named with
    /// its line, the misspelled key among them.
    #[test]
    fn each_value_that_does_not_fit_takes_its_default_and_the_rest_apply() {
        let text = r#"{
  "ui_font_size": "large",
  "ui_density": "compact",
  "theme": { "mode": "purple", "dark": "Ayu Dark" },
  "stuido": { "tree_page_size": 20 },
  "studio": { "tree_page_size": 20, "canvas_max_nodes": -1 },
  "agent": { "command": ["claude", 3] }
}"#;
        let reading = read(text).unwrap();
        let settings = &reading.settings;
        let defaults = StudioSettings::default();
        assert_eq!(settings.ui_font_size, defaults.ui_font_size);
        assert_eq!(settings.ui_density, crate::Density::Compact);
        assert_eq!(settings.theme.mode, ThemeMode::System);
        assert_eq!(settings.theme.dark, "Ayu Dark");
        assert_eq!(settings.studio.tree_page_size, 20);
        assert_eq!(
            settings.studio.canvas_max_nodes,
            defaults.studio.canvas_max_nodes
        );
        assert!(
            settings.agent.command.is_empty(),
            "no command runs with part of its list"
        );
        let problems = reading.problems.join("\n");
        for expected in [
            "`ui_font_size` (line 2) was not applied: invalid type: string \"large\", expected a \
             number.",
            "`theme.mode` (line 4) was not applied: unknown variant `purple`",
            "`studio.canvas_max_nodes` (line 6) was not applied: invalid value: integer `-1`, \
             expected a whole number, 0 or more.",
            "`agent.command` (line 7) was not applied: `[1]`: invalid type: integer `3`, expected \
             a string.",
            "`stuido` (line 5) is not a setting, so it does nothing.",
        ] {
            assert!(problems.contains(expected), "{expected}\nin\n{problems}");
        }
        assert_eq!(reading.problems.len(), 5, "{problems}");
    }

    #[test]
    fn messages_name_shapes_not_rust_types() {
        assert_eq!(
            plain("invalid type: map, expected struct ThemeChoice"),
            "invalid type: object, expected an object"
        );
        assert_eq!(
            plain("invalid type: sequence, expected f32"),
            "invalid type: list, expected a number"
        );
        assert_eq!(
            plain("invalid value: integer `-1`, expected u32"),
            "invalid value: integer `-1`, expected a whole number, 0 or more"
        );
        assert_eq!(
            plain("unknown variant `a`, expected one of `b`"),
            "unknown variant `a`, expected one of `b`"
        );
    }

    #[test]
    fn a_saved_view_that_does_not_fit_is_set_aside_alone() {
        let text = r#"{"graphs": {"dev/knowledge": {
            "tree_page_size": 10,
            "views": [
                {"name": "Broken", "type": "Person", "filters": [{"property": "a", "op": 3}]},
                {"name": "Kept", "type": "Person"}
            ]
        }}}"#;
        let reading = read(text).unwrap();
        let graph = &reading.settings.graphs["dev/knowledge"];
        assert_eq!(graph.tree_page_size, Some(10));
        let names: Vec<&str> = graph.views.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["Kept"]);
        assert_eq!(reading.problems.len(), 1, "{:?}", reading.problems);
        assert!(
            reading.problems[0].starts_with(
                "`graphs.\"dev/knowledge\".views[0]` (line 3) was not applied: `filters[0].op`:"
            ),
            "{}",
            reading.problems[0]
        );
    }

    /// A key the file sets twice is edited where it first appears while the last one applies:
    /// the edit is refused, naming the key, rather than lost without a word.
    #[test]
    fn an_edit_of_a_key_set_twice_is_refused() {
        let text = "{\n  \"ui_font_size\": 13,\n  \"ui_font_size\": 15\n}\n";
        assert_eq!(read(text).unwrap().settings.ui_font_size, 15.);
        let old = read(text).unwrap().settings;
        let mut new = old.clone();
        new.ui_font_size = 18.;
        let error = edit(text, &old, &new).unwrap_err();
        assert!(
            error.contains("ui_font_size") && error.contains("more than once"),
            "{error}"
        );
    }

    /// The file keeps its comments, order and every key Studio did not change; the change is
    /// written where the setting already is.
    #[test]
    fn an_edit_rewrites_only_the_setting_that_changed() {
        let text = r#"{
  // Fonts
  "ui_font_size": 13,
  "stuido": 1,
  "studio": {
    "tree_page_size": 20, // a short tree
  },
}
"#;
        let out = edited(text, |s| s.studio.tree_page_size = 30);
        assert_eq!(
            out,
            r#"{
  // Fonts
  "ui_font_size": 13,
  "stuido": 1,
  "studio": {
    "tree_page_size": 30, // a short tree
  },
}
"#
        );
    }

    /// A new key goes in first, above a comment that belongs to the key below it; an object
    /// written on one line gets it on that line.
    #[test]
    fn a_new_key_leaves_comments_with_their_keys() {
        let text = "{\n  // Chosen by hand\n  \"ui_font_size\": 13,\n  \"theme\": {\"mode\": \"dark\"}\n}\n";
        let out = edited(text, |s| {
            s.studio.tree_page_size = 20;
            s.theme.dark = "Ayu Dark".into();
        });
        assert_eq!(
            out,
            "{\n  \"studio\": {\n    \"tree_page_size\": 20\n  },\n  // Chosen by hand\n  \
             \"ui_font_size\": 13,\n  \"theme\": {\"dark\": \"Ayu Dark\", \"mode\": \"dark\"}\n}\n"
        );
    }

    #[test]
    fn a_new_setting_is_added_and_nothing_else_is_written() {
        let out = edited("", |s| s.studio.canvas_max_nodes = 3000);
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"studio": {"canvas_max_nodes": 3000}})
        );

        let out = edited("{\n  \"ui_font_size\": 13\n}\n", |s| {
            s.theme.mode = ThemeMode::Dark
        });
        let value: Value = settings::parse_json_with_comments(&out).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"ui_font_size": 13, "theme": {"mode": "dark"}}),
            "{out}"
        );
    }

    /// A setting put back to its default leaves the file, so a later change to the default
    /// reaches the person; an object left empty goes with it.
    #[test]
    fn a_default_is_removed_not_written() {
        let text = "{\n  \"ui_font_size\": 13,\n  \"theme\": {\n    \"mode\": \"dark\"\n  }\n}\n";
        let out = edited(text, |s| s.theme.mode = ThemeMode::System);
        assert_eq!(out, "{\n  \"ui_font_size\": 13\n}\n");
        let out = edited(&out, |s| {
            s.ui_font_size = StudioSettings::default().ui_font_size
        });
        assert_eq!(
            read(&out).unwrap().settings,
            StudioSettings::default(),
            "{out}"
        );
        assert!(!out.contains("ui_font_size"), "{out}");
    }

    /// A value that did not fit stays in the file until its own setting is changed, which
    /// replaces it.
    #[test]
    fn a_value_that_did_not_fit_stays_until_its_setting_changes() {
        let text = "{\n  \"ui_font_size\": \"large\",\n  \"buffer_font_size\": 12\n}\n";
        let out = edited(text, |s| s.buffer_font_size = 15.);
        assert!(out.contains("\"ui_font_size\": \"large\""), "{out}");
        let out = edited(&out, |s| s.ui_font_size = 14.);
        assert!(out.contains("\"ui_font_size\": 14"), "{out}");
        assert!(read(&out).unwrap().problems.is_empty());
    }

    #[test]
    fn graph_entries_are_added_and_removed_whole() {
        let text = "{\n  // per graph\n  \"graphs\": {}\n}\n";
        let out = edited(text, |s| {
            s.graphs.insert(
                "dev/knowledge".into(),
                GraphOverrides {
                    tree_page_size: Some(10),
                    ..Default::default()
                },
            );
        });
        assert_eq!(
            read(&out).unwrap().settings.graphs["dev/knowledge"].tree_page_size,
            Some(10),
            "{out}"
        );
        let out = edited(&out, |s| {
            s.save_view(
                "dev/knowledge",
                SavedView {
                    name: "Founders".into(),
                    type_name: "Person".into(),
                    ..Default::default()
                },
            )
        });
        assert_eq!(
            read(&out)
                .unwrap()
                .settings
                .views_for("dev/knowledge", "Person")
                .len(),
            1
        );
        let out = edited(&out, |s| {
            s.graphs.clear();
        });
        assert!(out.starts_with("{\n  // per graph\n"), "{out}");
        assert!(read(&out).unwrap().settings.graphs.is_empty(), "{out}");
        assert!(!out.contains("dev/knowledge"), "{out}");
    }

    #[test]
    fn broken_text_is_not_edited() {
        let old = StudioSettings::default();
        let mut new = old.clone();
        new.ui_font_size = 13.;
        assert!(edit("{ \"ui_font_size\": ", &old, &new).is_err());
    }
}
