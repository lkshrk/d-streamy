# 10 — tui

Status: ready-for-agent
Depends: 09

## Scope

`dstreamy tui` with ratatui: guild/channel picker, target picker, settings form, live health panel (scores + sparklines + FPS/jitter/drops/audio cadence, green/yellow/red), start/stop. Pure view-model layer separate from rendering.

## Gates

- View-model unit tests
- Snapshot tests via ratatui `TestBackend` + `insta` for each screen
- Scripted key-event test driving full flow against mock with test-pattern
- Global gates from PRD

## Comments
