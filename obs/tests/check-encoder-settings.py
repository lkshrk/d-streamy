#!/usr/bin/env python3
"""Compares an `obs-harness --dump-encoder-settings` directory with the committed fixtures.

usage: check-encoder-settings.py DUMP FIXTURES ARTIFACT REQUIRED_ID...
"""
import json
import sys
from pathlib import Path

MISSING = object()

dump, fixtures, artifact = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
required = set(sys.argv[4:])
failures = []
dumped = sorted(dump.glob("*.json"))
seen = set()
for path in dumped:
    got = json.loads(path.read_text())
    seen.add(got["id"])
    fixture = fixtures / path.name
    if not fixture.exists():
        advice = (
            f"copy {path.name} from {artifact} to {fixtures}/, set \"kind\": \"captured\" and \"source\" "
            "to the CI run, and commit it"
        )
        if "com.apple.videotoolbox.videoencoder." in got["id"]:
            print(f"::warning::{got['id']}: VideoToolbox encoder with no fixture; {advice}")
        else:
            failures.append(f"{got['id']}: no fixture; {advice}")
        continue
    want = json.loads(fixture.read_text())
    for key, value in want["defaults"].items():
        actual = got["defaults"].get(key, MISSING)
        # Python's True == 1, but a bool default and an int default are different settings.
        if actual is MISSING or actual != value or isinstance(actual, bool) != isinstance(value, bool):
            shown = "missing" if actual is MISSING else repr(actual)
            failures.append(
                f"{got['id']}: default {key} is {shown} in libobs, {value!r} in {fixture}. "
                f"Fix the fixture from {artifact} and its cited source lines."
            )
    names = {p["name"] for p in got["properties"]}
    for prop in want["properties"]:
        if prop["name"] not in names:
            failures.append(f"{got['id']}: property {prop['name']} is in {fixture} but not in libobs's properties.")
for missing in sorted(required - seen):
    failures.append(f"{missing} was not dumped: its module did not load, so nothing was compared")
for failure in failures:
    print(f"FAIL {failure}")
print(f"checked {len(dumped)} dumped H.264 encoder(s) against {fixtures}: {'FAIL' if failures else 'PASS'}")
sys.exit(1 if failures else 0)
