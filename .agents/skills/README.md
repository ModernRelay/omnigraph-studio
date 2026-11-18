# Agent skills for working on Studio

Skills that coding agents load when they work in this repository. Codex reads `.agents/skills/`;
`.claude/skills` is a symlink to it, so Claude Code reads the same files.

## make-interfaces-feel-better

`make-interfaces-feel-better/` is a copy of the skill of that name, unmodified:
`skills/make-interfaces-feel-better/` and `LICENSE` from
[jakubkrehel/make-interfaces-feel-better](https://github.com/jakubkrehel/make-interfaces-feel-better)
at commit `1a2aa9695487dc6d2a59af28e34db9b1490ab181` (2026-11-16). It is MIT-licensed;
`make-interfaces-feel-better/LICENSE` is its license.

Use it when building or reviewing a view: radius, alignment, elevation, motion, icons, numbers
and hit areas. Its examples are CSS, Tailwind and React. Studio is GPUI, so:

- Express each fix in GPUI and Studio's design tokens, never in a second styling system.
- Where Studio already has a rule, Studio's rule wins: nested corners through
  `ds::radius::around`/`within` and Zed's buttons at `radius::BUTTON` (spec 002, Visual language),
  colours and sizes as tokens only (`script/check-design-tokens.py`), and what a keystroke may
  draw (`docs/drawing.md`).
- Advice that only applies to a browser (`will-change`, `text-wrap`, `-webkit-font-smoothing`,
  the browser's Animations panel) has no GPUI counterpart; skip it.

To move to a newer upstream version, replace the folder with that commit's
`skills/make-interfaces-feel-better/` and its `LICENSE`, and update the commit and date above.
