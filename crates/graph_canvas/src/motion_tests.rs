//! The canvas's motion (spec 006, Motion): glides that are sent elsewhere mid-flight carry on
//! from where they are drawn, everything comes to rest one glide after its last change, and a
//! canvas at rest asks for no frame and is not drawn. Time is GPUI's clock, which these tests
//! move a frame at a time.

use std::time::Duration;

use gpui::{Modifiers, MouseButton, VisualTestContext};

use super::*;

/// A frame at 60 Hz.
const FRAME: Duration = Duration::from_millis(16);

/// A canvas walking from Ada, in a window, laid out and fitted, with the keyboard.
fn walk_in_window(cx: &mut gpui::TestAppContext) -> (Entity<GraphCanvas>, &mut VisualTestContext) {
    cx.update(studio_theme::init_test);
    let (session, _served) = graph_store::test_server::followed_session(
        "node Person {\n  slug: String @key\n  name: String\n}\n",
        |_| serde_json::json!([{ "id": "p1", "label": "Ada", "n": 0 }]),
        cx,
    );
    cx.run_until_parked();
    let (canvas, cx) = cx
        .add_window_view(|_, cx| GraphCanvas::new(session, NodeRef::new("Person", "p1"), None, cx));
    cx.update(|window, cx| window.focus(&canvas.focus_handle(cx), cx));
    until_rest(cx);
    (canvas, cx)
}

/// One frame as the app draws it while something moves: a frame's time passes, the frames
/// asked for are delivered, and the window draws what they marked. How many were asked for.
fn frame(cx: &mut VisualTestContext) -> usize {
    cx.executor().advance_clock(FRAME);
    let asked = cx.update(|window, cx| window.simulate_next_frame(cx));
    cx.run_until_parked();
    asked
}

/// Frames until none is asked for; how many it took.
fn until_rest(cx: &mut VisualTestContext) -> usize {
    cx.run_until_parked();
    let mut frames = 0;
    while frame(cx) > 0 {
        frames += 1;
        assert!(frames < 600, "the canvas never comes to rest");
    }
    frames
}

/// The most frames one glide takes: its length, and the frame that finds it over.
fn one_glide() -> usize {
    (motion::GLIDE.as_millis() / FRAME.as_millis()) as usize + 2
}

fn camera(canvas: &Entity<GraphCanvas>, cx: &mut VisualTestContext) -> Camera {
    canvas.read_with(cx, |canvas, cx| canvas.camera_now(motion::now(cx)))
}

fn drawn(canvas: &Entity<GraphCanvas>, index: usize, cx: &mut VisualTestContext) -> Vec2 {
    canvas.read_with(cx, |canvas, cx| {
        canvas.drawn_position(index, motion::now(cx))
    })
}

/// Lays `names` out at `positions`, as a layout landing would.
fn land(
    canvas: &Entity<GraphCanvas>,
    names: &[&str],
    positions: Vec<Vec2>,
    cx: &mut VisualTestContext,
) {
    let measure = |text: &str, _: f32, _: gpui::FontWeight| text.len() as f32 * 7.;
    let scene = Scene {
        nodes: names
            .iter()
            .map(|name| crate::model::SceneNode {
                id: SceneId::Node(NodeRef::new("T", *name)),
                kind: SceneKind::Node {
                    node: NodeRef::new("T", *name),
                    root: false,
                    expanded: false,
                },
                label: (*name).into(),
                color_key: (*name).into(),
                parent: None,
                path: None,
            })
            .collect(),
        edges: Vec::new(),
        capped: None,
        cap: 0,
    };
    let texts: Vec<paint::ChipText> = scene
        .nodes
        .iter()
        .map(|node| paint::chip_text(node, &measure))
        .collect();
    canvas.update(cx, |canvas, cx| {
        canvas.apply_layout(scene, texts, vec![30.; names.len()], positions, cx)
    });
}

/// `=` pressed again while the view still zooms from the first: the view goes on from where it
/// is drawn, at speed, and comes to rest at both steps in one glide from the second. Before, a
/// second press began a new ease-in-out from a standstill, so the zoom stopped dead and
/// crept off again at every press.
#[gpui::test]
async fn a_zoom_key_pressed_mid_glide_goes_on_from_where_the_view_is(
    cx: &mut gpui::TestAppContext,
) {
    let (canvas, cx) = walk_in_window(cx);
    let start = camera(&canvas, cx).zoom;
    cx.dispatch_action(ZoomIn);
    for _ in 0..4 {
        frame(cx);
    }
    let before = camera(&canvas, cx).zoom;
    frame(cx);
    let pressed = camera(&canvas, cx).zoom;
    let pace = (pressed / before).ln();
    assert!(pace > 0., "zooming in");
    cx.dispatch_action(ZoomIn);
    assert!(
        (camera(&canvas, cx).zoom - pressed).abs() < 1e-4,
        "no jump at the press"
    );
    frame(cx);
    let next = (camera(&canvas, cx).zoom / pressed).ln();
    assert!(
        next >= pace,
        "goes on at least as fast as it was going: {next} after {pace}"
    );
    let frames = until_rest(cx);
    assert!(
        frames <= one_glide(),
        "at rest one glide after the press: {frames}"
    );
    let end = canvas.read_with(cx, |canvas, _| {
        assert!(canvas.glide.is_none(), "the glide is forgotten at rest");
        canvas.zoom
    });
    assert!((end - start * ZOOM_STEP * ZOOM_STEP).abs() < 1e-4);
    assert!(
        (camera(&canvas, cx).zoom - end).abs() < 1e-6,
        "drawn where it rests"
    );
}

/// Fit pressed while a zoom key's glide runs, then the reverse: each sends the view on from
/// where it is drawn, without a jump.
#[gpui::test]
async fn a_fit_mid_glide_and_a_zoom_mid_fit_never_jump(cx: &mut gpui::TestAppContext) {
    let (canvas, cx) = walk_in_window(cx);
    for action in [
        Box::new(ZoomIn) as Box<dyn gpui::Action>,
        Box::new(FitToWindow),
        Box::new(ZoomOut),
        Box::new(FitToWindow),
    ] {
        frame(cx);
        frame(cx);
        let before = camera(&canvas, cx);
        cx.update(|window, cx| window.dispatch_action(action.boxed_clone(), cx));
        let after = camera(&canvas, cx);
        assert!((after.zoom - before.zoom).abs() < 1e-4, "{action:?} jumped");
        assert!(
            (after.center - before.center).length() < 1e-3,
            "{action:?} jumped"
        );
    }
    assert!(until_rest(cx) <= one_glide());
}

/// A node on its way to its place when another layout lands goes on from where it is drawn and
/// keeps moving; it is at rest one glide after the last layout, at the last place.
#[gpui::test]
async fn a_node_sent_on_mid_glide_continues_and_comes_to_rest(cx: &mut gpui::TestAppContext) {
    let (canvas, cx) = walk_in_window(cx);
    land(
        &canvas,
        &["A", "B"],
        vec![Vec2::default(), Vec2::new(0., 100.)],
        cx,
    );
    until_rest(cx);
    land(
        &canvas,
        &["A", "B"],
        vec![Vec2::default(), Vec2::new(300., 100.)],
        cx,
    );
    for _ in 0..4 {
        frame(cx);
    }
    let before = drawn(&canvas, 1, cx);
    assert!(before.x > 0. && before.x < 300., "on its way: {before:?}");
    frame(cx);
    let at = drawn(&canvas, 1, cx);
    let pace = at.x - before.x;
    // Pages landing: the same scene again with B sent further, and a new chip ahead of it.
    land(
        &canvas,
        &["New", "A", "B"],
        vec![Vec2::new(0., -100.), Vec2::default(), Vec2::new(400., 100.)],
        cx,
    );
    assert!((drawn(&canvas, 2, cx) - at).length() < 1e-3, "no jump");
    frame(cx);
    let step = drawn(&canvas, 2, cx).x - at.x;
    assert!(
        step >= pace,
        "keeps moving at least as fast: {step} after {pace}"
    );
    assert!(until_rest(cx) <= one_glide());
    assert_eq!(drawn(&canvas, 2, cx), Vec2::new(400., 100.));
    canvas.read_with(cx, |canvas, _| assert!(canvas.appear.is_empty()));
}

/// Pages that land while a node glides, leaving its place as it was, neither hold it back nor
/// start it over: it arrives when its own glide ends. Before, every layout began each moving
/// node's glide again from where it was drawn, so while pages landed (Expand all, a long
/// list read page by page) the walk crept and did not come to rest until they stopped.
#[gpui::test]
async fn pages_landing_mid_glide_do_not_hold_a_node_back(cx: &mut gpui::TestAppContext) {
    let (canvas, cx) = walk_in_window(cx);
    land(
        &canvas,
        &["A", "B"],
        vec![Vec2::default(), Vec2::new(0., 100.)],
        cx,
    );
    until_rest(cx);
    land(
        &canvas,
        &["A", "B"],
        vec![Vec2::default(), Vec2::new(300., 100.)],
        cx,
    );
    let mut frames = 0;
    for page in 0..4 {
        for _ in 0..3 {
            frame(cx);
            frames += 1;
        }
        let mut names = vec!["A", "B"];
        let more: Vec<String> = (0..=page).map(|n| format!("P{n}")).collect();
        names.extend(more.iter().map(String::as_str));
        let mut places = vec![Vec2::default(), Vec2::new(300., 100.)];
        places.extend((0..=page).map(|n| Vec2::new(-200., 60. * n as f32)));
        land(&canvas, &names, places, cx);
    }
    while drawn(&canvas, 1, cx) != Vec2::new(300., 100.) {
        frame(cx);
        frames += 1;
        assert!(
            frames <= one_glide(),
            "B held back by the pages: {frames} frames"
        );
    }
}

/// Once everything is at rest the canvas asks for no frame and is not drawn again, however long
/// the window stays open (docs/drawing.md: no redraw while idle).
#[gpui::test]
async fn a_canvas_at_rest_is_not_drawn_again(cx: &mut gpui::TestAppContext) {
    let (canvas, cx) = walk_in_window(cx);
    land(
        &canvas,
        &["A", "B"],
        vec![Vec2::default(), Vec2::new(0., 100.)],
        cx,
    );
    cx.dispatch_action(FitToWindow);
    let renders = canvas.read_with(cx, |canvas, _| canvas.renders.get());
    let frames = until_rest(cx);
    let moving = canvas.read_with(cx, |canvas, _| canvas.renders.get()) - renders;
    assert!(frames > 2, "it moved: {frames} frames");
    assert!(
        moving <= frames + 1,
        "one draw a frame while it moves: {moving}"
    );
    let renders = canvas.read_with(cx, |canvas, _| canvas.renders.get());
    for _ in 0..120 {
        assert_eq!(frame(cx), 0, "a frame asked for at rest");
    }
    cx.executor().advance_clock(Duration::from_secs(30));
    cx.run_until_parked();
    canvas.read_with(cx, |canvas, _| {
        assert_eq!(canvas.renders.get(), renders, "drawn while nothing moves");
        assert!(canvas.glide.is_none() && canvas.appear.is_empty());
    });
}

/// A chip grabbed while it glides is taken from where it is drawn: it stays under the pointer,
/// rather than jumping to where it was going.
#[gpui::test]
async fn a_chip_grabbed_mid_glide_stays_under_the_pointer(cx: &mut gpui::TestAppContext) {
    let (canvas, cx) = walk_in_window(cx);
    land(
        &canvas,
        &["A", "B"],
        vec![Vec2::default(), Vec2::new(0., 60.)],
        cx,
    );
    until_rest(cx);
    land(
        &canvas,
        &["A", "B"],
        vec![Vec2::default(), Vec2::new(240., 60.)],
        cx,
    );
    for _ in 0..3 {
        frame(cx);
    }
    let chip = canvas.read_with(cx, |canvas, _| canvas.chips.borrow()[1]);
    let grabbed = drawn(&canvas, 1, cx);
    assert!(grabbed.x < 240., "still on its way");
    let zoom = canvas.read_with(cx, |canvas, _| canvas.zoom);
    let at = chip.center();
    cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    let to = point(at.x + px(40.), at.y);
    cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    let placed = canvas.read_with(cx, |canvas, _| canvas.positions[1]);
    let expected = grabbed + Vec2::new(40. / zoom, 0.);
    assert!(
        (placed - expected).length() < 0.5,
        "under the pointer: {placed:?}, not {expected:?}"
    );
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
}

/// Reset layout glides the chips to their new places and the view to the fit; a walk's first
/// layout is drawn at once.
#[gpui::test]
async fn reset_layout_glides_to_the_new_layout(cx: &mut gpui::TestAppContext) {
    let (canvas, cx) = walk_in_window(cx);
    let ada = SceneId::Node(NodeRef::new("Person", "p1"));
    canvas.update(cx, |canvas, cx| {
        canvas
            .pinned
            .insert(pin_key(&ada).unwrap(), Vec2::new(400., 300.));
        canvas.rebuild(cx);
    });
    // Zoomed in, so the fit after the reset moves the view too.
    cx.dispatch_action(ZoomIn);
    until_rest(cx);
    let index = canvas.read_with(cx, |canvas, _| canvas.index_of(&ada).unwrap());
    assert_eq!(drawn(&canvas, index, cx), Vec2::new(400., 300.));
    cx.dispatch_action(ResetLayout);
    cx.run_until_parked();
    frame(cx);
    let on_way = drawn(&canvas, index, cx);
    assert!(
        on_way != Vec2::new(400., 300.) && on_way != Vec2::default(),
        "gliding home, not teleported: {on_way:?}"
    );
    assert!(
        canvas.read_with(cx, |canvas, _| canvas.glide.is_some()),
        "the view glides"
    );
    assert!(until_rest(cx) <= one_glide() + 1);
    assert_eq!(drawn(&canvas, index, cx), Vec2::default());
}

/// Under reduced motion nothing on the canvas moves: zoom keys, fits and layouts land in one
/// frame, and no frame is asked for after it.
#[gpui::test]
async fn reduced_motion_moves_nothing(cx: &mut gpui::TestAppContext) {
    let (canvas, cx) = walk_in_window(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    cx.dispatch_action(ZoomIn);
    land(
        &canvas,
        &["A", "B"],
        vec![Vec2::default(), Vec2::new(0., 100.)],
        cx,
    );
    land(
        &canvas,
        &["A", "B"],
        vec![Vec2::default(), Vec2::new(300., 100.)],
        cx,
    );
    canvas.read_with(cx, |canvas, _| {
        assert!(canvas.glide.is_none() && canvas.appear.is_empty());
    });
    assert_eq!(drawn(&canvas, 1, cx), Vec2::new(300., 100.));
    assert!(until_rest(cx) <= 1);
}

/// An arrow key that selects a chip past the canvas's edge brings it into view at once: keys
/// pressed many times a minute never animate (constitution VII), and the selection is never
/// left out of sight.
#[gpui::test]
async fn an_arrow_key_keeps_the_chip_it_reaches_in_view(cx: &mut gpui::TestAppContext) {
    let (canvas, cx) = walk_in_window(cx);
    land(
        &canvas,
        &["A", "B"],
        vec![Vec2::default(), Vec2::new(5000., 0.)],
        cx,
    );
    until_rest(cx);
    canvas.update(cx, |canvas, cx| canvas.select(Some(0), cx));
    cx.dispatch_action(MoveRight);
    canvas.read_with(cx, |canvas, cx| {
        assert_eq!(canvas.selected_index(), Some(1));
        assert!(canvas.glide.is_none(), "at once");
        let viewport = canvas.bounds.get().unwrap();
        let at = to_screen(canvas.positions[1], viewport, canvas.center, canvas.zoom);
        assert!(viewport.contains(&at), "B is in view");
        assert_eq!(canvas.camera_now(motion::now(cx)).center, canvas.center);
    });
}
