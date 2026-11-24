# Agent skills for working on Studio

Skills that coding agents load when they work in this repository. Codex reads `.agents/skills/`;
`.claude/skills` is a symlink to it, so Claude Code reads the same files.

## jakubkrehel/skills

Every folder here is a copy of a skill of that name, unmodified: `skills/` from
[jakubkrehel/skills](https://github.com/jakubkrehel/skills) at commit
`547d415dc10a23e66088eba5aa6664e6e772432c` (2026-11-18). They are MIT-licensed;
`LICENSE-jakubkrehel-skills` is their license.

| Skill | Use it for |
|---|---|
| `better-interface` | A review of a view across all the skills below, as one ranked report |
| `better-ui` | Polish: radius, optical alignment, elevation, icons, motion |
| `better-layout` | Grouping, alignment, reading order, room for longer text |
| `better-typography` | Type scale, spacing, truncation, numbers |
| `better-colors` | Palettes, semantic tokens, contrast |
| `better-accessibility` | Keyboard and focus, accessible names, motion, zoom |
| `better-writing` | Labels, errors, empty states, confirmations |
| `interface-review` | The interface problems a branch or PR introduced (user-invoked) |
| `explain-interface` | How a site or effect was built, from a URL or a screenshot (user-invoked) |
| `build-design` | Building a view from a Figma file or an image |
| `break`, `state-machine`, `variant` | A component under every scenario, every state, or several variants (user-invoked) |
| `afk`, `upskill` | Unattended improvement passes; skills and hooks from past sessions (user-invoked) |

Their examples are CSS, Tailwind and React, and some render on a web page. Studio is GPUI, so:

- Express each fix in GPUI and Studio's design tokens, never in a second styling system.
- Where Studio already has a rule, Studio's rule wins: nested corners through
  `ds::radius::around`/`within` and Zed's buttons at `radius::BUTTON` (spec 002, Visual language),
  colours and sizes as tokens only (`script/check-design-tokens.py`), what a keystroke may
  draw (`docs/drawing.md`), and a destructive action confirmed with ↵ or its button, never by
  typing a name (constitution IV; `better-writing`'s "Type 'acme-web' to confirm" does not
  apply).
- Advice that only applies to a browser (`will-change`, `text-wrap`, `-webkit-font-smoothing`,
  the browser's Animations panel, ARIA) has no GPUI counterpart; skip it, or use its GPUI
  equivalent where one exists.
- Where a skill renders a component on a throwaway web page (`break`, `state-machine`,
  `variant`), draw it in a GPUI test or an offscreen snapshot instead
  (`crates/studio/examples/snapshot.rs`).

To move to a newer upstream version, replace these folders with that commit's `skills/` and its
`LICENSE`, and update the commit and date above.
