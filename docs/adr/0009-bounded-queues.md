# 0009 — Bounded queues between OBS and Discord

Status: accepted (2026-11-06). Details 0004 ("a bounded ring").

## Context

ADR 0004 requires that no OBS thread ever blocks on D-Streamy and that a slow consumer drops media rather than stalling the user's main stream. It names a bounded ring without sizes. The sizes decide how much latency a hiccup adds and how much queued payload a stalled service can pin, not total process resident memory.

## Decision

- **Plugin packet ring** (`rust/crates/dstreamy-obs-producer/src/ring.rs`): 256 packets or 32 MiB, whichever fills first. The packet budget is roughly 2 s at 60 fps plus 20 ms audio, not an invisible-pause guarantee. The byte budget counts queued payload only. An oversized payload (above 8 MiB minus the 10-byte header) is represented by a size-only marker instead of being retained in the ring; the writer reports `oversized` and ends the run.
- **Drop policy when the ring is full:** evict the oldest video group when a newer queued keyframe can preserve a decodable suffix. Otherwise reclaim oldest audio first; if the only video group must be lost, discard its deltas and await a new keyframe. Audio never evicts the only decodable video group. A new keyframe can displace old video or audio. Drops are counted per track (`RingStats`) and shown in the panel.
- **Service source queue** (`rust/crates/dstreamy-media/src/external.rs`): the held first frames of a new attachment are released before admitting a frame would exceed the queue capacity or 64 MiB (`QUEUE_MAX_BYTES`). These limits apply to queued payload, not all allocations or in-flight frames; frames lost to a full queue are counted in `source.stats` (ADR 0004 `status`).
- **Service event buffer** (`rust/crates/dstreamy-cli/src/serve.rs`, `DEFAULT_EVENT_CAPACITY`): 256 events per subscriber. A subscriber that falls behind is disconnected with `lagged`, as ADR 0004 says, instead of slowing the session.

## Consequences

- A lossless pause can still be visible, but does not itself require discarding decodable deltas. Actual loss waits for a real keyframe; after source video loss, failure to receive one within 2 s detaches the producer with an actionable error (ADR 0004), rather than forwarding undecodable deltas.
- The ring retains at most 32 MiB of queued payload per attachment. Encoder buffers, packet metadata, producer copies and in-flight writes are outside that budget, so it is not a process-memory bound.
- The numbers are policy; `ring.rs` keeps them as constants so a measurement can change them in one place.

## Rejected

- An unbounded queue: it hides a stalled service until memory runs out, inside OBS.
- Dropping video frames one by one: a decoder cannot use P-frames after a gap, so everything up to the next keyframe is waste anyway.
