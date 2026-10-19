#!/usr/bin/env python3
"""Every size, space, corner, colour and shadow a view draws comes from the design system.

    script/check-design-tokens.py

Views (every crate but the design system itself) take their values from `studio_ui::ds`
tokens and components (spec 002, "Visual language"), so a change to the system reaches
every surface and nothing drifts. This refuses, outside the design system and tests:

- a literal size, space, corner, offset or type size (`.h(px(12.))`, `.gap(px(6.))`,
  `point(px(-4.), …)`, `rems_from_px(22.)`);
- GPUI's rem utilities (`.gap_2()`, `.px_3()`, `.h_6()`) and its corner and shadow utilities
  (`.rounded_md()`, `.shadow_lg()`);
- a raw colour (`gpui::black()`, `rgb(0x…)`, `hsla(…)`) or the theme's palette read directly
  (`cx.theme().colors()`, `cx.theme().status()`): views name a `ds::surface` role or a `ui`
  `Color`;
- a font weight named directly (`FontWeight::MEDIUM`): views take `ds::text::weight`;
- a `ui` component the design system wraps (`Switch::new`): views take `ds::switch`.

A line the rule cannot fit says why in a `// ds-exempt: <reason>` comment on it or the line
above, so every exception shows up in review.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The design system, where tokens are defined from the theme and from numbers.
SYSTEM = {
    "crates/studio_ui/src/ds.rs",
    "crates/studio_ui/src/button.rs",
    "crates/studio_ui/src/modal.rs",
    "crates/studio_ui/src/menu.rs",
}
# Crates that are not views: the theme itself, and those that draw nothing.
NOT_VIEWS = {"studio_theme", "studio_settings", "studio_actions", "graph_client", "graph_store",
             "gq_language"}

LAYOUT = (
    r"h|w|min_h|max_h|min_w|max_w|size|p|px|py|pt|pb|pl|pr|gap|gap_x|gap_y|m|mx|my|mt|mb|ml|mr|"
    r"top|left|right|bottom|rounded|rounded_t|rounded_b|rounded_l|rounded_r|rounded_tl|"
    r"rounded_tr|rounded_bl|rounded_br|text_size|line_height|border_w"
)
RULES = [
    ("literal size or space", re.compile(rf"\.({LAYOUT})\(px\(-?(?!0\.?\)|0_f32)[0-9][0-9.]*(_f32)?\.?\)\)")),
    ("rem utility", re.compile(
        r"\.(h|w|min_h|max_h|min_w|max_w|size|p|px|py|pt|pb|pl|pr|gap|m|mx|my|mt|mb|ml|mr|"
        r"top|left|right|bottom|inset)_(neg_)?(0p5|1p5|2p5|3p5|[1-9][0-9]*|px)\(\)")),
    ("literal offset", re.compile(
        r"(point|snap_to_window_with_margin|size)\(\s*px\(-?(?!0\.?\))[0-9][0-9.]*\.?\)")),
    ("literal type size", re.compile(r"\brems(_from_px)?\(\s*[0-9]")),
    ("corner utility", re.compile(r"\.rounded_(xs|sm|md|lg|xl|2xl|3xl)\(\)")),
    ("shadow utility", re.compile(r"\.shadow_(2xs|xs|sm|md|lg|xl|2xl)\(\)")),
    ("raw colour", re.compile(r"gpui::(black|white)\(\)|\brgb\(0x|\brgba\(0x|\bhsla\(")),
    ("theme palette", re.compile(r"\.theme\(\)\s*\.\s*(colors|status)\(\)")),
    ("font weight", re.compile(r"\bFontWeight::[A-Z_]+")),
    ("component outside the design system", re.compile(r"\bSwitch::new\(")),
]
EXEMPT = re.compile(r"//\s*ds-exempt:\s*\S")


def views():
    for path in sorted((ROOT / "crates").glob("*/src/**/*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        crate = rel.split("/")[1]
        if rel in SYSTEM or crate in NOT_VIEWS:
            continue
        yield rel, path


def main() -> int:
    errors = []
    for rel, path in views():
        lines = path.read_text().splitlines()
        in_test = False
        for index, line in enumerate(lines):
            # A test module runs to the end of its file; a test-only item elsewhere is
            # still checked.
            if re.match(r"\s*#\[cfg\(test\)\]", line) and re.match(
                r"\s*(pub(\(crate\))?\s+)?mod\s", lines[index + 1] if index + 1 < len(lines) else ""
            ):
                in_test = True
            if in_test:
                continue
            code = line.split("//")[0] if "ds-exempt" not in line else line
            for name, rule in RULES:
                if rule.search(code):
                    previous = lines[index - 1] if index else ""
                    if EXEMPT.search(line) or EXEMPT.search(previous):
                        continue
                    errors.append(f"{rel}:{index + 1}: {name}: {line.strip()}")
    for error in errors:
        print(error)
    if errors:
        print(f"{len(errors)} value(s) not from the design system (script/check-design-tokens.py)")
        return 1
    print("design tokens ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
