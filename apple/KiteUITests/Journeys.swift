import XCTest

/// Journeys through the real app. Each test gets its own data folder under
/// `~/Library/Application Support/Kite`, filled from `Fixtures/` with the
/// signed `kite` CLI (`scripts/build-cli.sh`), and launches Kite on it with
/// `-KiteTestData <folder>`, so no test sees real mail.
@MainActor
final class Journeys: XCTestCase {
    private static let repo = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    private static let fixtures = repo.appending(path: "apple/KiteUITests/Fixtures")

    private func launch() throws -> XCUIApplication {
        let home = getpwuid(getuid()).map { String(cString: $0.pointee.pw_dir) } ?? NSHomeDirectory()
        let name = "UITest-" + UUID().uuidString.prefix(8)
        let folder = home + "/Library/Application Support/Kite/" + name
        try kite(folder, "dev", "local-account", "me@test.invalid")
        let inbox = ["01-numbers.eml", "03-numbers-thanks.eml", "04-lunch.eml", "05-deals.eml", "06-invoice.eml"]
        try kite(folder, ["import", "me@test.invalid", "inbox"] + inbox.map { Self.fixtures.appending(path: $0).path })
        try kite(folder, "import", "me@test.invalid", "sent", Self.fixtures.appending(path: "02-numbers-reply.eml").path)

        let app = XCUIApplication()
        app.launchArguments = ["-KiteTestData", name]
        app.launch()
        addTeardownBlock { @MainActor in
            app.terminate()
            try? FileManager.default.removeItem(atPath: folder)
        }
        XCTAssertTrue(app.staticTexts["Lunch on Friday"].waitForExistence(timeout: 15), "the inbox shows the seeded mail")
        return app
    }

    private func kite(_ folder: String, _ arguments: String...) throws {
        try kite(folder, arguments)
    }

    private func kite(_ folder: String, _ arguments: [String]) throws {
        let process = Process()
        process.executableURL = Self.repo.appending(path: "target/release/kite")
        process.arguments = ["--data-dir", folder] + arguments
        process.standardOutput = FileHandle.nullDevice
        try process.run()
        process.waitUntilExit()
        XCTAssertEqual(process.terminationStatus, 0, "kite \(arguments.joined(separator: " ")) failed; run scripts/build-cli.sh")
    }

    private func visible(_ app: XCUIApplication, _ subject: String) -> Bool {
        app.staticTexts[subject].exists
    }

    func testArchiveAndUndoWithKeys() throws {
        let app = try launch()
        app.staticTexts["Lunch on Friday"].click()
        app.typeKey("e", modifierFlags: [])
        XCTAssertTrue(app.staticTexts["Lunch on Friday"].waitForNonExistence(timeout: 5), "e archives the selected conversation")
        app.typeKey("z", modifierFlags: [])
        XCTAssertTrue(app.staticTexts["Lunch on Friday"].waitForExistence(timeout: 5), "z brings it back")
    }

    func testOpenConversation() throws {
        let app = try launch()
        app.staticTexts["Re: Quarterly numbers"].click()
        app.typeKey(.return, modifierFlags: [])
        let reader = app.webViews.firstMatch
        XCTAssertTrue(reader.waitForExistence(timeout: 5))
        for text in ["Can you send me the Q3 numbers before Thursday?", "Got them, thanks a lot!"] {
            XCTAssertTrue(reader.staticTexts[text].waitForExistence(timeout: 5), "the reader shows \(text)")
        }
    }

    /// Keys typed while the palette is open go to the palette, never to the
    /// list: an `e` there used to archive mail (fixed 2026-11-02).
    func testPaletteKeepsTheKeyboard() throws {
        let app = try launch()
        app.staticTexts["Lunch on Friday"].click()
        app.typeKey("k", modifierFlags: .command)
        let field = app.textFields.firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 5), "⌘K opens the palette")
        app.typeText("e")
        XCTAssertTrue(visible(app, "Lunch on Friday"), "typing in the palette didn't archive")
        app.typeKey(.escape, modifierFlags: [])
        app.typeKey("e", modifierFlags: [])
        XCTAssertTrue(app.staticTexts["Lunch on Friday"].waitForNonExistence(timeout: 5), "after Esc, keys reach the list again")
    }

    func testSearchFiltersTheList() throws {
        let app = try launch()
        let search = app.searchFields.firstMatch.exists ? app.searchFields.firstMatch : app.textFields.firstMatch
        search.click()
        search.typeText("from:bob\n")
        XCTAssertTrue(app.staticTexts["Weekly deals"].waitForNonExistence(timeout: 5), "other senders leave the list")
        XCTAssertTrue(visible(app, "Lunch on Friday"))
    }
}
