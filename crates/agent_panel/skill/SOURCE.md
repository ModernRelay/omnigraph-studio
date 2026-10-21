# The OmniGraph guide in Studio's agent skill

`upstream/` is a copy of the OmniGraph skill, unmodified: `skills/omnigraph/` and `LICENSE`
from [ModernRelay/omnigraph](https://github.com/ModernRelay/omnigraph) at tag
`edge-20261020-7fd9cc9` (commit `7fd9cc9037c634e2f3284f1ede55a85a6f7b28e8`), the first build
that speaks HTTP contract 0.14, as Studio does; its CLI has targets, which the agent's CLI must
have. It is MIT-licensed; `upstream/LICENSE` is its license.

Studio writes it into each graph's agent working directory as the `omnigraph-studio` skill
(`src/skill.rs`): `SKILL.md` here is the entry point, and the upstream `SKILL.md` becomes
`GUIDE.md`, so agents do not list it as a second skill.

To move to another release, replace `upstream/` with that tag's `skills/omnigraph/` and
`LICENSE`, update the tag and commit above and `guide-version` in `SKILL.md`, and keep the file
list in `src/skill.rs` complete (a test checks it).
