//! Studio's log (spec 002): warnings, errors and panics from Studio and from the Zed crates it
//! uses, written to `logs/Studio.log` in the data directory and kept near 1 MB, with the previous
//! file beside it. A run from a terminal writes to standard output instead. `RUST_LOG` raises or
//! narrows the level.
//!
//! Nothing Studio logs carries a token, query text or row data (constitution). A panic's message
//! loses its quoted parts before it is written, since a panic in string handling quotes the text
//! it was given; and that line is all a panic prints. Rust's own panic report, which Studio's
//! replaces, prints the message whole, and a backtrace when one is asked for.

use std::io::{IsTerminal as _, Write as _};
use std::panic::PanicHookInfo;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();
static OLD_LOG_FILE: OnceLock<PathBuf> = OnceLock::new();
/// The log is written to standard output or standard error, where a panic's line shows already.
static LOGS_TO_TERMINAL: AtomicBool = AtomicBool::new(false);

/// Where the log is written when Studio was not started from a terminal.
pub fn log_file() -> PathBuf {
    studio_settings::data_dir().join("logs").join("Studio.log")
}

/// Starts logging and records panics. Call once, before the app runs.
pub fn init() {
    // Before the log, so a panic while it starts is reported the same way.
    std::panic::set_hook(Box::new(report_panic));
    if let Err(error) = zlog::try_init(Some("warn".into())) {
        eprintln!("Studio could not start its log: {error}");
        return;
    }
    if std::io::stdout().is_terminal() {
        zlog::init_output_stdout();
        LOGS_TO_TERMINAL.store(true, Ordering::SeqCst);
    } else {
        let file = LOG_FILE.get_or_init(log_file);
        let old = OLD_LOG_FILE.get_or_init(|| file.with_file_name("Studio.old.log"));
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        if let Err(error) = zlog::init_output_file(file, Some(old)) {
            eprintln!("Studio could not open {}: {error}", file.display());
            zlog::init_output_stderr();
            LOGS_TO_TERMINAL.store(true, Ordering::SeqCst);
        }
    }
}

/// Studio's panic report, in place of Rust's: the line [`panic_line`] makes, logged, and printed
/// to standard error unless the log already goes to the terminal.
fn report_panic(info: &PanicHookInfo) {
    let line = panic_line(info);
    log::error!("{line}");
    zlog::flush();
    if !LOGS_TO_TERMINAL.load(Ordering::SeqCst) {
        writeln!(std::io::stderr(), "{line}").ok();
    }
}

/// A panic in one line: its thread, where it happened, and its message without the text it
/// quotes ([`without_quotes`]).
fn panic_line(info: &PanicHookInfo) -> String {
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("unnamed");
    let at = info
        .location()
        .map(|at| format!("{}:{}", at.file(), at.line()))
        .unwrap_or_default();
    let message = without_quotes(info.payload_as_str().unwrap_or("a non-text panic"));
    format!("panic on thread {thread} at {at}: {message}")
}

/// `message` with the text inside quotes and backticks replaced by `…`, cut to 300 characters.
/// An apostrophe inside a word (`can't`, `Ada's`) neither opens nor closes a quote, and a quote
/// left open hides the rest.
fn without_quotes(message: &str) -> String {
    let mut out = String::new();
    let mut open: Option<char> = None;
    let mut before: Option<char> = None;
    let mut chars = message.chars().peekable();
    while let Some(c) = chars.next() {
        let after = chars.peek().copied();
        let in_word = |side: Option<char>| side.is_some_and(char::is_alphanumeric);
        match open {
            Some(quote) if c == quote && !(c == '\'' && in_word(after)) => {
                out.push(c);
                open = None;
            }
            Some(_) => {}
            None if matches!(c, '`' | '"') || (c == '\'' && !in_word(before)) => {
                out.push(c);
                out.push('…');
                open = Some(c);
            }
            None => out.push(c),
        }
        before = Some(c);
    }
    if out.chars().count() > 300 {
        out = out.chars().take(299).collect();
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_message_loses_the_text_it_quotes() {
        assert_eq!(
            without_quotes(
                "byte index 3 is not a char boundary; it is inside 'é' (bytes 2..4) of `Ada Lovelace's notes`"
            ),
            "byte index 3 is not a char boundary; it is inside '…' (bytes 2..4) of `…`"
        );
        assert_eq!(
            without_quotes(r#"called `Option::unwrap()` on a `None` value"#),
            "called `…` on a `…` value"
        );
        assert_eq!(without_quotes("plain"), "plain");
        // An apostrophe is no quote: the quoted name after it is still hidden.
        assert_eq!(without_quotes("can't open 'secret.json'"), "can't open '…'");
        assert_eq!(
            without_quotes("no row for 'Ada's notes' here"),
            "no row for '…' here"
        );
        assert_eq!(without_quotes("left open: 'secret"), "left open: '…");
        assert_eq!(without_quotes(&"x".repeat(400)).chars().count(), 300);
    }

    /// What a panic reports is its thread, where it happened and its message without the text
    /// it quotes, and nothing else: Rust's own report, which would print the message whole, is
    /// not chained to.
    #[test]
    fn a_panic_reports_one_line_without_the_text_it_quotes() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let previous = Arc::new(std::panic::take_hook());
        let (heard, other) = (seen.clone(), previous.clone());
        // Another test's panic, on another thread meanwhile, is reported as before.
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current().name() == Some("quoting-panic") {
                heard.lock().unwrap().push(panic_line(info));
            } else {
                other(info);
            }
        }));
        let panicked = std::thread::Builder::new()
            .name("quoting-panic".into())
            .spawn(|| panic!("no row for 'Ada Lovelace' in `secret notes`"))
            .unwrap()
            .join();
        let restored = previous.clone();
        std::panic::set_hook(Box::new(move |info| restored(info)));
        assert!(panicked.is_err());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        let at = format!("panic on thread quoting-panic at {}:", file!());
        assert!(seen[0].starts_with(&at), "{}", seen[0]);
        assert!(seen[0].ends_with(": no row for '…' in `…`"), "{}", seen[0]);
        assert!(!seen[0].contains("Ada") && !seen[0].contains("secret"));
    }
}
