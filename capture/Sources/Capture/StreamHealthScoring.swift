import Foundation

public enum StreamHealthLevel: Sendable {
    case good, warn, bad

    public init(score: Double) {
        if score >= 0.9 { self = .good }
        else if score >= 0.6 { self = .warn }
        else { self = .bad }
    }
}

public struct StreamHealthSample: Sendable {
    public let fps: Int
    public let gapP95Ms: Int
    public let vDrop: Int
    public let aSent: Int
    public let aDropNotReady: Int
    public let aEncErr: Int

    public init(fps: Int, gapP95Ms: Int, vDrop: Int, aSent: Int, aDropNotReady: Int, aEncErr: Int) {
        self.fps = fps
        self.gapP95Ms = gapP95Ms
        self.vDrop = vDrop
        self.aSent = aSent
        self.aDropNotReady = aDropNotReady
        self.aEncErr = aEncErr
    }
}

public enum StreamHealthScoring {
    public static let audioFramesPerSec = 50.0

    public static func video(_ s: StreamHealthSample, targetFps: Int) -> Double {
        let target = Double(max(targetFps, 1))
        let frameInterval = 1000.0 / target
        // WebRTC freeze detection: gap >= max(3*frame_dur, frame_dur+150ms)
        let freezeMs = max(3 * frameInterval, frameInterval + 150)

        let fps = Double(s.fps)
        let jitter = Double(s.gapP95Ms)
        let dropPct = Double(s.vDrop) / target * 100

        let goodFps = fps >= 0.95 * target
        let warnFps = fps >= 0.85 * target
        let goodJit = jitter <= 2 * frameInterval
        let warnJit = jitter < freezeMs

        if goodFps && goodJit && dropPct <= 2 { return 1.0 }
        if warnFps && warnJit && dropPct <= 5 { return 0.5 }
        return 0.0
    }

    public static func audio(_ s: StreamHealthSample) -> Double {
        let sentPct = Double(s.aSent) / audioFramesPerSec * 100
        if s.aEncErr == 0 && s.aDropNotReady <= 1 && sentPct >= 96 { return 1.0 }
        if s.aEncErr == 0 && s.aDropNotReady <= 3 && sentPct >= 90 { return 0.5 }
        return 0.0
    }
}
