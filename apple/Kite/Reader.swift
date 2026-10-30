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
        let rules = """
        [{"trigger":{"url-filter":".*"},"action":{"type":"block"}},
         {"trigger":{"url-filter":"^(about|cid|data|x-remote):"},"action":{"type":"ignore-previous-rules"}}]
        """
        ruleList = try? await WKContentRuleListStore.default().compileContentRuleList(forIdentifier: "kite-block-all", encodedContentRuleList: rules)
    }

    func show(html: String) {
        _ = page.load(html: html, baseURL: URL(string: "about:blank")!)
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
            Text(thread.subject.isEmpty ? "(no subject)" : thread.subject)
                .font(.system(size: 18, weight: .semibold))
                .foregroundStyle(Color(Theme.text))
                .padding(.horizontal, 28)
                .padding(.top, 20)
                .padding(.bottom, 8)
                .textSelection(.enabled)
            WebView(reader.page)
                .webViewBackForwardNavigationGestures(.disabled)
        }
        .onAppear { reader.show(html: thread.html) }
        .onChange(of: thread.html) { _, html in reader.show(html: html) }
    }
}

enum ReaderStyle {
    /// The page around each message. Message HTML keeps its own look inside `.kite-body`.
    static let css = """
    :root { color-scheme: light dark; }
    html, body { margin: 0; padding: 0; background: transparent; font: 14px/1.5 -apple-system, system-ui; color: #1d1d1f; }
    @media (prefers-color-scheme: dark) { html, body { color: #e8e8ea; } }
    .kite-message { border-bottom: 1px solid rgba(127,127,127,.18); padding: 4px 28px; }
    .kite-message > summary { list-style: none; cursor: default; display: flex; gap: 12px; align-items: baseline; padding: 10px 0; }
    .kite-message > summary::-webkit-details-marker { display: none; }
    .kite-from { font-weight: 600; white-space: nowrap; }
    .kite-snippet { flex: 1; opacity: .55; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .kite-message[open] .kite-snippet { visibility: hidden; }
    .kite-date { opacity: .55; font-size: 12px; white-space: nowrap; font-variant-numeric: tabular-nums; }
    .kite-to { display: none; }
    .kite-message[open] .kite-to { display: block; position: absolute; visibility: hidden; }
    .kite-body { padding: 4px 0 20px; overflow-wrap: anywhere; }
    .kite-body img { max-width: 100%; height: auto; }
    .kite-plain { white-space: normal; }
    .kite-quote { opacity: .6; }
    """
}
