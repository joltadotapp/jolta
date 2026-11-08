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

    /// A list row's invitation state, by its whole label.
    private func chip(_ app: XCUIApplication, _ state: String) -> XCUIElement {
        app.tables.staticTexts.matching(NSPredicate(format: "label == %@ OR value == %@", state, state)).firstMatch
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
    /// With the invitation's message collapsed, its card is off screen, and
    /// the keys and the palette leave it alone. The list row shows each
    /// answer beside the calendar mark.
    func testAnswerInvitationWithKeys() throws {
        let app = try launch()
        XCTAssertTrue(chip(app, "Not answered").waitForExistence(timeout: 5), "the list marks the invitation still to answer")
        text(app, "Re: Invitation: Design crit").click()
        app.typeKey(.return, modifierFlags: [])
        let reader = app.webViews.firstMatch
        let body = reader.staticTexts.matching(NSPredicate(format: "value CONTAINS %@ OR label CONTAINS %@", "Shall we go", "Shall we go")).firstMatch
        XCTAssertTrue(body.waitForExistence(timeout: 15), "the reader shows the invitation")
        let link = { (label: String) in reader.links.matching(NSPredicate(format: "title BEGINSWITH %@ OR label BEGINSWITH %@", label, label)).firstMatch }
        XCTAssertTrue(link("Accept").waitForExistence(timeout: 10), "the card offers the answers")
        app.typeKey("y", modifierFlags: [])
        XCTAssertTrue(link("Accepted").waitForExistence(timeout: 5), "y accepts")
        XCTAssertTrue(text(app, "Accepted. Jolta emailed Ann.").waitForExistence(timeout: 5), "the toast says who was told")
        XCTAssertTrue(chip(app, "Accepted").waitForExistence(timeout: 5), "the list row shows the answer")
        XCTAssertFalse(chip(app, "Not answered").exists)
        app.typeKey("m", modifierFlags: [])
        XCTAssertTrue(text(app, "Answered maybe. Jolta emailed Ann.").waitForExistence(timeout: 5), "m answers maybe")
        XCTAssertTrue(link("Accept").waitForExistence(timeout: 5) && !link("Accepted").exists)
        app.typeKey("n", modifierFlags: [])
        XCTAssertTrue(link("Declined").waitForExistence(timeout: 5), "n declines")
        XCTAssertTrue(chip(app, "Declined").waitForExistence(timeout: 5), "the list row follows")
        // The reader loads the answered page again, before or after the list
        // row changes; while it loads, the header isn't laid out.
        let header = reader.staticTexts.matching(NSPredicate(format: "value == %@ OR label == %@", "Ann Organizer", "Ann Organizer")).firstMatch
        XCTAssertTrue(waitUntil { header.exists && header.isHittable }, "the page lays out the message header")
        header.click()
        XCTAssertTrue(link("Declined").waitForNonExistence(timeout: 5), "the invitation's message collapses")
        app.typeKey("y", modifierFlags: [])
        XCTAssertFalse(text(app, "Accepted. Jolta emailed Ann.").waitForExistence(timeout: 3), "y answers no card off screen")
        app.typeKey("k", modifierFlags: .command)
        XCTAssertTrue(app.textFields.firstMatch.waitForExistence(timeout: 5))
        app.typeText("invitation")
        XCTAssertFalse(paletteRow(app, "Accept invitation").waitForExistence(timeout: 2), "the palette offers no answer either")
        app.typeKey(.escape, modifierFlags: [])

        XCTAssertTrue(waitUntil { header.exists && header.isHittable })
        header.click()
        XCTAssertTrue(link("Declined").waitForExistence(timeout: 5), "expanded again, the card is back")
        app.typeKey("k", modifierFlags: .command)
        app.typeText("Accept invitation")
        XCTAssertTrue(paletteRow(app, "Accept invitation").waitForExistence(timeout: 5), "the palette offers the answers again")
        app.typeKey(.escape, modifierFlags: [])
        app.typeKey("y", modifierFlags: [])
        XCTAssertTrue(link("Accepted").waitForExistence(timeout: 5), "y accepts the card on screen")
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

    /// The event editor: C opens it on the next half hour, a title and ⌘↩
    /// put the event in the week, selected; E opens it again, a typed start
    /// moves it and a typed end, digit by digit, stays on its day; ⌘⌫
    /// deletes it and Z brings it back.
    func testEventEditor() throws {
        let app = try launch()
        app.typeKey("2", modifierFlags: .command)
        XCTAssertTrue(app.descendants(matching: .any).matching(identifier: "calendar-title").firstMatch.waitForExistence(timeout: 5), "⌘2 opens calendar mode")

        app.typeKey("c", modifierFlags: [])
        let title = app.textFields["editor-title"]
        XCTAssertTrue(title.waitForExistence(timeout: 5), "C opens the editor")
        title.typeText("Journey review")
        app.typeKey(.return, modifierFlags: .command)
        let created = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Journey review,")).firstMatch
        XCTAssertTrue(created.waitForExistence(timeout: 5), "⌘↩ saves it into the week")
        XCTAssertTrue(title.waitForNonExistence(timeout: 5), "and closes the editor")

        app.typeKey("e", modifierFlags: [])
        let start = app.textFields["editor-start"]
        XCTAssertTrue(start.waitForExistence(timeout: 5), "E opens the event just saved")
        XCTAssertEqual(app.textFields["editor-title"].value as? String, "Journey review")
        start.click()
        app.typeKey("a", modifierFlags: .command)
        start.typeText("1400")
        let end = app.textFields["editor-end"]
        end.click()
        XCTAssertEqual(end.value as? String, "14:30", "the typed start moves it, keeping its 30 minutes")
        app.typeKey("a", modifierFlags: .command)
        // "1" alone reads as 13:00, before the start; the end must still land today.
        end.typeText("1600")
        app.typeKey(.return, modifierFlags: .command)
        let moved = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Journey review, 14:00 to 16:00")).firstMatch
        XCTAssertTrue(moved.waitForExistence(timeout: 5), "the typed times are saved")

        app.typeKey("e", modifierFlags: [])
        XCTAssertTrue(start.waitForExistence(timeout: 5))
        let length = app.descendants(matching: .any).matching(identifier: "editor-length").firstMatch
        XCTAssertTrue(length.waitForExistence(timeout: 5))
        XCTAssertEqual(length.title, "2 h", "a typed end lands on the start's day")
        app.typeKey(.delete, modifierFlags: .command)
        XCTAssertTrue(moved.waitForNonExistence(timeout: 5), "⌘⌫ deletes it")
        app.typeKey("z", modifierFlags: [])
        XCTAssertTrue(moved.waitForExistence(timeout: 5), "Z brings it back")
    }

    /// Moving between calendars: the editor's calendar picker on a stored
    /// event moves it there with its other changes, and Z takes back both;
    /// a meeting someone else organizes offers no picker, and the user's own
    /// meeting with guests finds the calendars on this Mac greyed.
    func testMoveBetweenCalendars() throws {
        let app = try launch()
        app.typeKey("2", modifierFlags: .command)
        XCTAssertTrue(app.descendants(matching: .any).matching(identifier: "calendar-title").firstMatch.waitForExistence(timeout: 5), "⌘2 opens calendar mode")
        let picker = app.descendants(matching: .any).matching(identifier: "editor-calendar").firstMatch
        let pick = { (name: String) in
            picker.click()
            let item = app.menuItems[name]
            XCTAssertTrue(item.waitForExistence(timeout: 5), "the calendar picker lists \(name)")
            XCTAssertTrue(item.isEnabled, "\(name) can take the event")
            item.click()
        }
        app.typeKey("c", modifierFlags: [])
        let title = app.textFields["editor-title"]
        XCTAssertTrue(title.waitForExistence(timeout: 5), "C opens the editor")
        title.typeText("Move check")
        pick("Work")
        app.typeKey(.return, modifierFlags: .command)
        let created = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Move check,")).firstMatch
        XCTAssertTrue(created.waitForExistence(timeout: 5), "⌘↩ saves it into Work")

        app.typeKey("e", modifierFlags: [])
        XCTAssertTrue(title.waitForExistence(timeout: 5), "E opens it again")
        XCTAssertTrue(picker.waitForExistence(timeout: 5), "a stored event has the calendar picker too")
        title.click()
        app.typeKey("a", modifierFlags: .command)
        title.typeText("Moved check")
        pick("Personal")
        app.typeKey(.return, modifierFlags: .command)
        XCTAssertTrue(text(app, "Moved Moved check to Personal").waitForExistence(timeout: 5), "the toast says where it went, with Undo")
        let moved = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Moved check,")).firstMatch
        XCTAssertTrue(moved.waitForExistence(timeout: 5), "the new title shows")

        let work = app.descendants(matching: .any).matching(identifier: "calendar-Work").firstMatch
        XCTAssertTrue(work.waitForExistence(timeout: 5), "the sidebar lists the Work calendar")
        let key = try XCTUnwrap((work.value as? String)?.split(separator: " ").last.map(String.init), "Work has a number key")
        app.typeKey(key, modifierFlags: [])
        XCTAssertTrue(waitUntil { app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Design review,")).count == 0 }, "\(key) hides Work")
        XCTAssertTrue(moved.exists, "the moved event isn't in Work any more")
        app.typeKey("z", modifierFlags: [])
        XCTAssertTrue(moved.waitForNonExistence(timeout: 5), "Z takes the move and the new title back, into the hidden Work")
        app.typeKey(key, modifierFlags: [])
        XCTAssertTrue(created.waitForExistence(timeout: 5), "Work shows it again, as it was")

        let review = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Design review,")).firstMatch
        XCTAssertTrue(review.waitForExistence(timeout: 5))
        review.click()
        app.typeKey("e", modifierFlags: [])
        let notice = app.descendants(matching: .any).matching(identifier: "editor-notice").firstMatch
        XCTAssertTrue(notice.waitForExistence(timeout: 5), "E opens someone else's meeting, saying who organizes it")
        XCTAssertFalse(picker.exists, "it stays in the calendar the invitation reached: no picker")
        app.typeKey(.escape, modifierFlags: [])

        let team = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Team sync,")).firstMatch
        XCTAssertTrue(team.waitForExistence(timeout: 5), "the user's own meeting on a Google calendar")
        team.click()
        app.typeKey("e", modifierFlags: [])
        XCTAssertTrue(picker.waitForExistence(timeout: 5), "a meeting the user organizes has the picker")
        picker.click()
        let mac = app.menuItems["Work"]
        XCTAssertTrue(mac.waitForExistence(timeout: 5))
        XCTAssertFalse(mac.isEnabled, "a calendar on this Mac is greyed for a meeting with guests")
        let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        shot.name = "move-picker"
        shot.lifetime = .keepAlways
        add(shot)
        app.typeKey(.escape, modifierFlags: [])
        app.typeKey(.escape, modifierFlags: [])
    }

    /// Add video call: on a Google calendar the menu offers Google Meet,
    /// with Teams greyed, and the call shows as made on save; a calendar on
    /// this Mac can't have one and says so; the saved event keeps asking for
    /// the call until Google answers.
    func testAddVideoCall() throws {
        let app = try launch()
        app.typeKey("2", modifierFlags: .command)
        XCTAssertTrue(app.descendants(matching: .any).matching(identifier: "calendar-title").firstMatch.waitForExistence(timeout: 5), "⌘2 opens calendar mode")
        app.typeKey("c", modifierFlags: [])
        let title = app.textFields["editor-title"]
        XCTAssertTrue(title.waitForExistence(timeout: 5), "C opens the editor")
        title.typeText("Video check")
        let picker = app.descendants(matching: .any).matching(identifier: "editor-calendar").firstMatch
        let pick = { (name: String) in
            picker.click()
            let item = app.menuItems[name]
            XCTAssertTrue(item.waitForExistence(timeout: 5), "the calendar picker lists \(name)")
            item.click()
        }
        pick("Team")
        let add = app.descendants(matching: .any).matching(identifier: "editor-video").firstMatch
        XCTAssertTrue(add.waitForExistence(timeout: 5), "a Google calendar offers Add video call")
        add.click()
        let meet = app.menuItems.matching(NSPredicate(format: "title BEGINSWITH %@", "Google Meet")).firstMatch
        let teams = app.menuItems.matching(NSPredicate(format: "title BEGINSWITH %@", "Teams meeting")).firstMatch
        XCTAssertTrue(meet.waitForExistence(timeout: 5), "the menu lists Google Meet")
        XCTAssertTrue(meet.isEnabled && teams.exists && !teams.isEnabled, "Teams is greyed on Google")
        meet.click()
        let pending = text(app, "Google Meet · link made on save")
        XCTAssertTrue(pending.waitForExistence(timeout: 5), "the chosen call waits for the save")

        pick("Work")
        XCTAssertTrue(text(app, "Work can't have a video call").waitForExistence(timeout: 5), "a calendar on this Mac says it can't")
        pick("Team")
        XCTAssertTrue(pending.waitForExistence(timeout: 5))
        app.typeKey(.return, modifierFlags: .command)
        let created = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Video check,")).firstMatch
        XCTAssertTrue(created.waitForExistence(timeout: 5), "⌘↩ saves it into the week")
        app.typeKey("e", modifierFlags: [])
        XCTAssertTrue(pending.waitForExistence(timeout: 5), "until Google answers, the event still asks for its Meet")
        XCTAssertFalse(add.exists, "and offers no second call")
        app.typeKey(.escape, modifierFlags: [])
    }

    /// A repeating event: ⌘R in the editor opens the repeat popover, ↓ picks
    /// the weekly preset and Return takes it. A new time for the second
    /// occurrence asks which events it is for, and 1 moves only that one;
    /// ⌘⌫ on the third asks again, and 1 deletes only that one.
    func testRepeatingEvent() throws {
        let app = try launch()
        app.typeKey("2", modifierFlags: .command)
        let calendarTitle = app.descendants(matching: .any).matching(identifier: "calendar-title").firstMatch
        XCTAssertTrue(calendarTitle.waitForExistence(timeout: 5), "⌘2 opens calendar mode")

        app.typeKey("c", modifierFlags: [])
        let title = app.textFields["editor-title"]
        XCTAssertTrue(title.waitForExistence(timeout: 5), "C opens the editor")
        title.typeText("Weekly sync")
        // A start of its own, inside the hours the week shows whatever the
        // time of day: the next half hour falls off the grid late at night.
        let firstStart = app.textFields["editor-start"]
        firstStart.click()
        app.typeKey("a", modifierFlags: .command)
        firstStart.typeText("1000\t")
        XCTAssertTrue(waitUntil { (firstStart.value as? String) == "10:00" }, "the start reads 10:00")
        let original = "10:00"
        title.click()

        app.typeKey("r", modifierFlags: .command)
        let popover = app.descendants(matching: .any).matching(identifier: "repeat-popover").firstMatch
        XCTAssertTrue(popover.waitForExistence(timeout: 5), "⌘R opens the repeat popover")
        let rows = app.buttons.matching(identifier: "repeat-row")
        let weekly = rows.matching(NSPredicate(format: "label BEGINSWITH %@", "Every week on")).firstMatch
        XCTAssertTrue(weekly.waitForExistence(timeout: 5), "the presets offer every week on the event's weekday")
        for _ in 0..<4 where !weekly.isSelected { app.typeKey(.downArrow, modifierFlags: []) }
        XCTAssertTrue(weekly.isSelected, "↓ moves to the weekly preset")
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(popover.waitForNonExistence(timeout: 5), "Return takes it and closes the popover")
        let row = app.buttons["editor-repeat"]
        XCTAssertNotEqual(row.label, "Does not repeat", "the Repeat row shows the rule")
        app.typeKey(.return, modifierFlags: .command)
        XCTAssertTrue(title.waitForNonExistence(timeout: 5), "⌘↩ saves the series")

        let atOriginal = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Weekly sync, \(original) to"))
        XCTAssertTrue(atOriginal.firstMatch.waitForExistence(timeout: 5), "the first occurrence is in its week")
        let firstWeek = calendarTitle.label
        app.typeKey("j", modifierFlags: [])
        XCTAssertTrue(waitUntil { calendarTitle.label != firstWeek }, "J moves to the next week")
        XCTAssertTrue(atOriginal.firstMatch.waitForExistence(timeout: 5), "the second occurrence is a week later")

        atOriginal.firstMatch.click()
        app.typeKey("e", modifierFlags: [])
        let start = app.textFields["editor-start"]
        XCTAssertTrue(start.waitForExistence(timeout: 5), "E opens the second occurrence")
        // Two hours away, inside the hours the week shows.
        let hour = Int(original.prefix(2)) ?? 12
        let new = String(format: "%02d:%@", hour >= 12 ? hour - 2 : hour + 2, String(original.suffix(2)))
        start.click()
        app.typeKey("a", modifierFlags: .command)
        start.typeText(new.replacingOccurrences(of: ":", with: ""))
        app.typeKey(.return, modifierFlags: .command)
        let prompt = app.descendants(matching: .any).matching(identifier: "scope-prompt").firstMatch
        XCTAssertTrue(prompt.waitForExistence(timeout: 5), "saving one occurrence asks which events")
        XCTAssertTrue(text(app, "Change a repeating event").exists)
        app.typeKey("1", modifierFlags: [])
        XCTAssertTrue(prompt.waitForNonExistence(timeout: 5), "1 picks This event")
        let moved = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Weekly sync, \(new) to")).firstMatch
        XCTAssertTrue(moved.waitForExistence(timeout: 5), "the second occurrence moved")
        XCTAssertFalse(atOriginal.firstMatch.exists, "and nothing else in its week")

        app.typeKey("k", modifierFlags: [])
        XCTAssertTrue(waitUntil { calendarTitle.label == firstWeek })
        XCTAssertTrue(atOriginal.firstMatch.waitForExistence(timeout: 5), "the first occurrence stays where it was")
        XCTAssertFalse(moved.exists)
        app.typeKey("j", modifierFlags: [])
        app.typeKey("j", modifierFlags: [])
        XCTAssertTrue(waitUntil { calendarTitle.label != firstWeek })
        XCTAssertTrue(atOriginal.firstMatch.waitForExistence(timeout: 5), "the third occurrence stays where it was")
        XCTAssertFalse(moved.exists)

        atOriginal.firstMatch.click()
        app.typeKey("e", modifierFlags: [])
        XCTAssertTrue(start.waitForExistence(timeout: 5), "E opens the third occurrence")
        app.typeKey(.delete, modifierFlags: .command)
        XCTAssertTrue(prompt.waitForExistence(timeout: 5), "deleting one occurrence asks which events")
        XCTAssertTrue(text(app, "Delete a repeating event").exists)
        app.typeKey("1", modifierFlags: [])
        XCTAssertTrue(prompt.waitForNonExistence(timeout: 5), "1 picks This event")
        XCTAssertTrue(atOriginal.firstMatch.waitForNonExistence(timeout: 5), "the third occurrence is gone")
        app.typeKey("k", modifierFlags: [])
        XCTAssertTrue(moved.waitForExistence(timeout: 5), "the moved occurrence stays")
    }

    /// A meeting the user organizes on a Google calendar: a person added in
    /// the editor, ⌘↩ and the scope prompt's 1, then Send an update? names
    /// the new person; Esc goes back to the editor, Return sends with the
    /// undo window and Z takes it back before anything goes out. ⌘⌫ asks to
    /// cancel the meeting, and Return keeps it.
    func testMeetingUpdate() throws {
        let app = try launch()
        app.typeKey("2", modifierFlags: .command)
        XCTAssertTrue(app.descendants(matching: .any).matching(identifier: "calendar-title").firstMatch.waitForExistence(timeout: 5), "⌘2 opens calendar mode")
        let meeting = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Team sync,")).firstMatch
        XCTAssertTrue(meeting.waitForExistence(timeout: 5), "the daily meeting is in the week")
        meeting.click()
        app.typeKey("e", modifierFlags: [])
        let people = app.textFields["editor-people"]
        XCTAssertTrue(people.waitForExistence(timeout: 5), "the organizer gets the People field")
        XCTAssertEqual(app.descendants(matching: .any).matching(identifier: "person-chip").count, 2, "a chip per guest, the user left out")
        people.click()
        people.typeText("ward@example.com\r")
        let chips = app.descendants(matching: .any).matching(identifier: "person-chip")
        XCTAssertTrue(waitUntil { chips.count == 3 }, "Return adds the typed address as a chip")
        XCTAssertTrue(app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Save and send")).firstMatch.waitForExistence(timeout: 5), "Save says it sends")

        app.typeKey(.return, modifierFlags: .command)
        let prompt = app.descendants(matching: .any).matching(identifier: "scope-prompt").firstMatch
        XCTAssertTrue(prompt.waitForExistence(timeout: 5), "one occurrence of a series asks which events first")
        app.typeKey("1", modifierFlags: [])
        let dialog = app.descendants(matching: .any).matching(identifier: "send-dialog").firstMatch
        XCTAssertTrue(dialog.waitForExistence(timeout: 5), "then Send an update?")
        XCTAssertTrue(text(app, "Invite 1 new person?").exists)
        XCTAssertTrue(text(app, "ward@example.com gets an invitation. The 2 others get nothing.").exists)
        app.typeKey(.escape, modifierFlags: [])
        XCTAssertTrue(dialog.waitForNonExistence(timeout: 5), "Esc keeps editing")
        XCTAssertTrue(people.exists, "with the editor still open")

        app.typeKey(.return, modifierFlags: .command)
        XCTAssertTrue(prompt.waitForExistence(timeout: 5))
        app.typeKey("1", modifierFlags: [])
        XCTAssertTrue(dialog.waitForExistence(timeout: 5))
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(dialog.waitForNonExistence(timeout: 5), "Return sends")
        XCTAssertTrue(text(app, "Update sent to 1 person").waitForExistence(timeout: 5), "the toast offers Undo while the update waits")
        app.typeKey("z", modifierFlags: [])
        XCTAssertTrue(text(app, "Undone").waitForExistence(timeout: 5), "Z takes it back before it goes")

        XCTAssertTrue(meeting.waitForExistence(timeout: 5))
        meeting.click()
        app.typeKey("e", modifierFlags: [])
        XCTAssertTrue(people.waitForExistence(timeout: 5))
        XCTAssertEqual(chips.count, 2, "the new person went with the undo")
        app.typeKey(.delete, modifierFlags: .command)
        XCTAssertTrue(prompt.waitForExistence(timeout: 5), "deleting one occurrence asks which events")
        app.typeKey("3", modifierFlags: [])
        let cancel = app.descendants(matching: .any).matching(identifier: "cancel-dialog").firstMatch
        XCTAssertTrue(cancel.waitForExistence(timeout: 5), "then whether to cancel the meeting")
        XCTAssertTrue(text(app, "Cancel the meeting for 2 people?").exists)
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(cancel.waitForNonExistence(timeout: 5), "Return keeps the meeting")
        XCTAssertTrue(meeting.exists, "and the meeting stays")
    }

    /// Editing on the grid. A drag over empty time opens the editor on that
    /// range; dragging the event moves it an hour and dragging its bottom
    /// edge makes it 30 minutes longer. Three ⌥↓ presses save once, so one Z
    /// takes all three back, and ⌥↑ moves it 15 minutes earlier. ⌥-drag
    /// copies an event without its meeting link. Dragging a meeting with
    /// guests asks which events, then Send an update?, where Return sends.
    func testGridDragAndKeys() throws {
        let app = try launch()
        app.typeKey("2", modifierFlags: .command)
        XCTAssertTrue(app.descendants(matching: .any).matching(identifier: "calendar-title").firstMatch.waitForExistence(timeout: 5), "⌘2 opens calendar mode")
        // The view is remembered per test folder, and this journey ends in month view.
        app.typeKey("w", modifierFlags: [])
        let event = { (label: String) in app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", label)).firstMatch }
        let standup = event("Standup, 9:30 to 9:45")
        XCTAssertTrue(standup.waitForExistence(timeout: 5), "Monday's standup marks the column and the hours")
        // The top of an event's box is a point below its start's line.
        let nineThirty = standup.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0)).withOffset(CGVector(dx: 0, dy: -1))
        let at = { (hour: Double, plus: Double) in nineThirty.withOffset(CGVector(dx: 0, dy: (hour - 9.5) * 48 + plus)) }

        at(10, 2).click(forDuration: 0.1, thenDragTo: at(11, 2))
        let title = app.textFields["editor-title"]
        XCTAssertTrue(title.waitForExistence(timeout: 5), "a drag over empty time opens the editor")
        XCTAssertEqual(app.textFields["editor-start"].value as? String, "10:00", "on the range dragged")
        XCTAssertEqual(app.textFields["editor-end"].value as? String, "11:00")
        title.typeText("Grid check")
        app.typeKey(.return, modifierFlags: .command)
        XCTAssertTrue(event("Grid check, 10:00 to 11:00").waitForExistence(timeout: 5), "⌘↩ saves it")

        at(10.5, 0).click(forDuration: 0.1, thenDragTo: at(11.5, 0))
        XCTAssertTrue(event("Grid check, 11:00 to 12:00").waitForExistence(timeout: 5), "dragging it moves it an hour")
        XCTAssertTrue(text(app, "Saved Grid check").waitForExistence(timeout: 5), "with Undo in the toast")

        at(12, -2.5).click(forDuration: 0.1, thenDragTo: at(12.5, -2.5))
        let longer = event("Grid check, 11:00 to 12:30")
        XCTAssertTrue(longer.waitForExistence(timeout: 5), "dragging its bottom edge makes it 30 minutes longer")

        longer.click()
        for _ in 0..<3 { app.typeKey(.downArrow, modifierFlags: .option) }
        XCTAssertTrue(event("Grid check, 11:45 to 13:15").waitForExistence(timeout: 5), "three ⌥↓ move it 45 minutes")
        app.typeKey("z", modifierFlags: [])
        XCTAssertTrue(longer.waitForExistence(timeout: 5), "and were one save: one Z takes all three back")

        longer.click()
        app.typeKey(.upArrow, modifierFlags: .option)
        let earlier = event("Grid check, 10:45 to 12:15")
        XCTAssertTrue(earlier.waitForExistence(timeout: 5), "⌥↑ moves it 15 minutes earlier")

        earlier.click()
        app.typeKey(.downArrow, modifierFlags: .shift)
        app.typeKey(.downArrow, modifierFlags: .shift)
        XCTAssertTrue(event("Grid check, 10:45 to 12:45").waitForExistence(timeout: 5), "two ⇧↓ make it 30 minutes longer")
        app.typeKey("z", modifierFlags: [])
        XCTAssertTrue(earlier.waitForExistence(timeout: 5))
        earlier.click()
        app.typeKey(.downArrow, modifierFlags: [])
        RunLoop.current.run(until: Date() + 1.5)
        XCTAssertTrue(earlier.exists, "↓ moves the selection, never the event")

        let call = event("1:1 Els, 11:00 to 12:00")
        XCTAssertTrue(call.waitForExistence(timeout: 5))
        XCTAssertTrue(call.label.contains("Google Meet"), "the original has a meeting link: \(call.label)")
        let from = call.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        XCUIElement.perform(withKeyModifiers: .option) {
            from.click(forDuration: 0.1, thenDragTo: from.withOffset(CGVector(dx: 0, dy: 4 * 48)))
        }
        let copy = event("1:1 Els, 15:00 to 16:00")
        XCTAssertTrue(copy.waitForExistence(timeout: 5), "⌥-drag copies it four hours later")
        XCTAssertFalse(copy.label.contains("Google Meet"), "without the meeting link: \(copy.label)")
        XCTAssertTrue(call.exists, "and the original stays")

        let sync = event("Team sync,")
        XCTAssertTrue(sync.waitForExistence(timeout: 5), "the daily meeting with guests is in the week")
        let held = sync.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        held.click(forDuration: 0.1, thenDragTo: held.withOffset(CGVector(dx: 0, dy: 48)))
        let prompt = app.descendants(matching: .any).matching(identifier: "scope-prompt").firstMatch
        XCTAssertTrue(prompt.waitForExistence(timeout: 5), "a dragged occurrence asks which events first")
        app.typeKey("1", modifierFlags: [])
        let dialog = app.descendants(matching: .any).matching(identifier: "send-dialog").firstMatch
        XCTAssertTrue(dialog.waitForExistence(timeout: 5), "then Send an update?, never a toast alone")
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(dialog.waitForNonExistence(timeout: 5), "Return sends")
        XCTAssertTrue(text(app, "Update sent to 2 people").waitForExistence(timeout: 5), "the toast offers Undo while the update waits")
        app.typeKey("z", modifierFlags: [])
        XCTAssertTrue(text(app, "Undone").waitForExistence(timeout: 5), "Z takes it back before it goes")

        let birthday = event("Birthday: Mum")
        XCTAssertTrue(birthday.waitForExistence(timeout: 5), "Sunday's all-day event is in the row above the hours")
        let sunday = birthday.frame
        let lane = birthday.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        lane.click(forDuration: 0.1, thenDragTo: lane.withOffset(CGVector(dx: -sunday.width - 6, dy: 0)))
        XCTAssertTrue(prompt.waitForExistence(timeout: 5), "an occurrence of a series asks which events")
        app.typeKey("1", modifierFlags: [])
        XCTAssertTrue(text(app, "Saved Birthday: Mum").waitForExistence(timeout: 5))
        XCTAssertTrue(waitUntil { birthday.frame.minX < sunday.minX - 50 }, "dragging it a column left moves it to Saturday")
        app.typeKey("z", modifierFlags: [])
        XCTAssertTrue(waitUntil { abs(birthday.frame.minX - sunday.minX) < 2 }, "Z puts it back")

        app.typeKey("m", modifierFlags: [])
        let line = event("Birthday: Mum")
        XCTAssertTrue(line.waitForExistence(timeout: 5), "month view lists it")
        let day = line.frame
        let cell = line.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        cell.click(forDuration: 0.1, thenDragTo: cell.withOffset(CGVector(dx: -day.width, dy: 0)))
        XCTAssertTrue(prompt.waitForExistence(timeout: 5))
        app.typeKey("1", modifierFlags: [])
        XCTAssertTrue(waitUntil { line.frame.minX < day.minX - 50 }, "month view moves it a day earlier too")
    }

    /// A meeting in a calendar on this Mac, whose invitations Jolta mails
    /// itself: the first save asks once which account sends them, Return
    /// takes the first, and Send invitations? names it and has a message
    /// field; Return sends after the undo window and Z takes it back. The
    /// next meeting goes straight to Send invitations?.
    func testLocalMeetingSendsItself() throws {
        let app = try launch()
        app.typeKey("2", modifierFlags: .command)
        XCTAssertTrue(app.descendants(matching: .any).matching(identifier: "calendar-title").firstMatch.waitForExistence(timeout: 5), "⌘2 opens calendar mode")
        let meeting = { (name: String) in
            app.typeKey("c", modifierFlags: [])
            let title = app.textFields["editor-title"]
            XCTAssertTrue(title.waitForExistence(timeout: 5), "C opens the editor on a calendar on this Mac")
            title.typeText(name)
            let people = app.textFields["editor-people"]
            people.click()
            people.typeText("ward@example.com\r")
            XCTAssertTrue(app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Save and send")).firstMatch.waitForExistence(timeout: 5), "Save says it sends")
            app.typeKey(.return, modifierFlags: .command)
        }
        let sender = app.descendants(matching: .any).matching(identifier: "sender-dialog").firstMatch
        let dialog = app.descendants(matching: .any).matching(identifier: "send-dialog").firstMatch

        meeting("Studio review")
        XCTAssertTrue(sender.waitForExistence(timeout: 5), "the calendar asks which account sends its invitations")
        XCTAssertTrue(text(app, "Send invitations from").exists)
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(dialog.waitForExistence(timeout: 5), "then Send invitations?")
        XCTAssertTrue(text(app, "Send invitations to 1 person?").exists)
        XCTAssertTrue(text(app, "ward@example.com gets an invitation. Jolta sends them from me@test.invalid.").exists, "it names the account picked")
        XCTAssertTrue(app.textFields["send-message"].exists, "Jolta's own mail takes a message")
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(dialog.waitForNonExistence(timeout: 5), "Return sends")
        XCTAssertTrue(text(app, "Invitation sent to 1 person").waitForExistence(timeout: 5), "the toast offers Undo while the invitation waits")
        app.typeKey("z", modifierFlags: [])
        XCTAssertTrue(text(app, "Undone").waitForExistence(timeout: 5), "Z takes it back before it goes")

        meeting("Studio review again")
        XCTAssertTrue(dialog.waitForExistence(timeout: 5), "the account is asked once per calendar")
        XCTAssertFalse(sender.exists)
        app.typeKey(.escape, modifierFlags: [])
        XCTAssertTrue(dialog.waitForNonExistence(timeout: 5), "Esc keeps editing")
        XCTAssertTrue(app.textFields["editor-people"].exists, "with the editor still open")
    }

    /// Settings › Calendar makes a calendar on this Mac, which the calendar
    /// sidebar then lists with the others.
    func testNewCalendarOnThisMac() throws {
        let app = try launch()
        app.typeKey(",", modifierFlags: .command)
        let page = app.buttons.matching(NSPredicate(format: "label == 'Calendar' AND identifier != 'mode-calendar'")).firstMatch
        XCTAssertTrue(page.waitForExistence(timeout: 5), "settings lists the Calendar page")
        page.click()
        let name = app.textFields.matching(identifier: "new-calendar-name").firstMatch
        XCTAssertTrue(name.waitForExistence(timeout: 5), "the page offers a new calendar on this Mac")
        name.click()
        name.typeText("Family")
        app.buttons["Create"].firstMatch.click()
        XCTAssertTrue(app.staticTexts["Made Family."].firstMatch.waitForExistence(timeout: 5), "Create makes it")
        app.typeKey("w", modifierFlags: .command)
        app.typeKey("2", modifierFlags: .command)
        let family = app.descendants(matching: .any).matching(identifier: "calendar-Family").firstMatch
        XCTAssertTrue(family.waitForExistence(timeout: 5), "the calendar sidebar lists it")
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

    /// Quick create: Q opens the field, a line shows the event it makes,
    /// Return opens the editor on it and ⌘↩ saves it. A name nobody was
    /// written to waits as a chip and Save stays off. G goes to a typed day.
    func testQuickCreateAndGoToDate() throws {
        let app = try launch()
        app.typeKey("2", modifierFlags: .command)
        let calendarTitle = app.descendants(matching: .any).matching(identifier: "calendar-title").firstMatch
        XCTAssertTrue(calendarTitle.waitForExistence(timeout: 5), "⌘2 opens calendar mode")
        app.typeKey("w", modifierFlags: [])

        app.typeKey("q", modifierFlags: [])
        let field = app.textFields["quick-create-field"]
        XCTAssertTrue(field.waitForExistence(timeout: 5), "Q opens quick create")
        field.typeText("Journey lunch tomorrow 12:30 at Mok")
        let preview = app.descendants(matching: .any).matching(identifier: "quick-create-preview").firstMatch
        XCTAssertTrue(preview.waitForExistence(timeout: 5), "the line shows the event it would make")
        app.typeKey(.return, modifierFlags: [])
        let title = app.textFields["editor-title"]
        XCTAssertTrue(title.waitForExistence(timeout: 10), "Return opens the editor")
        XCTAssertEqual(title.value as? String, "Journey lunch")
        XCTAssertEqual(app.textFields["editor-start"].value as? String, "12:30")
        XCTAssertEqual(app.textFields["editor-end"].value as? String, "13:00", "30 minutes, the default length")
        XCTAssertEqual(app.textFields["editor-location"].value as? String, "Mok")
        app.typeKey(.return, modifierFlags: .command)
        let saved = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Journey lunch, 12:30 to 13:00")).firstMatch
        XCTAssertTrue(saved.waitForExistence(timeout: 5), "⌘↩ saves it into the week")

        app.typeKey("q", modifierFlags: [])
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.typeText("Sync with Nobody tomorrow 9am")
        app.typeKey(.return, modifierFlags: [])
        let chip = app.descendants(matching: .any).matching(identifier: "unresolved-chip").firstMatch
        XCTAssertTrue(chip.waitForExistence(timeout: 10), "a name nobody was written to waits as a chip")
        XCTAssertFalse(app.buttons["editor-save"].isEnabled, "and nothing saves until it is picked or removed")
        app.typeKey(.escape, modifierFlags: [])
        let discard = app.buttons["Discard"]
        XCTAssertTrue(discard.waitForExistence(timeout: 5))
        discard.click()
        XCTAssertTrue(chip.waitForNonExistence(timeout: 5))

        app.typeKey("g", modifierFlags: [])
        let day = app.textFields["go-to-date-field"]
        XCTAssertTrue(day.waitForExistence(timeout: 5), "G opens go to date")
        day.typeText("2027-01-04")
        XCTAssertTrue(app.descendants(matching: .any).matching(identifier: "go-to-date-day").firstMatch.waitForExistence(timeout: 5), "it reads the day")
        app.typeKey(.return, modifierFlags: [])
        XCTAssertTrue(waitUntil { calendarTitle.label.hasPrefix("January 2027") }, "Return shows that week: \(calendarTitle.label)")
    }

    /// Y, N and ⇧M answer the invitation selected in the week (decision 8:
    /// M stays Month). The fixture's Google calendar has no server, so each
    /// answer goes to Google and stops at the missing sign-in; the toast
    /// that says so shows the key reached the answer. Keys on an event
    /// that isn't an invitation answer nothing.
    func testAnswerInvitationInCalendar() throws {
        let app = try launch()
        app.typeKey("2", modifierFlags: .command)
        let title = app.descendants(matching: .any).matching(identifier: "calendar-title").firstMatch
        XCTAssertTrue(title.waitForExistence(timeout: 5), "⌘2 opens calendar mode")
        app.typeKey("w", modifierFlags: [])
        let event = { (label: String) in app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", label)).firstMatch }
        let refused = app.staticTexts.matching(NSPredicate(format: "value CONTAINS %@ OR label CONTAINS %@", "credential", "credential")).firstMatch

        let climbing = event("Climbing,")
        XCTAssertTrue(climbing.waitForExistence(timeout: 5), "the week shows the user's own Climbing")
        climbing.click()
        app.typeKey("y", modifierFlags: [])
        XCTAssertFalse(refused.waitForExistence(timeout: 2), "Y on an event that isn't an invitation answers nothing")

        let review = event("Design review,")
        XCTAssertTrue(review.waitForExistence(timeout: 5), "the week shows Els's Design review")
        review.click()
        for (key, flags, name) in [("n", XCUIElement.KeyModifierFlags(), "N"), ("m", .shift, "⇧M"), ("y", [], "Y")] {
            app.typeKey(key, modifierFlags: flags)
            XCTAssertTrue(refused.waitForExistence(timeout: 5), "\(name) answers the selected invitation through Google")
            XCTAssertTrue(refused.waitForNonExistence(timeout: 15), "the toast goes")
        }
        XCTAssertTrue(title.label.contains("Week"), "⇧M doesn't switch to Month: \(title.label)")
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
