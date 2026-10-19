//! Small value helpers: commit ids, relative and absolute times, copyable values.

use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{App, ClipboardItem, ElementId, SharedString, Window};
use ui::Tooltip;
use ui::prelude::*;

use crate::button::IconButton;

/// A value as plain text: a string as it is, nothing for null, anything else as JSON. What a
/// copied cell holds, and what a text field shows and reads back.
pub fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The last segment of a graph commit id, eight characters long, for display. Commit ids such as
/// `hb1.<ulid>.7.<ulid>` end in a ULID whose tail is random.
pub fn short_commit_id(id: &str) -> String {
    let tail = id.rsplit('.').next().unwrap_or(id);
    let chars: Vec<char> = tail.chars().collect();
    chars[chars.len().saturating_sub(8)..].iter().collect()
}

/// `20000` → `20,000`.
pub fn group_digits(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A byte count for people: "690 B", "2.1 KB", "14 MB", in powers of 1000 as macOS shows sizes.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.;
    let mut unit = 0;
    while value >= 999.95 && unit < UNITS.len() - 1 {
        value /= 1000.;
        unit += 1;
    }
    if value < 10. {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// Now, in Unix microseconds (the unit of OmniGraph timestamps).
pub fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// "just now", "5 min ago", "3 h ago", "2 days ago", then the date.
pub fn relative_time(micros: i64, now_micros: i64) -> String {
    let seconds = (now_micros - micros) / 1_000_000;
    match seconds {
        s if s < 45 => "just now".into(),
        s if s < 60 * 60 => format!("{} min ago", (s + 30) / 60),
        s if s < 60 * 60 * 24 => format!("{} h ago", s / 3600),
        s if s < 60 * 60 * 24 * 14 => {
            let days = s / 86_400;
            format!("{days} day{} ago", if days == 1 { "" } else { "s" })
        }
        _ => utc_date_time(micros)[..10].to_owned(),
    }
}

/// `2026-10-12 14:03:09 UTC` for Unix microseconds.
pub fn utc_date_time(micros: i64) -> String {
    let seconds = micros.div_euclid(1_000_000);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A value with a copy button. Missing values read "Not reported by this server" (spec 010).
#[derive(IntoElement)]
pub struct CopyableValue {
    id: ElementId,
    value: Option<SharedString>,
    mono: bool,
}

impl CopyableValue {
    pub fn new(id: impl Into<ElementId>, value: Option<impl Into<SharedString>>) -> Self {
        Self {
            id: id.into(),
            value: value.map(Into::into),
            mono: true,
        }
    }

    /// Show the value in the UI font rather than monospace.
    pub fn plain(mut self) -> Self {
        self.mono = false;
        self
    }
}

impl RenderOnce for CopyableValue {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Some(value) = self.value else {
            return h_flex()
                .id(self.id)
                .tooltip(Tooltip::text("Not reported by this server"))
                .child(
                    Label::new("—")
                        .size(LabelSize::Small)
                        .color(Color::Disabled),
                )
                .into_any_element();
        };
        let mut label = crate::ds::text::body(value.clone())
            .single_line()
            .truncate();
        if self.mono {
            label = label.buffer_font(cx);
        }
        // The copy button shows on hover, as Delta reveals row actions, so a page of values
        // is not a column of icons.
        let group: SharedString = format!("copy-{:?}", self.id).into();
        h_flex()
            .id(self.id.clone())
            .group(group.clone())
            .gap(px(crate::ds::space::XS))
            .min_w_0()
            .child(div().min_w_0().child(label))
            .child(
                IconButton::new(self.id, IconName::Copy)
                    .size(ButtonSize::Compact)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .visible_on_hover(group)
                    .tooltip(Tooltip::text("Copy"))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(value.to_string()))
                    }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_ids_shorten_to_their_random_tail() {
        assert_eq!(
            short_commit_id("hb1.01M4RAG6MF4TBACF5XK66BG0PF.7.01M4RAG6XNMGWMFFYMBM7XFPEG"),
            "BM7XFPEG"
        );
        assert_eq!(short_commit_id("abc"), "abc");
        assert_eq!(group_digits(20_000), "20,000");
    }

    #[test]
    fn byte_counts_read_in_powers_of_a_thousand() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(690), "690 B");
        assert_eq!(format_bytes(2_140), "2.1 KB");
        assert_eq!(format_bytes(14_200_000), "14 MB");
        assert_eq!(format_bytes(999_999), "1.0 MB");
        assert_eq!(format_bytes(3_000_000_000), "3.0 GB");
    }

    #[test]
    fn times_read_relative_then_as_dates() {
        let now = 1_791_817_685_000_000;
        assert_eq!(relative_time(now - 10_000_000, now), "just now");
        assert_eq!(relative_time(now - 300_000_000, now), "5 min ago");
        assert_eq!(relative_time(now - 3 * 3_600_000_000, now), "3 h ago");
        assert_eq!(relative_time(now - 86_400_000_000, now), "1 day ago");
        assert_eq!(relative_time(now - 30 * 86_400_000_000, now), "2026-09-12");
        assert_eq!(utc_date_time(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(
            utc_date_time(951_782_400_000_000),
            "2000-02-29 00:00:00 UTC"
        );
    }
}
