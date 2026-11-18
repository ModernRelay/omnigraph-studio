# The Omnigraph guide in Studio's agent skill

`upstream/` is a copy of the Omnigraph skill, unmodified: `skills/omnigraph/` and `LICENSE`
from [ModernRelay/omnigraph](https://github.com/ModernRelay/omnigraph) at tag `v0.15.0`
(commit `373834b4c1c4b29cf0ddd64bee9565f98c60cc26`), the release Studio pins. Studio's agent
drives a CLI of `v0.14.0` or later, which reads the version-1 operator configuration Studio
writes and speaks HTTP contract 0.14, as Studio does; the guide follows 0.15, so on a 0.14 CLI a
few commands it names (`index build`, `--format toon`) do not exist yet, and `@card` minimums
are checked only as 0.14 checked them. It is MIT-licensed; `upstream/LICENSE` is its license.

Studio writes it into each graph's agent working directory as the `omnigraph-studio` skill
(`src/skill.rs`): `SKILL.md` here is the entry point, and the upstream `SKILL.md` becomes
`GUIDE.md`, so agents do not list it as a second skill.

To move to another release, replace `upstream/` with that commit's `skills/omnigraph/` and
`LICENSE` (`git archive <commit> skills/omnigraph LICENSE`), update the tag and commit above and
`guide-version` in `SKILL.md`, and keep the file list in `src/skill.rs` complete (a test checks
it).
