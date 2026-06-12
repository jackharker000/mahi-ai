// swift-tools-version: 5.10
//
// MahiKit — the Swift seam over the Mahi Rust core.
//
// Two layers live in this package:
//
//   • `MahiKit`  — pure-Swift domain models, the `MahiEngineProtocol` abstraction,
//     `MahiClient` (the app-facing @MainActor ObservableObject), and
//     `MahiComputerController` (ScreenCaptureKit / Accessibility / CGEvent — the thing
//     that makes computer-use real on the Mac).
//
//   • `Mahi` + `MahiFFI` — the UniFFI-generated Swift bindings and the compiled Rust
//     core (`crates/mahi-ffi`). Both are *generated* by
//     `macos/scripts/build-xcframework.sh` and are only attached to the package when
//     the artifacts exist on disk, so the app builds and runs (against the
//     PreviewMock engine) before the Rust core has ever been compiled.

import Foundation
import PackageDescription

let packageDirectory = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let ffiArtifactPath = packageDirectory
    .appendingPathComponent("Artifacts/MahiFFI.xcframework").path
let ffiBindingsPath = packageDirectory
    .appendingPathComponent("Sources/Mahi/Mahi.swift").path

/// True once `scripts/build-xcframework.sh` has produced the Rust core + bindings.
let ffiAvailable = FileManager.default.fileExists(atPath: ffiArtifactPath)
    && FileManager.default.fileExists(atPath: ffiBindingsPath)

var targets: [Target] = [
    .target(
        name: "MahiKit",
        dependencies: ffiAvailable ? [.target(name: "Mahi")] : [],
        path: "Sources/MahiKit"
    ),
    .testTarget(
        name: "MahiKitTests",
        dependencies: ["MahiKit"],
        path: "Tests/MahiKitTests"
    ),
]

if ffiAvailable {
    targets.append(
        .target(name: "Mahi", dependencies: ["MahiFFI"], path: "Sources/Mahi")
    )
    targets.append(
        .binaryTarget(name: "MahiFFI", path: "Artifacts/MahiFFI.xcframework")
    )
}

let package = Package(
    name: "MahiKit",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "MahiKit", targets: ["MahiKit"])
    ],
    targets: targets
)
