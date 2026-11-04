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
        // With another Jolta running (the one installed on this Mac) the test
        // instance starts in the background and SwiftUI never presents the
        // main window; the menu bar item's Open Jolta does.
        if !app.windows.firstMatch.waitForExistence(timeout: 3) {
            app.statusItems.firstMatch.click()
            app.menuItems["Open Jolta"].click()
        }
        XCTAssertTrue(text(app, "Lunch on Friday").waitForExistence(timeout: 15), "the inbox shows the seeded mail")
        return app
    }

    /// A label in the app by the text it starts with. A list row's subject
    /// and preview share one value ("Lunch on Friday  Shall we…").
    private func text(_ app: XCUIApplication, _ string: String) -> XCUIElement {
        app.staticTexts.matching(NSPredicate(format: "value BEGINSWITH %@ OR label BEGINSWITH %@", string, string)).firstMatch
    }

    /// A palette row by any of its texts: VoiceOver reads a row as one
    /// button labelled with them all ("Graphite Dark, Dark, Dark mode").
    private func paletteRow(_ app: XCUIApplication, _ string: String) -> XCUIElement {
        app.buttons.matching(NSPredicate(format: "label CONTAINS %@", string)).firstMatch
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

    /// Y, M and N answer the invitation in the open conversation. The test
    /// account has no calendar and no server, so each answer is a reply
    /// mail that waits in the outbox, and the card shows the answer given.
    func testAnswerInvitationWithKeys() throws {
        let app = try launch()
        text(app, "Invitation: Design crit").click()
        app.typeKey(.return, modifierFlags: [])
        let reader = app.webViews.firstMatch
        let body = reader.staticTexts.matching(NSPredicate(format: "value CONTAINS %@ OR label CONTAINS %@", "Shall we go", "Shall we go")).firstMatch
        XCTAssertTrue(body.waitForExistence(timeout: 15), "the reader shows the invitation")
        let link = { (label: String) in reader.links.matching(NSPredicate(format: "title BEGINSWITH %@ OR label BEGINSWITH %@", label, label)).firstMatch }
        XCTAssertTrue(link("Accept").waitForExistence(timeout: 10), "the card offers the answers")
        app.typeKey("y", modifierFlags: [])
        XCTAssertTrue(link("Accepted").waitForExistence(timeout: 5), "y accepts")
        XCTAssertTrue(text(app, "Accepted. Jolta emailed Ann.").waitForExistence(timeout: 5), "the toast says who was told")
        app.typeKey("m", modifierFlags: [])
        XCTAssertTrue(text(app, "Answered maybe. Jolta emailed Ann.").waitForExistence(timeout: 5), "m answers maybe")
        XCTAssertTrue(link("Accept").waitForExistence(timeout: 5) && !link("Accepted").exists)
        app.typeKey("n", modifierFlags: [])
        XCTAssertTrue(link("Declined").waitForExistence(timeout: 5), "n declines")
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
        XCTAssertTrue(paletteRow(app, "Back to Commands").waitForExistence(timeout: 5), "Change Theme… opens the theme list in the same palette")
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
        XCTAssertTrue(paletteRow(app, "Back to Commands").waitForNonExistence(timeout: 5), "Esc closes the theme list")
        let reverted = listColor(app, "After Esc") { self.distance($0, graphite) <= 3 }
        XCTAssertLessThanOrEqual(distance(reverted, graphite), 3, "Esc puts back the theme that was in use")

        openThemes(app)
        app.typeKey(.downArrow, modifierFlags: [])
        app.typeKey(.downArrow, modifierFlags: [])
        _ = listColor(app, "Previewing before choosing") { self.distance($0, listed) > 6 }
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(paletteRow(app, "Back to Commands").waitForNonExistence(timeout: 5), "Enter on a theme closes the palette")
        let chosen = listColor(app, "Chosen theme, palette closed") { self.distance($0, graphite) > 6 }
        XCTAssertGreaterThan(distance(chosen, graphite), 6, "the chosen theme stays after the palette closes")

        openThemes(app)
        XCTAssertTrue(paletteRow(app, "Dark mode").exists && paletteRow(app, "Light mode").exists, "the theme list marks the theme for each appearance")
        for _ in 0..<5 { app.typeKey(.upArrow, modifierFlags: []) }
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(paletteRow(app, "Back to Commands").waitForNonExistence(timeout: 5), "Enter on Back to Commands leaves the theme list")
        XCTAssertTrue(commandField(app).waitForExistence(timeout: 5), "the palette shows the commands again")

        app.typeText("theme")
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(paletteRow(app, "Back to Commands").waitForExistence(timeout: 5), "the theme list opens again")
        app.typeKey(.delete, modifierFlags: [])
        XCTAssertTrue(commandField(app).waitForExistence(timeout: 5), "⌫ in the empty field goes back to the commands")
        app.typeKey(.escape, modifierFlags: [])
        let kept = listColor(app, "Chosen theme after leaving the palette") { self.distance($0, chosen) <= 3 }
        XCTAssertLessThanOrEqual(distance(kept, chosen), 3, "leaving the theme list without choosing keeps the saved theme")
    }

    /// A theme of the other mode than the Mac's shows at once, as the one
    /// theme Jolta always uses, until Match macOS Appearance.
    func testThemeOfTheOtherModeShowsAlways() throws {
        let app = try launch()
        let graphite = listColor(app, "Graphite, matching macOS")
        let other = luminance(graphite) < 0.5 ? "Graphite Light" : "Graphite Dark"

        openThemes(app)
        app.typeText(other)
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(text(app, "Always using \(other).").waitForExistence(timeout: 5), "a toast says the theme now shows always")
        let always = listColor(app, "\(other), always") { self.distance($0, graphite) > 40 }
        XCTAssertGreaterThan(distance(always, graphite), 40, "the theme of the other mode shows")

        openThemes(app)
        XCTAssertTrue(paletteRow(app, "Always").exists, "the theme list marks the theme shown always")
        XCTAssertTrue(paletteRow(app, "Match macOS Appearance").exists, "the theme list offers matching macOS again")
        app.typeKey(.escape, modifierFlags: [])

        app.typeKey("k", modifierFlags: .command)
        app.typeText("match macos")
        app.typeKey(.return, modifierFlags: [])
        let matched = listColor(app, "Matching macOS again") { self.distance($0, graphite) <= 3 }
        XCTAssertLessThanOrEqual(distance(matched, graphite), 3, "Match macOS Appearance brings back the theme for the Mac's mode")
    }

    private func luminance(_ c: [Int]) -> Double {
        c.count == 3 ? Double(c.reduce(0, +)) / (3 * 255) : 0
    }

    func testSearchFiltersTheList() throws {
        let app = try launch()
        let search = app.searchFields.firstMatch.exists ? app.searchFields.firstMatch : app.textFields.firstMatch
        search.click()
        search.typeText("from:bob\n")
        XCTAssertTrue(text(app, "Weekly deals").waitForNonExistence(timeout: 5), "other senders leave the list")
        XCTAssertTrue(visible(app, "Lunch on Friday"))
    }

    /// ⌘2 opens the calendar on this week; J and K move a week and back;
    /// an event opens with Return; its calendar's number hides it.
    func testCalendarKeys() throws {
        let app = try launch()
        app.typeKey("2", modifierFlags: .command)
        let title = app.descendants(matching: .any).matching(identifier: "calendar-title").firstMatch
        XCTAssertTrue(title.waitForExistence(timeout: 5), "⌘2 opens calendar mode")
        let week = title.label
        XCTAssertTrue(week.contains("Week"), "the week view is the default: \(week)")

        app.typeKey("j", modifierFlags: [])
        XCTAssertTrue(waitUntil { title.label != week }, "J moves to the next week")
        app.typeKey("k", modifierFlags: [])
        XCTAssertTrue(waitUntil { title.label == week }, "K comes back")

        let climbing = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Climbing, 10:00 to 12:30")).firstMatch
        XCTAssertTrue(climbing.waitForExistence(timeout: 5), "the week shows the fixture's Saturday event with its full label")
        climbing.click()
        app.typeKey(.return, modifierFlags: [])
        let detail = app.staticTexts.matching(identifier: "event-detail-title").firstMatch
        XCTAssertTrue(detail.waitForExistence(timeout: 5), "Return opens the selected event")
        XCTAssertEqual(detail.value as? String ?? detail.label, "Climbing", "the detail shows the event's title")
        app.typeKey(.escape, modifierFlags: [])
        XCTAssertTrue(detail.waitForNonExistence(timeout: 5), "Esc closes the detail")

        let personal = app.descendants(matching: .any).matching(identifier: "calendar-Personal").firstMatch
        XCTAssertTrue(personal.waitForExistence(timeout: 5), "the sidebar lists the Personal calendar")
        let key = try XCTUnwrap((personal.value as? String)?.split(separator: " ").last.map(String.init), "Personal has a number key")
        app.typeKey(key, modifierFlags: [])
        XCTAssertTrue(climbing.waitForNonExistence(timeout: 5), "\(key) hides the Personal calendar's events")
        app.typeKey(key, modifierFlags: [])
        XCTAssertTrue(climbing.waitForExistence(timeout: 5), "\(key) again shows them")

        app.typeKey("n", modifierFlags: .command)
        XCTAssertTrue(app.descendants(matching: .any).matching(identifier: "compose-subject").firstMatch.waitForExistence(timeout: 5),
                      "⌘N in calendar mode shows the new message in mail")
        app.typeKey(.escape, modifierFlags: [])
    }

    /// Return in a new section's query field saves it. The page used to read
    /// the sections before they reloaded, drop the new row under the focused
    /// field, and crash (2026-11-04, a "Starred" section with is:starred).
    func testAddSectionWithReturn() throws {
        let app = try launch()
        app.typeKey(",", modifierFlags: .command)
        let page = app.descendants(matching: .any).matching(NSPredicate(format: "label == 'Sections' OR value == 'Sections'")).firstMatch
        XCTAssertTrue(page.waitForExistence(timeout: 5), "settings lists the Sections page")
        page.click()
        app.buttons["Add Section"].firstMatch.click()
        let name = app.textFields.matching(NSPredicate(format: "value == 'New section'")).firstMatch
        XCTAssertTrue(name.waitForExistence(timeout: 5), "Add Section adds a row")
        name.click()
        app.typeKey("a", modifierFlags: .command)
        app.typeText("Starred")
        let query = app.textFields.matching(NSPredicate(format: "value == 'from:'")).firstMatch
        query.click()
        app.typeKey("a", modifierFlags: .command)
        app.typeText("is:starred")
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(app.buttons["Revert"].waitForNonExistence(timeout: 5), "Return saves the sections")
        XCTAssertEqual(app.state, .runningForeground, "Jolta is still running")
        XCTAssertTrue(app.textFields.matching(NSPredicate(format: "value == 'Starred'")).firstMatch.exists, "the saved section stays on the page")
    }

    private func waitUntil(_ done: () -> Bool) -> Bool {
        let deadline = Date() + 5
        while Date() < deadline {
            if done() { return true }
            RunLoop.current.run(until: Date() + 0.1)
        }
        return done()
    }
}
