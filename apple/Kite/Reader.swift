@preconcurrency import KiteFFI
import AppKit
import SwiftUI
import WebKit

/// The reader's web view, locked down in four independent layers: the core's
/// sanitizer and CSP, JavaScript off, subresource loading off, and a content
/// rule list that blocks everything but local schemes. Every navigation after
/// the initial load is cancelled; a clicked link opens in the browser only
/// after the user confirms its real destination.
@MainActor
final class ReaderPage {
    let page: WebPage
    private static var ruleList: WKContentRuleList?

    init() {
        var config = WebPage.Configuration()
        config.websiteDataStore = .nonPersistent()
        config.loadsSubresources = false
        config.defaultNavigationPreferences.allowsContentJavaScript = false
        config.urlSchemeHandlers[URLScheme("cid")!] = BlockedImages()
        config.urlSchemeHandlers[URLScheme("x-remote")!] = BlockedImages()
        if let rules = Self.ruleList {
            config.userContentController.add(rules)
        }
        page = WebPage(configuration: config, navigationDecider: Decider())
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

/// Inline (`cid:`) and remote (`x-remote:`) images are not served yet; both answer with nothing.
private struct BlockedImages: URLSchemeHandler {
    func reply(for request: URLRequest) -> AsyncThrowingStream<URLSchemeTaskResult, any Error> {
        AsyncThrowingStream { continuation in
            let response = URLResponse(url: request.url ?? URL(string: "about:blank")!, mimeType: "image/gif", expectedContentLength: 0, textEncodingName: nil)
            continuation.yield(.response(response))
            continuation.yield(.data(Data()))
            continuation.finish()
        }
    }
}

@MainActor
private struct Decider: WebPage.NavigationDeciding {
    func decidePolicy(for action: WebPage.NavigationAction, preferences: inout WebPage.NavigationPreferences) async -> WKNavigationActionPolicy {
        if #available(macOS 26.4, *) {
            preferences.securityRestrictionMode = .maximizeCompatibility
        }
        preferences.allowsContentJavaScript = false
        let url = action.request.url
        if action.navigationType == .other, url?.absoluteString == "about:blank" {
            return .allow
        }
        if action.navigationType == .linkActivated, let url, ["http", "https", "mailto"].contains(url.scheme ?? "") {
            confirmOpen(url)
        }
        return .cancel
    }

    private func confirmOpen(_ url: URL) {
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
    @State private var reader = ReaderPage()

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 6) {
                Text(thread.subject.isEmpty ? "(no subject)" : thread.subject)
                    .font(.system(size: 21, weight: .semibold))
                    .tracking(-0.3)
                    .foregroundStyle(Color(Theme.text))
                    .textSelection(.enabled)
                    .frame(maxWidth: 560, alignment: .leading)
                if thread.messageCount > 1 {
                    Text("\(thread.messageCount) messages")
                        .font(.system(size: 12.5))
                        .foregroundStyle(Color(Theme.text3))
                }
            }
            .padding(.horizontal, 56)
            .padding(.top, 30)
            .padding(.bottom, 16)
            WebView(reader.page)
                .webViewBackForwardNavigationGestures(.disabled)
                .webViewMagnificationGestures(.disabled)
                .webViewLinkPreviews(.disabled)
        }
        .onAppear { reader.show(html: thread.html) }
        .onChange(of: thread.html) { _, html in reader.show(html: html) }
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
    body { padding: 0 56px 40px; }
    ::selection { background: color-mix(in oklab, var(--acc) 30%, transparent); }
    .kite-message { border-top: 1px solid var(--line2); }
    .kite-message > summary { list-style: none; cursor: default; display: flex; gap: 14px; align-items: center;
      height: 42px; white-space: nowrap; font-size: 13px; }
    .kite-message > summary::-webkit-details-marker { display: none; }
    .kite-from { font-weight: 600; width: 120px; flex: none; overflow: hidden; text-overflow: ellipsis; }
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
    .kite-body img[src^="x-remote:"] { display: none; }
    .kite-body a { color: inherit; text-decoration-color: var(--tx3); }
    .kite-quote { color: var(--tx3); }
    """
}
