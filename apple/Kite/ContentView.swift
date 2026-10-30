@preconcurrency import KiteFFI
import SwiftUI

struct ContentView: View {
    let model: AppModel

    var body: some View {
        NavigationSplitView {
            Sidebar(model: model)
                .navigationSplitViewColumnWidth(min: 180, ideal: 220, max: 300)
        } detail: {
            HSplitView {
                VStack(spacing: 0) {
                    SearchField(model: model)
                    MessageList(model: model)
                }
                .frame(minWidth: 380, idealWidth: 520)
                Group {
                    if let thread = model.openThread {
                        ReaderView(thread: thread)
                    } else {
                        Text("No conversation selected")
                            .foregroundStyle(.tertiary)
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                    }
                }
                .frame(minWidth: 420, maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .navigationTitle(model.source.title)
    }
}

struct Sidebar: View {
    @Bindable var model: AppModel

    var body: some View {
        List(selection: Binding(get: { model.source }, set: { if let s = $0 { model.source = s } })) {
            Label("Inbox", systemImage: "tray").tag(ListSource.unifiedInbox)
            ForEach(model.accounts) { section in
                Section(section.account.displayName.isEmpty ? section.account.address : section.account.displayName) {
                    ForEach(section.mailboxes, id: \.id) { box in
                        HStack {
                            Text(box.name).lineLimit(1)
                            Spacer()
                            if box.unread > 0 {
                                Text("\(box.unread)").foregroundStyle(.secondary).monospacedDigit()
                            }
                        }
                        .tag(ListSource.mailbox(id: box.id, name: box.name))
                    }
                }
            }
        }
    }
}

struct SearchField: View {
    @Bindable var model: AppModel
    @FocusState private var focused: Bool

    var body: some View {
        TextField("Search", text: $model.search)
            .textFieldStyle(.plain)
            .focused($focused)
            .padding(.horizontal, 14)
            .padding(.vertical, 9)
            .background(.quaternary.opacity(0.5))
            .onExitCommand { model.search = ""; focused = false }
            .background {
                Button("") { focused = true }.keyboardShortcut("/", modifiers: []).hidden()
            }
    }
}
