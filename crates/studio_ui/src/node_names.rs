//! The choice of the property that names a type's nodes (spec 002, "Node names"), as a menu:
//! a canvas chip's "Show as", a table's "Name rows by" and the settings tab's dropdown are one.

use studio_settings::TypeNaming;
use ui::prelude::*;

use crate::menu::ContextMenu;

/// What the choice reads when none is made: the automatic property, named.
pub fn automatic(names: &TypeNaming) -> String {
    format!("Automatic · {}", names.default)
}

/// What names the nodes now, as the choice reads it.
pub fn shown(names: &TypeNaming) -> String {
    match &names.chosen {
        Some(chosen) => chosen.clone(),
        None => automatic(names),
    }
}

/// "Automatic", then each property that can name the nodes; the one in effect is checked, and
/// picking one names the type's nodes by it across the graph.
pub fn add_to(mut menu: ContextMenu, names: &TypeNaming) -> ContextMenu {
    let choices = std::iter::once((automatic(names), None)).chain(
        names
            .candidates
            .iter()
            .map(|property| (property.clone(), Some(property.clone()))),
    );
    for (label, choice) in choices {
        let checked = choice == names.chosen;
        let names = names.clone();
        menu = menu.toggleable_entry(label, checked, IconPosition::Start, None, move |_, cx| {
            names.choose(choice.clone(), cx)
        });
    }
    menu
}
