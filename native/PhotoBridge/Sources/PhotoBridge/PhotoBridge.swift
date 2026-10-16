import AppKit
import CoreGraphics
import Foundation
import ImageIO
import ImageCaptureCore
import Photos
import SwiftRs
import UniformTypeIdentifiers

private func json(_ value: Any) -> String {
    guard JSONSerialization.isValidJSONObject(value),
          let bytes = try? JSONSerialization.data(withJSONObject: value, options: [.sortedKeys]),
          let string = String(data: bytes, encoding: .utf8) else { return "{}" }
    return string
}

private enum NativeAsyncCache {
    static let lock = NSLock()
    static let photoThumbnailBudget = 32 * 1024 * 1024
    static var photoThumbnailBytes = 0
    static var photoThumbnailClock: UInt64 = 0
    static var photoAuthorizationPending = false
    static var photoThumbnails: [String: String] = [:]
    static var photoThumbnailSizes: [String: Int] = [:]
    static var photoThumbnailAccess: [String: UInt64] = [:]
    static var photoThumbnailPending = Set<String>()
    static var photoImages: [String: String] = [:]
    static var photoImagePending = Set<String>()
    static var photoDeleteResults: [String: String] = [:]
    static var photoDeletePending = Set<String>()
}

private func photoAuthorizationName(_ status: PHAuthorizationStatus) -> String {
    switch status {
    case .authorized: "authorized"
    case .limited: "limited"
    case .denied: "denied"
    case .restricted: "restricted"
    default: "not_determined"
    }
}

private func photoPage(offset: Int, limit: Int, favoritesOnly: Bool = false) -> (assets: [PHAsset], total: Int) {
    let status = PHPhotoLibrary.authorizationStatus(for: .readWrite)
    guard status == .authorized || status == .limited else { return ([], 0) }
    let options = PHFetchOptions()
    options.sortDescriptors = [NSSortDescriptor(key: "creationDate", ascending: false)]
    let images = NSPredicate(format: "mediaType == %d", PHAssetMediaType.image.rawValue)
    options.predicate = favoritesOnly
        ? NSCompoundPredicate(andPredicateWithSubpredicates: [images, NSPredicate(format: "isFavorite == YES")])
        : images
    let result = PHAsset.fetchAssets(with: options)
    let start = max(0, offset)
    let end = min(result.count, start + min(200, max(1, limit)))
    var assets: [PHAsset] = []
    if start < end {
        for index in start..<end { assets.append(result.object(at: index)) }
    }
    return (assets, result.count)
}

@_cdecl("shiguang_photos_status")
public func shiguangPhotosStatus() -> SRString {
    let status = PHPhotoLibrary.authorizationStatus(for: .readWrite)
    return SRString(json(["status": photoAuthorizationName(status), "count": photoPage(offset: 0, limit: 1).total]))
}

@_cdecl("shiguang_photos_request_authorization")
public func shiguangPhotosRequestAuthorization() -> SRString {
    let status = PHPhotoLibrary.authorizationStatus(for: .readWrite)
    if status == .notDetermined {
        DispatchQueue.main.async {
            PHPhotoLibrary.requestAuthorization(for: .readWrite) { _ in }
        }
    }
    return SRString(json(["status": photoAuthorizationName(status)]))
}

@_cdecl("shiguang_photos_list")
public func shiguangPhotosList(_ offset: Int, _ limit: Int, _ favorites: Int) -> SRString {
    let page = photoPage(offset: Int(offset), limit: Int(limit), favoritesOnly: favorites != 0)
    let rows: [[String: Any]] = page.assets.map { asset in
        ["id": asset.localIdentifier, "source": "photos",
         "name": asset.value(forKey: "filename") as? String ?? "照片",
         "date": asset.creationDate.map { ISO8601DateFormatter().string(from: $0) } ?? "",
         "width": asset.pixelWidth, "height": asset.pixelHeight, "favorite": asset.isFavorite,
         "mediaType": asset.mediaType == .video ? "video" : "image"]
    }
    return SRString(json(["status": "ok", "total": page.total, "assets": rows]))
}

private func jpegThumbnail(_ image: NSImage, quality: CGFloat = 0.82) -> Data? {
    guard let tiff = image.tiffRepresentation, let bitmap = NSBitmapImageRep(data: tiff) else { return nil }
    return bitmap.representation(using: .jpeg, properties: [.compressionFactor: quality])
}

private func imageDataURL(_ data: Data, uniformTypeIdentifier: String?) -> String {
    let mime = uniformTypeIdentifier.flatMap { UTType($0)?.preferredMIMEType } ?? "application/octet-stream"
    return "data:\(mime);base64,\(data.base64EncodedString())"
}

// ImageIO decodes the original asset at a useful model input size while preserving
// EXIF orientation and aspect ratio. DigUp prepares regular photos at 1024 px
// before embedding them with its 140-token photo budget.
private func embeddingJPEG(_ original: Data, maxDimension: Int = 1024) -> Data? {
    guard let source = CGImageSourceCreateWithData(original as CFData, nil) else { return nil }
    let options: [CFString: Any] = [
        kCGImageSourceCreateThumbnailFromImageAlways: true,
        kCGImageSourceCreateThumbnailWithTransform: true,
        kCGImageSourceThumbnailMaxPixelSize: maxDimension,
        kCGImageSourceShouldCacheImmediately: true,
    ]
    guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else { return nil }
    let output = NSMutableData()
    guard let destination = CGImageDestinationCreateWithData(output, UTType.jpeg.identifier as CFString, 1, nil) else { return nil }
    CGImageDestinationAddImage(destination, image, [kCGImageDestinationLossyCompressionQuality: 0.97] as CFDictionary)
    guard CGImageDestinationFinalize(destination) else { return nil }
    return output as Data
}

private func requestPhotoImage(_ asset: PHAsset, forEmbedding: Bool = false) -> String {
    let key = asset.localIdentifier + (forEmbedding ? ":embedding" : ":original")
    NativeAsyncCache.lock.lock()
    if let result = NativeAsyncCache.photoImages.removeValue(forKey: key) {
        NativeAsyncCache.lock.unlock()
        return result
    }
    let shouldRequest = !NativeAsyncCache.photoImagePending.contains(key)
    if shouldRequest { NativeAsyncCache.photoImagePending.insert(key) }
    NativeAsyncCache.lock.unlock()
    guard shouldRequest else { return "" }

    let resources = PHAssetResource.assetResources(for: asset)
    guard let resource = resources.first(where: { $0.type == .fullSizePhoto })
        ?? resources.first(where: { $0.type == .photo }) else {
        NativeAsyncCache.lock.lock()
        NativeAsyncCache.photoImages[key] = "ERR:照片原始资源不可用"
        NativeAsyncCache.photoImagePending.remove(key)
        NativeAsyncCache.lock.unlock()
        return ""
    }
    let options = PHAssetResourceRequestOptions()
    options.isNetworkAccessAllowed = true
    var original = Data()
    PHAssetResourceManager.default().requestData(for: resource, options: options) { chunk in
        original.append(chunk)
    } completionHandler: { error in
        let result: String
        if let error {
            result = "ERR:" + error.localizedDescription
        } else if forEmbedding, let jpeg = embeddingJPEG(original) {
            result = imageDataURL(jpeg, uniformTypeIdentifier: UTType.jpeg.identifier)
        } else if forEmbedding {
            result = "ERR:无法解码照片原图以建立向量"
        } else {
            result = imageDataURL(original, uniformTypeIdentifier: resource.uniformTypeIdentifier)
        }
        NativeAsyncCache.lock.lock()
        NativeAsyncCache.photoImages[key] = result
        NativeAsyncCache.photoImagePending.remove(key)
        NativeAsyncCache.lock.unlock()
    }
    return ""
}

private func requestPhotoThumbnail(_ asset: PHAsset) -> String? {
    let id = asset.localIdentifier
    NativeAsyncCache.lock.lock()
    if let cached = NativeAsyncCache.photoThumbnails[id] {
        NativeAsyncCache.photoThumbnailClock &+= 1
        NativeAsyncCache.photoThumbnailAccess[id] = NativeAsyncCache.photoThumbnailClock
        NativeAsyncCache.lock.unlock()
        return cached
    }
    let shouldRequest = !NativeAsyncCache.photoThumbnailPending.contains(id)
    if shouldRequest { NativeAsyncCache.photoThumbnailPending.insert(id) }
    NativeAsyncCache.lock.unlock()
    if shouldRequest {
        let semaphore = DispatchSemaphore(value: 0)
        let options = PHImageRequestOptions()
        options.deliveryMode = .highQualityFormat
        options.resizeMode = .fast
        options.isNetworkAccessAllowed = true
        PHImageManager.default().requestImage(for: asset, targetSize: CGSize(width: 800, height: 800),
                                              contentMode: .aspectFit, options: options) { image, info in
            let degraded = (info?[PHImageResultIsDegradedKey] as? Bool) == true
            guard !degraded else { return }
            let data = image.flatMap { jpegThumbnail($0) }
            NativeAsyncCache.lock.lock()
            if let data {
                NativeAsyncCache.photoThumbnails[id] = data.base64EncodedString()
                NativeAsyncCache.photoThumbnailSizes[id] = data.count
                NativeAsyncCache.photoThumbnailClock &+= 1
                NativeAsyncCache.photoThumbnailAccess[id] = NativeAsyncCache.photoThumbnailClock
                NativeAsyncCache.photoThumbnailBytes += data.count
                while NativeAsyncCache.photoThumbnailBytes > NativeAsyncCache.photoThumbnailBudget,
                      let oldest = NativeAsyncCache.photoThumbnailAccess.min(by: { $0.value < $1.value })?.key {
                    NativeAsyncCache.photoThumbnailBytes -= NativeAsyncCache.photoThumbnailSizes.removeValue(forKey: oldest) ?? 0
                    NativeAsyncCache.photoThumbnailAccess.removeValue(forKey: oldest)
                    NativeAsyncCache.photoThumbnails.removeValue(forKey: oldest)
                }
            }
            NativeAsyncCache.photoThumbnailPending.remove(id)
            NativeAsyncCache.lock.unlock()
            semaphore.signal()
        }
        guard semaphore.wait(timeout: .now() + 90) == .success else { return nil }
    } else {
        let deadline = Date().addingTimeInterval(90)
        while Date() < deadline {
            NativeAsyncCache.lock.lock()
            let cached = NativeAsyncCache.photoThumbnails[id]
            let pending = NativeAsyncCache.photoThumbnailPending.contains(id)
            NativeAsyncCache.lock.unlock()
            if let cached { return cached }
            if !pending { return nil }
            Thread.sleep(forTimeInterval: 0.025)
        }
        return nil
    }
    NativeAsyncCache.lock.lock()
    defer { NativeAsyncCache.lock.unlock() }
    return NativeAsyncCache.photoThumbnails[id]
}

@_cdecl("shiguang_photo_thumbnail")
public func shiguangPhotoThumbnail(_ rawID: SRString) -> SRString {
    let id = rawID.toString()
    guard let asset = PHAsset.fetchAssets(withLocalIdentifiers: [id], options: nil).firstObject else { return SRString("") }
    return SRString(requestPhotoThumbnail(asset) ?? "")
}

@_cdecl("shiguang_photo_image")
public func shiguangPhotoImage(_ rawID: SRString) -> SRString {
    let id = rawID.toString()
    guard let asset = PHAsset.fetchAssets(withLocalIdentifiers: [id], options: nil).firstObject else {
        return SRString("ERR:照片已不在当前图库中")
    }
    return SRString(requestPhotoImage(asset))
}

@_cdecl("shiguang_photo_embedding_image")
public func shiguangPhotoEmbeddingImage(_ rawID: SRString) -> SRString {
    let id = rawID.toString()
    guard let asset = PHAsset.fetchAssets(withLocalIdentifiers: [id], options: nil).firstObject else {
        return SRString("ERR:照片已不在当前图库中")
    }
    return SRString(requestPhotoImage(asset, forEmbedding: true))
}

private func photoDelete(_ ids: [String]) -> String {
    let key = ids.sorted().joined(separator: "\u{0}")
    NativeAsyncCache.lock.lock()
    if let completed = NativeAsyncCache.photoDeleteResults.removeValue(forKey: key) {
        NativeAsyncCache.lock.unlock()
        return completed
    }
    let shouldStart = !NativeAsyncCache.photoDeletePending.contains(key)
    if shouldStart { NativeAsyncCache.photoDeletePending.insert(key) }
    NativeAsyncCache.lock.unlock()
    guard shouldStart else { return json(["pending": true]) }

    let assets = PHAsset.fetchAssets(withLocalIdentifiers: ids, options: nil)
    guard assets.count == ids.count else {
        NativeAsyncCache.lock.lock()
        NativeAsyncCache.photoDeletePending.remove(key)
        NativeAsyncCache.photoDeleteResults[key] = json(["ok": false, "error": "部分照片已不在当前图库，请刷新来源后重试"])
        NativeAsyncCache.lock.unlock()
        return json(["pending": true])
    }
    PHPhotoLibrary.shared().performChanges {
        PHAssetChangeRequest.deleteAssets(assets)
    } completionHandler: { success, error in
        let result = success
            ? json(["ok": true, "deleted": ids.count])
            : json(["ok": false, "error": error?.localizedDescription ?? "系统照片图库拒绝了删除请求"])
        NativeAsyncCache.lock.lock()
        NativeAsyncCache.photoDeletePending.remove(key)
        NativeAsyncCache.photoDeleteResults[key] = result
        NativeAsyncCache.lock.unlock()
    }
    return json(["pending": true])
}

@_cdecl("shiguang_photos_delete")
public func shiguangPhotosDelete(_ rawIDs: SRString) -> SRString {
    let ids = (try? JSONSerialization.jsonObject(with: Data(rawIDs.toString().utf8))) as? [String] ?? []
    return SRString(photoDelete(ids))
}

private final class USBSession: NSObject, ICDeviceBrowserDelegate, ICDeviceDelegate, ICCameraDeviceDelegate {
    static let shared = USBSession()
    private var browser: ICDeviceBrowser?
    private(set) var camera: ICCameraDevice?
    private var files: [String: ICCameraFile] = [:]
    private var ready = false
    private var searching = false
    private var lastError: String?
    private var deviceID = "iphone"

    private let snapshotLock = NSLock()
    private var listSnapshot = "{\"status\":\"searching\",\"assets\":[]}"
    private var statusSnapshot = "{\"status\":\"searching\"}"
    private var allRows: [[String: Any]] = []
    private let thumbnailBudget = 32 * 1024 * 1024
    private var thumbnailBytes = 0
    private var thumbnailClock: UInt64 = 0
    private var thumbnailResults: [String: String] = [:]
    private var thumbnailSizes: [String: Int] = [:]
    private var thumbnailAccess: [String: UInt64] = [:]
    private var thumbnailPending = Set<String>()
    private var imageResults: [String: String] = [:]
    private var imagePending = Set<String>()
    private var deleteResults: [String: String] = [:]
    private var deletePending = Set<String>()

    private func saveSnapshots() {
        guard Thread.isMainThread else {
            DispatchQueue.main.async { [weak self] in self?.saveSnapshots() }
            return
        }
        let connected = camera != nil
        let state = lastError != nil ? "error" : connected ? (ready ? "connected" : "indexing") : (searching ? "searching" : "disconnected")
        let status: [String: Any] = ["status": state,
                                     "error": lastError ?? (connected ? "" : (searching ? "正在查找已连接的 iPhone" : "请连接并解锁 iPhone")),
                                     "device": camera?.name ?? "iPhone"]
        let rows: [[String: Any]] = ready ? files.values.map { file in
            ["id": identifier(for: file), "source": "usb", "name": file.name ?? "iPhone 照片",
             "date": file.creationDate.map { ISO8601DateFormatter().string(from: $0) } ?? "",
             "width": file.width, "height": file.height, "favorite": false, "mediaType": "image"] as [String: Any]
        }.sorted { left, right in
            let leftDate = (left["date"] as? String) ?? ""
            let rightDate = (right["date"] as? String) ?? ""
            if leftDate != rightDate { return leftDate > rightDate }
            return (left["name"] as? String ?? "") < (right["name"] as? String ?? "")
        } : []
        let list = json(["status": state, "error": status["error"] ?? "", "device": camera?.name ?? "iPhone", "total": rows.count, "assets": Array(rows.prefix(48))])
        let statusJSON = json(status)
        snapshotLock.lock()
        allRows = rows
        listSnapshot = list
        statusSnapshot = statusJSON
        snapshotLock.unlock()
    }

    private func startOnMain() {
        guard browser == nil else { return }
        lastError = nil
        let browser = ICDeviceBrowser()
        self.browser = browser
        browser.delegate = self
        browser.browsedDeviceTypeMask = ICDeviceTypeMask(rawValue: 0x101)!
        searching = true
        browser.start()
        saveSnapshots()
    }

    func deviceBrowser(_ browser: ICDeviceBrowser, didAdd device: ICDevice, moreComing: Bool) {
        guard let camera = device as? ICCameraDevice else { return }
        guard camera.transportType == ICDeviceTransport.transportTypeUSB.rawValue else { return }
        self.camera = camera
        deviceID = camera.uuidString ?? camera.name ?? "iphone"
        searching = false
        ready = false
        camera.delegate = self
        camera.requestOpenSession(options: nil) { [weak self] error in
            if let error { self?.lastError = error.localizedDescription }
            self?.saveSnapshots()
        }
        saveSnapshots()
    }

    func deviceBrowser(_ browser: ICDeviceBrowser, didRemove device: ICDevice, moreGoing: Bool) {
        if (device as? ICCameraDevice) === camera {
            camera = nil; ready = false; searching = true; lastError = nil; files.removeAll()
            saveSnapshots()
        }
    }

    func deviceBrowserDidEnumerateLocalDevices(_ browser: ICDeviceBrowser) {
        if camera == nil { searching = false }
        saveSnapshots()
    }

    func didRemove(_ device: ICDevice) {
        if (device as? ICCameraDevice) === camera { camera = nil; ready = false; searching = true; lastError = nil; files.removeAll(); saveSnapshots() }
    }

    func deviceDidBecomeReady(withCompleteContentCatalog device: ICCameraDevice) {
        let camera = device
        if let items = camera.mediaFiles { index(items) }
        lastError = nil
        ready = true
        saveSnapshots()
    }

    func cameraDevice(_ camera: ICCameraDevice, didAdd items: [ICCameraItem]) { index(items); saveSnapshots() }

    func cameraDevice(_ camera: ICCameraDevice, didRemove items: [ICCameraItem]) { remove(items); saveSnapshots() }

    func cameraDevice(_ camera: ICCameraDevice, didRenameItems items: [ICCameraItem]) { index(items); saveSnapshots() }

    private func remove(_ items: [ICCameraItem]) {
        for item in items {
            if let file = item as? ICCameraFile { files.removeValue(forKey: identifier(for: file)) }
        }
    }

    private func identifier(for file: ICCameraFile) -> String {
        "\(deviceID):\(file.ptpObjectHandle):\(file.name ?? "image")"
    }

    func cameraDeviceDidRemoveAccessRestriction(_ device: ICDevice) { if let camera = device as? ICCameraDevice, let items = camera.mediaFiles { index(items) } }

    func cameraDeviceDidEnableAccessRestriction(_ device: ICDevice) { ready = false }
    func cameraDevice(_ camera: ICCameraDevice, didReceiveThumbnail thumbnail: CGImage?, for item: ICCameraItem, error: Error?) {}
    func cameraDevice(_ camera: ICCameraDevice, didReceiveMetadata metadata: [AnyHashable: Any]?, for item: ICCameraItem, error: Error?) {}
    func device(_ device: ICDevice, didOpenSessionWithError error: Error?) {
        if let error { lastError = error.localizedDescription; ready = false }
        saveSnapshots()
    }
    func device(_ device: ICDevice, didCloseSessionWithError error: Error?) {
        if let error { lastError = error.localizedDescription; ready = false }
        saveSnapshots()
    }
    func device(_ device: ICDevice, didEncounterError error: Error?) {
        if let error { lastError = error.localizedDescription; ready = false }
        saveSnapshots()
    }
    func deviceDidBecomeReady(_ device: ICDevice) { }
    func device(_ device: ICDevice, didReceiveStatusInformation status: [ICDeviceStatus : Any]) { }
    func cameraDeviceDidChangeCapability(_ camera: ICCameraDevice) {}
    func cameraDevice(_ camera: ICCameraDevice, didReceivePTPEvent eventData: Data) {}

    private func index(_ items: [ICCameraItem]) {
        for item in items {
            guard let file = item as? ICCameraFile,
                  let rawUTI = file.uti,
                  UTType(rawUTI)?.conforms(to: .image) == true else { continue }
            files[identifier(for: file)] = file
        }
    }

    func list(offset: Int, limit: Int) -> String {
        DispatchQueue.main.async { [weak self] in
            self?.startOnMain()
            self?.saveSnapshots()
        }
        snapshotLock.lock(); defer { snapshotLock.unlock() }
        let state = statusSnapshot.data(using: .utf8).flatMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] } ?? [:]
        let start = min(max(0, offset), allRows.count)
        let end = min(allRows.count, start + min(200, max(1, limit)))
        let rows = start < end ? Array(allRows[start..<end]) : []
        return json(["status": state["status"] ?? "searching", "error": state["error"] ?? "", "device": state["device"] ?? "iPhone", "total": allRows.count, "assets": rows])
    }

    func status() -> String {
        DispatchQueue.main.async { [weak self] in
            self?.startOnMain()
            self?.saveSnapshots()
        }
        snapshotLock.lock(); defer { snapshotLock.unlock() }
        return statusSnapshot
    }

    func thumbnail(_ identifier: String) -> String {
        snapshotLock.lock()
        if let cached = thumbnailResults[identifier] {
            thumbnailClock &+= 1
            thumbnailAccess[identifier] = thumbnailClock
            snapshotLock.unlock()
            return cached
        }
        let shouldRequest = !thumbnailPending.contains(identifier)
        if shouldRequest { thumbnailPending.insert(identifier) }
        snapshotLock.unlock()
        if shouldRequest {
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
            startOnMain()
                guard let file = files[identifier] else {
                    self.snapshotLock.lock(); self.thumbnailPending.remove(identifier); self.snapshotLock.unlock()
                    return
                }
                if let image = file.thumbnail, let data = jpegThumbnail(NSImage(cgImage: image, size: .zero), quality: 0.8) {
                    self.snapshotLock.lock()
                    self.cacheThumbnailLocked(identifier, data: data)
                    self.thumbnailPending.remove(identifier)
                    self.snapshotLock.unlock()
                    return
                }
                file.requestThumbnailData(options: nil) { [weak self] data, error in
                    guard let self else { return }
                    self.snapshotLock.lock()
                    if let data, error == nil { self.cacheThumbnailLocked(identifier, data: data) }
                    self.thumbnailPending.remove(identifier)
                    self.snapshotLock.unlock()
                }
            }
        }
        return ""
    }

    func image(_ identifier: String, forEmbedding: Bool = false) -> String {
        let key = identifier + (forEmbedding ? ":embedding" : ":original")
        snapshotLock.lock()
        if let result = imageResults.removeValue(forKey: key) {
            snapshotLock.unlock()
            return result
        }
        let shouldRequest = !imagePending.contains(key)
        if shouldRequest { imagePending.insert(key) }
        snapshotLock.unlock()
        guard shouldRequest else { return "" }

        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.startOnMain()
            guard let file = self.files[identifier] else {
                self.finishImage(key, result: "ERR:iPhone 照片已不可用，请重新连接设备")
                return
            }
            guard file.fileSize > 0 else {
                self.finishImage(key, result: "ERR:照片原始文件为空")
                return
            }
            file.requestReadData(atOffset: 0, length: file.fileSize) { data, error in
                let result: String
                if let data, forEmbedding, let jpeg = embeddingJPEG(data) {
                    result = imageDataURL(jpeg, uniformTypeIdentifier: UTType.jpeg.identifier)
                } else if let data, forEmbedding {
                    result = "ERR:无法解码照片原图以建立向量"
                } else if let data {
                    result = imageDataURL(data, uniformTypeIdentifier: file.uti)
                } else {
                    result = "ERR:" + (error?.localizedDescription ?? "无法读取 iPhone 照片原图")
                }
                self.finishImage(key, result: result)
            }
        }
        return ""
    }

    private func finishImage(_ key: String, result: String) {
        snapshotLock.lock()
        imageResults[key] = result
        imagePending.remove(key)
        snapshotLock.unlock()
    }

    private func cacheThumbnailLocked(_ identifier: String, data: Data) {
        if let oldSize = thumbnailSizes[identifier] { thumbnailBytes -= oldSize }
        thumbnailResults[identifier] = data.base64EncodedString()
        thumbnailSizes[identifier] = data.count
        thumbnailClock &+= 1
        thumbnailAccess[identifier] = thumbnailClock
        thumbnailBytes += data.count
        while thumbnailBytes > thumbnailBudget,
              let oldest = thumbnailAccess.min(by: { $0.value < $1.value })?.key {
            thumbnailBytes -= thumbnailSizes.removeValue(forKey: oldest) ?? 0
            thumbnailAccess.removeValue(forKey: oldest)
            thumbnailResults.removeValue(forKey: oldest)
        }
    }

    func delete(_ identifiers: [String]) -> String {
        let requestKey = identifiers.sorted().joined(separator: "\u{0}")
        snapshotLock.lock()
        if let completed = deleteResults.removeValue(forKey: requestKey) { snapshotLock.unlock(); return completed }
        let shouldStart = !deletePending.contains(requestKey)
        if shouldStart { deletePending.insert(requestKey) }
        snapshotLock.unlock()
        guard shouldStart else { return json(["pending": true]) }
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            startOnMain()
            guard let camera else {
                self.finishDelete(requestKey, result: json(["ok": false, "error": lastError ?? "iPhone 尚未连接", "deletedIDs": []]))
                return
            }
            let selected = identifiers.compactMap { self.files[$0] }
            guard selected.count == identifiers.count, !selected.isEmpty else {
                self.finishDelete(requestKey, result: json(["ok": false, "deletedIDs": [], "error": "部分照片已不在 iPhone 上，请刷新来源后重试"]))
                return
            }
            var deletedIDs: [String] = []
            camera.requestDeleteFiles(selected, deleteFailed: { errors in
                if !errors.isEmpty { /* Individual completion results below determine which items were removed. */ }
            }, completion: { result, error in
                let success = result[.successful] ?? []
                for item in success {
                    if let file = item as? ICCameraFile {
                        let id = self.identifier(for: file)
                        deletedIDs.append(id)
                        self.files.removeValue(forKey: id)
                    }
                }
                self.saveSnapshots()
                self.finishDelete(requestKey, result: json(["ok": error == nil && deletedIDs.count == identifiers.count,
                                                             "deletedIDs": deletedIDs,
                                                             "error": error?.localizedDescription ?? (deletedIDs.count == identifiers.count ? "" : "部分照片未能删除，请检查 iPhone 是否解锁")]))
            })
        }
        return json(["pending": true])
    }

    private func finishDelete(_ key: String, result: String) {
        snapshotLock.lock()
        deletePending.remove(key)
        deleteResults[key] = result
        snapshotLock.unlock()
    }
}

@_cdecl("shiguang_usb_list")
public func shiguangUSBList(_ offset: Int, _ limit: Int) -> SRString { SRString(USBSession.shared.list(offset: Int(offset), limit: Int(limit))) }

@_cdecl("shiguang_usb_status")
public func shiguangUSBStatus() -> SRString { SRString(USBSession.shared.status()) }

@_cdecl("shiguang_usb_thumbnail")
public func shiguangUSBThumbnail(_ rawID: SRString) -> SRString { SRString(USBSession.shared.thumbnail(rawID.toString())) }

@_cdecl("shiguang_usb_image")
public func shiguangUSBImage(_ rawID: SRString) -> SRString {
    SRString(USBSession.shared.image(rawID.toString()))
}

@_cdecl("shiguang_usb_embedding_image")
public func shiguangUSBEmbeddingImage(_ rawID: SRString) -> SRString {
    SRString(USBSession.shared.image(rawID.toString(), forEmbedding: true))
}

@_cdecl("shiguang_usb_delete")
public func shiguangUSBDelete(_ rawIDs: SRString) -> SRString {
    let ids = (try? JSONSerialization.jsonObject(with: Data(rawIDs.toString().utf8))) as? [String] ?? []
    return SRString(USBSession.shared.delete(ids))
}
