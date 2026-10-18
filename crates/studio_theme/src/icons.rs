//! Studio's icons: Nucleo's `ui` family, outline-duo, drawn at 18 px — the set Tower uses, so
//! the two apps share one vocabulary of drawings (spec 002, "Icons").
//!
//! Studio names icons with Zed's `IconName`, and Zed loads each from `icons/<name>.svg`.
//! [`NUCLEO`] maps those paths to Nucleo drawings, and `Assets` serves the drawing in place of
//! Zed's, so every view, and every Zed component Studio draws (disclosures, checkboxes, menus),
//! changes at once. An icon not listed keeps Zed's drawing: the brand marks, and the modifier
//! glyphs of key bindings, which are type, not pictures.
//!
//! Nucleo is a licensed set: an open-source project may carry at most 100 of its icons, with
//! its notice (`assets/icons/nucleo/NOTICE`). Only the drawings listed here are in the
//! repository; `script/vendor-icons.py` copies them from the licensed bundle, and a test holds
//! the count under the limit and every file to a row here. Change an icon by changing its row
//! and re-running the script.

/// One icon: Zed's path, the Nucleo drawing that replaces it, and that drawing's bytes.
pub(crate) struct Icon {
    pub zed: &'static str,
    /// Read by the licence test, which counts the drawings.
    #[cfg_attr(not(test), allow(dead_code))]
    pub nucleo: &'static str,
    pub svg: &'static [u8],
}

macro_rules! nucleo {
    ($(($zed:literal, $nucleo:literal)),* $(,)?) => {
        &[$(Icon {
            zed: concat!("icons/", $zed, ".svg"),
            nucleo: $nucleo,
            svg: include_bytes!(concat!("../../../assets/icons/nucleo/", $nucleo, ".svg")),
        }),*]
    };
}

/// Zed's icon (by its file stem) → the Nucleo drawing Studio shows for it.
pub(crate) const NUCLEO: &[Icon] = nucleo![
    ("archive", "archive"),
    ("arrow_circle", "loader"),
    ("arrow_down", "arrow-down"),
    ("arrow_left", "arrow-left"),
    ("arrow_right", "arrow-right"),
    ("arrow_up", "arrow-up"),
    ("arrow_up_right", "arrow-up-right"),
    ("blocks", "cubes"),
    ("bolt_outlined", "bolt"),
    ("case_sensitive", "text-title-case"),
    ("check", "check"),
    ("check_double", "check-double"),
    ("chevron_down", "chevron-down"),
    ("chevron_left", "chevron-left"),
    ("chevron_right", "chevron-right"),
    ("chevron_up", "chevron-up"),
    ("chevron_up_down", "caret-expand-y"),
    ("clock", "clock"),
    ("close", "xmark"),
    ("code", "code"),
    ("copy", "copy"),
    ("crosshair", "crosshairs"),
    ("dash", "minus"),
    ("database_zap", "database"),
    ("ellipsis", "dots"),
    ("eye", "eye"),
    ("eye_off", "eye-slash"),
    ("file", "file"),
    ("file_code", "code"),
    ("file_doc", "file-content"),
    ("file_generic", "file"),
    ("file_text_outlined", "file-content"),
    ("filter", "filter"),
    ("folder", "folder"),
    ("folder_open", "folder-open"),
    ("git_branch", "code-branch"),
    ("git_commit", "code-commit"),
    ("git_graph", "nodes"),
    ("git_merge_conflict", "code-merge"),
    ("hash", "hashtag"),
    ("history_rerun", "history"),
    ("image", "image"),
    ("info", "circle-info"),
    ("keyboard", "keyboard"),
    ("link", "link"),
    ("list_collapse", "arrows-to-line-y"),
    ("list_todo", "list-todo"),
    ("list_tree", "list-tree"),
    ("load_circle", "loader"),
    ("magnifying_glass", "magnifier"),
    ("maximize", "expand"),
    ("minimize", "reduce"),
    ("pencil", "pencil"),
    ("person", "user"),
    ("pin", "pin-tack"),
    ("play_filled", "media-play"),
    ("plus", "plus"),
    ("pull_request", "code-pull-request"),
    ("reader", "book-open"),
    ("rotate_ccw", "refresh-anticlockwise"),
    ("rotate_cw", "refresh-clockwise"),
    ("send", "paper-plane"),
    ("server", "server"),
    ("settings", "gear"),
    ("sparkle", "sparkle"),
    ("split", "split-view"),
    ("star", "star"),
    ("stop", "media-stop"),
    ("table", "table"),
    ("thinking_mode", "brain"),
    ("threads_sidebar_left_closed", "sidebar-left"),
    ("threads_sidebar_left_open", "sidebar-left"),
    ("trash", "trash"),
    ("undo", "undo"),
    ("unpin", "pin-tack-slash"),
    ("user_check", "user-check"),
    ("warning", "triangle-warning"),
    ("x_circle", "circle-xmark"),
    ("zed_assistant", "sparkle"),
];

/// The Nucleo drawing for one of Zed's icon paths, if Studio replaces it.
pub(crate) fn nucleo(path: &str) -> Option<&'static [u8]> {
    NUCLEO
        .iter()
        .find(|icon| icon.zed == path)
        .map(|icon| icon.svg)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use gpui::AssetSource as _;

    use super::*;

    /// Nucleo's licence lets an open-source project carry at most 100 of its icons, with its
    /// notice; every drawing in the repository is one a row names, so none is carried unused.
    #[test]
    fn the_nucleo_drawings_stay_within_the_licence() {
        let named: BTreeSet<&str> = NUCLEO.iter().map(|icon| icon.nucleo).collect();
        assert!(named.len() <= 100, "{} Nucleo icons", named.len());

        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/icons/nucleo");
        let mut on_disk = BTreeSet::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            match name.strip_suffix(".svg") {
                Some(stem) => {
                    on_disk.insert(stem.to_owned());
                }
                None => assert_eq!(name, "NOTICE", "only drawings and the notice"),
            }
        }
        let named: BTreeSet<String> = named.into_iter().map(str::to_owned).collect();
        assert_eq!(on_disk, named);
        let notice = std::fs::read_to_string(format!("{dir}/NOTICE")).unwrap();
        assert!(notice.contains("Copyright © Nucleo"));
    }

    /// Each row replaces an icon Zed has, once, with an 18 px drawing.
    #[test]
    fn every_row_replaces_one_of_zeds_icons() {
        let mut seen = BTreeSet::new();
        for icon in NUCLEO {
            assert!(seen.insert(icon.zed), "{} listed twice", icon.zed);
            assert!(
                assets::Assets.load(icon.zed).unwrap().is_some(),
                "Zed has no {}",
                icon.zed
            );
            let svg = std::str::from_utf8(icon.svg).unwrap();
            assert!(svg.contains(r#"viewBox="0 0 18 18""#), "{}", icon.nucleo);
        }
    }

    /// Studio's assets serve the Nucleo drawing for a replaced icon and Zed's for the rest.
    #[test]
    fn assets_serve_the_replacement() {
        let search = crate::Assets
            .load("icons/magnifying_glass.svg")
            .unwrap()
            .unwrap();
        assert_eq!(
            search.as_ref(),
            nucleo("icons/magnifying_glass.svg").unwrap()
        );
        let zeds = assets::Assets
            .load("icons/ai_anthropic.svg")
            .unwrap()
            .unwrap();
        let served = crate::Assets
            .load("icons/ai_anthropic.svg")
            .unwrap()
            .unwrap();
        assert_eq!(served, zeds);
    }
}
