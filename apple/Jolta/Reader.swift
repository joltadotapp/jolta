@preconcurrency import JoltaFFI
import AppKit
import SwiftUI
import WebKit

/// The reader's web view, locked down in independent layers: the core's
/// sanitizer and CSP, JavaScript off, and a content rule list that blocks
/// everything but local schemes. Images come only through Jolta's own `cid:`
/// and `x-remote:` handlers; remote ones load after the user asks. Every
/// navigation after the initial load is cancelled; a clicked link opens in
/// the browser only after the user confirms its real destination.
@MainActor
final class ReaderPage {
    let page: WebPage
    let images = ImageSource()
    /// Called when the user clicks an attachment chip in the message.
    var onAttachment: ((Int64, UInt32) -> Void)? {
        get { links.onAttachment }
        set { links.onAttachment = newValue }
    }
    /// Called for Save all under a message's attachments.
    var onSaveAll: ((Int64) -> Void)? {
        get { links.onSaveAll }
        set { links.onSaveAll = newValue }
    }
    /// Called for an invitation card's `x-rsvp:<message>/<action>` links.
    var onRsvp: ((Int64, String) -> Void)? {
        get { links.onRsvp }
        set { links.onRsvp = newValue }
    }
    /// Called when a message is expanded (true) or collapsed.
    var onToggle: ((Int64, Bool) -> Void)? {
        get { links.onToggle }
        set { links.onToggle = newValue }
    }
    private let links = LinkActions()
    private static var ruleList: WKContentRuleList?

    init() {
        var config = WebPage.Configuration()
        config.websiteDataStore = .nonPersistent()
        config.defaultNavigationPreferences.allowsContentJavaScript = false
        config.urlSchemeHandlers[URLScheme("cid")!] = InlineImages(source: images)
        config.urlSchemeHandlers[URLScheme("x-remote")!] = RemoteImages(source: images)
        if let rules = Self.ruleList {
            config.userContentController.add(rules)
        }
        // In the app's own script world, which runs with the page's
        // JavaScript off: every expand and collapse of a message, by its id.
        let toggles = """
        document.addEventListener("toggle", (e) => {
          const body = e.target.matches?.("details.jolta-message") && e.target.querySelector(":scope > .jolta-body");
          if (body) webkit.messageHandlers.toggle.postMessage({ id: body.id.slice(1), open: e.target.open });
        }, true);
        """
        config.userContentController.addUserScript(WKUserScript(source: toggles, injectionTime: .atDocumentStart, forMainFrameOnly: true, in: .defaultClient))
        config.userContentController.add(ToggleMessages(links: links), contentWorld: .defaultClient, name: "toggle")
        page = WebPage(configuration: config, navigationDecider: Decider(links: links))
    }

    /// Compiles the block-all rule list once, before the first page is built.
    static func prepare() async {
        let allowed = ["about", "cid", "data", "x-remote"].map {
            #"{"trigger":{"url-filter":"^\#($0):"},"action":{"type":"ignore-previous-rules"}}"#
        }
        // Every load except the message document itself; frames inside it are
        // cancelled by the navigation decider and the CSP.
        let block = #"{"trigger":{"url-filter":".*","resource-type":["image","style-sheet","script","font","raw","svg-document","media","popup","ping","fetch","websocket","other"]},"action":{"type":"block"}}"#
        let rules = "[" + ([block] + allowed).joined(separator: ",") + "]"
        do {
            ruleList = try await WKContentRuleListStore.default().compileContentRuleList(
                forIdentifier: "jolta-block-all", encodedContentRuleList: rules)
        } catch {
            NSLog("Jolta: content rule list failed to compile: \(error)")
        }
    }

    /// Loads a rendered conversation; `then` runs once it has loaded.
    func show(html: String, then done: (@MainActor () -> Void)? = nil) {
        var html = images.allowsRemote ? html.replacingOccurrences(of: "<body>", with: "<body class=\"jolta-images\">") : html
        // Here rather than in the core's `appCss`: a page loads again, with
        // images, from HTML rendered before the theme last changed.
        if let head = html.range(of: "</head>") {
            html.insert(contentsOf: "<style id=\"jolta-theme\">\(ReaderStyle.themeCSS)</style>", at: head.lowerBound)
        }
        let events = page.load(html: html, baseURL: URL(string: "about:blank")!)
        let debug = UserDefaults.standard.bool(forKey: "JoltaReaderDebug")
        guard debug || done != nil else { return }
        Task {
            var log = "html \(html.count) chars\n"
            do {
                for try await event in events { log += "event \(event)\n" }
            } catch {
                log += "load error \(error)\n"
            }
            done?()
            guard debug else { return }
            let probe = try? await page.callJavaScript("return [document.body ? document.body.innerText.length : -1, getComputedStyle(document.body).color, innerWidth, innerHeight].join(' ')")
            log += "probe \(String(describing: probe))\nurl \(String(describing: page.url))\n"
            try? log.write(toFile: AppModel.dataDirectory + "/readerdebug.txt", atomically: true, encoding: .utf8)
            try? html.write(toFile: AppModel.dataDirectory + "/readerdebug.html", atomically: true, encoding: .utf8)
        }
    }

    /// Expands or collapses messages by id, as the user left them before
    /// the page loaded again.
    func expand(_ states: [Int64: Bool]) {
        let script = """
        for (const [id, open] of Object.entries(states)) {
          const details = document.getElementById("m" + id)?.closest("details.jolta-message");
          if (details) details.open = open;
        }
        """
        let states = Dictionary(uniqueKeysWithValues: states.map { (String($0.key), $0.value) })
        Task { _ = try? await page.callJavaScript(script, arguments: ["states": states]) }
    }

    /// Swaps the theme's colors into the page on screen. The app's own
    /// scripts run even with the page's JavaScript off.
    func applyTheme() {
        let script = """
        const style = document.getElementById("jolta-theme") ?? document.createElement("style");
        style.id = "jolta-theme";
        style.textContent = css;
        document.head.appendChild(style);
        """
        Task { _ = try? await page.callJavaScript(script, arguments: ["css": ReaderStyle.themeCSS]) }
    }
}

/// What the image handlers may serve for the page on screen.
final class ImageSource: @unchecked Sendable {
    var core: Core?
    /// Set when the user asks to load this conversation's remote images.
    var allowsRemote = false
}

private func imageReply(_ url: URL?, mime: String, data: Data) -> AsyncThrowingStream<URLSchemeTaskResult, any Error> {
    AsyncThrowingStream { continuation in
        let response = URLResponse(url: url ?? URL(string: "about:blank")!, mimeType: mime, expectedContentLength: data.count, textEncodingName: nil)
        continuation.yield(.response(response))
        continuation.yield(.data(data))
        continuation.finish()
    }
}

/// `cid:<message id>/<content id>`: an image part of the message itself.
private struct InlineImages: URLSchemeHandler {
    let source: ImageSource

    func reply(for request: URLRequest) -> AsyncThrowingStream<URLSchemeTaskResult, any Error> {
        let url = request.url
        let path = url?.absoluteString.dropFirst("cid:".count) ?? ""
        guard let core = source.core, let slash = path.firstIndex(of: "/"), let message = Int64(path[..<slash]) else {
            return imageReply(url, mime: "image/gif", data: Data())
        }
        let cid = String(path[path.index(after: slash)...]).removingPercentEncoding ?? String(path[path.index(after: slash)...])
        return AsyncThrowingStream { continuation in
            Task {
                let part = try? await core.inlineImage(messageId: message, contentId: cid)
                let response = URLResponse(url: url ?? URL(string: "about:blank")!, mimeType: part?.mimeType ?? "image/gif",
                                           expectedContentLength: part?.bytes.count ?? 0, textEncodingName: nil)
                continuation.yield(.response(response))
                continuation.yield(.data(part.map { Data($0.bytes) } ?? Data()))
                continuation.finish()
            }
        }
    }
}

/// `x-remote:<http URL>`: nothing until the user loads images, then a
/// cookieless, cacheless fetch that sends no referrer.
private struct RemoteImages: URLSchemeHandler {
    let source: ImageSource
    private static let session = URLSession(configuration: {
        let c = URLSessionConfiguration.ephemeral
        c.httpCookieStorage = nil
        c.urlCache = nil
        c.httpShouldSetCookies = false
        c.timeoutIntervalForRequest = 20
        return c
    }())

    func reply(for request: URLRequest) -> AsyncThrowingStream<URLSchemeTaskResult, any Error> {
        let url = request.url
        let target = url.flatMap { URL(string: String($0.absoluteString.dropFirst("x-remote:".count))) }
        guard source.allowsRemote, let target, ["http", "https"].contains(target.scheme?.lowercased() ?? "") else {
            return imageReply(url, mime: "image/gif", data: Data())
        }
        return AsyncThrowingStream { continuation in
            Task {
                let fetched = try? await Self.session.data(from: target)
                let mime = (fetched?.1 as? HTTPURLResponse)?.mimeType ?? "image/gif"
                let data = fetched?.0 ?? Data()
                let response = URLResponse(url: url ?? target, mimeType: mime, expectedContentLength: data.count, textEncodingName: nil)
                continuation.yield(.response(response))
                continuation.yield(.data(data))
                continuation.finish()
            }
        }
    }
}

@MainActor
private final class LinkActions {
    var onAttachment: ((Int64, UInt32) -> Void)?
    var onSaveAll: ((Int64) -> Void)?
    var onRsvp: ((Int64, String) -> Void)?
    var onToggle: ((Int64, Bool) -> Void)?
}

/// The toggle script's messages: `{id, open}` for one message.
private final class ToggleMessages: NSObject, WKScriptMessageHandler {
    let links: LinkActions

    init(links: LinkActions) {
        self.links = links
    }

    func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let body = message.body as? [String: Any], let id = (body["id"] as? String).flatMap(Int64.init), let open = body["open"] as? Bool else { return }
        MainActor.assumeIsolated { links.onToggle?(id, open) }
    }
}

@MainActor
private struct Decider: WebPage.NavigationDeciding {
    let links: LinkActions

    func decidePolicy(for action: WebPage.NavigationAction, preferences: inout WebPage.NavigationPreferences) async -> WKNavigationActionPolicy {
        if #available(macOS 26.4, *) {
            preferences.securityRestrictionMode = .maximizeCompatibility
        }
        preferences.allowsContentJavaScript = false
        let url = action.request.url
        if action.navigationType == .other, url?.absoluteString == "about:blank" {
            return .allow
        }
        if action.navigationType == .linkActivated, let url, url.scheme == "x-attachment" {
            let parts = url.absoluteString.dropFirst("x-attachment:".count).split(separator: "/")
            if parts.count == 2, let message = Int64(parts[0]) {
                if parts[1] == "all" {
                    links.onSaveAll?(message)
                } else if let index = UInt32(parts[1]) {
                    links.onAttachment?(message, index)
                }
            }
            return .cancel
        }
        if action.navigationType == .linkActivated, let url, url.scheme == "x-rsvp" {
            let parts = url.absoluteString.dropFirst("x-rsvp:".count).split(separator: "/")
            if parts.count == 2, let message = Int64(parts[0]) {
                links.onRsvp?(message, String(parts[1]))
            }
            return .cancel
        }
        if action.navigationType == .linkActivated, let url, url.scheme == "x-join" {
            Self.join(url)
            return .cancel
        }
        if action.navigationType == .linkActivated, let url, ["http", "https", "mailto"].contains(url.scheme ?? "") {
            Self.confirmOpen(url)
        }
        return .cancel
    }

    /// `x-join:open?url=…&app=…` from an invitation card: the meeting's app
    /// when it is installed, else its https link, as the calendar's Join.
    static func join(_ url: URL) {
        let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
        let link = { (name: String) in items.first { $0.name == name }?.value.flatMap(URL.init(string:)) }
        if let app = link("app"), NSWorkspace.shared.urlForApplication(toOpen: app) != nil {
            NSWorkspace.shared.open(app)
        } else if let web = link("url"), web.scheme == "https" {
            NSWorkspace.shared.open(web)
        }
    }

    static func confirmOpen(_ url: URL) {
        let alert = NSAlert()
        alert.messageText = "Open this link?"
        alert.informativeText = url.absoluteString
        alert.addButton(withTitle: "Open")
        alert.addButton(withTitle: "Cancel")
        if alert.runModal() == .alertFirstButtonReturn {
            NSWorkspace.shared.open(url)
        }
    }
}

struct ReaderView: View {
    let thread: ThreadView
    let model: AppModel
    var core: Core { model.core }
    let onAttachment: (Int64, UInt32) -> Void
    var onSaveAll: (Int64) -> Void = { _ in }
    var onUnsubscribe: () -> Void = {}
    @State private var reader = ReaderPage()
    @State private var imagesLoaded = false
    /// Set when the user trusts or stops trusting the sender in this view.
    @State private var trustOverride: Bool?
    @State private var shownThread: Int64?

    private var trusted: Bool { trustOverride ?? thread.imagesTrusted }
    private var imagesBlocked: Bool { thread.remoteImages > 0 && !imagesLoaded && !trusted }

    private func render() {
        if shownThread != thread.threadId {
            shownThread = thread.threadId
            imagesLoaded = false
            trustOverride = nil
        }
        reader.images.allowsRemote = imagesLoaded || trusted
        let expanded = model.expandedMessages
        reader.show(html: thread.html) {
            if !expanded.isEmpty { reader.expand(expanded) }
            if model.answerChoices != AnswerChoices() || model.pendingAnswer != nil { reflect() }
        }
    }

    /// The open card's choices and pending answer, marked on the page.
    private func reflect() {
        guard let invitation = model.activeInvitation else { return }
        let chosen = model.pendingAnswer ?? invitation.answer?.invitationAnswer
        reader.reflect(message: invitation.messageId, chosen: chosen, sending: model.pendingAnswer != nil,
                       choices: model.answerChoices, day: invitation.occurrenceDay)
    }

    private func rsvp(_ message: Int64, _ action: String) {
        model.rsvpMessage = message
        if let answer = InvitationAnswer(name: action) {
            return model.answerInvitation(answer, message: message)
        }
        switch action {
        case "remove":
            model.removeCancelled(message: message)
        case "update":
            model.updateAddedCopy(message: message)
        case "add":
            if case .calendars = model.rsvpPanel { model.rsvpPanel = nil; return }
            Task {
                guard let frame = await reader.frame(of: ".jolta-add", message: message) else { return }
                model.openAddMenu(message: message, at: frame)
            }
        case "quiet":
            model.rsvpPanel = nil
            model.answerChoices.quiet.toggle()
        case "decline-proposal":
            model.declineProposal(message: message)
        case "note", "scope", "propose":
            if case .note = model.rsvpPanel, action == "note" { model.rsvpPanel = nil; return }
            if case .scope = model.rsvpPanel, action == "scope" { model.rsvpPanel = nil; return }
            if case .propose = model.rsvpPanel, action == "propose" { model.rsvpPanel = nil; return }
            Task {
                guard let frame = await reader.frame(of: ".jolta-rsvp-\(action)", message: message) else { return }
                model.rsvpPanel = switch action {
                case "note": .note(frame)
                case "scope": .scope(frame)
                default: .propose(frame)
                }
            }
        default: break
        }
    }

    @ViewBuilder private var rsvpPanel: some View {
        if let panel = model.rsvpPanel, let invitation = model.activeInvitation {
            let anchor = panel.anchor
            Group {
                switch panel {
                case .note: RsvpNotePanel(model: model, invitation: invitation)
                case .scope: RsvpScopePanel(model: model, day: invitation.occurrenceDay ?? "this one")
                case .calendars: AddCalendarPanel(model: model, message: model.rsvpMessage ?? invitation.messageId)
                case .propose:
                    if let slot = invitation.newTime { RsvpProposePanel(model: model, invitation: invitation, slot: slot) }
                }
            }
            .offset(x: max(8, panel.left), y: anchor.maxY + 6)
            .transition(.opacity.combined(with: .scale(scale: 0.97, anchor: .topLeading)))
        }
    }

    private func loadImages() {
        imagesLoaded = true
        render()
    }

    private func trustSender(_ trust: Bool) {
        guard !thread.imageSender.isEmpty else { return }
        try? core.setSenderTrusted(address: thread.imageSender, trusted: trust)
        trustOverride = trust
        imagesLoaded = trust
        render()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 6) {
                Text(thread.subject.isEmpty ? "(no subject)" : thread.subject)
                    .font(.system(size: 21, weight: .semibold))
                    .tracking(-0.3)
                    .foregroundStyle(Color(Theme.text))
                    .textSelection(.enabled)
                    .frame(maxWidth: 560, alignment: .leading)
                if thread.messageCount > 1 || thread.mailingList {
                    HStack(spacing: 14) {
                        if thread.messageCount > 1 {
                            Text("\(thread.messageCount) messages").foregroundStyle(Color(Theme.text3))
                        }
                        if thread.mailingList {
                            Button(action: onUnsubscribe) {
                                HStack(spacing: 6) {
                                    Text("Unsubscribe")
                                    Text("⌘U").font(.system(size: 11)).foregroundStyle(Color(Theme.text3))
                                }
                            }
                            .buttonStyle(.plain)
                            .foregroundStyle(Color(Theme.text2))
                            .help("Leave this mailing list and archive the conversation")
                        }
                    }
                    .font(.system(size: 12.5))
                }
                if imagesBlocked {
                    HStack(spacing: 10) {
                        Image(systemName: "photo.badge.exclamationmark").foregroundStyle(Color(Theme.text3))
                        Text(thread.remoteImages == 1 ? "1 remote image blocked" : "\(thread.remoteImages) remote images blocked")
                            .foregroundStyle(Color(Theme.text2))
                            .fixedSize()
                            .help("Jolta blocks remote images so senders can't see when you read their mail.")
                        Button("Load Images", action: loadImages)
                            .buttonStyle(.plain)
                            .fontWeight(.medium)
                            .foregroundStyle(Color(Theme.accent))
                        if !thread.imageSender.isEmpty {
                            Button("Always from \(thread.imageSender)") { trustSender(true) }
                                .buttonStyle(.plain)
                                .foregroundStyle(Color(Theme.text2))
                                .lineLimit(1)
                                .truncationMode(.middle)
                                .help("Load remote images from this sender without asking")
                        }
                    }
                    .font(.system(size: 12.5))
                    .padding(.top, 8)
                }
            }
            .padding(.horizontal, 56)
            .padding(.top, 30)
            .padding(.bottom, 16)
            WebView(reader.page)
                .webViewBackForwardNavigationGestures(.disabled)
                .webViewMagnificationGestures(.disabled)
                .webViewLinkPreviews(.disabled)
                .overlay(alignment: .topLeading) { rsvpPanel }
                .webViewContextMenu { element in
                    if let link = element.linkURL {
                        Button("Open Link…") { Decider.confirmOpen(link) }
                        Button("Copy Link") {
                            NSPasteboard.general.clearContents()
                            NSPasteboard.general.setString(link.absoluteString, forType: .string)
                        }
                        Divider()
                    }
                    Button("Copy") { NSApp.sendAction(#selector(NSText.copy(_:)), to: nil, from: nil) }
                    if imagesBlocked {
                        Button("Load Images", action: loadImages)
                        if !thread.imageSender.isEmpty {
                            Button("Always Load Images from \(thread.imageSender)") { trustSender(true) }
                        }
                    } else if trusted, !thread.imageSender.isEmpty {
                        Button("Stop Loading Images from \(thread.imageSender)") { trustSender(false) }
                    }
                }
        }
        .animation(.snappy(duration: 0.16), value: model.rsvpPanel)
        .onChange(of: model.answerChoices) { reflect() }
        .onChange(of: model.pendingAnswer) { reflect() }
        .onChange(of: model.pendingRemoval) { old, new in
            if let message = new ?? old { reader.markSending(message: message, on: new != nil) }
        }
        .onChange(of: model.pendingDecline) { old, new in
            if let message = new ?? old { reader.markSending(message: message, on: new != nil) }
        }
        .onChange(of: model.proposeFor) { _, new in
            guard let new else { return }
            model.proposeFor = nil
            rsvp(new, "propose")
        }
        .onAppear {
            reader.onAttachment = onAttachment
            reader.onSaveAll = onSaveAll
            reader.onRsvp = { rsvp($0, $1) }
            reader.onToggle = { model.expandedMessages[$0] = $1 }
            reader.images.core = core
            render()
            // `-JoltaLoadImages YES` loads remote images at once, for checking from scripts.
            if UserDefaults.standard.bool(forKey: "JoltaLoadImages"), imagesBlocked { loadImages() }
        }
        .onChange(of: thread.html) { render() }
        .onChange(of: ThemeStore.shared.active) { reader.applyTheme() }
    }
}

enum ReaderStyle {
    /// The variables `css` uses, from the light and the dark theme on screen.
    static var themeCSS: String {
        let pair = ThemeStore.shared.active
        return ":root { \(variables(pair.light)) } @media (prefers-color-scheme: dark) { :root { \(variables(pair.dark)) } }"
    }

    private static func variables(_ t: ThemeTokens) -> String {
        [("bg", t.reader), ("tx", t.text), ("tx2", t.text2), ("tx3", t.text3), ("line2", t.line2), ("fill", t.fill), ("acc", t.accent)]
            .map { "--\($0.0):#\(String(format: "%06x", $0.1));" }
            .joined(separator: " ")
    }

    /// Styles for the page around each message. Message HTML keeps its own
    /// look inside `.jolta-body`.
    static let css = """
    :root { color-scheme: light dark; }
    html, body { margin: 0; padding: 0; background: var(--bg); color: var(--tx);
      font: 14px/22px -apple-system, system-ui; -webkit-font-smoothing: antialiased; }
    body { padding: 0 clamp(16px, 5vw, 56px) 40px; }
    ::selection { background: color-mix(in oklab, var(--acc) 30%, transparent); }
    .jolta-message { border-top: 1px solid var(--line2); }
    .jolta-message > summary { list-style: none; cursor: default; display: flex; gap: 14px; align-items: center;
      height: 42px; white-space: nowrap; font-size: 13px; }
    .jolta-message > summary::-webkit-details-marker { display: none; }
    .jolta-from { font-weight: 600; width: 120px; flex: none; overflow: hidden; text-overflow: ellipsis; }
    .jolta-vip { flex: none; margin-left: -8px; padding: 0 5px; border-radius: 4px; box-shadow: inset 0 0 0 1px var(--tx3);
      color: var(--tx2); font-size: 9.5px; font-weight: 600; letter-spacing: .05em; line-height: 15px; }
    .jolta-sig { flex: none; margin-left: -8px; display: inline-flex; align-items: center; gap: 3px; padding: 0 5px;
      border-radius: 4px; box-shadow: inset 0 0 0 1px var(--tx3); color: var(--tx2); font-size: 9.5px; font-weight: 600;
      letter-spacing: .05em; line-height: 15px; }
    .jolta-sig.jolta-muted { color: var(--tx3); box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--tx3) 55%, transparent); }
    .jolta-sig.jolta-warn { color: var(--acc); box-shadow: inset 0 0 0 1px var(--acc); }
    .jolta-snippet { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; color: var(--tx3); }
    .jolta-date { color: var(--tx3); font: 11.5px ui-monospace, SFMono-Regular, monospace; }
    .jolta-to { display: none; }
    .jolta-message[open] > summary { height: auto; flex-wrap: wrap; padding-top: 20px; row-gap: 2px; }
    .jolta-message[open] .jolta-from { width: auto; font-size: 13.5px; }
    .jolta-message[open] .jolta-snippet { visibility: hidden; }
    .jolta-message[open] .jolta-to { display: block; flex-basis: 100%; color: var(--tx3); font-size: 12.5px;
      overflow: hidden; text-overflow: ellipsis; }
    .jolta-sigline { display: none; }
    .jolta-message[open] .jolta-sigline { display: block; flex-basis: 100%; color: var(--tx3); font-size: 12.5px;
      line-height: 18px; white-space: normal; }
    .jolta-message[open] .jolta-sigline.jolta-warn { color: var(--acc); }
    .jolta-notice { margin: 0 0 18px; color: var(--tx2); }
    .jolta-body { padding: 14px 0 24px; max-width: 62ch; overflow-wrap: anywhere; }
    .jolta-body img { max-width: 100%; height: auto; }
    body:not(.jolta-images):not(#jolta-a):not(#jolta-b) .jolta-body img[src^="x-remote:"] { display: none !important; }
    .jolta-body.jolta-wide { max-width: 100%; width: fit-content; margin: 0 auto; overflow-x: auto; }
    .jolta-body a { color: inherit; text-decoration-color: var(--tx3); }
    .jolta-quote { color: var(--tx3); }
    .jolta-files { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin: 0 0 18px; }
    .jolta-file { display: flex; align-items: center; gap: 9px; height: 40px; padding: 0 12px 0 9px; max-width: 100%;
      border-radius: 9px; box-shadow: inset 0 0 0 1px var(--line2); text-decoration: none; color: var(--tx); cursor: default; }
    .jolta-file svg { flex: none; color: var(--tx3); }
    .jolta-file span { display: flex; flex-direction: column; min-width: 0; }
    .jolta-file b { font-weight: 500; font-size: 12.5px; line-height: 16px; max-width: 220px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .jolta-file small { color: var(--tx3); font-size: 11.5px; line-height: 15px; }
    .jolta-save-all { margin-left: 4px; padding: 4px 8px; border-radius: 7px; font-size: 12.5px; color: var(--tx2);
      text-decoration: none; cursor: default; }
    .jolta-save-all:hover, .jolta-file:hover { background: var(--fill); }
    .jolta-clip { display: inline-flex; align-items: center; gap: 3px; color: var(--tx3); font-size: 11.5px; margin-right: 10px; }
    .jolta-message[open] .jolta-clip { display: none; }
    .jolta-invite { margin: 14px 0 0; width: min(100%, 520px); box-sizing: border-box; padding: 14px 16px; border-radius: 10px;
      box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx2); font-size: 12.5px; line-height: 18px; }
    .jolta-invite + .jolta-invite { margin-top: 8px; }
    .jolta-invite svg { flex: none; color: var(--tx3); }
    .jolta-invite-head { display: flex; align-items: center; justify-content: space-between; gap: 10px; margin-bottom: 6px; }
    .jolta-invite-label { display: inline-flex; align-items: center; gap: 6px; color: var(--tx3); font-size: 11px; font-weight: 600;
      letter-spacing: .01em; }
    .jolta-invite-answer { padding: 0 6px; border-radius: 5px; box-shadow: inset 0 0 0 1px var(--line2); font-size: 11px; line-height: 16px; }
    .jolta-invite-title { color: var(--tx); font-size: 15px; line-height: 21px; font-weight: 600; letter-spacing: -.01em; }
    .jolta-invite-cancelled .jolta-invite-title { color: var(--tx2); text-decoration: line-through; text-decoration-color: var(--tx3); }
    .jolta-invite-when { margin-top: 2px; color: var(--tx); font-size: 13px; font-variant-numeric: tabular-nums; }
    .jolta-invite-elsewhere, .jolta-invite-was { color: var(--tx3); font-variant-numeric: tabular-nums; }
    .jolta-invite-facts { display: flex; flex-wrap: wrap; gap: 2px 16px; margin-top: 10px; }
    .jolta-invite-facts span { display: inline-flex; align-items: center; gap: 6px; }
    .jolta-invite-people { display: flex; align-items: center; gap: 6px; margin-top: 2px; }
    .jolta-invite-facts + .jolta-invite-people { margin-top: 2px; }
    .jolta-invite-when + .jolta-invite-people, .jolta-invite-elsewhere + .jolta-invite-people,
      .jolta-invite-was + .jolta-invite-people { margin-top: 10px; }
    .jolta-invite-people b { color: var(--tx); font-weight: 500; }
    .jolta-invite-note, .jolta-invite-free { margin-top: 10px; }
    .jolta-invite-note + .jolta-invite-note, .jolta-invite-note + .jolta-invite-free { margin-top: 2px; }
    .jolta-invite .jolta-warn { color: var(--acc); }
    .jolta-invite del { color: var(--tx3); text-decoration: line-through; text-decoration-color: var(--tx3); }
    .jolta-invite ins { padding: 0 3px; margin: 0 -1px; border-radius: 4px; text-decoration: none; color: var(--tx);
      background: color-mix(in oklab, var(--acc) 15%, transparent); -webkit-box-decoration-break: clone; }
    .jolta-invite-facts span.jolta-edit { display: inline; }
    .jolta-invite-outdated .jolta-invite-title, .jolta-invite-outdated .jolta-invite-when { color: var(--tx2); }
    .jolta-strip { position: relative; margin-top: 8px; }
    .jolta-strip-hour { position: absolute; top: 0; bottom: 0; border-left: 1px solid var(--line2); }
    .jolta-strip-hour i { position: absolute; top: 0; left: 4px; color: var(--tx3); font: 10.5px/14px ui-monospace, SFMono-Regular, monospace; }
    .jolta-strip-busy, .jolta-strip-this { position: absolute; height: 18px; box-sizing: border-box; padding: 0 6px; border-radius: 4px;
      font-size: 11px; line-height: 18px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .jolta-strip-busy { background: var(--fill); color: var(--tx2); box-shadow: inset 0 0 0 1px var(--line2); }
    .jolta-strip-busy.jolta-conflict { color: var(--tx); box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--acc) 75%, transparent); }
    .jolta-strip-this { background: color-mix(in oklab, var(--tx) 13%, var(--bg)); color: var(--tx); font-weight: 600;
      box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--tx) 35%, transparent); }
    .jolta-strip .jolta-strip-beside { width: max-content; background: var(--bg); box-shadow: none; padding: 0 4px 0 5px; border-radius: 0; }
    .jolta-invite-actions { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin-top: 12px; }
    .jolta-join { display: inline-flex; align-items: center; height: 26px; box-sizing: border-box; padding: 0 10px; border-radius: 7px;
      box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx); font-weight: 500; text-decoration: none; cursor: default; }
    .jolta-rsvp + .jolta-join, .jolta-remove + .jolta-join { margin-left: auto; }
    .jolta-remove { display: inline-flex; align-items: center; height: 26px; box-sizing: border-box; padding: 0 10px; border-radius: 7px;
      box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx); font-weight: 500; text-decoration: none; cursor: default;
      transition: background-color .16s ease; }
    .jolta-remove:hover { background: var(--fill); }
    .jolta-sending .jolta-remove { animation: jolta-sending .7s ease-in-out infinite alternate; }
    .jolta-add, .jolta-update { display: inline-flex; align-items: center; gap: 5px; height: 22px; box-sizing: border-box; padding: 0 8px;
      margin: -3px -4px -3px 0; border-radius: 6px; box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx); font-size: 11.5px;
      font-weight: 500; text-decoration: none; cursor: default; transition: background-color .16s ease; }
    .jolta-add:hover, .jolta-update:hover { background: var(--fill); }
    .jolta-sending .jolta-add, .jolta-sending .jolta-update { animation: jolta-sending .7s ease-in-out infinite alternate; }
    .jolta-decline-proposal { display: inline-flex; align-items: center; gap: 6px; height: 26px; box-sizing: border-box;
      padding: 0 5px 0 10px; border-radius: 7px; box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx); font-weight: 500;
      text-decoration: none; cursor: default; transition: background-color .16s ease; }
    .jolta-decline-proposal:hover { background: var(--fill); }
    .jolta-decline-proposal kbd, .jolta-rsvp-propose kbd { min-width: 16px; height: 16px; box-sizing: border-box; padding: 0 4px;
      border-radius: 4px; text-align: center; box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx3);
      font: 10.5px/16px ui-monospace, SFMono-Regular, monospace; }
    .jolta-rsvp-propose kbd { min-width: 14px; height: 14px; padding: 0 3px; font-size: 9.5px; line-height: 14px; }
    .jolta-sending .jolta-decline-proposal { animation: jolta-sending .7s ease-in-out infinite alternate; }
    .jolta-invite-hint { color: var(--tx3); font-size: 12px; }
    .jolta-rsvp-fixed { color: var(--tx3); }
    .jolta-rsvp { display: inline-flex; gap: 6px; }
    .jolta-rsvp a { display: inline-flex; align-items: center; gap: 6px; height: 26px; box-sizing: border-box; padding: 0 5px 0 10px;
      border-radius: 7px; box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx); font-weight: 500; text-decoration: none;
      cursor: default; transition: background-color .16s ease, box-shadow .16s ease; }
    .jolta-rsvp a:hover { background: var(--fill); }
    .jolta-rsvp kbd { min-width: 16px; height: 16px; box-sizing: border-box; padding: 0 4px; border-radius: 4px; text-align: center;
      box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx3); font: 10.5px/16px ui-monospace, SFMono-Regular, monospace; }
    .jolta-rsvp svg { display: none; }
    .jolta-rsvp a.jolta-chosen { padding-left: 8px; font-weight: 600; background: color-mix(in oklab, var(--tx) 11%, var(--bg));
      box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--tx) 40%, transparent); }
    .jolta-rsvp a.jolta-chosen svg { display: block; color: var(--tx); }
    .jolta-sending .jolta-rsvp a.jolta-chosen { animation: jolta-sending .7s ease-in-out infinite alternate; }
    @keyframes jolta-sending { to { opacity: .45; } }
    .jolta-rsvp-options { display: flex; flex-wrap: wrap; gap: 2px 16px; margin-top: 8px; font-size: 12px; }
    .jolta-rsvp-options a { display: inline-flex; align-items: center; gap: 5px; max-width: 240px; overflow: hidden;
      text-overflow: ellipsis; white-space: nowrap; color: var(--tx3); text-decoration: none; cursor: default; }
    .jolta-rsvp-options a:hover { color: var(--tx2); }
    .jolta-rsvp-options a.jolta-on { color: var(--tx); }
    .jolta-rsvp-options .jolta-rsvp-note { display: block; }
    .jolta-invite a:focus { outline: none; }
    .jolta-invite a:focus-visible { outline: 2px solid var(--acc); outline-offset: 2px; border-radius: 7px; }
    .jolta-rsvp-quiet .jolta-tick { opacity: 0; }
    .jolta-rsvp-quiet.jolta-on .jolta-tick { opacity: 1; }
    .jolta-join:hover { background: var(--fill); }
    .jolta-join.jolta-live { background: var(--acc); color: #fff; box-shadow: none; }
    .jolta-invite-reply { display: flex; flex-wrap: wrap; align-items: center; gap: 2px 8px; padding: 10px 14px; }
    .jolta-invite-reply b { color: var(--tx); font-weight: 600; }
    .jolta-invite-about { margin-left: auto; color: var(--tx3); }
    """
}
