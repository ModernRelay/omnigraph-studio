# Media for fix/canvas-animations

Offscreen films (`crates/studio/examples/snapshot.rs`, `record:`, 60 fps, `release-fast`)
against Omnigraph v0.15.0's `omnigraph-server`: `script/dev-server` (story2.steps) and
`script/dense-server` (story-dense.steps). "Before" is main at 98c7cea with one change: its
canvas reads GPUI's clock instead of `Instant::now()` (the same clock in the app), so the
recorder can film it.

- `*.gif`: before (left) and after (right), at half speed, every frame kept.
- `film-*.mp4`: the whole films at full speed.
- `motion-*.txt`: per-frame motion of the canvas per step (mean pixel change; `.` almost still,
  `o` moving, `#` large change, blank still).

This branch exists for the pull request's media; delete it after review.
