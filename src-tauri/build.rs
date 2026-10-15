fn main() {
    tauri_build::build();
    #[cfg(target_os = "macos")]
    swift_rs::SwiftLinker::new("13.0")
        .with_package("PhotoBridge", "../native/PhotoBridge")
        .link();
}
