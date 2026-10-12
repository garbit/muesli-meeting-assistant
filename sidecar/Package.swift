// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "MuesliSidecar",
    platforms: [.macOS("14.4")],
    products: [
        .executable(name: "muesli-sidecar", targets: ["MuesliSidecar"])
    ],
    dependencies: [
        .package(url: "https://github.com/FluidInference/FluidAudio", exact: "0.17.7")
    ],
    targets: [
        .executableTarget(
            name: "MuesliSidecar",
            dependencies: [
                .product(name: "FluidAudio", package: "FluidAudio")
            ],
            path: "Sources/MuesliSidecar",
            swiftSettings: [
                .swiftLanguageMode(.v5)
            ],
            linkerSettings: [
                // Embed an Info.plist so the bare executable carries usage
                // descriptions when run outside the app bundle (spikes/dev).
                .unsafeFlags([
                    "-Xlinker", "-sectcreate",
                    "-Xlinker", "__TEXT",
                    "-Xlinker", "__info_plist",
                    "-Xlinker", "Sources/MuesliSidecar/Info.plist",
                ])
            ]
        )
    ]
)
