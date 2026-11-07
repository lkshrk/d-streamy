import XCTest
@testable import DStreamyApp

final class SourceDropTests: XCTestCase {
    private func stats(_ additions: String = "") throws -> StreamStats {
        let json = """
        {"video_frames_sent":10,"video_frames_dropped":0,"audio_frames_sent":20,"bytes_sent":100,"resumes":0,"fps":30,"last_error":null\(additions)}
        """
        return try DaemonCoding.decoder.decode(StreamStats.self, from: Data(json.utf8))
    }

    func testLegacyStatsDefaultMissingSourceDropsToZero() throws {
        let value = try stats()
        XCTAssertEqual(value.sourceVideoFramesDropped, 0)
        XCTAssertEqual(value.sourceAudioFramesDropped, 0)
        XCTAssertEqual(value.totalVideoFramesDropped, 0)
    }

    func testSourceOnlyLossIsIncludedWithZeroTransportLoss() throws {
        let value = try stats(#", "source_video_frames_dropped":7,"source_audio_frames_dropped":11"#)
        XCTAssertEqual(value.videoFramesDropped, 0)
        XCTAssertEqual(value.totalVideoFramesDropped, 7)
        XCTAssertEqual(value.sourceAudioFramesDropped, 11)
    }

    func testTransportAndSourceVideoAreCountedExactlyOnce() throws {
        let json = #"{"video_frames_sent":10,"video_frames_dropped":5,"source_video_frames_dropped":7,"source_audio_frames_dropped":11,"audio_frames_sent":20,"bytes_sent":100,"resumes":0,"fps":30,"last_error":null,"e2ee":"encrypted","dave_protocol_version":1,"future_counter":99}"#
        let value = try DaemonCoding.decoder.decode(StreamStats.self, from: Data(json.utf8))
        XCTAssertEqual(value.totalVideoFramesDropped, 12)
        XCTAssertEqual(value.videoFramesDropped, 5)
        XCTAssertEqual(value.sourceVideoFramesDropped, 7)
        XCTAssertEqual(value.sourceAudioFramesDropped, 11)
        XCTAssertEqual(value.e2ee, .encrypted)
        XCTAssertEqual(value.daveProtocolVersion, 1)

        let roundTrip = try DaemonCoding.decoder.decode(StreamStats.self, from: DaemonCoding.encoder.encode(value))
        XCTAssertEqual(roundTrip, value)
        XCTAssertEqual(roundTrip.totalVideoFramesDropped, 12)
    }

    func testNullSourceCountersAndFutureEncryptionRemainCompatible() throws {
        let value = try stats(#", "source_video_frames_dropped":null,"source_audio_frames_dropped":null,"e2ee":"future_state""#)
        XCTAssertEqual(value.sourceVideoFramesDropped, 0)
        XCTAssertEqual(value.sourceAudioFramesDropped, 0)
        XCTAssertEqual(value.e2ee, .unknown)
    }
    func testSourceAudioLossDoesNotChangeTransportIntervalMetrics() throws {
        let path = DaemonFixtureTests.fixtureDir.appendingPathComponent("session.out.ndjson")
        let lines = try String(contentsOf: path, encoding: .utf8).split(whereSeparator: \.isNewline)
        for line in lines {
            let message = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any])
            guard var payload = message["event"] as? [String: Any], payload["event"] as? String == "health" else { continue }
            var stats = try XCTUnwrap(payload["stats"] as? [String: Any])
            stats["source_audio_frames_dropped"] = 11
            stats["source_video_frames_dropped"] = 7
            payload["stats"] = stats
            let snapshot = try DaemonCoding.decoder.decode(HealthSnapshot.self, from: JSONSerialization.data(withJSONObject: payload))
            XCTAssertEqual(snapshot.stats.sourceAudioFramesDropped, 11)
            XCTAssertEqual(snapshot.stats.totalVideoFramesDropped, 7)
            XCTAssertEqual(snapshot.metrics.aDropNotReady, 0)
            XCTAssertEqual(snapshot.metrics.aEncErr, 0)
            XCTAssertEqual(snapshot.stats.e2ee, .encrypted)
            return
        }
        XCTFail("session fixture has no health event")
    }

}
