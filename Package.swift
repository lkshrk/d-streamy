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
        .executableTarget(
            name: "DStreamyApp",
            path: "App",
            resources: [
                .process("Assets.xcassets"),
            ],
            linkerSettings: [
                .linkedFramework("AppKit"),
            ]
        ),
        .testTarget(
            name: "DStreamyAppTests",
            dependencies: ["DStreamyApp"],
            path: "AppTests"
        ),
    ]
)
