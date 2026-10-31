//! The application menus (spec 002). Every item is an action, so the menu shows the action's
//! binding and the palette offers the same command.

use gpui::{App, Menu, MenuItem, OsAction, SystemMenuType};
use studio_actions::agent::OpenAgent;
use studio_actions::graph::{
    CreateBranch, NewQuery, OpenCanvas, OpenHistory, OpenOntology, OpenQueryFile, OpenSchema,
    ReviewBranch,
};
use studio_actions::query::{Save, SaveAs};
use studio_actions::update::{CheckForUpdates, RestartToUpdate};
use studio_actions::workspace::{
    About, ActivateNextPane, ActivatePreviousPane, CloseActiveItem, CloseAllItems, CloseOtherItems,
    GoBack, GoForward, GoTo, NewWindow, OpenGraph, OpenSettings, Quit, Refresh, RevealLog,
    ShowKeyBindings, SplitDown, SplitRight, SwitchBranch, ToggleActivity, ToggleCommandPalette,
    ToggleInspector, TogglePinTab, ToggleSidebar,
};

pub fn set(cx: &mut App) {
    // Check for Updates, or Restart to Update once an update is ready (spec 017).
    let update = if studio_update::Updater::global(cx).is_some_and(|updater| {
        matches!(
            updater.read(cx).state(),
            studio_update::UpdateState::Ready { .. }
        )
    }) {
        MenuItem::action("Restart to Update", RestartToUpdate)
    } else {
        MenuItem::action("Check for Updates…", CheckForUpdates)
    };
    cx.set_menus([
        Menu::new("Omnigraph Studio").items([
            MenuItem::action("About Omnigraph Studio", About),
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            update,
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Quit Omnigraph Studio", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("Open Graph…", OpenGraph),
            MenuItem::action("New Window", NewWindow),
            MenuItem::separator(),
            MenuItem::action("New Query", NewQuery),
            MenuItem::action("Open Query File…", OpenQueryFile::default()),
            MenuItem::action("Save Query", Save),
            MenuItem::action("Save Query As…", SaveAs),
            MenuItem::separator(),
            MenuItem::action("Close Tab", CloseActiveItem),
            MenuItem::action("Close Other Tabs", CloseOtherItems),
            MenuItem::action("Close All Tabs", CloseAllItems),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", editor::actions::Undo, OsAction::Undo),
            MenuItem::os_action("Redo", editor::actions::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", editor::actions::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", editor::actions::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", editor::actions::Paste, OsAction::Paste),
            MenuItem::os_action(
                "Select All",
                editor::actions::SelectAll,
                OsAction::SelectAll,
            ),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette", ToggleCommandPalette),
            MenuItem::action("Go to Node…", GoTo),
            MenuItem::separator(),
            MenuItem::action("Back", GoBack),
            MenuItem::action("Forward", GoForward),
            MenuItem::separator(),
            MenuItem::action("Sidebar", ToggleSidebar),
            MenuItem::action("Inspector", ToggleInspector),
            MenuItem::action("Bottom Panel", ToggleActivity),
            MenuItem::separator(),
            MenuItem::action("Split Right", SplitRight),
            MenuItem::action("Split Down", SplitDown),
            MenuItem::action("Next Pane", ActivateNextPane),
            MenuItem::action("Previous Pane", ActivatePreviousPane),
            MenuItem::action("Pin Tab", TogglePinTab),
        ]),
        Menu::new("Graph").items([
            MenuItem::action("Refresh", Refresh),
            MenuItem::action("Switch Branch…", SwitchBranch),
            MenuItem::separator(),
            MenuItem::action("New Query", NewQuery),
            MenuItem::action("History", OpenHistory),
            MenuItem::action("Ontology", OpenOntology),
            MenuItem::action("Schema", OpenSchema::default()),
            MenuItem::action("Canvas", OpenCanvas),
            MenuItem::action("Agent", OpenAgent),
            MenuItem::separator(),
            MenuItem::action("Create Branch…", CreateBranch),
            MenuItem::action("Review Branch", ReviewBranch),
        ]),
        Menu::new("Help").items([
            MenuItem::action("Keyboard Shortcuts", ShowKeyBindings),
            MenuItem::action("Reveal Log File", RevealLog),
        ]),
    ]);
}
