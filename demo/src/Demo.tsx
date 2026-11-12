import React, {useMemo} from 'react';
import {
  AbsoluteFill,
  Easing,
  Img,
  interpolate,
  spring,
  staticFile,
  useCurrentFrame,
  useVideoConfig,
} from 'remotion';
import {Backdrop} from './Backdrop';
import {Camera, Plan, plan, Timeline} from './schedule';

export type DemoProps = {timeline: Timeline | null};

// The film is 1920x1200 (16:10, as the window is). The window is 1600 wide, a little above the
// middle so the caption has room under it.
const WINDOW_WIDTH = 1600;
const WINDOW_TOP = 64;
const CAPTION_Y = 1132;

type Layout = {k: number; left: number; top: number; width: number; height: number};

const layout = (timeline: Timeline, width: number): Layout => {
  const k = WINDOW_WIDTH / timeline.width;
  return {
    k,
    left: (width - WINDOW_WIDTH) / 2,
    top: WINDOW_TOP,
    width: WINDOW_WIDTH,
    height: timeline.height * k,
  };
};

/** A spring that settles without overshoot, as a camera or a window moves. */
const settle = (frame: number, fps: number, durationInFrames = 54) =>
  frame <= 0
    ? 0
    : spring({frame, fps, durationInFrames, config: {damping: 200}});

const mix = (a: number, b: number, t: number) => a + (b - a) * t;

/** Where the camera is at `frame`: each zoom eases from wherever the one before had got to. */
const cameraAt = (p: Plan, frame: number, fps: number, home: Camera): Camera => {
  let from = home;
  let to = home;
  let start = -Infinity;
  for (const zoom of p.zooms) {
    if (zoom.at > frame) break;
    const t = start === -Infinity ? 1 : settle(zoom.at - start, fps);
    from = {scale: mix(from.scale, to.scale, t), x: mix(from.x, to.x, t), y: mix(from.y, to.y, t)};
    to = zoom.to;
    start = zoom.at;
  }
  const t = start === -Infinity ? 1 : settle(frame - start, fps);
  return {scale: mix(from.scale, to.scale, t), x: mix(from.x, to.x, t), y: mix(from.y, to.y, t)};
};

const clamp = (value: number, lo: number, hi: number) => Math.min(Math.max(value, lo), hi);

/** The stage's transform for a camera: the window point it looks at goes to the middle of the
 * film, as near as it can without leaving the window, and never beyond the backdrop. */
const stageTransform = (camera: Camera, l: Layout, width: number, height: number) => {
  const s = camera.scale;
  const cx = width / 2;
  const cy = height / 2;
  let x = l.left + camera.x * l.k;
  let y = l.top + camera.y * l.k;
  if (l.left + cx / s <= l.left + l.width - cx / s) {
    x = clamp(x, l.left + cx / s, l.left + l.width - cx / s);
  }
  if (l.top + cy / s <= l.top + l.height - cy / s) {
    y = clamp(y, l.top + cy / s, l.top + l.height - cy / s);
  }
  x = clamp(x, cx / s, width - cx / s);
  y = clamp(y, cy / s, height - cy / s);
  return `translate(${cx - x * s}px, ${cy - y * s}px) scale(${s})`;
};

/** The red, yellow and green buttons, where Studio leaves room for them (12 pt across, 20 pt
 * apart, in the 32 pt top strip: studio_workspace::traffic_light_position). */
const TrafficLights: React.FC<{k: number}> = ({k}) => (
  <>
    {['#ff5f57', '#febc2e', '#28c840'].map((color, i) => (
      <div
        key={color}
        style={{
          position: 'absolute',
          left: (13 + 20 * i) * k,
          top: 10 * k,
          width: 12 * k,
          height: 12 * k,
          borderRadius: '50%',
          backgroundColor: color,
          boxShadow: `inset 0 0 0 ${0.5 * k}px rgba(0, 0, 0, 0.12)`,
        }}
      />
    ))}
  </>
);

const Window: React.FC<{l: Layout; file: string; enter: number}> = ({l, file, enter}) => {
  const radius = 10 * l.k;
  return (
    <div
      style={{
        position: 'absolute',
        left: l.left,
        top: l.top,
        width: l.width,
        height: l.height,
        borderRadius: radius,
        opacity: enter,
        transform: `translateY(${(1 - enter) * 28}px) scale(${mix(0.97, 1, enter)})`,
        // Layered as macOS draws a window: a hairline edge, a close soft shadow and a wide one.
        boxShadow: [
          '0 0 0 1px rgba(0, 0, 0, 0.10)',
          '0 1px 2px rgba(15, 20, 40, 0.10)',
          '0 6px 14px rgba(15, 20, 40, 0.10)',
          '0 18px 40px rgba(15, 20, 40, 0.14)',
          '0 40px 90px rgba(15, 20, 40, 0.22)',
        ].join(', '),
      }}
    >
      <div style={{position: 'absolute', inset: 0, borderRadius: radius, overflow: 'hidden'}}>
        <Img src={staticFile(`film/${file}`)} style={{width: '100%', height: '100%', display: 'block'}} />
        <TrafficLights k={l.k} />
        {/* The light catching the window's top edge. */}
        <div
          style={{
            position: 'absolute',
            inset: 0,
            borderRadius: radius,
            boxShadow: 'inset 0 1px 0 rgba(255, 255, 255, 0.6)',
            pointerEvents: 'none',
          }}
        />
      </div>
    </div>
  );
};

/** Our own arrow, its tip at (0, 0), 1 unit to a window point. */
const ARROW = 'M0 0 L0 17.2 L4.3 13.1 L7.2 19.8 L10.2 18.5 L7.4 12 L13.2 12 Z';

const Pointer: React.FC<{p: Plan; l: Layout; frame: number}> = ({p, l, frame}) => {
  // Where the pointer is: at rest after its last glide, or on its way along a gentle arc.
  let at = {x: 1000, y: 760};
  let shown = 0;
  for (const move of p.moves) {
    if (move.start > frame) break;
    const t = interpolate(frame, [move.start, move.end], [0, 1], {
      extrapolateLeft: 'clamp',
      extrapolateRight: 'clamp',
      easing: Easing.bezier(0.45, 0, 0.2, 1),
    });
    const dx = move.to.x - at.x;
    const dy = move.to.y - at.y;
    const bow = Math.min(36, Math.hypot(dx, dy) * 0.08) * Math.sin(Math.PI * t);
    const length = Math.hypot(dx, dy) || 1;
    at = {x: at.x + dx * t + (-dy / length) * bow, y: at.y + dy * t + (dx / length) * bow};
    shown = 1;
  }
  // It comes in with its first glide.
  const first = p.moves[0]?.start ?? 0;
  const appear = interpolate(frame, [first, first + 10], [0, 1], {
    extrapolateLeft: 'clamp',
    extrapolateRight: 'clamp',
  });

  // A press: down over 5 frames, back over 10, with a ring going out from the tip.
  let press = 0;
  let ring: {r: number; o: number} | null = null;
  for (const at of p.presses) {
    const d = frame - at;
    if (d >= -5 && d < 0) press = Math.max(press, (d + 5) / 5);
    if (d >= 0 && d < 10) press = Math.max(press, 1 - d / 10);
    if (d >= 0 && d < 24) {
      const t = d / 24;
      ring = {r: mix(4, 22, Easing.out(Easing.cubic)(t)), o: 0.35 * (1 - t)};
    }
  }
  const x = l.left + at.x * l.k;
  const y = l.top + at.y * l.k;
  const size = 1.15 * l.k * (1 - 0.16 * press);
  return (
    <div style={{position: 'absolute', left: x, top: y, opacity: shown * appear}}>
      {ring ? (
        <div
          style={{
            position: 'absolute',
            left: -ring.r * l.k,
            top: -ring.r * l.k,
            width: 2 * ring.r * l.k,
            height: 2 * ring.r * l.k,
            borderRadius: '50%',
            border: `${2 * l.k}px solid rgba(40, 60, 120, ${ring.o})`,
            backgroundColor: `rgba(80, 110, 220, ${ring.o * 0.35})`,
          }}
        />
      ) : null}
      <svg
        width={16 * size}
        height={22 * size}
        viewBox="-1.5 -1.5 16 22"
        style={{
          position: 'absolute',
          left: -1.5 * size,
          top: -1.5 * size,
          overflow: 'visible',
          filter: 'drop-shadow(0 2px 3px rgba(0, 0, 0, 0.30))',
        }}
      >
        <path d={ARROW} fill="#111" stroke="#fff" strokeWidth={1.4} strokeLinejoin="round" />
      </svg>
    </div>
  );
};

const Caption: React.FC<{p: Plan; frame: number}> = ({p, frame}) => {
  const current = [...p.captions].reverse().find((c) => c.at <= frame);
  if (!current || current.text === '') return null;
  const t = interpolate(frame - current.at, [0, 18], [0, 1], {
    extrapolateRight: 'clamp',
    easing: Easing.out(Easing.cubic),
  });
  return (
    <div
      style={{
        position: 'absolute',
        left: 0,
        right: 0,
        top: CAPTION_Y,
        display: 'flex',
        justifyContent: 'center',
        transform: `translateY(${(1 - t) * 10 - 22}px)`,
        opacity: t,
      }}
    >
      <div
        style={{
          fontFamily: '-apple-system, BlinkMacSystemFont, "Inter", "Segoe UI", system-ui, sans-serif',
          fontSize: 30,
          fontWeight: 500,
          letterSpacing: -0.2,
          color: '#1d2233',
          padding: '8px 22px',
          borderRadius: 999,
          backgroundColor: 'rgba(255, 255, 255, 0.72)',
          boxShadow: '0 0 0 1px rgba(20, 30, 60, 0.06), 0 6px 18px rgba(20, 30, 60, 0.10)',
        }}
      >
        {current.text}
      </div>
    </div>
  );
};

/** The keys just pressed, over the bottom of the window, for a second. */
const Keys: React.FC<{p: Plan; frame: number; fps: number; l: Layout}> = ({p, frame, fps, l}) => {
  const current = [...p.keys].reverse().find((k) => k.at <= frame && frame < k.at + fps * 1.1);
  if (!current) return null;
  const d = frame - current.at;
  const o = interpolate(d, [0, 6, fps * 0.9, fps * 1.1], [0, 1, 1, 0], {extrapolateRight: 'clamp'});
  return (
    <div
      style={{
        position: 'absolute',
        left: 0,
        right: 0,
        top: l.top + l.height - 120,
        display: 'flex',
        justifyContent: 'center',
        opacity: o,
        transform: `scale(${mix(0.94, 1, Math.min(1, d / 6))})`,
      }}
    >
      <div
        style={{
          display: 'flex',
          gap: 10,
          padding: '12px 18px',
          borderRadius: 16,
          backgroundColor: 'rgba(20, 24, 36, 0.78)',
          boxShadow: '0 10px 30px rgba(0, 0, 0, 0.25)',
        }}
      >
        {current.label.split(' ').filter(Boolean).map((key, i) => (
          <div
            key={i}
            style={{
              minWidth: 46,
              padding: '6px 12px',
              borderRadius: 9,
              textAlign: 'center',
              fontFamily: '-apple-system, BlinkMacSystemFont, "Inter", system-ui, sans-serif',
              fontSize: 30,
              fontWeight: 500,
              color: '#fff',
              backgroundColor: 'rgba(255, 255, 255, 0.14)',
              boxShadow: 'inset 0 -2px 0 rgba(0, 0, 0, 0.25)',
            }}
          >
            {key}
          </div>
        ))}
      </div>
    </div>
  );
};

export const Demo: React.FC<DemoProps> = ({timeline}) => {
  const frame = useCurrentFrame();
  const {fps, width, height} = useVideoConfig();
  const p = useMemo(() => (timeline ? plan(timeline) : null), [timeline]);
  if (!timeline || !p) {
    return <AbsoluteFill style={{backgroundColor: '#e8ebf4'}} />;
  }
  const l = layout(timeline, width);
  const home = {scale: 1, x: timeline.width / 2, y: timeline.height / 2};
  const camera = cameraAt(p, frame, fps, home);
  const slot = p.slots[Math.min(frame, p.slots.length - 1)];
  const enter = settle(frame, fps, 30);
  return (
    <AbsoluteFill style={{overflow: 'hidden'}}>
      <AbsoluteFill style={{transformOrigin: '0 0', transform: stageTransform(camera, l, width, height)}}>
        <Backdrop />
        <Window l={l} file={timeline.frames[slot]} enter={enter} />
        <Pointer p={p} l={l} frame={frame} />
      </AbsoluteFill>
      <Caption p={p} frame={frame} />
      <Keys p={p} frame={frame} fps={fps} l={l} />
    </AbsoluteFill>
  );
};
