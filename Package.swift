// swift-tools-version: 6.0

import Foundation
import PackageDescription

let coreLibraryDirectory = ProcessInfo.processInfo.environment["WAGA_CORE_LIB_DIR"]
    ?? "target/release"
let binaryCore = "Artifacts/CWagaWebRTC.xcframework"
let packageDirectory = URL(fileURLWithPath: #filePath).deletingLastPathComponent().path
let hasBinaryCore = FileManager.default.fileExists(atPath: "\(packageDirectory)/\(binaryCore)")

let coreTarget: Target = hasBinaryCore
    ? .binaryTarget(name: "CWagaWebRTC", path: binaryCore)
    : .target(
        name: "CWagaWebRTC",
        path: "Sources/CWagaWebRTC",
        publicHeadersPath: "include",
        linkerSettings: [
            .unsafeFlags(["-L", coreLibraryDirectory]),
            .linkedLibrary("waga_webrtc_core"),
        ]
    )

let package = Package(
    name: "WagaWebRTC",
    platforms: [
        .iOS(.v15),
    ],
    products: [
        .library(name: "WagaWebRTC", targets: ["WagaWebRTC"]),
    ],
    targets: [
        coreTarget,
        .target(
            name: "WagaWebRTC",
            dependencies: ["CWagaWebRTC"],
            linkerSettings: [
                .linkedFramework("CryptoKit"),
                .linkedFramework("Security"),
            ]
        ),
        .testTarget(
            name: "WagaWebRTCTests",
            dependencies: ["WagaWebRTC"]
        ),
    ]
)
