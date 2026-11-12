@preconcurrency import JoltaFFI
import AppKit
import SwiftUI

/// The kinds of account Jolta adds, for the welcome screen, Settings › Add
/// Account and ⌘K.
enum NewAccount: CaseIterable, Identifiable {
    case google, microsoft, imap

    var id: Self { self }

    var name: String {
        switch self {
        case .google: "Google"
        case .microsoft: "Microsoft 365"
        case .imap: "iCloud or IMAP"
        }
    }

    var icon: String {
        switch self {
        case .google: SettingsView.icon("Google")
        case .microsoft: SettingsView.icon("Microsoft 365")
        case .imap: SettingsView.icon("iCloud")
        }
    }
}

extension AppModel {
    /// Starts adding an account from the main window: the browser sign-in
    /// for Google and Microsoft 365, the address and password sheet for the
    /// rest.
    func addAccount(_ kind: NewAccount) {
        switch kind {
        case .google: signIn(.google)
        case .microsoft: signIn(.microsoft)
        case .imap: addingIMAP = true
        }
    }

    /// Signs in to a new Google or Microsoft 365 account in the browser.
    /// `added` runs once the account is there. The welcome screen and
    /// Settings show how it goes; with mail on screen a toast does.
    func signIn(_ provider: OAuthProvider, added: @escaping (AccountInfo) -> Void = { _ in }) {
        guard signIn != .waiting else { return }
        signIn = .waiting
        signInAttempt += 1
        let attempt = signInAttempt
        if !needsAccount { notify("Continue in your browser…") }
        Task {
            do {
                let session = try await core.beginSignIn(provider: provider)
                // Cancelled while the core set the sign-in up.
                guard attempt == signInAttempt, signIn == .waiting else { return session.cancel() }
                signInSession = session
                if let url = URL(string: session.url()) { NSWorkspace.shared.open(url) }
                let account = try await session.finish()
                reloadAccounts()
                notify("Signed in as \(account.address). Jolta is syncing it now.")
                added(account)
                guard attempt == signInAttempt else { return }
                signInSession = nil
                signIn = nil
            } catch {
                guard attempt == signInAttempt, signIn == .waiting else { return }
                signInSession = nil
                let failure = "Sign-in failed: \(error)"
                signIn = .failed(failure)
                if !needsAccount { notify(failure) }
            }
        }
    }

    /// Stops waiting for the browser. Finishing in the browser afterwards
    /// adds nothing.
    func cancelSignIn() {
        signInSession?.cancel()
        signInSession = nil
        signIn = nil
    }
}

/// The main window before the first account: what Jolta needs, and the
/// three ways to give it.
struct Welcome: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(spacing: 10) {
            Text("Welcome to Jolta").font(.system(size: Theme.display, weight: .semibold)).tracking(-0.3)
            Text("Add a mail account, and Jolta downloads your mail straight from your provider to this Mac.")
                .font(.system(size: Theme.body))
                .foregroundStyle(Color(Theme.text2))
                .multilineTextAlignment(.center)
                .frame(maxWidth: 380)
            HStack(spacing: 8) {
                ForEach(NewAccount.allCases) { kind in
                    KitButton(title: kind.name, icon: kind.icon) { model.addAccount(kind) }
                        .disabled(model.signIn == .waiting)
                        .accessibilityIdentifier("welcome-add-\(kind)")
                }
            }
            .padding(.top, 10)
            SignInStatusRow(model: model).frame(minHeight: Theme.controlSmall).padding(.top, 4)
        }
        .padding(.bottom, 80)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(TitleBarArea())
        .overlay(alignment: .bottom) { ToastLayer(model: model) }
    }
}

/// How a browser sign-in is going: waiting for the browser, with Cancel, or
/// why the last one failed, until the next attempt. Empty otherwise.
struct SignInStatusRow: View {
    let model: AppModel

    var body: some View {
        switch model.signIn {
        case .waiting:
            // Cancel goes under the words where the row is narrow, as in Settings' sidebar.
            ViewThatFits(in: .horizontal) {
                HStack(spacing: 10) { waiting; cancel }
                VStack(alignment: .leading, spacing: 6) { waiting; cancel }
            }
            .font(.system(size: Theme.small))
        case .failed(let message):
            Text(message)
                .font(.system(size: Theme.small))
                .foregroundStyle(Color(Theme.dangerText))
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
        case nil:
            EmptyView()
        }
    }

    private var waiting: some View {
        HStack(spacing: 8) {
            ProgressView().controlSize(.small)
            Text("Continue in your browser…").foregroundStyle(Color(Theme.text2)).fixedSize()
        }
    }

    private var cancel: some View {
        KitButton(title: "Cancel", size: .small) { model.cancelSignIn() }
            .accessibilityIdentifier("sign-in-cancel")
    }
}
