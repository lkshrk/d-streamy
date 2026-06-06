// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "D-Streamy",
    platforms: [
        .macOS(.v14)
    ],
    products: [
        .executable(name: "D-Streamy", targets: ["DStreamyApp"]),
    ],
    targets: [
        // Shared capture library
        .target(
            name: "CaptureLib",
            path: "capture/Sources/Capture",
            linkerSettings: [
                .linkedFramework("ScreenCaptureKit"),
                .linkedFramework("CoreMedia"),
                .linkedFramework("VideoToolbox"),
                .linkedFramework("AVFoundation"),
                .linkedFramework("AppKit"),
            ]
        ),
        // SwiftUI menu bar app
        .executableTarget(
            name: "DStreamyApp",
            dependencies: ["CaptureLib"],
            path: "App",
            resources: [
                .process("Assets.xcassets"),
            ],
            linkerSettings: [
                .linkedFramework("ScreenCaptureKit"),
                .linkedFramework("CoreMedia"),
                .linkedFramework("VideoToolbox"),
                .linkedFramework("AVFoundation"),
                .linkedFramework("AppKit"),
            ]
        ),
        .testTarget(
            name: "CaptureLibTests",
            dependencies: ["CaptureLib"],
            path: "capture/Tests/CaptureLibTests"
        ),
    ]
)
