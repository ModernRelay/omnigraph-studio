#!/usr/bin/env python3
"""Writes assets/themes/studio/studio-theme.json: Studio Light and Studio Dark (spec 002, "Gray
tones").

Each is Vitesse Refined Light or Dark (assets/themes/vitesse-refined/, MIT) with every interface
grey replaced by interfaces.dev's grey scale (its `--color-gray-*`, `--color-gray-background`,
`--color-preview-*`), light and dark. Vitesse keeps its syntax, terminal and status colours. Run it
again after changing a grey here; the theme file is generated, not edited.

    python3 script/studio-theme.py
"""

import json
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
SOURCE = ROOT / "assets/themes/vitesse-refined/vitesse-theme-refined.json"
OUT = ROOT / "assets/themes/studio/studio-theme.json"

# interfaces.dev's scale: `--color-gray-100` … `-1200`, the page (`--color-gray-background`), and a
# card (`--color-preview-bg`, `--color-preview-border`).
LIGHT = {
    100: "#fcfcfc", 200: "#f6f6f6", 300: "#f0f0f0", 400: "#e8e8e8", 500: "#e0e0e0",
    600: "#d9d9d9", 700: "#cecece", 800: "#bbbbbb", 900: "#8d8d8d", 1000: "#838383",
    1100: "#646464", 1200: "#202020",
    "page": "#fcfcfc", "card": "#ffffff", "card_border": "#e8e8e8",
}
DARK = {
    100: "#111111", 200: "#191919", 300: "#222222", 400: "#252525", 500: "#313131",
    600: "#3a3a3a", 700: "#484848", 800: "#606060", 900: "#6e6e6e", 1000: "#7b7b7b",
    1100: "#b5b5b5", 1200: "#eeeeee",
    "page": "#101010", "card": "#0b0b0b", "card_border": "#1f1f1f",
}


def tint(hex_colour, alpha):
    """`hex_colour` at `alpha` (0..1), as Zed's `#rrggbbaa`."""
    return f"{hex_colour}{round(alpha * 255):02x}"


def greys(g, dark):
    """The interface greys of one theme. A hover, a selection or a guide is the text grey at a
    low alpha, so it holds on every fill; the page is the window's chrome and a card the
    document and anything that floats, as on interfaces.dev."""
    ink = g[1200]
    floating = g[200] if dark else g["card"]
    clear = "#00000000"
    return {
        "border": g[400],
        "border.variant": g[300],
        "border.focused": g[1100],
        "border.selected": g[400],
        "border.transparent": clear,
        "border.disabled": g[300],
        "background": g["page"],
        "surface.background": g["page"],
        "elevated_surface.background": floating,
        "element.background": g[200],
        "element.hover": tint(ink, 0.06),
        "element.active": tint(ink, 0.08),
        "element.selected": tint(ink, 0.08),
        "element.disabled": g["page"],
        "element.selection_background": tint(ink, 0.15),
        "drop_target.background": tint(ink, 0.06),
        "ghost_element.hover": tint(ink, 0.06),
        "ghost_element.active": tint(ink, 0.08),
        "ghost_element.selected": tint(ink, 0.08),
        "ghost_element.disabled": g["page"],
        "text": ink,
        "text.muted": g[1100],
        "text.placeholder": g[800],
        "text.disabled": g[800],
        "icon": g[1100],
        "icon.muted": g[1000],
        "icon.disabled": g[800],
        "icon.placeholder": g[800],
        "status_bar.background": g["page"],
        "title_bar.background": g["page"],
        "title_bar.inactive_background": g["page"],
        "toolbar.background": g["card"],
        "tab_bar.background": g["page"],
        "tab.inactive_background": g["page"],
        "tab.active_background": g["card"],
        "search.match_background": g[300],
        "search.active_match_background": tint(ink, 0.1),
        "panel.background": g["page"],
        "panel.focused_border": g[800],
        "panel.indent_guide": g[300],
        "panel.indent_guide_active": tint(ink, 0.1),
        "panel.indent_guide_hover": tint(ink, 0.1),
        "panel.overlay_background": floating,
        "panel.overlay_hover": g[100],
        "pane.focused_border": g[300],
        "pane_group.border": g[400],
        "scrollbar.thumb.background": tint(ink, 0.1),
        "scrollbar.thumb.hover_background": tint(ink, 0.15),
        "scrollbar.thumb.active_background": tint(ink, 0.2),
        "scrollbar.thumb.border": clear,
        "scrollbar.track.background": clear,
        "scrollbar.track.border": clear,
        "minimap.thumb.background": tint(ink, 0.1),
        "minimap.thumb.hover_background": tint(ink, 0.15),
        "minimap.thumb.active_background": tint(ink, 0.2),
        "minimap.thumb.border": clear,
        "editor.foreground": ink,
        "editor.background": g["card"],
        "editor.gutter.background": g["card"],
        "editor.subheader.background": g["card"],
        "editor.active_line.background": g[100],
        "editor.highlighted_line.background": g["card"],
        "editor.line_number": g[800],
        "editor.active_line_number": g[1100],
        "editor.hover_line_number": g[1100],
        "editor.invisible": tint(ink, 0.1),
        "editor.wrap_guide": tint(ink, 0.1),
        "editor.active_wrap_guide": tint(ink, 0.1),
        "editor.indent_guide": g[300],
        "editor.indent_guide_active": tint(ink, 0.1),
        "editor.debugger_active_line.background": g[100],
        "hint": g[900],
        "predictive": g[900],
        "hidden": g[800],
        "ignored": g[800],
        "unreachable": g[800],
    }


def build(name, source_name, g, dark):
    source = json.loads(SOURCE.read_text())
    theme = next(t for t in source["themes"] if t["name"] == source_name)
    style = dict(theme["style"])
    style.update(greys(g, dark))
    # Vim's mode colours follow the text, as the rest does.
    for key in list(style):
        if key.startswith("vim.") and key.endswith(".foreground"):
            style[key] = g[1200]
        if key.startswith("vim.") and key.endswith(".background"):
            style[key] = g["card"]
    return {"name": name, "appearance": "dark" if dark else "light", "style": style}


def main():
    family = {
        "$schema": "https://zed.dev/schema/themes/v0.2.0.json",
        "name": "Studio",
        "author": "Omnigraph Studio",
        "themes": [
            build("Studio Light", "Vitesse Refined Light", LIGHT, False),
            build("Studio Dark", "Vitesse Refined Dark", DARK, True),
        ],
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(family, indent=2) + "\n")
    print(f"wrote {OUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
