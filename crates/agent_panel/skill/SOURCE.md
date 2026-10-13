# The OmniGraph guide in Studio's agent skill

`upstream/` is a copy of the OmniGraph skill, unmodified: `skills/omnigraph/` and `LICENSE`
from [ModernRelay/omnigraph](https://github.com/ModernRelay/omnigraph) at tag `v0.13.0`
(commit `926ba3528c51583049c862f02a9604a610a9f560`), the release whose HTTP contract (0.13)
Studio speaks. It is MIT-licensed; `upstream/LICENSE` is its license.

Studio writes it into each graph's agent working directory as the `omnigraph-studio` skill
(`src/skill.rs`): `SKILL.md` here is the entry point, and the upstream `SKILL.md` becomes
`GUIDE.md`, so agents do not list it as a second skill.

To move to another release, replace `upstream/` with that tag's `skills/omnigraph/` and
`LICENSE`, update the tag and commit above and `guide-version` in `SKILL.md`, and keep the file
list in `src/skill.rs` complete (a test checks it).
