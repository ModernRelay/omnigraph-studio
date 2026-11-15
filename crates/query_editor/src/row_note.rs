//! A result's row as the inspector shows it (spec 020) while one of its values is selected:
//! every column of the row, one under another, each value whole. A wide result is then read a
//! row at a time without scrolling sideways, and a long text is not cut at its column. A cell
//! that holds a node shows that node instead (`Picked::Node` in the tab).

use gpui::{ClickEvent, Context, IntoElement, Render, SharedString, Window, div, px};
use serde_json::Value;
use studio_actions::graph::OpenNodePage;
use studio_ui::ds::{self, Tipped as _, size, space};
use ui::prelude::*;

use crate::results::ResultTable;

/// A value this short, on one line, sits in a value box as a property's does; a longer one,
/// or one with line breaks, is read as text under its column's name.
const BOXED: usize = 36;

/// A long text is shown to this many characters: the note is for reading a row, and a text of
/// megabytes would be laid out whole.
const TEXT_SHOWN: usize = 4000;

pub(crate) struct RowNote {
    /// The row's place among the rows read, from one, and how many were read.
    number: usize,
    of: usize,
    fields: Vec<Field>,
    /// The commit the result's rows are held at, which a node opened from here is held at too.
    at_commit: Option<String>,
}

struct Field {
    name: SharedString,
    icon: IconName,
    value: Shown,
}

/// A cell's value as the note shows it.
#[derive(Debug, PartialEq)]
enum Shown {
    /// Nothing: a null.
    Nothing,
    /// A short value on one line.
    Short(SharedString),
    /// A long text, or one with line breaks, cut at [`TEXT_SHOWN`].
    Text(SharedString),
    /// A list, each item in full.
    List(Vec<SharedString>),
    /// A whole node: a link to its page.
    Node { type_name: String, id: String },
}

impl Shown {
    fn of(value: &Value, node_type: Option<&String>) -> Self {
        if let (Some(type_name), Some(id)) = (node_type, value.get("@id").and_then(Value::as_str)) {
            return Self::Node {
                type_name: type_name.clone(),
                id: id.to_owned(),
            };
        }
        match value {
            Value::Null => Self::Nothing,
            Value::Array(items) => Self::List(
                items
                    .iter()
                    .map(|item| studio_ui::format_value(item).into())
                    .collect(),
            ),
            other => {
                let text = studio_ui::format_value(other);
                if text.chars().count() <= BOXED && !text.contains('\n') {
                    Self::Short(text.into())
                } else {
                    Self::Text(cut(text).into())
                }
            }
        }
    }
}

/// `text` to [`TEXT_SHOWN`] characters, saying how many more there are.
fn cut(text: String) -> String {
    let count = text.chars().count();
    if count <= TEXT_SHOWN {
        return text;
    }
    let shown: String = text.chars().take(TEXT_SHOWN).collect();
    format!(
        "{shown}… ({} more characters)",
        studio_ui::group_digits(count - TEXT_SHOWN)
    )
}

impl RowNote {
    /// The note for row `row` of `table`, or none when the table has no such row.
    pub(crate) fn new(row: usize, table: &ResultTable, at_commit: Option<String>) -> Option<Self> {
        let values = table.rows.get(row)?;
        let fields = table
            .columns
            .iter()
            .zip(values)
            .enumerate()
            .map(|(column, (name, value))| {
                let node_type = table.node_types.get(column).and_then(Option::as_ref);
                let value_shown = Shown::of(value, node_type);
                Field {
                    name: name.clone().into(),
                    icon: match value_shown {
                        Shown::Node { .. } => IconName::ArrowUpRight,
                        _ => studio_ui::value_icon(value),
                    },
                    value: value_shown,
                }
            })
            .collect();
        Some(Self {
            number: row + 1,
            of: table.rows.len(),
            fields,
            at_commit,
        })
    }

    /// What the note says of each column, for a test: its name and its value as text.
    #[cfg(test)]
    pub(crate) fn read(&self) -> Vec<(String, String)> {
        self.fields
            .iter()
            .map(|field| {
                let value = match &field.value {
                    Shown::Nothing => "—".to_owned(),
                    Shown::Short(text) | Shown::Text(text) => text.to_string(),
                    Shown::List(items) => items
                        .iter()
                        .map(|item| item.to_string())
                        .collect::<Vec<_>>()
                        .join(" | "),
                    Shown::Node { type_name, id } => format!("{type_name} {id}"),
                };
                (field.name.to_string(), value)
            })
            .collect()
    }

    /// The row's place, as its heading says it.
    #[cfg(test)]
    pub(crate) fn place(&self) -> (usize, usize) {
        (self.number, self.of)
    }
}

impl Render for RowNote {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.fields.iter().enumerate().map(|(at, field)| {
            let row = ds::property_row(field.icon, field.name.clone(), size::FIELD_LABEL);
            let value = ds::property_value();
            match &field.value {
                Shown::Nothing => row.child(value.child(ds::text::muted("—"))),
                Shown::Short(text) => row.child(value.child(ds::value_text(text.clone(), cx))),
                // A long text reads as text, wrapping in the value's column.
                Shown::Text(text) => row.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .py(px(space::SM))
                        .child(ds::text::body(text.clone())),
                ),
                Shown::List(items) if items.is_empty() => {
                    row.child(value.child(ds::text::muted("—")))
                }
                Shown::List(items) => row.child(
                    value.child(
                        ds::value_list()
                            .children(items.iter().map(|item| ds::value_text(item.clone(), cx))),
                    ),
                ),
                // A link in the inspector opens in the center (spec 020): the node's page, as
                // a preview tab, which a second click keeps.
                Shown::Node { type_name, id } => {
                    let open = {
                        let (type_name, id) = (type_name.clone(), id.clone());
                        let at_commit = self.at_commit.clone();
                        move |event: &ClickEvent, window: &mut Window, cx: &mut gpui::App| {
                            window.dispatch_action(
                                Box::new(OpenNodePage {
                                    type_name: type_name.clone(),
                                    id: id.clone(),
                                    at_commit: at_commit.clone(),
                                    preview: event.click_count() < 2,
                                }),
                                cx,
                            )
                        }
                    };
                    row.child(
                        value.child(
                            div()
                                .id(("row-note-node", at))
                                .min_w_0()
                                .cursor_pointer()
                                .tip(ds::tooltip(format!(
                                    "{type_name} {id} · click to open its page"
                                )))
                                .on_click(open)
                                .child(ds::node_value(type_name, id.clone(), cx)),
                        ),
                    )
                }
            }
        });
        v_flex()
            .px(px(space::LG))
            .pt(px(space::XL))
            .pb(px(space::LG))
            .gap(px(space::MD))
            .child(
                h_flex()
                    .gap(px(space::SM))
                    .items_center()
                    .child(ds::text::heading(format!(
                        "Row {}",
                        studio_ui::group_digits(self.number)
                    )))
                    .child(ds::text::meta(format!(
                        "of {}",
                        studio_ui::group_digits(self.of)
                    ))),
            )
            .child(ds::fields().children(rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A row reads column by column: a null as nothing, a short value boxed, a long text or
    /// one with line breaks as text, cut where it runs past what is shown, a list item by
    /// item, and a whole node as its type and id.
    #[test]
    fn a_row_reads_each_column_whole() {
        let long = "x".repeat(TEXT_SHOWN + 1234);
        let table = ResultTable {
            columns: ["who", "age", "bio", "note", "tags", "none"]
                .map(str::to_owned)
                .to_vec(),
            types: vec![None; 6],
            node_types: vec![Some("Person".into()), None, None, None, None, None],
            rows: vec![
                vec![json!(null); 6],
                vec![
                    json!({"@id": "p1", "name": "Ada"}),
                    json!(36),
                    json!(long),
                    json!("two\nlines"),
                    json!(["a", "b", "c", "d"]),
                    json!(null),
                ],
            ],
        };
        let note = RowNote::new(1, &table, None).expect("the second row");
        assert_eq!(note.place(), (2, 2));
        let kinds: Vec<&Shown> = note.fields.iter().map(|field| &field.value).collect();
        assert_eq!(
            kinds[0],
            &Shown::Node {
                type_name: "Person".into(),
                id: "p1".into()
            }
        );
        assert_eq!(kinds[1], &Shown::Short("36".into()));
        let Shown::Text(bio) = kinds[2] else {
            panic!("a long text: {:?}", kinds[2]);
        };
        assert!(bio.ends_with("… (1,234 more characters)"), "cut, and said");
        assert_eq!(kinds[3], &Shown::Text("two\nlines".into()));
        assert_eq!(
            kinds[4],
            &Shown::List(vec!["a".into(), "b".into(), "c".into(), "d".into()]),
            "every item, where the grid shows three"
        );
        assert_eq!(kinds[5], &Shown::Nothing);
        assert!(RowNote::new(2, &table, None).is_none(), "no such row");
    }
}
