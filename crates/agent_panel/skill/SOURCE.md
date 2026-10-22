# The OmniGraph guide in Studio's agent skill

`upstream/` is a copy of the OmniGraph skill, unmodified: `skills/omnigraph/` and `LICENSE`
from [ModernRelay/omnigraph](https://github.com/ModernRelay/omnigraph) at tag
`edge-20261022-7fff9df` (main commit `7fff9dfd91835201986d72f3e3e014b36c3e636a`), the edge
build the agent's CLI is checked against. It contains main `ff542202`, the first CLI that reads
the version-1 operator configuration Studio writes for the agent, and it speaks HTTP contract
0.14, as Studio does. It is MIT-licensed; `upstream/LICENSE` is its license.

Studio writes it into each graph's agent working directory as the `omnigraph-studio` skill
(`src/skill.rs`): `SKILL.md` here is the entry point, and the upstream `SKILL.md` becomes
`GUIDE.md`, so agents do not list it as a second skill.

To move to another build, replace `upstream/` with that commit's `skills/omnigraph/` and
`LICENSE` (`git archive <commit> skills/omnigraph LICENSE`), update the tag and commit above and
`guide-version` in `SKILL.md`, and keep the file list in `src/skill.rs` complete (a test checks
it).
