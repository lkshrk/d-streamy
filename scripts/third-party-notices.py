#!/usr/bin/env python3
"""Write the license notices of every third-party crate linked into a release binary."""

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

PROJECT_LICENSE = "GPL-3.0-or-later"
LICENSE_STEMS = ("license", "licence", "copying", "notice", "copyright")
TEMPLATES = Path(__file__).resolve().parent / "licenses"
TEMPLATE_ORDER = ("MIT", "Apache-2.0", "BSD-2-Clause", "MPL-2.0")


def metadata(manifest: Path, target: str) -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1",
         "--manifest-path", str(manifest), "--filter-platform", target],
        check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def shipped(meta: dict, root_name: str) -> set[str]:
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = next(p["id"] for p in meta["packages"]
                if p["name"] == root_name and p["id"] in meta["workspace_members"])
    seen, todo = set(), [root]
    while todo:
        pid = todo.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]):
                todo.append(dep["pkg"])
    return seen


def license_files(crate_dir: Path) -> list[Path]:
    def matches(p: Path) -> bool:
        return p.is_file() and p.name.lower().startswith(LICENSE_STEMS)

    top = sorted(p for p in crate_dir.iterdir() if matches(p))
    if top:
        return top
    return sorted(p for sub in crate_dir.iterdir() if sub.is_dir()
                  for p in sub.iterdir() if matches(p))


def template_text(pkg: dict) -> str | None:
    ids = set(re.findall(r"[A-Za-z0-9.+-]+", (pkg.get("license") or "").replace("/", " OR ")))
    for spdx in TEMPLATE_ORDER:
        if spdx in ids:
            holders = ", ".join(re.sub(r"\s*<[^>]*>", "", a) for a in pkg["authors"]) \
                or f'the {pkg["name"]} authors'
            body = (TEMPLATES / f"{spdx}.txt").read_text().strip()
            return f"{spdx}\n\nCopyright (c) {holders}\n\n{body}"
    return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--manifest", type=Path, default=Path("rust/Cargo.toml"))
    ap.add_argument("--package", default="dstreamy-cli")
    ap.add_argument("--target", action="append", required=True)
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()

    packages, ids = {}, set()
    for target in args.target:
        meta = metadata(args.manifest, target)
        packages.update({p["id"]: p for p in meta["packages"]})
        ours = {p["id"] for p in meta["packages"]
                if p["id"] in meta["workspace_members"] and p.get("license") == PROJECT_LICENSE}
        ids |= shipped(meta, args.package) - ours

    texts: dict[str, str] = {}
    users: dict[str, list[str]] = {}
    missing = []
    for pkg in sorted((packages[i] for i in ids), key=lambda p: (p["name"], p["version"])):
        bodies = [f.read_text(errors="replace").strip()
                  for f in license_files(Path(pkg["manifest_path"]).parent)]
        bodies = [b for b in bodies if b]
        if not bodies:
            fallback = template_text(pkg)
            if fallback is None:
                missing.append(f'{pkg["name"]} {pkg["version"]} ({pkg.get("license")})')
                continue
            bodies = [fallback]
        label = f'{pkg["name"]} {pkg["version"]} ({pkg.get("license") or "see below"})'
        for body in bodies:
            key = hashlib.sha256(re.sub(r"\s+", " ", body).encode()).hexdigest()
            texts.setdefault(key, body)
            users.setdefault(key, []).append(label)

    out = [
        "D-Streamy is licensed under GPL-3.0-or-later; see LICENSE.\n"
        "It includes the third-party software listed below, each under its own license.\n"
        "libopus (BSD-3-Clause) is included as the unsafe-libopus translation, and\n"
        "OpenH264 (BSD-2-Clause) is compiled from source by openh264-sys2.\n"
    ]
    for key in sorted(users, key=lambda k: users[k][0]):
        out.append("=" * 78 + "\n" + "\n".join(users[key]) + "\n" + "=" * 78 + "\n\n" + texts[key] + "\n")
    args.output.write_text("\n".join(out))
    if missing:
        print("no license text for: " + ", ".join(missing), file=sys.stderr)
        return 1
    crates = {label for labels in users.values() for label in labels}
    print(f"{args.output}: {len(crates)} crates, {len(texts)} license texts")
    return 0


if __name__ == "__main__":
    sys.exit(main())
