#!/usr/bin/env python3
"""Fail if a crate depends on a Studio crate in the same or a higher layer, or if the UI
tools leave their crate.

Layers are defined in docs/architecture.md; keep this table in step with it.
"""
import glob
import re
import sys
import tomllib

LAYERS = {
    "studio_actions": 0,
    "studio_settings": 1,
    "studio_theme": 1,
    "graph_client": 2,
    "control_client": 2,
    "oauth_client": 2,
    "model": 2,
    "gq_language": 2,
    "studio_ui": 2,
    "graph_store": 3,
    "studio_update": 3,
    "filter_words": 3,
    "studio_workspace": 4,
    "change_cards": 4,
    "command_palette": 5,
    "type_table": 5,
    "relationship_tree": 5,
    "graph_canvas": 5,
    "query_editor": 5,
    "review": 5,
    "history": 5,
    "agent_panel": 5,
    "ui_tool_server": 5,
    "studio": 6,
}
NO_GPUI = {
    "graph_client",
    "control_client",
    "gq_language",
    "oauth_client",
    "model",
    "filter_words",
}

errors = []
for manifest in sorted(glob.glob("crates/*/Cargo.toml")):
    data = tomllib.load(open(manifest, "rb"))
    name = data["package"]["name"]
    if name not in LAYERS:
        errors.append(f"{name}: not in the layer table (docs/architecture.md, script/check-layers.py)")
        continue
    deps = set(data.get("dependencies", {}))
    for table in data.get("target", {}).values():
        deps |= set(table.get("dependencies", {}))
    for dep in sorted(deps):
        if dep in LAYERS and LAYERS[dep] >= LAYERS[name]:
            errors.append(f"{name} (layer {LAYERS[name]}) depends on {dep} (layer {LAYERS[dep]})")
    if name in NO_GPUI and deps & {"gpui", "ui", "theme", "gpui_tokio"}:
        errors.append(f"{name} must not depend on GPUI")

# The UI tools stay in their crate (spec 032, "Where each part lives"). Three things keep a
# person who uses no agent, and a crate that serves none, clear of them:
#   1. only the binary depends on the tool crate;
#   2. only the binary's `ui_tools` module and its two test modules name it in code;
#   3. no crate but the tool crate, the binary and the agent panel's tests knows a tool by name:
#      a view says what it shows and why it would not act, and the tool crate words it.
TOOLS = "ui_tool_server"
TOOLS_NAMED_IN = {
    "crates/studio/src/ui_tools.rs",
    "crates/studio/src/ui_tools_tests.rs",
    "crates/studio/src/ui_tools_canvas_tests.rs",
}
TOOL_NAMES_KNOWN_TO = {TOOLS, "studio", "agent_panel"}

for manifest in sorted(glob.glob("crates/*/Cargo.toml")):
    data = tomllib.load(open(manifest, "rb"))
    name = data["package"]["name"]
    if name in (TOOLS, "studio"):
        continue
    tables = [data.get("dependencies", {}), data.get("dev-dependencies", {})]
    tables += [t.get("dependencies", {}) for t in data.get("target", {}).values()]
    if any(TOOLS in table for table in tables):
        errors.append(f"{name} depends on {TOOLS}: only studio does (spec 032)")

tool_list = open(f"crates/{TOOLS}/src/tools.rs", encoding="utf-8").read()
allowed = tool_list[tool_list.index("pub static ALLOWED") :]
allowed = allowed[: allowed.index("\n];")]
tool_names = set(re.findall(r'"([a-z_]+)",\s*"studio[A-Za-z_]*::', allowed))
reads = tool_list[tool_list.index("pub const READS") :]
tool_names |= set(re.findall(r'"([a-z_]+)"', reads[: reads.index("];")]))
# Two reads are named as the canvas's and the tree's action namespaces are: those words are
# every crate's.
tool_names -= {"studio_canvas", "studio_tree"}
if len(tool_names) < 40:
    errors.append(f"only {len(tool_names)} tool names read from {TOOLS}/src/tools.rs")
named = re.compile(r"(?<![A-Za-z0-9_])(" + "|".join(sorted(tool_names)) + r")(?![A-Za-z0-9_])")

for source in sorted(glob.glob("crates/*/**/*.rs", recursive=True)):
    crate = source.split("/")[1]
    if crate == TOOLS:
        continue
    for number, line in enumerate(open(source, encoding="utf-8"), 1):
        if f"{TOOLS}::" in line and source not in TOOLS_NAMED_IN:
            errors.append(
                f"{source}:{number} names {TOOLS}: only crates/studio/src/ui_tools.rs does (spec 032)"
            )
        if crate not in TOOL_NAMES_KNOWN_TO and (found := named.search(line)):
            errors.append(
                f"{source}:{number} names the UI tool `{found.group(1)}`: "
                f"a tool's name is {TOOLS}'s alone (spec 032)"
            )

if errors:
    print("\n".join(errors))
    sys.exit(1)
print(f"layers ok: {len(glob.glob('crates/*/Cargo.toml'))} crates; {len(tool_names)} UI tools in their crate")
