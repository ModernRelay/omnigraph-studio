//! One node property written from its page, its peek or its text editor (spec 014): bound to
//! the value it was read at, as a table cell saves (spec 004), so it changes nothing if the
//! value changed elsewhere meanwhile.

use gpui::{App, Entity, Task};
use gq_language::{SchemaModel, builder};
use graph_store::GraphSession;
use serde_json::Value;

use crate::model::{SaveResult, save_result};

/// Sends the write to `branch`, the one the value was read on, and resolves with how it ended;
/// `Err` when no write can be built.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_property(
    session: &Entity<GraphSession>,
    branch: &str,
    schema: &SchemaModel,
    type_name: &str,
    id: &str,
    property: &str,
    old: &Value,
    new: &Value,
    cx: &mut App,
) -> Result<Task<SaveResult>, String> {
    let query = builder::update_cell(schema, type_name, property, id, old, new)
        .map_err(|error| error.to_string())?;
    let description = format!("Set {property} of {id}");
    let write = session.update(cx, |session, cx| {
        session.write_on(branch, &query, None, description, None, cx)
    });
    let branch = branch.to_owned();
    Ok(cx.spawn(async move |_| save_result(&write.await, &branch)))
}
