#!/usr/bin/env python3
"""Write third-party notices for a release's normal and build dependency graph."""

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


def metadata(manifest: Path, target: str, package: str, features: list[str]) -> dict:
    feature_args = ["--features", ",".join(f"{package}/{feature}" for feature in features)] if features else []
    command = ["cargo", "metadata", "--locked", "--format-version", "1",
               "--manifest-path", str(manifest), "--filter-platform", target, *feature_args]
    out = subprocess.run(
        command,
        check=True, capture_output=True, text=True, encoding="utf-8",
    ).stdout
    meta = json.loads(out)
    tree_command = ["cargo", "tree", "--locked", "--manifest-path", str(manifest),
                    "--target", target, "--package", package, "--edges", "normal,build",
                    "--prefix", "none", "--format", "{p}", *feature_args]
    tree = subprocess.run(tree_command, check=True, capture_output=True, text=True, encoding="utf-8").stdout
    selected = {tuple(match.groups()) for line in tree.splitlines()
                if (match := re.match(r"^(\S+) v(\S+)(?: |$)", line))}
    selected_ids = {pkg["id"] for pkg in meta["packages"] if (pkg["name"], pkg["version"]) in selected}
    if not selected_ids:
        raise ValueError(f"empty production dependency tree for {package!r} on {target!r}")
    for node in meta["resolve"]["nodes"]:
        node["deps"] = [dep for dep in node["deps"] if dep["pkg"] in selected_ids]
    return meta


def shipped(meta: dict, root_name: str) -> set[str]:
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    roots = [p["id"] for p in meta["packages"]
             if p["name"] == root_name and p["id"] in meta["workspace_members"]]
    if len(roots) != 1:
        raise ValueError(f"expected one workspace package named {root_name!r}")
    root = roots[0]
    seen, todo = set(), [root]
    while todo:
        pid = todo.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            if any(k["kind"] in (None, "build") for k in dep["dep_kinds"]):
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
            body = (TEMPLATES / f"{spdx}.txt").read_text(encoding="utf-8").strip()
            return f"{spdx}\n\nCopyright (c) {holders}\n\n{body}"
    return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--manifest", type=Path, default=Path("rust/Cargo.toml"))
    ap.add_argument("--package", default="dstreamy-cli")
    ap.add_argument("--target", action="append", required=True)
    ap.add_argument("--features", action="append", default=[])
    ap.add_argument("--append", type=Path, action="append", default=[])
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    if not args.manifest.is_file():
        ap.error(f"manifest does not exist: {args.manifest}")
    features = [feature for group in args.features for feature in group.replace(",", " ").split()]

    packages, ids = {}, set()
    try:
        for target in args.target:
            meta = metadata(args.manifest, target, args.package, features)
            packages.update({p["id"]: p for p in meta["packages"]})
            ours = {p["id"] for p in meta["packages"]
                    if p.get("source") is None and p.get("license") == PROJECT_LICENSE}
            ids |= shipped(meta, args.package) - ours
        additions = [path.read_text(encoding="utf-8").strip() for path in args.append]
        if any(not text for text in additions):
            raise ValueError("appended notice text must not be empty")
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"cannot generate notices: {error}", file=sys.stderr)
        return 1

    texts: dict[str, str] = {}
    users: dict[str, list[str]] = {}
    missing = []
    for pkg in sorted((packages[i] for i in ids), key=lambda p: (p["name"], p["version"])):
        bodies = [f.read_text(encoding="utf-8", errors="replace").strip()
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

    if missing:
        print("no license text for: " + ", ".join(missing), file=sys.stderr)
        return 1
    out = [
        "D-Streamy is licensed under GPL-3.0-or-later; see LICENSE.\n"
        "The release's normal and build dependency graph includes the third-party\n"
        "software listed below, each under its own license. Build dependencies\n"
        "are not necessarily embedded in the distributed binary.\n"
    ]
    names = {packages[pid]["name"] for pid in ids}
    if "unsafe-libopus" in names:
        out.append("libopus (BSD-3-Clause) is included as the unsafe-libopus translation.\n")
    if "openh264-sys2" in names:
        out.append("OpenH264 (BSD-2-Clause) is compiled from source by openh264-sys2.\n")
    if "discord-stream-rs" in names:
        out.append("discord-stream-rs is D-Streamy's fork of Tky567's MIT-licensed crate;\n"
                   "rust/vendor/discord-stream-rs/VENDOR.md lists the local changes.\n")
    for key in sorted(users, key=lambda k: users[k][0]):
        out.append("=" * 78 + "\n" + "\n".join(users[key]) + "\n" + "=" * 78 + "\n\n" + texts[key] + "\n")
    out.extend(additions)
    args.output.write_text("\n".join(out) + "\n", encoding="utf-8")
    crates = {label for labels in users.values() for label in labels}
    print(f"{args.output}: {len(crates)} crates, {len(texts)} license texts")
    return 0


if __name__ == "__main__":
    sys.exit(main())
