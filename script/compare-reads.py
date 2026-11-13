#!/usr/bin/env python3
"""Compare two runs of `script/measure-frames --reads` (spec 030): for each step, the requests
sent, the reads asked, the time to the first answer and to settled, and each tally (what was
read, who asked, how it was answered) whose count changed, marked `*`.

    script/compare-reads.py before.json after.json
"""

import json
import sys


def load(path):
    with open(path) as f:
        return json.load(f)


def ms(value):
    return "-" if value is None else f"{value:.0f} ms"


def key(read):
    return (read["what"], read["asker"], read["answered"])


def main():
    if len(sys.argv) != 3:
        sys.exit("usage: compare-reads.py <before.json> <after.json>")
    before, after = load(sys.argv[1]), load(sys.argv[2])
    print(f"latency {before.get('latency_ms')} ms -> {after.get('latency_ms')} ms")
    for index, b in enumerate(before["steps"]):
        a = after["steps"][index] if index < len(after["steps"]) else None
        if a is None or a["step"] != b["step"]:
            print(f"\n{b['step']}: no such step after")
            continue
        late = " (not settled)" if b["timed_out"] or a["timed_out"] else ""
        print(
            f"\n{b['step']}: sent {b['sent']} -> {a['sent']}, asked {b['asked']} -> {a['asked']}, "
            f"first {ms(b['first_ms'])} -> {ms(a['first_ms'])}, "
            f"settled {ms(b['settled_ms'])} -> {ms(a['settled_ms'])}{late}"
        )
        counts_b = {key(r): r["count"] for r in b["reads"]}
        counts_a = {key(r): r["count"] for r in a["reads"]}
        for k in sorted(set(counts_b) | set(counts_a)):
            nb, na = counts_b.get(k, 0), counts_a.get(k, 0)
            mark = "" if nb == na else "  *"
            print(f"  {nb:>5} -> {na:<5} {k[0]:<28} {k[1]:<18} {k[2]}{mark}")
    for b in after["steps"][len(before["steps"]):]:
        print(f"\n{b['step']}: only after")


if __name__ == "__main__":
    main()
