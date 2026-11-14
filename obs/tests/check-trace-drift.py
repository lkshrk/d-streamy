#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path

committed, fresh = map(Path, sys.argv[1:3])
old = {p.parent.name: p for p in committed.glob("*/trace.json")}
new = {p.parent.name: p for p in fresh.glob("*/trace.json")}
failures = sorted(set(old) - set(new))
for name in failures:
    print(f"FAIL missing fresh trace: {name}")
messages = []
for name in sorted(set(old) & set(new)):
    before, after = (json.loads(paths[name].read_text()) for paths in (old, new))
    keys = (set(before) | set(after)) - {"ci_run"}
    for key in sorted(keys):
        if before.get(key) != after.get(key):
            messages.append(f"{name}: {key}: committed={before.get(key)!r}, fresh={after.get(key)!r}")
for name in sorted(set(new) - set(old)):
    messages.append(f"{name}: fresh trace has no committed counterpart")
for message in messages:
    print(f"::warning::OBS trace drift: {message}")
summary = f"OBS trace drift: {len(messages)} changed metadata or packet-count field(s), {len(failures)} missing trace(s)."
print(summary)
if os.environ.get("GITHUB_STEP_SUMMARY"):
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as output:
        output.write(summary + "\n")
        for message in messages:
            output.write(f"- {message}\n")
sys.exit(1 if failures else 0)
