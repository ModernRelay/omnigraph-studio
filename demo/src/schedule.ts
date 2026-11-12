// What happens in each frame of the film: which recorded frame shows, where the pointer goes
// and when it presses, where the camera looks, and which caption and keys show. All of it comes
// from the recording's timeline.json (crates/studio/examples/snapshot.rs, `record:`).

export type Step = {
  step: string;
  /** The recorded frame (1/fps slot) the step began at. */
  frame: number;
  kind?: 'click' | 'hover' | 'drag';
  /** The pointer's point in window coordinates. */
  x?: number;
  y?: number;
};

export type Timeline = {
  fps: number;
  /** The window's size in points; the frames are drawn at twice that. */
  width: number;
  height: number;
  /** Each slot's PNG, in film/. */
  frames: string[];
  steps: Step[];
};

type Point = {x: number; y: number};

export type Camera = {scale: number; x: number; y: number};

export type Plan = {
  durationInFrames: number;
  /** For each frame of the film, the recorded slot it shows. */
  slots: number[];
  /** The pointer glides to (x, y) between start and end. */
  moves: {start: number; end: number; to: Point}[];
  /** Frames at which the pointer presses. */
  presses: number[];
  /** The camera eases to a scale around a window point from `at`. */
  zooms: {at: number; to: Camera}[];
  captions: {at: number; text: string}[];
  keys: {at: number; label: string}[];
  poster: number;
};

/** How long the pointer takes to reach a point, in seconds. */
const GLIDE = 0.6;
/** The still frames before the first step and after the last, in seconds. */
const INTRO = 0.6;
const OUTRO = 1.2;

const KEY_NAMES: Record<string, string> = {
  cmd: '⌘',
  shift: '⇧',
  alt: '⌥',
  ctrl: '⌃',
  enter: '↵',
  escape: 'esc',
  tab: '⇥',
  up: '↑',
  down: '↓',
  left: '←',
  right: '→',
};

/** `cmd-shift-r` as the keycaps a person reads: ⌘ ⇧ R. */
export const keyLabel = (keys: string): string =>
  keys
    .split(' ')
    .map((stroke) =>
      stroke
        .split('-')
        .map((key) => KEY_NAMES[key] ?? key.toUpperCase())
        .join(' '),
    )
    .join('  ');

export const plan = (timeline: Timeline): Plan => {
  const fps = timeline.fps;
  const glide = Math.round(GLIDE * fps);
  const slots: number[] = [];
  const hold = (slot: number, frames: number) => {
    for (let i = 0; i < frames; i++) slots.push(slot);
  };
  const result: Plan = {
    durationInFrames: 0,
    slots,
    moves: [],
    presses: [],
    zooms: [],
    captions: [],
    keys: [],
    poster: 0,
  };

  hold(0, Math.round(INTRO * fps));
  let played = 0;
  for (const step of timeline.steps) {
    // Play what was recorded up to this step.
    for (; played < step.frame; played++) slots.push(played);
    if (step.kind && step.x !== undefined && step.y !== undefined) {
      // The pointer glides there over the frame before the step, then the step plays.
      result.moves.push({start: slots.length, end: slots.length + glide, to: {x: step.x, y: step.y}});
      hold(Math.max(step.frame - 1, 0), glide);
      if (step.kind !== 'hover') result.presses.push(slots.length);
    } else if (step.step.startsWith('mark:')) {
      const [name, value = ''] = step.step.slice('mark:'.length).split(/=(.*)/s);
      if (name === 'caption') result.captions.push({at: slots.length, text: value});
      if (name === 'poster') result.poster = slots.length;
      if (name === 'zoom') {
        const [scale, x, y] = value.split(',').map(Number);
        const to =
          scale === 1 || x === undefined || y === undefined
            ? {scale: 1, x: timeline.width / 2, y: timeline.height / 2}
            : {scale, x, y};
        result.zooms.push({at: slots.length, to});
      }
    } else if (step.step.startsWith('key:')) {
      result.keys.push({at: slots.length, label: keyLabel(step.step.slice('key:'.length))});
    }
  }
  for (; played < timeline.frames.length; played++) slots.push(played);
  hold(timeline.frames.length - 1, Math.round(OUTRO * fps));
  result.durationInFrames = slots.length;
  return result;
};
