import XCTest

/// Journeys through the real app. Run them with `scripts/ui-tests.sh`, which
/// fills one data folder per test under `~/Library/Application Support/Jolta`
/// from `Fixtures/` and removes them afterwards: the test runner is
/// sandboxed and can't write there itself. Each test launches Jolta on its own
/// folder with `-JoltaTestData UITest-<test name>`, so no test sees real mail.
@MainActor
final class Journeys: XCTestCase {
    private func launch(_ test: String = #function) throws -> XCUIApplication {
        let name = "UITest-" + test.replacingOccurrences(of: "()", with: "")
        let home = getpwuid(getuid()).map { String(cString: $0.pointee.pw_dir) } ?? NSHomeDirectory()
        let folder = home + "/Library/Application Support/Jolta/" + name
        guard FileManager.default.fileExists(atPath: folder + "/jolta.sqlite") else {
            throw XCTSkip("No test data in \(folder); run the UI tests with scripts/ui-tests.sh")
        }
        let app = XCUIApplication()
        app.launchArguments = ["-JoltaTestData", name]
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

    /// The color of the empty list below the rows, as 0–255 sRGB channels,
    /// sampled until `done` accepts it or 5 seconds pass. Its screenshot is attached as `name`.
    private func listColor(_ app: XCUIApplication, _ name: String, until done: ([Int]) -> Bool = { _ in true }) -> [Int] {
        let window = app.windows.firstMatch
        let deadline = Date() + 5
        var shot: XCUIScreenshot
        var color: [Int]
        repeat {
            shot = window.screenshot()
            color = []
            if let image = shot.image.cgImage(forProposedRect: nil, context: nil, hints: nil) {
                let scale = Double(image.width) / window.frame.width
                // Left edge of the list column, under the rows and above the toasts.
                let x = Int((232 + 12) * scale), y = Int((window.frame.height - 140) * scale)
                if let c = NSBitmapImageRep(cgImage: image).colorAt(x: x, y: y)?.usingColorSpace(.sRGB) {
                    color = [c.redComponent, c.greenComponent, c.blueComponent].map { Int(($0 * 255).rounded()) }
                }
            }
        } while !done(color) && Date() < deadline
        let attachment = XCTAttachment(screenshot: shot)
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
        return color
    }

    private func distance(_ a: [Int], _ b: [Int]) -> Int {
        guard a.count == 3, b.count == 3 else { return .max }
        return zip(a, b).map { abs($0 - $1) }.max() ?? 0
    }

    private func openThemes(_ app: XCUIApplication) {
        app.typeKey("k", modifierFlags: .command)
        app.typeText("theme")
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(text(app, "Back to Commands").waitForExistence(timeout: 5), "Change Theme… opens the theme list in the same palette")
    }

    private func commandField(_ app: XCUIApplication) -> XCUIElement {
        app.textFields.matching(NSPredicate(format: "placeholderValue == %@", "Type a command, mailbox or section")).firstMatch
    }

    /// The theme list opens on the theme in use; two rows down is the first
    /// Omarchy theme of the Mac's own mode (Catppuccin or Catppuccin Latte).
    func testThemePreviewAndChoose() throws {
        let app = try launch()
        let graphite = listColor(app, "Graphite, palette closed")

        openThemes(app)
        let listed = listColor(app, "Theme list open on Graphite")
        app.typeKey(.downArrow, modifierFlags: [])
        app.typeKey(.downArrow, modifierFlags: [])
        let previewed = listColor(app, "Previewing the theme two rows down") { self.distance($0, listed) > 6 }
        XCTAssertGreaterThan(distance(previewed, listed), 6, "moving the selection to another theme previews it behind the palette")

        app.typeKey(.escape, modifierFlags: [])
        XCTAssertTrue(text(app, "Back to Commands").waitForNonExistence(timeout: 5), "Esc closes the theme list")
        let reverted = listColor(app, "After Esc") { self.distance($0, graphite) <= 3 }
        XCTAssertLessThanOrEqual(distance(reverted, graphite), 3, "Esc puts back the theme that was in use")

        openThemes(app)
        app.typeKey(.downArrow, modifierFlags: [])
        app.typeKey(.downArrow, modifierFlags: [])
        _ = listColor(app, "Previewing before choosing") { self.distance($0, listed) > 6 }
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(text(app, "Back to Commands").waitForNonExistence(timeout: 5), "Enter on a theme closes the palette")
        let chosen = listColor(app, "Chosen theme, palette closed") { self.distance($0, graphite) > 6 }
        XCTAssertGreaterThan(distance(chosen, graphite), 6, "the chosen theme stays after the palette closes")

        openThemes(app)
        XCTAssertTrue(text(app, "In use").exists, "the theme list marks the saved themes")
        for _ in 0..<5 { app.typeKey(.upArrow, modifierFlags: []) }
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(text(app, "Back to Commands").waitForNonExistence(timeout: 5), "Enter on Back to Commands leaves the theme list")
        XCTAssertTrue(commandField(app).waitForExistence(timeout: 5), "the palette shows the commands again")

        app.typeText("theme")
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(text(app, "Back to Commands").waitForExistence(timeout: 5), "the theme list opens again")
        app.typeKey(.delete, modifierFlags: [])
        XCTAssertTrue(commandField(app).waitForExistence(timeout: 5), "⌫ in the empty field goes back to the commands")
        app.typeKey(.escape, modifierFlags: [])
        let kept = listColor(app, "Chosen theme after leaving the palette") { self.distance($0, chosen) <= 3 }
        XCTAssertLessThanOrEqual(distance(kept, chosen), 3, "leaving the theme list without choosing keeps the saved theme")
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
