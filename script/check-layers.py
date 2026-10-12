#!/usr/bin/env python3
"""Fail if a crate depends on a Studio crate in the same or a higher layer.

Layers are defined in docs/architecture.md; keep this table in step with it.
"""
import glob
import sys
import tomllib

LAYERS = {
    "studio_actions": 0,
    "studio_settings": 1,
    "studio_theme": 1,
    "graph_client": 2,
    "gq_language": 2,
    "studio_ui": 2,
    "graph_store": 3,
    "studio_workspace": 4,
    "command_palette": 5,
    "type_table": 5,
    "relationship_tree": 5,
    "graph_canvas": 5,
    "query_editor": 5,
    "review": 5,
    "history": 5,
    "inspectors": 5,
    "studio": 6,
}
NO_GPUI = {"graph_client", "gq_language"}

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

if errors:
    print("\n".join(errors))
    sys.exit(1)
print(f"layers ok: {len(glob.glob('crates/*/Cargo.toml'))} crates")
