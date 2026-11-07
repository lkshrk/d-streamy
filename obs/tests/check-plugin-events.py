#!/usr/bin/env python3
import json
import re
import sys
from pathlib import Path

path, event, *conditions = sys.argv[1:]
events = []
for line in Path(path).read_text().splitlines():
    if line.startswith('{"'):
        events.append(json.loads(line))

def matches(row):
    if row.get("event") != event:
        return False
    for condition in conditions:
        if "~=" in condition:
            key, pattern = condition.split("~=", 1)
            if re.search(pattern, str(row.get(key, ""))) is None:
                return False
        elif ">" in condition:
            key, minimum = condition.split(">", 1)
            if not isinstance(row.get(key), (int, float)) or row[key] <= float(minimum):
                return False
        else:
            key, value = condition.split("=", 1)
            try:
                value = json.loads(value)
            except json.JSONDecodeError:
                pass
            if row.get(key) != value:
                return False
    return True

if not any(matches(row) for row in events):
    raise SystemExit(f"FAIL {path}: no {event} event matches {conditions}")
