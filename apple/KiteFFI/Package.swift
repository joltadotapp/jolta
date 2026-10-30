// swift-tools-version:6.2
// UniFFI's generated Swift does not pass Swift 6 strict concurrency, so this
// package builds in Swift 5 mode and the app stays in Swift 6 mode.
import PackageDescription

let package = Package(
    name: "KiteFFI",
    platforms: [.macOS(.v26)],
    products: [
        .library(name: "KiteFFI", targets: ["KiteFFI"]),
    ],
    targets: [
        .binaryTarget(name: "kite_ffiFFI", path: "KiteFFI.xcframework"),
        .target(name: "KiteFFI", dependencies: ["kite_ffiFFI"]),
    ],
    swiftLanguageModes: [.v5]
)
