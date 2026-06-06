import XCTest
@testable import CaptureLib

final class StreamHealthScoringTests: XCTestCase {
    private func sample(
        fps: Int = 30, gapP95Ms: Int = 33, vDrop: Int = 0,
        aSent: Int = 50, aDropNotReady: Int = 0, aEncErr: Int = 0
    ) -> StreamHealthSample {
        StreamHealthSample(
            fps: fps, gapP95Ms: gapP95Ms, vDrop: vDrop,
            aSent: aSent, aDropNotReady: aDropNotReady, aEncErr: aEncErr
        )
    }

    // MARK: Video

    func testVideoHealthy() {
        XCTAssertEqual(StreamHealthScoring.video(sample(), targetFps: 30), 1.0)
    }

    func testVideoFpsBoundaries() {
        // 95% of 30 = 28.5 → good needs >=29; 85% = 25.5 → warn needs >=26
        XCTAssertEqual(StreamHealthScoring.video(sample(fps: 29), targetFps: 30), 1.0)
        XCTAssertEqual(StreamHealthScoring.video(sample(fps: 28), targetFps: 30), 0.5)
        XCTAssertEqual(StreamHealthScoring.video(sample(fps: 26), targetFps: 30), 0.5)
        XCTAssertEqual(StreamHealthScoring.video(sample(fps: 25), targetFps: 30), 0.0)
    }

    func testVideoJitterFreezeThreshold() {
        // 30fps: interval=33.3, good <=66.6ms; freeze = max(100, 183.3) = 183.3
        XCTAssertEqual(StreamHealthScoring.video(sample(gapP95Ms: 66), targetFps: 30), 1.0)
        XCTAssertEqual(StreamHealthScoring.video(sample(gapP95Ms: 120), targetFps: 30), 0.5)
        XCTAssertEqual(StreamHealthScoring.video(sample(gapP95Ms: 183), targetFps: 30), 0.5)
        XCTAssertEqual(StreamHealthScoring.video(sample(gapP95Ms: 184), targetFps: 30), 0.0)
    }

    func testVideoDropPercent() {
        // 30fps: 1 drop = 3.3% (>2 → not good, <=5 → warn); 2 drops = 6.7% (>5 → bad)
        XCTAssertEqual(StreamHealthScoring.video(sample(vDrop: 0), targetFps: 30), 1.0)
        XCTAssertEqual(StreamHealthScoring.video(sample(vDrop: 1), targetFps: 30), 0.5)
        XCTAssertEqual(StreamHealthScoring.video(sample(vDrop: 2), targetFps: 30), 0.0)
    }

    func testVideoHighFpsTargetScalesThresholds() {
        // 60fps: interval=16.7, good jitter <=33.3, freeze=max(50,166.7)=166.7
        XCTAssertEqual(StreamHealthScoring.video(sample(fps: 60, gapP95Ms: 33), targetFps: 60), 1.0)
        XCTAssertEqual(StreamHealthScoring.video(sample(fps: 60, gapP95Ms: 50), targetFps: 60), 0.5)
    }

    // MARK: Audio

    func testAudioHealthy() {
        XCTAssertEqual(StreamHealthScoring.audio(sample()), 1.0)
    }

    func testAudioSentBoundaries() {
        // 50/s baseline: 96% = 48 (good), 90% = 45 (warn)
        XCTAssertEqual(StreamHealthScoring.audio(sample(aSent: 48)), 1.0)
        XCTAssertEqual(StreamHealthScoring.audio(sample(aSent: 47)), 0.5)
        XCTAssertEqual(StreamHealthScoring.audio(sample(aSent: 45)), 0.5)
        XCTAssertEqual(StreamHealthScoring.audio(sample(aSent: 44)), 0.0)
    }

    func testAudioEncodeErrorForcesBad() {
        XCTAssertEqual(StreamHealthScoring.audio(sample(aEncErr: 1)), 0.0)
    }

    func testAudioDropNotReadyBoundaries() {
        XCTAssertEqual(StreamHealthScoring.audio(sample(aDropNotReady: 1)), 1.0)
        XCTAssertEqual(StreamHealthScoring.audio(sample(aDropNotReady: 2)), 0.5)
        XCTAssertEqual(StreamHealthScoring.audio(sample(aDropNotReady: 4)), 0.0)
    }

    // MARK: Level mapping

    func testLevelThresholds() {
        XCTAssertEqual(StreamHealthLevel(score: 1.0), .good)
        XCTAssertEqual(StreamHealthLevel(score: 0.9), .good)
        XCTAssertEqual(StreamHealthLevel(score: 0.89), .warn)
        XCTAssertEqual(StreamHealthLevel(score: 0.6), .warn)
        XCTAssertEqual(StreamHealthLevel(score: 0.59), .bad)
        XCTAssertEqual(StreamHealthLevel(score: 0.0), .bad)
    }
}

extension StreamHealthLevel: Equatable {}
