#!/usr/bin/env python3
"""The product is written Omnigraph: one word, a lower-case g.

    script/check-brand.py

Refuses any other form (a capital G, or a space or hyphen inside the name) in every tracked
text file. Identifiers and paths stay `omnigraph`, environment variables `OMNIGRAPH_`. The
copies of upstream files Studio keeps unmodified are left as upstream wrote them: they change
when the copy is updated, never by editing it.
"""
import os
import re
import subprocess
import sys

WRONG = re.compile(r"Omni[ -]?Graph|Omni[ -]graph")
UPSTREAM = (
    "crates/agent_panel/fixtures/omnigraph-operator-config.schema.json",
    "crates/agent_panel/skill/upstream/",
    "crates/graph_client/openapi/",
)

listed = subprocess.run(["git", "ls-files", "-z"], capture_output=True, check=True).stdout
errors = []
checked = 0
for path in listed.decode().split("\0"):
    if not path or path.startswith(UPSTREAM) or os.path.islink(path):
        continue
    try:
        with open(path, encoding="utf-8") as file:
            text = file.read()
    except (UnicodeDecodeError, FileNotFoundError, IsADirectoryError):
        continue  # binary, deleted in the working tree, or a submodule
    checked += 1
    for number, line in enumerate(text.splitlines(), 1):
        for match in WRONG.finditer(line):
            errors.append(f"{path}:{number}: {match.group()} → Omnigraph")

if errors:
    print("\n".join(errors))
    print(f"{len(errors)} misspellings of Omnigraph")
    sys.exit(1)
print(f"brand ok: {checked} files")
