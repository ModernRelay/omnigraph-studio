# The demo video

The README's video is made by a script, so anyone can make it again after Studio changes. Studio
is recorded offscreen against the development graph, and Remotion frames the recording as a
window on a desktop, with a pointer, zooms and captions. How it works: `demo/README.md`.

## What to install

- What building Studio needs (`AGENTS.md`, "Build and test"), and Omnigraph's `omnigraph` and
  `omnigraph-server` for `script/dev-server`.
- Node 20 or later, with npm. `script/demo-video` installs the Remotion project's packages
  (`demo/package-lock.json`); Remotion downloads its own Chrome and brings its own FFmpeg.

Remotion is not open source: it is free for individuals and for companies of up to three
people. Read `demo/README.md`, "Licence", before a company runs it.

## Make the video

```bash
script/dev-server          # in another shell, and leave it running
```

```bash
script/demo-video
```

It records the story into `demo/public/film/`, then writes:

| File | What | In git |
|---|---|---|
| `demo/out/demo.mp4` | The video, H.264, 1920x1200 at 60 fps | No: it is uploaded (below) |
| `docs/images/demo-poster.png` | The poster | Yes |
| `docs/images/demo.gif` | The README's fallback, under 5 MB | Yes |

`script/demo-video --render` renders again from the last recording without recording: for a
change to the framing alone. `cd demo && npm run studio` opens Remotion's Studio on the
recording, to try a change frame by frame.

The first run makes a branch, `q4-hires`, on the development graph: two people added, three
titles changed, one person removed. Later runs use it as it is; `script/dev-server --reset`
starts the graph again without it. Nothing is ever merged.

## Change the story

The story is `demo/story.steps`, one snapshot step a line: the steps of
`crates/studio/examples/snapshot.rs` (actions, `click:`, `hover:`, `key:`, `type:`, `wait:`)
and the film's own `mark:` steps (zoom, caption, poster), all described at the top of the file.

To add a scene:

1. Find its points. A still is the quickest way: run the snapshot tool with the steps that lead
   there and a `shot:<path>` step, and read the coordinates off the PNG at half its size.

   ```bash
   OMNIGRAPH_STUDIO_SERVER=http://127.0.0.1:8787 OMNIGRAPH_STUDIO_TOKEN=dev-token \
     OMNIGRAPH_STUDIO_GRAPH=knowledge \
     target/release-fast/examples/snapshot /tmp/scene.png \
     'studio_graph::OpenType={"type_name":"Person"}' 'click:495,417'
   ```

2. Add the steps under a `mark:caption=` line in `demo/story.steps`, with `wait:` where the
   viewer needs time to read, and a `mark:zoom=` and `mark:zoom=1` around what is small.
3. Run `script/demo-video` and watch `demo/out/demo.mp4`.

The look (backdrop, window, shadow, pointer, timing) is in `demo/src/`: `Backdrop.tsx`,
`Demo.tsx`, and `schedule.ts` for the pointer's glide and the holds.

## Update the README video

GitHub plays a video in a README only from a `user-attachments` URL.

1. Drag `demo/out/demo.mp4` into a comment box on any issue or pull request of this repository.
   Wait for the upload; there is no need to post the comment.
2. Copy the `https://github.com/user-attachments/assets/...` line the box gets.
3. In `README.md`, put that URL on a line of its own where the video goes; GitHub draws it as a
   player. Keep the GIF under it as the fallback for places that do not play videos.
4. Commit the new `docs/images/demo.gif` and `docs/images/demo-poster.png` with it.
