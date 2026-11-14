import XCTest
@testable import DStreamyApp

final class EncryptionTests: XCTestCase {
    private func event(_ text: String) throws -> SessionEvent {
        try DaemonCoding.decoder.decode(SessionEvent.self, from: Data(text.utf8))
    }

    private func health(_ fields: [String: Any] = [:]) throws -> HealthSnapshot {
        let path = DaemonFixtureTests.fixtureDir.appendingPathComponent("session.out.ndjson")
        let lines = try String(contentsOf: path, encoding: .utf8).split(whereSeparator: \.isNewline)
        for line in lines {
            let message = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any])
            guard var payload = message["event"] as? [String: Any], payload["event"] as? String == "health" else { continue }
            var stats = try XCTUnwrap(payload["stats"] as? [String: Any])
            stats.removeValue(forKey: "e2ee")
            stats.removeValue(forKey: "dave_protocol_version")
            stats.merge(fields) { _, value in value }
            payload["stats"] = stats
            return try DaemonCoding.decoder.decode(HealthSnapshot.self, from: JSONSerialization.data(withJSONObject: payload))
        }
        XCTFail("session fixture has no health event")
        throw NSError(domain: "EncryptionTests", code: 1)
    }

    func testStatsDecodeNewAndMissingFields() throws {
        let legacy = try health().stats
        XCTAssertNil(legacy.e2ee)
        XCTAssertNil(legacy.daveProtocolVersion)

        let encrypted = try health(["e2ee": "encrypted", "dave_protocol_version": 1]).stats
        XCTAssertEqual(encrypted.e2ee, .encrypted)
        XCTAssertEqual(encrypted.daveProtocolVersion, 1)

        let unencrypted = try health(["e2ee": "unencrypted", "dave_protocol_version": 0]).stats
        XCTAssertEqual(unencrypted.e2ee, .unencrypted)
        XCTAssertEqual(unencrypted.daveProtocolVersion, 0)

        XCTAssertEqual(try health(["e2ee": "future_state"]).stats.e2ee, .unknown)
        XCTAssertNil(try health(["e2ee": NSNull(), "dave_protocol_version": NSNull()]).stats.e2ee)
    }

    func testEncryptionEventsDecodeWithoutOptionalFields() throws {
        XCTAssertEqual(
            try event(#"{"event":"e2ee","state":"encrypted","dave_protocol_version":1}"#),
            .e2ee(state: .encrypted, daveProtocolVersion: 1)
        )
        XCTAssertEqual(
            try event(#"{"event":"e2ee","state":"unencrypted","dave_protocol_version":0}"#),
            .e2ee(state: .unencrypted, daveProtocolVersion: 0)
        )
        XCTAssertEqual(try event(#"{"event":"e2ee"}"#), .e2ee(state: .unknown, daveProtocolVersion: nil))
        XCTAssertEqual(
            try event(#"{"event":"e2ee","state":null,"dave_protocol_version":null}"#),
            .e2ee(state: .unknown, daveProtocolVersion: nil)
        )
        XCTAssertEqual(
            try event(#"{"event":"e2ee","state":"future_state"}"#),
            .e2ee(state: .unknown, daveProtocolVersion: nil)
        )
    }

    func testEncryptionStatusDoesNotInferEncryptionFromProtocolVersion() {
        let unknown = EncryptionStatus(state: .unknown, daveProtocolVersion: 1)
        XCTAssertEqual(unknown.title, "Encryption status unknown")
        XCTAssertNil(unknown.warning)
        XCTAssertEqual(EncryptionStatus().detail, "DAVE protocol not reported")

        let unencrypted = EncryptionStatus(state: .unencrypted, daveProtocolVersion: 1)
        XCTAssertEqual(unencrypted.title, "Not end-to-end encrypted")
        XCTAssertEqual(unencrypted.detail, "DAVE protocol 1")
        XCTAssertTrue(unencrypted.warning?.contains("Discord can read") == true)
        XCTAssertNil(EncryptionStatus(state: .encrypted, daveProtocolVersion: 1).warning)
    }

    @MainActor
    func testDowngradeIsVisibleWithoutBlamingLogin() async throws {
        let state = authenticatedState()
        state.handle(.state(.connecting))
        state.handle(try event(#"{"event":"e2ee","state":"encrypted","dave_protocol_version":1}"#))
        XCTAssertEqual(state.encryption.state, .encrypted)

        state.handle(try event(#"{"event":"e2ee","state":"unencrypted","dave_protocol_version":0}"#))
        XCTAssertNotNil(state.encryption.warning)
        XCTAssertEqual(state.encryption.daveProtocolVersion, 0)
        XCTAssertTrue(state.isAuthenticated)
        XCTAssertNil(state.authError)

        state.handle(.health(try health()))
        XCTAssertEqual(state.encryption.state, .unencrypted)
        state.handle(.health(try health(["e2ee": "encrypted", "dave_protocol_version": 1])))
        XCTAssertEqual(state.encryption.state, .encrypted)
        XCTAssertNil(state.encryption.warning)

        state.handle(.state(.reconnecting(attempt: 1)))
        XCTAssertEqual(state.encryption.state, .unknown)
        state.handle(.e2ee(state: .encrypted, daveProtocolVersion: 1))
        state.handle(.state(.stopped(.requested)))
        XCTAssertEqual(state.encryption, EncryptionStatus())
        state.handle(.e2ee(state: .unencrypted, daveProtocolVersion: 0))
        XCTAssertEqual(state.encryption, EncryptionStatus())
    }

    @MainActor
    func testEncryptionUnavailableEndsSessionButKeepsLogin() async throws {
        let state = authenticatedState()
        state.handle(.state(.connecting))
        state.handle(try event(#"{"event":"state","state":{"state":"stopped","reason":{"reason":"e2ee_unavailable"}}}"#))
        XCTAssertEqual(state.streamState, .error(StopReason.e2eeUnavailable.message))
        XCTAssertFalse(state.streamState.canStop)
        XCTAssertTrue(state.isAuthenticated)
        XCTAssertNil(state.authError)
        XCTAssertEqual(state.encryption, EncryptionStatus())
    }
}
