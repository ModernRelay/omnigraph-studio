#!/usr/bin/env python3
"""script/measure-expand-all runs where only BSD tools are, as on macOS: shims in front of
PATH refuse what GNU alone accepts (`realpath -m`, `date +%N`, `\\|` in a basic grep), a fake
`curl` answers the server's health check, and a fake snapshot binary runs the `run:` steps and
writes the reads JSON the script reads. The script must exit 0 and print the measured step.

    python3 script/test_measure_expand_all.py
"""

import json
import os
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "script" / "measure-expand-all"

# Each shim refuses a GNU-only form and hands anything else to the real tool, found on the
# PATH after the shims.
SHIMS = {
    "realpath": r"""
for arg in "$@"; do [ "$arg" = -m ] && { echo "realpath: illegal option -- m" >&2; exit 1; }; done
""",
    "date": r"""
case "$*" in *%N*) echo "date: %N is GNU only" >&2; exit 1;; esac
""",
    "grep": r"""
for arg in "$@"; do case "$arg" in -*) ;; *'\|'*) echo "grep: \| is GNU only" >&2; exit 2;; esac; done
""",
    "curl": "exit 0\n",
}

# The snapshot example's stand-in: runs each `run:` step as the example does (`sh -c`), and
# writes a reads file whose last step asked the most.
FAKE_SNAPSHOT = r"""#!/usr/bin/env python3
import json, subprocess, sys
reads = None
for arg in sys.argv[2:]:
    if arg.startswith("reads:"):
        reads = arg[len("reads:"):]
    elif arg.startswith("run:"):
        subprocess.run(["sh", "-c", arg[len("run:"):]], check=True)
step = {"asked": 1831, "sent": 68, "first_ms": 595.0, "settled_ms": 595.0, "timed_out": False,
        "reads": [{"count": 1763, "what": "count", "asker": "View(Exploration)", "answered": "Batched"}]}
quiet = dict(step, asked=2, sent=2, reads=[])
json.dump({"steps": [quiet, step]}, open(reads, "w"))
"""


def main() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        tmp = pathlib.Path(tmp)
        shims = tmp / "shims"
        shims.mkdir()
        path = os.environ.get("PATH", "")
        real_path = ":".join(p for p in path.split(":") if p)
        for name, body in SHIMS.items():
            shim = shims / name
            # The real tool, looked up past the shims.
            shim.write_text(
                "#!/bin/sh\n"
                + body
                + f'PATH="{real_path}" exec {name} "$@"\n'
            )
            shim.chmod(0o755)
        fake = tmp / "snapshot"
        fake.write_text(FAKE_SNAPSHOT)
        fake.chmod(0o755)
        out = tmp / "out" / "reads.json"
        out.parent.mkdir()
        env = dict(os.environ, PATH=f"{shims}:{path}")
        run = subprocess.run(
            ["bash", str(SCRIPT), str(fake), str(out)],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
        )
        if run.returncode != 0:
            print(run.stdout, run.stderr, sep="\n", file=sys.stderr)
            print(f"measure-expand-all exited {run.returncode} with BSD-like tools", file=sys.stderr)
            return 1
        if "Expand all: 1831 asked, 68 sent" not in run.stdout:
            print(run.stdout, file=sys.stderr)
            print("measure-expand-all did not print the measured step", file=sys.stderr)
            return 1
        json.loads(out.read_text())
    print("measure-expand-all ok with BSD-like tools")
    return 0


if __name__ == "__main__":
    sys.exit(main())
