# 0007 — OBS: a status-bar panel and a settings dialog instead of a dock

Status: accepted (2026-11-06; panel decided 2026-11-05, status-bar design 2026-11-06). Amends 0004 (OBS plugin UI).

## Context

ADR 0004 put the plugin's UI into a dock. OBS adds docks hidden and floating, so a user has to find and place one before Start is reachable, and a dock takes screen space for a control the user presses twice per stream. The plugin also has to take over an audio track on OBS's sources to send Discord a mix without the microphone, which must not happen without consent.

## Decision

- **A status-bar panel.** The plugin adds one `QWidget` to OBS's status bar (`statusBar()->addPermanentWidget`, `obs/ui/dstreamy-ui.cpp`). It holds Start and Stop, the session state, and the health counters. Start and Stop ask a confirm first (`Stop the Discord stream?`).
- **A settings dialog** from Tools > D-Streamy… (`rust/crates/dstreamy-obs/src/frontend.rs`) with the server and voice channel picker, the account row (ADR 0005), the video encoder mode and the audio track settings. Both views are built from the same Rust model (`rust/crates/dstreamy-obs-producer/src/panel.rs`), and the C++ side renders a JSON view; the host table they share is versioned (`DSTREAMY_UI_ABI_VERSION`), and a library and plugin of different versions refuse each other.
- **Status-bar design** (`status_bar.rs`): no text prefix; a coloured state dot; four counters that are always visible, each with a monochrome SVG icon in the theme's text colour: missed frames, audio state, reconnects and viewers; the stream's duration last. Bitrate and fps live in the tooltip and the dialog. A counter in trouble tints its icon amber or red; the tooltip says the same in words.
- **Audio track** (`rust/crates/dstreamy-obs/src/audio.rs`): the Discord track (Track 6) is managed per scene collection, and a collection is never taken over without the confirm. Go Live is blocked, with a hint, while the Discord track is unmanaged and a microphone source feeds it. Settings saved on Track 1 by an earlier build migrate silently to Track 6. A managed track that is not the Discord track offers "Stop managing Track N", which changes no mixer and says how to turn the track back on in Advanced Audio Properties. The dialog asks the same confirm as the panel, and its Cancel puts the mixer flags it touched back.
- **Windows:** the panel shows that D-Streamy for OBS is not supported there yet, with Start and Stop disabled (ADR 0004, Deferred).

## Consequences

- The controls are always visible, and the rarely used settings do not take dock space.
- The status bar is one row, so every counter is a number or an icon; the words are in the tooltip, which screen readers get.
- The Rust model keeps `Dock` in some type names until renamed.

## Rejected

- A dock (`obs_frontend_add_dock_by_id`): hidden and floating by default.
- Text labels or emojis in the status bar, and bitrate or fps there: the user judges a stream by drops, audio, reconnects and viewers, as the macOS app's health view did.
- A global "managed" flag: the mixer flags are per scene collection, so a global flag let the microphone reach Discord in a collection the user never took over.
