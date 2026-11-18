//! The gallery's keys, paging, reads and drawing, against the fake server (spec 035).

use gpui::{KeyBinding, TestAppContext, VisualTestContext};
use serde_json::json;

use super::*;
use crate::test_support::{Blobs, session_with_files};

const NOTES: &str =
    "node Note {\n  slug: String @key\n  title: String\n  attachment: Blob?\n  scan: Blob?\n}\n";

/// `total` notes, `n000` on, each titled.
fn note(i: usize) -> Value {
    json!({"n": {"@id": format!("n{i:03}"), "slug": format!("n{i:03}"), "title": format!("Note {i}")}})
}

/// Answers the gallery's reads of `total` notes: a keyset page after its `after` id, at most its
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
            KeyBinding::new("left", MoveLeft, Some("Gallery")),
            KeyBinding::new("right", MoveRight, Some("Gallery")),
            KeyBinding::new("up", MoveUp, Some("Gallery")),
            KeyBinding::new("down", MoveDown, Some("Gallery")),
            KeyBinding::new("home", MoveToFirst, Some("Gallery")),
            KeyBinding::new("end", MoveToLast, Some("Gallery")),
            KeyBinding::new("x", TogglePick, Some("Gallery")),
            KeyBinding::new("cmd-a", PickAll, Some("Gallery")),
            KeyBinding::new("escape", ClearPicks, Some("Gallery")),
            KeyBinding::new("f", ShowNextFile, Some("Gallery")),
            KeyBinding::new("=", LargerTiles, Some("Gallery")),
            KeyBinding::new("-", SmallerTiles, Some("Gallery")),
        ]);
    });
}

fn opening(type_name: &str) -> ShowSetInGallery {
    ShowSetInGallery {
        title: type_name.into(),
        type_name: type_name.into(),
        filters: None,
        sort: None,
        ids: None,
        property: None,
        at_commit: None,
        snapshot: None,
        select: None,
        preview: false,
    }
}

/// A gallery of `opening` in a window, its schema read first, focused, settled.
fn open(
    session: Entity<GraphSession>,
    opening: ShowSetInGallery,
    cx: &mut TestAppContext,
) -> (Entity<TypeGallery>, &mut VisualTestContext) {
    cx.run_until_parked();
    let (gallery, cx) = cx.add_window_view(|window, cx| {
        TypeGallery::new(
            WeakEntity::new_invalid(),
            session,
            opening,
            TileSize::Medium,
            window,
            cx,
        )
    });
    gallery.update_in(cx, |gallery, window, cx| {
        window.focus(&gallery.focus_handle, cx)
    });
    settle(cx);
    (gallery, cx)
}

/// Lets reads land, then the pause before tiles are read, then those reads.
fn settle(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.executor().advance_clock(READ_AFTER);
    cx.run_until_parked();
    cx.executor().advance_clock(LANDING_DELAY);
    cx.run_until_parked();
}

fn grid_renders(gallery: &Entity<TypeGallery>, cx: &mut VisualTestContext) -> usize {
    let grid = gallery.read_with(cx, |gallery, _| gallery.grid.clone());
    grid.read_with(cx, |grid, _| grid.renders)
}

fn columns(gallery: &Entity<TypeGallery>, cx: &mut VisualTestContext) -> usize {
    gallery.read_with(cx, |gallery, _| gallery.line_layout().0)
}

/// Each line's tiles fill it: as many of at least the size as fit, with the edges and gaps
/// kept, and one when the line is narrower than a tile.
#[test]
fn a_lines_tiles_fill_it() {
    let edge = studio_ui::ds::size::TILE;
    let (columns, tile) = line_layout(1200., edge);
    assert_eq!(columns, 6);
    let used = columns as f32 * tile + (columns as f32 - 1.) * space::LG + 2. * space::EDGE;
    assert!((used - 1200.).abs() < 0.01, "{used}");
    assert!(tile >= edge);
    let (columns, tile) = line_layout(100., edge);
    assert_eq!(columns, 1);
    assert_eq!(tile, edge);
    // Larger tiles, fewer to a line.
    assert!(line_layout(1200., studio_ui::ds::size::TILE_LARGE).0 < 6);
    assert!(line_layout(1200., studio_ui::ds::size::TILE_SMALL).0 > 6);
}

/// The arrows move across the grid: across a line's ends, a line up and down in the same
/// column, ↓ above a short last line to the last tile, Home and End; X picks, ⌘A picks every
/// tile, Esc takes them out; = and − change the tiles' size.
#[gpui::test]
async fn the_keys_move_across_the_grid_and_pick(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(13), None, cx);
    let (gallery, cx) = open(session, opening("Note"), cx);
    let columns = columns(&gallery, cx);
    assert!((2..13).contains(&columns), "{columns} columns");
    let cursor = |cx: &mut VisualTestContext| gallery.read_with(cx, |g, _| g.cursor);
    assert_eq!(cursor(cx), Some(0));

    cx.simulate_keystrokes("right");
    assert_eq!(cursor(cx), Some(1));
    cx.simulate_keystrokes("left left");
    assert_eq!(cursor(cx), Some(0), "the first tile is the first");
    cx.simulate_keystrokes("down");
    assert_eq!(cursor(cx), Some(columns), "a line down, the same column");
    cx.simulate_keystrokes("up");
    assert_eq!(cursor(cx), Some(0));
    cx.simulate_keystrokes("up");
    assert_eq!(cursor(cx), Some(0), "above the first line it stays");
    // Along the line's end to the next line's start.
    for _ in 0..columns {
        cx.simulate_keystrokes("right");
    }
    assert_eq!(cursor(cx), Some(columns));
    cx.simulate_keystrokes("end");
    assert_eq!(cursor(cx), Some(12));
    cx.simulate_keystrokes("home");
    assert_eq!(cursor(cx), Some(0));
    // ↓ from a line whose column the short last line lacks goes to the last tile.
    let last_line_start = 12 / columns * columns;
    let above = last_line_start - columns + columns - 1;
    if 12 % columns != columns - 1 && above + columns > 12 {
        gallery.update(cx, |gallery, cx| gallery.set_cursor(above, cx));
        cx.simulate_keystrokes("down");
        assert_eq!(cursor(cx), Some(12));
    }

    cx.simulate_keystrokes("home x right x");
    gallery.read_with(cx, |gallery, _| {
        assert_eq!(
            gallery.picks.iter().cloned().collect::<Vec<_>>(),
            ["n000", "n001"]
        );
    });
    cx.simulate_keystrokes("x");
    gallery.read_with(cx, |gallery, _| assert_eq!(gallery.picks.len(), 1));
    cx.simulate_keystrokes("cmd-a");
    gallery.read_with(cx, |gallery, _| assert_eq!(gallery.picks.len(), 13));
    cx.simulate_keystrokes("escape");
    gallery.read_with(cx, |gallery, _| assert!(gallery.picks.is_empty()));

    cx.simulate_keystrokes("=");
    gallery.read_with(cx, |gallery, _| assert_eq!(gallery.size, TileSize::Large));
    cx.simulate_keystrokes("- - -");
    gallery.read_with(cx, |gallery, _| assert_eq!(gallery.size, TileSize::Small));

    // F shows the type's next file, then the first again.
    gallery.read_with(cx, |gallery, _| {
        assert_eq!(gallery.property.as_deref(), Some("attachment"))
    });
    cx.simulate_keystrokes("f");
    gallery.read_with(cx, |gallery, _| {
        assert_eq!(gallery.property.as_deref(), Some("scan"))
    });
    cx.simulate_keystrokes("f");
    gallery.read_with(cx, |gallery, _| {
        assert_eq!(gallery.property.as_deref(), Some("attachment"))
    });
}

/// The first page is read alone; the next only once the lines drawn come within a page of the
/// last row read, at the first page's commit; a set read to its end reads nothing more.
#[gpui::test]
async fn pages_are_read_as_the_lines_drawn_near_the_end(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(500), None, cx);
    let (gallery, cx) = open(session, opening("Note"), cx);
    gallery.read_with(cx, |gallery, _| {
        assert_eq!(
            gallery.rows.len(),
            200,
            "a first page is two of the graph's pages"
        );
        assert!(!gallery.paging.exhausted);
        assert_eq!(gallery.paging.pinned.as_deref(), Some("c1"));
        assert_eq!(gallery.count, Loadable::Loaded(500));
        assert_eq!(gallery.count_text(), "200 of 500 read");
    });
    // Near the end of what was read, the next page is read.
    let columns = columns(&gallery, cx);
    gallery.update(cx, |gallery, cx| {
        let line = 150 / columns;
        gallery.scroll.scroll_to_item(line, ScrollStrategy::Top);
        cx.notify();
    });
    settle(cx);
    gallery.read_with(cx, |gallery, _| {
        assert_eq!(gallery.rows.len(), 500, "the second page read the rest");
        assert!(gallery.paging.exhausted);
        assert_eq!(gallery.count_text(), "500 rows");
        let ids: HashSet<&str> = gallery.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids.len(), 500, "no row twice");
    });
}

/// A gallery of rows picked in a table holds them alone, in the order they were picked, read
/// by id.
#[gpui::test]
async fn a_gallery_of_listed_rows_holds_them_in_their_order(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(20), None, cx);
    let listed = ShowSetInGallery {
        ids: Some(vec!["n007".into(), "n002".into(), "n015".into()]),
        select: Some("n002".into()),
        ..opening("Note")
    };
    let (gallery, cx) = open(session, listed, cx);
    gallery.read_with(cx, |gallery, _| {
        let ids: Vec<&str> = gallery.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["n007", "n002", "n015"]);
        assert_eq!(gallery.cursor, Some(1), "the table's cursor row");
        assert!(gallery.paging.exhausted);
        assert_eq!(gallery.count_text(), "3 rows");
    });
}

/// Only the tiles on the lines drawn are read, once scrolling rests, at most four at once;
/// each reads what its cell holds, its first bytes and, for a picture, the rest; a null cell
/// reads nothing past its `HEAD`. A read for tiles scrolled away before it was sent is
/// dropped, and nothing of theirs is asked.
#[gpui::test]
async fn only_the_tiles_drawn_are_read_and_those_scrolled_away_are_dropped(
    cx: &mut TestAppContext,
) {
    init(cx);
    let (session, blobs) = session_with_files(NOTES, notes_answering(400), None, cx);
    let picture = {
        let mut bytes = Vec::new();
        image::RgbaImage::from_pixel(600, 300, image::Rgba([10, 120, 200, 255]))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    };
    blobs.set("Note/n000/attachment", &picture);
    blobs.set("Note/n001/attachment", b"plain text, short");
    // n002 holds none: its HEAD answers 404, which the client reads as null.
    let (gallery, cx) = open(session, opening("Note"), cx);
    let reads = |blobs: &Blobs| blobs.reads.lock().unwrap().clone();
    let first = reads(&blobs);
    let (drawn, per_line) = gallery.read_with(cx, |gallery, _| {
        (gallery.lines_drawn.clone(), gallery.line_layout().0)
    });
    let tiles_drawn = (drawn.end * per_line).min(400);
    let heads = first.iter().filter(|read| read.starts_with("HEAD")).count();
    assert_eq!(heads, tiles_drawn, "a HEAD for each tile drawn: {first:?}");
    assert!(first.contains(&"HEAD Note/n000/attachment".to_owned()));
    assert!(
        !first.iter().any(|read| read.contains("/n399/")),
        "nothing far below is read"
    );
    // The picture is read in two: its first 64 KiB judged it, then the rest. Its size here is
    // under 64 KiB, so one range holds it whole.
    let picture_reads: Vec<&String> = first
        .iter()
        .filter(|read| read.starts_with("GET Note/n000/"))
        .collect();
    assert_eq!(picture_reads.len(), 1, "{picture_reads:?}");
    assert!(
        !first.iter().any(|read| read.starts_with("GET Note/n002/")),
        "a null cell reads no bytes"
    );
    gallery.read_with(cx, |gallery, _| {
        let tile = |id: &str| gallery.tiles.get(&(id.to_owned(), "attachment".to_owned()));
        assert!(matches!(
            tile("n000").map(|tile| &tile.look),
            Some(Loadable::Loaded(Look::Image { .. }))
        ));
        assert_eq!(
            tile("n000")
                .and_then(|tile| tile.facts)
                .and_then(|facts| facts.pixels),
            Some((600, 300))
        );
        assert!(matches!(
            tile("n001").map(|tile| &tile.look),
            Some(Loadable::Loaded(Look::Text(_)))
        ));
        assert!(matches!(
            tile("n002").map(|tile| &tile.meta),
            Some(Loadable::Loaded(BlobMeta::Null))
        ));
        assert!(gallery.tile_tasks.is_empty(), "every read ended");
        assert!(gallery.held > 0);
    });

    // Tiles asked for and then scrolled past before their reads were sent ask nothing.
    blobs.reads.lock().unwrap().clear();
    let (lines, far) = gallery.read_with(cx, |gallery, _| {
        let lines = gallery.line_count();
        (lines - 4..lines - 2, lines - 1..lines)
    });
    gallery.update(cx, |gallery, cx| {
        let columns = gallery.line_layout().0;
        gallery.read_tiles(lines, cx);
        assert_eq!(gallery.tile_tasks.len(), READS_AT_ONCE, "four at once");
        assert_eq!(gallery.tile_queue.len(), 2 * columns - READS_AT_ONCE);
        gallery.read_tiles(far.clone(), cx);
        assert!(
            gallery
                .tile_tasks
                .keys()
                .chain(gallery.tile_queue.iter())
                .all(|(id, _)| {
                    let ix: usize = id.trim_start_matches('n').parse().unwrap();
                    (far.start * columns..far.end * columns).contains(&ix)
                }),
            "only the lines now drawn are read"
        );
    });
    cx.run_until_parked();
    let later = reads(&blobs);
    let columns = columns(&gallery, cx);
    assert!(
        later.iter().all(|read| {
            let id = read.split('/').nth(1).unwrap();
            let ix: usize = id.trim_start_matches('n').parse().unwrap();
            (far.start * columns..far.end * columns).contains(&ix)
        }),
        "a read for a tile scrolled away was sent: {later:?}"
    );
}

/// Past the cap, the previews farthest from the lines drawn are let go, freeing what they
/// held; those drawn stay, even past the cap, so a screen is never read again and again.
#[gpui::test]
async fn previews_past_the_cap_are_let_go_farthest_first(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(100), None, cx);
    let (gallery, cx) = open(session, opening("Note"), cx);
    gallery.update(cx, |gallery, cx| {
        gallery.release_all(cx);
        gallery.lines_drawn = 0..1;
        let each = PREVIEWS_HELD / 4;
        for id in ["n000", "n050", "n099", "n001"] {
            let image = crate::blob::ShownImage::from_rgba(1, 1, vec![0; 4]).unwrap();
            gallery.tiles.insert(
                (id.to_owned(), "attachment".to_owned()),
                Tile {
                    look: Loadable::Loaded(Look::Image { image, bytes: each }),
                    ..Tile::default()
                },
            );
            gallery.held += each;
        }
        gallery.held += 1;
        gallery.let_go_past_cap(cx);
        let kept = |gallery: &TypeGallery, id: &str| {
            matches!(
                gallery.tiles[&(id.to_owned(), "attachment".to_owned())].look,
                Loadable::Loaded(Look::Image { .. })
            )
        };
        assert!(kept(gallery, "n000") && kept(gallery, "n001") && kept(gallery, "n050"));
        assert!(!kept(gallery, "n099"), "the farthest went");
        assert!(gallery.held <= PREVIEWS_HELD);

        // Past the cap with only the tiles drawn holding previews, they keep them.
        gallery.held = PREVIEWS_HELD * 2;
        gallery.let_go_past_cap(cx);
        assert!(
            kept(gallery, "n000") && kept(gallery, "n001"),
            "the tiles drawn keep theirs"
        );
    });
}

/// On a branch, a tile is marked as the branch changed its row (spec 033): the rows it
/// removed come first, read where they last were; an added row, a changed one, and one whose
/// file changed say so; the toolbar counts them.
#[gpui::test]
async fn a_branch_marks_its_tiles_and_shows_removed_rows_first(cx: &mut TestAppContext) {
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
    let (gallery, cx) = open(session, opening("Note"), cx);
    gallery.read_with(cx, |gallery, _| {
        let ids: Vec<&str> = gallery.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["n900", "n000", "n001", "n002"]);
        assert_eq!(gallery.change_of("n000"), Some(NetChange::Added));
        assert_eq!(gallery.change_of("n001"), Some(NetChange::Changed));
        assert!(gallery.file_changed("n001"));
        assert!(!gallery.file_changed("n000"));
        assert!(gallery.is_removed("n900"));
        assert_eq!(gallery.diff.as_ref().unwrap().counts(), (1, 1, 1));
        assert_eq!(
            gallery.cursor,
            Some(1),
            "the cursor stays on the first tile read"
        );
        assert_eq!(gallery.selected_row_id(), Some("n000"));
        assert_eq!(gallery.diff_base.as_deref(), Some("c0"));
    });
    // The removed row's file is read at the merge base, where it last was.
    assert!(
        blobs
            .reads
            .lock()
            .unwrap()
            .iter()
            .any(|read| read.starts_with("HEAD Note/n900/attachment")),
    );
    // A removed tile is not picked.
    gallery.update(cx, |gallery, cx| gallery.pick(0, false, cx));
    gallery.read_with(cx, |gallery, _| assert!(gallery.picks.is_empty()));
}

/// The grid is a cached view of its own: it follows the gallery's `notify`, a key that moves
/// the cursor draws it once, and previews landing together draw it once, not once a tile.
#[gpui::test]
async fn a_key_draws_the_grid_once_and_previews_land_together(cx: &mut TestAppContext) {
    init(cx);
    let (session, blobs) = session_with_files(NOTES, notes_answering(40), None, cx);
    for i in 0..8 {
        blobs.set(
            &format!("Note/n{i:03}/attachment"),
            format!("text {i}").as_bytes(),
        );
    }
    let (gallery, cx) = open(session, opening("Note"), cx);
    // The first key switches GPUI to keyboard input, which draws the window once.
    cx.simulate_keystrokes("right");
    cx.run_until_parked();
    let before = grid_renders(&gallery, cx);
    cx.simulate_keystrokes("right");
    cx.run_until_parked();
    assert_eq!(grid_renders(&gallery, cx), before + 1, "one draw a key");
    cx.simulate_keystrokes("left left");
    cx.run_until_parked();
    assert_eq!(grid_renders(&gallery, cx), before + 3, "one draw a key");

    // Every tile read again: the tiles drawn land, and the grid is drawn far fewer times
    // than the tiles that landed.
    gallery.update(cx, |gallery, cx| {
        gallery.release_all(cx);
        gallery.lines_read = None;
        cx.notify();
    });
    cx.run_until_parked();
    let before = grid_renders(&gallery, cx);
    let landed = gallery.read_with(cx, |gallery, _| {
        gallery.keys_on(gallery.lines_drawn.clone()).len()
    });
    cx.executor().advance_clock(READ_AFTER);
    cx.run_until_parked();
    cx.executor().advance_clock(LANDING_DELAY);
    cx.run_until_parked();
    let drew = grid_renders(&gallery, cx) - before;
    gallery.read_with(cx, |gallery, _| {
        let read = gallery
            .tiles
            .values()
            .filter(|tile| matches!(tile.look, Loadable::Loaded(_)))
            .count();
        assert_eq!(read, landed);
    });
    assert_eq!(drew, 1, "{drew} draws for {landed} tiles landing together");
}
