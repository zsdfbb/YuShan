// ocr/vision.swift —— macOS Vision OCR 后端，输出与 ocr/tsv.rs 解析器**同列序**的 TSV。
//
// 【这是兼容性契约，不是实现细节】
// 本文件经 include_str! 内嵌进 Rust 二进制，并由 `swiftc -O` 现场编译（见
// docs/arch/ocr-tool/design.md §3.5：源码即资产，产物按源码哈希命名）。它不像普通脚本那样
// 可以随便改 —— 下面四点任一改动都会**静默**破坏 ocr/tsv.rs 的解析：
//
//   1. 列序（design.md §4.1）：固定 12 列、`\t` 分隔、一行一条：
//        level  page_num  block_num  par_num  line_num  word_num  left  top  width  height  conf  text
//      `level == 4` ⇒ 行记录，`level == 5` ⇒ 词记录，其余 level 一律不输出。
//      前 11 列必须能被 Rust 侧解析成整数；`text` 是第 12 列起的全部内容，其中不得出现 `\t`。
//   2. 输出顺序：先该行的 `level == 4` 记录，紧跟其 `level == 5` 词记录。tsv.rs 用
//      `current_line` 把词挂到最近一条行记录上；词记录出现在任何行记录之前 ⇒ 整条丢弃。
//   3. 坐标原点（design.md §9）：Vision 给的是 [0,1] 归一化、原点**左下**；本脚本一次性换算成
//      **原图像素、原点左上**。Rust 侧不做任何二次变换 —— 换算只在这里发生一次。
//   4. sanitize：文本里的 `\t` / `\r` / `\n`（及 Unicode 行/段分隔符）必须换成空格，
//      否则一条记录会被撑成多列，列序整体错位。
//
// 调用：`<binary> <image-path>`（只收一个参数 —— 语种由 Vision 自动判定，不从 argv 读 lang）。
// 输出：stdout 上是 TSV，stderr 上是警告（不影响 stdout）。
// 退出码：0 成功（「图里没字」也算成功）；2 参数缺失 / 图片无法解码 / 非 macOS；1 Vision 执行失败。

#if canImport(Vision)

import AppKit
import CoreGraphics
import Foundation
import ImageIO
import Vision

// MARK: - 契约常量

/// `level == 4` ⇒ 行框；`level == 5` ⇒ 词框。tsv.rs 只认这两个值。
private let levelLine = 4
private let levelWord = 5

/// Vision 的语言列表是**自动**行为（`automaticallyDetectsLanguage` + 多语种候选），
/// 这里只给出优先顺序，绝不来自命令行参数。
private let preferredLanguages = ["zh-Hans", "zh-Hant", "en-US"]

// MARK: - 输出

/// 把一条记录格式化成 TSV 行（含行尾 `\n`）。
///
/// `page_num` / `block_num` / `par_num` / `line_num` / `word_num` 五列固定 1：Vision 没有
/// 这些层级，Rust 侧也不读它们，但 tesseract 在这些位置同样吐 1，保持同形态便于逐字节比对。
private func record(level: Int, rect: (x: Int, y: Int, w: Int, h: Int), conf: Int, text: String) -> String {
    "\(level)\t1\t1\t1\t1\t1\t\(rect.x)\t\(rect.y)\t\(rect.w)\t\(rect.h)\t\(conf)\t\(text)\n"
}

private func warn(_ message: String) {
    FileHandle.standardError.write(Data("vision.swift: \(message)\n".utf8))
}

// MARK: - 文本与数值

/// 行内 `\t` / `\r` / `\n` 等换成空格 —— TSV 列序的唯一防线。
private func sanitize(_ raw: String) -> String {
    var out = ""
    out.reserveCapacity(raw.count)
    for character in raw {
        switch character {
        case "\t", "\r", "\n", "\u{0B}", "\u{0C}", "\u{1C}", "\u{1D}", "\u{1E}", "\u{2028}", "\u{2029}":
            out.append(" ")
        default:
            out.append(character)
        }
    }
    return out.trimmingCharacters(in: .whitespaces)
}

/// `VNConfidence` 是 `[0,1]` 的 `Float` ⇒ 0–100 的整数。
/// Vision 的词级候选通常不给可信置信度（返回 0 或 NaN）—— 拿不到就填 0，绝不编造；
/// 0 是 tesseract 语义下的有效值（tsv.rs 的过滤**不看** conf）。上限截到 100：`conf > 100`
/// 会被 Rust 侧当成脏列整条跳过。
private func confPercent(_ confidence: Float) -> Int {
    guard confidence.isFinite, confidence > 0 else { return 0 }
    return min(100, max(0, Int((Double(confidence) * 100).rounded())))
}

// MARK: - 坐标

/// 归一化（原点左下）⇒ 原图像素（原点左上）。
///
/// `x = minX * W`、`y = (1 - maxY) * H`、`w = width * W`、`h = height * H`；
/// 结果夹到图像范围内并保证 `x + w <= W`、`y + h <= H`（负值或越界的坐标会被 Rust 侧丢弃）。
private func pixelRect(_ box: CGRect, width W: Int, height H: Int) -> (x: Int, y: Int, w: Int, h: Int)? {
    guard !box.isNull, !box.isInfinite, box.width > 0, box.height > 0 else { return nil }

    func clamp(_ value: Double, _ upper: Int) -> Int {
        Int(min(Double(upper), max(0.0, value.rounded())))
    }

    let x = clamp(box.minX * Double(W), W)
    let y = clamp((1 - box.maxY) * Double(H), H)
    let w = clamp(box.width * Double(W), W - x)
    let h = clamp(box.height * Double(H), H - y)
    guard w > 0, h > 0 else { return nil }
    return (x, y, w, h)
}

/// 一个词：已 sanitize 的文本 + 像素框。
private struct WordBox {
    let text: String
    let rect: (x: Int, y: Int, w: Int, h: Int)
}

/// 按词范围取框。取不到框的范围直接跳过（不猜、不借用整行框）。
private func wordBoxes(in candidate: VNRecognizedText, width W: Int, height H: Int) -> [WordBox] {
    var boxes: [WordBox] = []
    let string = candidate.string
    string.enumerateSubstrings(in: string.startIndex..<string.endIndex, options: [.byWords]) { substring, range, _, _ in
        let text = sanitize(substring ?? String(string[range]))
        guard !text.isEmpty else { return }
        guard let observation = (try? candidate.boundingBox(for: range)) ?? nil else { return }
        guard let rect = pixelRect(observation.boundingBox, width: W, height: H) else { return }
        boxes.append(WordBox(text: text, rect: rect))
    }
    return boxes
}

// MARK: - 图像解码

private func loadImage(_ path: String) -> (image: CGImage, orientation: CGImagePropertyOrientation)? {
    if let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: path) as CFURL, nil),
       let image = CGImageSourceCreateImageAtIndex(source, 0, nil) {
        var orientation: CGImagePropertyOrientation = .up
        if let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
           let raw = properties[kCGImagePropertyOrientation] as? UInt32,
           let parsed = CGImagePropertyOrientation(rawValue: raw) {
            orientation = parsed
        }
        return (image, orientation)
    }
    // ImageIO 不解的位图交给 AppKit 再试一次（EXIF 方向已由 AppKit 烘进像素）。
    guard let image = NSImage(contentsOfFile: path) else { return nil }
    var proposed = NSRect(origin: .zero, size: image.size)
    guard let cgImage = image.cgImage(forProposedRect: &proposed, context: nil, hints: nil) else { return nil }
    return (cgImage, .up)
}

// MARK: - 识别请求

/// 识别语种：本地固定优先顺序，取系统支持的子集。**不从 argv 读 lang**（design.md §2.3：
/// Vision 下 `lang` 被忽略，头行回显 `n/a`）。系统一个都不支持时保持 Vision 默认，不置空列表。
private func configureLanguages(_ request: VNRecognizeTextRequest) {
    let supported: [String]
    if #available(macOS 12.0, *) {
        supported = (try? request.supportedRecognitionLanguages()) ?? []
    } else {
        supported = (try? VNRecognizeTextRequest.supportedRecognitionLanguages(
            for: request.recognitionLevel,
            revision: request.revision
        )) ?? []
    }
    let picked = preferredLanguages.filter { supported.contains($0) }
    if !picked.isEmpty {
        request.recognitionLanguages = picked
    }
    if #available(macOS 13.0, *) {
        request.automaticallyDetectsLanguage = true
    }
}

private func makeRequest() -> VNRecognizeTextRequest {
    let request = VNRecognizeTextRequest()
    request.recognitionLevel = .accurate
    request.usesLanguageCorrection = true
    configureLanguages(request)
    return request
}

// MARK: - 主流程

private func preview(_ text: String) -> String {
    text.count <= 40 ? text : String(text.prefix(40)) + "…"
}

private func run() -> Int32 {
    let arguments = CommandLine.arguments
    guard arguments.count == 2 else {
        warn("usage: vision <image-path>（需要且仅需要一个图片路径参数）")
        return 2
    }
    let path = arguments[1]
    guard let (image, orientation) = loadImage(path) else {
        warn("cannot decode image: \(path)")
        return 2
    }

    let request = makeRequest()
    do {
        try VNImageRequestHandler(cgImage: image, orientation: orientation, options: [:]).perform([request])
    } catch {
        warn("vision failed: \(error.localizedDescription)")
        return 1
    }

    let width = image.width
    let height = image.height
    var out = ""
    for observation in request.results ?? [] {
        guard let candidate = observation.topCandidates(1).first else { continue }
        let lineText = sanitize(candidate.string)
        // 空行不占记录：行号连续性是 Rust 侧按出现顺序自行重编的，这里只需不吐空文本。
        guard !lineText.isEmpty else { continue }
        guard let lineRect = pixelRect(observation.boundingBox, width: width, height: height) else { continue }

        let conf = confPercent(candidate.confidence)
        out += record(level: levelLine, rect: lineRect, conf: conf, text: lineText)

        let words = wordBoxes(in: candidate, width: width, height: height)
        if words.isEmpty {
            // 退化：这一行拿不到任何词框（竖排、极短行、引擎不给范围框）⇒ 整行当一个词框。
            // 粒度因此降级到行级，但列序与 level 语义不变；stderr 提示，stdout 不受影响。
            warn("no word boxes for line \"\(preview(lineText))\" — degenerating to a single word box")
            out += record(level: levelWord, rect: lineRect, conf: conf, text: lineText)
        } else {
            for word in words {
                out += record(level: levelWord, rect: word.rect, conf: conf, text: word.text)
            }
        }
    }

    FileHandle.standardOutput.write(Data(out.utf8))
    return 0
}

exit(run())

#else

// 非 macOS：Vision 框架不存在，契约要求以 2 退出（与「图片无法解码」同档）。
import Foundation

FileHandle.standardError.write(Data("vision.swift: 需要 macOS（Vision 框架不可用）\n".utf8))
exit(2)

#endif
