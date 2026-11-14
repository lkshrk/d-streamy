# 0007 — OBS: a status-bar panel and a settings dialog instead of a dock

Status: accepted (2026-11-06; panel decided 2026-11-05, status-bar design 2026-11-06). Amends 0004 (OBS plugin UI).

## Context

ADR 0004 put the plugin's UI into a dock. OBS adds docks hidden and floating, so a user has to find and place one before Start is reachable, and a dock takes screen space for a control the user presses twice per stream. The plugin also has to take over an audio track on OBS's sources to send Discord a mix without the microphone, which must not happen without consent.

## Decision

- **A status-bar panel.** The plugin adds one `QWidget` to OBS's status bar (`statusBar()->addPermanentWidget`, `obs/ui/dstreamy-ui.cpp`). It holds Start and Stop, the session state, and the health counters. Start and Stop ask a confirm first (`Stop the Discord stream?`).
- **A popup and a fallback dialog.** Clicking the status-bar item, or Tools > D-Streamy…, opens the panel's popup: the account row (ADR 0005), the server and voice channel pickers, the managed track and Start or Stop. When the Qt UI library is missing or refuses to load, Tools > D-Streamy… opens OBS's stock properties dialog for the private source `dstreamy_settings` with the same model (`rust/crates/dstreamy-obs/src/frontend.rs`, `settings_source.rs`). Neither view shows the encoder settings (`mode`, `video_encoder`, `bitrate_kbps`, `share_fallback`); they are fields of `config.json`. Both views are built from the same Rust model (`rust/crates/dstreamy-obs-producer/src/panel.rs`), and the C++ side renders a JSON view; the host table they share is versioned (`DSTREAMY_UI_ABI_VERSION`), and a library and plugin of different versions refuse each other.
- **Status-bar design** (`status_bar.rs`): no text prefix; a coloured state dot; four counters that are always visible, each with a monochrome SVG icon in the theme's text colour: missed frames, audio state, reconnects and viewers; the stream's duration last. Bitrate and fps live in the tooltip and in the popup's detail line while live. A counter in trouble tints its icon amber or red; the tooltip says the same in words.
- **Audio track** (`rust/crates/dstreamy-obs/src/audio.rs`): the Discord track (Track 6) is managed per scene collection, and a collection is never taken over without the confirm. Go Live is blocked, with a hint, while the Discord track is unmanaged and a microphone source feeds it, checked at Start and again just before the output starts. A microphone that reaches the live track mid-stream (a mixer change, a new microphone source, or a switch to a scene collection whose Discord track is unmanaged) doesn't stop the stream: the plugin warns prominently, with ⚠ in the status bar, the headline and the fix button, and keeps streaming (decided 2026-11-05). Settings saved on Track 1 by an earlier build migrate silently to Track 6. A managed track that is not the Discord track offers "Stop managing Track N", which changes no mixer and says how to turn the track back on in Advanced Audio Properties. The dialog asks the same confirm as the panel, and its Cancel puts the mixer flags it touched back.
- **Windows:** the same panel and dialog as on Linux and macOS. The plugin starts the `dstreamy.exe` it ships beside its DLL when no serve runs, and shuts it down on unload unless another client's session is live on it (ADR 0004, Supervision).

## Consequences

- The controls are always visible, and the rarely used settings do not take dock space.
- The status bar is one row, so every counter is a number or an icon; the words are in the tooltip, which screen readers get.
- The Rust model types were renamed from `Dock*` to panel terms on 2026-11-14 (`PanelModel`, `PanelSettings`, `PanelState`, `StateView`).

## Rejected

- A dock (`obs_frontend_add_dock_by_id`): hidden and floating by default.
- Text labels or emojis in the status bar, and bitrate or fps there: the user judges a stream by drops, audio, reconnects and viewers, as the macOS app's health view did.
- A global "managed" flag: the mixer flags are per scene collection, so a global flag let the microphone reach Discord in a collection the user never took over.
