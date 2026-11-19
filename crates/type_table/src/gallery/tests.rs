//! The gallery's keys, paging, reads and drawing, against the fake server (spec 035).

use gpui::{KeyBinding, TestAppContext, VisualTestContext};
use serde_json::json;

use super::*;
use crate::test_support::session_with_files;

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

/// Lets reads land, then the pause before tiles are read, then those reads, then the draw of
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

fn grid_renders(gallery: &Entity<TypeGallery>, cx: &mut VisualTestContext) -> usize {
    let grid = gallery.read_with(cx, |gallery, _| gallery.grid.clone());
    grid.read_with(cx, |grid, _| grid.renders)
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

/// Each row's place, by row, as the gallery holds them.
fn places(gallery: &Entity<TypeGallery>, cx: &mut VisualTestContext) -> Vec<Place> {
    gallery.read_with(cx, |gallery, _| {
        (0..gallery.masonry.placed())
            .map(|row| *gallery.masonry.place_of(row).unwrap())
            .collect()
    })
}

/// The columns fill the width: as many of at least the tile size as fit, with the edges and
/// gaps kept, and one when the width is narrower than a column.
#[test]
fn the_columns_fill_the_width() {
    let edge = studio_ui::ds::size::TILE;
    let (columns, width) = column_layout(1200., edge);
    assert_eq!(columns, 6);
    let used = columns as f32 * width + (columns as f32 - 1.) * space::LG + 2. * space::EDGE;
    assert!((used - 1200.).abs() < 0.01, "{used}");
    assert!(width >= edge);
    let (columns, width) = column_layout(100., edge);
    assert_eq!(columns, 1);
    assert_eq!(width, edge);
    assert!(column_layout(1200., studio_ui::ds::size::TILE_LARGE).0 < 6);
    assert!(column_layout(1200., studio_ui::ds::size::TILE_SMALL).0 > 6);
}

/// Tiles are as tall as their files' shapes, from the pictures' headers, and the kinds'
/// shapes where a file has none: a row without a file is a short band.
#[gpui::test]
async fn a_tile_is_as_tall_as_its_files_shape(cx: &mut TestAppContext) {
    init(cx);
    let (session, blobs) = session_with_files(NOTES, notes_answering(4), None, cx);
    blobs.set("Note/n000/attachment", &png(200, 400, false));
    blobs.set("Note/n001/attachment", &png(400, 100, false));
    blobs.set("Note/n002/attachment", b"%PDF-1.4\n%a page\n");
    // n003 holds none.
    let (gallery, cx) = open(session, opening("Note"), cx);
    gallery.read_with(cx, |gallery, _| {
        let m = &gallery.masonry;
        assert_eq!(m.placed(), 4);
        let preview = |row: usize| m.place_of(row).unwrap().height - tile_chrome();
        let width = m.width();
        assert!((preview(0) - width * 2.).abs() < 0.01, "a tall picture");
        assert!(
            (preview(1) - width * 0.4).abs() < 0.01,
            "a wide one, held to the bound"
        );
        assert!(
            (preview(2) - width * tile::shape::DOCUMENT).abs() < 0.01,
            "a page"
        );
        assert!(
            (preview(3) - width * tile::shape::NO_FILE).abs() < 0.01,
            "no file"
        );
    });
}

/// Up and down stay in a column; left and right go to the next column's tile whose middle is
/// nearest; Home and End, X, ⌘A and Esc, = and −, and F as before. A size change flows the
/// columns again from the shapes kept, reading nothing.
#[gpui::test]
async fn the_arrows_move_within_and_across_columns(cx: &mut TestAppContext) {
    init(cx);
    let (session, blobs) = session_with_files(NOTES, notes_answering(40), None, cx);
    // Shapes that leave the columns uneven: tall, square, wide, by turns.
    for i in 0..40 {
        let (w, h) = [(100, 200), (100, 100), (200, 100)][i % 3];
        blobs.set(&format!("Note/n{i:03}/attachment"), &png(w, h, false));
    }
    let (gallery, cx) = open(session, opening("Note"), cx);
    let (columns, placed) = gallery.read_with(cx, |g, _| (g.masonry.columns(), g.masonry.placed()));
    assert!((3..20).contains(&columns), "{columns} columns");
    assert_eq!(placed, 40);
    let cursor = |cx: &mut VisualTestContext| gallery.read_with(cx, |g, _| g.cursor.unwrap());
    let column_of = |cx: &mut VisualTestContext, row: usize| places(&gallery, cx)[row].column;
    assert_eq!(cursor(cx), 0);

    // Down the first column, and back up.
    let below = gallery
        .read_with(cx, |g, _| g.masonry.vertical(0, 1))
        .unwrap();
    cx.simulate_keystrokes("down");
    assert_eq!(cursor(cx), below);
    assert_eq!(column_of(cx, below), 0, "down stays in its column");
    cx.simulate_keystrokes("up up");
    assert_eq!(cursor(cx), 0, "above the first it stays");

    // Right, from the lower tile: the next column's tile whose middle is nearest.
    cx.simulate_keystrokes("down right");
    let at = cursor(cx);
    let all = places(&gallery, cx);
    assert_eq!(all[at].column, 1);
    let middle = all[below].middle();
    let nearest = (0..all.len())
        .filter(|&row| all[row].column == 1)
        .min_by(|&a, &b| {
            (all[a].middle() - middle)
                .abs()
                .total_cmp(&(all[b].middle() - middle).abs())
        })
        .unwrap();
    assert_eq!(at, nearest);
    cx.simulate_keystrokes("left");
    let at = cursor(cx);
    assert_eq!(column_of(cx, at), 0);
    cx.simulate_keystrokes("left");
    assert_eq!(cursor(cx), at, "nothing left of the first column");
    cx.simulate_keystrokes("end");
    assert_eq!(cursor(cx), 39);
    cx.simulate_keystrokes("home");
    assert_eq!(cursor(cx), 0);

    cx.simulate_keystrokes("x down x");
    gallery.read_with(cx, |gallery, _| {
        let picked: Vec<&str> = gallery.picks.iter().map(String::as_str).collect();
        assert_eq!(picked, ["n000", gallery.rows[below].id.as_str()]);
    });
    cx.simulate_keystrokes("cmd-a");
    gallery.read_with(cx, |gallery, _| assert_eq!(gallery.picks.len(), 40));
    cx.simulate_keystrokes("escape");
    gallery.read_with(cx, |gallery, _| assert!(gallery.picks.is_empty()));

    // A size change flows the columns again from the shapes kept, reading nothing.
    let read_before = blobs.reads.lock().unwrap().len();
    cx.simulate_keystrokes("- -");
    settle(cx);
    gallery.read_with(cx, |gallery, _| {
        assert_eq!(gallery.size, TileSize::Small);
        assert!(
            gallery.masonry.columns() > columns,
            "narrower columns, more of them"
        );
        assert_eq!(gallery.masonry.placed(), 40);
    });
    assert_eq!(
        blobs.reads.lock().unwrap().len(),
        read_before,
        "the shapes kept laid the columns out"
    );
    cx.simulate_keystrokes("= =");
    gallery.read_with(cx, |gallery, _| assert_eq!(gallery.size, TileSize::Large));

    cx.simulate_keystrokes("f");
    gallery.read_with(cx, |g, _| assert_eq!(g.property.as_deref(), Some("scan")));
    cx.simulate_keystrokes("f");
    gallery.read_with(cx, |g, _| {
        assert_eq!(g.property.as_deref(), Some("attachment"))
    });
}

/// A placed tile keeps its position when later shapes arrive: with the fourth file's bytes
/// held, the three before it are placed and nothing after it, whatever landed, and a band
/// waits under the columns; once they come, the rest are placed after them, and the first
/// three are where they were.
#[gpui::test]
async fn a_placed_tile_keeps_its_place_when_later_shapes_arrive(cx: &mut TestAppContext) {
    init(cx);
    let (session, blobs) = session_with_files(NOTES, notes_answering(8), None, cx);
    for i in 0..8 {
        let (w, h) = [(100, 160), (160, 100), (100, 100), (100, 220)][i % 4];
        blobs.set(&format!("Note/n{i:03}/attachment"), &png(w, h, false));
    }
    let held = blobs.hold_reads("Note/n003/attachment");
    let (gallery, cx) = open(session, opening("Note"), cx);
    let early = places(&gallery, cx);
    assert_eq!(early.len(), 3, "placed up to the shape not known");
    gallery.read_with(cx, |gallery, _| {
        let key = |i: usize| (format!("n{i:03}"), "attachment".to_owned());
        assert!(
            gallery
                .tiles
                .get(&key(5))
                .is_some_and(|t| t.shape.is_some()),
            "a later shape landed and waits"
        );
        assert!(gallery.masonry.placed() < gallery.rows.len());
    });
    drop(held);
    settle(cx);
    let all = places(&gallery, cx);
    assert_eq!(all.len(), 8);
    assert_eq!(&all[..3], &early[..], "a placed tile moved");
}

/// The tiles shown have their files read and drawn; rows a screen below are measured from
/// their first bytes alone; nothing further is read. Reads for tiles no longer wanted before
/// they were sent ask nothing.
#[gpui::test]
async fn the_tiles_shown_are_drawn_and_the_rows_below_measured(cx: &mut TestAppContext) {
    init(cx);
    let (session, blobs) = session_with_files(NOTES, notes_answering(400), None, cx);
    let big = png(300, 300, true);
    assert!(big.len() as u64 > FIRST_READ, "{}", big.len());
    for i in 0..80 {
        blobs.set(&format!("Note/n{i:03}/attachment"), &big);
    }
    let (gallery, cx) = open(session, opening("Note"), cx);
    let reads = blobs.reads.lock().unwrap().clone();
    let (shown, placed) = gallery.read_with(cx, |gallery, _| {
        (
            gallery.masonry.meeting(gallery.span.clone()),
            gallery.masonry.placed(),
        )
    });
    assert!(!shown.is_empty());
    assert!(
        placed > *shown.last().unwrap() + 1,
        "rows below were measured and placed"
    );
    assert!(placed < 400, "not every row: {placed}");
    let rest = |row: usize| {
        let cell = format!("GET Note/n{row:03}/attachment 65536-");
        reads.iter().any(|read| read.starts_with(&cell))
    };
    let first = |row: usize| {
        let cell = format!("GET Note/n{row:03}/attachment 0-65535");
        reads.contains(&cell)
    };
    for &row in &shown {
        assert!(first(row) && rest(row), "shown row {row} drawn: {reads:?}");
    }
    let below = *shown.last().unwrap() + 1;
    assert!(first(below), "row {below} measured");
    assert!(
        !rest(below),
        "row {below} measured from its first bytes alone"
    );
    assert!(
        !reads
            .iter()
            .any(|read| read.contains(&format!("/n{:03}/", placed + 50))),
        "nothing far below is read"
    );
    gallery.read_with(cx, |gallery, _| {
        let tile = |row: usize| &gallery.tiles[&(format!("n{row:03}"), "attachment".to_owned())];
        assert!(matches!(
            tile(shown[0]).look,
            Loadable::Loaded(Look::Image { .. })
        ));
        assert_eq!(
            tile(below).head.as_ref().map(Vec::len),
            Some(FIRST_READ as usize)
        );
        assert!(gallery.tile_tasks.is_empty(), "every read ended");
    });

    // Asked for and then no longer wanted before they were sent: nothing of theirs is read.
    blobs.reads.lock().unwrap().clear();
    gallery.update(cx, |gallery, cx| {
        gallery.release_all(cx);
        gallery.span = 0.0..400.;
        gallery.read_tiles(cx);
        assert_eq!(gallery.tile_tasks.len(), READS_AT_ONCE, "four at once");
        let bottom = gallery.masonry.height();
        gallery.span = (bottom - 100.)..bottom;
        gallery.read_tiles(cx);
        let wanted: HashSet<TileKey> = gallery.wanted().into_iter().map(|(key, _)| key).collect();
        assert!(gallery.tile_tasks.keys().all(|key| wanted.contains(key)));
    });
    cx.run_until_parked();
    let later = blobs.reads.lock().unwrap().clone();
    gallery.read_with(cx, |gallery, _| {
        let near: HashSet<usize> = gallery.masonry.meeting(0.0..400.).into_iter().collect();
        let far: HashSet<usize> = gallery
            .masonry
            .meeting((gallery.masonry.height() - 100.)..gallery.masonry.height())
            .into_iter()
            .collect();
        for read in &later {
            let id = read.split('/').nth(1).unwrap();
            let row: usize = id.trim_start_matches('n').parse().unwrap();
            assert!(
                !near.contains(&row) || far.contains(&row),
                "a read for a tile scrolled away was sent: {read}"
            );
        }
    });
}

/// Pages are read as the rows wanted a screen below the view near the end of those read, at
/// the first page's commit, and no further: a set of 5,000 is not read whole until it is
/// scrolled through.
#[gpui::test]
async fn pages_are_read_as_the_columns_near_the_end(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(5_000), None, cx);
    let (gallery, cx) = open(session, opening("Note"), cx);
    let opened = gallery.read_with(cx, |gallery, _| {
        assert!(
            gallery.rows.len() >= 200,
            "a first page is two of the graph's pages"
        );
        assert!(gallery.rows.len() < 5_000, "{} read", gallery.rows.len());
        assert!(gallery.masonry.placed() < gallery.rows.len());
        assert!(!gallery.paging.exhausted);
        assert_eq!(gallery.paging.pinned.as_deref(), Some("c1"));
        assert_eq!(gallery.count, Loadable::Loaded(5_000));
        gallery.rows.len()
    });
    let mut read = vec![opened];
    for _ in 0..80 {
        gallery.update(cx, |gallery, cx| {
            let bottom = gallery.masonry.height();
            gallery.scroll.set_offset(point(px(0.), px(-bottom)));
            cx.notify();
        });
        settle(cx);
        read.push(gallery.read_with(cx, |gallery, _| gallery.rows.len()));
    }
    assert!(
        read.windows(2).any(|pair| pair[1] > pair[0]),
        "scrolled down, later pages were read: {read:?}"
    );
    gallery.read_with(cx, |gallery, _| {
        assert_eq!(
            gallery.rows.len(),
            5_000,
            "scrolled through, every row was read"
        );
        assert!(gallery.paging.exhausted);
        assert_eq!(gallery.count_text(), "5,000 rows");
        let ids: HashSet<&str> = gallery.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids.len(), 5_000, "no row twice");
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
        assert_eq!(gallery.masonry.placed(), 3);
        assert_eq!(gallery.count_text(), "3 rows");
    });
}

/// Past the cap, the previews and first bytes held farthest from the span shown are let go;
/// those shown stay, even past the cap; every shape stays.
#[gpui::test]
async fn previews_past_the_cap_are_let_go_farthest_first(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(2_000), None, cx);
    let (gallery, cx) = open(session, opening("Note"), cx);
    gallery.update(cx, |gallery, cx| {
        assert!(gallery.masonry.placed() + 1 < gallery.rows.len());
        let shown = gallery.masonry.meeting(gallery.span.clone());
        let near = shown[0];
        let far = gallery.masonry.placed() - 1;
        let unplaced = gallery.masonry.placed() + 1;
        let key = |row: usize| (format!("n{row:03}"), "attachment".to_owned());
        let each = PREVIEWS_HELD / 3;
        for row in [near, far] {
            let image = crate::blob::ShownImage::from_rgba(1, 1, vec![0; 4]).unwrap();
            let tile = gallery.tiles.entry(key(row)).or_default();
            tile.look = Loadable::Loaded(Look::Image { image, bytes: each });
        }
        gallery.tiles.entry(key(unplaced)).or_default().head = Some(vec![0; each]);
        gallery.held = 3 * each + each / 2;
        gallery.let_go_past_cap(cx);
        let tile = |gallery: &TypeGallery, row: usize| gallery.tiles[&key(row)].held();
        assert!(tile(gallery, unplaced) == 0, "an unplaced row is farthest");
        assert!(tile(gallery, near) > 0 && tile(gallery, far) > 0);
        gallery.held = 2 * PREVIEWS_HELD;
        gallery.let_go_past_cap(cx);
        assert!(tile(gallery, near) > 0, "a tile shown keeps its preview");
        assert_eq!(tile(gallery, far), 0);
        assert!(gallery.tiles[&key(far)].shape.is_some(), "its shape stays");
    });
}

/// On a branch, nothing is placed until the branch's diff is in, so the rows it removed come
/// first without moving a placed tile; tiles are marked as the branch changed their rows
/// (spec 033), the removed row's file read where it last was.
#[gpui::test]
async fn a_branch_marks_its_tiles_and_places_removed_rows_first(cx: &mut TestAppContext) {
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
        let first = gallery.masonry.place_of(0).unwrap();
        assert_eq!(
            (first.column, first.top),
            (0, 0.),
            "the removed row is placed first"
        );
        assert_eq!(gallery.masonry.placed(), 4);
        assert_eq!(gallery.change_of("n000"), Some(NetChange::Added));
        assert_eq!(gallery.change_of("n001"), Some(NetChange::Changed));
        assert!(gallery.file_changed("n001"));
        assert!(!gallery.file_changed("n000"));
        assert!(gallery.is_removed("n900"));
        assert_eq!(gallery.diff.as_ref().unwrap().counts(), (1, 1, 1));
        assert_eq!(
            gallery.selected_row_id(),
            Some("n000"),
            "the cursor stays on its tile"
        );
        assert_eq!(gallery.diff_base.as_deref(), Some("c0"));
    });
    assert!(
        blobs
            .reads
            .lock()
            .unwrap()
            .iter()
            .any(|read| read.starts_with("HEAD Note/n900/attachment")),
    );
    gallery.update(cx, |gallery, cx| gallery.pick(0, false, cx));
    gallery.read_with(cx, |gallery, _| assert!(gallery.picks.is_empty()));
}

/// The grid is a cached view of its own: a key that moves the cursor draws it once, and
/// previews landing together draw it once, not once a tile.
#[gpui::test]
async fn a_key_draws_the_grid_once_and_previews_land_together(cx: &mut TestAppContext) {
    init(cx);
    let (session, blobs) = session_with_files(NOTES, notes_answering(40), None, cx);
    for i in 0..40 {
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
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(grid_renders(&gallery, cx), before + 1, "one draw a key");
    cx.simulate_keystrokes("up left");
    cx.run_until_parked();
    assert_eq!(grid_renders(&gallery, cx), before + 3, "one draw a key");

    // Every preview read again: the tiles shown land, and the grid is drawn once for them.
    gallery.update(cx, |gallery, cx| {
        for tile in gallery.tiles.values_mut() {
            tile.look = Loadable::NotLoaded;
        }
        gallery.read_tiles(cx);
    });
    let before = grid_renders(&gallery, cx);
    cx.run_until_parked();
    let landed = gallery.read_with(cx, |gallery, _| {
        gallery.masonry.meeting(gallery.span.clone()).len()
    });
    cx.executor().advance_clock(LANDING_DELAY);
    cx.run_until_parked();
    let drew = grid_renders(&gallery, cx) - before;
    assert!(landed > 4, "{landed} tiles shown");
    assert_eq!(drew, 1, "{drew} draws for {landed} tiles landing together");
}

/// A set whose rows all lack the sort's value: the first page, of rows with one, is empty, and
/// the gallery reads on to the rows without one rather than saying it has none. Review of
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
    let sorted = ShowSetInGallery {
        sort: Some(json!({ "property": "written", "descending": false })),
        ..opening("Note")
    };
    let (gallery, cx) = open(session, sorted, cx);
    gallery.read_with(cx, |gallery, _| {
        assert_eq!(gallery.rows.len(), 10, "the rows without a date were read");
        assert_eq!(gallery.cursor, Some(0));
        assert_eq!(gallery.masonry.placed(), 10);
    });
}

/// A listed set whose first batch is gone at the commit reads the next batch.
#[gpui::test]
async fn a_listed_batch_of_rows_gone_reads_the_next(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(20), None, cx);
    let mut ids: Vec<String> = (900..1100).map(|i| format!("n{i:03}")).collect();
    ids.extend(["n003".to_owned(), "n001".to_owned()]);
    let listed = ShowSetInGallery {
        ids: Some(ids),
        ..opening("Note")
    };
    let (gallery, cx) = open(session, listed, cx);
    gallery.read_with(cx, |gallery, _| {
        let ids: Vec<&str> = gallery.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["n003", "n001"]);
        assert_eq!(
            gallery.count_text(),
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
    let listed = ShowSetInGallery {
        ids: Some((0..300).map(|i| format!("n{i:03}")).collect()),
        ..opening("Note")
    };
    let (gallery, cx) = open(session, listed, cx);
    // Scrolled to the columns' end, the second batch is asked for, and refused.
    for _ in 0..4 {
        gallery.update(cx, |gallery, cx| {
            let bottom = gallery.masonry.height();
            gallery.scroll.set_offset(point(px(0.), px(-bottom)));
            cx.notify();
        });
        settle(cx);
    }
    gallery.read_with(cx, |gallery, _| {
        assert!(gallery.more_error.is_some(), "the second batch failed");
        assert_eq!(gallery.rows.len(), 200);
    });
    gallery.update(cx, |gallery, cx| gallery.retry_page(cx));
    settle(cx);
    let asked = asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 3, "{asked:?}");
    assert_eq!(asked[2], asked[1], "Retry asked for the batch that failed");
    gallery.read_with(cx, |gallery, _| {
        assert_eq!(gallery.rows.len(), 300);
        let ids: Vec<&str> = gallery.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids[200], "n200", "in their order");
    });
}

/// A gallery held at a commit made before a property was added reads its rows without it, as
/// the table does, listed or not. Before, the refusal was the gallery's error.
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
    let held = ShowSetInGallery {
        at_commit: Some("c0".into()),
        ..opening("Note")
    };
    let (gallery, cx) = open(session.clone(), held.clone(), cx);
    gallery.read_with(cx, |gallery, _| {
        assert!(
            matches!(gallery.first_page, Loadable::Loaded(())),
            "{:?}",
            gallery.first_page
        );
        assert_eq!(gallery.rows.len(), 3);
        assert_eq!(gallery.rows[0].value("title"), &json!("Note 0"));
        assert!(gallery.absent.contains_key("category"));
    });
    let listed = ShowSetInGallery {
        ids: Some(vec!["n000".into(), "n001".into()]),
        ..held
    };
    let (gallery, cx) = open(session, listed, cx);
    gallery.read_with(cx, |gallery, _| {
        assert_eq!(gallery.rows.len(), 2, "{:?}", gallery.first_page);
    });
}

/// Opened at a row past the first page (the table's cursor), the gallery reads on to it, puts
/// the cursor there once it is read, and scrolls to it once it is placed. Before, the row was
/// looked for in the first page alone, and the cursor fell to the first tile.
#[gpui::test]
async fn the_cursor_opens_on_its_row_past_the_first_page(cx: &mut TestAppContext) {
    init(cx);
    let (session, _) = session_with_files(NOTES, notes_answering(500), None, cx);
    let at = ShowSetInGallery {
        select: Some("n350".into()),
        ..opening("Note")
    };
    let (gallery, cx) = open(session, at, cx);
    gallery.read_with(cx, |gallery, _| {
        let cursor = gallery.cursor.expect("a cursor");
        assert_eq!(gallery.rows[cursor].id, "n350");
        assert!(cursor < gallery.masonry.placed(), "its tile is placed");
        let place = gallery.masonry.place_of(cursor).unwrap();
        let top = -f32::from(gallery.scroll.offset().y);
        assert!(
            top > 0. && place.top >= top - 1.,
            "scrolled to it: {top} {place:?}"
        );
    });
}

/// On a branch, a gallery opened at a row the branch removed puts its cursor there once the
/// diff puts the row first.
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
    let at = ShowSetInGallery {
        select: Some("n900".into()),
        ..opening("Note")
    };
    let (gallery, cx) = open(session, at, cx);
    gallery.read_with(cx, |gallery, _| {
        assert_eq!(gallery.cursor, Some(0));
        assert_eq!(gallery.rows[0].id, "n900");
    });
}

/// A window resized so the columns flow again reads the tiles that now meet the same span.
/// Before, the span read last was the same, and the tiles brought into view stayed unread.
#[gpui::test]
async fn a_resize_reads_the_tiles_it_brings_into_view(cx: &mut TestAppContext) {
    init(cx);
    let (session, blobs) = session_with_files(NOTES, notes_answering(120), None, cx);
    let big = png(300, 300, true);
    for i in 0..120 {
        blobs.set(&format!("Note/n{i:03}/attachment"), &big);
    }
    let (gallery, cx) = open(session, opening("Note"), cx);
    let columns = gallery.read_with(cx, |g, _| g.masonry.columns());
    let size = cx.update(|window, _| window.viewport_size());
    cx.simulate_resize(gpui::size(size.width * 2., size.height));
    settle(cx);
    gallery.read_with(cx, |gallery, _| {
        assert!(gallery.masonry.columns() > columns, "wider, more columns");
        let unread: Vec<usize> = gallery
            .masonry
            .meeting(gallery.span.clone())
            .into_iter()
            .filter(|&row| {
                let key = (gallery.rows[row].id.clone(), "attachment".to_owned());
                !matches!(
                    gallery.tiles.get(&key).map(|t| &t.look),
                    Some(Loadable::Loaded(_))
                )
            })
            .collect();
        assert!(
            unread.is_empty(),
            "tiles in view unread after the resize: {unread:?}"
        );
    });
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

/// On a branch, a gallery of listed rows reads the branch's diff too: its tiles are marked,
/// and a listed row the branch removed is shown first rather than lost.
#[gpui::test]
async fn a_listed_gallery_on_a_branch_marks_its_rows(cx: &mut TestAppContext) {
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
    let listed = ShowSetInGallery {
        ids: Some(vec!["n001".into(), "n900".into(), "n002".into()]),
        ..opening("Note")
    };
    let (gallery, cx) = open(session, listed, cx);
    gallery.read_with(cx, |gallery, _| {
        let ids: Vec<&str> = gallery.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["n900", "n001", "n002"]);
        assert_eq!(gallery.change_of("n001"), Some(NetChange::Added));
        assert_eq!(gallery.count_text(), "3 rows");
    });
}
