# Contributing

Contributions are welcome: bug reports, fixes and improvements to capture and
streaming.

Please read the [Disclaimer](README.md#disclaimer) first. Never include your
Discord token in issues, logs or commits.

## Reporting issues

Open an issue with your macOS version, the window or app you tried to stream,
and the relevant log output with tokens and IDs removed.

## Development

Requires macOS 14+, Xcode and [Bun](https://bun.sh). See
[Architecture](README.md#architecture) for how the Swift app and the Bun daemon
fit together.

```sh
bun install
bun run build   # outputs bin/D-Streamy.app
```

Before opening a PR, run the same checks as CI:

```sh
bun test
bunx tsc --noEmit
swift test
```

## Pull requests

- Fork, branch from `main`, and keep each PR focused on one change.
- Add tests for new behaviour.
- Use [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat:`, `fix:`, `docs:`, `chore:`) with a scope where it helps, e.g.
  `fix(daemon): ...`.
