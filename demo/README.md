# The README's demo video

`script/demo-video` makes it, end to end, against the development graph (`script/dev-server`):

1. The snapshot tool (`crates/studio/examples/snapshot.rs`) runs Studio offscreen and records
   the story with its `record:` step: a 2x PNG every 1/60 s, paced to real time, so Studio's
   own animations play out. Frames that repeat are saved once. It writes them, with
   `timeline.json`, to `public/film/`.
2. Remotion draws the film from them (`src/`): the window with rounded corners and a layered
   shadow on our own backdrop, a pointer that glides to each click and presses, keys as they
   are pressed, captions, and eased zooms where the story's `mark:zoom` steps ask.
3. Remotion renders `out/demo.mp4` (H.264) and the poster; FFmpeg (Remotion's own) makes the
   GIF from the MP4.

`src/schedule.ts` turns the timeline into what each frame of the film shows. The pointer's
glides are time added to the film: the recorded frame before a click holds while the pointer
moves there.

`npm run studio` opens Remotion's Studio on the last recording, to tune the motion without
recording again; `script/demo-video --render` renders from it.

## Versions

Remotion and its packages are pinned exactly (`4.0.543`, the current release when this was
written): Remotion changes Chrome's flags and its own defaults between patch releases.
`package-lock.json` pins the rest.

## Licence

Remotion is not open source. Its licence (<https://www.remotion.dev/docs/license>) is free for
an individual, and for a company or team of up to three people in all, contractors included;
a company of four or more needs a Company License. Running `npx remotion render` from a script
counts as an automation in its terms, which for a licensed company is the "Remotion for
Automators" option ($100 a month at least); "Remotion for Creators" ($25 a seat a month) covers
making videos by hand. A contributor who runs this as an individual needs nothing.

The code here is ours and GPL-3.0-or-later like the rest of the repository. Remotion is a
development dependency of this folder only: Studio does not link, bundle or ship it, and
nothing here draws Remotion's own assets (the pointer and the backdrop are drawn in `src/`).
