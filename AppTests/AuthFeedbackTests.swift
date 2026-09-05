import XCTest
@testable import DStreamyApp

final class AuthFeedbackTests: XCTestCase {
    func testNormalizesCopiedAuthorizationHeader() {
        XCTAssertEqual(normalizeDiscordToken("  Authorization: token-value\n"), "token-value")
    }

    func testUnauthorizedResponseExplainsHowToRetry() {
        let response = HTTPURLResponse(
            url: URL(string: "https://discord.com/api/v9/users/@me")!,
            statusCode: 401,
            httpVersion: nil,
            headerFields: nil
        )!

        XCTAssertThrowsError(try discordUsername(from: Data(), response: response)) { error in
            XCTAssertEqual(
                error.localizedDescription,
                "Discord rejected this token. Copy only the Authorization header value and try again."
            )
        }
    }

    func testTokenPresenceAloneIsNotAuthenticated() {
        XCTAssertFalse(isDiscordAuthenticated(username: "", authError: nil))
        XCTAssertTrue(isDiscordAuthenticated(username: "user", authError: nil))
        XCTAssertFalse(isDiscordAuthenticated(username: "user", authError: "rejected"))
    }
}
