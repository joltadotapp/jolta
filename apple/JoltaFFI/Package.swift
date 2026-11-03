// swift-tools-version:6.2
// UniFFI's generated Swift does not pass Swift 6 strict concurrency, so this
// package builds in Swift 5 mode and the app stays in Swift 6 mode.
import PackageDescription

let package = Package(
    name: "JoltaFFI",
    platforms: [.macOS(.v26)],
    products: [
        .library(name: "JoltaFFI", targets: ["JoltaFFI"]),
    ],
    targets: [
        .binaryTarget(name: "jolta_ffiFFI", path: "JoltaFFI.xcframework"),
        .target(
            name: "JoltaFFI",
            dependencies: ["jolta_ffiFFI"],
            linkerSettings: [.linkedFramework("Security"), .linkedFramework("SystemConfiguration")]
        ),
    ],
    swiftLanguageModes: [.v5]
)
