#!/usr/bin/env python3
"""Builds crates/app-core/src/campus_map/walkways.txt, the walkways the map's
routes follow outdoors, from OpenStreetMap.

    python3 scripts/campus-map/walkways.py extract.json crates/app-core/src/campus_map/walkways.txt

`extract.json` is {"pts": [[lng, lat], ...], "ways": [{"t": tags, "n": [index
into pts, ...]}, ...]}: every `highway` way of
https://api.openstreetmap.org/api/0.6/map.json for the boxes
139.4797,35.5106,139.4876,35.5183 (Suzukakedai) and
139.6762,35.5986,139.6870,35.6106 (Ookayama), with its nodes. The data is
(c) OpenStreetMap contributors, ODbL; the map page already credits it.

Kept: every way a person may walk (no motorways or trunk roads, nothing
private or closed to walkers). A way that ends within STITCH metres of another
is joined to it, since such ends are drawn short of the way they meet. Each
campus keeps its largest connected part.
"""
import collections
import json
import math
import sys

DROP = {"motorway", "motorway_link", "trunk", "trunk_link", "primary", "construction", "proposed"}
PATHS = {"footway", "path", "pedestrian", "track"}
STITCH = 3.0


def metres(a, b):
    kx = math.cos(math.radians((a[1] + b[1]) / 2)) * 111320.0
    return math.hypot((a[0] - b[0]) * kx, (a[1] - b[1]) * 110574.0)


def main(source, target):
    raw = json.load(open(source))
    pts = [tuple(p) for p in raw["pts"]]
    adj = collections.defaultdict(set)
    kind = {}

    def link(a, b, k):
        adj[a].add(b)
        adj[b].add(a)
        kind[(min(a, b), max(a, b))] = k

    for way in raw["ways"]:
        tags = way["t"]
        if tags["highway"] in DROP or tags.get("access") in ("no", "private") or tags.get("foot") == "no":
            continue
        k = "s" if tags["highway"] == "steps" else "p" if tags["highway"] in PATHS else "r"
        for a, b in zip(way["n"], way["n"][1:]):
            if a != b:
                link(a, b, k)
    stitched = 0
    for n in [n for n in list(adj) if len(adj[n]) == 1]:
        q = pts[n]
        kx = math.cos(math.radians(q[1])) * 111320.0
        best = None
        for (a, b) in list(kind):
            if n in (a, b) or abs(pts[a][0] - q[0]) > 0.001 or abs(pts[a][1] - q[1]) > 0.001:
                continue
            ax, ay = (pts[a][0] - q[0]) * kx, (pts[a][1] - q[1]) * 110574.0
            bx, by = (pts[b][0] - q[0]) * kx, (pts[b][1] - q[1]) * 110574.0
            dx, dy = bx - ax, by - ay
            length = dx * dx + dy * dy
            t = 0 if length == 0 else max(0, min(1, -(ax * dx + ay * dy) / length))
            off = math.hypot(ax + t * dx, ay + t * dy)
            if best is None or off < best[0]:
                best = (off, a, b, t)
        if best and best[0] <= STITCH:
            _, a, b, t = best
            foot = len(pts)
            pts.append((pts[a][0] + (pts[b][0] - pts[a][0]) * t, pts[a][1] + (pts[b][1] - pts[a][1]) * t))
            k = kind.pop((min(a, b), max(a, b)))
            adj[a].discard(b)
            adj[b].discard(a)
            link(foot, a, k)
            link(foot, b, k)
            link(foot, n, "p")
            stitched += 1
    seen, parts = set(), []
    for start in list(adj):
        if start in seen:
            continue
        part, stack = {start}, [start]
        while stack:
            for m in adj[stack.pop()]:
                if m not in part:
                    part.add(m)
                    stack.append(m)
        seen |= part
        parts.append(part)
    keep = set()
    for west in (True, False):
        keep |= max((p for p in parts if (pts[next(iter(p))][0] < 139.6) == west), key=len)
    order = sorted(keep)
    index = {n: i for i, n in enumerate(order)}
    out = [
        "# The walkways of the Ookayama and Suzukakedai campuses, from OpenStreetMap",
        "# (c) OpenStreetMap contributors, ODbL. Generated; do not edit by hand.",
        "# n <lng> <lat> in 1e-7 degrees; e <node> <node> <p path | r road | s steps>.",
    ]
    out += ["n %d %d" % (round(pts[n][0] * 1e7), round(pts[n][1] * 1e7)) for n in order]
    out += ["e %d %d %s" % (index[a], index[b], k) for (a, b), k in sorted(kind.items()) if a in keep and b in keep]
    open(target, "w").write("\n".join(out) + "\n")
    print("stitched", stitched, "nodes", len(order), "edges", len(out) - 3 - len(order))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
