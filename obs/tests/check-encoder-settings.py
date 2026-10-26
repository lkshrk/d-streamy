#!/usr/bin/env python3
"""Compares an `obs-harness --dump-encoder-settings` directory with the committed fixtures."""
import json
import sys
from pathlib import Path

dump, fixtures = Path(sys.argv[1]), Path(sys.argv[2])
artifact = sys.argv[3] if len(sys.argv) > 3 else "the CI artifact"
failures = []
dumped = sorted(dump.glob("*.json"))
if not dumped:
    failures.append(f"{dump} holds no dumped encoder")
for path in dumped:
    got = json.loads(path.read_text())
    fixture = fixtures / path.name
    if not fixture.exists():
        failures.append(
            f"{got['id']}: no fixture. Copy {path.name} from {artifact} to {fixtures}/, "
            "set \"kind\": \"captured\" and \"source\" to the CI run, and commit it."
        )
        continue
    want = json.loads(fixture.read_text())
    for key, value in want["defaults"].items():
        if got["defaults"].get(key) != value:
            failures.append(
                f"{got['id']}: default {key} is {got['defaults'].get(key)!r} in libobs, {value!r} in {fixture}. "
                f"Fix the fixture from {artifact} and its cited source lines."
            )
    names = {p["name"] for p in got["properties"]}
    for prop in want["properties"]:
        if prop["name"] not in names:
            failures.append(f"{got['id']}: property {prop['name']} is in {fixture} but not in libobs's properties.")
for failure in failures:
    print(f"FAIL {failure}")
print(f"checked {len(dumped)} dumped H.264 encoder(s) against {fixtures}: {'FAIL' if failures else 'PASS'}")
sys.exit(1 if failures else 0)
