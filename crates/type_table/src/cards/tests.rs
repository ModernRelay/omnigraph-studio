//! The cards' keys, paging, reads, format and drawing, against the fake server (spec 048; they
//! began as the tests of spec 035's gallery).

use std::cell::RefCell;

use gpui::{Bounds, KeyBinding, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels};
use gpui::{TestAppContext, VisualTestContext};
use serde_json::json;

use studio_actions::table::{SelectNextSuggestion, SelectPreviousSuggestion};
use studio_settings::CardCover;

use super::*;
use crate::test_support::{changed, session_served, session_with_files};
use gq_language::FilterOp;

const NOTES: &str =
    "node Note {\n  slug: String @key\n  title: String\n  attachment: Blob?\n  scan: Blob?\n}\n";

/// `total` notes, `n000` on, each titled.
fn note(i: usize) -> Value {
    json!({"n": {"@id": format!("n{i:03}"), "slug": format!("n{i:03}"), "title": format!("Note {i}")}})
}

/// Answers the cards' reads of `total` notes: a keyset page after its `after` id, at most its
/// `limit`; the count; and listed rows by id.
fn notes_answering(total: usize) -> impl Fn(&Value) -> Value + Send + Sync + 'static {
    move |request: &Value| {
        let name = request["name"].as_str().unwrap_or_default();
        let text = request["query"].as_str().unwrap_or_default();
        let limit = text
            .split("limit ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(total);
        match name {
            "studio_count" => json!([{ "n": total }]),
            "studio_page" => {
                let after = request["params"]["after"].as_str().map(str::to_owned);
                let from = after.map_or(0, |after| {
                    after.trim_start_matches('n').parse::<usize>().unwrap_or(0) + 1
                });
                Value::from((from..total).take(limit).map(note).collect::<Vec<_>>())
            }
            "studio_nodes_by_id" => Value::from(
                request["params"]["ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|id| id.as_str()?.trim_start_matches('n').parse::<usize>().ok())
                    .filter(|&i| i < total)
                    .map(note)
                    .collect::<Vec<_>>(),
            ),
            _ => json!([]),
        }
    }
}

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        studio_theme::init_test(cx);
        // A change to a card's format is kept as the type's in the settings, held in memory.
        studio_settings::init_test(cx);
        studio_settings::keep_changes_in_memory(cx);
        editor::init(cx);
        cx.bind_keys([
            // As the keymaps bind them (spec 048, slice 5): a key that would act on the cards
            // while a person writes a filter is bound only while no field has the keyboard.
            KeyBinding::new("left", MoveLeft, Some("Cards && !editing")),
            KeyBinding::new("right", MoveRight, Some("Cards && !editing")),
            KeyBinding::new("up", MoveUp, Some("Cards && !editing")),
            KeyBinding::new("down", MoveDown, Some("Cards && !editing")),
            KeyBinding::new("home", MoveToFirst, Some("Cards && !editing")),
            KeyBinding::new("end", MoveToLast, Some("Cards && !editing")),
            KeyBinding::new("enter", OpenPage, Some("Cards && !editing")),
            KeyBinding::new("cmd-enter", OpenPage, Some("Cards && !editing")),
            KeyBinding::new("alt-enter", OpenCover, Some("Cards && !editing")),
            KeyBinding::new("cmd-a", PickAll, Some("Cards && !editing")),
            KeyBinding::new("cmd-c", CopyIds, Some("Cards && !editing")),
            KeyBinding::new("g", ShowOnCanvas, Some("Cards && !editing")),
            KeyBinding::new("x", TogglePick, Some("Cards && !editing")),
            KeyBinding::new("f", ShowNextCover, Some("Cards && !editing")),
            KeyBinding::new("=", LargerCards, Some("Cards && !editing")),
            KeyBinding::new("-", SmallerCards, Some("Cards && !editing")),
            KeyBinding::new("escape", Cancel, Some("Cards")),
            KeyBinding::new("cmd-r", Refresh, Some("Cards")),
            KeyBinding::new("cmd-f", AddFilter, Some("Cards")),
            KeyBinding::new("cmd-alt-backspace", ClearFilters, Some("Cards")),
            KeyBinding::new("enter", ConfirmEdit, Some("Cards && editing")),
            // The value field's own keys: its offers, and the text's, as Zed's keymap binds
            // them in an editor.
            KeyBinding::new("up", SelectPreviousSuggestion, Some("FilterValue > Editor")),
            KeyBinding::new("down", SelectNextSuggestion, Some("FilterValue > Editor")),
            KeyBinding::new("left", editor::actions::MoveLeft, Some("Editor")),
            KeyBinding::new(
                "home",
                editor::actions::MoveToBeginningOfLine {
                    stop_at_soft_wraps: true,
                    stop_at_indent: false,
                },
                Some("Editor"),
            ),
            KeyBinding::new("cmd-a", editor::actions::SelectAll, Some("Editor")),
            KeyBinding::new("cmd-c", editor::actions::Copy, Some("Editor")),
            KeyBinding::new("escape", editor::actions::Cancel, Some("Editor")),
            // As a person's keymap written for the gallery has it.
            KeyBinding::new("ctrl-shift-e", MoveToLast, Some("Gallery")),
        ]);
    });
}

fn opening(type_name: &str) -> ShowSetInCards {
    ShowSetInCards {
        title: type_name.into(),
        type_name: type_name.into(),
        filters: None,
        sort: None,
        ids: None,
        cover: None,
        at_commit: None,
        snapshot: None,
        select: None,
        preview: false,
    }
}

/// The cards of `opening` in a window, as a new tab opens them: their schema read first,
/// focused, settled.
fn open(
    session: Entity<GraphSession>,
    opening: ShowSetInCards,
    cx: &mut TestAppContext,
) -> (Entity<TypeCards>, &mut VisualTestContext) {
    open_with(session, opening, None, cx)
}

/// As [`open`], with the format a restored tab kept, or `None` for a new tab's.
fn open_with(
    session: Entity<GraphSession>,
    opening: ShowSetInCards,
    format: Option<CardFormat>,
    cx: &mut TestAppContext,
) -> (Entity<TypeCards>, &mut VisualTestContext) {
    cx.run_until_parked();
    let (cards, cx) = cx.add_window_view(|window, cx| {
        TypeCards::new(
            WeakEntity::new_invalid(),
            session,
            opening,
            format,
            window,
            cx,
        )
    });
    cards.update_in(cx, |cards, window, cx| {
        window.focus(&cards.focus_handle, cx)
    });
    settle(cx);
    (cards, cx)
}

/// Lets reads land, then the pause before covers are read, then those reads, then the draw of
/// what landed.
fn settle(cx: &mut VisualTestContext) {
    for _ in 0..3 {
        cx.run_until_parked();
        cx.executor().advance_clock(READ_AFTER);
        cx.run_until_parked();
        cx.executor().advance_clock(LANDING_DELAY);
        cx.run_until_parked();
    }
}

fn grid_renders(cards: &Entity<TypeCards>, cx: &mut VisualTestContext) -> usize {
    let grid = cards.read_with(cx, |cards, _| cards.grid.clone());
    grid.read_with(cx, |grid, _| grid.renders)
}

/// Where card `ix` was drawn in the window, when the last frame laid it out.
fn card_bounds(ix: usize, cx: &mut VisualTestContext) -> Option<Bounds<Pixels>> {
    // A test's selectors live as long as the test binary.
    cx.debug_bounds(Box::leak(format!("card-{ix}").into_boxed_str()))
}

/// Scrolls the cards to their end, as far as GPUI lets the offset go.
fn scroll_to_end(cards: &Entity<TypeCards>, cx: &mut VisualTestContext) {
    cards.update(cx, |cards, cx| {
        let bottom = cards.card_grid().height(cards.placed());
        cards.scroll.set_offset(point(px(0.), px(-bottom)));
        cx.notify();
    });
    settle(cx);
}

/// A PNG of `width` by `height` pixels, of noise when `noisy` so it does not compress (and is
/// read in more than its first 64 KiB), else of one colour.
fn png(width: u32, height: u32, noisy: bool) -> Vec<u8> {
    let mut seed = 0x2545_f491_u32;
    let image = image::RgbImage::from_fn(width, height, |_, _| {
        if noisy {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let [a, b, c, _] = seed.to_le_bytes();
            image::Rgb([a, b, c])
        } else {
            image::Rgb([40, 120, 200])
        }
    });
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

/// The key of row `row`'s cover of `attachment`.
fn cover_key(row: usize) -> CoverKey {
    (format!("n{row:03}"), "attachment".to_owned())
}

/// The columns fill the tab's width: as many of at least the card size as fit, with the edges
/// and gaps kept, and one when the width is narrower than a card. A card's height is its
/// cover's box, 4 by 3 across it, its title line and a line a field: with no cover it has no
/// box, and with no field no line.
#[test]
fn the_columns_fill_the_width() {
    let edge = studio_ui::ds::size::CARD;
    let grid = grid_for(1200., edge, true, 0);
    assert_eq!(grid.columns(), 4);
    let columns = grid.columns() as f32;
    let used = columns * grid.width() + (columns - 1.) * space::LG + 2. * space::EDGE;
    assert!((used - 1200.).abs() < 0.01, "{used}");
    assert!(grid.width() >= edge);
    let cover = grid.card_height() - 2. * card_inset() - studio_ui::ds::size::CARD_TITLE;
    let inner = grid.width() - 2. * card_inset();
    assert!((cover - inner * 3. / 4.).abs() <= 0.5, "{cover} of {inner}");
    assert_eq!(cover.fract(), 0., "on whole pixels");
    let narrow = grid_for(100., edge, true, 0);
    assert_eq!((narrow.columns(), narrow.width()), (1, edge));
    assert!(grid_for(1200., studio_ui::ds::size::CARD_LARGE, true, 0).columns() < 4);
    assert!(grid_for(1200., studio_ui::ds::size::CARD_SMALL, true, 0).columns() > 4);
    // The same columns whatever a card holds; each field is one line more, and the cover's box
    // is there or not.
    let (title, line) = (
        studio_ui::ds::size::CARD_TITLE,
        studio_ui::ds::size::CARD_LINE,
    );
    let bare = grid_for(1200., edge, false, 0);
    assert_eq!(bare.columns(), 4);
    assert_eq!(bare.card_height(), 2. * card_inset() + title);
    assert_eq!(grid.card_height() - bare.card_height(), cover);
    let three = grid_for(1200., edge, false, 3);
    let eight = grid_for(1200., edge, false, 8);
    assert_eq!(eight.card_height() - three.card_height(), 5. * line);
    assert!(three.card_height() - bare.card_height() >= 3. * line);
    assert_eq!(
        grid_for(1200., edge, true, 3).card_height() - three.card_height(),
        cover
    );
}

/// Every card is one size, whatever its cover holds: a tall picture, a wide one, a page and no
/// file are drawn in boxes of one shape, each card where its index puts it. In place of the
/// gallery's `a_tile_is_as_tall_as_its_files_shape`.
#[gpui::test]
async fn every_card_is_one_size_whatever_its_cover(cx: &mut TestAppContext) {
    init(cx);
    let (session, blobs) = session_with_files(NOTES, notes_answering(4), None, cx);
    blobs.set("Note/n000/attachment", &png(200, 400, false));
    blobs.set("Note/n001/attachment", &png(400, 100, false));
    blobs.set("Note/n002/attachment", b"%PDF-1.4\n%a page\n");
    // n003 holds none.
    let (cards, cx) = open(session, opening("Note"), cx);
    let grid = cards.read_with(cx, |cards, _| {
        assert_eq!(cards.placed(), 4);
        assert!(matches!(
            cards.covers[&cover_key(0)].look,
            Loadable::Loaded(Look::Image { .. })
        ));
        assert!(matches!(
            cards.covers[&cover_key(3)].meta,
            Loadable::Loaded(BlobMeta::Null)
        ));
        cards.card_grid()
    });
    let first = card_bounds(0, cx).expect("the first card is drawn");
    // A column's width is a fraction of a pixel, which GPUI lays out on the device's pixels:
    // within one of what the grid says. Heights and tops are whole pixels.
    let near = |drawn: Pixels, said: f32| (f32::from(drawn) - said).abs() < 1.;
    for ix in 0..4 {
        let bounds = card_bounds(ix, cx).expect("drawn");
        assert_eq!(
            bounds.size.height, first.size.height,
            "card {ix} is another height"
        );
        assert_eq!(f32::from(bounds.size.height), grid.card_height());
        assert!(near(bounds.size.width, grid.width()), "{bounds:?}");
        let (left, top) = grid.place_of(ix);
        assert!(near(bounds.origin.x - first.origin.x, left), "{bounds:?}");
        assert_eq!(f32::from(bounds.origin.y - first.origin.y), top);
    }
}

/// ← and → go to the card before and after in row order, across line ends; ↑ and ↓ by a line;
/// ↓ onto a short last line goes to the last card; Home and End, X, ⌘A and Esc, = and −, and F
/// as before. A size change flows the columns again from the previews kept: no cover is read
/// twice. Adapted from the gallery's `the_arrows_move_within_and_across_columns`.
#[gpui::test]
async fn the_arrows_move_in_row_order_and_by_lines(cx: &mut TestAppContext) {
    init(cx);
    // 41 cards: a prime, so the last line is short whatever the columns.
    let (session, blobs) = session_with_files(NOTES, notes_answering(41), None, cx);
    for i in 0..41 {
        let (w, h) = [(100, 200), (100, 100), (200, 100)][i % 3];
        blobs.set(&format!("Note/n{i:03}/attachment"), &png(w, h, false));
    }
    let (cards, cx) = open(session, opening("Note"), cx);
    let (columns, placed) = cards.read_with(cx, |c, _| (c.card_grid().columns(), c.placed()));
    assert!((3..20).contains(&columns), "{columns} columns");
    assert_eq!(placed, 41);
    let cursor = |cx: &mut VisualTestContext| cards.read_with(cx, |c, _| c.cursor.unwrap());
    assert_eq!(cursor(cx), 0);

    // Along the first line, and across its end to the next.
    cx.simulate_keystrokes("left");
    assert_eq!(cursor(cx), 0, "nothing before the first card");
    cx.simulate_keystrokes("right");
    assert_eq!(cursor(cx), 1);
    for _ in 1..columns {
        cx.simulate_keystrokes("right");
    }
    assert_eq!(
        cursor(cx),
        columns,
        "→ at a line's end goes to the next line"
    );
    cx.simulate_keystrokes("left");
    assert_eq!(
        cursor(cx),
        columns - 1,
        "← at a line's start goes to the line before"
    );

    // Up and down by a line, staying in the column.
    cx.simulate_keystrokes("home down");
    assert_eq!(cursor(cx), columns);
    cx.simulate_keystrokes("down");
    assert_eq!(cursor(cx), 2 * columns);
    cx.simulate_keystrokes("up up");
    assert_eq!(cursor(cx), 0);
    cx.simulate_keystrokes("up");
    assert_eq!(cursor(cx), 0, "above the first line it stays");

    // ↓ onto the short last line, from a column it does not reach: the last card.
    let last_line = (40 / columns) * columns;
    cards.update(cx, |cards, cx| cards.set_cursor(last_line - 1, cx));
    cx.simulate_keystrokes("down");
    assert_eq!(cursor(cx), 40, "the last card");
    cx.simulate_keystrokes("down right");
    assert_eq!(cursor(cx), 40, "nothing below or after the last card");
    cx.simulate_keystrokes("home");
    assert_eq!(cursor(cx), 0);
    cx.simulate_keystrokes("end");
    assert_eq!(cursor(cx), 40);
    cx.simulate_keystrokes("home ctrl-shift-e");
    assert_eq!(cursor(cx), 40, "a key bound in the gallery's context acts");
    cx.simulate_keystrokes("home");

    cx.simulate_keystrokes("x down x");
    cards.read_with(cx, |cards, _| {
        let picked: Vec<&str> = cards.picks.iter().map(String::as_str).collect();
        assert_eq!(picked, ["n000", cards.rows[columns].id.as_str()]);
    });
    cx.simulate_keystrokes("cmd-a");
    cards.read_with(cx, |cards, _| assert_eq!(cards.picks.len(), 41));
    cx.simulate_keystrokes("escape");
    cards.read_with(cx, |cards, _| assert!(cards.picks.is_empty()));

    // A size change flows the columns again: every card keeps its preview, and only the covers
    // it brings into view are read, each once.
    cx.simulate_keystrokes("- -");
    settle(cx);
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.format.size, CardSize::Small);
        assert!(
            cards.card_grid().columns() > columns,
            "narrower columns, more of them"
        );
        assert_eq!(cards.placed(), 41);
    });
    cx.simulate_keystrokes("= =");
    settle(cx);
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.format.size, CardSize::Large);
        assert!(cards.card_grid().columns() < columns);
    });
    let reads = blobs.reads.lock().unwrap().clone();
    let once: HashSet<&String> = reads.iter().collect();
    assert_eq!(once.len(), reads.len(), "a cover was read twice: {reads:?}");

    // F goes round the type's files, then no cover, then the first file again.
    let with_cover = cards.read_with(cx, |c, _| c.card_grid().card_height());
    cx.simulate_keystrokes("f");
    cards.read_with(cx, |c, _| assert_eq!(c.cover.as_deref(), Some("scan")));
    cx.simulate_keystrokes("f");
    cards.read_with(cx, |c, _| {
        assert_eq!(c.cover, None, "after the last file, none");
        assert_eq!(c.format.cover, CardCover::None);
        assert!(
            c.card_grid().card_height() < with_cover,
            "no card has the box"
        );
        assert!(c.wanted().is_empty(), "and no file is read");
    });
    cx.simulate_keystrokes("f");
    cards.read_with(cx, |c, _| {
        assert_eq!(c.cover.as_deref(), Some("attachment"));
        assert_eq!(c.format.cover, CardCover::First, "the default again");
        assert_eq!(c.card_grid().card_height(), with_cover);
    });
}

/// A card is placed as soon as its row is read, before any cover is, and keeps its place when
/// covers land: with one file's bytes held, every card is drawn where its index puts it, and
/// none has moved once the bytes come. In place of the gallery's
/// `a_placed_tile_keeps_its_place_when_later_shapes_arrive`, whose tiles waited for the shapes
/// before them.
#[gpui::test]
async fn a_card_is_placed_before_its_cover_and_stays_there(cx: &mut TestAppContext) {
    init(cx);
    let (session, blobs) = session_with_files(NOTES, notes_answering(8), None, cx);
    for i in 0..8 {
        let (w, h) = [(100, 160), (160, 100), (100, 100), (100, 220)][i % 4];
        blobs.set(&format!("Note/n{i:03}/attachment"), &png(w, h, false));
    }
    let held = blobs.hold_reads("Note/n003/attachment");
    let (cards, cx) = open(session, opening("Note"), cx);
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.placed(), 8, "every row read is a card");
        assert!(
            !matches!(cards.covers[&cover_key(3)].look, Loadable::Loaded(_)),
            "its bytes are held"
        );
        assert!(matches!(
            cards.covers[&cover_key(5)].look,
            Loadable::Loaded(Look::Image { .. })
        ));
    });
    let early: Vec<Bounds<Pixels>> = (0..8)
        .map(|ix| card_bounds(ix, cx).expect("drawn before its cover is read"))
        .collect();
    drop(held);
    settle(cx);
    cards.read_with(cx, |cards, _| {
        assert!(matches!(
            cards.covers[&cover_key(3)].look,
            Loadable::Loaded(Look::Image { .. })
        ));
    });
    let late: Vec<Bounds<Pixels>> = (0..8).map(|ix| card_bounds(ix, cx).unwrap()).collect();
    assert_eq!(late, early, "a card moved or changed size");
}

/// The covers of the cards a frame lays out are read and drawn, and no other: nothing is read
/// ahead of them. Reads for cards no longer laid out before they were sent ask nothing. In
/// place of the gallery's `the_tiles_shown_are_drawn_and_the_rows_below_measured`, which also
/// read the first bytes of the rows below to place them.
#[gpui::test]
async fn covers_are_read_for_the_cards_laid_out_and_no_others(cx: &mut TestAppContext) {
    init(cx);
    let (session, blobs) = session_with_files(NOTES, notes_answering(400), None, cx);
    let big = png(300, 300, true);
    assert!(big.len() as u64 > FIRST_READ, "{}", big.len());
    for i in 0..120 {
        blobs.set(&format!("Note/n{i:03}/attachment"), &big);
    }
    let (cards, cx) = open(session, opening("Note"), cx);
    let reads = blobs.reads.lock().unwrap().clone();
    let (shown, laid_out) = cards.read_with(cx, |cards, _| {
        assert_eq!(cards.placed(), cards.rows.len(), "every row read is placed");
        (
            cards
                .card_grid()
                .meeting(cards.span.clone(), cards.placed()),
            cards.laid_out(),
        )
    });
    assert!(!shown.is_empty());
    assert!(
        laid_out.start == 0 && laid_out.end > shown.end,
        "half a screen below is laid out: {shown:?} of {laid_out:?}"
    );
    assert!(laid_out.end < 120, "{laid_out:?}");
    let asked = |row: usize| {
        reads
            .iter()
            .any(|read| read.contains(&format!(" Note/n{row:03}/")))
    };
    let rest = |row: usize| {
        let cell = format!("GET Note/n{row:03}/attachment 65536-");
        reads.iter().any(|read| read.starts_with(&cell))
    };
    let first = |row: usize| {
        let cell = format!("GET Note/n{row:03}/attachment 0-65535");
        reads.contains(&cell)
    };
    for row in laid_out.clone() {
        assert!(first(row) && rest(row), "card {row} drawn: {reads:?}");
    }
    for row in laid_out.end..400 {
        assert!(!asked(row), "row {row}, not laid out, was read");
    }
    cards.read_with(cx, |cards, _| {
        assert!(matches!(
            cards.covers[&cover_key(shown.start)].look,
            Loadable::Loaded(Look::Image { .. })
        ));
        assert!(!cards.covers.contains_key(&cover_key(laid_out.end)));
        assert!(cards.cover_tasks.is_empty(), "every read ended");
    });

    // Asked for and then no longer wanted before they were sent: nothing of theirs is read.
    blobs.reads.lock().unwrap().clear();
    let (near, far) = cards.update(cx, |cards, cx| {
        cards.release_all(cx);
        cards.span = 0.0..400.;
        let near = cards.laid_out();
        cards.read_covers(cx);
        assert_eq!(cards.cover_tasks.len(), READS_AT_ONCE, "four at once");
        let bottom = cards.card_grid().height(cards.placed());
        cards.span = (bottom - 400.)..bottom;
        let far = cards.laid_out();
        cards.read_covers(cx);
        let wanted: HashSet<CoverKey> = cards.wanted().into_iter().collect();
        assert!(cards.cover_tasks.keys().all(|key| wanted.contains(key)));
        (near, far)
    });
    assert!(near.end <= far.start, "{near:?} {far:?}");
    cx.run_until_parked();
    let later = blobs.reads.lock().unwrap().clone();
    assert!(!later.is_empty());
    for read in &later {
        let id = read.split('/').nth(1).unwrap();
        let row: usize = id.trim_start_matches('n').parse().unwrap();
        assert!(
            far.contains(&row),
            "a read for a card scrolled away was sent: {read}"
        );
    }
}

/// Pages are read as the cards laid out near the end of the rows read, at the first page's
/// commit, and no further: a set of 5,000 is not read whole until it is scrolled through.
/// Adapted from the gallery's `pages_are_read_as_the_columns_near_the_end`.
#[gpui::test]
async fn pages_are_read_as_the_cards_near_the_end(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(5_000), None, cx);
    let (cards, cx) = open(session, opening("Note"), cx);
    let opened = cards.read_with(cx, |cards, _| {
        assert!(
            cards.rows.len() >= 200,
            "a first page is two of the graph's pages"
        );
        assert!(cards.rows.len() < 5_000, "{} read", cards.rows.len());
        assert!(cards.laid_out().end < cards.rows.len());
        assert!(!cards.paging.exhausted);
        assert_eq!(cards.paging.pinned.as_deref(), Some("c1"));
        assert_eq!(cards.count, Loadable::Loaded(5_000));
        cards.rows.len()
    });
    // Left alone, nothing more is read.
    settle(cx);
    cards.read_with(cx, |cards, _| assert_eq!(cards.rows.len(), opened));
    let mut read = vec![opened];
    for _ in 0..80 {
        scroll_to_end(&cards, cx);
        read.push(cards.read_with(cx, |cards, _| cards.rows.len()));
    }
    assert!(
        read.windows(2).any(|pair| pair[1] > pair[0]),
        "scrolled down, later pages were read: {read:?}"
    );
    cards.read_with(cx, |cards, _| {
        assert_eq!(
            cards.rows.len(),
            5_000,
            "scrolled through, every row was read"
        );
        assert!(cards.paging.exhausted);
        assert_eq!(cards.count_text(), "5,000 rows");
        let ids: HashSet<&str> = cards.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids.len(), 5_000, "no row twice");
    });
}

/// A frame lays out the cards of the lines near the view and no others, whatever is read: with
/// 5,000 rows read, at the end, in the middle and back at the start, it builds the cards of
/// the lines meeting the view and half a screen either side.
#[gpui::test]
async fn a_frame_lays_out_only_the_lines_near_the_view(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(5_000), None, cx);
    let (cards, cx) = open(session, opening("Note"), cx);
    for _ in 0..80 {
        scroll_to_end(&cards, cx);
    }
    let check = |cx: &mut VisualTestContext, at: &str| {
        let (laid_out, drawn, shown, columns) = cards.read_with(cx, |cards, _| {
            assert_eq!(cards.placed(), 5_000, "{at}");
            let grid = cards.card_grid();
            (
                cards.laid_out(),
                cards.laid_out_last,
                grid.meeting(cards.span.clone(), 5_000),
                grid.columns(),
            )
        });
        assert_eq!(
            drawn,
            laid_out.len(),
            "{at}: the frame built what is laid out"
        );
        assert!(!shown.is_empty(), "{at}");
        // The view's lines, and at most half a screen of lines either side.
        let lines = |cards: usize| cards.div_ceil(columns);
        assert!(
            lines(drawn) <= 2 * lines(shown.len()) + 2,
            "{at}: {drawn} cards for {} shown",
            shown.len()
        );
        assert!(drawn < 200, "{at}: {drawn} of 5,000 laid out");
        assert!(card_bounds(laid_out.start, cx).is_some(), "{at}");
        assert!(card_bounds(laid_out.end - 1, cx).is_some(), "{at}");
        for far in [laid_out.start.checked_sub(1), Some(laid_out.end)] {
            if let Some(far) = far.filter(|&far| far < 5_000) {
                assert!(card_bounds(far, cx).is_none(), "{at}: card {far} was built");
            }
        }
        laid_out
    };
    let end = check(cx, "at the end");
    assert_eq!(end.end, 5_000);
    assert!(end.start > 4_800);
    cards.update(cx, |cards, cx| {
        let middle = cards.card_grid().height(cards.placed()) / 2.;
        cards.scroll.set_offset(point(px(0.), px(-middle)));
        cx.notify();
    });
    settle(cx);
    let middle = check(cx, "in the middle");
    assert!(middle.start > 2_000 && middle.end < 3_000, "{middle:?}");
    cx.simulate_keystrokes("home");
    settle(cx);
    let start = check(cx, "at the start");
    assert_eq!(start.start, 0);
}

/// Cards of rows picked in a table hold them alone, in the order they were picked, read by id.
#[gpui::test]
async fn cards_of_listed_rows_hold_them_in_their_order(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(20), None, cx);
    let listed = ShowSetInCards {
        ids: Some(vec!["n007".into(), "n002".into(), "n015".into()]),
        select: Some("n002".into()),
        ..opening("Note")
    };
    let (cards, cx) = open(session, listed, cx);
    cards.read_with(cx, |cards, _| {
        let ids: Vec<&str> = cards.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["n007", "n002", "n015"]);
        assert_eq!(cards.cursor, Some(1), "the table's cursor row");
        assert!(cards.paging.exhausted);
        assert_eq!(cards.placed(), 3);
        assert_eq!(cards.count_text(), "3 rows");
    });
}

/// Past the cap, the previews held farthest from the span shown are let go, a preview of a
/// file that is no longer the cover first; those of the cards laid out stay, even past the
/// cap. Adapted from the gallery's test of the same name, which also held the first bytes of
/// rows measured ahead.
#[gpui::test]
async fn previews_past_the_cap_are_let_go_farthest_first(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(2_000), None, cx);
    let (cards, cx) = open(session, opening("Note"), cx);
    cards.update(cx, |cards, cx| {
        let laid_out = cards.laid_out();
        let near = laid_out.start;
        let far = cards.placed() - 1;
        assert!(!laid_out.contains(&far));
        let other = (format!("n{near:03}"), "scan".to_owned());
        let each = PREVIEWS_HELD / 3;
        for key in [cover_key(near), cover_key(far), other.clone()] {
            let image = crate::blob::ShownImage::from_rgba(1, 1, vec![0; 4]).unwrap();
            let cover = cards.covers.entry(key).or_default();
            cover.look = Loadable::Loaded(Look::Image { image, bytes: each });
        }
        cards.held = 3 * each + each / 2;
        cards.let_go_past_cap(cx);
        let held = |cards: &TypeCards, key: &CoverKey| cards.covers[key].held();
        assert_eq!(
            held(cards, &other),
            0,
            "a file that is not the cover goes first"
        );
        assert!(held(cards, &cover_key(near)) > 0 && held(cards, &cover_key(far)) > 0);
        cards.held = 2 * PREVIEWS_HELD;
        cards.let_go_past_cap(cx);
        assert!(
            held(cards, &cover_key(near)) > 0,
            "a card laid out keeps its preview"
        );
        assert_eq!(held(cards, &cover_key(far)), 0);
        assert!(
            cards.needs(&cover_key(far)),
            "it is read again when its card comes back"
        );
    });
}

/// On a branch, nothing is placed until the branch's diff is in, so the rows it removed come
/// first without moving a placed card; cards are marked as the branch changed their rows
/// (spec 033), the removed row's file read where it last was.
#[gpui::test]
async fn a_branch_marks_its_cards_and_puts_removed_rows_first(cx: &mut TestAppContext) {
    init(cx);
    let note_type = json!({ "id": "Note", "name": "Note" });
    let changes = json!([
        { "kind": "node", "type": note_type, "id": "n000", "op": "insert",
          "before": null, "after": { "properties": { "slug": "n000", "title": "Note 0" } } },
        { "kind": "node", "type": note_type, "id": "n001", "op": "update",
          "before": { "properties": { "slug": "n001", "title": "Note 1", "attachment": "base64:AAA=" } },
          "after": { "properties": { "slug": "n001", "title": "Note 1", "attachment": "base64:AQE=" } } },
        { "kind": "node", "type": note_type, "id": "n900", "op": "delete",
          "before": { "properties": { "slug": "n900", "title": "Gone" } }, "after": null },
    ]);
    let (session, blobs) = session_with_files(NOTES, notes_answering(3), Some(changes), cx);
    blobs.set("Note/n900/attachment", b"what it held");
    cx.run_until_parked();
    session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
    let (cards, cx) = open(session, opening("Note"), cx);
    cards.read_with(cx, |cards, _| {
        let ids: Vec<&str> = cards.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["n900", "n000", "n001", "n002"]);
        assert_eq!(cards.placed(), 4);
        assert_eq!(cards.change_of("n000"), Some(NetChange::Added));
        assert_eq!(cards.change_of("n001"), Some(NetChange::Changed));
        assert!(cards.file_changed("n001"));
        assert!(!cards.file_changed("n000"));
        assert!(cards.is_removed("n900"));
        assert_eq!(cards.diff.as_ref().unwrap().counts(), (1, 1, 1));
        assert_eq!(
            cards.selected_row_id(),
            Some("n000"),
            "the cursor stays on its card"
        );
        assert_eq!(cards.diff_base.as_deref(), Some("c0"));
    });
    // The removed row's card is the first: the top-left one.
    let removed = card_bounds(0, cx).expect("drawn");
    for ix in 1..4 {
        let bounds = card_bounds(ix, cx).expect("drawn");
        assert!(bounds.origin.x > removed.origin.x || bounds.origin.y > removed.origin.y);
    }
    assert!(
        blobs
            .reads
            .lock()
            .unwrap()
            .iter()
            .any(|read| read.starts_with("HEAD Note/n900/attachment")),
    );
    cards.update(cx, |cards, cx| cards.pick(0, false, cx));
    cards.read_with(cx, |cards, _| assert!(cards.picks.is_empty()));
    // While a branch's diff is out, nothing is placed, whatever is read.
    cards.update(cx, |cards, _| {
        cards.diff_pending = true;
        assert_eq!(cards.placed(), 0);
        assert!(cards.laid_out().is_empty() && cards.wanted().is_empty());
        cards.diff_pending = false;
    });
}

/// The cards are a cached view of their own: a key that moves the cursor draws them once, and
/// the covers of a screen landing together draw them once, not once a card
/// (`docs/drawing.md`).
#[gpui::test]
async fn a_key_draws_the_cards_once_and_covers_land_together(cx: &mut TestAppContext) {
    init(cx);
    let (session, blobs) = session_with_files(NOTES, notes_answering(40), None, cx);
    for i in 0..40 {
        blobs.set(
            &format!("Note/n{i:03}/attachment"),
            format!("text {i}").as_bytes(),
        );
    }
    let (cards, cx) = open(session, opening("Note"), cx);
    // The first key switches GPUI to keyboard input, which draws the window once.
    cx.simulate_keystrokes("right");
    cx.run_until_parked();
    for key in ["down", "up", "left", "right", "end", "home"] {
        let before = grid_renders(&cards, cx);
        let cursor = cards.read_with(cx, |cards, _| cards.cursor);
        cx.simulate_keystrokes(key);
        cx.run_until_parked();
        assert_ne!(
            cards.read_with(cx, |cards, _| cards.cursor),
            cursor,
            "{key}"
        );
        assert_eq!(grid_renders(&cards, cx), before + 1, "one draw for {key}");
    }

    // Every cover read again: those of the cards laid out land, and the grid is drawn once for
    // them.
    cards.update(cx, |cards, cx| {
        for cover in cards.covers.values_mut() {
            cover.look = Loadable::NotLoaded;
        }
        cards.read_covers(cx);
    });
    let before = grid_renders(&cards, cx);
    cx.run_until_parked();
    let landed = cards.read_with(cx, |cards, _| {
        assert!(cards.cover_tasks.is_empty());
        cards.laid_out().len()
    });
    assert_eq!(grid_renders(&cards, cx), before, "not drawn as they land");
    cx.executor().advance_clock(LANDING_DELAY);
    cx.run_until_parked();
    let drew = grid_renders(&cards, cx) - before;
    assert!(landed > 4, "{landed} cards laid out");
    assert_eq!(drew, 1, "{drew} draws for {landed} covers landing together");
}

/// ↵ and ⌘↵ open the page of the node under the cursor, ⌥↵ its cover's file in a Blob tab at
/// the commit read, and G the canvas the strip's Canvas opens (spec 020: ↵ opens a node's page
/// and G its canvas wherever a node is focused). The gallery opened the file with ↵.
#[gpui::test]
async fn enter_opens_the_page_alt_enter_the_cover_and_g_the_canvas(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(12), None, cx);
    let heard: Rc<RefCell<Vec<String>>> = Rc::default();
    cx.update(|cx| {
        let log = heard.clone();
        cx.on_action(move |open: &OpenNodePage, _| {
            log.borrow_mut()
                .push(format!("page {} {}", open.type_name, open.id));
        });
        let log = heard.clone();
        cx.on_action(move |open: &OpenBlob, _| {
            log.borrow_mut().push(format!(
                "file {} {} {} at {:?} held {}",
                open.type_name, open.id, open.property, open.at_commit, open.held
            ));
        });
        let log = heard.clone();
        cx.on_action(move |show: &ShowNodeOnCanvas, _| {
            log.borrow_mut().push(format!(
                "canvas {} {} preview {}",
                show.type_name, show.id, show.preview
            ));
        });
    });
    let (cards, cx) = open(session, opening("Note"), cx);
    let said = |cx: &mut VisualTestContext| {
        cx.run_until_parked();
        std::mem::take(&mut *heard.borrow_mut())
    };
    cx.simulate_keystrokes("right enter");
    assert_eq!(said(cx), ["page Note n001"]);
    cx.simulate_keystrokes("cmd-enter");
    assert_eq!(said(cx), ["page Note n001"]);
    cx.simulate_keystrokes("alt-enter");
    assert_eq!(
        said(cx),
        ["file Note n001 attachment at Some(\"c1\") held false"]
    );
    cx.simulate_keystrokes("g");
    assert_eq!(said(cx), ["canvas Note n001 preview true"]);
    // The same action the strip's Canvas dispatches.
    cards.read_with(cx, |cards, cx| {
        let strip = Item::view_action(cards, ViewKind::Canvas, cx).expect("a canvas to open");
        assert!(strip.partial_eq(&ShowNodeOnCanvas {
            type_name: "Note".into(),
            id: "n001".into(),
            at_commit: None,
            preview: true,
        }));
    });
    // The cover F chose is the file ⌥↵ opens.
    cx.simulate_keystrokes("f alt-enter");
    assert_eq!(said(cx), ["file Note n001 scan at Some(\"c1\") held false"]);
}

/// A click puts the cursor on a card; a second click opens its page, as ↵ does; ⌘-click picks
/// the card and ⇧-click from the last card picked to it.
#[gpui::test]
async fn a_click_selects_and_a_second_opens_the_page(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(12), None, cx);
    let opened: Rc<RefCell<Vec<String>>> = Rc::default();
    cx.update(|cx| {
        let log = opened.clone();
        cx.on_action(move |open: &OpenNodePage, _| log.borrow_mut().push(open.id.clone()));
        let log = opened.clone();
        cx.on_action(move |open: &OpenBlob, _| log.borrow_mut().push(format!("file {}", open.id)));
    });
    let (cards, cx) = open(session, opening("Note"), cx);
    let at = |ix: usize, cx: &mut VisualTestContext| card_bounds(ix, cx).unwrap().center();
    let position = at(2, cx);
    cx.simulate_click(position, Modifiers::none());
    cards.read_with(cx, |cards, _| assert_eq!(cards.cursor, Some(2)));
    assert!(opened.borrow().is_empty(), "one click opens nothing");
    // The second click of a double click.
    cx.simulate_event(MouseDownEvent {
        position,
        modifiers: Modifiers::none(),
        button: MouseButton::Left,
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_event(MouseUpEvent {
        position,
        modifiers: Modifiers::none(),
        button: MouseButton::Left,
        click_count: 2,
    });
    cx.run_until_parked();
    assert_eq!(*opened.borrow(), ["n002"], "the page, not the file");

    let position = at(4, cx);
    let secondary = cx.update(|_, cx| Modifiers::secondary_key(cx));
    cx.simulate_click(position, secondary);
    let position = at(1, cx);
    cx.simulate_click(position, Modifiers::shift());
    cards.read_with(cx, |cards, _| {
        let mut picked: Vec<&str> = cards.picks.iter().map(String::as_str).collect();
        picked.sort_unstable();
        assert_eq!(picked, ["n001", "n002", "n003", "n004"]);
        assert_eq!(cards.cursor, Some(1));
    });
    assert_eq!(opened.borrow().len(), 1, "a pick opens nothing");
}

/// A tab the gallery saved comes back as Cards (spec 048): its kind `gallery` is still
/// registered, to the opener Cards has, and its data is read as it was written, so the tab
/// holds the same set, shows that file as its cover and has that size. Saved again, it is a
/// `cards` tab. A kind nothing registers is dropped, which is why the old one is kept. A tab
/// saved by Cards before a card had a format (its cover and size beside the set) comes back
/// with that cover and size too.
#[gpui::test]
async fn a_saved_gallery_tab_comes_back_as_cards(cx: &mut TestAppContext) {
    init(cx);
    cx.update(crate::init);
    let (session, _) = session_with_files(NOTES, notes_answering(5), None, cx);
    cx.run_until_parked();
    let (workspace, cx) = cx.add_window_view(Workspace::new);
    workspace.update_in(cx, |workspace, window, cx| {
        // The tabs another test of this binary left open for the graph are not this one's.
        workspace.start_with_no_saved_tabs();
        workspace.set_session(session.clone(), window, cx)
    });
    cx.run_until_parked();
    // As `TypeGallery::serialize` wrote them (spec 035, Studio 0.4.12): the action that opened
    // the gallery, its file as `property`, and the tile size.
    let filtered = SavedItem {
        kind: "gallery".into(),
        data: json!({
            "title": "Note, design",
            "type_name": "Note",
            "filters": [{ "property": "title", "op": "Contains", "value": "design" }],
            "sort": { "property": "title", "descending": true },
            "ids": null,
            "property": "scan",
            "at_commit": null,
            "snapshot": null,
            "select": null,
            "preview": false,
            "size": "large",
        }),
    };
    let listed = SavedItem {
        kind: "gallery".into(),
        data: json!({
            "title": "Note, 2 selected rows",
            "type_name": "Note",
            "filters": null,
            "sort": null,
            "ids": ["n003", "n001"],
            "property": "attachment",
            "at_commit": "c0",
            "snapshot": null,
            "select": null,
            "preview": false,
            "size": "small",
        }),
    };
    let reopen = |item: &SavedItem, cx: &mut VisualTestContext| {
        let opened = workspace.update_in(cx, |workspace, window, cx| {
            workspace.reopen_saved(item, window, cx)
        });
        settle(cx);
        let cards = workspace.read_with(cx, |workspace, _| {
            workspace
                .active_item()
                .and_then(|item| item.to_any().downcast::<TypeCards>().ok())
        });
        (opened, cards)
    };
    let (opened, cards) = reopen(&filtered, cx);
    assert!(opened, "the kind `gallery` is registered");
    let cards = cards.expect("a Cards tab is open and active");
    let saved = cards.read_with(cx, |cards, cx| {
        assert_eq!(Item::tab_title(cards, cx), "Cards: Note, design");
        assert_eq!(Item::view_kind(cards, cx), Some(ViewKind::Cards));
        assert_eq!(
            cards.cover.as_deref(),
            Some("scan"),
            "that file is the cover"
        );
        assert_eq!(cards.format.size, CardSize::Large, "that size");
        assert_eq!(
            cards.format,
            CardFormat {
                fields: None,
                cover: CardCover::Property("scan".into()),
                size: CardSize::Large,
            },
            "and the default fields"
        );
        assert_eq!(
            cards.filters,
            [Filter {
                property: "title".into(),
                op: gq_language::FilterOp::Contains,
                value: json!("design"),
            }]
        );
        assert_eq!(
            cards.sort,
            Some(Sort {
                property: "title".into(),
                descending: true,
            })
        );
        assert_eq!((cards.ids.as_ref(), cards.at_commit.as_ref()), (None, None));
        assert_eq!(cards.rows.len(), 5, "and it read its set");
        Item::serialize(cards, cx).expect("it is saved")
    });
    assert_eq!(saved.kind, "cards", "saved as Cards from now on");
    assert_eq!(
        saved.data["format"],
        json!({ "cover": { "property": "scan" }, "size": "large" }),
        "with its format"
    );
    for old in ["property", "cover", "size"] {
        assert_eq!(saved.data.get(old), None, "`{old}` is the format's now");
    }
    assert_eq!(saved.data["filters"], filtered.data["filters"]);
    assert_eq!(saved.data["sort"], filtered.data["sort"]);
    // What it saves opens the tab it is, not another.
    let (opened, again) = reopen(&saved, cx);
    assert!(opened, "the kind `cards` is registered");
    assert_eq!(
        again.map(|cards| cards.entity_id()),
        Some(cards.entity_id())
    );
    workspace.read_with(cx, |workspace, _| assert_eq!(workspace.items().len(), 1));

    let (opened, cards) = reopen(&listed, cx);
    assert!(opened);
    let cards = cards.expect("a second Cards tab");
    workspace.read_with(cx, |workspace, _| assert_eq!(workspace.items().len(), 2));
    cards.read_with(cx, |cards, cx| {
        assert_eq!(
            Item::tab_title(cards, cx),
            "Cards: Note, 2 selected rows @ c0"
        );
        assert_eq!(cards.cover.as_deref(), Some("attachment"));
        assert_eq!(cards.format.size, CardSize::Small);
        assert_eq!(
            cards.ids,
            Some(vec!["n003".to_owned(), "n001".to_owned()]),
            "the same rows, in their order"
        );
        assert_eq!(cards.at_commit.as_deref(), Some("c0"), "held where it was");
        let ids: Vec<&str> = cards.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["n003", "n001"]);
    });

    // A kind nothing registers opens nothing: a restart drops the tab without a word.
    let unknown = SavedItem {
        kind: "gallery-of-another-app".into(),
        data: filtered.data.clone(),
    };
    let (opened, _) = reopen(&unknown, cx);
    assert!(!opened);
    workspace.read_with(cx, |workspace, _| assert_eq!(workspace.items().len(), 2));
    // A tab Cards saved before a card had a format (spec 048's slice 1, in no release): its
    // cover and its size beside the set. That cover, that size.
    let slice_one = SavedItem {
        kind: "cards".into(),
        data: json!({
            "title": "Note, one row",
            "type_name": "Note",
            "filters": null,
            "sort": null,
            "ids": ["n002"],
            "cover": "scan",
            "at_commit": null,
            "snapshot": null,
            "select": null,
            "preview": false,
            "size": "small",
        }),
    };
    let (opened, cards) = reopen(&slice_one, cx);
    assert!(opened);
    let cards = cards.expect("a third Cards tab");
    workspace.read_with(cx, |workspace, _| assert_eq!(workspace.items().len(), 3));
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.cover.as_deref(), Some("scan"));
        assert_eq!(
            cards.format,
            CardFormat {
                fields: None,
                cover: CardCover::Property("scan".into()),
                size: CardSize::Small,
            }
        );
        assert_eq!(cards.rows.len(), 1);
    });
    // The data alone, as the opener reads it: an old tab's cover and size are its format.
    let (opening, format) = crate::saved_cards(&filtered.data).expect("read");
    assert_eq!(opening.cover, None, "the format's, not the set's");
    assert_eq!(
        format,
        Some(CardFormat {
            fields: None,
            cover: CardCover::Property("scan".into()),
            size: CardSize::Large,
        })
    );
    let (_, bare) = crate::saved_cards(&json!({ "title": "Note", "type_name": "Note" })).unwrap();
    assert_eq!(bare, Some(CardFormat::default()), "an old tab with neither");
    assert_eq!(crate::CARDS_KIND, "cards");
    assert_eq!(crate::GALLERY_KIND, "gallery");
}

/// A set whose rows all lack the sort's value: the first page, of rows with one, is empty, and
/// the cards read on to the rows without one rather than saying there are none. Review of
/// #144: before, `wants_more` refused with no row read and the gallery said "No rows".
#[gpui::test]
async fn an_empty_page_that_is_not_the_last_reads_on(cx: &mut TestAppContext) {
    init(cx);
    let schema = NOTES.replace("  title: String\n", "  title: String\n  written: Date?\n");
    let (session, _) = session_with_files(
        &schema,
        |request: &Value| {
            let text = request["query"].as_str().unwrap_or_default();
            match request["name"].as_str() {
                // The rows with a date: none. The rows without one: ten.
                Some("studio_page") if text.contains("is not null") => json!([]),
                Some("studio_page") => Value::from((0..10).map(note).collect::<Vec<_>>()),
                Some("studio_count") => json!([{ "n": 10 }]),
                _ => json!([]),
            }
        },
        None,
        cx,
    );
    let sorted = ShowSetInCards {
        sort: Some(json!({ "property": "written", "descending": false })),
        ..opening("Note")
    };
    let (cards, cx) = open(session, sorted, cx);
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.rows.len(), 10, "the rows without a date were read");
        assert_eq!(cards.cursor, Some(0));
        assert_eq!(cards.placed(), 10);
    });
}

/// A listed set whose first batch is gone at the commit reads the next batch.
#[gpui::test]
async fn a_listed_batch_of_rows_gone_reads_the_next(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(20), None, cx);
    let mut ids: Vec<String> = (900..1100).map(|i| format!("n{i:03}")).collect();
    ids.extend(["n003".to_owned(), "n001".to_owned()]);
    let listed = ShowSetInCards {
        ids: Some(ids),
        ..opening("Note")
    };
    let (cards, cx) = open(session, listed, cx);
    cards.read_with(cx, |cards, _| {
        let ids: Vec<&str> = cards.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["n003", "n001"]);
        assert_eq!(
            cards.count_text(),
            "2 rows",
            "the rows gone are not counted"
        );
    });
}

/// A batch of listed rows that failed is read again by Retry: the same ids, in their order.
/// Before, the list had moved past them when the batch was asked for, and Retry asked for the
/// next one.
#[gpui::test]
async fn retry_reads_the_listed_batch_that_failed(cx: &mut TestAppContext) {
    init(cx);
    let asked: Arc<std::sync::Mutex<Vec<Vec<String>>>> = Arc::default();
    let refused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let answer = notes_answering(300);
    let (log, once) = (asked.clone(), refused.clone());
    let (session, _) = session_with_files(
        NOTES,
        move |request: &Value| {
            let ids: Vec<String> = request["params"]["ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect();
            if request["name"] == "studio_nodes_by_id" {
                log.lock().unwrap().push(ids.clone());
                if ids.first().map(String::as_str) != Some("n000")
                    && !once.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    return json!({ "refuse": "the server was busy" });
                }
            }
            answer(request)
        },
        None,
        cx,
    );
    let listed = ShowSetInCards {
        ids: Some((0..300).map(|i| format!("n{i:03}")).collect()),
        ..opening("Note")
    };
    let (cards, cx) = open(session, listed, cx);
    // Scrolled to the cards' end, the second batch is asked for, and refused.
    for _ in 0..4 {
        scroll_to_end(&cards, cx);
    }
    cards.read_with(cx, |cards, _| {
        assert!(cards.more_error.is_some(), "the second batch failed");
        assert_eq!(cards.rows.len(), 200);
    });
    // The status bar offers Retry for the page, as the table's does.
    let offered = |cx: &mut VisualTestContext| {
        cards.update_in(cx, |cards, window, cx| {
            Item::status(cards, window, cx).map_or(0, |status| status.start.len())
        })
    };
    assert_eq!(offered(cx), 1, "Retry, and nothing being read");
    cards.update(cx, |cards, cx| cards.retry_page(cx));
    settle(cx);
    assert_eq!(offered(cx), 0, "read, nothing to retry");
    let asked = asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 3, "{asked:?}");
    assert_eq!(asked[2], asked[1], "Retry asked for the batch that failed");
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.rows.len(), 300);
        let ids: Vec<&str> = cards.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids[200], "n200", "in their order");
    });
}

/// Cards held at a commit made before a property was added read their rows without it, as the
/// table does, listed or not. Before, the refusal was the gallery's error.
#[gpui::test]
async fn a_commit_before_a_property_was_added_reads_the_rest(cx: &mut TestAppContext) {
    init(cx);
    let schema = NOTES.replace(
        "  title: String\n",
        "  title: String\n  category: String?\n",
    );
    let fields = |i: usize| json!({ "id": format!("n{i:03}"), "v_slug": format!("n{i:03}"), "v_title": format!("Note {i}") });
    let (session, _) = session_with_files(
        &schema,
        move |request: &Value| match request["name"].as_str() {
            Some("studio_page") | Some("studio_nodes_by_id") => json!({
                "refuse": "property 'category' of node type 'Note' does not exist at this snapshot"
            }),
            Some("studio_page_fields") => Value::from((0..3).map(fields).collect::<Vec<_>>()),
            // The fake server answers a read of nodes' fields in its shape from `{"n": …}` rows.
            Some("studio_nodes_fields") => Value::from((0..2).map(note).collect::<Vec<_>>()),
            _ => json!([]),
        },
        None,
        cx,
    );
    let held = ShowSetInCards {
        at_commit: Some("c0".into()),
        ..opening("Note")
    };
    let (cards, cx) = open(session.clone(), held.clone(), cx);
    cards.read_with(cx, |cards, _| {
        assert!(
            matches!(cards.first_page, Loadable::Loaded(())),
            "{:?}",
            cards.first_page
        );
        assert_eq!(cards.rows.len(), 3);
        assert_eq!(cards.rows[0].value("title"), &json!("Note 0"));
        assert!(cards.absent.contains_key("category"));
    });
    let listed = ShowSetInCards {
        ids: Some(vec!["n000".into(), "n001".into()]),
        ..held
    };
    let (cards, cx) = open(session, listed, cx);
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.rows.len(), 2, "{:?}", cards.first_page);
    });
}

/// Opened at a row past the first page (the table's cursor), the cards read on to it, put the
/// cursor there once it is read, and scroll to it once the grid is measured. Before, the row
/// was looked for in the first page alone, and the cursor fell to the first tile.
#[gpui::test]
async fn the_cursor_opens_on_its_row_past_the_first_page(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(500), None, cx);
    let at = ShowSetInCards {
        select: Some("n350".into()),
        ..opening("Note")
    };
    let (cards, cx) = open(session, at, cx);
    let cursor = cards.read_with(cx, |cards, _| {
        let cursor = cards.cursor.expect("a cursor");
        assert_eq!(cards.rows[cursor].id, "n350");
        assert!(cursor < cards.placed(), "its card is placed");
        assert!(!cards.scroll_owed, "and was scrolled to");
        assert!(-f32::from(cards.scroll.offset().y) > 0.);
        assert!(cards.laid_out().contains(&cursor));
        cursor
    });
    // Whole in the view: inside the window, under the toolbar.
    let card = card_bounds(cursor, cx).expect("the cursor's card is drawn");
    let view = cards.read_with(cx, |cards, _| cards.scroll.bounds());
    assert!(
        card.top() >= view.top() && card.bottom() <= view.bottom(),
        "{card:?} in {view:?}"
    );
}

/// On a branch, cards opened at a row the branch removed put their cursor there once the diff
/// puts the row first.
#[gpui::test]
async fn the_cursor_opens_on_a_removed_row_once_it_is_placed(cx: &mut TestAppContext) {
    init(cx);
    let note_type = json!({ "id": "Note", "name": "Note" });
    let changes = json!([
        { "kind": "node", "type": note_type, "id": "n900", "op": "delete",
          "before": { "properties": { "slug": "n900", "title": "Gone" } }, "after": null },
    ]);
    let (session, _) = session_with_files(NOTES, notes_answering(3), Some(changes), cx);
    cx.run_until_parked();
    session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
    let at = ShowSetInCards {
        select: Some("n900".into()),
        ..opening("Note")
    };
    let (cards, cx) = open(session, at, cx);
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.cursor, Some(0));
        assert_eq!(cards.rows[0].id, "n900");
    });
}

/// A window resized so the columns flow again reads the covers of the cards that now meet the
/// same span. Before, the span read last was the same, and the tiles brought into view stayed
/// unread.
#[gpui::test]
async fn a_resize_reads_the_covers_it_brings_into_view(cx: &mut TestAppContext) {
    init(cx);
    let (session, blobs) = session_with_files(NOTES, notes_answering(120), None, cx);
    let big = png(300, 300, true);
    for i in 0..120 {
        blobs.set(&format!("Note/n{i:03}/attachment"), &big);
    }
    let (cards, cx) = open(session, opening("Note"), cx);
    let (columns, before) = cards.read_with(cx, |c, _| (c.card_grid().columns(), c.laid_out()));
    let size = cx.update(|window, _| window.viewport_size());
    cx.simulate_resize(gpui::size(size.width * 2., size.height));
    settle(cx);
    cards.read_with(cx, |cards, _| {
        assert!(cards.card_grid().columns() > columns, "wider, more columns");
        let laid_out = cards.laid_out();
        assert!(laid_out.end > before.end, "more cards in the same span");
        let unread: Vec<usize> = laid_out
            .filter(|&row| {
                !matches!(
                    cards.covers.get(&cover_key(row)).map(|cover| &cover.look),
                    Some(Loadable::Loaded(_))
                )
            })
            .collect();
        assert!(
            unread.is_empty(),
            "cards in view unread after the resize: {unread:?}"
        );
    });
}

/// A window widened while the cards are scrolled to their end has fewer lines: the frame of
/// the resize draws the last lines, from where the scroll will be held, not an empty stretch
/// past them (GPUI holds the offset within the new range only as it lays the frame out).
#[gpui::test]
async fn a_widened_window_at_the_end_still_draws_the_last_cards(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(150), None, cx);
    let (cards, cx) = open(session, opening("Note"), cx);
    scroll_to_end(&cards, cx);
    let before = cards.read_with(cx, |cards, _| {
        assert_eq!(cards.laid_out().end, 150, "at the end");
        cards.card_grid().height(150)
    });
    let size = cx.update(|window, _| window.viewport_size());
    cx.simulate_resize(gpui::size(size.width * 2., size.height));
    cx.run_until_parked();
    cards.read_with(cx, |cards, _| {
        assert!(cards.card_grid().height(150) < before, "fewer lines");
        assert_eq!(cards.laid_out().end, 150, "still at the end");
        assert!(cards.laid_out_last > 0, "the frame drew cards");
    });
    assert!(card_bounds(149, cx).is_some(), "the last card is drawn");
}

/// A preview made but never drawn holds nothing of GPUI's: an SVG enters the asset cache only
/// when an `img` draws it, and a raster image's texture only when it is painted, which is what
/// `ShownImage::release` frees. So a preview dropped with its cancelled read leaks nothing.
#[gpui::test]
async fn a_preview_never_drawn_holds_nothing_of_gpuis(cx: &mut TestAppContext) {
    let drawing = br#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"/>"#.to_vec();
    let svg = cx.update(|cx| cx.svg_renderer());
    let size = drawing.len() as u64;
    let Look::Image { image, .. } = tile::look(drawing, size, &svg) else {
        panic!("an SVG is drawn");
    };
    let crate::blob::ShownImage::Svg(image) = image else {
        panic!("kept for GPUI to draw");
    };
    cx.update(|cx| assert!(!image.is_asset_cached(cx)));
}

/// On a branch, cards of listed rows read the branch's diff too: they are marked, and a listed
/// row the branch removed is shown first rather than lost.
#[gpui::test]
async fn listed_cards_on_a_branch_mark_their_rows(cx: &mut TestAppContext) {
    init(cx);
    let note_type = json!({ "id": "Note", "name": "Note" });
    let changes = json!([
        { "kind": "node", "type": note_type, "id": "n001", "op": "insert",
          "before": null, "after": { "properties": { "slug": "n001", "title": "Note 1" } } },
        { "kind": "node", "type": note_type, "id": "n900", "op": "delete",
          "before": { "properties": { "slug": "n900", "title": "Gone" } }, "after": null },
        { "kind": "node", "type": note_type, "id": "n901", "op": "delete",
          "before": { "properties": { "slug": "n901", "title": "Not listed" } }, "after": null },
    ]);
    let (session, _) = session_with_files(NOTES, notes_answering(3), Some(changes), cx);
    cx.run_until_parked();
    session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
    let listed = ShowSetInCards {
        ids: Some(vec!["n001".into(), "n900".into(), "n002".into()]),
        ..opening("Note")
    };
    let (cards, cx) = open(session, listed, cx);
    cards.read_with(cx, |cards, _| {
        let ids: Vec<&str> = cards.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["n900", "n001", "n002"]);
        assert_eq!(cards.change_of("n001"), Some(NetChange::Added));
        assert_eq!(cards.count_text(), "3 rows");
    });
}

/// Cards with no row shown yet whose next page failed offer Retry, which reads that page again
/// at the pin, keeping the commit they opened at: before, it read the set from the start at
/// the branch head (Bugbot on #144).
#[gpui::test]
async fn retry_with_no_row_shown_reads_the_page_again_at_the_pin(cx: &mut TestAppContext) {
    init(cx);
    let schema = NOTES.replace("  title: String\n", "  title: String\n  written: Date?\n");
    let asked: Arc<std::sync::Mutex<Vec<(String, Value)>>> = Arc::default();
    let refused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (log, once) = (asked.clone(), refused.clone());
    let (session, _) = session_with_files(
        &schema,
        move |request: &Value| {
            let text = request["query"].as_str().unwrap_or_default().to_owned();
            if request["name"] == "studio_page" {
                log.lock()
                    .unwrap()
                    .push((text.clone(), request["snapshot"].clone()));
            }
            match request["name"].as_str() {
                Some("studio_page") if text.contains("is not null") => json!([]),
                Some("studio_page") if !once.swap(true, std::sync::atomic::Ordering::SeqCst) => {
                    json!({ "refuse": "the server was busy" })
                }
                Some("studio_page") => Value::from((0..4).map(note).collect::<Vec<_>>()),
                _ => json!([]),
            }
        },
        None,
        cx,
    );
    // Opened from a table that read commit c0.
    let sorted = ShowSetInCards {
        sort: Some(json!({ "property": "written", "descending": false })),
        snapshot: Some("c0".into()),
        ..opening("Note")
    };
    let (cards, cx) = open(session, sorted, cx);
    cards.read_with(cx, |cards, _| {
        assert!(cards.rows.is_empty());
        assert!(cards.more_error.is_some(), "the rows without a date failed");
        assert_eq!(cards.paging.pinned.as_deref(), Some("c0"));
    });
    let retry = cx.debug_bounds("cards-retry-page").expect("Retry shown");
    cx.simulate_click(retry.center(), gpui::Modifiers::none());
    settle(cx);
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.rows.len(), 4);
        assert_eq!(cards.paging.pinned.as_deref(), Some("c0"), "still at c0");
    });
    let asked = asked.lock().unwrap().clone();
    let last = asked.last().unwrap();
    assert!(
        last.0.contains("is null"),
        "the failed page asked again: {last:?}"
    );
    assert_eq!(last.1, json!("c0"), "at the pin: {asked:?}");
}

// ---- the format: fields, the cover and where they are kept (spec 048, slice 2) ----

/// A type with every kind of value a field shows, a file, and more properties than a card has
/// room for: eleven that can be fields.
const TASKS: &str = "node Task {\n  id: String @key\n  name: String\n  notes: String?\n  \
    state: enum(open, done)?\n  tags: [String]?\n  due: Date?\n  points: I32?\n  \
    owner: String?\n  link: String?\n  area: String?\n  team: String?\n  shot: Blob?\n}\n";

/// Task `i`: an even one holds a value of every kind, its note far longer than a line and on
/// two lines; an odd one holds none.
fn task(i: usize) -> Value {
    let id = format!("t{i:03}");
    let full = i.is_multiple_of(2);
    let text = |text: &str| if full { json!(text) } else { Value::Null };
    json!({"n": {
        "@id": id, "id": id, "name": format!("Task {i}"),
        "notes": if full {
            json!(format!("{}\nand a second line", "A long note. ".repeat(80)))
        } else {
            Value::Null
        },
        "state": text("open"),
        "tags": if full { json!(["a", "b", "c", "d", "e"]) } else { Value::Null },
        "due": text("2020-02-03"),
        "points": if full { json!(i) } else { Value::Null },
        "owner": text("Ada"), "link": text("https://example.com/a"),
        "area": text("north"), "team": text("core"),
    }})
}

/// Answers the cards' reads of `total` tasks, as [`notes_answering`] does of notes.
fn tasks_answering(total: usize) -> impl Fn(&Value) -> Value + Send + Sync + 'static {
    move |request: &Value| match request["name"].as_str().unwrap_or_default() {
        "studio_count" => json!([{ "n": total }]),
        "studio_page" => {
            let from = request["params"]["after"].as_str().map_or(0, |after| {
                after.trim_start_matches('t').parse::<usize>().unwrap_or(0) + 1
            });
            Value::from((from..total).map(task).collect::<Vec<_>>())
        }
        _ => json!([]),
    }
}

fn field_names(cards: &TypeCards) -> Vec<&str> {
    cards
        .fields
        .iter()
        .map(|property| property.name.as_str())
        .collect()
}

/// The type's format as the settings keep it for the session's graph.
fn kept_format(
    session: &Entity<GraphSession>,
    type_name: &str,
    cx: &mut VisualTestContext,
) -> Option<CardFormat> {
    let graph = session.read_with(cx, |session, _| session.overrides_key());
    cx.update(|_, cx| StudioSettings::get(cx).card_format(&graph, type_name))
}

/// Every card of a tab is one height, whatever its values hold: a text far longer than a line,
/// a list, an enum's member, a date, a number and no value at all are each one line, as the
/// table's cell shows them. The height is the cover's box, the title and a line a field: more
/// fields make every card taller, no cover takes the box from every card, and no file is read
/// then. What a row's fields show is made once, not again each frame.
#[gpui::test]
async fn every_card_is_one_height_with_fields_whatever_they_hold(cx: &mut TestAppContext) {
    init(cx);
    let (session, blobs) = session_with_files(TASKS, tasks_answering(12), None, cx);
    blobs.set("Task/t000/shot", &png(300, 200, false));
    let (cards, cx) = open(session, opening("Task"), cx);
    let line = studio_ui::ds::size::CARD_LINE;
    let heights = |cx: &mut VisualTestContext| {
        let said = cards.read_with(cx, |cards, _| cards.card_grid().card_height());
        for ix in 0..12 {
            let bounds = card_bounds(ix, cx).expect("drawn");
            assert_eq!(f32::from(bounds.size.height), said, "card {ix}");
        }
        said
    };
    // The default card: the first three properties that are not the key, the name or a file.
    let default = cards.read_with(cx, |cards, _| {
        assert_eq!(field_names(cards), ["notes", "state", "tags"]);
        assert_eq!(cards.cover.as_deref(), Some("shot"));
        let now = chrono::Local::now();
        let (full, temporal) = fields_shown(&cards.fields, &cards.rows[0], now);
        let FieldShown::Text(note) = &full[0] else {
            panic!("a text: {full:?}");
        };
        assert!(note.starts_with("A long note. A long note."), "{note}");
        assert!(!note.contains("second line"), "its first line");
        assert!(note.chars().count() <= 512, "and no more than a line shows");
        assert_eq!(
            full[1..],
            [
                FieldShown::Member("open".into()),
                FieldShown::Text("a, b, c +2".into())
            ]
        );
        assert!(!temporal);
        let (empty, _) = fields_shown(&cards.fields, &cards.rows[1], now);
        assert_eq!(empty, vec![FieldShown::Null; 3]);
        let width = cards.card_grid().width();
        assert_eq!(cards.card_grid().card_height(), card_height(width, true, 3));
        cards.card_grid().card_height()
    });
    assert_eq!(heights(cx), default);
    assert!(
        blobs
            .reads
            .lock()
            .unwrap()
            .iter()
            .any(|read| read.starts_with("HEAD Task/t000/shot")),
        "the cover is read"
    );

    // What a row shows is made once: a key draws the cards again from what was made.
    let made = |cx: &mut VisualTestContext| {
        cards.read_with(cx, |cards, _| cards.field_text.borrow()["t000"].clone())
    };
    let first = made(cx);
    cx.simulate_keystrokes("right left");
    assert!(Rc::ptr_eq(&first, &made(cx)), "formatted again for a key");

    // Eight fields: a date, a number and plain text among them, each a line more.
    cards.update(cx, |cards, cx| {
        for name in ["due", "points", "owner", "link", "area"] {
            cards.show_field(name, cx);
        }
        assert_eq!(cards.fields.len(), 8);
        let now = chrono::Local::now();
        let (full, temporal) = fields_shown(&cards.fields, &cards.rows[2], now);
        assert_eq!(
            full[3..5],
            [
                FieldShown::Text("2020-02-03".into()),
                FieldShown::Text("2".into())
            ]
        );
        assert!(temporal, "a date is made again in a later minute");
    });
    cx.run_until_parked();
    assert_eq!(heights(cx), default + 5. * line);
    assert!(!Rc::ptr_eq(&first, &made(cx)), "other fields: made again");

    // No cover: no card has the box, and no file is read.
    blobs.reads.lock().unwrap().clear();
    cards.update(cx, |cards, cx| cards.show_cover(None, cx));
    settle(cx);
    let bare = heights(cx);
    cards.read_with(cx, |cards, _| {
        let width = cards.card_grid().width();
        assert_eq!(bare, card_height(width, false, 8));
        assert!(cards.wanted().is_empty() && cards.cover_tasks.is_empty());
    });
    scroll_to_end(&cards, cx);
    assert!(blobs.reads.lock().unwrap().is_empty(), "a file was read");
}

/// The Fields menu's choices and what each does: a property chosen is shown last, a field
/// hidden leaves, one moved swaps with its neighbour; the ninth is refused and the menu says a
/// card is full; reset shows the default card at the size the tab has. The menu stays open
/// while it is used, and lists the card as it is now.
#[gpui::test]
async fn the_fields_menu_shows_hides_moves_and_resets(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(TASKS, tasks_answering(6), None, cx);
    let (cards, cx) = open(session, opening("Task"), cx);
    let choices =
        |cx: &mut VisualTestContext| cards.read_with(cx, |cards, _| cards.field_choices());
    assert_eq!(
        choices(cx),
        FieldChoices {
            shown: ["notes", "state", "tags"].map(String::from).to_vec(),
            hidden: [
                "id", "name", "due", "points", "owner", "link", "area", "team"
            ]
            .map(String::from)
            .to_vec(),
            full: false,
            files: vec!["shot".into()],
            cover: Some("shot".into()),
            changed: false,
        },
        "the fields in card order, the rest in schema order, files apart"
    );

    // The menu, open: a property not shown is shown last when it is chosen, and the menu is
    // still open, built again with it.
    // Opened as its chip opens it: outside the tab's own update, since it reads the tab.
    let menu = cards.read_with(cx, |cards, _| cards.fields_menu.clone());
    cx.update(|window, cx| menu.show(window, cx));
    cx.run_until_parked();
    let row = cx.debug_bounds("MENU_ITEM-due").expect("due is offered");
    cx.simulate_click(row.center(), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(choices(cx).shown, ["notes", "state", "tags", "due"]);
    assert!(choices(cx).changed);
    cards.read_with(cx, |cards, _| {
        assert!(cards.fields_menu.is_deployed(), "it stays open while used");
    });
    assert!(
        cx.debug_bounds("MENU_ITEM-due").is_none(),
        "due is a field now, not an offer"
    );
    // The cover, in the same menu: none, and the file again.
    let none = cx
        .debug_bounds("MENU_ITEM-None")
        .expect("a type with a file");
    cx.simulate_click(none.center(), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(choices(cx).cover, None);
    let file = cx.debug_bounds("MENU_ITEM-shot").expect("its file");
    cx.simulate_click(file.center(), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(choices(cx).cover.as_deref(), Some("shot"));

    // Move up, Move down and Hide, as a field's submenu does them.
    cards.update(cx, |cards, cx| {
        cards.move_field("due", -1, cx);
        assert_eq!(field_names(cards), ["notes", "state", "due", "tags"]);
        cards.move_field("due", -1, cx);
        cards.move_field("due", -1, cx);
        assert_eq!(field_names(cards), ["due", "notes", "state", "tags"]);
        cards.move_field("due", -1, cx);
        assert_eq!(field_names(cards)[0], "due", "the first stays");
        cards.move_field("due", 1, cx);
        assert_eq!(field_names(cards), ["notes", "due", "state", "tags"]);
        cards.move_field("tags", 1, cx);
        assert_eq!(field_names(cards)[3], "tags", "the last stays");
        cards.hide_field("state", cx);
        assert_eq!(field_names(cards), ["notes", "due", "tags"]);
        cards.hide_field("state", cx);
        assert_eq!(cards.fields.len(), 3, "hidden already");
    });

    // Eight is a card's most: the ninth is refused, from the menu too.
    cards.update(cx, |cards, cx| {
        for name in ["id", "name", "points", "owner", "link"] {
            cards.show_field(name, cx);
        }
    });
    cx.run_until_parked();
    let full = choices(cx);
    assert_eq!(full.shown.len(), 8);
    assert!(full.full);
    assert_eq!(full.hidden, ["state", "area", "team"]);
    assert_eq!(CARD_FULL, "A card shows at most 8 fields");
    cards.update(cx, |cards, cx| cards.show_field("team", cx));
    assert_eq!(choices(cx).shown.len(), 8, "the ninth is refused");
    cx.update(|window, cx| {
        menu.hide(cx);
        menu.show(window, cx);
    });
    cx.run_until_parked();
    let offer = cx.debug_bounds("MENU_ITEM-team").expect("listed, off");
    cx.simulate_click(offer.center(), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(choices(cx).shown.len(), 8, "and its row does nothing");

    // Reset, from the menu: the default card, at the size the tab has.
    cx.simulate_keystrokes("=");
    let reset = cx
        .debug_bounds("MENU_ITEM-Reset to the default")
        .expect("reset is offered");
    cx.simulate_click(reset.center(), Modifiers::none());
    cx.run_until_parked();
    cards.read_with(cx, |cards, _| {
        assert_eq!(field_names(cards), ["notes", "state", "tags"]);
        assert_eq!(cards.cover.as_deref(), Some("shot"));
        assert_eq!(
            cards.format,
            CardFormat {
                size: CardSize::Large,
                ..CardFormat::default()
            },
            "the size is not the card's"
        );
        assert!(!cards.format_changed());
    });
}

/// Where a format is kept (spec 048): a new tab starts from the type's format in the settings,
/// else from the default card; a tab that comes back keeps its own, whatever the type's is by
/// then; and a change made in a tab is written as the type's, so the next new tab opens the
/// same way. A format that names what the schema no longer has shows the rest, and is not
/// written again for that.
#[gpui::test]
async fn a_new_tab_takes_the_types_format_and_a_restored_tab_keeps_its_own(
    cx: &mut TestAppContext,
) {
    init(cx);
    let (session, _) = session_with_files(TASKS, tasks_answering(6), None, cx);
    cx.run_until_parked();
    let graph = session.read_with(cx, |session, _| session.overrides_key());
    // With nothing kept, the default card.
    let (cards, visual) = open(session.clone(), opening("Task"), cx);
    cards.read_with(visual, |cards, cx| {
        assert_eq!(cards.format, CardFormat::default());
        let saved = Item::serialize(cards, cx).expect("saved");
        assert_eq!(saved.data["format"], json!({}), "the default, as its own");
    });
    assert_eq!(
        kept_format(&session, "Task", visual),
        None,
        "opening writes nothing"
    );

    // The type's format, some of it gone from the schema since: a field and the cover.
    let kept = CardFormat {
        fields: Some(vec!["gone".into(), "points".into(), "owner".into()]),
        cover: CardCover::Property("thumbnail".into()),
        size: CardSize::Large,
    };
    let written = (graph.clone(), kept.clone());
    cx.update(|cx| {
        studio_settings::update(cx, move |settings| {
            settings.set_card_format(&written.0, "Task", Some(written.1))
        })
    });
    let (fresh, visual) = open(session.clone(), opening("Task"), cx);
    fresh.read_with(visual, |cards, _| {
        assert_eq!(cards.format, kept, "a new tab takes the type's");
        assert_eq!(
            field_names(cards),
            ["points", "owner"],
            "what is left of it"
        );
        assert_eq!(cards.cover, None, "its cover is gone: none is shown");
        assert_eq!(cards.format.size, CardSize::Large);
    });
    assert_eq!(
        kept_format(&session, "Task", visual),
        Some(kept.clone()),
        "and the settings keep it as it was written"
    );
    // The first tab, open since before, keeps the card it has.
    cards.read_with(visual, |cards, _| {
        assert_eq!(cards.format, CardFormat::default())
    });

    // A tab that comes back keeps its own.
    let own = CardFormat {
        fields: Some(vec!["due".into()]),
        cover: CardCover::None,
        size: CardSize::Small,
    };
    let saved = json!({ "title": "Task", "type_name": "Task", "format": own });
    let (opening_back, format_back) = crate::saved_cards(&saved).expect("it reads");
    assert_eq!(format_back, Some(own.clone()));
    let (restored, visual) = open_with(session.clone(), opening_back, format_back, cx);
    restored.read_with(visual, |cards, cx| {
        assert_eq!(cards.format, own, "not the type's");
        assert_eq!(field_names(cards), ["due"]);
        let saved = Item::serialize(cards, cx).expect("saved");
        assert_eq!(
            saved.data["format"],
            json!({ "fields": ["due"], "cover": "none", "size": "small" })
        );
        assert_eq!(saved.kind, "cards");
    });
    assert_eq!(kept_format(&session, "Task", visual), Some(kept));

    // A change in a tab is written as the type's: the next new tab opens the same way.
    restored.update(visual, |cards, cx| cards.show_field("points", cx));
    let changed = CardFormat {
        fields: Some(vec!["due".into(), "points".into()]),
        ..own
    };
    restored.read_with(visual, |cards, _| assert_eq!(cards.format, changed));
    assert_eq!(kept_format(&session, "Task", visual), Some(changed.clone()));
    let (next, visual) = open(session.clone(), opening("Task"), cx);
    next.read_with(visual, |cards, _| {
        assert_eq!(cards.format, changed);
        assert_eq!(field_names(cards), ["due", "points"]);
    });
    // The other tabs open keep theirs.
    fresh.read_with(visual, |cards, _| {
        assert_eq!(field_names(cards), ["points", "owner"])
    });

    // The size and the cover are the format's too; the default card is no entry.
    next.update_in(visual, |cards, window, cx| {
        window.focus(&cards.focus_handle, cx)
    });
    visual.simulate_keystrokes("= = f");
    let larger = kept_format(&session, "Task", visual).expect("kept");
    assert_eq!(larger.size, CardSize::Large, "two sizes up from Small");
    assert_eq!(
        larger.cover,
        CardCover::First,
        "F from none: the first file"
    );
    next.update(visual, |cards, cx| {
        cards.reset_format(cx);
        cards.set_size(CardSize::Medium, cx);
    });
    assert_eq!(kept_format(&session, "Task", visual), None);
    visual.update(|_, cx| {
        assert!(!StudioSettings::get(cx).graphs.contains_key(&graph));
    });

    // An action written for the gallery names the file to show: that one, for that tab,
    // over the type's; nothing is written for it.
    let (old, visual) = open(
        session.clone(),
        ShowSetInCards {
            cover: Some("shot".into()),
            ..opening("Task")
        },
        cx,
    );
    old.read_with(visual, |cards, _| {
        assert_eq!(cards.format.cover, CardCover::Property("shot".into()));
        assert_eq!(cards.cover.as_deref(), Some("shot"));
    });
    assert_eq!(kept_format(&session, "Task", visual), None);
}

/// A type with no file has cards too: its title and its fields, no cover's box, no file read,
/// and no cover to choose. Before, the tab said the type had no files.
#[gpui::test]
async fn a_type_with_no_file_has_cards_without_a_cover(cx: &mut TestAppContext) {
    init(cx);
    let schema = "node Topic {\n  slug: String @key\n  name: String\n  about: String?\n}\n";
    let topics = |request: &Value| match request["name"].as_str().unwrap_or_default() {
        "studio_count" => json!([{ "n": 3 }]),
        "studio_page" => json!([
            { "n": { "@id": "a", "slug": "a", "name": "Graphs", "about": "Nodes and edges" } },
            { "n": { "@id": "b", "slug": "b", "name": "Rust", "about": null } },
            { "n": { "@id": "c", "slug": "c", "name": "Design", "about": "How it looks" } },
        ]),
        _ => json!([]),
    };
    let (session, blobs) = session_with_files(schema, topics, None, cx);
    let (cards, cx) = open(session, opening("Topic"), cx);
    cards.update_in(cx, |cards, window, cx| {
        assert_eq!(cards.placed(), 3);
        assert_eq!(field_names(cards), ["about"]);
        assert_eq!((cards.cover.as_deref(), cards.files.len()), (None, 0));
        let width = cards.card_grid().width();
        assert_eq!(
            cards.card_grid().card_height(),
            card_height(width, false, 1)
        );
        let choices = cards.field_choices();
        assert!(choices.files.is_empty() && !choices.changed);
        assert_eq!(choices.hidden, ["slug", "name"]);
        // The status bar lists no key for a cover.
        let status = Item::status(cards, window, cx).expect("its keys");
        let keys: Vec<String> = status
            .keys
            .iter()
            .map(|key| key.label.to_string())
            .collect();
        assert!(keys.contains(&"Open page".to_owned()), "{keys:?}");
        assert!(
            !keys.iter().any(|key| key.contains("cover")),
            "no cover to open or to change: {keys:?}"
        );
    });
    for ix in 0..3 {
        assert!(card_bounds(ix, cx).is_some(), "card {ix} is drawn");
    }
    // F is not this tab's key: nothing changes.
    cx.simulate_keystrokes("f");
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.format, CardFormat::default())
    });
    assert!(blobs.reads.lock().unwrap().is_empty(), "no file is read");
    // Picked, a card with no cover carries its check all the same.
    cx.simulate_keystrokes("x");
    cards.read_with(cx, |cards, _| assert_eq!(cards.picks.len(), 1));
}

/// A change to the format draws the cards once, and with fields shown an arrow key still draws
/// them once (`docs/drawing.md`): a field has no view of its own and nothing of it is read.
#[gpui::test]
async fn a_format_change_draws_the_cards_once(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(TASKS, tasks_answering(40), None, cx);
    let (cards, cx) = open(session, opening("Task"), cx);
    // The first key switches GPUI to keyboard input, which draws the window once.
    cx.simulate_keystrokes("right");
    cx.run_until_parked();
    cards.read_with(cx, |cards, _| assert_eq!(cards.fields.len(), 3));
    for key in ["down", "up", "left", "right", "end", "home"] {
        let before = grid_renders(&cards, cx);
        cx.simulate_keystrokes(key);
        cx.run_until_parked();
        assert_eq!(grid_renders(&cards, cx), before + 1, "one draw for {key}");
    }
    type Change = fn(&mut TypeCards, &mut Context<TypeCards>);
    let changes: [(&str, Change); 6] = [
        ("a field shown", |cards, cx| cards.show_field("due", cx)),
        ("a field moved", |cards, cx| cards.move_field("due", -1, cx)),
        ("a field hidden", |cards, cx| cards.hide_field("notes", cx)),
        ("another size", |cards, cx| {
            cards.set_size(CardSize::Small, cx)
        }),
        ("no cover", |cards, cx| cards.show_cover(None, cx)),
        ("the default card", |cards, cx| cards.reset_format(cx)),
    ];
    for (what, change) in changes {
        let (before, format) = (
            grid_renders(&cards, cx),
            cards.read_with(cx, |cards, _| cards.format.clone()),
        );
        cards.update(cx, change);
        cx.run_until_parked();
        assert_ne!(
            cards.read_with(cx, |cards, _| cards.format.clone()),
            format,
            "{what}"
        );
        assert_eq!(grid_renders(&cards, cx), before + 1, "one draw for {what}");
    }
    // A change that changes nothing draws nothing.
    let before = grid_renders(&cards, cx);
    cards.update(cx, |cards, cx| {
        cards.hide_field("team", cx);
        cards.reset_format(cx);
    });
    cx.run_until_parked();
    assert_eq!(grid_renders(&cards, cx), before);
}

// ---- filters and order (spec 048, slice 5) ----

/// As [`notes_answering`], under what a page asks: a text its first filter holds narrows the
/// notes to those whose title or slug holds it, and an order by title descending turns them
/// around. One page: the tests that use it hold fewer notes than a page reads.
fn notes_under(total: usize) -> impl Fn(&Value) -> Value + Send + Sync + 'static {
    move |request: &Value| {
        let holds = request["params"]["f0"].as_str().map(str::to_owned);
        let mut notes: Vec<usize> = (0..total)
            .filter(|i| {
                holds.as_ref().is_none_or(|text| {
                    format!("Note {i}").contains(text.as_str())
                        || format!("n{i:03}").contains(text.as_str())
                })
            })
            .collect();
        if request["query"]
            .as_str()
            .is_some_and(|text| text.contains("title desc"))
        {
            notes.reverse();
        }
        match request["name"].as_str().unwrap_or_default() {
            "studio_count" => json!([{ "n": notes.len() }]),
            "studio_page" if request["params"]["after"].is_null() => {
                Value::from(notes.into_iter().map(note).collect::<Vec<_>>())
            }
            "studio_nodes_by_id" => Value::from(
                request["params"]["ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|id| id.as_str()?.trim_start_matches('n').parse::<usize>().ok())
                    .filter(|&i| i < total)
                    .map(note)
                    .collect::<Vec<_>>(),
            ),
            _ => json!([]),
        }
    }
}

/// Writes a filter through the bar as a person does: ⌘F opens the draft, its key and its test
/// are set as their menus set them, the value is typed and ↵ applies it. A test that takes no
/// value ("today", "is empty") has no field to type in, and ↵ adds it all the same.
fn write_filter(
    cards: &Entity<TypeCards>,
    property: &str,
    op: FilterOp,
    text: &str,
    cx: &mut VisualTestContext,
) {
    cx.simulate_keystrokes("cmd-f");
    cx.run_until_parked();
    cards.update_in(cx, |cards, window, cx| {
        assert!(cards.bar().draft.is_some(), "⌘F opens a filter to write");
        cards.set_draft_property(property.into(), window, cx);
        cards.set_draft_op(op, window, cx);
    });
    cx.run_until_parked();
    if op.takes_value() {
        cx.simulate_input(text);
        cx.run_until_parked();
    }
    cx.simulate_keystrokes("enter");
    settle(cx);
}

/// Another writer commits `commit` on the branch and the session's head watch finds it: the
/// head is past the commit the cards show, and they offer Refresh.
fn head_moves_to(
    commit: &str,
    session: &Entity<GraphSession>,
    writer: &crate::test_support::Writer,
    cards: &Entity<TypeCards>,
    cx: &mut VisualTestContext,
) {
    writer.commit(commit, vec![changed("node", "Note", "n001", None)]);
    session.update(cx, |session, cx| session.check_heads(cx));
    cx.run_until_parked();
    cards.read_with(cx, |cards, _| {
        assert!(cards.newer, "Refresh is offered");
        assert_ne!(cards.paging.pinned.as_deref(), Some(commit));
    });
}

fn row_ids(cards: &Entity<TypeCards>, cx: &mut VisualTestContext) -> Vec<String> {
    cards.read_with(cx, |cards, _| {
        cards.rows.iter().map(|row| row.id.clone()).collect()
    })
}

fn field_text(cards: &Entity<TypeCards>, cx: &mut VisualTestContext) -> String {
    cards.read_with(cx, |cards, cx| cards.bar().editor.read(cx).text(cx))
}

/// Clicks the middle of what `selector` names in the last frame.
fn click(selector: &'static str, cx: &mut VisualTestContext) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} is not drawn"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

/// A filter written in the bar reads the set again from its first page at the commit the tab
/// shows, not at the head another writer moved meanwhile (constitution II). The cursor goes to
/// the first card; the picks stay by id, as a table's selection does through a filter change,
/// and the card a range of picks would start from goes. The tab's title and its key follow the
/// filters, and ⌘⌥⌫ clears them, at the pin again.
#[gpui::test]
async fn a_filter_reads_again_at_the_pin_and_not_at_the_head(cx: &mut TestAppContext) {
    init(cx);
    let (session, writer, served) = session_served(NOTES, notes_under(12), cx);
    let (cards, cx) = open(session.clone(), opening("Note"), cx);
    cards.update_in(cx, |cards, window, cx| {
        assert_eq!(cards.paging.pinned.as_deref(), Some("c1"));
        assert_eq!(cards.rows.len(), 12);
        assert_eq!(Item::tab_title(cards, cx), "Cards: Note");
        assert!(cards.render_filter_bar(window, cx).is_none(), "no bar yet");
    });
    let first = served.sent("studio_page");
    assert_eq!(first.len(), 1);
    assert_eq!(
        first[0]["snapshot"],
        Value::Null,
        "the tab opened at the head"
    );
    let key = cards.read_with(cx, Item::dedupe_key);

    // Another writer commits: the head is past the commit the cards show.
    head_moves_to("c2", &session, &writer, &cards, cx);
    cx.simulate_keystrokes("right right x right x");
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.cursor, Some(3));
        assert_eq!(cards.picks.len(), 2);
        assert!(cards.pick_anchor.is_some());
    });

    write_filter(&cards, "title", FilterOp::Contains, "1", cx);
    let pages = served.sent("studio_page");
    assert_eq!(pages.len(), 2, "one read for the filter");
    assert_eq!(pages[1]["snapshot"], json!("c1"), "asked at the pin");
    assert_eq!(pages[1]["at"], json!("c1"), "and answered there");
    assert_eq!(pages[1]["params"]["f0"], json!("1"));
    assert_eq!(pages[1]["params"].get("after"), None, "from the first page");
    assert_eq!(row_ids(&cards, cx), ["n001", "n010", "n011"]);
    cards.update_in(cx, |cards, window, cx| {
        assert_eq!(cards.paging.pinned.as_deref(), Some("c1"), "the pin holds");
        assert!(cards.newer, "and Refresh is still offered");
        assert_eq!(
            cards.filters,
            [Filter {
                property: "title".into(),
                op: FilterOp::Contains,
                value: json!("1"),
            }]
        );
        assert_eq!(cards.cursor, Some(0), "the cursor is on the first card");
        assert_eq!(cards.count, Loadable::Loaded(3));
        // As the table's selection: kept by id, shown or not; the range's start goes.
        assert_eq!(
            cards.picks.iter().map(String::as_str).collect::<Vec<_>>(),
            ["n002", "n003"]
        );
        assert_eq!(cards.pick_anchor, None);
        assert!(cards.bar().draft.is_none());
        assert!(
            cards.focus_handle.is_focused(window),
            "the keyboard is the cards'"
        );
        assert!(
            cards.render_filter_bar(window, cx).is_some(),
            "its pill shows"
        );
        assert_eq!(Item::tab_title(cards, cx), "Cards: Note, 1");
        assert_ne!(
            Item::dedupe_key(cards, cx),
            key,
            "the key is the set read now"
        );
        assert_eq!(
            Item::dedupe_key(cards, cx),
            Some(crate::cards_key(&cards.opening()).into())
        );
    });

    // A second filter narrows what the first left; clearing them reads every note, at the pin.
    write_filter(&cards, "title", FilterOp::Contains, "Note 1", cx);
    cards.read_with(cx, |cards, cx| {
        assert_eq!(cards.filters.len(), 2);
        assert_eq!(Item::tab_title(cards, cx), "Cards: Note, 1, Note 1");
    });
    let pages = served.sent("studio_page");
    assert_eq!((pages.len(), &pages[2]["snapshot"]), (3, &json!("c1")));
    cx.simulate_keystrokes("cmd-alt-backspace");
    settle(cx);
    // The session holds what it read of every note at this commit, which never changes: the
    // server is asked nothing, and no read of the head stands in for it.
    assert_eq!(served.sent("studio_page").len(), 3);
    cards.read_with(cx, |cards, cx| {
        assert!(cards.filters.is_empty());
        assert_eq!(cards.rows.len(), 12);
        assert_eq!(cards.cursor, Some(0));
        assert_eq!(cards.paging.pinned.as_deref(), Some("c1"));
        assert!(cards.newer);
        assert_eq!(Item::tab_title(cards, cx), "Cards: Note");
        assert_eq!(Item::dedupe_key(cards, cx), key);
    });

    // Refresh is what reads the head.
    cx.simulate_keystrokes("cmd-r");
    settle(cx);
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.paging.pinned.as_deref(), Some("c2"));
        assert!(!cards.newer);
    });
}

/// The Sort chip's menu orders the cards by a property, and its direction turns them: each
/// reads the set again from its first page at the pin, the cursor on the first card. The chip's
/// × puts them back in the order of their ids.
#[gpui::test]
async fn sort_orders_the_cards_and_reads_at_the_pin(cx: &mut TestAppContext) {
    init(cx);
    let (session, writer, served) = session_served(NOTES, notes_under(8), cx);
    let (cards, cx) = open(session.clone(), opening("Note"), cx);
    head_moves_to("c2", &session, &writer, &cards, cx);
    cx.simulate_keystrokes("right right");
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.cursor, Some(2));
        assert_eq!(
            crate::model::sort_choices(&cards.all_columns),
            [("title".to_owned(), "title".to_owned())],
            "what the menu lists: the properties rows can be ordered by, and no file"
        );
    });

    // The menu, as a click on the chip opens it: no order, title, then the direction.
    let menu = cards.read_with(cx, |cards, _| cards.sort_menu.clone());
    cx.update(|window, cx| menu.show(window, cx));
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.simulate_next_frame(cx);
    });
    for _ in 0..2 {
        cx.dispatch_action(menu::SelectNext);
    }
    cx.dispatch_action(menu::Confirm);
    settle(cx);
    cards.read_with(cx, |cards, cx| {
        assert_eq!(
            cards.sort,
            Some(Sort {
                property: "title".into(),
                descending: false,
            })
        );
        assert_eq!(cards.cursor, Some(0));
        assert_eq!(cards.paging.pinned.as_deref(), Some("c1"));
        assert_eq!(
            Item::tab_title(cards, cx),
            "Cards: Note",
            "an order is no filter"
        );
    });
    let pages = served.sent("studio_page");
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[1]["snapshot"], json!("c1"));
    assert!(
        pages[1]["query"].as_str().unwrap().contains("$n.title asc"),
        "{}",
        pages[1]["query"]
    );

    cx.simulate_keystrokes("right");
    cards.update(cx, |cards, cx| {
        cards.set_sort(
            Some(Sort {
                property: "title".into(),
                descending: true,
            }),
            cx,
        )
    });
    settle(cx);
    let pages = served.sent("studio_page");
    assert_eq!(pages.len(), 3);
    assert_eq!(pages[2]["snapshot"], json!("c1"));
    assert!(
        pages[2]["query"]
            .as_str()
            .unwrap()
            .contains("$n.title desc")
    );
    assert_eq!(row_ids(&cards, cx)[0], "n007", "in the order read");
    cards.read_with(cx, |cards, _| assert_eq!(cards.cursor, Some(0)));

    // The same order again reads nothing; one the type cannot give is left.
    cards.update(cx, |cards, cx| {
        let same = cards.sort.clone();
        cards.set_sort(same, cx);
        cards.set_sort(
            Some(Sort {
                property: "attachment".into(),
                descending: false,
            }),
            cx,
        );
    });
    settle(cx);
    assert_eq!(served.sent("studio_page").len(), 3);

    // The chip's ×: the order of their ids, which the session read at this commit when the
    // tab opened, so the server is asked nothing.
    let clear = cx.debug_bounds("chip-clear").expect("a set chip has its ×");
    cx.simulate_click(clear.center(), Modifiers::none());
    settle(cx);
    assert_eq!(served.sent("studio_page").len(), 3);
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.sort, None);
        assert_eq!(cards.paging.pinned.as_deref(), Some("c1"));
        assert_eq!(cards.rows.len(), 8);
    });
    assert_eq!(row_ids(&cards, cx)[0], "n000");
}

/// A date preset is kept as the preset: each read is sent the days it means on the day of the
/// read, and the tab saves "today", not a date. So a tab left open, or restored, does not show
/// the day it was opened.
#[gpui::test]
async fn a_date_preset_is_resolved_at_each_read_and_saved_as_itself(cx: &mut TestAppContext) {
    init(cx);
    let (session, _, served) = session_served(TASKS, tasks_answering(6), cx);
    let (cards, cx) = open(session, opening("Task"), cx);
    let before = chrono::Local::now();
    write_filter(&cards, "due", FilterOp::Today, "", cx);
    let preset = Filter {
        property: "due".into(),
        op: FilterOp::Today,
        value: Value::Null,
    };
    cards.read_with(cx, |cards, cx| {
        assert_eq!(
            cards.filters,
            std::slice::from_ref(&preset),
            "kept as the preset"
        );
        let saved = Item::serialize(cards, cx).expect("saved");
        assert_eq!(
            saved.data["filters"],
            json!([{ "property": "due", "op": "Today", "value": null }]),
            "and saved as it"
        );
        assert_eq!(cards.opening().filters, saved.data.get("filters").cloned());
    });
    // The days of the read: today's, whichever side of midnight the test ran.
    let days = |now| crate::model::preset_range(FilterOp::Today, Some(ScalarKind::Date), now);
    let sent = |read: &Value| json!([read["params"]["f0"], read["params"]["f0_end"]]);
    let today = [days(before), days(chrono::Local::now())];
    let pages = served.sent("studio_page");
    assert_eq!(pages.len(), 2);
    assert!(today.contains(&sent(&pages[1])), "{}", pages[1]["params"]);

    // Read again, the preset is resolved again: the tab holds no date to send.
    cx.simulate_keystrokes("cmd-r");
    settle(cx);
    let pages = served.sent("studio_page");
    assert_eq!(pages.len(), 3);
    let today = [days(before), days(chrono::Local::now())];
    assert!(today.contains(&sent(&pages[2])), "{}", pages[2]["params"]);
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.filters, std::slice::from_ref(&preset));
    });
}

/// A filter on a property the schema no longer has is dropped when the tab takes the schema,
/// with the test the table's schema load makes ([`filter_stands`]), and the bar says which:
/// at opening, for a saved tab, and at a refresh after another writer changed the schema. An
/// order by a property that is gone goes with it. The notice goes with the next change.
#[gpui::test]
async fn a_filter_the_schema_lost_is_dropped_and_said(cx: &mut TestAppContext) {
    init(cx);
    let (session, writer, served) = session_served(NOTES, notes_under(12), cx);
    // A saved tab whose filter names a property the schema never had.
    let mut saved = opening("Note");
    saved.filters = Some(json!([
        { "property": "topic", "op": "Eq", "value": "design" },
        { "property": "title", "op": "Contains", "value": "1" },
    ]));
    saved.sort = Some(json!({ "property": "title", "descending": true }));
    let (cards, cx) = open(session.clone(), saved, cx);
    let title = Filter {
        property: "title".into(),
        op: FilterOp::Contains,
        value: json!("1"),
    };
    cards.update_in(cx, |cards, window, cx| {
        assert_eq!(
            cards.filters,
            std::slice::from_ref(&title),
            "the one it can read under"
        );
        assert_eq!(cards.lost, ["topic"]);
        assert!(cards.bar_note().is_some(), "and the bar says so");
        assert!(cards.render_filter_bar(window, cx).is_some());
        assert_eq!(Item::tab_title(cards, cx), "Cards: Note, 1");
        assert!(!Item::keeps_preview(cards, cx), "not the person's change");
    });
    let pages = served.sent("studio_page");
    assert_eq!(pages.len(), 1, "read once, without it");
    assert_eq!(pages[0]["params"]["f0"], json!("1"));
    assert_eq!(pages[0]["params"].get("f1"), None);
    assert_eq!(row_ids(&cards, cx), ["n011", "n010", "n001"]);

    // Another writer drops the title: the tab keeps what it shows until it is refreshed.
    let untitled = "node Note {\n  slug: String @key\n  body: String?\n  attachment: Blob?\n  scan: Blob?\n}\n";
    writer.set_schema(untitled);
    writer.commit("c2", vec![changed("node", "Note", "n001", None)]);
    session.update(cx, |session, cx| session.check_heads(cx));
    cx.run_until_parked();
    cards.read_with(cx, |cards, _| {
        assert!(cards.newer, "Refresh is offered");
        assert_eq!(cards.filters, std::slice::from_ref(&title));
        assert!(cards.sort.is_some());
    });
    cx.simulate_keystrokes("cmd-r");
    settle(cx);
    cards.update_in(cx, |cards, window, cx| {
        assert_eq!(&*cards.schema.as_ref().unwrap().source, untitled);
        assert!(cards.filters.is_empty(), "the filter on the title went");
        assert_eq!(cards.sort, None, "and the order by it");
        assert_eq!(cards.lost, ["title"], "said in place of the last notice");
        assert!(
            cards.render_filter_bar(window, cx).is_some(),
            "the bar shows for it"
        );
        assert_eq!(cards.rows.len(), 12);
        assert_eq!(Item::tab_title(cards, cx), "Cards: Note");
        // The menus follow the schema.
        assert!(
            !cards
                .filter_choices()
                .iter()
                .any(|(key, _, _)| key == "title")
        );
    });
    let last = served.sent("studio_page").pop().unwrap();
    assert_eq!(last["params"].get("f0"), None);
    assert!(!last["query"].as_str().unwrap().contains("title"));

    // The next change of the filters takes the notice with it.
    write_filter(&cards, "body", FilterOp::Contains, "n00", cx);
    cards.update_in(cx, |cards, window, cx| {
        assert!(cards.lost.is_empty() && cards.bar_note().is_none());
        assert_eq!(cards.filters.len(), 1);
        assert!(cards.render_filter_bar(window, cx).is_some(), "its pill");
    });
}

/// A tab of rows picked in a table says so in a pill before any filter, and its Filters and
/// Sort chips are off, each saying why: its rows are the ones listed. Clearing the pill shows
/// the type's rows with no filter, at the commit the tab shows, and the chips work.
#[gpui::test]
async fn a_tab_of_picked_rows_says_so_until_its_pill_is_cleared(cx: &mut TestAppContext) {
    init(cx);
    let (session, writer, served) = session_served(NOTES, notes_under(12), cx);
    let mut listed = opening("Note");
    listed.title = "Note, 3 selected rows".into();
    listed.ids = Some(vec!["n003".into(), "n001".into(), "n002".into()]);
    let (cards, cx) = open(session.clone(), listed, cx);
    head_moves_to("c2", &session, &writer, &cards, cx);
    assert_eq!(row_ids(&cards, cx), ["n003", "n001", "n002"]);
    cards.update_in(cx, |cards, window, cx| {
        assert_eq!(Item::tab_title(cards, cx), "Cards: Note, 3 selected rows");
        assert_eq!(
            cards.listed_note().as_deref(),
            Some("Showing 3 picked rows. Clear the pill to filter Note")
        );
        let status = Item::status(cards, window, cx).expect("a status");
        assert!(
            !status.keys.iter().any(|key| key.label == "Add a filter"),
            "the status bar offers no key that would do nothing"
        );
    });
    assert!(
        cx.debug_bounds("cards-listed").is_some(),
        "the pill is drawn"
    );

    // ⌘F, the Filters chip and the Sort chip do nothing meanwhile.
    cx.simulate_keystrokes("cmd-f cmd-alt-backspace");
    click("cards-filters", cx);
    click("cards-sort", cx);
    cards.update(cx, |cards, cx| {
        cards.set_sort(
            Some(Sort {
                property: "title".into(),
                descending: true,
            }),
            cx,
        )
    });
    settle(cx);
    cards.read_with(cx, |cards, _| {
        assert!(cards.bar().draft.is_none(), "no filter is written");
        assert!(!cards.sort_menu.is_deployed(), "no order is offered");
        assert_eq!(cards.sort, None);
        assert_eq!(cards.rows.len(), 3);
    });
    assert_eq!(
        served.sent("studio_page").len(),
        0,
        "listed rows are read by id"
    );

    // The pill's ×.
    click("set-pill-clear", cx);
    settle(cx);
    let pages = served.sent("studio_page");
    assert_eq!(pages.len(), 1);
    assert_eq!(
        pages[0]["snapshot"],
        json!("c1"),
        "at the commit the tab shows"
    );
    assert_eq!(pages[0]["params"].get("f0"), None, "under no filter");
    cards.read_with(cx, |cards, cx| {
        assert_eq!(cards.ids, None);
        assert_eq!(cards.rows.len(), 12);
        assert_eq!(cards.count, Loadable::Loaded(12));
        assert_eq!(cards.cursor, Some(0));
        assert_eq!(cards.listed_note(), None);
        assert_eq!(Item::tab_title(cards, cx), "Cards: Note");
        assert!(Item::keeps_preview(cards, cx), "the person changed the set");
        let saved = Item::serialize(cards, cx).expect("saved");
        assert_eq!(
            saved.data["ids"],
            Value::Null,
            "and it is saved as the type's"
        );
    });
    assert!(
        cx.debug_bounds("cards-listed").is_none(),
        "the pill is gone"
    );

    // The chips are the type's now.
    click("cards-filters", cx);
    cards.update_in(cx, |cards, window, cx| {
        assert!(
            cards.bar().draft.is_some(),
            "Filters opens a filter to write"
        );
        assert!(cards.bar().editor.focus_handle(cx).is_focused(window));
    });
    cx.simulate_keystrokes("escape");
    click("cards-sort", cx);
    cards.read_with(cx, |cards, _| assert!(cards.sort_menu.is_deployed()));
}

/// While the filter's value field has the keyboard its keys are the text's: the arrows and
/// Home move in it, ⌘A selects it, ⌘C copies it and a letter is typed, and none of them moves
/// the cursor, picks a card or opens anything. ↵ applies the filter and opens no page; the
/// next ↵, the cards', opens the page of the first card.
#[gpui::test]
async fn keys_typed_in_the_value_field_are_the_texts(cx: &mut TestAppContext) {
    init(cx);
    let (session, _, _) = session_served(NOTES, notes_under(12), cx);
    let heard: Rc<RefCell<Vec<String>>> = Rc::default();
    cx.update(|cx| {
        let log = heard.clone();
        cx.on_action(move |open: &OpenNodePage, _| log.borrow_mut().push(open.id.clone()));
        let log = heard.clone();
        cx.on_action(move |show: &ShowNodeOnCanvas, _| {
            log.borrow_mut().push(format!("canvas {}", show.id));
        });
    });
    let (cards, cx) = open(session, opening("Note"), cx);
    cx.simulate_keystrokes("right x");
    cx.simulate_keystrokes("cmd-f");
    cx.run_until_parked();
    cards.update_in(cx, |cards, window, cx| {
        cards.set_draft_op(FilterOp::Contains, window, cx);
        assert!(cards.bar().editor.focus_handle(cx).is_focused(window));
    });
    cx.run_until_parked();
    let state = |cx: &mut VisualTestContext| {
        cards.read_with(cx, |cards, _| {
            (cards.cursor, cards.picks.len(), cards.format.size)
        })
    };
    let before = state(cx);
    assert_eq!(before, (Some(1), 1, CardSize::Medium));

    cx.simulate_input("ab");
    // ← moves the caret, so the next letter lands before the last.
    cx.simulate_keystrokes("left");
    cx.simulate_input("X");
    assert_eq!(field_text(&cards, cx), "aXb");
    cx.simulate_keystrokes("home");
    cx.simulate_input("H");
    assert_eq!(field_text(&cards, cx), "HaXb");
    // The cards' other arrows and End have nothing to move in a field of one line.
    cx.simulate_keystrokes("right down up end");
    assert_eq!(state(cx), before, "no arrow moved the cursor");
    // ⌘A selects the text, not every card; ⌘C copies it, not an id.
    cx.simulate_keystrokes("cmd-a cmd-c");
    assert_eq!(state(cx), before, "⌘A picked cards");
    assert_eq!(
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .as_deref(),
        Some("HaXb")
    );
    // The letters the cards act on are typed: over the selection, then after it.
    cx.simulate_keystrokes("g x f = - space");
    assert_eq!(field_text(&cards, cx), "gxf=- ");
    assert_eq!(state(cx), before, "a letter acted on the cards");
    assert!(heard.borrow().is_empty(), "{:?}", heard.borrow());

    cx.simulate_keystrokes("cmd-a");
    cx.simulate_input("Note 1");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    settle(cx);
    assert!(heard.borrow().is_empty(), "↵ in the field opened a page");
    cards.update_in(cx, |cards, window, _| {
        assert_eq!(
            cards.filters,
            [Filter {
                property: "title".into(),
                op: FilterOp::Contains,
                value: json!("Note 1"),
            }]
        );
        assert!(cards.bar().draft.is_none());
        assert!(cards.focus_handle.is_focused(window));
    });
    assert_eq!(row_ids(&cards, cx), ["n001", "n010", "n011"]);
    // The keyboard is the cards' again.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(*heard.borrow(), ["n001"]);
    cx.simulate_keystrokes("right");
    cards.read_with(cx, |cards, _| assert_eq!(cards.cursor, Some(1)));
}

/// Esc closes the filter being written and leaves the picks; with no draft it takes the picks
/// out.
#[gpui::test]
async fn esc_closes_the_draft_first_and_clears_the_picks_second(cx: &mut TestAppContext) {
    init(cx);
    let (session, _, served) = session_served(NOTES, notes_under(12), cx);
    let (cards, cx) = open(session, opening("Note"), cx);
    cx.simulate_keystrokes("x right x");
    cx.simulate_keystrokes("cmd-f");
    cx.run_until_parked();
    cx.simulate_input("des");
    cards.update_in(cx, |cards, window, cx| {
        assert_eq!(cards.picks.len(), 2);
        assert!(cards.bar().draft.is_some());
        assert!(cards.bar().editor.focus_handle(cx).is_focused(window));
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cards.update_in(cx, |cards, window, _| {
        assert!(cards.bar().draft.is_none(), "Esc closed the draft");
        assert_eq!(cards.picks.len(), 2, "and left the picks");
        assert!(cards.filters.is_empty(), "nothing was filtered");
        assert!(cards.focus_handle.is_focused(window));
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cards.read_with(cx, |cards, _| assert!(cards.picks.is_empty()));
    assert_eq!(served.sent("studio_page").len(), 1, "no read for either");

    // A draft closed under its field, here by ⌘⌥⌫ over a filter and a draft, leaves the
    // keyboard with the cards: the next key is theirs.
    write_filter(&cards, "title", FilterOp::Contains, "Note", cx);
    cx.simulate_keystrokes("cmd-f");
    cx.run_until_parked();
    cards.update_in(cx, |cards, window, cx| {
        assert!(cards.bar().editor.focus_handle(cx).is_focused(window));
    });
    cx.simulate_keystrokes("cmd-alt-backspace");
    settle(cx);
    cards.update_in(cx, |cards, window, _| {
        assert!(cards.bar().draft.is_none() && cards.filters.is_empty());
        assert!(
            cards.focus_handle.is_focused(window),
            "the cards have the keyboard"
        );
        assert_eq!(cards.cursor, Some(0));
    });
    cx.simulate_keystrokes("right");
    cards.read_with(cx, |cards, _| assert_eq!(cards.cursor, Some(1)));
}

/// A key typed in the filter's value field, and a move through the values it offers, draw the
/// cards no times: the field is what changed, and the cards are a cached view beside it
/// (`docs/drawing.md`). A filter applied draws them once, without their rows, and once for
/// each read that lands: the page, then the count.
#[gpui::test]
async fn typing_a_filter_draws_the_cards_no_times(cx: &mut TestAppContext) {
    init(cx);
    // More tasks than a first page holds, so their count is a read of its own.
    let answer = |request: &Value| {
        let limit = request["query"]
            .as_str()
            .and_then(|text| text.split("limit ").nth(1))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(300);
        match request["name"].as_str().unwrap_or_default() {
            "studio_count" => json!([{ "n": 300 }]),
            "studio_page" => {
                let from = request["params"]["after"].as_str().map_or(0, |after| {
                    after.trim_start_matches('t').parse::<usize>().unwrap_or(0) + 1
                });
                Value::from((from..300).take(limit).map(task).collect::<Vec<_>>())
            }
            _ => json!([]),
        }
    };
    let (session, _, served) = session_served(TASKS, answer, cx);
    // No cover: the only reads are the rows'.
    let format = CardFormat {
        cover: CardCover::None,
        ..CardFormat::default()
    };
    let (cards, cx) = open_with(session, opening("Task"), Some(format), cx);
    // The first key switches GPUI to keyboard input, which draws the window once.
    cx.simulate_keystrokes("right");
    cx.simulate_keystrokes("cmd-f");
    cx.run_until_parked();
    cards.update_in(cx, |cards, window, cx| {
        cards.set_draft_property("name".into(), window, cx);
        cards.set_draft_op(FilterOp::Contains, window, cx);
    });
    cx.run_until_parked();

    let before = grid_renders(&cards, cx);
    for key in ["T", "a", "s", "k"] {
        cx.simulate_input(key);
        cx.run_until_parked();
    }
    cx.simulate_keystrokes("left home");
    cx.run_until_parked();
    assert_eq!(field_text(&cards, cx), "Task");
    assert_eq!(grid_renders(&cards, cx), before, "typing drew the cards");

    // An enum's field offers its members: ↓ and ↑ highlight one, and draw only the offers.
    cards.update_in(cx, |cards, window, cx| {
        cards.set_draft_property("state".into(), window, cx);
        cards.set_draft_op(FilterOp::Eq, window, cx);
    });
    cx.run_until_parked();
    let before = grid_renders(&cards, cx);
    let highlighted = |cx: &mut VisualTestContext| {
        cards.read_with(cx, |cards, _| {
            let offers = &cards.bar().draft.as_ref().unwrap().offers;
            (offers.len(), offers.highlighted())
        })
    };
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(highlighted(cx), (2, Some(0)));
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(highlighted(cx), (2, Some(1)));
    cx.simulate_keystrokes("up");
    cx.run_until_parked();
    assert_eq!(highlighted(cx), (2, Some(0)));
    cards.read_with(cx, |cards, _| {
        assert_eq!(
            cards.cursor,
            Some(1),
            "↓ moved in the offers, not the cards"
        );
    });
    assert_eq!(
        grid_renders(&cards, cx),
        before,
        "a move in the offers drew the cards"
    );

    // ↵ takes the highlighted member and filters: the cards are drawn once without their
    // rows, then once as the page lands and once as the count does.
    let page = served.hold(|read| read["name"] == "studio_page");
    let count = served.hold(|read| read["name"] == "studio_count");
    let before = grid_renders(&cards, cx);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.filters.len(), 1, "the filter is applied");
        assert!(matches!(cards.first_page, Loadable::Loading));
    });
    assert_eq!(grid_renders(&cards, cx), before + 1, "one draw to apply it");
    page.send(()).unwrap();
    cx.run_until_parked();
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.rows.len(), 200);
        assert_eq!(cards.count, Loadable::Loading);
    });
    assert_eq!(grid_renders(&cards, cx), before + 2, "one for the page");
    count.send(()).unwrap();
    cx.run_until_parked();
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.count, Loadable::Loaded(300))
    });
    assert_eq!(grid_renders(&cards, cx), before + 3, "one for the count");
    settle(cx);
    assert_eq!(grid_renders(&cards, cx), before + 3, "and no more");
}

/// A preview tab is replaced by the next preview until the person changes its filters, its
/// order or its format: then it is theirs, and the next preview opens beside it (spec 020).
#[gpui::test]
async fn a_preview_tab_the_person_changed_is_kept(cx: &mut TestAppContext) {
    init(cx);
    cx.update(crate::init);
    let (session, _) = session_with_files(NOTES, notes_under(12), None, cx);
    cx.run_until_parked();
    let (workspace, cx) = cx.add_window_view(Workspace::new);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_session(session.clone(), window, cx)
    });
    cx.run_until_parked();
    let preview = |ids: Option<Vec<String>>, cx: &mut VisualTestContext| {
        let opening = ShowSetInCards {
            ids,
            preview: true,
            ..opening("Note")
        };
        let cards = workspace
            .update_in(cx, |workspace, window, cx| {
                crate::open_cards(workspace, opening, None, window, cx)
            })
            .expect("a Cards tab opens");
        settle(cx);
        cards
    };
    let tabs = |cx: &mut VisualTestContext| {
        workspace.read_with(cx, |workspace, _| {
            (workspace.items().len(), workspace.active_tab_is_preview())
        })
    };
    let listed = |id: &str| Some(vec![id.to_owned()]);

    // Untouched, a preview gives its place to the next.
    let first = preview(None, cx);
    assert_eq!(tabs(cx), (1, true));
    first.read_with(cx, |cards, cx| assert!(!Item::keeps_preview(cards, cx)));
    let second = preview(listed("n001"), cx);
    assert_eq!(tabs(cx), (1, true), "the preview was replaced");

    // An order, a filter and a format each keep the tab they were made in.
    type Change = fn(&mut TypeCards, &mut Context<TypeCards>);
    let changes: [(&str, Change); 3] = [
        ("an order", |cards, cx| {
            cards.set_sort(
                Some(Sort {
                    property: "title".into(),
                    descending: true,
                }),
                cx,
            )
        }),
        ("a filter", |cards, cx| {
            cards.filters.push(Filter {
                property: "title".into(),
                op: FilterOp::Contains,
                value: json!("1"),
            });
            cards.filters_changed(cx);
        }),
        ("a format", |cards, cx| cards.set_size(CardSize::Large, cx)),
    ];
    drop(second);
    for (count, (what, change)) in changes.into_iter().enumerate() {
        let cards = preview(None, cx);
        assert_eq!(tabs(cx), (count + 1, true), "{what}: a preview opens");
        cards.update(cx, change);
        settle(cx);
        cards.read_with(cx, |cards, cx| {
            assert!(Item::keeps_preview(cards, cx), "{what} keeps the tab");
        });
        assert_eq!(tabs(cx), (count + 1, false), "{what}: the tab is kept");
        // The next preview opens beside it.
        preview(listed("n002"), cx);
        assert_eq!(
            tabs(cx),
            (count + 2, true),
            "{what}: the next opens beside it"
        );
    }
}

/// A saved tab comes back with its filters, a date preset still a preset, its order and its
/// format, reads its rows under them, and names itself by them.
#[gpui::test]
async fn a_saved_tab_comes_back_with_its_filters_order_and_format(cx: &mut TestAppContext) {
    init(cx);
    cx.update(crate::init);
    let (session, _, served) = session_served(TASKS, tasks_answering(6), cx);
    let (cards, cx) = open(session.clone(), opening("Task"), cx);
    write_filter(&cards, "name", FilterOp::Contains, "Task", cx);
    write_filter(&cards, "due", FilterOp::ThisWeek, "", cx);
    cards.update(cx, |cards, cx| {
        cards.set_sort(
            Some(Sort {
                property: "points".into(),
                descending: true,
            }),
            cx,
        );
        cards.set_size(CardSize::Large, cx);
        cards.hide_field("notes", cx);
    });
    settle(cx);
    let (saved, filters, format, title) = cards.read_with(cx, |cards, cx| {
        (
            Item::serialize(cards, cx).expect("saved"),
            cards.filters.clone(),
            cards.format.clone(),
            Item::tab_title(cards, cx),
        )
    });
    assert_eq!(saved.kind, "cards");
    assert_eq!(
        saved.data["filters"],
        json!([
            { "property": "name", "op": "Contains", "value": "Task" },
            { "property": "due", "op": "ThisWeek", "value": null },
        ])
    );
    assert_eq!(
        saved.data["sort"],
        json!({ "property": "points", "descending": true })
    );
    assert_eq!(saved.data["format"]["size"], json!("large"));
    assert_eq!(title, "Cards: Task, Task, due is this week");

    // In another window, as the next launch restores it.
    let (workspace, cx) = cx.add_window_view(Workspace::new);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_session(session.clone(), window, cx)
    });
    cx.run_until_parked();
    let read_before = served.sent("studio_page").len();
    let opened = workspace.update_in(cx, |workspace, window, cx| {
        workspace.reopen_saved(&saved, window, cx)
    });
    settle(cx);
    assert!(opened);
    let back = workspace
        .read_with(cx, |workspace, _| {
            workspace
                .active_item()
                .and_then(|item| item.to_any().downcast::<TypeCards>().ok())
        })
        .expect("a Cards tab is open");
    assert_ne!(back.entity_id(), cards.entity_id());
    back.read_with(cx, |back, cx| {
        assert_eq!(back.filters, filters, "its filters, the preset a preset");
        assert_eq!(
            back.sort,
            Some(Sort {
                property: "points".into(),
                descending: true,
            })
        );
        assert_eq!(back.format, format);
        assert!(!field_names(back).contains(&"notes"));
        assert_eq!(Item::tab_title(back, cx), title);
        assert_eq!(back.rows.len(), 6, "and it read its rows");
        assert!(!Item::keeps_preview(back, cx), "restored, not changed");
    });
    let pages = served.sent("studio_page");
    let page = &pages[read_before];
    assert_eq!(page["params"]["f0"], json!("Task"));
    assert!(page["params"]["f1"].is_string(), "the week's days: {page}");
    assert!(page["query"].as_str().unwrap().contains("$n.points desc"));
}

/// A filter keeps what was read at the commit it stays at. The cover of a card it still shows
/// is not read again. On a branch its marks stay, the rows the branch removed stand first
/// again, and the cursor is on the first card the branch still has. A preview kept of a row
/// the filter no longer shows is let go past the cap, as one scrolled far away is.
#[gpui::test]
async fn a_filter_keeps_the_covers_and_the_branchs_marks(cx: &mut TestAppContext) {
    init(cx);
    let note_type = json!({ "id": "Note", "name": "Note" });
    let changes = json!([
        { "kind": "node", "type": note_type, "id": "n001", "op": "insert",
          "before": null, "after": { "properties": { "slug": "n001", "title": "Note 1" } } },
        { "kind": "node", "type": note_type, "id": "n900", "op": "delete",
          "before": { "properties": { "slug": "n900", "title": "Gone" } }, "after": null },
    ]);
    let (session, blobs) = session_with_files(NOTES, notes_under(12), Some(changes), cx);
    for i in 0..12 {
        blobs.set(&format!("Note/n{i:03}/attachment"), &png(40, 30, false));
    }
    cx.run_until_parked();
    session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
    let (cards, cx) = open(session, opening("Note"), cx);
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.rows.len(), 13);
        assert_eq!(cards.rows[0].id, "n900");
        for row in 0..12 {
            assert!(
                matches!(cards.covers[&cover_key(row)].look, Loadable::Loaded(_)),
                "cover {row}"
            );
        }
    });
    blobs.reads.lock().unwrap().clear();
    let diffs = cards.read_with(cx, |cards, _| cards.diff.clone().map(|diff| diff.counts()));

    write_filter(&cards, "title", FilterOp::Contains, "1", cx);
    assert_eq!(row_ids(&cards, cx), ["n900", "n001", "n010", "n011"]);
    cards.update(cx, |cards, cx| {
        assert_eq!(cards.cursor, Some(1), "the first card the branch still has");
        assert_eq!(cards.selected_row_id(), Some("n001"));
        assert_eq!(cards.change_of("n001"), Some(NetChange::Added));
        assert!(cards.is_removed("n900"));
        assert_eq!(cards.diff.as_ref().map(|diff| diff.counts()), diffs);
        assert!(!cards.diff_pending);
        assert!(matches!(
            cards.covers[&cover_key(1)].look,
            Loadable::Loaded(Look::Image { .. })
        ));
        // Past the cap, what the filter no longer shows is let go; what it shows is laid out
        // and stays.
        let hidden = [0, 2, 3, 4, 5, 6, 7, 8, 9].map(cover_key);
        for key in &hidden {
            assert!(cards.covers[key].held() > 0, "kept through the filter");
        }
        cards.held = 2 * PREVIEWS_HELD;
        cards.let_go_past_cap(cx);
        for key in &hidden {
            assert_eq!(cards.covers[key].held(), 0, "{key:?} is still held");
        }
        for shown in [1, 10, 11] {
            assert!(cards.covers[&cover_key(shown)].held() > 0);
        }
    });
    assert!(
        blobs.reads.lock().unwrap().is_empty(),
        "a file was read again: {:?}",
        blobs.reads.lock().unwrap()
    );
}

/// A test that takes no value ("is today", "is empty") has no value field, so its draft is
/// written with the cards' keyboard: ↵ adds it and opens no page, Esc closes it and leaves the
/// picks, and the keys that act on cards wait for one of them. The field, which is not drawn,
/// does not keep the keyboard where no key would reach the tab.
#[gpui::test]
async fn a_draft_with_no_value_field_takes_enter_and_esc(cx: &mut TestAppContext) {
    init(cx);
    let (session, _, served) = session_served(TASKS, tasks_answering(6), cx);
    let heard: Rc<RefCell<Vec<String>>> = Rc::default();
    cx.update(|cx| {
        let log = heard.clone();
        cx.on_action(move |open: &OpenNodePage, _| log.borrow_mut().push(open.id.clone()));
    });
    let (cards, cx) = open(session, opening("Task"), cx);
    cx.simulate_keystrokes("right x");
    let no_field = |cards: &Entity<TypeCards>, cx: &mut VisualTestContext| {
        cx.simulate_keystrokes("cmd-f");
        cx.run_until_parked();
        cards.update_in(cx, |cards, window, cx| {
            // As the key menu's "due" and the test menu's "is empty" leave it.
            cards.set_draft_property("due".into(), window, cx);
            cards.set_draft_op(FilterOp::IsEmpty, window, cx);
        });
        cx.run_until_parked();
        cards.update_in(cx, |cards, window, cx| {
            assert!(cards.draft_has_no_field());
            assert!(
                !cards.bar().editor.focus_handle(cx).is_focused(window),
                "a field that is not drawn has the keyboard"
            );
            assert!(cards.focus_handle.is_focused(window), "the cards have it");
        });
    };

    no_field(&cards, cx);
    // The cards' keys wait: the draft is what is being written.
    cx.simulate_keystrokes("right down x g");
    cards.read_with(cx, |cards, _| {
        assert_eq!((cards.cursor, cards.picks.len()), (Some(1), 1));
        assert!(cards.bar().draft.is_some());
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cards.read_with(cx, |cards, _| {
        assert!(cards.bar().draft.is_none(), "Esc closed it");
        assert_eq!(cards.picks.len(), 1, "and left the picks");
        assert!(cards.filters.is_empty());
    });
    cx.simulate_keystrokes("right");
    cards.read_with(cx, |cards, _| assert_eq!(cards.cursor, Some(2)));

    no_field(&cards, cx);
    cx.simulate_keystrokes("enter");
    settle(cx);
    assert!(heard.borrow().is_empty(), "↵ opened a page");
    cards.update_in(cx, |cards, window, _| {
        assert_eq!(
            cards.filters,
            [Filter {
                property: "due".into(),
                op: FilterOp::IsEmpty,
                value: Value::Null,
            }],
            "↵ added it"
        );
        assert!(cards.bar().draft.is_none());
        assert!(cards.focus_handle.is_focused(window));
    });
    assert_eq!(served.sent("studio_page").len(), 2);
    // With no draft, ↵ is the cards' again.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(heard.borrow().len(), 1);
}
