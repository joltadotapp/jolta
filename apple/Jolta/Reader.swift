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
    /// The context menu's items after Copy, for the page on screen.
    var menuExtras: () -> [ReaderMenuItem] = { [] }
    private let links = LinkActions()
    private let navigator: Navigator
    /// The last page shown, loaded again if its web content process ends.
    private var shown: String?
    private static var ruleList: WKContentRuleList?

    /// Built on first use: SwiftUI makes a `ReaderPage` each time a view that
    /// owns one in `@State` is initialised, and keeps only the first.
    private(set) lazy var webView: ReaderWebView = {
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        config.defaultWebpagePreferences.allowsContentJavaScript = false
        if #available(macOS 26.4, *) {
            config.defaultWebpagePreferences.securityRestrictionMode = .maximizeCompatibility
        }
        config.setURLSchemeHandler(ImageScheme(serve: InlineImages(source: images).serve), forURLScheme: "cid")
        config.setURLSchemeHandler(ImageScheme(serve: RemoteImages(source: images).serve), forURLScheme: "x-remote")
        let content = config.userContentController
        if let rules = Self.ruleList {
            content.add(rules)
        }
        // In the app's own script world, which runs with the page's
        // JavaScript off: every expand and collapse of a message, by its id,
        // and the link under the pointer as a context menu opens. WebKit
        // dispatches `contextmenu` before it asks the app to show the menu,
        // so the link arrives first.
        let script = """
        document.addEventListener("toggle", (e) => {
          const body = e.target.matches?.("details.jolta-message") && e.target.querySelector(":scope > .jolta-body");
          if (body) webkit.messageHandlers.toggle.postMessage({ id: body.id.slice(1), open: e.target.open });
        }, true);
        document.addEventListener("contextmenu", (e) => {
          const a = e.target.closest?.("a[href]");
          const href = a ? (typeof a.href === "string" ? a.href : a.href.baseVal) : "";
          webkit.messageHandlers.menu.postMessage(href);
        }, true);
        """
        content.addUserScript(WKUserScript(source: script, injectionTime: .atDocumentStart, forMainFrameOnly: true, in: .defaultClient))
        content.add(ToggleMessages(links: links), contentWorld: .defaultClient, name: "toggle")
        let menuLinks = MenuLinks()
        content.add(menuLinks, contentWorld: .defaultClient, name: "menu")

        let view = ReaderWebView(frame: .zero, configuration: config)
        view.allowsBackForwardNavigationGestures = false
        view.allowsMagnification = false
        view.allowsLinkPreview = false
        view.navigationDelegate = navigator
        view.uiDelegate = navigator
        view.menuItems = { [weak self] link in self?.menuItems(link: link) ?? [] }
        menuLinks.webView = view
        return view
    }()

    init() {
        navigator = Navigator(links: links)
        navigator.onTerminate = { [weak self] in
            guard let self, let html = shown else { return }
            show(html: html)
        }
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

    /// Loads a rendered conversation; `then` runs once it has loaded, or
    /// once the load failed or was replaced by the next one.
    func show(html: String, then done: (@MainActor () -> Void)? = nil) {
        shown = html
        var page = images.allowsRemote ? html.replacingOccurrences(of: "<body>", with: "<body class=\"jolta-images\">") : html
        // Here rather than in the core's `appCss`: a page loads again, with
        // images, from HTML rendered before the theme last changed.
        if let head = page.range(of: "</head>") {
            page.insert(contentsOf: "<style id=\"jolta-theme\">\(ReaderStyle.themeCSS)</style>", at: head.lowerBound)
        }
        let html = page
        let navigation = webView.loadHTMLString(html, baseURL: URL(string: "about:blank"))
        #if DEBUG
        let debug = UserDefaults.standard.bool(forKey: "JoltaReaderDebug")
        #else
        let debug = false
        #endif
        guard debug || done != nil else { return }
        guard let navigation else { done?(); return }
        var log = "html \(html.count) chars\n"
        navigator.track(navigation, event: { log += "event \($0)\n" }) { [weak self] error in
            if let error { log += "load error \(error)\n" }
            done?()
            #if DEBUG
            guard debug, let self else { return }
            let events = log
            Task {
                var log = events
                let probe = await self.call("return [document.body ? document.body.innerText.length : -1, getComputedStyle(document.body).color, innerWidth, innerHeight].join(' ')")
                log += "probe \(String(describing: probe))\nurl \(String(describing: self.webView.url))\n"
                try? log.write(toFile: AppModel.dataDirectory + "/readerdebug.txt", atomically: true, encoding: .utf8)
                try? html.write(toFile: AppModel.dataDirectory + "/readerdebug.html", atomically: true, encoding: .utf8)
            }
            #endif
        }
    }

    /// Runs the app's own script on the page. The page world, as WebKit for
    /// SwiftUI's `callJavaScript` used: the app's scripts run there even with
    /// the page's JavaScript off, and they read the page's own elements.
    func call(_ script: String, arguments: [String: Any] = [:]) async -> Any? {
        try? await webView.callAsyncJavaScript(script, arguments: arguments, in: nil, contentWorld: .page)
    }

    /// `call` without waiting for the result.
    func run(_ script: String, arguments: [String: Any] = [:]) {
        Task { _ = await call(script, arguments: arguments) }
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
        run(script, arguments: ["states": states])
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
        run(script, arguments: ["css": ReaderStyle.themeCSS])
    }

    /// Open Link… and Copy Link on a link the reader would open, then Copy
    /// and the view's own items.
    private func menuItems(link: URL?) -> [NSMenuItem] {
        var items: [ReaderMenuItem] = []
        if let link, Navigator.opens(link) {
            items += [
                .button("Open Link…") { Navigator.confirmOpen(link) },
                .button("Copy Link") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(link.absoluteString, forType: .string)
                },
                .divider,
            ]
        }
        items.append(.button("Copy") { [weak self] in NSApp.sendAction(#selector(NSText.copy(_:)), to: self?.webView, from: nil) })
        return (items + menuExtras()).map(\.menuItem)
    }
}

/// One entry of the reader's context menu.
enum ReaderMenuItem {
    case button(String, @MainActor () -> Void)
    case divider

    @MainActor fileprivate var menuItem: NSMenuItem {
        switch self {
        case .divider:
            return .separator()
        case .button(let title, let action):
            let target = MenuAction(action)
            let item = NSMenuItem(title: title, action: #selector(MenuAction.run), keyEquivalent: "")
            item.target = target
            // The item holds its target weakly.
            item.representedObject = target
            return item
        }
    }
}

@MainActor
private final class MenuAction: NSObject {
    let action: @MainActor () -> Void

    init(_ action: @escaping @MainActor () -> Void) {
        self.action = action
    }

    @objc func run() { action() }
}

/// The reader's `WKWebView`, with Jolta's context menu in place of WebKit's.
final class ReaderWebView: WKWebView {
    /// The link the pointer was on when the context menu was asked for, from
    /// the page's `contextmenu` event; nil off a link.
    fileprivate var menuLink: URL?
    fileprivate var menuItems: (URL?) -> [NSMenuItem] = { _ in [] }

    override func rightMouseDown(with event: NSEvent) {
        menuLink = nil
        super.rightMouseDown(with: event)
    }

    override func mouseDown(with event: NSEvent) {
        menuLink = nil
        super.mouseDown(with: event)
    }

    override func willOpenMenu(_ menu: NSMenu, with event: NSEvent) {
        menu.items = menuItems(menuLink)
        menuLink = nil
        super.willOpenMenu(menu, with: event)
    }
}

/// SwiftUI's host for a `ReaderPage`'s one web view.
struct ReaderPageView: NSViewRepresentable {
    let page: ReaderPage
    var menu: () -> [ReaderMenuItem] = { [] }

    func makeNSView(context: Context) -> ReaderWebView { page.webView }

    func updateNSView(_ view: ReaderWebView, context: Context) {
        page.menuExtras = menu
    }
}

/// What the image handlers may serve for the page on screen.
@MainActor
final class ImageSource {
    var core: Core?
    /// Set when the user asks to load this conversation's remote images.
    var allowsRemote = false
}

/// Answers a custom scheme's loads with what `serve` returns. WebKit stops a
/// load when the page goes away; a stopped task must not be answered, so
/// each running one is tracked until it is answered or stopped.
@MainActor
private final class ImageScheme: NSObject, WKURLSchemeHandler {
    typealias Reply = (mime: String, data: Data)
    private let serve: @MainActor (URL) async -> Reply
    private var running: [ObjectIdentifier: Task<Void, Never>] = [:]

    init(serve: @escaping @MainActor (URL) async -> Reply) {
        self.serve = serve
    }

    func webView(_ webView: WKWebView, start task: any WKURLSchemeTask) {
        let id = ObjectIdentifier(task)
        guard let url = task.request.url else {
            task.didFailWithError(URLError(.badURL))
            return
        }
        running[id] = Task {
            let reply = await serve(url)
            guard running.removeValue(forKey: id) != nil else { return }
            task.didReceive(URLResponse(url: url, mimeType: reply.mime, expectedContentLength: reply.data.count, textEncodingName: nil))
            task.didReceive(reply.data)
            task.didFinish()
        }
    }

    func webView(_ webView: WKWebView, stop task: any WKURLSchemeTask) {
        running.removeValue(forKey: ObjectIdentifier(task))?.cancel()
    }
}

/// `cid:<message id>/<content id>`: an image part of the message itself.
@MainActor
private struct InlineImages {
    let source: ImageSource

    func serve(_ url: URL) async -> ImageScheme.Reply {
        let path = url.absoluteString.dropFirst("cid:".count)
        guard let core = source.core, let slash = path.firstIndex(of: "/"), let message = Int64(path[..<slash]) else {
            return ("image/gif", Data())
        }
        let cid = String(path[path.index(after: slash)...]).removingPercentEncoding ?? String(path[path.index(after: slash)...])
        let part = try? await core.inlineImage(messageId: message, contentId: cid)
        return (part?.mimeType ?? "image/gif", part.map { Data($0.bytes) } ?? Data())
    }
}

/// `x-remote:<http URL>`: nothing until the user loads images, then a
/// cookieless, cacheless fetch that sends no referrer.
@MainActor
private struct RemoteImages {
    let source: ImageSource
    private static let session = URLSession(configuration: {
        let c = URLSessionConfiguration.ephemeral
        c.httpCookieStorage = nil
        c.urlCache = nil
        c.httpShouldSetCookies = false
        c.timeoutIntervalForRequest = 20
        return c
    }())

    func serve(_ url: URL) async -> ImageScheme.Reply {
        let target = URL(string: String(url.absoluteString.dropFirst("x-remote:".count)))
        guard source.allowsRemote, let target, ["http", "https"].contains(target.scheme?.lowercased() ?? "") else {
            return ("image/gif", Data())
        }
        let fetched = try? await Self.session.data(from: target)
        return ((fetched?.1 as? HTTPURLResponse)?.mimeType ?? "image/gif", fetched?.0 ?? Data())
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

/// The context menu script's messages: the link's URL, or "" off a link.
private final class MenuLinks: NSObject, WKScriptMessageHandler {
    weak var webView: ReaderWebView?

    func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        let link = (message.body as? String).flatMap { $0.isEmpty ? nil : URL(string: $0) }
        MainActor.assumeIsolated { webView?.menuLink = link }
    }
}

/// Decides every navigation and reports the loads `ReaderPage.show` starts.
@MainActor
private final class Navigator: NSObject, WKNavigationDelegate, WKUIDelegate {
    let links: LinkActions
    var onTerminate: (() -> Void)?
    private var loads: [(navigation: WKNavigation, event: @MainActor (String) -> Void, end: @MainActor (String?) -> Void)] = []

    init(links: LinkActions) {
        self.links = links
    }

    /// Calls `event` as the load moves on and `end` once, when it finished
    /// (nil) or failed (the error).
    func track(_ navigation: WKNavigation, event: @escaping @MainActor (String) -> Void, end: @escaping @MainActor (String?) -> Void) {
        loads.append((navigation, event, end))
    }

    private func report(_ navigation: WKNavigation?, _ event: String) {
        loads.first { $0.navigation === navigation }?.event(event)
    }

    private func end(_ navigation: WKNavigation?, _ event: String, error: String?) {
        guard let i = loads.firstIndex(where: { $0.navigation === navigation }) else { return }
        let load = loads.remove(at: i)
        load.event(event)
        load.end(error)
    }

    func webView(_ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) {
        report(navigation, "started")
    }

    func webView(_ webView: WKWebView, didCommit navigation: WKNavigation!) {
        report(navigation, "committed")
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        end(navigation, "finished", error: nil)
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: any Error) {
        end(navigation, "failed", error: "\(error)")
    }

    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: any Error) {
        end(navigation, "failed", error: "\(error)")
    }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        for load in loads { load.end("web content process ended") }
        loads.removeAll()
        onTerminate?()
    }

    /// No page opens a window: a `target="_blank"` link was already decided
    /// as a navigation.
    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration, for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        nil
    }

    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction, preferences: WKWebpagePreferences) async -> (WKNavigationActionPolicy, WKWebpagePreferences) {
        if #available(macOS 26.4, *) {
            preferences.securityRestrictionMode = .maximizeCompatibility
        }
        preferences.allowsContentJavaScript = false
        return (decide(action), preferences)
    }

    private func decide(_ action: WKNavigationAction) -> WKNavigationActionPolicy {
        let url = action.request.url
        if action.navigationType == .other, url?.absoluteString == "about:blank" {
            return .allow
        }
        guard action.navigationType == .linkActivated, let url else { return .cancel }
        switch url.scheme {
        case "x-attachment":
            let parts = url.absoluteString.dropFirst("x-attachment:".count).split(separator: "/")
            if parts.count == 2, let message = Int64(parts[0]) {
                if parts[1] == "all" {
                    links.onSaveAll?(message)
                } else if let index = UInt32(parts[1]) {
                    links.onAttachment?(message, index)
                }
            }
        case "x-rsvp":
            let parts = url.absoluteString.dropFirst("x-rsvp:".count).split(separator: "/")
            if parts.count == 2, let message = Int64(parts[0]) {
                links.onRsvp?(message, String(parts[1]))
            }
        case "x-join":
            Self.join(url)
        default:
            // After WebKit has its answer: the alert's modal loop would
            // otherwise hold the decision open.
            if Self.opens(url) { Task { Self.confirmOpen(url) } }
        }
        return .cancel
    }

    /// The links the reader opens in another app, after asking.
    static func opens(_ url: URL) -> Bool {
        ["http", "https", "mailto"].contains(url.scheme ?? "")
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
        reader.show(html: thread.html) {
            // Read once the page is in, so a message expanded or collapsed
            // while it loaded stays that way.
            let expanded = model.expandedMessages
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
        case "accept-proposal":
            model.acceptProposal(message: message)
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
            // Hidden, not closed, behind Find a time, so Escape brings it back as it was.
            .opacity(model.findTime == nil ? 1 : 0)
            .accessibilityHidden(model.findTime != nil)
            .transition(.opacity.combined(with: .scale(scale: 0.97, anchor: .topLeading)))
        }
    }

    /// The context menu's image items, after Copy.
    private func imageMenu() -> [ReaderMenuItem] {
        if imagesBlocked {
            var items: [ReaderMenuItem] = [.button("Load Images", loadImages)]
            if !thread.imageSender.isEmpty {
                items.append(.button("Always Load Images from \(thread.imageSender)") { trustSender(true) })
            }
            return items
        }
        if trusted, !thread.imageSender.isEmpty {
            return [.button("Stop Loading Images from \(thread.imageSender)") { trustSender(false) }]
        }
        return []
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
            if let line = model.holdLine {
                HoldsLineView(line: line, model: model)
                    .padding(.horizontal, 56)
                    .padding(.bottom, 12)
            }
            ReaderPageView(page: reader, menu: imageMenu)
                .overlay(alignment: .topLeading) { rsvpPanel }
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
            #if DEBUG
            // `-JoltaLoadImages YES` loads remote images at once, for checking from scripts.
            if UserDefaults.standard.bool(forKey: "JoltaLoadImages"), imagesBlocked { loadImages() }
            #endif
        }
        .onChange(of: thread.html) { render() }
        .task(id: HoldsKey(thread: thread.threadId, revision: model.holdsRevision)) {
            let line = try? model.core.holdsForThread(threadId: thread.threadId)
            if line?.setId != model.holdLine?.setId { model.holdCursor = nil }
            model.holdLine = line
        }
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
    .jolta-decline-proposal, .jolta-accept-proposal { display: inline-flex; align-items: center; gap: 6px; height: 26px; box-sizing: border-box;
      padding: 0 5px 0 10px; border-radius: 7px; box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx); font-weight: 500;
      text-decoration: none; cursor: default; transition: background-color .16s ease; }
    .jolta-decline-proposal:hover, .jolta-accept-proposal:hover { background: var(--fill); }
    .jolta-accept-proposal { margin-right: 6px; box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--acc) 55%, var(--line2)); }
    .jolta-decline-proposal kbd, .jolta-accept-proposal kbd, .jolta-rsvp-propose kbd { min-width: 16px; height: 16px; box-sizing: border-box; padding: 0 4px;
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

private struct HoldsKey: Hashable {
    let thread: Int64
    let revision: Int
}

/// The times a sent mail in this conversation holds (C4 U10): booked ones
/// marked, taken ones struck through, with Book (B) for the picked time
/// and Release all.
struct HoldsLineView: View {
    let line: HoldLine
    let model: AppModel

    private var held: [HoldSlotRow] { line.slots.filter { $0.state == .held } }
    private var cursor: HoldSlotRow? { held.first { $0.start == model.holdCursor } ?? held.first }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                RoundedRectangle(cornerRadius: 2).fill(Color(CalendarColor.color(Self.hue, .bar))).frame(width: 8, height: 8)
                Text(line.title).font(.system(size: 12.5, weight: .semibold)).lineLimit(1)
                Spacer(minLength: 8)
                if let slot = cursor {
                    Button { model.bookHold(slot) } label: {
                        HStack(spacing: 5) {
                            Text("Book \(Self.short(slot))")
                            KeyCap(key: "B")
                        }
                    }
                    .buttonStyle(.plain)
                    .font(.system(size: 12, weight: .semibold))
                    .padding(.horizontal, 8)
                    .frame(height: 24)
                    .background(RoundedRectangle(cornerRadius: 6).fill(Color(Theme.fill)))
                    .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
                    .help("Open the event editor at this time with the people you wrote to")
                    .accessibilityIdentifier("holds-book")
                }
                Button("Release all") { model.releaseHolds(line.setId) }
                    .buttonStyle(.plain)
                    .font(.system(size: 12))
                    .foregroundStyle(Color(Theme.text2))
                    .help("Delete every hold of this message from your calendar")
                    .accessibilityIdentifier("holds-release")
            }
            if let ask = line.ask {
                Text(ask).font(.system(size: 12)).foregroundStyle(Color(Theme.text2))
            }
            HStack(spacing: 6) {
                ForEach(line.slots, id: \.start) { slot in
                    chip(slot)
                }
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(maxWidth: 720, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 10).fill(Color(Theme.raise)))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color(Theme.line), lineWidth: 0.5))
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("holds-line")
    }

    /// The holds' own color, between the calendars' hues.
    static let hue = "#8a7a5a"

    private func chip(_ slot: HoldSlotRow) -> some View {
        let current = slot.state == .held && slot.start == cursor?.start
        return Button { model.holdCursor = slot.start } label: {
            HStack(spacing: 5) {
                if slot.state == .booked { Image(systemName: "checkmark").font(.system(size: 9, weight: .bold)) }
                Text(slot.label).strikethrough(slot.state == .taken)
            }
            .font(.system(size: 11.5, weight: .medium))
            .foregroundStyle(Color(current || slot.state == .booked ? Theme.text : Theme.text2))
            .padding(.horizontal, 8)
            .frame(height: 22)
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(current ? Theme.accent : Theme.line), lineWidth: current ? 1.5 : 1))
            .opacity(slot.state == .taken ? 0.5 : 1)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(slot.state != .held)
        .help(slot.state == .taken ? "No longer free" : slot.state == .booked ? "Booked" : "Held")
    }

    /// "Tue 16:00".
    private static func short(_ slot: HoldSlotRow) -> String {
        let f = DateFormatter()
        f.dateFormat = "EEE HH:mm"
        return f.string(from: Date(timeIntervalSince1970: TimeInterval(slot.start)))
    }
}
