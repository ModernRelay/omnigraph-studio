#!/usr/bin/env python3
"""Write a dense knowledge graph as Omnigraph NDJSON load envelopes, for measuring Studio.

The shape is script/dense-graph.pg: Topic, Artifact, Mention, Person and Org nodes (about
17,000) joined by thirty edge types (about 119,000 edges). Each edge type picks its targets,
and less steeply its sources, by a power law over a fixed shuffle of each type, so a few nodes
are hubs with thousands of relationships while most have a handful. Every node type has ten
to fourteen relationship groups, which is what makes expanding many nodes at once ask for
thousands of counts. Deterministic for a given seed and scale; standard library only.

    python3 script/dense-graph.py > dense.jsonl
    python3 script/dense-graph.py --scale 0.1 > small.jsonl     # a tenth of every count
    script/dense-server                                          # generates, loads and serves it
"""

import argparse
import bisect
import itertools
import json
import random
import sys

# Nodes of each type at scale 1.
NODES = {"Topic": 1500, "Artifact": 5000, "Mention": 7500, "Person": 2200, "Org": 800}

# (edge, source type, target type, edges at scale 1). `None` edges: at most one per source.
EDGES = [
    ("Broader", "Topic", "Topic", 1400),
    ("RelatedTo", "Topic", "Topic", 3000),
    ("Contrasts", "Topic", "Topic", 800),
    ("AboutTopic", "Artifact", "Topic", 11000),
    ("IntroducesTopic", "Artifact", "Topic", 3000),
    ("CritiquesTopic", "Artifact", "Topic", 1500),
    ("Cites", "Artifact", "Artifact", 12000),
    ("Extends", "Artifact", "Artifact", 2500),
    ("RespondsTo", "Artifact", "Artifact", 2000),
    ("AuthoredBy", "Artifact", "Person", 9000),
    ("EditedBy", "Artifact", "Person", 2000),
    ("PublishedBy", "Artifact", "Org", 4500),
    ("FoundIn", "Mention", "Artifact", None),
    ("QuotesArtifact", "Mention", "Artifact", 3000),
    ("MentionsTopic", "Mention", "Topic", 12000),
    ("SupportsTopic", "Mention", "Topic", 4000),
    ("MentionsPerson", "Mention", "Person", 9000),
    ("SpokenBy", "Mention", "Person", None),
    ("MentionsOrg", "Mention", "Org", 5000),
    ("Echoes", "Mention", "Mention", 3000),
    ("Contradicts", "Mention", "Mention", 1200),
    ("WorksAt", "Person", "Org", 2600),
    ("AdvisesOrg", "Person", "Org", 800),
    ("FoundedOrg", "Person", "Org", 400),
    ("Knows", "Person", "Person", 5000),
    ("ExpertIn", "Person", "Topic", 3500),
    ("FollowsTopic", "Person", "Topic", 4000),
    ("SubsidiaryOf", "Org", "Org", 300),
    ("PartnersWith", "Org", "Org", 900),
    ("FocusesOn", "Org", "Topic", 2000),
]

# The share of Mentions that have each at-most-one edge.
AT_MOST_ONE = {"FoundIn": 0.95, "SpokenBy": 0.4}

# Power-law exponents: the k-th most popular node is picked with weight k^-exponent.
TARGET_EXPONENT = 0.9
SOURCE_EXPONENT = 0.5

WORDS = [
    "graph", "vector", "agent", "memory", "schema", "query", "index", "branch", "commit",
    "ontology", "retrieval", "context", "planner", "compiler", "storage", "lattice", "kernel",
    "signal", "pattern", "embedding", "cluster", "policy", "ledger", "stream", "protocol",
    "network", "model", "theory", "market", "design", "language", "reasoning", "evaluation",
    "privacy", "latency", "consensus", "inference", "search", "ranking", "feedback",
]
FIRST = [
    "Ada", "Alan", "Barbara", "Claude", "Donald", "Edsger", "Frances", "Grace", "Hedy", "Ivan",
    "Jean", "Ken", "Leslie", "Margaret", "Niklaus", "Ole", "Radia", "Shafi", "Tim", "Yukihiro",
]
LAST = [
    "Lovelace", "Turing", "Liskov", "Shannon", "Knuth", "Dijkstra", "Allen", "Hopper", "Lamarr",
    "Sutherland", "Sammet", "Thompson", "Lamport", "Hamilton", "Wirth", "Dahl", "Perlman",
    "Goldwasser", "Berners-Lee", "Matsumoto",
]
ORG_SUFFIX = ["Labs", "Systems", "Institute", "Group", "Works", "AI", "Research", "Foundation"]
CATEGORIES = ["method", "field", "tool", "problem", "concept", None]
ROLES = ["Engineer", "Researcher", "Founder", "Analyst", "Editor", "Professor", None]


def slug(type_name, index):
    return f"{type_name.lower()}-{index:05d}"


def phrase(rng, low, high):
    return " ".join(rng.choice(WORDS) for _ in range(rng.randint(low, high)))


def maybe(rng, share, value):
    return value if rng.random() < share else None


def node_data(rng, type_name, index):
    key = slug(type_name, index)
    if type_name == "Topic":
        name = phrase(rng, 1, 3).title()
        return {
            "slug": key,
            "name": name,
            "summary": maybe(rng, 0.8, f"{name}: {phrase(rng, 6, 14)}."),
            "category": rng.choice(CATEGORIES),
        }
    if type_name == "Artifact":
        return {
            "slug": key,
            "title": f"On {phrase(rng, 2, 6)}",
            "summary": maybe(rng, 0.7, phrase(rng, 10, 24).capitalize() + "."),
            "url": maybe(rng, 0.6, f"https://example.org/{key}"),
        }
    if type_name == "Mention":
        return {
            "slug": key,
            "title": phrase(rng, 3, 7).capitalize(),
            "quote": maybe(rng, 0.8, f"“{phrase(rng, 8, 20)}”"),
            "context": maybe(rng, 0.5, phrase(rng, 4, 10)),
        }
    if type_name == "Person":
        name = f"{rng.choice(FIRST)} {rng.choice(LAST)}"
        role = rng.choice(ROLES)
        return {
            "slug": key,
            "name": name,
            "headline": role and f"{role}, {phrase(rng, 1, 2)}",
            "bio": maybe(rng, 0.5, f"{name} works on {phrase(rng, 3, 8)}."),
        }
    name = f"{rng.choice(LAST)} {rng.choice(ORG_SUFFIX)}"
    return {
        "slug": key,
        "name": name,
        "domain": maybe(rng, 0.7, f"{key}.example.com"),
        "description": maybe(rng, 0.6, phrase(rng, 6, 14).capitalize() + "."),
    }


class Picker:
    """Picks node indices of one type by a power law over a fixed shuffle of them."""

    def __init__(self, rng, count, exponent):
        order = list(range(count))
        rng.shuffle(order)
        self.order = order
        self.cumulative = list(itertools.accumulate((k + 1) ** -exponent for k in range(count)))

    def pick(self, rng):
        at = bisect.bisect_left(self.cumulative, rng.random() * self.cumulative[-1])
        return self.order[min(at, len(self.order) - 1)]


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--seed", type=int, default=30)
    parser.add_argument("--scale", type=float, default=1.0)
    args = parser.parse_args()
    rng = random.Random(args.seed)
    out = sys.stdout

    def emit(record):
        out.write(json.dumps(record, separators=(",", ":"), ensure_ascii=False) + "\n")

    counts = {t: max(10, round(n * args.scale)) for t, n in NODES.items()}
    for type_name, count in counts.items():
        for index in range(count):
            data = {k: v for k, v in node_data(rng, type_name, index).items() if v is not None}
            emit({"type": type_name, "data": data})

    # One popularity order per type, shared by every edge type that targets it, so a hub is a
    # hub across its relationships; sources get an order of their own, less steep.
    targets = {t: Picker(rng, n, TARGET_EXPONENT) for t, n in counts.items()}
    sources = {t: Picker(rng, n, SOURCE_EXPONENT) for t, n in counts.items()}

    for edge, src_type, dst_type, wanted in EDGES:
        if wanted is None:
            for src in range(counts[src_type]):
                if rng.random() < AT_MOST_ONE[edge]:
                    dst = targets[dst_type].pick(rng)
                    emit({"edge": edge, "from": slug(src_type, src), "to": slug(dst_type, dst)})
            continue
        wanted = round(wanted * args.scale)
        seen = set()
        tries = 0
        while len(seen) < wanted and tries < wanted * 20:
            tries += 1
            src = sources[src_type].pick(rng)
            dst = targets[dst_type].pick(rng)
            if (src_type == dst_type and src == dst) or (src, dst) in seen:
                continue
            seen.add((src, dst))
            emit({"edge": edge, "from": slug(src_type, src), "to": slug(dst_type, dst)})


if __name__ == "__main__":
    main()
