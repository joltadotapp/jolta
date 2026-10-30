import SwiftUI

@main
struct KiteApp: App {
    @State private var model: AppModel?
    @State private var failure: String?

    var body: some Scene {
        WindowGroup {
            Group {
                if let model {
                    ContentView(model: model)
                } else if let failure {
                    Text(failure).padding(40)
                } else {
                    ProgressView()
                }
            }
            .frame(minWidth: 1140, minHeight: 600)
            .task {
                await ReaderPage.prepare()
                do { model = try AppModel() } catch { failure = "Kite could not open its data: \(error)" }
            }
        }
        .windowStyle(.hiddenTitleBar)
        .defaultSize(width: 1440, height: 900)
    }
}
