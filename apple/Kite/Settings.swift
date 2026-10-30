@preconcurrency import KiteFFI
import SwiftUI

/// Split-inbox sections: each is a name and a search query over the inbox.
struct SettingsView: View {
    let model: AppModel?

    var body: some View {
        TabView {
            SectionsSettings(model: model).tabItem { Label("Sections", systemImage: "rectangle.split.3x1") }
            AgentsSettings(model: model).tabItem { Label("Agents", systemImage: "cpu") }
        }
        .frame(width: 620, height: 460)
    }
}

struct SectionsSettings: View {
    let model: AppModel?
    @State private var sections: [SectionInfo] = []
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Inbox sections").font(.system(size: 13, weight: .semibold))
            Text("Each section is a tab over the inbox showing the conversations its query matches. Queries use the search language: from:, to:, subject:, has:attachment, has:list, is:unread, is:vip, is:newsletter, is:notification.")
                .font(.system(size: 12))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Table($sections) {
                TableColumn("Name") { $s in TextField("", text: $s.name) }.width(140)
                TableColumn("Query") { $s in TextField("", text: $s.query).font(.system(size: 12, design: .monospaced)) }
            }
            .frame(minHeight: 180)
            HStack {
                Button("Add") { sections.append(SectionInfo(id: -Int64(sections.count + 1), name: "New section", query: "from:")) }
                Button("Remove last") { if !sections.isEmpty { sections.removeLast() } }
                Spacer()
                if let error { Text(error).foregroundStyle(.red).font(.system(size: 12)).lineLimit(2) }
                Button("Save") {
                    do {
                        try model?.saveSections(sections)
                        error = nil
                    } catch {
                        self.error = "\(error)"
                    }
                }
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .onAppear { sections = model?.sections ?? [] }
    }
}

extension SectionInfo: @retroactive Identifiable {}
