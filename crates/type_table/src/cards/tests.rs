//! The cards' keys, paging, reads and drawing, against the fake server (spec 048; they began as
//! the tests of spec 035's gallery).

use std::cell::RefCell;

use gpui::{Bounds, KeyBinding, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels};
use gpui::{TestAppContext, VisualTestContext};
use serde_json::json;

use super::*;
use crate::test_support::session_with_files;

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
        editor::init(cx);
        cx.bind_keys([
            KeyBinding::new("left", MoveLeft, Some("Cards")),
            KeyBinding::new("right", MoveRight, Some("Cards")),
            KeyBinding::new("up", MoveUp, Some("Cards")),
            KeyBinding::new("down", MoveDown, Some("Cards")),
            KeyBinding::new("home", MoveToFirst, Some("Cards")),
            KeyBinding::new("end", MoveToLast, Some("Cards")),
            KeyBinding::new("enter", OpenPage, Some("Cards")),
            KeyBinding::new("cmd-enter", OpenPage, Some("Cards")),
            KeyBinding::new("alt-enter", OpenCover, Some("Cards")),
            KeyBinding::new("g", ShowOnCanvas, Some("Cards && !editing")),
            KeyBinding::new("x", TogglePick, Some("Cards && !editing")),
            KeyBinding::new("cmd-a", PickAll, Some("Cards")),
            KeyBinding::new("escape", Cancel, Some("Cards")),
            KeyBinding::new("f", ShowNextCover, Some("Cards && !editing")),
            KeyBinding::new("=", LargerCards, Some("Cards && !editing")),
            KeyBinding::new("-", SmallerCards, Some("Cards && !editing")),
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

/// The cards of `opening` in a window, their schema read first, focused, settled.
fn open(
    session: Entity<GraphSession>,
    opening: ShowSetInCards,
    cx: &mut TestAppContext,
) -> (Entity<TypeCards>, &mut VisualTestContext) {
    cx.run_until_parked();
    let (cards, cx) = cx.add_window_view(|window, cx| {
        TypeCards::new(
            WeakEntity::new_invalid(),
            session,
            opening,
            CardSize::Medium,
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
/// cover's box, 4 by 3 across it, and its title line.
#[test]
fn the_columns_fill_the_width() {
    let edge = studio_ui::ds::size::CARD;
    let grid = grid_for(1200., edge);
    assert_eq!(grid.columns(), 4);
    let columns = grid.columns() as f32;
    let used = columns * grid.width() + (columns - 1.) * space::LG + 2. * space::EDGE;
    assert!((used - 1200.).abs() < 0.01, "{used}");
    assert!(grid.width() >= edge);
    let cover = grid.card_height() - 2. * card_inset() - studio_ui::ds::size::CARD_TITLE;
    let inner = grid.width() - 2. * card_inset();
    assert!((cover - inner * 3. / 4.).abs() <= 0.5, "{cover} of {inner}");
    assert_eq!(cover.fract(), 0., "on whole pixels");
    let narrow = grid_for(100., edge);
    assert_eq!((narrow.columns(), narrow.width()), (1, edge));
    assert!(grid_for(1200., studio_ui::ds::size::CARD_LARGE).columns() < 4);
    assert!(grid_for(1200., studio_ui::ds::size::CARD_SMALL).columns() > 4);
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
        assert_eq!(cards.size, CardSize::Small);
        assert!(
            cards.card_grid().columns() > columns,
            "narrower columns, more of them"
        );
        assert_eq!(cards.placed(), 41);
    });
    cx.simulate_keystrokes("= =");
    settle(cx);
    cards.read_with(cx, |cards, _| {
        assert_eq!(cards.size, CardSize::Large);
        assert!(cards.card_grid().columns() < columns);
    });
    let reads = blobs.reads.lock().unwrap().clone();
    let once: HashSet<&String> = reads.iter().collect();
    assert_eq!(once.len(), reads.len(), "a cover was read twice: {reads:?}");

    cx.simulate_keystrokes("f");
    cards.read_with(cx, |c, _| assert_eq!(c.cover.as_deref(), Some("scan")));
    cx.simulate_keystrokes("f");
    cards.read_with(cx, |c, _| {
        assert_eq!(c.cover.as_deref(), Some("attachment"))
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
/// `cards` tab. A kind nothing registers is dropped, which is why the old one is kept.
#[gpui::test]
async fn a_saved_gallery_tab_comes_back_as_cards(cx: &mut TestAppContext) {
    init(cx);
    cx.update(|cx| {
        studio_settings::init_test(cx);
        studio_settings::keep_changes_in_memory(cx);
        crate::init(cx);
    });
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
        assert_eq!(cards.size, CardSize::Large, "that size");
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
    assert_eq!(saved.data["cover"], json!("scan"));
    assert_eq!(saved.data.get("property"), None);
    assert_eq!(saved.data["size"], json!("large"));
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
        assert_eq!(cards.size, CardSize::Small);
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
    // The data alone, as the opener reads it.
    let (opening, size) = crate::saved_cards(&filtered.data).expect("read");
    assert_eq!(
        (opening.cover.as_deref(), size),
        (Some("scan"), CardSize::Large)
    );
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
