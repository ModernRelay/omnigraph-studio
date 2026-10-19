//! Key bindings (spec 002): Zed's default keymap first, for the editor and menus, then Studio's,
//! which wins where both bind a key, then the person's `keymap.json` in Studio's data directory,
//! which wins over both. The person's file is read again when it changes: a binding that does
//! not load is reported and the rest apply; a file that does not parse keeps the bindings in
//! effect. Either way the problems show in a toast and on the settings tab.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpui::{App, Global, KeyBinding};
use settings::{KeymapFile, KeymapFileLoadResult};

#[cfg(target_os = "macos")]
const STUDIO_KEYMAP: &str = include_str!("../../../assets/keymaps/default-macos.json");
#[cfg(target_os = "windows")]
const STUDIO_KEYMAP: &str = include_str!("../../../assets/keymaps/default-windows.json");
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const STUDIO_KEYMAP: &str = include_str!("../../../assets/keymaps/default-linux.json");

/// When `keymap.json` was last read, so the watch sees a change.
struct UserKeymap {
    modified: Option<SystemTime>,
}

impl Global for UserKeymap {}

pub fn load(cx: &mut App) {
    cx.set_global(UserKeymap {
        modified: modified(),
    });
    bind_defaults(cx);
    // A file that does not parse at launch leaves Studio's bindings alone.
    let user = read_user(cx).unwrap_or_default();
    cx.bind_keys(user);
    watch(cx);
}

fn bind_defaults(cx: &mut App) {
    // Zed's keymap binds many actions Studio does not have; those bindings are dropped.
    bind(&settings::default_keymap(), cx);
    bind(STUDIO_KEYMAP, cx);
}

fn bind(content: &str, cx: &mut App) {
    match KeymapFile::load(content, cx) {
        KeymapFileLoadResult::Success { key_bindings }
        | KeymapFileLoadResult::SomeFailedToLoad { key_bindings, .. } => cx.bind_keys(key_bindings),
        KeymapFileLoadResult::JsonParseFailure { error } => {
            log::error!("Studio could not read a keymap: {error}");
        }
    }
}

fn modified() -> Option<SystemTime> {
    std::fs::metadata(studio_settings::keymap_path())
        .and_then(|meta| meta.modified())
        .ok()
}

/// The person's bindings, recording what did not load; `None` when the file cannot be read or
/// does not parse, so the bindings in effect stay. A missing file binds nothing.
fn read_user(cx: &mut App) -> Option<Vec<KeyBinding>> {
    let path = studio_settings::keymap_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            studio_settings::set_keymap_problems(Vec::new(), cx);
            return Some(Vec::new());
        }
        Err(error) => {
            let problem = format!("{} could not be read: {error}", path.display());
            studio_settings::set_keymap_problems(vec![problem], cx);
            return None;
        }
    };
    let (bindings, problems) = match KeymapFile::load(&text, cx) {
        KeymapFileLoadResult::Success { key_bindings } => (Some(key_bindings), Vec::new()),
        KeymapFileLoadResult::SomeFailedToLoad {
            key_bindings,
            error_message,
        } => (Some(key_bindings), binding_problems(&error_message.0)),
        KeymapFileLoadResult::JsonParseFailure { error } => (
            None,
            vec![format!(
                "{} could not be read: {error}. The shortcuts in effect stay until it reads \
                 again.",
                path.display()
            )],
        ),
    };
    studio_settings::set_keymap_problems(problems, cx);
    bindings
}

/// One line per binding that did not load, from Zed's report: a heading per section of the
/// file (`In section with \`context = "Workspace"\`:`), then an item per failure (`- In binding
/// \`"cmd-k"\`, didn't find …`). A line reads "`cmd-k` in the `Workspace` section: didn't find
/// …". A report in another shape is kept whole.
fn binding_problems(report: &str) -> Vec<String> {
    let mut problems: Vec<String> = Vec::new();
    let mut context: Option<String> = None;
    for line in report.lines().skip(1) {
        let line = unquote(line.trim());
        if line.is_empty() {
            continue;
        }
        if let Some(heading) = line.strip_prefix("In section ") {
            let (heading, rest) = heading.split_once(':').unwrap_or((heading, ""));
            context = heading
                .strip_prefix("with `context = ")
                .and_then(|context| context.strip_suffix('`'))
                .map(|context| context.trim_matches('"').to_owned());
            if !rest.trim().is_empty() {
                problems.push(format!("{}: {}", section(&context), rest.trim()));
            }
        } else if let Some(item) = line.strip_prefix("- ") {
            let binding = item
                .strip_prefix("In binding ")
                .map(|rest| (false, rest))
                .or_else(|| item.strip_prefix("In unbind ").map(|rest| (true, rest)))
                .and_then(|(unbinding, rest)| Some((unbinding, rest.split_once(", ")?)));
            problems.push(match binding {
                Some((unbinding, (keys, message))) => {
                    let verb = if unbinding { "Unbinding " } else { "" };
                    let place = match &context {
                        Some(context) => format!(" in the `{context}` section"),
                        None => String::new(),
                    };
                    format!("{verb}{keys}{place}: {message}")
                }
                None => format!("{}: {}", section(&context), lowercase_first(item)),
            });
        } else if let Some(last) = problems.last_mut() {
            last.push(' ');
            last.push_str(&line);
        }
    }
    if problems.is_empty() {
        problems.push(report.trim().to_owned());
    }
    problems
}

fn section(context: &Option<String>) -> String {
    match context {
        Some(context) => format!("The `{context}` section"),
        None => "The section without a context".to_owned(),
    }
}

/// Zed quotes keys and action names inside code spans (`` `"cmd-k"` ``); the quotes go.
fn unquote(line: &str) -> String {
    line.replace("`\"", "`").replace("\"`", "`")
}

fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Loads `keymap.json` again when another program changes it (checked every two seconds). A
/// change rebinds everything, defaults first, and redraws the menus, whose shortcuts are fixed
/// when they are set.
fn watch(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            // Off the main thread: a slow or network home would hold up frames.
            let modified = cx.background_executor().spawn(async { modified() }).await;
            let changed = cx.update(|cx| cx.global::<UserKeymap>().modified != modified);
            if changed {
                cx.update(|cx| {
                    cx.global_mut::<UserKeymap>().modified = modified;
                    reload(cx);
                });
            }
        }
    })
    .detach();
}

/// Reads `keymap.json` and, when it parses, rebinds every key from the defaults up.
pub(crate) fn reload(cx: &mut App) {
    let Some(user) = read_user(cx) else {
        return;
    };
    cx.clear_key_bindings();
    bind_defaults(cx);
    cx.bind_keys(user);
    crate::menus::set(cx);
    cx.refresh_windows();
}

/// `keymap.json`, written with a starting point when the person has none: an empty section to
/// fill in and Studio's own bindings as a commented reference.
pub(crate) fn user_keymap_file() -> std::io::Result<PathBuf> {
    let path = studio_settings::keymap_path();
    if !path.exists() {
        studio_settings::write_atomic(
            &path,
            starting_keymap().as_bytes(),
            studio_settings::Access::Private,
        )?;
    }
    Ok(path)
}

fn starting_keymap() -> String {
    let modifier = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    let mut text = format!(
        "// Your shortcuts, over Studio's own. A binding here wins over Studio's for the same keys
// where its \"context\" holds, and `null` removes a key. Studio reads this file again when it
// is saved; a binding it cannot use is listed on the settings tab.
[
  {{
    \"context\": \"Workspace\",
    \"bindings\": {{
      // \"{modifier}-alt-o\": \"studio::OpenGraph\",
      // \"{modifier}-k\": null
    }}
  }}
]

// Studio {}'s bindings, for reference:
//
",
        env!("CARGO_PKG_VERSION")
    );
    for line in STUDIO_KEYMAP.lines() {
        text.push_str("// ");
        text.push_str(line);
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zed's report becomes one line per binding, named by its keys and section.
    #[test]
    fn each_binding_that_did_not_load_is_one_problem() {
        let report = "Errors in user keymap file.\n\
            In section with `context = \"Workspace\"`:\n\n\
            - In binding `\"cmd-k\"`, didn't find an action named `\"studio::Nope\"`.\n\n\
            - In unbind `\"cmd-j\"`, invalid keystroke `\"cmd-\"`.\n\n \
            - Unrecognized fields: `[\"bindngs\"]`\n\
            In section without context predicate:\n\n\
            - In binding `\"x\"`, didn't find an action named `\"y\"`.";
        assert_eq!(
            binding_problems(report),
            [
                "`cmd-k` in the `Workspace` section: didn't find an action named `studio::Nope`.",
                "Unbinding `cmd-j` in the `Workspace` section: invalid keystroke `cmd-`.",
                "The `Workspace` section: unrecognized fields: `[\"bindngs\"]`",
                "`x`: didn't find an action named `y`.",
            ]
        );
        assert_eq!(binding_problems("Something else"), ["Something else"]);
    }

    /// The file Studio writes for the person reads as it stands, binding nothing until a line
    /// is uncommented.
    #[test]
    fn the_starting_keymap_parses_and_binds_nothing() {
        let text = starting_keymap();
        KeymapFile::parse(&text).unwrap();
        let sections: serde_json::Value = settings::parse_json_with_comments(&text).unwrap();
        assert_eq!(
            sections,
            serde_json::json!([{"context": "Workspace", "bindings": {}}])
        );
        assert!(
            text.contains("// Studio "),
            "the defaults follow as a reference"
        );
    }
}
