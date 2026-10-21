import XCTest
@testable import DStreamyApp

final class AuthFeedbackTests: XCTestCase {
    func testNormalizesCopiedAuthorizationHeader() {
        XCTAssertEqual(normalizeDiscordToken("  Authorization: token-value\n"), "token-value")
    }
}
