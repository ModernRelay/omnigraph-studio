# The Omnigraph guide in Studio's agent skill

`upstream/` is a copy of the Omnigraph skill, unmodified: `skills/omnigraph/` and `LICENSE`
from [ModernRelay/omnigraph](https://github.com/ModernRelay/omnigraph) at tag `v0.14.0`
(commit `60bfd19db780fdff3a5178f959c1204143420114`), the oldest release whose CLI Studio's agent
drives: it reads the version-1 operator configuration Studio writes for the agent, and speaks
HTTP contract 0.14, as Studio does. It is MIT-licensed; `upstream/LICENSE` is its license.

Studio writes it into each graph's agent working directory as the `omnigraph-studio` skill
(`src/skill.rs`): `SKILL.md` here is the entry point, and the upstream `SKILL.md` becomes
`GUIDE.md`, so agents do not list it as a second skill.

To move to another release, replace `upstream/` with that commit's `skills/omnigraph/` and
`LICENSE` (`git archive <commit> skills/omnigraph LICENSE`), update the tag and commit above and
`guide-version` in `SKILL.md`, and keep the file list in `src/skill.rs` complete (a test checks
it).
