#!/usr/bin/env python3
"""Write Studio's development graph as Omnigraph NDJSON load envelopes.

Deterministic for a given size, so screenshots and tests see the same rows.

    python3 script/dev-seed.py > dev/seed.jsonl                # 200 people
    python3 script/dev-seed.py --people 100000 > big.jsonl     # for paging and canvas spikes
"""

import argparse
import json
import random
import sys

FIRST = [
    "Ada", "Alan", "Barbara", "Claude", "Donald", "Edsger", "Frances", "Grace", "Hedy", "Ivan",
    "Jean", "Ken", "Leslie", "Margaret", "Niklaus", "Ole", "Radia", "Shafi", "Tim", "Yukihiro",
]
LAST = [
    "Lovelace", "Turing", "Liskov", "Shannon", "Knuth", "Dijkstra", "Allen", "Hopper", "Lamarr",
    "Sutherland", "Sammet", "Thompson", "Lamport", "Hamilton", "Wirth", "Dahl", "Perlman",
    "Goldwasser", "Berners-Lee", "Matsumoto",
]
TITLES = ["Engineer", "Researcher", "Designer", "Manager", "Founder", "Analyst", None]
TAGS = ["graphs", "databases", "compilers", "ui", "security", "ml", "systems", "web"]
CITIES = ["London", "Berlin", "San Francisco", "Tokyo", "Lisbon", "Toronto", None]
STATUS = ["active", "paused", "done"]
ROLES = ["staff", "lead", "advisor", "contractor", None]


def date(rng, start_year, end_year):
    return f"{rng.randint(start_year, end_year):04d}-{rng.randint(1, 12):02d}-{rng.randint(1, 28):02d}"


def node(type_name, data):
    return {"type": type_name, "data": {k: v for k, v in data.items() if v is not None}}


def edge(edge_name, src, dst, data=None):
    out = {"edge": edge_name, "from": src, "to": dst}
    if data:
        out["data"] = {k: v for k, v in data.items() if v is not None}
    return out


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--people", type=int, default=200)
    parser.add_argument("--seed", type=int, default=7)
    args = parser.parse_args()
    rng = random.Random(args.seed)

    companies = max(5, args.people // 8)
    projects = max(8, args.people // 5)
    out = sys.stdout

    def emit(record):
        out.write(json.dumps(record, separators=(",", ":")) + "\n")

    for i in range(companies):
        emit(node("Company", {
            "slug": f"company-{i:05d}",
            "name": f"{rng.choice(LAST)} {rng.choice(['Labs', 'Systems', 'Works', 'Group', 'AI'])}",
            "founded": rng.randint(1950, 2025) if rng.random() > 0.1 else None,
            "city": rng.choice(CITIES),
        }))
    for i in range(projects):
        emit(node("Project", {
            "slug": f"project-{i:05d}",
            "name": f"Project {rng.choice(['Atlas', 'Beacon', 'Comet', 'Delta', 'Ember', 'Fable'])} {i}",
            "status": rng.choice(STATUS),
            "started": date(rng, 2015, 2026) if rng.random() > 0.2 else None,
        }))
    for i in range(args.people):
        first, last = rng.choice(FIRST), rng.choice(LAST)
        emit(node("Person", {
            "slug": f"person-{i:06d}",
            "name": f"{first} {last}",
            "title": rng.choice(TITLES),
            "email": f"{first.lower()}.{last.lower()}{i}@example.com" if rng.random() > 0.3 else None,
            "born": date(rng, 1950, 2002) if rng.random() > 0.4 else None,
            "tags": rng.sample(TAGS, rng.randint(0, 3)) or None,
        }))

    for i in range(args.people):
        person = f"person-{i:06d}"
        for c in rng.sample(range(companies), rng.choice([0, 1, 1, 1, 2])):
            emit(edge("WorksAt", person, f"company-{c:05d}", {
                "role": rng.choice(ROLES),
                "since": date(rng, 2000, 2026) if rng.random() > 0.3 else None,
            }))
        for p in rng.sample(range(args.people), min(args.people - 1, rng.randint(0, 4))):
            if p != i:
                emit(edge("Knows", person, f"person-{p:06d}"))
        for p in rng.sample(range(projects), rng.randint(0, 3)):
            emit(edge("Contributes", person, f"project-{p:05d}", {
                "hours": rng.randint(1, 400) if rng.random() > 0.2 else None,
            }))
    for p in range(projects):
        emit(edge("Owns", f"company-{rng.randrange(companies):05d}", f"project-{p:05d}"))


if __name__ == "__main__":
    main()
