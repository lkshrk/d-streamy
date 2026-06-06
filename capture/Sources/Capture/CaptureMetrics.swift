import Foundation

public struct CaptureMetricsSnapshot {
    public let delivered: Int
    public let encSubmit: Int
    public let encDrop: Int
    public let audioBuf: Int
    public let audioConvFail: Int
}

/// Thread-safe capture-stage counters. Incremented on capture callback threads,
/// snapshotted on the app's metrics timer. snapshot() returns deltas and resets.
public final class CaptureMetrics {
    private let lock = NSLock()
    private var delivered = 0
    private var encSubmit = 0
    private var encDrop = 0
    private var audioBuf = 0
    private var audioConvFail = 0

    public init() {}

    public func recordDelivered() { bump { delivered += 1 } }
    public func recordEncodeSubmit() { bump { encSubmit += 1 } }
    public func recordEncodeDrop() { bump { encDrop += 1 } }
    public func recordAudioBuffer() { bump { audioBuf += 1 } }
    public func recordAudioConvFail() { bump { audioConvFail += 1 } }

    public func snapshot() -> CaptureMetricsSnapshot {
        lock.lock()
        defer { lock.unlock() }
        let snap = CaptureMetricsSnapshot(
            delivered: delivered,
            encSubmit: encSubmit,
            encDrop: encDrop,
            audioBuf: audioBuf,
            audioConvFail: audioConvFail
        )
        delivered = 0; encSubmit = 0; encDrop = 0; audioBuf = 0; audioConvFail = 0
        return snap
    }

    private func bump(_ body: () -> Void) {
        lock.lock(); body(); lock.unlock()
    }
}
