# D-Streamy documentation

Start with the overview, then the architecture. The component chapters go deep on one part each.

| Document | What it covers |
|---|---|
| [Overview](overview.md) | What D-Streamy is, its parts, platforms and repository layout |
| [Architecture](architecture.md) | System context, processes, crates and the main design decisions |
| [Workflows](workflows.md) | Logging in, streaming from the CLI, streaming from OBS, recovery and stopping |
| [Discord session](components/discord-session.md) | `dstreamy-discord`: gateway, voice, Go Live, DAVE, the media pump |
| [Media and capture](components/media-and-capture.md) | Sources: Linux portal and GStreamer, macOS ScreenCaptureKit, the test pattern, the external source |
| [Service and IPC](components/service-and-ipc.md) | `dstreamy serve`, `dstreamy service`, the control and producer protocol, transports |
| [Command line and core](components/cli-and-core.md) | Every `dstreamy` command, configuration, accounts, logging |
| [OBS plugin](components/obs-plugin.md) | The libobs module, the Qt panel, the producer half, Windows auto-start |
| [macOS app](components/macos-app.md) | The SwiftUI menu bar app over `dstreamy daemon` |
| [Build, test and release](components/build-test-release.md) | Local builds, test infrastructure, CI, the release workflow |

Reference material:

- [`CONTEXT.md`](../CONTEXT.md): the glossary. The docs, the code and the user-facing text use its terms.
- [`adr/`](adr/): the architecture decision records. They are binding; a change that contradicts one needs a new ADR or an amendment.
- [`specs.md`](specs.md): every external behaviour the service and the plugin rely on, each with its source. Nothing without a row there is relied on or mocked.
