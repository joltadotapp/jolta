import XCTest

/// Journeys through the real app. Run them with `scripts/ui-tests.sh`, which
/// fills one data folder per test under `~/Library/Application Support/Kite`
/// from `Fixtures/` and removes them afterwards: the test runner is
/// sandboxed and can't write there itself. Each test launches Kite on its own
/// folder with `-KiteTestData UITest-<test name>`, so no test sees real mail.
@MainActor
final class Journeys: XCTestCase {
    private func launch(_ test: String = #function) throws -> XCUIApplication {
        let name = "UITest-" + test.replacingOccurrences(of: "()", with: "")
        let home = getpwuid(getuid()).map { String(cString: $0.pointee.pw_dir) } ?? NSHomeDirectory()
        let folder = home + "/Library/Application Support/Kite/" + name
        guard FileManager.default.fileExists(atPath: folder + "/kite.sqlite") else {
            throw XCTSkip("No test data in \(folder); run the UI tests with scripts/ui-tests.sh")
        }
        let app = XCUIApplication()
        app.launchArguments = ["-KiteTestData", name]
        app.launch()
        addTeardownBlock { @MainActor in app.terminate() }
        XCTAssertTrue(text(app, "Lunch on Friday").waitForExistence(timeout: 15), "the inbox shows the seeded mail")
        return app
    }

    /// A label in the app by the text it starts with. A list row's subject
    /// and preview share one value ("Lunch on Friday  Shall we…").
    private func text(_ app: XCUIApplication, _ string: String) -> XCUIElement {
        app.staticTexts.matching(NSPredicate(format: "value BEGINSWITH %@ OR label BEGINSWITH %@", string, string)).firstMatch
    }

    private func visible(_ app: XCUIApplication, _ subject: String) -> Bool {
        text(app, subject).exists
    }

    func testArchiveAndUndoWithKeys() throws {
        let app = try launch()
        text(app, "Lunch on Friday").click()
        app.typeKey("e", modifierFlags: [])
        XCTAssertTrue(text(app, "Lunch on Friday").waitForNonExistence(timeout: 5), "e archives the selected conversation")
        app.typeKey("z", modifierFlags: [])
        XCTAssertTrue(text(app, "Lunch on Friday").waitForExistence(timeout: 5), "z brings it back")
    }

    func testOpenConversation() throws {
        let app = try launch()
        text(app, "Re: Quarterly numbers").click()
        app.typeKey(.return, modifierFlags: [])
        let reader = app.webViews.firstMatch
        XCTAssertTrue(reader.waitForExistence(timeout: 5))
        for text in ["Can you send me the Q3 numbers before Thursday?", "Got them, thanks a lot!"] {
            let shown = reader.staticTexts.matching(NSPredicate(format: "value CONTAINS %@ OR label CONTAINS %@", text, text)).firstMatch
            XCTAssertTrue(shown.waitForExistence(timeout: 5), "the reader shows \(text)")
        }
    }

    /// Keys typed while the palette is open go to the palette, never to the
    /// list: an `e` there used to archive mail (fixed 2026-11-02).
    func testPaletteKeepsTheKeyboard() throws {
        let app = try launch()
        text(app, "Lunch on Friday").click()
        app.typeKey("k", modifierFlags: .command)
        let field = app.textFields.firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 5), "⌘K opens the palette")
        app.typeText("e")
        XCTAssertTrue(visible(app, "Lunch on Friday"), "typing in the palette didn't archive")
        app.typeKey(.escape, modifierFlags: [])
        app.typeKey("e", modifierFlags: [])
        XCTAssertTrue(text(app, "Lunch on Friday").waitForNonExistence(timeout: 5), "after Esc, keys reach the list again")
    }

    func testSearchFiltersTheList() throws {
        let app = try launch()
        let search = app.searchFields.firstMatch.exists ? app.searchFields.firstMatch : app.textFields.firstMatch
        search.click()
        search.typeText("from:bob\n")
        XCTAssertTrue(text(app, "Weekly deals").waitForNonExistence(timeout: 5), "other senders leave the list")
        XCTAssertTrue(visible(app, "Lunch on Friday"))
    }
}
