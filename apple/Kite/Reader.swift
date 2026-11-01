@preconcurrency import KiteFFI
import AppKit
import SwiftUI
import WebKit

/// The reader's web view, locked down in independent layers: the core's
/// sanitizer and CSP, JavaScript off, and a content rule list that blocks
/// everything but local schemes. Images come only through Kite's own `cid:`
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
                forIdentifier: "kite-block-all", encodedContentRuleList: rules)
        } catch {
            NSLog("Kite: content rule list failed to compile: \(error)")
        }
    }

    func show(html: String) {
        let html = images.allowsRemote ? html.replacingOccurrences(of: "<body>", with: "<body class=\"kite-images\">") : html
        let events = page.load(html: html, baseURL: URL(string: "about:blank")!)
        guard UserDefaults.standard.bool(forKey: "KiteReaderDebug") else { return }
        Task {
            var log = "html \(html.count) chars\n"
            do {
                for try await event in events { log += "event \(event)\n" }
            } catch {
                log += "load error \(error)\n"
            }
            let probe = try? await page.callJavaScript("return [document.body ? document.body.innerText.length : -1, getComputedStyle(document.body).color, innerWidth, innerHeight].join(' ')")
            log += "probe \(String(describing: probe))\nurl \(String(describing: page.url))\n"
            try? log.write(toFile: AppModel.dataDirectory + "/readerdebug.txt", atomically: true, encoding: .utf8)
            try? html.write(toFile: AppModel.dataDirectory + "/readerdebug.html", atomically: true, encoding: .utf8)
        }
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
            if parts.count == 2, let message = Int64(parts[0]), let index = UInt32(parts[1]) {
                links.onAttachment?(message, index)
            }
            return .cancel
        }
        if action.navigationType == .linkActivated, let url, ["http", "https", "mailto"].contains(url.scheme ?? "") {
            Self.confirmOpen(url)
        }
        return .cancel
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
    let core: Core
    let onAttachment: (Int64, UInt32) -> Void
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
        reader.show(html: thread.html)
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
                            .help("Kite blocks remote images so senders can't see when you read their mail.")
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
        .onAppear {
            reader.onAttachment = onAttachment
            reader.images.core = core
            render()
            // `-KiteLoadImages YES` loads remote images at once, for checking from scripts.
            if UserDefaults.standard.bool(forKey: "KiteLoadImages"), imagesBlocked { loadImages() }
        }
        .onChange(of: thread.html) { render() }
    }
}

enum ReaderStyle {
    /// Graphite styles for the page around each message. Message HTML keeps
    /// its own look inside `.kite-body`.
    static let css = """
    :root { color-scheme: light dark; --bg:#ffffff; --tx:#17171a; --tx2:#55555d; --tx3:#66666e; --line2:#efefec; --acc:#e5521a; }
    @media (prefers-color-scheme: dark) { :root { --bg:#161619; --tx:#ececee; --tx2:#a6a6ae; --tx3:#8d8d97; --line2:#1c1c20; --acc:#ff6b2c; } }
    html, body { margin: 0; padding: 0; background: var(--bg); color: var(--tx);
      font: 14px/22px -apple-system, system-ui; -webkit-font-smoothing: antialiased; }
    body { padding: 0 clamp(16px, 5vw, 56px) 40px; }
    ::selection { background: color-mix(in oklab, var(--acc) 30%, transparent); }
    .kite-message { border-top: 1px solid var(--line2); }
    .kite-message > summary { list-style: none; cursor: default; display: flex; gap: 14px; align-items: center;
      height: 42px; white-space: nowrap; font-size: 13px; }
    .kite-message > summary::-webkit-details-marker { display: none; }
    .kite-from { font-weight: 600; width: 120px; flex: none; overflow: hidden; text-overflow: ellipsis; }
    .kite-vip { flex: none; margin-left: -8px; padding: 0 5px; border-radius: 4px; box-shadow: inset 0 0 0 1px var(--tx3);
      color: var(--tx2); font-size: 9.5px; font-weight: 600; letter-spacing: .05em; line-height: 15px; }
    .kite-snippet { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; color: var(--tx3); }
    .kite-date { color: var(--tx3); font: 11.5px ui-monospace, SFMono-Regular, monospace; }
    .kite-to { display: none; }
    .kite-message[open] > summary { height: auto; flex-wrap: wrap; padding-top: 20px; row-gap: 2px; }
    .kite-message[open] .kite-from { width: auto; font-size: 13.5px; }
    .kite-message[open] .kite-snippet { visibility: hidden; }
    .kite-message[open] .kite-to { display: block; flex-basis: 100%; color: var(--tx3); font-size: 12.5px;
      overflow: hidden; text-overflow: ellipsis; }
    .kite-body { padding: 14px 0 24px; max-width: 62ch; overflow-wrap: anywhere; }
    .kite-body img { max-width: 100%; height: auto; }
    body:not(.kite-images):not(#kite-a):not(#kite-b) .kite-body img[src^="x-remote:"] { display: none !important; }
    .kite-body.kite-wide { max-width: 100%; width: fit-content; margin: 0 auto; overflow-x: auto; }
    .kite-body a { color: inherit; text-decoration-color: var(--tx3); }
    .kite-quote { color: var(--tx3); }
    .kite-files { display: flex; flex-wrap: wrap; gap: 10px; margin-top: 18px; }
    .kite-file { display: flex; flex-direction: column; justify-content: center; height: 44px; padding: 0 14px;
      border-radius: 10px; box-shadow: inset 0 0 0 1px var(--line2); text-decoration: none; color: var(--tx); cursor: default; }
    .kite-file b { font-weight: 500; font-size: 12.5px; line-height: 16px; max-width: 220px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .kite-file small { color: var(--tx3); font-size: 11.5px; line-height: 15px; }
    """
}
