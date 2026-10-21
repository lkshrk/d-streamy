# 14 — manual checklist and retire

Status: ready-for-human
Depends: 13, 16

## Scope

Finalise `MANUAL-CHECKLIST.md` with exact commands for each human-only check. After human signs off: remove the Bun daemon (`src/`, `package.json`, `bun.lock`, `tsconfig.json`, `stubs/`, `bin/` leftovers), the Swift `capture/` library and its tests, and old CI steps; update vault project page. Keep the SwiftUI app (`App/`, `AppTests/`, `Package.swift`, Casks entry) — per ADR 0003 it is the macOS frontend over `dstreamy daemon`.

## Gates

- Checklist complete and reproducible
- Removal part is ready-for-human: blocked until checklist signed off
- Global gates from PRD

## Comments
