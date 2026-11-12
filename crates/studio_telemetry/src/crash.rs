//! Crash reports (spec 018, "Crash reports"): what a panic or a native crash leaves, cut down to
//! its kind, its one line, the crashed thread's function names and how long Studio had run.
//! Nothing here holds a path under the home folder, a quoted value or a graph's data.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::catalog::RunBucket;

/// Most frames a report keeps.
pub const MAX_FRAMES: usize = 64;

/// The crates whose frames are Studio's or Zed's own, marked `in_app` for PostHog's grouping.
const IN_APP: &[&str] = &[
    "studio",
    "graph_",
    "type_table",
    "relationship_tree",
    "query_editor",
    "review",
    "history",
    "agent_panel",
    "command_palette",
    "change_cards",
    "control_client",
    "oauth_client",
    "filter_words",
    "gq_language",
    "model",
    "gpui",
    "ui",
    "editor",
    "workspace",
];

/// A crash, kept on disk until it is sent or deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashReport {
    /// `panic`, or the signal or exception macOS reported (`EXC_BAD_ACCESS`).
    pub kind: String,
    /// The panic's one line (its quoted parts already replaced), or the exception's codes.
    pub line: String,
    pub thread: String,
    pub frames: Vec<Frame>,
    pub ran: Option<RunBucket>,
    /// Studio's version when it crashed.
    pub version: String,
    /// When it crashed, RFC 3339.
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub function: String,
    pub in_app: bool,
}

/// `text` with the home folder written `~`, so no path names the person's account.
pub fn without_home(text: &str, home: &Path) -> String {
    let home = home.to_string_lossy();
    if home.len() < 2 {
        return text.to_owned();
    }
    text.replace(home.as_ref(), "~")
}

fn in_app(function: &str) -> bool {
    let krate = function
        .trim_start_matches('<')
        .split("::")
        .next()
        .unwrap_or_default();
    IN_APP
        .iter()
        .any(|prefix| krate == *prefix || (prefix.ends_with('_') && krate.starts_with(prefix)))
        || krate.starts_with("studio_")
}

/// The function names of a backtrace as `std::backtrace::Backtrace` prints it, without the
/// panic machinery's own frames, at most [`MAX_FRAMES`]. File paths are dropped: a function's
/// name says where it is.
pub fn frames_of_backtrace(printed: &str) -> Vec<Frame> {
    let mut frames = Vec::new();
    for line in printed.lines() {
        let line = line.trim();
        // "12: path::to::function" (the "at file:line" lines that follow are dropped).
        let Some((number, function)) = line.split_once(": ") else {
            continue;
        };
        if number.parse::<u32>().is_err() {
            continue;
        }
        let function = strip_hash(function.trim());
        if function.starts_with("std::panicking")
            || function.starts_with("core::panicking")
            || function.starts_with("std::backtrace")
            || function.starts_with("std::sys::backtrace")
            || function.starts_with("studio::logging")
            || function.starts_with("studio_telemetry::crash")
            || function.starts_with("<alloc::boxed::Box<F,A> as core::ops::function::Fn")
            || function.starts_with("rust_begin_unwind")
            || function == "__rust_begin_short_backtrace"
        {
            continue;
        }
        frames.push(Frame {
            in_app: in_app(&function),
            function,
        });
        if frames.len() == MAX_FRAMES {
            break;
        }
    }
    frames
}

/// A symbol without its trailing `::h0123abcd` hash.
fn strip_hash(function: &str) -> String {
    match function.rsplit_once("::h") {
        Some((head, hash)) if hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit()) => {
            head.to_owned()
        }
        _ => function.to_owned(),
    }
}

/// A macOS crash report (`.ips`, a JSON header line and a JSON body) cut down to its exception
/// type, the crashed thread's frames and nothing else. `None` when it is not one Studio can read.
pub fn from_ips(text: &str, version: &str, at: String) -> Option<CrashReport> {
    let (_, body) = text.split_once('\n')?;
    let body: serde_json::Value = serde_json::from_str(body).ok()?;
    let exception = body.get("exception")?;
    let kind = exception
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("crash")
        .to_owned();
    let signal = exception
        .get("signal")
        .and_then(|s| s.as_str())
        .unwrap_or_default();
    let images: Vec<String> = body
        .get("usedImages")
        .and_then(|i| i.as_array())
        .map(|images| {
            images
                .iter()
                .map(|image| {
                    image
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("?")
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default();
    let index = body
        .get("faultingThread")
        .and_then(|t| t.as_u64())
        .unwrap_or(0) as usize;
    let thread = body.get("threads")?.as_array()?.get(index)?;
    let name = thread
        .get("name")
        .or_else(|| thread.get("queue"))
        .and_then(|n| n.as_str())
        .unwrap_or("unnamed")
        .to_owned();
    let frames = thread
        .get("frames")
        .and_then(|f| f.as_array())
        .map(|frames| {
            frames
                .iter()
                .take(MAX_FRAMES)
                .map(|frame| {
                    let image = frame
                        .get("imageIndex")
                        .and_then(|i| i.as_u64())
                        .and_then(|i| images.get(i as usize))
                        .map_or("?", String::as_str);
                    let function = frame
                        .get("symbol")
                        .and_then(|s| s.as_str())
                        .map_or_else(|| image.to_owned(), strip_hash);
                    Frame {
                        in_app: image == "omnigraph-studio",
                        function,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    Some(CrashReport {
        line: if signal.is_empty() {
            kind.clone()
        } else {
            format!("{kind} ({signal})")
        },
        kind,
        thread: name,
        frames,
        ran: None,
        version: version.to_owned(),
        at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backtrace_keeps_function_names_and_drops_paths_and_panic_frames() {
        let printed = "   0: std::backtrace::Backtrace::force_capture\n             at /rustc/abc/library/std/src/backtrace.rs:310:9\n   1: studio::logging::report_panic\n   2: std::panicking::rust_panic_with_hook\n   3: type_table::table::TypeTable::render_rows::h0123456789abcdef\n             at /Users/someone/work/omnigraph-studio/crates/type_table/src/table.rs:120:5\n   4: gpui::window::Window::draw\n   5: core::ops::function::FnOnce::call_once\n";
        let frames = frames_of_backtrace(printed);
        let names: Vec<&str> = frames.iter().map(|f| f.function.as_str()).collect();
        assert_eq!(
            names,
            [
                "type_table::table::TypeTable::render_rows",
                "gpui::window::Window::draw",
                "core::ops::function::FnOnce::call_once"
            ]
        );
        assert_eq!(
            frames.iter().map(|f| f.in_app).collect::<Vec<_>>(),
            [true, true, false]
        );
        assert!(!format!("{frames:?}").contains("someone"));
    }

    #[test]
    fn the_home_folder_is_written_as_a_tilde() {
        let home = Path::new("/Users/someone");
        assert_eq!(
            without_home(
                "panic at /Users/someone/.cargo/registry/src/x.rs:1: boom",
                home
            ),
            "panic at ~/.cargo/registry/src/x.rs:1: boom"
        );
    }

    #[test]
    fn a_macos_report_keeps_the_exception_and_the_crashed_threads_symbols() {
        let ips = r#"{"app_name":"omnigraph-studio","bug_type":"309"}
{"exception":{"type":"EXC_BAD_ACCESS","signal":"SIGSEGV","codes":"0x1"},"faultingThread":1,
 "usedImages":[{"name":"omnigraph-studio","path":"/Applications/Omnigraph Studio.app/x"},{"name":"libsystem_kernel.dylib"}],
 "threads":[{"frames":[{"imageIndex":1,"symbol":"__pthread_kill"}]},
            {"name":"render","frames":[{"imageIndex":0,"symbol":"gpui::platform::draw::h0123456789abcdef"},{"imageIndex":1}]}],
 "userName":"someone"}"#;
        let report = from_ips(ips, "0.4.4", "2026-11-11T00:00:00Z".into()).unwrap();
        assert_eq!(report.kind, "EXC_BAD_ACCESS");
        assert_eq!(report.line, "EXC_BAD_ACCESS (SIGSEGV)");
        assert_eq!(report.thread, "render");
        assert_eq!(report.frames[0].function, "gpui::platform::draw");
        assert!(report.frames[0].in_app);
        assert_eq!(report.frames[1].function, "libsystem_kernel.dylib");
        let all = serde_json::to_string(&report).unwrap();
        assert!(!all.contains("someone") && !all.contains("/Applications"));
    }
}
