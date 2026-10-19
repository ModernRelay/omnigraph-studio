//! Motion for things Studio paints itself (the canvas's nodes and camera): easing curves,
//! values that blend, and a [`Tween`] that glides a value to its target. After the design of
//! `gpui-animation` (MIT OR Apache-2.0): easing as a curve over linear progress, interpolation as
//! a trait, and a glide a new one can replace mid-flight, starting from wherever the value is.
//! Unlike that crate there is no registry or tick thread: a view asks for the next
//! frame while one of its tweens runs ([`keep_animating`]), so nothing runs while nothing moves.
//!
//! Motion is for changes the person should follow (a node grows out of the one it came from; the
//! view glides to what it centres on), never for frequent interactions, and it stops when the
//! system asks for reduced motion.

use std::time::{Duration, Instant};

use gpui::{App, Window};

/// How long a change of place takes: long enough to follow, short enough not to wait for.
pub const GLIDE: Duration = Duration::from_millis(220);

/// An easing curve over linear progress `0..=1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ease {
    Linear,
    /// Fast, then settling: for things arriving.
    OutCubic,
    /// Slow at both ends: for the view moving as a whole.
    InOutCubic,
}

impl Ease {
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0., 1.);
        match self {
            Ease::Linear => t,
            Ease::OutCubic => 1. - (1. - t).powi(3),
            Ease::InOutCubic => {
                if t < 0.5 {
                    4. * t * t * t
                } else {
                    1. - (-2. * t + 2.).powi(3) / 2.
                }
            }
        }
    }
}

/// A value that can blend towards another.
pub trait Lerp: Clone {
    fn lerp(&self, to: &Self, t: f32) -> Self;
}

impl Lerp for f32 {
    fn lerp(&self, to: &Self, t: f32) -> Self {
        self + (to - self) * t
    }
}

/// A value gliding to its target. A new target mid-glide is a new glide from the value then
/// ([`Tween::value`]), so a glide can always be interrupted without a jump.
#[derive(Debug, Clone)]
pub struct Tween<T: Lerp> {
    from: T,
    to: T,
    started: Instant,
    duration: Duration,
    ease: Ease,
}

impl<T: Lerp> Tween<T> {
    /// A value gliding from `from` to `to`, starting `now`.
    pub fn glide(from: T, to: T, duration: Duration, ease: Ease, now: Instant) -> Self {
        Self {
            from,
            to,
            started: now,
            duration,
            ease,
        }
    }

    /// The value at `now`.
    pub fn value(&self, now: Instant) -> T {
        match self.progress(now) {
            Some(t) => self.from.lerp(&self.to, self.ease.apply(t)),
            None => self.to.clone(),
        }
    }

    /// Whether the value is still on its way at `now`.
    pub fn is_running(&self, now: Instant) -> bool {
        self.progress(now).is_some()
    }

    fn progress(&self, now: Instant) -> Option<f32> {
        let elapsed = now.saturating_duration_since(self.started);
        (elapsed < self.duration).then(|| elapsed.as_secs_f32() / self.duration.as_secs_f32())
    }
}

/// Whether Studio should move things at all: not when the system asks for reduced motion.
pub fn enabled(cx: &App) -> bool {
    !cx.reduce_motion()
}

/// Asks for the next frame while something is still moving; call it where the moving values
/// are painted.
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
        for ease in [Ease::Linear, Ease::OutCubic, Ease::InOutCubic] {
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
    fn a_tween_glides_from_where_it_starts_to_where_it_ends() {
        let start = Instant::now();
        let x = Tween::glide(0f32, 100., Duration::from_millis(100), Ease::Linear, start);
        assert_eq!(x.value(start), 0.);
        let half = start + Duration::from_millis(50);
        assert!((x.value(half) - 50.).abs() < 1e-3);
        assert!(x.is_running(half));
        let end = start + Duration::from_millis(100);
        assert_eq!(x.value(end), 100.);
        assert!(!x.is_running(end));
    }
}
