// swift-tools-version: 6.0
// Built by scripts/build.sh, which first compiles the Rust core and generates
// the UniFFI bindings into Sources/ghv_coreFFI and Sources/GitHubViewer/Generated.
import PackageDescription

let package = Package(
    name: "GitHubViewer",
    platforms: [.macOS(.v14)],
    targets: [
        // C header + module map for the Rust library's FFI symbols.
        .systemLibrary(name: "ghv_coreFFI", path: "Sources/ghv_coreFFI"),
        .executableTarget(
            name: "GitHubViewer",
            dependencies: ["ghv_coreFFI"],
            linkerSettings: [
                // RustLib holds only the static library, so the app has no runtime dylib dependency.
                .unsafeFlags(["-L\(Context.packageDirectory)/RustLib"]),
                .linkedLibrary("ghv_core"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
            ]
        ),
    ],
    // The generated bindings aren't written for Swift 6 strict concurrency.
    swiftLanguageModes: [.v5]
)
