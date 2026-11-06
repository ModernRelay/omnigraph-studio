#!/usr/bin/env python3
"""Write a large graph for measuring Studio at scale: its schema, and its rows as Omnigraph
NDJSON load envelopes. Deterministic for a given size.

- `Account`: a wide type, `slug` and `name` and 93 more columns of every scalar kind
  (95 in all), `--accounts` rows (100,000 by default).
- `Tag`: 1,000 rows. `Region`: 50 rows.
- `Follows: Account -> Account`, `--follows` per account (5: 500,000 edges by default).
- `Tagged: Account -> Tag`, 2 per account. `In: Account -> Region`, 1 per account.

    python3 script/dev-seed-big.py --schema > big.pg
    python3 script/dev-seed-big.py > big.jsonl
    python3 script/dev-seed-big.py --accounts 1000 > small.jsonl

`script/dev-big-server` serves it, with history and branches made through the server.
"""

import argparse
import json
import random
import sys

KINDS = ["String", "I64", "F64", "Bool", "Date", "I32", "DateTime"]
WIDE = 93
WORDS = [
    "amber", "basalt", "cedar", "delta", "ember", "fjord", "garnet", "harbor", "iris", "juniper",
    "kelp", "lumen", "meadow", "nectar", "onyx", "pollen", "quartz", "river", "slate", "tundra",
]


def column(i):
    return f"c{i:02d}", KINDS[i % len(KINDS)]


def schema():
    lines = ["// A large graph for measuring Studio at scale (script/dev-seed-big.py).", ""]
    lines.append("node Account {")
    lines.append("  slug: String @key")
    lines.append("  name: String")
    for i in range(WIDE):
        name, kind = column(i)
        lines.append(f"  {name}: {kind}?")
    lines.append("}")
    lines += ["", "node Tag {", "  slug: String @key", "  name: String", "}"]
    lines += ["", "node Region {", "  slug: String @key", "  name: String", "}"]
    lines += [
        "",
        "edge Follows: Account -> Account @card(0..) {",
        "  weight: F64?",
        "}",
        "",
        "edge Tagged: Account -> Tag @card(0..)",
        "",
        "edge In: Account -> Region @card(0..)",
    ]
    return "\n".join(lines) + "\n"


def value(rng, kind, i, row):
    if rng.random() < 0.15:
        return None
    if kind == "String":
        return " ".join(rng.choice(WORDS) for _ in range(1 + (i + row) % 6))
    if kind in ("I64", "I32"):
        return rng.randint(-100000, 100000)
    if kind == "F64":
        return round(rng.uniform(-1000, 1000), 3)
    if kind == "Bool":
        return rng.random() < 0.5
    if kind == "Date":
        return f"{rng.randint(1950, 2026):04d}-{rng.randint(1, 12):02d}-{rng.randint(1, 28):02d}"
    return (
        f"{rng.randint(2000, 2026):04d}-{rng.randint(1, 12):02d}-{rng.randint(1, 28):02d}"
        f"T{rng.randint(0, 23):02d}:{rng.randint(0, 59):02d}:00Z"
    )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--schema", action="store_true", help="print the schema and stop")
    parser.add_argument("--accounts", type=int, default=100_000)
    parser.add_argument("--follows", type=int, default=5)
    args = parser.parse_args()
    if args.schema:
        sys.stdout.write(schema())
        return
    rng = random.Random(7)
    out = sys.stdout
    write = lambda record: out.write(json.dumps(record, separators=(",", ":")) + "\n")
    for r in range(50):
        write({"type": "Region", "data": {"slug": f"region-{r:02d}", "name": f"Region {r}"}})
    for t in range(1000):
        write({"type": "Tag", "data": {"slug": f"tag-{t:04d}", "name": rng.choice(WORDS) + f"-{t}"}})
    for a in range(args.accounts):
        data = {"slug": f"account-{a:06d}", "name": f"{rng.choice(WORDS).title()} {a}"}
        for i in range(WIDE):
            name, kind = column(i)
            v = value(rng, kind, i, a)
            if v is not None:
                data[name] = v
        write({"type": "Account", "data": data})
    for a in range(args.accounts):
        src = f"account-{a:06d}"
        for _ in range(args.follows):
            dst = f"account-{rng.randrange(args.accounts):06d}"
            write({"edge": "Follows", "from": src, "to": dst, "data": {"weight": round(rng.random(), 3)}})
        for _ in range(2):
            write({"edge": "Tagged", "from": src, "to": f"tag-{rng.randrange(1000):04d}"})
        write({"edge": "In", "from": src, "to": f"region-{rng.randrange(50):02d}"})


if __name__ == "__main__":
    main()
