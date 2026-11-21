//! Zed's settings, themes and assets, set up for Studio, plus the one graph token: a colour per
//! node or edge type ([`type_color`]).
//!
//! Layer 1 (`docs/architecture.md`). Plan: `specs/002-app-shell/plan.md`.
//!
//! Studio uses Zed's `editor` for every text input (spike S4), and the editor reads Zed's
//! settings store and `theme_settings`. So Studio runs Zed's own bootstrap here: the release
//! channel, the settings store with Zed's defaults, the bundled themes and fonts.

use std::borrow::Cow;
use std::collections::HashMap;

use gpui::{App, AssetSource, Global, Hsla, SharedString};

mod icons;

/// Themes Studio bundles beside Zed's, by asset path (`assets/themes/` in this repository, with
/// their notices in `assets/themes/LICENSES`). Zed's theme loader lists `themes/` and loads each
/// family, so these are registered with Zed's own.
const STUDIO_THEMES: &[(&str, &[u8])] = &[
    (
        "themes/vitesse-refined/vitesse-theme-refined.json",
        include_bytes!("../../../assets/themes/vitesse-refined/vitesse-theme-refined.json"),
    ),
    // Studio Light and Studio Dark, the defaults: Vitesse Refined with interfaces.dev's greys,
    // written by `script/studio-theme.py`.
    (
        "themes/studio/studio-theme.json",
        include_bytes!("../../../assets/themes/studio/studio-theme.json"),
    ),
];

/// Inter, Tower's typeface and Studio's default interface font (`assets/fonts/inter/`, with its
/// OFL licence): the weights Studio draws, static files GPUI loads as they are.
const STUDIO_FONTS: &[&[u8]] = &[
    include_bytes!("../../../assets/fonts/inter/Inter-Regular.ttf"),
    include_bytes!("../../../assets/fonts/inter/Inter-Italic.ttf"),
    include_bytes!("../../../assets/fonts/inter/Inter-Medium.ttf"),
    include_bytes!("../../../assets/fonts/inter/Inter-SemiBold.ttf"),
    include_bytes!("../../../assets/fonts/inter/Inter-Bold.ttf"),
];

/// Zed's assets (fonts, icons, themes, keymaps), Studio's own themes, and Studio's icons in
/// place of Zed's (`icons`).
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if let Some(svg) = icons::nucleo(path) {
            return Ok(Some(Cow::Borrowed(svg)));
        }
        match STUDIO_THEMES.iter().find(|(own, _)| *own == path) {
            Some((_, bytes)) => Ok(Some(Cow::Borrowed(bytes))),
            None => assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut list = assets::Assets.list(path)?;
        list.extend(
            STUDIO_THEMES
                .iter()
                .filter(|(own, _)| own.starts_with(path))
                .map(|(own, _)| SharedString::from(*own)),
        );
        Ok(list)
    }
}

impl Assets {
    /// Loads Zed's bundled fonts and Studio's Inter into the text system.
    pub fn load_fonts(&self, cx: &App) -> gpui::Result<()> {
        assets::Assets.load_fonts(cx)?;
        cx.text_system().add_fonts(
            STUDIO_FONTS
                .iter()
                .map(|font| Cow::Borrowed(*font))
                .collect(),
        )
    }
}

/// Sets up the release channel and Zed's settings store with Zed's defaults. Put the user's
/// settings into the store next, then call [`init_themes`].
pub fn init_settings_store(cx: &mut App) {
    release_channel::init_test(
        semver::Version::new(0, 1, 0),
        release_channel::ReleaseChannel::Dev,
        cx,
    );
    settings::init(cx);
}

/// Loads the bundled themes and fonts and selects the theme the settings store names. Must run
/// before any window opens.
///
/// The first theme is read from the store here, not from a later store notification: GPUI's
/// launch callback runs without flushing effects, so a notification sent during launch reaches
/// `theme_settings`' observer only if it arrives after the observer is active, and it does not.
/// Zed's own `main` orders its bootstrap the same way.
pub fn init_themes(cx: &mut App) {
    theme_settings::init(theme::LoadThemes::All(Box::new(Assets)), cx);
    if let Err(error) = Assets.load_fonts(cx) {
        log::warn!("Studio could not load its bundled fonts: {error}");
    }
}

/// Zed's settings store and its base theme, without fonts or bundled themes: enough for a test
/// window to draw Studio's views and editors.
#[cfg(any(test, feature = "test-support"))]
pub fn init_test(cx: &mut App) {
    init_settings_store(cx);
    theme_settings::init(theme::LoadThemes::JustBase, cx);
}

/// Eight muted tints: grey, slate blue, green, terracotta, violet, brick, teal, ochre.
const TINTS: [u32; 8] = [
    0x6b7280, 0x3f6aa8, 0x467350, 0xb0663a, 0x7a5ba8, 0xa55651, 0x2f7d86, 0x8a6d1f,
];

/// The tint a type's name hashes to, an index into [`TINTS`].
fn hashed_tint(type_name: &str) -> usize {
    let hash = type_name.encode_utf16().fold(0_u32, |h, unit| {
        h.wrapping_mul(31).wrapping_add(u32::from(unit))
    });
    (hash % TINTS.len() as u32) as usize
}

/// A stable color for a node or edge type, derived from its name alone. Views draw
/// [`type_tint`], which keeps the types of one graph apart where their names hash alike.
pub fn type_color(type_name: &str) -> Hsla {
    gpui::rgb(TINTS[hashed_tint(type_name)]).into()
}

/// The tints of the node types of the graphs read, by name ([`assign_type_tints`]).
#[derive(Default)]
struct TypeTints(HashMap<String, usize>);

impl Global for TypeTints {}

/// Each of `names`' tint when they are one graph's node types: the one its name hashes to,
/// unless a type before it in name order took it, when the next one free. So a graph's types
/// differ while it has no more than there are tints, and a type keeps its tint as long as the
/// graph's set of types does: it depends on the set, not on the order the schema lists them.
/// Past [`TINTS`]' count, the rest keep their hashed tint.
pub fn type_tints<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<(&'a str, usize)> {
    tints_beside(&HashMap::new(), names)
}

/// [`type_tints`] for one more graph's node types (`names`) beside the tints `held` gives the
/// types of the graphs read before: a name held keeps its tint, so no graph read later moves
/// another graph's types onto one tint; a new name takes the tint its name hashes to, else the
/// next one that neither another graph's type nor one of this graph's holds, else the next one
/// this graph's types leave free, else its hashed tint. So each graph's types differ while the
/// graphs read have no more types than there are tints, and one graph's while it has no more.
pub fn tints_beside<'a>(
    held: &HashMap<String, usize>,
    names: impl IntoIterator<Item = &'a str>,
) -> Vec<(&'a str, usize)> {
    let mut names: Vec<&str> = names.into_iter().collect();
    names.sort_unstable();
    names.dedup();
    let mut here = [false; TINTS.len()];
    let mut anywhere = [false; TINTS.len()];
    for &tint in held.values() {
        anywhere[tint] = true;
    }
    for name in &names {
        if let Some(&tint) = held.get(*name) {
            here[tint] = true;
        }
    }
    names
        .into_iter()
        .map(|name| {
            if let Some(&tint) = held.get(name) {
                return (name, tint);
            }
            let hashed = hashed_tint(name);
            let next = |free: &dyn Fn(usize) -> bool| {
                (0..TINTS.len())
                    .map(|step| (hashed + step) % TINTS.len())
                    .find(|&tint| free(tint))
            };
            let tint = next(&|tint| !here[tint] && !anywhere[tint])
                .or_else(|| next(&|tint| !here[tint]))
                .unwrap_or(hashed);
            here[tint] = true;
            anywhere[tint] = true;
            (name, tint)
        })
        .collect()
}

/// Notes a graph's node types (`names`, all of them), so [`type_tint`] keeps them apart
/// ([`tints_beside`]): a type another graph named already keeps that graph's tint.
pub fn assign_type_tints<'a>(names: impl IntoIterator<Item = &'a str>, cx: &mut App) {
    let tints = cx.default_global::<TypeTints>();
    for (name, tint) in tints_beside(&tints.0, names) {
        tints.0.insert(name.to_owned(), tint);
    }
}

/// The color a node type is drawn in: its graph's tint for it ([`assign_type_tints`]), or the
/// one its name hashes to ([`type_color`]) before a graph has named it.
pub fn type_tint(type_name: &str, cx: &App) -> Hsla {
    match cx
        .try_global::<TypeTints>()
        .and_then(|tints| tints.0.get(type_name))
    {
        Some(&tint) => gpui::rgb(TINTS[tint]).into(),
        None => type_color(type_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Studio's themes are listed with Zed's, so Zed's loader registers them, and each parses
    /// as a theme family with its themes named.
    #[test]
    fn studio_themes_are_listed_and_read_as_families() {
        let listed = Assets.list("themes/").unwrap();
        assert!(
            listed.iter().any(|path| path.ends_with("one/one.json")),
            "Zed's"
        );
        for (path, bytes) in STUDIO_THEMES {
            assert!(listed.iter().any(|listed| listed == path), "{path} listed");
            assert_eq!(Assets.load(path).unwrap().as_deref(), Some(*bytes));
            let family: serde_json::Value = serde_json::from_slice(bytes).expect(path);
            assert!(
                family["themes"]
                    .as_array()
                    .is_some_and(|themes| !themes.is_empty())
            );
        }
        let names: Vec<String> = STUDIO_THEMES
            .iter()
            .flat_map(|(_, bytes)| {
                let family: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                family["themes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|theme| theme["name"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .collect();
        for name in ["Vitesse Refined Light", "Studio Light", "Studio Dark"] {
            assert!(names.iter().any(|named| named == name), "{name}: {names:?}");
        }
    }

    #[test]
    fn a_type_name_always_gets_the_same_tint() {
        assert_eq!(type_color("Person"), type_color("Person"));
        // Project slate blue, Note green, Topic ochre, Person brick.
        assert_eq!(type_color("Project"), gpui::rgb(0x3f6aa8).into());
        assert_eq!(type_color("Note"), gpui::rgb(0x467350).into());
        assert_eq!(type_color("Topic"), gpui::rgb(0x8a6d1f).into());
        assert_eq!(type_color("Person"), gpui::rgb(0xa55651).into());
    }

    /// Company and Person hash to the same brick: as one graph's types they differ, the first
    /// by name keeping it, and the same set always gets the same tints.
    #[test]
    fn one_graphs_types_get_different_tints() {
        assert_eq!(hashed_tint("Company"), hashed_tint("Person"));
        let tints = type_tints(["Person", "Company", "Project"]);
        assert_eq!(
            tints,
            vec![("Company", 5), ("Person", 6), ("Project", 1)],
            "Company keeps brick, Person takes the next free, teal"
        );
        assert_eq!(tints, type_tints(["Project", "Company", "Person"]));
        // Nine types: eight differ, and the ninth keeps its hashed tint.
        let names: Vec<String> = (0..9).map(|n| format!("T{n}")).collect();
        let tints = type_tints(names.iter().map(String::as_str));
        let distinct: std::collections::HashSet<usize> =
            tints[..8].iter().map(|(_, tint)| *tint).collect();
        assert_eq!(distinct.len(), 8);
        assert_eq!(tints[8].1, hashed_tint("T8"));
    }

    /// A second graph never moves the first graph's types onto one tint: Person, which hashes
    /// to Company's brick, keeps the teal the first graph gave it when a graph without Company
    /// is read, where alone it would take brick; and the second graph's own types differ.
    #[test]
    fn a_second_graph_keeps_the_first_graphs_types_apart() {
        let mut held: HashMap<String, usize> = HashMap::new();
        for (name, tint) in tints_beside(&held, ["Company", "Person"]) {
            held.insert(name.to_owned(), tint);
        }
        assert_ne!(held["Company"], held["Person"]);
        assert_eq!(
            type_tints(["Person", "Topic"])[0],
            ("Person", 5),
            "alone: brick"
        );
        for (name, tint) in tints_beside(&held, ["Person", "Topic", "Note"]) {
            held.insert(name.to_owned(), tint);
        }
        assert_ne!(
            held["Company"], held["Person"],
            "the first graph's types stay apart"
        );
        let second: std::collections::HashSet<usize> = ["Person", "Topic", "Note"]
            .iter()
            .map(|n| held[*n])
            .collect();
        assert_eq!(second.len(), 3, "the second graph's types differ");
    }

    /// The same through the app's tints, as two sessions note their schemas: before 2026-11-20
    /// the second graph's Person took brick and drew the first graph's people as companies.
    #[gpui::test]
    fn a_second_graphs_schema_leaves_the_first_graphs_tints(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            assign_type_tints(["Company", "Person"], cx);
            let person = type_tint("Person", cx);
            assign_type_tints(["Person", "Topic"], cx);
            assert_eq!(type_tint("Person", cx), person);
            assert_ne!(type_tint("Company", cx), type_tint("Person", cx));
            assert_ne!(type_tint("Topic", cx), type_tint("Person", cx));
        });
    }
}
