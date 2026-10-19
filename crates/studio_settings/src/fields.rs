//! The settings tab's rows, generated from the settings document's JSON schema (spec 002):
//! one [`SettingField`] per leaf of [`StudioSettings`], in declaration order. A field's doc
//! comment is its description; `schemars` attributes give its title, range, and these
//! extensions:
//!
//! - `x-section`: the left-nav section; a field without one takes its parent's.
//! - `x-control`: a control whose options come from the running app (`theme`, `font`, `agent`).
//! - `x-step`: the stepper increment of a number.
//! - `x-hidden`: edited in `settings.json` only.
//! - `x-per-graph`: a graph may override it under `graphs` ([`Scope::Graph`]).
//!
//! Values are read and written by path through the document's JSON form, so a new setting
//! needs only its field and attributes.

use std::sync::LazyLock;

use serde_json::{Map, Value};

use crate::StudioSettings;

#[derive(Debug, Clone, PartialEq)]
pub struct SettingField {
    /// The keys from the document's root, e.g. `["studio", "tree_page_size"]`.
    pub path: Vec<String>,
    pub section: String,
    pub title: String,
    pub description: String,
    pub kind: FieldKind,
    /// Whether a graph can have its own value (`x-per-graph`).
    pub per_graph: bool,
}

/// Where the settings tab reads and writes: the shared document, or one graph's overrides
/// under `graphs` (the key from [`crate::graph_key`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    All,
    Graph(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    /// One of a fixed set of strings.
    Choice(Vec<Choice>),
    Number {
        min: f64,
        max: f64,
        step: f64,
        integer: bool,
    },
    Toggle,
    /// Free text; `nullable` when empty means "unset".
    Text {
        nullable: bool,
    },
    /// Options the app supplies at run time (`x-control`).
    Custom(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

static FIELDS: LazyLock<Vec<SettingField>> = LazyLock::new(|| {
    let schema = serde_json::to_value(schemars::schema_for!(StudioSettings))
        .expect("the settings schema is JSON");
    let mut fields = Vec::new();
    walk(&schema, &schema, &mut Vec::new(), "General", &mut fields);
    fields
});

/// Every visible setting, in the order the document declares them.
pub fn fields() -> &'static [SettingField] {
    &FIELDS
}

/// The sections, in the order their first setting appears.
pub fn sections() -> Vec<&'static str> {
    let mut sections: Vec<&'static str> = Vec::new();
    for field in fields() {
        if !sections.contains(&field.section.as_str()) {
            sections.push(&field.section);
        }
    }
    sections
}

fn walk(
    root: &Value,
    schema: &Value,
    path: &mut Vec<String>,
    section: &str,
    out: &mut Vec<SettingField>,
) {
    let resolved = resolve(root, schema);
    let section = text(schema, "x-section")
        .or_else(|| text(&resolved, "x-section"))
        .unwrap_or(section)
        .to_owned();
    if schema.get("x-hidden") == Some(&Value::Bool(true)) {
        return;
    }
    let control = text(schema, "x-control").map(str::to_owned);
    if control.is_none()
        && let Some(properties) = resolved.get("properties").and_then(Value::as_object)
    {
        for (key, property) in properties {
            path.push(key.clone());
            walk(root, property, path, &section, out);
            path.pop();
        }
        return;
    }
    let Some(kind) = control
        .map(FieldKind::Custom)
        .or_else(|| kind(&resolved, schema))
    else {
        return;
    };
    let key = path.last().cloned().unwrap_or_default();
    out.push(SettingField {
        path: path.clone(),
        section,
        title: text(schema, "title").map_or_else(|| sentence(&key), str::to_owned),
        description: paragraphs(text(schema, "description").unwrap_or_default()),
        kind,
        per_graph: schema.get("x-per-graph") == Some(&Value::Bool(true)),
    });
}

/// A doc comment as the tab shows it: the lines of a paragraph joined by spaces, paragraphs
/// kept apart.
fn paragraphs(doc: &str) -> String {
    doc.split("\n\n")
        .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `schema` with a `$ref` (direct or as the one `allOf` member) replaced by its definition.
fn resolve(root: &Value, schema: &Value) -> Value {
    let reference = schema.get("$ref").and_then(Value::as_str).or_else(|| {
        schema
            .get("allOf")
            .and_then(Value::as_array)
            .filter(|all| all.len() == 1)
            .and_then(|all| all[0].get("$ref"))
            .and_then(Value::as_str)
    });
    reference
        .and_then(|r| r.strip_prefix("#/"))
        .and_then(|pointer| root.pointer(&format!("/{pointer}")))
        .cloned()
        .unwrap_or_else(|| schema.clone())
}

fn kind(resolved: &Value, field: &Value) -> Option<FieldKind> {
    if let Some(values) = resolved.get("enum").and_then(Value::as_array) {
        let choices = values
            .iter()
            .filter_map(Value::as_str)
            .map(|value| Choice {
                value: value.to_owned(),
                label: sentence(value),
                description: None,
            })
            .collect();
        return Some(FieldKind::Choice(choices));
    }
    if let Some(variants) = resolved.get("oneOf").and_then(Value::as_array) {
        let choices: Option<Vec<Choice>> = variants
            .iter()
            .map(|variant| {
                let value = variant.get("const").and_then(Value::as_str)?;
                Some(Choice {
                    value: value.to_owned(),
                    label: sentence(value),
                    description: text(variant, "description").map(str::to_owned),
                })
            })
            .collect();
        return choices.map(FieldKind::Choice);
    }
    let types: Vec<&str> = match resolved.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let nullable = types.contains(&"null");
    let number = |key: &str| {
        field
            .get(key)
            .or_else(|| resolved.get(key))
            .and_then(Value::as_f64)
    };
    if types.contains(&"integer") || types.contains(&"number") {
        let integer = types.contains(&"integer");
        return Some(FieldKind::Number {
            min: number("minimum").unwrap_or(if integer { 0. } else { f64::MIN }),
            max: number("maximum").unwrap_or(f64::MAX),
            step: number("x-step").unwrap_or(1.),
            integer,
        });
    }
    if types.contains(&"boolean") {
        return Some(FieldKind::Toggle);
    }
    if types.contains(&"string") {
        return Some(FieldKind::Text { nullable });
    }
    None
}

/// Where a graph's override of `field` lives: `graphs.<key>.<last key>`.
fn override_path(scope: &Scope, field: &SettingField) -> Option<Vec<String>> {
    match scope {
        Scope::Graph(key) if field.per_graph => Some(vec![
            "graphs".into(),
            key.clone(),
            field.path.last()?.clone(),
        ]),
        _ => None,
    }
}

fn text<'a>(schema: &'a Value, key: &str) -> Option<&'a str> {
    schema
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// `type_table_page_size` → "Type table page size"; `light` → "Light".
fn sentence(key: &str) -> String {
    let words = key.replace('_', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The settings in their JSON form, serialized once: for reading many settings at a time, as
/// the settings tab does for every row it draws.
pub struct SettingsValues(Value);

impl SettingsValues {
    /// The value at `path`; `Null` where there is none.
    pub fn at(&self, path: &[String]) -> &Value {
        at(&self.0, path)
    }

    /// Whether the setting at `path` has its default value.
    fn is_default_at(&self, path: &[String]) -> bool {
        self.at(path) == at(defaults(), path)
    }

    /// The value `field` has in `scope`: a graph's override, else the shared value.
    pub fn value_in(&self, scope: &Scope, field: &SettingField) -> &Value {
        match override_path(scope, field).map(|path| self.at(&path)) {
            Some(value) if !value.is_null() => value,
            _ => self.at(&field.path),
        }
    }

    /// Whether `field` is unchanged in `scope`: at its default, or not overridden by the graph.
    pub fn is_default_in(&self, scope: &Scope, field: &SettingField) -> bool {
        match override_path(scope, field) {
            Some(path) => self.at(&path).is_null(),
            None => self.is_default_at(&field.path),
        }
    }
}

fn at<'a>(document: &'a Value, path: &[String]) -> &'a Value {
    static NULL: Value = Value::Null;
    path.iter()
        .try_fold(document, |value, key| value.get(key))
        .unwrap_or(&NULL)
}

/// The default settings in their JSON form, serialized once.
fn defaults() -> &'static Value {
    static DEFAULTS: LazyLock<Value> =
        LazyLock::new(|| serde_json::to_value(StudioSettings::default()).unwrap_or_default());
    &DEFAULTS
}

impl StudioSettings {
    /// The document's values, to read by path.
    pub fn values(&self) -> SettingsValues {
        SettingsValues(serde_json::to_value(self).unwrap_or_default())
    }

    /// The document with `value` at `path`, or why it does not fit there.
    pub fn with_value(&self, path: &[String], value: Value) -> Result<Self, String> {
        let mut document = serde_json::to_value(self).map_err(|e| e.to_string())?;
        let (last, parents) = path.split_last().ok_or("an empty path")?;
        let mut object: &mut Map<String, Value> = document
            .as_object_mut()
            .ok_or("the document is not an object")?;
        for key in parents {
            object = object
                .get_mut(key)
                .and_then(Value::as_object_mut)
                .ok_or_else(|| format!("no section `{key}`"))?;
        }
        object.insert(last.clone(), value);
        serde_json::from_value(document).map_err(|e| e.to_string())
    }

    /// The document with the setting at `path` back at its default.
    fn reset_at(&self, path: &[String]) -> Self {
        self.with_value(path, at(defaults(), path).clone())
            .unwrap_or_else(|_| self.clone())
    }

    /// The document with `value` for `field` in `scope`.
    pub fn with_value_in(
        &self,
        scope: &Scope,
        field: &SettingField,
        value: Value,
    ) -> Result<Self, String> {
        let Some(path) = override_path(scope, field) else {
            return self.with_value(&field.path, value);
        };
        let Scope::Graph(key) = scope else {
            unreachable!("only a graph scope has an override path")
        };
        let mut next = self.clone();
        next.graphs.entry(key.clone()).or_default();
        let mut next = next.with_value(&path, value)?;
        next.graphs.retain(|_, overrides| !overrides.is_empty());
        Ok(next)
    }

    /// The document with `field` reset in `scope`: to its default, or the graph's override
    /// removed.
    pub fn reset_in(&self, scope: &Scope, field: &SettingField) -> Self {
        match override_path(scope, field) {
            Some(_) => self
                .with_value_in(scope, field, Value::Null)
                .unwrap_or_else(|_| self.clone()),
            None => self.reset_at(&field.path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Density, WindowStyle};

    fn field(path: &str) -> &'static SettingField {
        fields()
            .iter()
            .find(|f| f.path.join(".") == path)
            .unwrap_or_else(|| panic!("no field {path}"))
    }

    #[test]
    fn every_setting_is_a_row_in_declaration_order() {
        let paths: Vec<String> = fields().iter().map(|f| f.path.join(".")).collect();
        assert_eq!(
            paths,
            [
                "theme.mode",
                "theme.light",
                "theme.dark",
                "ui_font_family",
                "ui_font_size",
                "buffer_font_family",
                "buffer_font_size",
                "ui_density",
                "studio.window_style",
                "studio.corner_radius",
                "studio.bottom_dock_layout",
                "studio.type_table_page_size",
                "studio.tree_page_size",
                "studio.canvas_max_nodes",
                "studio.health_check_seconds",
                "studio.show_performance",
                "agent.command",
                "agent.ask_before_writes",
                "agent.omnigraph_cli",
            ],
            "agent.env, a map, and panels are edited in settings.json or elsewhere"
        );
        assert_eq!(
            sections(),
            ["Appearance", "Browsing", "Connection", "Agent"]
        );
    }

    #[test]
    fn rows_take_titles_descriptions_and_controls_from_the_schema() {
        let page = field("studio.type_table_page_size");
        assert_eq!(page.title, "Table page size");
        assert_eq!(page.description, "Rows read per page in type tables.");
        assert_eq!(page.section, "Browsing");
        assert_eq!(
            page.kind,
            FieldKind::Number {
                min: 25.,
                max: 1000.,
                step: 25.,
                integer: true
            }
        );
        let mode = field("theme.mode");
        assert_eq!(mode.section, "Appearance", "inherited from `theme`");
        let FieldKind::Choice(choices) = &mode.kind else {
            panic!("theme mode is a choice: {:?}", mode.kind);
        };
        let labels: Vec<_> = choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["System", "Light", "Dark"]);
        let FieldKind::Choice(styles) = &field("studio.window_style").kind else {
            panic!("window style is a choice");
        };
        assert_eq!(styles[1].value, "glass");
        assert!(
            styles[1].description.is_some(),
            "variant docs describe choices"
        );
        assert_eq!(field("theme.dark").kind, FieldKind::Custom("theme".into()));
        assert_eq!(
            field("ui_font_family").kind,
            FieldKind::Custom("font".into())
        );
        assert_eq!(field("agent.command").section, "Agent");
        assert_eq!(field("agent.ask_before_writes").kind, FieldKind::Toggle);
        // A doc comment's lines are one paragraph.
        assert!(!field("agent.ask_before_writes").description.contains('\n'));
        assert_eq!(field("agent.omnigraph_cli").title, "OmniGraph CLI");
        assert_eq!(
            field("agent.omnigraph_cli").kind,
            FieldKind::Text { nullable: true }
        );
    }

    #[test]
    fn values_are_read_written_and_reset_by_path() {
        let path = |p: &str| p.split('.').map(str::to_owned).collect::<Vec<_>>();
        let settings = StudioSettings::default();
        assert_eq!(settings.values().at(&path("studio.tree_page_size")), 50);
        let changed = settings
            .with_value(&path("studio.window_style"), "glass".into())
            .unwrap()
            .with_value(&path("ui_density"), "compact".into())
            .unwrap()
            .with_value(&path("ui_font_family"), "IBM Plex Sans".into())
            .unwrap();
        assert_eq!(changed.studio.window_style, WindowStyle::Glass);
        assert_eq!(changed.ui_density, Density::Compact);
        assert!(!changed.values().is_default_at(&path("ui_font_family")));
        assert!(changed.values().is_default_at(&path("theme.mode")));
        let reset = changed.reset_at(&path("ui_font_family"));
        assert_eq!(reset.ui_font_family, None);
        assert_eq!(reset.ui_density, Density::Compact, "only that setting");
        assert!(
            settings
                .with_value(&path("ui_density"), "roomy".into())
                .is_err()
        );
    }

    #[test]
    fn a_graph_scope_writes_and_clears_its_own_override() {
        let page = field("studio.tree_page_size");
        assert!(page.per_graph);
        assert!(!field("studio.health_check_seconds").per_graph);
        let scope = Scope::Graph("dev/knowledge".into());
        let settings = StudioSettings::default();
        assert_eq!(
            *settings.values().value_in(&scope, page),
            50,
            "the shared value shows through"
        );
        assert!(settings.values().is_default_in(&scope, page));
        let changed = settings.with_value_in(&scope, page, 20.into()).unwrap();
        assert_eq!(*changed.values().value_in(&scope, page), 20);
        assert_eq!(
            changed.studio.tree_page_size, 50,
            "the shared value is untouched"
        );
        assert!(!changed.values().is_default_in(&scope, page));
        assert!(changed.values().is_default_in(&Scope::All, page));
        assert_eq!(changed.for_graph("dev", "knowledge").tree_page_size, 20);
        let cleared = changed.reset_in(&scope, page);
        assert!(
            cleared.graphs.is_empty(),
            "an empty override leaves no entry"
        );
        let health = field("studio.health_check_seconds");
        let shared = settings.with_value_in(&scope, health, 60.into()).unwrap();
        assert_eq!(
            shared.studio.health_check_seconds, 60,
            "not per graph: writes the shared value"
        );
    }
}
