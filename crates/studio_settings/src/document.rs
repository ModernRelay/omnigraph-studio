//! `settings.json` as text the person owns (spec 002). Reading accepts comments and trailing
//! commas, applies every value that fits, and reports each one that does not and each key that
//! does nothing. Writing edits the text in place: only the settings that changed are rewritten,
//! a list entry by entry, a setting put back to its default is removed, and comments, order,
//! every other key and every value set aside stay.

use serde_json::{Map, Value};

use crate::{FieldKind, StudioSettings};

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

/// Reads a settings document. A value that does not fit, a number outside its setting's range
/// among them, is set aside and its setting takes the default; the problem is reported with
/// every other one. An error means the text is not a JSON object at all (its syntax), or that
/// more than [`MOST_PROBLEMS`] values do not fit, so nothing in it is used.
pub fn read(text: &str) -> Result<Reading, String> {
    read_setting_aside(text).map(|(reading, _)| reading)
}

/// As [`read`], with where each value set aside is in the text: its keys, and its position
/// when it is a list element set aside alone.
fn read_setting_aside(text: &str) -> Result<(Reading, Vec<Vec<Step>>), String> {
    let mut document = parse(text)?;
    // (line, problem), so they read top to bottom.
    let mut problems: Vec<(usize, String)> = Vec::new();
    let mut aside: Vec<Vec<Step>> = Vec::new();
    for _ in 0..MOST_PROBLEMS {
        let mut ignored = Vec::new();
        let attempt = serde_path_to_error::deserialize::<_, StudioSettings>(
            serde_ignored::Deserializer::new(&document, &mut |path| {
                ignored.push(ignored_steps(&path))
            }),
        );
        // A number outside its range reads as the type it is: it does not fit all the same.
        let attempt = match attempt {
            Ok(settings) => match out_of_range(&document) {
                Some(found) => Err(found),
                None => Ok(settings),
            },
            Err(error) => {
                let failed = error_steps(error.path());
                Err((failed, plain(&error.into_inner().to_string())))
            }
        };
        match attempt {
            Ok(settings) => {
                problems.extend(ignored.into_iter().map(|steps| {
                    let (line, at) = locate(text, &steps);
                    (line, format!("{at} is not a setting, so it does nothing."))
                }));
                problems.sort_by_key(|(line, _)| *line);
                let problems = problems.into_iter().map(|(_, problem)| problem).collect();
                return Ok((Reading { settings, problems }, aside));
            }
            Err((failed, reason)) => {
                let Some(dropped) = drop_target(&document, &failed) else {
                    return Err(reason);
                };
                let in_text = in_text(&aside, &dropped);
                let (line, at) = locate(text, &in_text);
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
                aside.push(in_text);
            }
        }
    }
    Err(format!("more than {MOST_PROBLEMS} values do not fit"))
}

/// Where `path`, in the document as read so far, is in the text. A list element set aside
/// alone leaves the document, so a later element's position counts it back in. Only the last
/// step of a path set aside is a position ([`drop_target`]), so the keys before it are the
/// text's.
fn in_text(aside: &[Vec<Step>], path: &[Step]) -> Vec<Step> {
    let Some((Step::Index(at), list)) = path.split_last() else {
        return path.to_vec();
    };
    let gone: Vec<usize> = aside
        .iter()
        .filter_map(|set_aside| match set_aside.split_last() {
            Some((Step::Index(index), of)) if of == list => Some(*index),
            _ => None,
        })
        .collect();
    let mut left = *at;
    let mut index = 0;
    while gone.contains(&index) || left > 0 {
        if !gone.contains(&index) {
            left -= 1;
        }
        index += 1;
    }
    let mut path = list.to_vec();
    path.push(Step::Index(index));
    path
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
/// does not fit (one saved view) is set aside alone, and so is one that is itself wrong in a
/// list of objects (a view that is `null`): the others apply, and a later save keeps them. A
/// list of plain values whose element is wrong (a number in the agent's command) is set aside
/// whole, so no setting runs with part of its list missing. `None` when the failure is the
/// document itself.
fn drop_target(document: &Value, failed: &[Step]) -> Option<Vec<Step>> {
    match failed
        .iter()
        .position(|step| matches!(step, Step::Index(_)))
    {
        Some(index) if index + 1 < failed.len() => Some(failed[..=index].to_vec()),
        Some(index) if index > 0 && holds_objects(document, &failed[..index]) => {
            Some(failed[..=index].to_vec())
        }
        Some(index) => (index > 0).then(|| failed[..index].to_vec()),
        None => (!failed.is_empty()).then(|| failed.to_vec()),
    }
}

/// The first number outside its setting's range, the shared value or a graph's own, and why.
fn out_of_range(document: &Value) -> Option<(Vec<Step>, String)> {
    let graphs = document.get("graphs").and_then(Value::as_object);
    for field in crate::fields() {
        let FieldKind::Number { min, max, .. } = field.kind else {
            continue;
        };
        let own = graphs
            .filter(|_| field.per_graph)
            .into_iter()
            .flat_map(Map::keys)
            .filter_map(|graph| {
                let key = field.path.last()?;
                Some(vec!["graphs".to_owned(), graph.clone(), key.clone()])
            });
        for path in std::iter::once(field.path.clone()).chain(own) {
            let Some(number) = value_at(document, &path).and_then(Value::as_f64) else {
                continue;
            };
            if number < min || number > max {
                let steps = path.into_iter().map(Step::Key).collect();
                return Some((
                    steps,
                    format!("`{number}` is out of range, expected {min} to {max}"),
                ));
            }
        }
    }
    None
}

/// Whether the list at `path` holds an object: its elements are records, each its own value.
fn holds_objects(document: &Value, path: &[Step]) -> bool {
    path.iter()
        .try_fold(document, |value, step| match step {
            Step::Key(key) => value.get(key.as_str()),
            Step::Index(index) => value.get(*index),
        })
        .and_then(Value::as_array)
        .is_some_and(|list| list.iter().any(Value::is_object))
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
/// Everything else in the text (comments, order, unknown keys, values that did not fit) stays:
/// a list is edited entry by entry, so its comments and the entries set aside stay, and an
/// object removed keeps what was set aside inside it. An error means the text is not a
/// settings document, so it is left alone.
pub fn edit(text: &str, old: &StudioSettings, new: &StudioSettings) -> Result<String, String> {
    let (reading, aside) = read_setting_aside(text)?;
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
    let held = Held {
        read: as_value(&reading.settings),
        aside,
        tab,
    };
    for (path, value) in &changes {
        held.write(&mut text, path, value.as_ref());
    }
    // The edit must leave a document that still reads; otherwise the file stays as it was.
    let edited = read(&text)
        .map_err(|error| format!("the change would break the file ({error})"))?
        .settings;
    // And one that reads as changed: a key the file sets twice is edited where it first
    // appears, while the last one applies, so the change would be lost without a word.
    let (edited, default) = (as_value(&edited), as_value(&StudioSettings::default()));
    for (path, value) in &changes {
        let read = value_at(&edited, path);
        let changed = match value {
            Some(value) => read == Some(value),
            // Gone, or holding only what was set aside, which reads as nothing.
            None => {
                read.is_none()
                    || read == value_at(&default, path)
                    || read.is_some_and(|read| {
                        read.as_object().is_some_and(Map::is_empty)
                            || read.as_array().is_some_and(Vec::is_empty)
                    })
            }
        };
        if !changed {
            let at = display(&path.iter().cloned().map(Step::Key).collect::<Vec<_>>());
            return Err(format!(
                "{at} is set more than once, and only the last one applies; remove the others"
            ));
        }
    }
    Ok(text)
}

/// The settings as a `Value`. Serializing them cannot fail: every map key is a string.
fn as_value(settings: &StudioSettings) -> Value {
    serde_json::to_value(settings).unwrap_or_default()
}

/// The value at `path` of keys.
fn value_at<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(value, |at, key| at.get(key))
}

/// What the text held before the edit: the settings it read as, and where the values it set
/// aside are.
struct Held {
    read: Value,
    aside: Vec<Vec<Step>>,
    tab: usize,
}

impl Held {
    /// Whether the value at `path` is itself set aside.
    fn set_aside(&self, path: &[String]) -> bool {
        self.aside
            .iter()
            .any(|aside| aside.len() == path.len() && under(aside, path))
    }

    /// Whether something set aside is inside the value at `path`.
    fn holds_aside(&self, path: &[String]) -> bool {
        self.aside
            .iter()
            .any(|aside| aside.len() > path.len() && under(aside, path))
    }

    /// The positions of the elements set aside in the list at `path`.
    fn aside_in(&self, path: &[String]) -> Vec<usize> {
        self.aside
            .iter()
            .filter_map(|aside| match aside.split_last() {
                Some((Step::Index(index), list))
                    if list.len() == path.len() && under(list, path) =>
                {
                    Some(*index)
                }
                _ => None,
            })
            .collect()
    }

    /// Writes `value` at `path`, or removes the setting there when `None`. A list the text has
    /// is edited entry by entry. Any other value is written whole, unless it holds something set
    /// aside, around which it is written part by part. A value set aside is the person's until
    /// its own setting changes, which replaces it: a removal around it leaves it.
    fn write(&self, text: &mut String, path: &[String], value: Option<&Value>) {
        let range = settings::find_value_range_in_json_text(text, path);
        let written: Option<Value> = range
            .clone()
            .and_then(|range| settings::parse_json_with_comments(&text[range]).ok());
        let around = self.holds_aside(path);
        match (value, written) {
            (Some(value), _) if self.set_aside(path) => {
                replace_in_text(text, path, Some(value), self.tab)
            }
            (None, _) if self.set_aside(path) => {}
            (Some(Value::Array(items)), Some(Value::Array(written))) => {
                self.edit_list(text, path, items, written.len())
            }
            (Some(Value::Object(object)), Some(Value::Object(written))) if around => {
                let mut path = path.to_vec();
                for (key, value) in object {
                    path.push(key.clone());
                    if value_at(&self.read, &path) != Some(value) {
                        self.write(text, &path, Some(value));
                    }
                    path.pop();
                }
                for key in written.keys().filter(|key| !object.contains_key(*key)) {
                    path.push(key.clone());
                    self.write(text, &path, None);
                    path.pop();
                }
            }
            (None, Some(Value::Array(written))) if around => {
                let aside = self.aside_in(path);
                for index in (0..written.len()).rev().filter(|i| !aside.contains(i)) {
                    replace_in_text(text, &element(path, index), None, 0);
                }
            }
            (None, Some(Value::Object(written))) if around => {
                let mut path = path.to_vec();
                for key in written.keys() {
                    path.push(key.clone());
                    self.write(text, &path, None);
                    path.pop();
                }
            }
            (None, _) => remove_in_text(text, path),
            (Some(value), _) => {
                if range.is_some() || !add_in_text(text, path, value, self.tab) {
                    replace_in_text(text, path, Some(value), self.tab);
                }
            }
        }
    }

    /// Edits the list at `path`, `written` entries long in the text, to read as `items`: each
    /// entry that changed is written in its place, each one gone is removed, and new ones go at
    /// the end, so the entries set aside and the comments between entries stay. Zed's editor
    /// inserts only at a list's end: entries added before unchanged ones write those again,
    /// each in its place.
    fn edit_list(&self, text: &mut String, path: &[String], items: &[Value], written: usize) {
        let aside = self.aside_in(path);
        let slots: Vec<usize> = (0..written).filter(|i| !aside.contains(i)).collect();
        let held: &[Value] = value_at(&self.read, path)
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice);
        // The entries the text holds, unchanged at the start and the end. Without the text's
        // own reading of each entry, every one is written again in its place.
        let (start, mut end) = if held.len() == slots.len() {
            let start = held.iter().zip(items).take_while(|(a, b)| a == b).count();
            let most = held.len().min(items.len()) - start;
            let end = held[start..]
                .iter()
                .rev()
                .zip(items[start..].iter().rev())
                .take(most)
                .take_while(|(a, b)| a == b)
                .count();
            (start, end)
        } else {
            (0, 0)
        };
        if items.len() > slots.len() {
            end = 0;
        }
        let old = &slots[start..slots.len() - end];
        let new = &items[start..items.len() - end];
        for (slot, item) in old.iter().zip(new) {
            replace_in_text(text, &element(path, *slot), Some(item), self.tab);
        }
        for slot in old.iter().skip(new.len()).rev() {
            replace_in_text(text, &element(path, *slot), None, 0);
        }
        for item in new.iter().skip(old.len()) {
            append_in_text(text, path, item, self.tab);
        }
    }
}

/// Appends `value` to the list at `path`. Zed's editor is given the list's own text, whose
/// entries keep their columns: reached through the document's keys, it indents a nested
/// list's new entry twice, by the last entry's column and again by the list's key's. It
/// writes the closing bracket as a top-level list's, at the line's start, so the bracket's
/// own indent is put back. A list with no entry is written whole.
fn append_in_text(text: &mut String, path: &[String], value: &Value, tab: usize) {
    let Some(range) = settings::find_value_range_in_json_text(text, path) else {
        return;
    };
    let list = &text[range.clone()];
    let empty = settings::parse_json_with_comments::<Value>(list)
        .ok()
        .and_then(|list| list.as_array().map(Vec::is_empty))
        .unwrap_or(true);
    if empty {
        replace_in_text(text, path, Some(&Value::Array(vec![value.clone()])), tab);
        return;
    }
    let before_bracket = &list[..list.len().saturating_sub(1)];
    let bracket_indent = before_bracket
        .rfind('\n')
        .map(|at| &before_bracket[at + 1..])
        .filter(|gap| gap.trim().is_empty())
        .unwrap_or_default();
    let (at, mut entry) = settings::append_top_level_array_value_in_json_text(list, value, tab);
    if entry.ends_with('\n') {
        entry.push_str(bracket_indent);
    }
    text.replace_range(range.start + at.start..range.start + at.end, &entry);
}

/// Whether `steps` start with the keys of `path`.
fn under(steps: &[Step], path: &[String]) -> bool {
    steps.len() >= path.len()
        && steps
            .iter()
            .zip(path)
            .all(|(step, key)| matches!(step, Step::Key(own) if own == key))
}

/// The path of the list entry at `index`, as Zed's editor names one (`#index`).
fn element(path: &[String], index: usize) -> Vec<String> {
    let mut path = path.to_vec();
    path.push(format!("#{index}"));
    path
}

/// Writes `value` at `path` with Zed's editor, or removes what is there when `None`.
fn replace_in_text(text: &mut String, path: &[String], value: Option<&Value>, tab: usize) {
    let (range, replacement) = settings::replace_value_in_json_text(text, path, tab, value, None);
    text.replace_range(range, &replacement);
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
  "studio": { "tree_page_size": 20, "canvas_max_nodes": -1, "corner_radius": 50 },
  "agent": { "command": ["claude", 3] },
  "graphs": { "dev/knowledge": { "tree_page_size": 100000, "canvas_max_nodes": 1000 } }
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
        let graph = &settings.graphs["dev/knowledge"];
        assert_eq!(
            graph.tree_page_size, None,
            "a graph's own value out of range"
        );
        assert_eq!(graph.canvas_max_nodes, Some(1000));
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
            "`graphs.\"dev/knowledge\".tree_page_size` (line 8) was not applied: `100000` is out \
             of range, expected 10 to 500.",
            "`studio.corner_radius` (line 6) was not applied: `50` is out of range, expected 0 \
             to 20.",
        ] {
            assert!(problems.contains(expected), "{expected}\nin\n{problems}");
        }
        assert_eq!(reading.problems.len(), 7, "{problems}");
        assert_eq!(settings.studio.corner_radius, defaults.studio.corner_radius);
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

        // A view that is not an object at all is set aside alone too, and a save keeps the rest.
        let text = r#"{"graphs": {"dev/knowledge": {
            "views": [null, {"name": "Kept", "type": "Person"}]
        }}}"#;
        let names = |text: &str| -> Vec<String> {
            read(text).unwrap().settings.graphs["dev/knowledge"]
                .views
                .iter()
                .map(|v| v.name.clone())
                .collect()
        };
        assert_eq!(names(text), ["Kept"]);
        let reading = read(text).unwrap();
        assert_eq!(reading.problems.len(), 1, "{:?}", reading.problems);
        assert!(
            reading.problems[0].starts_with("`graphs.\"dev/knowledge\".views[0]` (line 2)"),
            "{}",
            reading.problems[0]
        );
        let view = SavedView {
            name: "New".into(),
            type_name: "Person".into(),
            ..Default::default()
        };
        let out = edited(text, |s| s.save_view("dev/knowledge", view));
        assert_eq!(names(&out), ["Kept", "New"], "{out}");
        assert!(out.contains("null"), "the view set aside stays: {out}");
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

    /// A list is edited entry by entry: saving, changing and deleting views leaves the view set
    /// aside and the comments between entries where they were, and a graph's entry whose
    /// views all go stays while it holds the one set aside.
    #[test]
    fn a_list_keeps_its_comments_and_the_entries_set_aside() {
        let text = r#"{
  "graphs": {
    "dev/knowledge": {
      "views": [
        // Broken by hand
        {"name": "Broken", "type": "Person", "filters": [{"property": "a", "op": 3}]},
        // The one I use
        {"name": "Kept", "type": "Person"}
      ]
    }
  }
}
"#;
        let view = |name: &str, group_by: Option<&str>| SavedView {
            name: name.into(),
            type_name: "Person".into(),
            group_by: group_by.map(Into::into),
            ..Default::default()
        };
        let names = |text: &str| -> Vec<(String, Option<String>)> {
            read(text).unwrap().settings.graphs["dev/knowledge"]
                .views
                .iter()
                .map(|v| (v.name.clone(), v.group_by.clone()))
                .collect()
        };
        let broken =
            r#"{"name": "Broken", "type": "Person", "filters": [{"property": "a", "op": 3}]}"#;

        let out = edited(text, |s| s.save_view("dev/knowledge", view("New", None)));
        for kept in [
            "// Broken by hand",
            broken,
            "// The one I use",
            r#"{"name": "Kept", "type": "Person"}"#,
        ] {
            assert!(out.contains(kept), "{kept} in\n{out}");
        }
        assert_eq!(
            names(&out),
            [("Kept".into(), None), ("New".into(), None)],
            "{out}"
        );
        assert!(
            out.contains("\n        {\n          \"name\": \"New\",")
                && out.contains("\n      ]\n"),
            "the new view at the list's indent: {out}"
        );

        let out = edited(&out, |s| {
            s.save_view("dev/knowledge", view("Kept", Some("title")))
        });
        assert!(
            out.contains(broken) && out.contains("// The one I use"),
            "{out}"
        );
        assert_eq!(
            names(&out),
            [("Kept".into(), Some("title".into())), ("New".into(), None)],
            "{out}"
        );

        let out = edited(&out, |s| {
            s.delete_view("dev/knowledge", "Person", "Kept");
            s.delete_view("dev/knowledge", "Person", "New");
        });
        assert!(out.contains(broken), "{out}");
        assert!(
            !out.contains("\"Kept\"") && !out.contains("\"New\""),
            "{out}"
        );
        let reading = read(&out).unwrap();
        assert!(reading.settings.graphs["dev/knowledge"].views.is_empty());
        assert_eq!(reading.problems.len(), 1, "the view set aside, still named");
    }

    /// Any list is edited entry by entry: a comment between its entries stays when an entry is
    /// added, changed or removed.
    #[test]
    fn a_list_edit_keeps_the_comments_between_its_entries() {
        let text = "{\n  \"agent\": {\n    \"command\": [\n      \"claude\", // the program\n      \"--acp\"\n    ]\n  }\n}\n";
        let command = |text: &str| read(text).unwrap().settings.agent.command;
        let out = edited(text, |s| s.agent.command.push("--verbose".into()));
        assert!(out.contains("\"claude\", // the program"), "{out}");
        assert!(
            out.contains("      \"--acp\",\n      \"--verbose\"\n    ]\n"),
            "an entry added at the list's own indent: {out}"
        );
        assert_eq!(command(&out), ["claude", "--acp", "--verbose"], "{out}");
        let out = edited(&out, |s| s.agent.command[1] = "--stdio".into());
        assert!(out.contains("\"claude\", // the program"), "{out}");
        assert_eq!(command(&out), ["claude", "--stdio", "--verbose"], "{out}");
        let out = edited(&out, |s| {
            s.agent.command.pop();
        });
        assert!(out.contains("\"claude\", // the program"), "{out}");
        assert_eq!(command(&out), ["claude", "--stdio"], "{out}");
    }

    /// Entries set aside before one that fits are counted where the text has them: each is
    /// named by its own place, and a change keeps them both.
    #[test]
    fn entries_set_aside_are_named_by_their_place_in_the_text() {
        let text = r#"{"graphs": {"g": {"views": [
            {"name": "A", "type": "Person", "filters": [{"op": 1}]},
            {"name": "B", "type": "Person", "filters": [{"op": 2}]},
            {"name": "C", "type": "Person"}
        ]}}}"#;
        let problems = read(text).unwrap().problems;
        assert!(
            problems[0].starts_with("`graphs.g.views[0]`"),
            "{problems:?}"
        );
        assert!(
            problems[1].starts_with("`graphs.g.views[1]`"),
            "{problems:?}"
        );
        let out = edited(text, |s| {
            s.save_view(
                "g",
                SavedView {
                    name: "C".into(),
                    type_name: "Person".into(),
                    hidden: vec!["email".into()],
                    ..Default::default()
                },
            )
        });
        assert!(
            out.contains("\"op\": 1") && out.contains("\"op\": 2"),
            "{out}"
        );
        let views = read(&out).unwrap().settings.graphs["g"].views.clone();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].hidden, ["email"]);
    }

    #[test]
    fn broken_text_is_not_edited() {
        let old = StudioSettings::default();
        let mut new = old.clone();
        new.ui_font_size = 13.;
        assert!(edit("{ \"ui_font_size\": ", &old, &new).is_err());
    }
}
