//! Motion for things Studio paints itself (the canvas's nodes and camera): easing curves, values
//! that glide, and a [`Glide`] that takes a value to its target and can be sent elsewhere
//! mid-flight without a jump or a restart. Easing as a curve over linear progress, after the
//! design of `gpui-animation` (MIT OR Apache-2.0). There is no registry or tick thread: a view
//! asks for the next frame while one of its glides runs ([`keep_animating`]), so nothing runs
//! while nothing moves, and time is GPUI's clock ([`now`]), which tests move.
//!
//! Motion is for changes the person should follow (a node grows out of the one it came from; the
//! view glides to what it centres on), never for frequent interactions, and it stops when the
//! system asks for reduced motion.

use std::time::{Duration, Instant};

use gpui::{App, Window};

/// How long a change of place takes on the canvas (a node to its new place, the view to what it
/// shows): long enough to follow, short enough not to wait for.
pub const GLIDE: Duration = Duration::from_millis(220);

/// The curve of every change of place: fast, then settling, as constitution VII asks of rare
/// motion ("a short ease-out"). One curve for nodes and the view alike, so a node and the view
/// moving together arrive together.
pub const GLIDE_EASE: Ease = Ease::OutCubic;

/// How long something that comes up over the work (a dialog, a toast) takes to arrive: inside
/// the 120–180 ms of spec 002, so the eye catches where it lands and nothing waits on it.
pub const ENTER: Duration = Duration::from_millis(150);

/// How long a toast takes to go: faster than it came, since the person is done with it.
pub const LEAVE: Duration = Duration::from_millis(100);

/// An entrance or exit over `duration`, eased as things arriving. For an element that is
/// drawn whole each frame: a cached view inside one whose place does not change is drawn
/// again only when it notifies, so it would keep the opacity of its first frame.
pub fn arrival(duration: Duration) -> gpui::Animation {
    gpui::Animation::new(duration).with_easing(|t| Ease::OutCubic.apply(t))
}

/// How far apart a live dot's beats are ([`crate::ds::live_dot`]): each sends one ring out
/// from the dot, and between rings nothing is drawn.
pub const PULSE: Duration = Duration::from_millis(3000);

/// How long a beat's ring takes to go out from its dot and fade.
pub const PULSE_RING: Duration = Duration::from_millis(900);

/// How often ambient motion (a beat's ring, a spinner) redraws at most. Every frame builds the
/// whole window's scene again, whatever changed: a cached view skips its render and layout but
/// still puts each of its quads and lines back in the scene's draw order (`Scene::replay`), so
/// a frame of a full table costs about as much as the table. 24 steps a second read as smooth
/// for a ring a few pixels wide or a small icon turning once in two seconds, where every frame
/// would be 60 to 120.
pub const AMBIENT_FPS: f32 = 24.;

/// A ring's place: how far out it is (`0..=1`, eased as things arriving) and how visible (`1`
/// to `0`), from its linear progress `t`.
pub fn pulse(t: f32) -> (f32, f32) {
    let t = t.clamp(0., 1.);
    (Ease::OutCubic.apply(t), 1. - t)
}

/// An easing curve over linear progress `0..=1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ease {
    Linear,
    /// Fast, then settling: for things arriving, and every change of place ([`GLIDE_EASE`]).
    OutCubic,
}

impl Ease {
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0., 1.);
        match self {
            Ease::Linear => t,
            Ease::OutCubic => 1. - (1. - t).powi(3),
        }
    }
}

/// A value that can glide: the change from one value to another ([`Glides::toward`]), and a
/// value moved by part of a change ([`Glides::moved`]).
pub trait Glides: Clone {
    type Delta: Copy;
    /// The change that takes `self` to `to`.
    fn toward(&self, to: &Self) -> Self::Delta;
    /// `self` moved by `t` of `by` (`t` of 1 is all of it).
    fn moved(&self, by: Self::Delta, t: f32) -> Self;
    /// Whether a change is too small to see, so gliding it would only ask for frames.
    fn unseen(delta: Self::Delta) -> bool;
}

impl Glides for f32 {
    type Delta = f32;
    fn toward(&self, to: &Self) -> f32 {
        to - self
    }
    fn moved(&self, by: f32, t: f32) -> Self {
        self + by * t
    }
    fn unseen(delta: f32) -> bool {
        delta.abs() < 0.01
    }
}

/// A value on its way to its target, interruptible without a jump or a restart.
///
/// The value is its target plus what is left of each change still under way: a glide from
/// `from` to `to` is `to`, offset by `from - to` times `1 - ease(progress)`. A new target
/// ([`Glide::retarget`]) adds one more offset, from the new target back to the old one, and
/// leaves the others running, as additive animations do (Core Animation's, the web's
/// `composite: add`): the value is where it was drawn the frame before, the motion under way
/// carries on at its own pace, and every offset ends its own [`GLIDE`] after it began, so a
/// stream of new targets (pages landing, a zoom key held) never holds a value short of rest
/// for longer than one glide after the last.
#[derive(Debug, Clone)]
pub struct Glide<T: Glides> {
    target: T,
    offsets: Vec<(T::Delta, Instant)>,
}

impl<T: Glides> Glide<T> {
    /// A value gliding from `from` to `to`, starting `now`, over [`GLIDE`] on [`GLIDE_EASE`].
    pub fn new(from: T, to: T, now: Instant) -> Self {
        let mut glide = Self {
            target: to.clone(),
            offsets: Vec::new(),
        };
        let back = to.toward(&from);
        if !T::unseen(back) {
            glide.offsets.push((back, now));
        }
        glide
    }

    /// Where the value is going.
    pub fn target(&self) -> &T {
        &self.target
    }

    /// The value at `now`.
    pub fn value(&self, now: Instant) -> T {
        self.offsets.iter().fold(
            self.target.clone(),
            |value, &(back, started)| match progress(started, now) {
                Some(t) => value.moved(back, 1. - GLIDE_EASE.apply(t)),
                None => value,
            },
        )
    }

    /// Sends the value to `to` from wherever it is at `now`, keeping the motion under way.
    pub fn retarget(&mut self, to: T, now: Instant) {
        let back = to.toward(&self.target);
        self.target = to;
        if !T::unseen(back) {
            self.offsets.push((back, now));
        }
        self.settle(now);
    }

    /// Moves the target, and every point on the way with it, without a glide of its own: for a
    /// change the person should not see as motion (the view kept steady as the canvas resizes).
    pub fn shift_to(&mut self, to: T) {
        self.target = to;
    }

    /// Whether the value is still on its way at `now`.
    pub fn is_running(&self, now: Instant) -> bool {
        self.offsets
            .iter()
            .any(|&(_, started)| progress(started, now).is_some())
    }

    /// Forgets the changes that are over at `now`; whether any is still under way.
    pub fn settle(&mut self, now: Instant) -> bool {
        self.offsets
            .retain(|&(_, started)| progress(started, now).is_some());
        !self.offsets.is_empty()
    }
}

/// How far through a [`GLIDE`] begun at `started` it is at `now`, `0..1`; none once it is over.
fn progress(started: Instant, now: Instant) -> Option<f32> {
    let elapsed = now.saturating_duration_since(started);
    (elapsed < GLIDE).then(|| elapsed.as_secs_f32() / GLIDE.as_secs_f32())
}

/// The time motion is drawn at: GPUI's clock, the system's in the app and the test clock in
/// tests and offscreen recordings, which move it with `advance_clock`. Read once a frame and
/// passed down, so every value a frame draws is drawn at the same moment.
pub fn now(cx: &App) -> Instant {
    cx.background_executor().now()
}

/// Whether Studio should move things at all: not when the system asks for reduced motion.
pub fn enabled(cx: &App) -> bool {
    !cx.reduce_motion()
}

/// Asks for the next frame while something is still moving; call it where the moving values
/// are painted. Nothing asks once everything is at rest, so an idle view is not drawn again.
pub fn keep_animating(running: bool, window: &Window) {
    if running {
        window.request_animation_frame();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_start_at_zero_end_at_one_and_never_leave_the_range() {
        for ease in [Ease::Linear, Ease::OutCubic] {
            assert_eq!(ease.apply(0.), 0.);
            assert!((ease.apply(1.) - 1.).abs() < 1e-6);
            for i in 0..=20 {
                let v = ease.apply(i as f32 / 20.);
                assert!((0. ..=1.).contains(&v));
            }
        }
        // Out-cubic is past halfway at a quarter of the time: arriving things slow down.
        assert!(Ease::OutCubic.apply(0.25) > 0.5);
    }

    #[test]
    fn a_ring_goes_out_as_it_fades() {
        // Starts on the dot, fully visible; reaches its full size as it fades out.
        assert_eq!(pulse(0.), (0., 1.));
        let (reach, seen) = pulse(1.);
        assert!((reach - 1.).abs() < 1e-6);
        assert_eq!(seen, 0.);
        // Out faster than it fades: most of the way out while still half visible.
        let (reach, seen) = pulse(0.5);
        assert!(reach > 0.8 && (seen - 0.5).abs() < 1e-6);
        // A beat's ring is over well before the next beat.
        assert!(PULSE_RING * 3 <= PULSE);
    }

    #[test]
    fn what_comes_up_arrives_within_spec_002s_window_and_goes_faster() {
        let ms = |d: Duration| d.as_millis();
        assert!((120..=180).contains(&ms(ENTER)));
        assert!(LEAVE < ENTER);
    }

    #[test]
    fn a_glide_goes_from_where_it_starts_to_where_it_ends_and_comes_to_rest() {
        let start = Instant::now();
        let mut x = Glide::new(0f32, 100., start);
        assert_eq!(x.value(start), 0.);
        let half = start + GLIDE / 2;
        // Ease-out: past halfway at half the time.
        assert!(x.value(half) > 50. && x.value(half) < 100.);
        assert!(x.is_running(half));
        let end = start + GLIDE;
        assert_eq!(x.value(end), 100.);
        assert!(!x.is_running(end));
        assert!(
            !x.settle(end),
            "over at its end: nothing asks for another frame"
        );
        // A glide to where it already is never runs.
        assert!(!Glide::new(5f32, 5., start).is_running(start));
    }

    #[test]
    fn a_new_target_continues_from_where_the_value_is() {
        let start = Instant::now();
        let mut x = Glide::new(0f32, 100., start);
        let mid = start + GLIDE / 3;
        let before = x.value(mid);
        x.retarget(-50., mid);
        // No jump: the value is where it was drawn the moment before.
        assert!((x.value(mid) - before).abs() < 1e-3);
        // No restart: a frame later it has moved on the way it was going, under way already,
        // rather than standing still or starting over from rest.
        let frame = Duration::from_millis(16);
        let step = x.value(mid + frame) - x.value(mid);
        assert!(
            step.abs() > 0.5,
            "moving the moment it is sent elsewhere: {step}"
        );
        // It ends one glide after the last new target, at that target.
        assert!(x.is_running(mid + GLIDE - frame));
        assert_eq!(x.value(mid + GLIDE), -50.);
        assert!(!x.settle(mid + GLIDE));
    }

    #[test]
    fn a_stream_of_new_targets_never_holds_the_value_back() {
        // Pages landing every 50 ms send a node on to the same place each time: the node keeps
        // to its first glide and is at rest when it ends, rather than easing again each time.
        let start = Instant::now();
        let mut x = Glide::new(0f32, 100., start);
        for i in 1..4 {
            x.retarget(100., start + Duration::from_millis(50 * i));
        }
        assert_eq!(x.value(start + GLIDE), 100.);
        assert!(!x.is_running(start + GLIDE));
    }

    #[test]
    fn a_shifted_target_carries_the_glide_with_it() {
        let start = Instant::now();
        let mut x = Glide::new(0f32, 100., start);
        let mid = start + GLIDE / 2;
        let before = x.value(mid);
        x.shift_to(110.);
        assert!((x.value(mid) - (before + 10.)).abs() < 1e-3);
        assert_eq!(x.value(start + GLIDE), 110.);
    }
}
