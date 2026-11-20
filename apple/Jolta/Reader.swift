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
    /// Called once when the header of a message that `update` added is
    /// wholly in view.
    var onSeen: ((Int64) -> Void)? {
        get { links.onSeen }
        set { links.onSeen = newValue }
    }
    /// The context menu's items after Copy, for the page on screen.
    var menuExtras: () -> [MenuEntry] = { [] }
    private let links = LinkActions()
    private let navigator: Navigator
    /// The last page shown, loaded again if its web content process ends.
    private var shown: String?
    /// Counts page loads. Each page carries its number, so an update meant
    /// for an earlier page leaves a newer one alone.
    private(set) var generation = 0
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
        //
        // `fit` sizes mail wider than the reader. An image that `max-width`
        // shrinks gets `jolta-fit`, whose `height: auto` beats the height
        // Outlook writes into the image's style, and a click outside a link
        // toggles it to its own size. A fixed-width layout zooms down to the
        // reader's width, except while one of its images shows at its own
        // size; below half size it scrolls sideways instead.
        //
        // A message that `update` adds is fitted as it arrives, and reported
        // once its header is wholly in view.
        let script = """
        const zoom = (body) => {
          body.style.zoom = "";
          if (!body.classList.contains("jolta-wide") || body.querySelector("img.jolta-full")) return;
          const scale = body.clientWidth / body.scrollWidth;
          if (scale < 1 && scale >= 0.5) body.style.zoom = scale;
        };
        const fit = (body) => {
          if (!body || !body.clientWidth) return;
          body.style.zoom = "";
          const full = [...body.querySelectorAll("img.jolta-full")];
          for (const img of full) img.classList.remove("jolta-full");
          for (const img of body.querySelectorAll("img")) {
            const fitted = img.getBoundingClientRect().width;
            img.classList.add("jolta-natural");
            const natural = img.getBoundingClientRect().width;
            img.classList.remove("jolta-natural");
            img.classList.toggle("jolta-fit", natural > fitted + 1);
            if (natural > fitted + 1 && full.includes(img)) img.classList.add("jolta-full");
          }
          zoom(body);
        };
        const fitAll = () => document.querySelectorAll(".jolta-body").forEach(fit);
        document.addEventListener("DOMContentLoaded", fitAll);
        document.addEventListener("load", (e) => { if (e.target.tagName === "IMG") fit(e.target.closest(".jolta-body")); }, true);
        let resizing = 0;
        addEventListener("resize", () => { cancelAnimationFrame(resizing); resizing = requestAnimationFrame(fitAll); });
        document.addEventListener("click", (e) => {
          const img = e.target;
          if (!img.matches?.("img.jolta-fit") || img.closest("a[href]")) return;
          const body = img.closest(".jolta-body");
          const was = img.getBoundingClientRect();
          const x = (e.clientX - was.left) / was.width, y = (e.clientY - was.top) / was.height;
          img.classList.toggle("jolta-full");
          zoom(body);
          const now = img.getBoundingClientRect();
          body.scrollLeft += now.left + x * now.width - e.clientX;
          scrollBy(0, now.top + y * now.height - e.clientY);
        });
        document.addEventListener("toggle", (e) => {
          const body = e.target.matches?.("details.jolta-message") && e.target.querySelector(":scope > .jolta-body");
          if (body) webkit.messageHandlers.toggle.postMessage({ id: body.id.slice(1), open: e.target.open });
          if (body && e.target.open) fit(body);
        }, true);
        document.addEventListener("contextmenu", (e) => {
          const a = e.target.closest?.("a[href]");
          const href = a ? (typeof a.href === "string" ? a.href : a.href.baseVal) : "";
          webkit.messageHandlers.menu.postMessage(href);
        }, true);
        // Wholly in view, or filling the view when taller than it.
        const seen = new IntersectionObserver((entries) => {
          for (const e of entries) {
            const whole = e.intersectionRatio > 0.99 || (e.rootBounds && e.intersectionRect.height >= e.rootBounds.height * 0.98);
            if (!e.isIntersecting || !whole) continue;
            seen.unobserve(e.target);
            e.target.parentElement.classList.add("jolta-seen");
            const body = e.target.parentElement.querySelector(":scope > .jolta-body");
            if (body) webkit.messageHandlers.seen.postMessage(body.id.slice(1));
          }
        }, { threshold: [...Array(101).keys()].map((i) => i / 100) });
        document.addEventListener("DOMContentLoaded", () => {
          new MutationObserver((records) => {
            for (const node of records.flatMap((r) => [...r.addedNodes])) {
              if (!node.matches?.("details.jolta-message")) continue;
              if (node.open) fit(node.querySelector(":scope > .jolta-body"));
              if (node.classList.contains("jolta-new")) seen.observe(node.querySelector(":scope > summary"));
            }
          }).observe(document.body, { childList: true });
        });
        """
        content.addUserScript(WKUserScript(source: script, injectionTime: .atDocumentStart, forMainFrameOnly: true, in: .defaultClient))
        content.add(ToggleMessages(links: links), contentWorld: .defaultClient, name: "toggle")
        content.add(SeenMessages(links: links), contentWorld: .defaultClient, name: "seen")
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

    /// Loads a rendered conversation; `then` runs once it has loaded (true),
    /// or once the load failed or was replaced by the next one.
    func show(html: String, then done: (@MainActor (Bool) -> Void)? = nil) {
        shown = html
        generation += 1
        var page = html
        // Here rather than in the core's `appCss`: a page loads again, with
        // images, from HTML rendered before the theme last changed.
        if let head = page.range(of: "</head>") {
            page.insert(contentsOf: "<meta name=\"jolta-page\" content=\"\(generation)\"><style id=\"jolta-theme\">\(ReaderStyle.themeCSS)</style>\(images.style)",
                        at: head.lowerBound)
        }
        let html = page
        let navigation = webView.loadHTMLString(html, baseURL: URL(string: "about:blank"))
        #if DEBUG
        let debug = UserDefaults.standard.bool(forKey: "JoltaReaderDebug")
        #else
        let debug = false
        #endif
        guard debug || done != nil else { return }
        guard let navigation else { done?(false); return }
        var log = "html \(html.count) chars\n"
        navigator.track(navigation, event: { log += "event \($0)\n" }) { [weak self] error in
            if let error { log += "load error \(error)\n" }
            done?(error == nil)
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

    /// Brings the page on screen up to `html`, a newer render of the same
    /// conversation, without loading it again: the scroll position, the
    /// selection, expanded messages and loaded images stay. Messages that
    /// changed are replaced and new ones added with a "New" mark, which
    /// earlier new ones lose once they have been in view. Returns the new
    /// messages whose header is not below the view; nil when the page can't
    /// be updated and has to load again, or `page` is no longer on screen.
    func update(html: String, page: Int) async -> [Int64]? {
        guard page == generation, let previous = shown else { return nil }
        shown = html
        let script = """
        if (document.head.querySelector('meta[name="jolta-page"]')?.content !== page) return null;
        const parse = (text) => new DOMParser().parseFromString(text, "text/html");
        const fresh = parse(html);
        const key = (d) => d.querySelector(":scope > .jolta-body")?.id;
        // Whether a message changed, apart from whether it starts open.
        const shape = (d) => { const c = d.cloneNode(true); c.removeAttribute("open"); return c.outerHTML; };
        const before = new Map([...parse(previous).querySelectorAll("body > details.jolta-message")].map((d) => [key(d), shape(d)]));
        const wanted = [...fresh.querySelectorAll("body > details.jolta-message")];
        const styles = [...document.head.querySelectorAll("style:not([id])")];
        const freshStyles = [...fresh.head.querySelectorAll("style:not([id])")];
        if (styles.length !== freshStyles.length || wanted.some((d) => !key(d))) return null;
        const ids = new Set(wanted.map(key));
        const live = [...document.querySelectorAll("body > details.jolta-message")];
        // The first message on screen keeps its distance from the top.
        const anchor = live.find((d) => ids.has(key(d)) && d.getBoundingClientRect().bottom > 0);
        const anchorKey = anchor && key(anchor), top = anchor?.getBoundingClientRect().top;
        freshStyles.forEach((s, i) => { if (styles[i].textContent !== s.textContent) styles[i].textContent = s.textContent; });
        document.getElementById("jolta-images").textContent = images;
        for (const d of live) if (!ids.has(key(d))) d.remove();
        const added = [];
        let previousNode = null;
        for (const d of wanted) {
          const id = key(d);
          let node = document.getElementById(id)?.closest("body > details.jolta-message");
          if (!node || before.get(id) !== shape(d)) {
            const next = document.importNode(d, true);
            if (!node || node.classList.contains("jolta-new")) {
              next.classList.add("jolta-new");
              next.querySelector(":scope > summary > .jolta-from")?.insertAdjacentHTML("afterend", '<span class="jolta-new-mark">New</span>');
            }
            if (node) {
              next.open = node.open;
              node.replaceWith(next);
            } else {
              added.push(next);
            }
            node = next;
          }
          if (previousNode ? previousNode.nextElementSibling !== node : document.body.firstElementChild !== node) {
            if (previousNode) previousNode.after(node); else document.body.prepend(node);
          }
          previousNode = node;
        }
        // Only what arrived since the reader last looked keeps its mark.
        if (added.length) {
          for (const d of document.querySelectorAll("details.jolta-new.jolta-seen")) {
            d.classList.remove("jolta-new", "jolta-seen");
            d.querySelector(":scope > summary > .jolta-new-mark")?.remove();
          }
        }
        const moved = anchorKey && document.getElementById(anchorKey);
        if (moved) scrollBy(0, moved.closest("details.jolta-message").getBoundingClientRect().top - top);
        return added.filter((d) => d.querySelector(":scope > summary").getBoundingClientRect().bottom <= innerHeight).map((d) => key(d).slice(1));
        """
        let arguments: [String: Any] = ["html": html, "previous": previous, "images": images.css, "page": String(page)]
        guard let below = await call(script, arguments: arguments) as? [String] else { return nil }
        return below.compactMap(Int64.init)
    }

    /// Marks messages new again after the page loaded anew, so their "New"
    /// mark shows and the seen script reports them once they are in view.
    /// Returns those whose header is not below the view.
    func markNew(_ messages: [Int64]) async -> [Int64] {
        let script = """
        const above = [];
        for (const id of ids) {
          const node = document.getElementById("m" + id)?.closest("body > details.jolta-message");
          if (!node || node.classList.contains("jolta-new")) continue;
          const next = node.cloneNode(true);
          next.classList.add("jolta-new");
          next.querySelector(":scope > summary > .jolta-from")?.insertAdjacentHTML("afterend", '<span class="jolta-new-mark">New</span>');
          node.replaceWith(next);
          if (next.querySelector(":scope > summary").getBoundingClientRect().bottom <= innerHeight) above.push(id);
        }
        return above;
        """
        let above = await call(script, arguments: ["ids": messages.map(String.init)]) as? [String] ?? []
        return above.compactMap(Int64.init)
    }

    /// Scrolls the message's header to the top of the reader.
    func reveal(message: Int64, smooth: Bool) {
        let script = """
        document.getElementById("m" + id)?.closest("details.jolta-message")?.scrollIntoView({ block: "start", behavior: smooth ? "smooth" : "instant" });
        """
        run(script, arguments: ["id": String(message), "smooth": smooth])
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

    /// Scrolls by a page less two lines, which stay in sight to read on from.
    func scroll(pages: Int) {
        let smooth = !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
        run("scrollBy({ top: pages * (innerHeight - 44), behavior: smooth ? 'smooth' : 'instant' })", arguments: ["pages": pages, "smooth": smooth])
    }

    /// Selects the next or previous place the text occurs and scrolls to
    /// it, around the ends. False when it occurs nowhere.
    func find(_ text: String, backwards: Bool) async -> Bool {
        let configuration = WKFindConfiguration()
        configuration.backwards = backwards
        configuration.caseSensitive = false
        configuration.wraps = true
        return (try? await webView.find(text, configuration: configuration))?.matchFound ?? false
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
        var items: [MenuEntry] = []
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

/// One entry of a right-click menu built in AppKit: the reader's, a list
/// row's, an event's in the week.
enum MenuEntry {
    case button(String, @MainActor () -> Void)
    case divider

    @MainActor var menuItem: NSMenuItem {
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

/// `MenuEntry`s as the items of a SwiftUI context menu.
struct MenuEntries: View {
    let entries: () -> [MenuEntry]

    var body: some View {
        ForEach(Array(entries().enumerated()), id: \.offset) { _, entry in
            switch entry {
            case .divider: Divider()
            case .button(let title, let action): Button(title, action: action)
            }
        }
    }
}

extension NSMenu {
    convenience init(entries: [MenuEntry]) {
        self.init()
        for entry in entries { addItem(entry.menuItem) }
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
    var menu: () -> [MenuEntry] = { [] }

    func makeNSView(context: Context) -> ReaderWebView { page.webView }

    func updateNSView(_ view: ReaderWebView, context: Context) {
        page.menuExtras = menu
    }
}

/// What the image handlers may serve for the page on screen.
@MainActor
final class ImageSource {
    var core: Core?
    /// The messages whose remote images the user allowed.
    var allowed: Set<Int64> = []

    /// Hides every remote image outside the allowed messages.
    var css: String {
        let shown = allowed.sorted().map { ":not([src^=\"x-remote:\($0)/\"])" }.joined()
        return "body:not(#jolta-a):not(#jolta-b) .jolta-body img[src^=\"x-remote:\"]\(shown) { display: none !important; }"
    }

    var style: String { "<style id=\"jolta-images\">\(css)</style>" }
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

/// `x-remote:<message id>/<http URL>`: nothing until the user loads that
/// message's images, then a cookieless, cacheless fetch that sends no referrer.
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
        let path = url.absoluteString.dropFirst("x-remote:".count)
        let (message, address) = (path.prefix { $0 != "/" }, path.drop { $0 != "/" }.dropFirst())
        guard let message = Int64(message), source.allowed.contains(message), let target = URL(string: String(address)),
              ["http", "https"].contains(target.scheme?.lowercased() ?? "") else {
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
    var onSeen: ((Int64) -> Void)?
}

/// The seen script's messages: the id of a new message whose header came into view.
private final class SeenMessages: NSObject, WKScriptMessageHandler {
    let links: LinkActions

    init(links: LinkActions) {
        self.links = links
    }

    func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let id = (message.body as? String).flatMap(Int64.init) else { return }
        MainActor.assumeIsolated { links.onSeen?(id) }
    }
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
        let ended = loads
        loads.removeAll()
        onTerminate?()
        // After the page loads again: an `end` that loads it once more replaces that load.
        for load in ended { load.end("web content process ended") }
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
    /// Set when the user trusts or stops trusting the sender in this view;
    /// it holds only while that sender is the conversation's image sender.
    @State private var trustOverride: (sender: String, trusted: Bool)?
    @State private var shownThread: Int64?
    /// Another conversation is loading: it fades in once its page is in.
    @State private var arriving = false
    /// Counts page loads, so only the last one's completion acts.
    @State private var loads = 0
    /// A page is loading; a newer render loads in full instead of updating it.
    @State private var loading = false
    /// The page loading opens the conversation, so it scrolls to the start
    /// message the open found, even when a newer render replaces it before
    /// it is in.
    @State private var opening = false
    @State private var startMessage: Int64?
    /// Loads of the page in a row that failed. The first failure loads it
    /// again at once; after the second the reader offers Try Again.
    @State private var failedLoads = 0
    /// The page is placing replies new to it, so the pill waits to count only those below the view.
    @State private var placing = false
    /// The find bar (⌘F), what it looks for, and whether that occurs nowhere.
    @State private var finding = false
    @State private var findText = ""
    @State private var findMissed = false
    @FocusState private var findFocused: Bool

    /// The messages whose own sender is trusted. An image sender sent every
    /// message with remote images, so trusting it here covers them all.
    private var trustedImages: Set<Int64> {
        guard let trustOverride, trustOverride.sender == thread.imageSender else { return Set(thread.trustedImageMessages) }
        return trustOverride.trusted ? Set(thread.remoteImages.keys) : []
    }
    private var trusted: Bool { !thread.imageSender.isEmpty && !trustedImages.isEmpty }
    private var imagesBlocked: Bool { blockedImages > 0 }
    /// The remote images of the messages whose images don't load yet.
    private var blockedImages: UInt32 {
        let allowed = allowedImages
        return thread.remoteImages.filter { !allowed.contains($0.key) }.values.reduce(0, +)
    }
    /// The messages whose remote images load: those from a trusted sender
    /// and those the user loaded.
    private var allowedImages: Set<Int64> {
        trustedImages.union(model.imagesLoaded.intersection(thread.remoteImages.keys))
    }

    private func render() {
        if shownThread != thread.threadId {
            // Not the first one, which comes with the reader itself, and not under Reduce Motion.
            arriving = shownThread != nil && Theme.quick != nil
            shownThread = thread.threadId
            trustOverride = nil
            failedLoads = 0
            opening = true
            startMessage = model.openStart?.thread == thread.threadId ? model.openStart?.message : thread.startMessageId
        }
        let (start, threadID, messages) = (opening ? startMessage : nil, thread.threadId, thread.messages.map(\.id))
        reader.images.allowed = allowedImages
        loads += 1
        let load = loads
        loading = true
        model.readerLoading = true
        reader.show(html: thread.html) { loaded in
            guard load == loads else { return }
            loading = false
            if arriving { withAnimation(Theme.quick) { arriving = false } }
            // A page that didn't load showed nothing to read; the next one opens.
            guard loaded else {
                failedLoads += 1
                if failedLoads == 1 { render() }
                return
            }
            failedLoads = 0
            model.readerLoading = false
            if opening { model.conversationShown(threadID, messages: messages) }
            opening = false
            // Read once the page is in, so a message expanded or collapsed
            // while it loaded stays that way.
            let expanded = model.expandedMessages
            if !expanded.isEmpty { reader.expand(expanded) }
            if let start { reader.reveal(message: start, smooth: false) }
            // A page loaded anew has lost the marks of replies still below.
            if !model.unseenReplies.isEmpty {
                placing = true
                Task {
                    let inView = await reader.markNew(model.unseenReplies.map(\.id))
                    withAnimation(Theme.quick) {
                        model.unseenReplies.removeAll { inView.contains($0.id) }
                        placing = false
                    }
                }
            }
            if model.answerChoices != AnswerChoices() || model.pendingAnswer != nil { reflect() }
        }
    }

    /// A newer render of the conversation on screen goes into the page in
    /// place, so the reader keeps its place; another conversation loads.
    private func refresh() {
        guard shownThread == thread.threadId, !loading else { return render() }
        let (html, load, page, threadID) = (thread.html, loads, reader.generation, thread.threadId)
        reader.images.allowed = allowedImages
        placing = true
        Task {
            let update = await reader.update(html: html, page: page)
            // Another page loaded meanwhile, maybe another conversation's.
            guard load == loads, model.openThread?.threadId == threadID else { placing = false; return }
            guard let inView = update else { placing = false; return render() }
            // The model holds every reply new to the page; those in view aren't waiting below.
            withAnimation(Theme.quick) {
                model.unseenReplies.removeAll { inView.contains($0.id) }
                placing = false
            }
            if model.answerChoices != AnswerChoices() || model.pendingAnswer != nil { reflect() }
        }
    }

    /// A reply that arrived while open came into view.
    private func seen(_ message: Int64) {
        withAnimation(Theme.quick) { model.unseenReplies.removeAll { $0.id == message } }
        model.replyCameIntoView(message)
    }

    /// Over the reader's foot while replies that arrived are below its view.
    @ViewBuilder private var newRepliesPill: some View {
        if let first = model.unseenReplies.first, !loading, !placing {
            let title = model.unseenReplies.count == 1 ? "New reply from \(first.from)" : "\(model.unseenReplies.count) new replies"
            Button {
                reader.reveal(message: first.id, smooth: Theme.quick != nil)
            } label: {
                HStack(spacing: 6) {
                    Text(title).lineLimit(1).truncationMode(.middle)
                    Image(systemName: "arrow.down").font(.system(size: Theme.caption, weight: .semibold))
                }
                .font(.system(size: Theme.small, weight: .medium))
                .foregroundStyle(Color(Theme.bg))
                .padding(.horizontal, 12)
                .frame(height: Theme.controlSmall + 4)
                .background(Capsule().fill(Color(Theme.text)))
                .contentShape(Capsule())
            }
            .buttonStyle(.plain)
            .menuShadow()
            .frame(maxWidth: 360)
            .padding(.bottom, 18)
            .help("Scroll to the first new reply")
            .accessibilityIdentifier("reader-new-replies")
            .transition(.panel(anchor: .bottom))
        }
    }

    /// In place of a page that failed to load twice.
    @ViewBuilder private var loadFailed: some View {
        if failedLoads > 1, !loading {
            VStack(spacing: 10) {
                Text("The messages didn't load").font(.system(size: Theme.body)).foregroundStyle(Color(Theme.text3))
                KitButton(title: "Try Again", size: .small, action: render)
                    .accessibilityIdentifier("reader-try-again")
            }
            .padding(.top, 40)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            .background(Color(Theme.reader))
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

    private func find(backwards: Bool) {
        let text = findText
        guard !text.isEmpty else { return findMissed = false }
        Task {
            let found = await reader.find(text, backwards: backwards)
            if text == findText { findMissed = !found }
        }
    }

    private func closeFind() {
        finding = false
        model.requestListFocus()
    }

    /// Return finds the next place, ⇧Return the one before, Esc closes.
    private var findBar: some View {
        HStack(spacing: 6) {
            HStack(spacing: 7) {
                Image(systemName: "magnifyingglass").font(.system(size: Theme.small)).foregroundStyle(Color(Theme.text3))
                TextField("Find in conversation", text: $findText)
                    .textFieldStyle(.plain)
                    .font(.system(size: Theme.small))
                    .focused($findFocused)
                    .onKeyPress(.return, phases: .down) { press in
                        find(backwards: press.modifiers.contains(.shift))
                        return .handled
                    }
                    .onChange(of: findText) { find(backwards: false) }
                    .accessibilityIdentifier("reader-find")
                if findMissed {
                    Text("No matches").font(.system(size: Theme.small)).foregroundStyle(Color(Theme.text3)).fixedSize()
                }
            }
            .fieldBox(focused: findFocused, width: 240)
            KitButton(title: "Previous Match", key: "⇧↩", icon: "chevron.up", kind: .quiet, size: .small, showsTitle: false, showsKey: false) { find(backwards: true) }
                .help("Previous match (⇧↩)")
            KitButton(title: "Next Match", key: "↩", icon: "chevron.down", kind: .quiet, size: .small, showsTitle: false, showsKey: false) { find(backwards: false) }
                .help("Next match (↩)")
            KitButton(title: "Done", key: "esc", kind: .quiet, size: .small, action: closeFind)
        }
        .padding(.horizontal, 6)
        .frame(height: 40)
        .menuPanel()
        .padding(.top, 8)
        .padding(.trailing, 16)
        .onExitCommand(perform: closeFind)
        // The palette that ran Find gives the keyboard to the list on the
        // next turn of the run loop; take it again after that.
        .onAppear {
            findFocused = true
            DispatchQueue.main.async { findFocused = true }
        }
        .transition(.panel(anchor: .topTrailing))
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
            .transition(.panel(anchor: .topLeading))
        }
    }

    /// The context menu's image items, after Copy.
    private func imageMenu() -> [MenuEntry] {
        if imagesBlocked {
            var items: [MenuEntry] = [.button("Load Images", loadImages)]
            if !thread.imageSender.isEmpty {
                items.append(.button("Always Load Images from \(thread.imageSender)") { trustSender(true) })
            }
            return items
        }
        if trusted {
            return [.button("Stop Loading Images from \(thread.imageSender)") { trustSender(false) }]
        }
        return []
    }

    private func loadImages() {
        model.imagesLoaded.formUnion(thread.remoteImages.keys)
        render()
    }

    private func trustSender(_ trust: Bool) {
        guard !thread.imageSender.isEmpty else { return }
        try? core.setSenderTrusted(address: thread.imageSender, trusted: trust)
        trustOverride = (thread.imageSender, trust)
        if !trust { model.imagesLoaded.subtract(thread.remoteImages.keys) }
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
                                    KeyCap(key: "⌘U")
                                }
                            }
                            .buttonStyle(.plain)
                            .foregroundStyle(Color(Theme.text2))
                            .help("Leave this mailing list and archive the conversation")
                        }
                    }
                    .font(.system(size: Theme.body))
                }
                if imagesBlocked {
                    HStack(spacing: 10) {
                        Image(systemName: "photo.badge.exclamationmark").foregroundStyle(Color(Theme.text3))
                        Text(blockedImages == 1 ? "1 remote image blocked" : "\(blockedImages) remote images blocked")
                            .foregroundStyle(Color(Theme.text2))
                            .fixedSize()
                            .help("Jolta blocks remote images so senders can't see when you read their mail.")
                        KitButton(title: "Load Images", size: .small, action: loadImages)
                        if !thread.imageSender.isEmpty {
                            Button("Always from \(thread.imageSender)") { trustSender(true) }
                                .buttonStyle(.plain)
                                .foregroundStyle(Color(Theme.text2))
                                .lineLimit(1)
                                .truncationMode(.middle)
                                .help("Load remote images from this sender without asking")
                        }
                    }
                    .font(.system(size: Theme.body))
                    .padding(.top, 8)
                }
            }
            .padding(.horizontal, Theme.gutter)
            .padding(.top, 30)
            .padding(.bottom, 16)
            if let line = model.holdLine {
                HoldsLineView(line: line, model: model)
                    .padding(.horizontal, Theme.gutter)
                    .padding(.bottom, 12)
            }
            ReaderPageView(page: reader, menu: imageMenu)
                .overlay(alignment: .topLeading) { rsvpPanel }
                .overlay(alignment: .topTrailing) { if finding { findBar } }
                .overlay(alignment: .bottom) { newRepliesPill }
                .overlay(alignment: .top) { loadFailed }
        }
        .opacity(arriving ? 0 : 1)
        .animation(Theme.quick, value: model.rsvpPanel)
        .animation(Theme.quick, value: finding)
        .onChange(of: model.readerScroll.count) { reader.scroll(pages: model.readerScroll.pages) }
        .onChange(of: model.findRequest) {
            finding = true
            findFocused = true
            DispatchQueue.main.async { findFocused = true }
        }
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
            reader.onSeen = { seen($0) }
            reader.images.core = core
            render()
            #if DEBUG
            // `-JoltaLoadImages YES` loads remote images at once, for checking from scripts.
            if UserDefaults.standard.bool(forKey: "JoltaLoadImages"), imagesBlocked { loadImages() }
            #endif
        }
        .onChange(of: thread.html) { refresh() }
        // Opening the conversation again loads a page that failed, also when its HTML is the same.
        .onChange(of: model.opens) { if failedLoads > 1 { render() } }
        .task(id: HoldsKey(thread: thread.threadId, revision: model.holdsRevision)) {
            let line = try? model.core.holdsForThread(threadId: thread.threadId)
            if line?.setId != model.holdLine?.setId { model.holdCursor = nil }
            model.holdLine = line
        }
        .onChange(of: ThemeStore.shared.active) { reader.applyTheme() }
        .onChange(of: ThemeStore.shared.textSize) { reader.applyTheme() }
        .onChange(of: model.readerZoom, initial: true) { reader.webView.pageZoom = model.readerZoom }
    }
}

enum ReaderStyle {
    /// The variables `css` uses, from the light and the dark theme on screen.
    static var themeCSS: String {
        let pair = ThemeStore.shared.active
        return ":root { \(variables(pair.light)) } @media (prefers-color-scheme: dark) { :root { \(variables(pair.dark)) } } \(textCSS)"
    }

    /// The body text at the chosen text size. It comes after `css` in the
    /// page, so it replaces the size there. Mail that sets its own sizes
    /// keeps them; ⌘+ zooms those.
    private static var textCSS: String {
        "html, body { font-size: \(Int(Theme.scaled(Theme.readerBody)))px; line-height: \(Int(Theme.scaled(Theme.readerLine)))px; }"
    }

    private static func variables(_ t: ThemeTokens) -> String {
        let colors = [("bg", t.reader), ("tx", t.text), ("tx2", t.text2), ("tx3", t.text3), ("line2", t.line2), ("acc", t.accent),
                      ("acctx", t.accentText), ("onacc", t.onAccent)]
            .map { "--\($0.0):#\(String(format: "%06x", $0.1));" }
        // A hovered control's fill: the text color at the theme's strength, as `Theme.fill`.
        let fill = "--fill:color-mix(in srgb, var(--tx) \(t.fillAlpha * 100)%, transparent);"
        return (colors + [fill]).joined(separator: " ")
    }

    /// Styles for the page around each message. Message HTML keeps its own
    /// look inside `.jolta-body`.
    static let css = """
    :root { color-scheme: light dark; }
    html, body { margin: 0; padding: 0; background: var(--bg); color: var(--tx);
      font: \(Int(Theme.readerBody))px/\(Int(Theme.readerLine))px -apple-system, system-ui; -webkit-font-smoothing: antialiased; }
    body { padding: 0 \(Int(Theme.gutter))px 40px; }
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
    .jolta-sig.jolta-warn { color: var(--acctx); box-shadow: inset 0 0 0 1px var(--acc); }
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
    .jolta-message[open] .jolta-sigline.jolta-warn { color: var(--acctx); }
    .jolta-notice { margin: 0 0 18px; color: var(--tx2); }
    .jolta-body { padding: 14px 0 24px; max-width: 62ch; overflow-wrap: anywhere; }
    .jolta-body img { max-width: 100%; height: auto; }
    .jolta-body.jolta-wide { max-width: 100%; width: fit-content; margin: 0 auto; overflow-x: auto; }
    .jolta-body img.jolta-fit { height: auto !important; }
    .jolta-body img.jolta-fit:not(.jolta-full):not(a[href] img) { cursor: zoom-in; }
    .jolta-body img.jolta-natural, .jolta-body img.jolta-full { max-width: none !important; }
    .jolta-body img.jolta-full { cursor: zoom-out; }
    .jolta-body:has(img.jolta-full) { max-width: 100%; overflow-x: auto; }
    .jolta-body a { color: inherit; text-decoration-color: var(--tx3); }
    .jolta-quote { color: var(--tx3); }
    .jolta-files { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin: 0 0 18px; }
    .jolta-file { display: flex; align-items: center; gap: 9px; height: 40px; padding: 0 12px 0 9px; max-width: 100%;
      border-radius: 9px; box-shadow: inset 0 0 0 1px var(--line2); text-decoration: none; color: var(--tx); cursor: default; }
    .jolta-file svg { flex: none; color: var(--tx3); }
    .jolta-file span { display: flex; flex-direction: column; min-width: 0; }
    .jolta-file b { font-weight: 500; font-size: 12.5px; line-height: 16px; max-width: 220px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .jolta-file small { color: var(--tx3); font-size: 11.5px; line-height: 15px; }
    .jolta-save-all { margin-left: 4px; padding: 4px 8px; border-radius: 6px; font-size: 12.5px; color: var(--tx2);
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
    .jolta-invite .jolta-warn { color: var(--acctx); }
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
    .jolta-join { display: inline-flex; align-items: center; height: 26px; box-sizing: border-box; padding: 0 10px; border-radius: 6px;
      box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx); font-weight: 500; text-decoration: none; cursor: default; }
    .jolta-rsvp + .jolta-join, .jolta-remove + .jolta-join { margin-left: auto; }
    .jolta-remove { display: inline-flex; align-items: center; height: 26px; box-sizing: border-box; padding: 0 10px; border-radius: 6px;
      box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx); font-weight: 500; text-decoration: none; cursor: default; }
    .jolta-remove:hover { background: var(--fill); }
    .jolta-add, .jolta-update { display: inline-flex; align-items: center; gap: 5px; height: 22px; box-sizing: border-box; padding: 0 8px;
      margin: -3px -4px -3px 0; border-radius: 6px; box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx); font-size: 11.5px;
      font-weight: 500; text-decoration: none; cursor: default; }
    .jolta-add:hover, .jolta-update:hover { background: var(--fill); }
    .jolta-decline-proposal, .jolta-accept-proposal { display: inline-flex; align-items: center; gap: 6px; height: 26px; box-sizing: border-box;
      padding: 0 5px 0 10px; border-radius: 6px; box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx); font-weight: 500;
      text-decoration: none; cursor: default; }
    .jolta-decline-proposal:hover, .jolta-accept-proposal:hover { background: var(--fill); }
    .jolta-accept-proposal { margin-right: 6px; box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--acc) 55%, var(--line2)); }
    .jolta-decline-proposal kbd, .jolta-accept-proposal kbd, .jolta-rsvp-propose kbd { min-width: 16px; height: 16px; box-sizing: border-box; padding: 0 4px;
      border-radius: 4px; text-align: center; box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx3);
      font: 10.5px/16px ui-monospace, SFMono-Regular, monospace; }
    .jolta-rsvp-propose kbd { min-width: 14px; height: 14px; padding: 0 3px; font-size: 9.5px; line-height: 14px; }
    .jolta-invite-hint { color: var(--tx3); font-size: 12px; }
    .jolta-rsvp-fixed { color: var(--tx3); }
    .jolta-rsvp { display: inline-flex; gap: 6px; }
    .jolta-rsvp a { display: inline-flex; align-items: center; gap: 6px; height: 26px; box-sizing: border-box; padding: 0 5px 0 10px;
      border-radius: 6px; box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx); font-weight: 500; text-decoration: none;
      cursor: default; }
    .jolta-rsvp a:hover { background: var(--fill); }
    .jolta-rsvp kbd { min-width: 16px; height: 16px; box-sizing: border-box; padding: 0 4px; border-radius: 4px; text-align: center;
      box-shadow: inset 0 0 0 1px var(--line2); color: var(--tx3); font: 10.5px/16px ui-monospace, SFMono-Regular, monospace; }
    .jolta-rsvp svg { display: none; }
    .jolta-rsvp a.jolta-chosen { padding-left: 8px; font-weight: 600; background: color-mix(in oklab, var(--tx) 11%, var(--bg));
      box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--tx) 40%, transparent); }
    .jolta-rsvp a.jolta-chosen svg { display: block; color: var(--tx); }
    @keyframes jolta-sending { to { opacity: .45; } }
    @media (prefers-reduced-motion: no-preference) {
      .jolta-remove, .jolta-add, .jolta-update, .jolta-decline-proposal, .jolta-accept-proposal { transition: background-color .16s ease; }
      .jolta-rsvp a { transition: background-color .16s ease, box-shadow .16s ease; }
      .jolta-sending .jolta-remove, .jolta-sending .jolta-add, .jolta-sending .jolta-update, .jolta-sending .jolta-decline-proposal,
      .jolta-sending .jolta-rsvp a.jolta-chosen { animation: jolta-sending .7s ease-in-out infinite alternate; }
    }
    @media (prefers-reduced-motion: reduce) {
      .jolta-sending .jolta-remove, .jolta-sending .jolta-add, .jolta-sending .jolta-update, .jolta-sending .jolta-decline-proposal,
      .jolta-sending .jolta-rsvp a.jolta-chosen { opacity: .6; }
    }
    .jolta-rsvp-options { display: flex; flex-wrap: wrap; gap: 2px 16px; margin-top: 8px; font-size: 12px; }
    .jolta-rsvp-options a { display: inline-flex; align-items: center; gap: 5px; max-width: 240px; overflow: hidden;
      text-overflow: ellipsis; white-space: nowrap; color: var(--tx3); text-decoration: none; cursor: default; }
    .jolta-rsvp-options a:hover { color: var(--tx2); }
    .jolta-rsvp-options a.jolta-on { color: var(--tx); }
    .jolta-rsvp-options .jolta-rsvp-note { display: block; }
    .jolta-invite a:focus { outline: none; }
    .jolta-invite a:focus-visible { outline: 2px solid var(--acc); outline-offset: 2px; border-radius: 6px; }
    .jolta-rsvp-quiet .jolta-tick { opacity: 0; }
    .jolta-rsvp-quiet.jolta-on .jolta-tick { opacity: 1; }
    .jolta-join:hover { background: var(--fill); }
    .jolta-join.jolta-live { background: var(--acc); color: var(--onacc); box-shadow: none; }
    .jolta-invite-reply { display: flex; flex-wrap: wrap; align-items: center; gap: 2px 8px; padding: 10px 14px; }
    .jolta-invite-reply b { color: var(--tx); font-weight: 600; }
    .jolta-invite-about { margin-left: auto; color: var(--tx3); }
    .jolta-new-mark { flex: none; margin-left: -8px; padding: 0 5px; border-radius: 4px; box-shadow: inset 0 0 0 1px var(--tx3);
      color: var(--tx2); font-size: 10px; font-weight: 600; letter-spacing: .03em; line-height: 15px; }
    @media (prefers-reduced-motion: no-preference) {
      @keyframes jolta-arrive { from { opacity: 0; } }
      details.jolta-new { animation: jolta-arrive .3s ease-out; }
    }
    .jolta-quoted { margin: 8px 0 0; }
    .jolta-quoted > summary { list-style: none; display: inline-flex; align-items: center; height: 14px; padding: 0 7px;
      border-radius: 7px; background: var(--fill); color: var(--tx2); font: 700 9px/14px -apple-system, system-ui;
      letter-spacing: 1px; cursor: default; user-select: none; -webkit-user-select: none; }
    .jolta-quoted > summary::-webkit-details-marker { display: none; }
    .jolta-quoted > summary:hover, .jolta-quoted[open] > summary { color: var(--tx); background: color-mix(in srgb, var(--tx) 16%, transparent); }
    .jolta-quoted > summary:focus { outline: none; }
    .jolta-quoted > summary:focus-visible { outline: 2px solid var(--acc); outline-offset: 2px; }
    .jolta-quoted[open] > summary { margin-bottom: 10px; }
    @media (prefers-reduced-motion: no-preference) {
      .jolta-quoted > summary { transition: background-color .16s ease, color .16s ease; }
    }
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
                Text(line.title).font(.system(size: Theme.body, weight: .semibold)).lineLimit(1)
                Spacer(minLength: 8)
                if let slot = cursor {
                    KitButton(title: "Book \(Self.short(slot))", key: "B", size: .small) { model.bookHold(slot) }
                        .help("Open the event editor at this time with the people you wrote to")
                        .accessibilityIdentifier("holds-book")
                }
                KitButton(title: "Release All", kind: .quiet, size: .small) { model.releaseHolds(line.setId) }
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
        .card()
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
            .font(.system(size: Theme.small, weight: .medium))
            .foregroundStyle(Color(current || slot.state == .booked ? Theme.text : Theme.text2))
            .padding(.horizontal, 8)
            .frame(height: Theme.controlSmall)
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
