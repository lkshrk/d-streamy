# 09 — cli commands

Status: ready-for-agent
Depends: 08

## Scope

`dstreamy` clap CLI: `login` (store token), `guilds`, `channels <guild>`, `targets`, `stream --guild --channel --target <id|test-pattern> [--crop x,y,w,h] [--codec] [--fps] [--res] [--bitrate] [--gain]`, `--json` output for machine use. Hidden `--gateway-url` override used by tests to hit mock.

## Gates

- `assert_cmd` tests vs mock for every subcommand, incl. `stream --target test-pattern` running 3s and exiting cleanly on SIGINT
- Global gates from PRD

## Comments
