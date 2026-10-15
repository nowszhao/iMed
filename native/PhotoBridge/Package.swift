// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "PhotoBridge",
    platforms: [.macOS(.v13)],
    products: [.library(name: "PhotoBridge", type: .static, targets: ["PhotoBridge"])],
    dependencies: [.package(url: "https://github.com/Brendonovich/swift-rs", from: "1.0.9")],
    targets: [.target(name: "PhotoBridge", dependencies: [.product(name: "SwiftRs", package: "swift-rs")])]
)
