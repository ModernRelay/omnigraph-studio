//! Studio's log (spec 002): warnings, errors and panics from Studio and from the Zed crates it
//! uses, written to `logs/Studio.log` in the data directory and kept near 1 MB, with the previous
//! file beside it. A run from a terminal writes to standard output instead. `RUST_LOG` raises or
//! narrows the level.
//!
//! Nothing Studio logs carries a token, query text or row data (constitution). A panic's message
//! loses its quoted parts before it is written, since a panic in string handling quotes the text
//! it was given.

use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::sync::OnceLock;

static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();
static OLD_LOG_FILE: OnceLock<PathBuf> = OnceLock::new();

/// Where the log is written when Studio was not started from a terminal.
pub fn log_file() -> PathBuf {
    studio_settings::data_dir().join("logs").join("Studio.log")
}

/// Starts logging and records panics. Call once, before the app runs.
pub fn init() {
    if let Err(error) = zlog::try_init(Some("warn".into())) {
        eprintln!("Studio could not start its log: {error}");
        return;
    }
    if std::io::stdout().is_terminal() {
        zlog::init_output_stdout();
    } else {
        let file = LOG_FILE.get_or_init(log_file);
        let old = OLD_LOG_FILE.get_or_init(|| file.with_file_name("Studio.old.log"));
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        if let Err(error) = zlog::init_output_file(file, Some(old)) {
            eprintln!("Studio could not open {}: {error}", file.display());
            zlog::init_output_stderr();
        }
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        let at = info
            .location()
            .map(|at| format!("{}:{}", at.file(), at.line()))
            .unwrap_or_default();
        let message = without_quotes(info.payload_as_str().unwrap_or("a non-text panic"));
        log::error!("panic on thread {thread} at {at}: {message}");
        zlog::flush();
        previous(info);
    }));
}

/// `message` with the text inside quotes and backticks replaced by `…`, cut to 300 characters.
fn without_quotes(message: &str) -> String {
    let mut out = String::new();
    let mut open: Option<char> = None;
    for c in message.chars() {
        match open {
            Some(quote) if c == quote => {
                out.push(c);
                open = None;
            }
            Some(_) => {}
            None if matches!(c, '`' | '\'' | '"') => {
                out.push(c);
                out.push('…');
                open = Some(c);
            }
            None => out.push(c),
        }
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
        assert_eq!(without_quotes(&"x".repeat(400)).chars().count(), 300);
    }
}
