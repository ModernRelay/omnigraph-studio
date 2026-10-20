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
- a `ui` component the design system wraps (`Switch`, `Headline`, `Divider`, `Banner`,
  `Callout`, `Chip`, `Checkbox`, which draw their own corners, tones and spacing): views take
  `ds::switch`, `ds::text::title` and `ds::text::heading`, `ds::rule_h`, `ds::banner`,
  `ds::callout`, `ds::chip` or `ds::tag`, and `ds::checkbox`;
- GPUI's text sizes (`.text_sm()`) and an opacity written as a number (`.opacity(0.6)`): views
  take `ds::Prose` and `ds::opacity` or a `ds::surface` role;
- a label made outside `ds::text` (`Label::new`): every view's text takes a role (`plain`,
  `body`, `muted`, `meta`, `state`, `title`, `heading`, `page_title`, `error`, `error_note`);
- a label or an icon given a size (`Label::new(..).size(LabelSize::Small)`,
  `Icon::new(..).size(IconSize::XSmall)`), or a label given only the muted colour: views take
  sizes from `ds::text::body`, `muted`, `meta`, `state`, `title`, `heading`, `error`,
  `error_note` and `ds::icon::lead`, `meta`, and add only a semantic `ui` `Color`;
- a sized or tinted icon given to a button (`.start_icon(Icon::new(..).size(..))`,
  `.start_icon(ds::icon::lead(..))`): a button sizes and inks its own icons, so it takes a
  plain `Icon::new(..)`;
- a button's size or an icon button's icon size named directly (`.size(ButtonSize::Compact)`,
  `.icon_size(IconSize::XSmall)`): views take the role for where the button sits
  (`ds::button`, `toolbar_button`, `bar_button`, `row_button` and their icon forms,
  `ds::inline_icon_button`), whose size gives the icon its size;
- a border wider than a hairline written as a GPUI utility (`.border_l_2()`): views take
  `ds::size::ACCENT_LINE` or `DRAG_LINE`;
- a turning icon (`.with_rotate_animation(..)`): work under way is `ds::spinner()`;
- any icon or type size named in a view (`IconSize::XSmall`, `LabelSize::Small`,
  `ui::TextSize::Small.rems(cx)`, `.text_ui_sm(cx)`): icons take `ds::icon::lead` or `meta`,
  labels a `ds::text` role, and other text `ds::text::plain_size`, `body_size` or
  `meta_size`.
The last two rules read a file whole, since a builder chain spans lines.

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
    ("component outside the design system",
     re.compile(r"\b(Switch|Headline|Banner|Callout|Chip|Checkbox)::new\(|\bDivider::(horizontal|vertical)")),
    ("GPUI text size", re.compile(r"\.text_(xs|sm|base|lg|xl|2xl|3xl)\(\)")),
    ("label outside ds::text", re.compile(r"\bLabel::new\(")),
    ("opacity as a number", re.compile(r"\.opacity\(\s*0?\.\d")),
    ("button size outside a ds role", re.compile(r"\bButtonSize::[A-Z]|\.icon_size\(")),
    ("icon size outside ds::icon", re.compile(r"\bIconSize::[A-Z]")),
    ("border wider than a hairline", re.compile(r"\.border_([tblrxy]_)?([2-9]|[1-9][0-9])\(\)")),
    ("spinner outside ds::spinner", re.compile(r"\.with_rotate_animation\(")),
    ("type size outside ds::text",
     re.compile(r"\b(TextSize|LabelSize)::[A-Z]|\.text_ui(_xs|_sm|_lg)?\(")),
]
EXEMPT = re.compile(r"//\s*ds-exempt:\s*\S")
# Builder chains that rebuild a role, whichever order the two calls come in.
LABEL_SIZE = r"\.size\(LabelSize::(?:Small|XSmall)\)"
LABEL_TONE = r"\.color\(Color::(?:Muted|Error)\)"
ICON_SIZE = r"\.size\(IconSize::(?:Small|XSmall)\)"
ROLE_CHAINS = re.compile(
    rf"\b(?:Label::new\((?:[^()]|\([^()]*\))*\)\s*(?:{LABEL_SIZE}\s*{LABEL_TONE}|{LABEL_TONE}\s*{LABEL_SIZE}"
    rf"|\.size\(LabelSize::Small\)(?!\s*\.color\()"
    rf"|\.color\(Color::Muted\)(?!\s*\.size\())"
    rf"|Icon::new\((?:[^()]|\([^()]*\))*\)\s*(?:{ICON_SIZE}\s*\.color\(Color::Muted\)"
    rf"|\.color\(Color::Muted\)\s*{ICON_SIZE}))"
)
def chain_end(text, open_at):
    """The index after the parenthesis closing the one at `open_at`, or -1."""
    depth, j, quoted = 0, open_at, False
    while j < len(text):
        ch = text[j]
        if quoted:
            if ch == "\\":
                j += 2
                continue
            if ch == '"':
                quoted = False
        elif ch == '"':
            quoted = True
        elif ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth == 0:
                return j + 1
        j += 1
    return -1


CALL = re.compile(r"\s*\.(\w+)\(")


def sized_builders(text):
    """Each `Label::new(..)` or `Icon::new(..)` whose builder chain sets a size."""
    for match in re.finditer(r"\b(Label|Icon)::new\(", text):
        end = chain_end(text, match.end() - 1)
        while end > 0:
            call = CALL.match(text, end)
            if not call:
                break
            if call.group(1) == "size":
                yield match.start()
                break
            end = chain_end(text, call.end() - 1)


# A button's icon with a size or a colour of its own, or an icon role, which the button
# overrides with its own size and ink.
BUTTON_ICONS = re.compile(
    r"\.(?:start|end)_icon\(\s*(?:(?:studio_ui|crate)::ds::icon::|"
    r"Icon::new\((?:[^()]|\([^()]*\))*\)\s*\.(?:size|color)\()"
)
# The Zed menu fork stays close to upstream.
ROLE_EXEMPT_FILES = {"crates/studio_ui/src/menu.rs"}


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
        text = path.read_text()
        if rel not in ROLE_EXEMPT_FILES:
            body = text.split("#[cfg(test)]\nmod ")[0]
            for match in ROLE_CHAINS.finditer(body):
                line = body.count("\n", 0, match.start()) + 1
                errors.append(f"{rel}:{line}: role built by hand: {match.group(0).split(chr(10))[0].strip()}")
            for start in sized_builders(body):
                line = body.count("\n", 0, start) + 1
                errors.append(f"{rel}:{line}: size set by hand: {body[start:].split(chr(10))[0].strip()}")
            for match in BUTTON_ICONS.finditer(body):
                line = body.count("\n", 0, match.start()) + 1
                errors.append(f"{rel}:{line}: a button's icon sized or tinted: {match.group(0).strip()}")
        lines = text.splitlines()
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
