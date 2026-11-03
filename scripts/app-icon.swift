// Renders the app icon from apple/jolta-icon.svg into every macOS
// size in the asset catalog. WebKit draws the SVG, so its filters (paper
// grain, bevel, shadows) come out as in a browser.
//
//   swiftc -O -o /tmp/app-icon scripts/app-icon.swift && /tmp/app-icon apple/jolta-icon.svg apple/Jolta/Assets.xcassets/AppIcon.appiconset

import AppKit
import WebKit

let args = CommandLine.arguments
guard args.count == 3, let svg = try? String(contentsOfFile: args[1], encoding: .utf8) else {
    FileHandle.standardError.write(Data("usage: app-icon <icon.svg> <AppIcon.appiconset>\n".utf8))
    exit(2)
}
let folder = args[2]
let canvas: CGFloat = 1024

final class Loader: NSObject, WKNavigationDelegate {
    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
            let config = WKSnapshotConfiguration()
            config.rect = CGRect(x: 0, y: 0, width: canvas, height: canvas)
            webView.takeSnapshot(with: config) { image, error in
                guard let image, let cg = image.cgImage(forProposedRect: nil, context: nil, hints: nil) else {
                    FileHandle.standardError.write(Data("snapshot failed: \(String(describing: error))\n".utf8))
                    exit(1)
                }
                for size in [16, 32, 64, 128, 256, 512, 1024] {
                    write(cg, size: size, to: "\(folder)/icon_\(size).png")
                }
                exit(0)
            }
        }
    }
}

func write(_ image: CGImage, size: Int, to path: String) {
    let ctx = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
                        space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.interpolationQuality = .high
    ctx.draw(image, in: CGRect(x: 0, y: 0, width: size, height: size))
    let rep = NSBitmapImageRep(cgImage: ctx.makeImage()!)
    try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: path))
}

let app = NSApplication.shared
let web = WKWebView(frame: NSRect(x: 0, y: 0, width: canvas, height: canvas))
web.setValue(false, forKey: "drawsBackground")
let loader = Loader()
web.navigationDelegate = loader
let sized = svg.replacingOccurrences(of: "<svg ", with: "<svg style=\"display:block;width:\(Int(canvas))px;height:\(Int(canvas))px\" ")
web.loadHTMLString("<html><body style=\"margin:0;background:transparent\">\(sized)</body></html>", baseURL: nil)
app.run()
