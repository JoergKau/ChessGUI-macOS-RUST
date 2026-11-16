#!/usr/bin/env python3
"""Licence guard rail for SiliconBoard.

Two jobs:

  tools/license-notices.py            check the locked dependency graph
  tools/license-notices.py --write    regenerate THIRD-PARTY-NOTICES.md

The check fails when a crate in `Cargo.lock` is under a copyleft licence
(GPL, AGPL, LGPL, MPL, EPL, CDDL, SSPL, EUPL, CC-BY-SA) and is not one of
the crates already accounted for in `EXPECTED`. Dual licences are fine when
a permissive alternative is offered ("MIT OR GPL-2.0-only"), because that
alternative can be chosen.

`shakmaty` is expected: it is GPL-3.0-or-later and makes the application as
a whole GPL-3.0-or-later. Adding another copyleft crate changes the
licensing story, so it must be a deliberate edit to this file.

Licences are read from the local cargo registry first (offline, exact for
the crates actually built), then from crates.io for platform crates that
are not downloaded on this machine.
"""

from __future__ import annotations

import collections
import glob
import json
import os
import re
import sys
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LOCK = os.path.join(ROOT, "Cargo.lock")
OUT = os.path.join(ROOT, "THIRD-PARTY-NOTICES.md")

# Crates that are already known to be copyleft, with why.
EXPECTED = {
    "shakmaty": "chess rules; GPL-3.0-or-later makes the application GPL",
    "chess-scid-rw": "Scid .si4/.si5 import; GPL-3.0-or-later",
}

COPYLEFT = re.compile(r"\b(?:A?GPL|LGPL|MPL|EPL|CDDL|SSPL|EUPL|CC-BY-SA)", re.I)
PERMISSIVE = re.compile(
    r"\b(?:MIT|Apache|BSD|ISC|Zlib|0BSD|Unlicense|Unicode|OFL|Ubuntu-font|BSL)\b",
    re.I,
)


def locked_packages() -> list[tuple[str, str]]:
    text = open(LOCK, encoding="utf-8").read()
    found = re.findall(
        r'\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"', text
    )
    return [(n, v) for n, v in found if n != "siliconboard"]


def registry_licences() -> dict[str, str]:
    table = {}
    for src in glob.glob(os.path.expanduser("~/.cargo/registry/src/*")):
        for manifest in glob.glob(os.path.join(src, "*", "Cargo.toml")):
            key = os.path.basename(os.path.dirname(manifest))
            try:
                text = open(manifest, encoding="utf-8", errors="replace").read()
            except OSError:
                continue
            start = text.find("[package]")
            section = text[start:] if start >= 0 else text
            section = section.split("\n[", 1)[0]
            for field in ("license", "license-file"):
                match = re.search(rf"^{field}\s*=\s*(.+)$", section, re.M)
                if match:
                    table.setdefault(key, match.group(1).strip().strip('"'))
                    break
    return table


def index_licences() -> dict[str, str]:
    """Licences from cargo's sparse-index cache (works offline, all versions).

    Each cache file holds NUL-separated `version` / JSON pairs; the JSON has a
    `lic` field. This covers the platform crates that are resolved but never
    downloaded on this machine (windows, android, ios).
    """
    table: dict[str, str] = {}
    pattern = os.path.expanduser("~/.cargo/registry/index/*/.cache")
    for root in glob.glob(pattern):
        for dirpath, _dirs, files in os.walk(root):
            for name in files:
                try:
                    data = open(os.path.join(dirpath, name), "rb").read()
                except OSError:
                    continue
                for chunk in data.split(b"\x00"):
                    if not chunk.startswith(b"{"):
                        continue
                    try:
                        entry = json.loads(chunk)
                    except ValueError:
                        continue
                    licence = entry.get("lic")
                    if licence:
                        key = f"{entry.get('name')}-{entry.get('vers')}"
                        table.setdefault(key, licence)
    return table


def crates_io_licence(name: str, version: str) -> str | None:
    url = f"https://crates.io/api/v1/crates/{name}/{version}"
    request = urllib.request.Request(url, headers={"User-Agent": "siliconboard"})
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            payload = json.load(response)
        return (payload.get("version") or {}).get("license")
    except Exception:
        return None


def is_copyleft(expression: str) -> bool:
    """True when every `OR` alternative inside every `AND` part is copyleft."""
    for part in re.split(r"\bAND\b", expression, flags=re.I):
        alternatives = re.split(r"\s+OR\s+|/", part)
        usable = [a for a in alternatives if a.strip()]
        if not usable:
            continue
        if not any(COPYLEFT.search(a) for a in usable):
            continue  # this part has a permissive alternative
        if not any(PERMISSIVE.search(a) for a in usable):
            return True
    return False


def existing_notices() -> dict[str, str]:
    """Licences already written to THIRD-PARTY-NOTICES.md, as a cache.

    Keeps regeneration offline and deterministic: only crates that are new
    since the file was last written need the crates.io lookup.
    """
    if not os.path.exists(OUT):
        return {}
    text = open(OUT, encoding="utf-8").read()
    rows = re.findall(r"^\| `([^`]+)` \| ([^|]+) \| ([^|]+) \|$", text, re.M)
    return {
        f"{name}-{version.strip()}": licence.strip()
        for name, version, licence in rows
        if licence.strip() and licence.strip() != "see crates.io"
    }


def collect() -> list[tuple[str, str, str]]:
    local = registry_licences()
    index = index_licences()
    cached = existing_notices()
    rows = []
    for name, version in locked_packages():
        key = f"{name}-{version}"
        licence = local.get(key) or index.get(key) or cached.get(key)
        if not licence:
            licence = crates_io_licence(name, version)
        if not licence:
            licence = "see crates.io"
        rows.append((name, version, licence))
    rows.sort(key=lambda row: (row[0].lower(), row[1]))
    return rows


def check(rows: list[tuple[str, str, str]]) -> int:
    problems = [
        (n, v, l)
        for n, v, l in rows
        if is_copyleft(l) and n not in EXPECTED
    ]
    if problems:
        print("Unexpected copyleft dependencies:", file=sys.stderr)
        for name, version, licence in problems:
            print(f"  {name} {version} -> {licence}", file=sys.stderr)
        print(
            "\nIf this is deliberate, add the crate to EXPECTED in\n"
            "tools/license-notices.py, update THIRD-PARTY-NOTICES.md and the\n"
            "licence section of README.md, then run --write.",
            file=sys.stderr,
        )
        return 1
    print(
        f"{len(rows)} crates checked; the only copyleft crate is "
        + ", ".join(sorted(EXPECTED))
        + "."
    )
    return 0


def write(rows: list[tuple[str, str, str]]) -> int:
    counts = collections.Counter(licence for _, _, licence in rows)
    shakmaty = next((r for r in rows if r[0] == "shakmaty"), None)
    version = shakmaty[1] if shakmaty else "?"

    out = [
        "# Third-party notices",
        "",
        "SiliconBoard is built from the Rust crates listed below, pinned in",
        "`Cargo.lock`. This file is generated from that lock file by",
        "`tools/license-notices.py --write`.",
        "",
        "## Licence of the application as a whole",
        "",
        "SiliconBoard's own source is MIT (see `LICENSE-MIT`). The application also",
        "links `shakmaty`, which is licensed under **GPL-3.0-or-later**, so the",
        "distributed application as a whole is released under",
        "**GPL-3.0-or-later** (see `LICENSE`). The corresponding source for every",
        "release is this repository at the release tag.",
        "",
        "## The GPL component",
        "",
        "| Crate | Version | Licence | Copyright |",
        "| --- | --- | --- | --- |",
        f"| `shakmaty` | {version} | GPL-3.0-or-later | Niklas Fiekas and other shakmaty contributors |",
        "| `chess-scid-rw` | (see the table below) | GPL-3.0-or-later | Fab2bprog and other contributors |",
        "",
        "`shakmaty` provides move generation, SAN, FEN, Zobrist keys and",
        "position equality. Its source is at <https://github.com/niklasf/shakmaty>",
        "and on crates.io.",
        "",
        "`chess-scid-rw` reads and writes Scid 4/5 databases for the optional",
        "database import. Its source is at",
        "<https://github.com/Fab2bprog/chess-scid-rw> and on crates.io. It is a",
        "Cargo feature (`scid`, on by default); a build with `--no-default-features`",
        "omits it.",
        "",
        "## Bundled assets",
        "",
        "Board themes, piece sets, the figurine font and the ECO table have their",
        "own terms; see `public/LICENSING.md`.",
        "",
        "## All crates",
        "",
        "| Crate | Version | Licence |",
        "| --- | --- | --- |",
    ]
    out += [f"| `{n}` | {v} | {l} |" for n, v, l in rows]
    out += ["", "### Licence summary", ""]
    out += [f"- {c} × {l}" for l, c in counts.most_common()]

    with open(OUT, "w", encoding="utf-8") as handle:
        handle.write("\n".join(out) + "\n")
    print(f"Wrote {OUT} ({len(rows)} crates).")
    return 0


def main() -> int:
    rows = collect()
    if "--write" in sys.argv[1:]:
        return write(rows)
    return check(rows)


if __name__ == "__main__":
    sys.exit(main())
