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
            .frame(minWidth: 900, minHeight: 560)
            .task {
                await ReaderPage.prepare()
                do { model = try AppModel() } catch { failure = "Kite could not open its data: \(error)" }
            }
        }
    }
}
