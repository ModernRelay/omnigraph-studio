# The OmniGraph guide in Studio's agent skill

`upstream/` is a copy of the OmniGraph skill, unmodified: `skills/omnigraph/` and `LICENSE`
from [ModernRelay/omnigraph](https://github.com/ModernRelay/omnigraph) at tag
`edge-20261020-b692610` (commit `b692610c4aa31066cb6a9fd34830f56977288943`), the first build
whose CLI has targets, which the agent's CLI must have; it speaks HTTP contract 0.13, as Studio
does. It is MIT-licensed; `upstream/LICENSE` is its license.

Studio writes it into each graph's agent working directory as the `omnigraph-studio` skill
(`src/skill.rs`): `SKILL.md` here is the entry point, and the upstream `SKILL.md` becomes
`GUIDE.md`, so agents do not list it as a second skill.

To move to another release, replace `upstream/` with that tag's `skills/omnigraph/` and
`LICENSE`, update the tag and commit above and `guide-version` in `SKILL.md`, and keep the file
list in `src/skill.rs` complete (a test checks it).
