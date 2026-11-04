//! 图像媒体：容器格式嗅探与细节偏好的**纯函数**（零 IO、零依赖）。
//!
//! 本模块只吃前缀字节，不打开文件、不解码像素、不引入图像库。判定**只看内容**，
//! 不看扩展名 —— 一个叫 `x.png` 实为 JPEG 的文件必须判成 [`ImageFormat::Jpeg`]。
//!
//! 两条对外契约：
//!
//! - [`ImageDetail`] / [`ImageFormat`] 的 serde 形态是**小写**（`"high"` / `"png"`），
//!   因为它们会落进会话 JSONL、并出现在给模型/用户看的占位文案里。
//! - [`sniff_image`] **绝不 panic**：头不足、魔数不匹配、尺寸字段异常一律返回 `None`。

use serde::{Deserialize, Serialize};

/// 图像容器格式，按**内容**判定（不认扩展名）。
///
/// serde 形态是**对外契约**：落盘为 `"png"` / `"jpeg"` / `"webp"` 等小写形态，
/// 因为同一个拼写也出现在给用户看的占位文案（`(detected: bmp)`）里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    WebP,
    Bmp,
    Tiff,
}

impl ImageFormat {
    /// 该格式的 MIME 类型（出站请求里图像块声明的类型）。
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::WebP => "image/webp",
            Self::Bmp => "image/bmp",
            Self::Tiff => "image/tiff",
        }
    }

    /// 是否属于**视觉模型普遍接受**的四种（PNG / JPEG / GIF / WebP）。
    ///
    /// BMP / TIFF 判为 `false`：它们能被嗅探出尺寸，但不能直接送进视觉请求，
    /// 调用方据此改走文本占位而不是发出去等 provider 报错。
    pub fn is_vision_supported(self) -> bool {
        matches!(self, Self::Png | Self::Jpeg | Self::Gif | Self::WebP)
    }
}

/// 图像细节偏好的**意图值**。
///
/// 它是数据（用户/模型的意图），不是 wire 值：各 provider 的拼写由适配器翻译，
/// 不认识该字段的 provider 由适配器**丢弃**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    Low,
    /// 缺省档：取图多半是为了看清细节，所以它是 `Default`。
    #[default]
    High,
    Original,
    Auto,
}

/// 随工具结果一起回喂给模型的媒体附件。
///
/// 只存**路径与意图**，不存字节 —— 字节在适配器出站时现读（design.md §3.7）。
/// 落盘形态是对外契约：`{"Image":{"path":"…","detail":"high"}}`（外部标签，serde 默认表示）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MediaAttachment {
    Image {
        path: String,
        /// 省略该键等价于「用默认档位」；写 `null` 会与 `ContentBlock::Image` 的形态分叉。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ImageDetail>,
    },
}

/// 从文件头字节判定格式与像素尺寸。
///
/// 返回 `None` 表示：头不足、魔数不匹配、或尺寸字段异常（含任一维为 0）。
/// **绝不 panic** —— 所有切片都走 `.get(..)`，任何越界都退化成 `None`。
///
/// 支持 PNG / JPEG / GIF / WebP（VP8 / VP8L / VP8X）/ BMP / TIFF（小端与大端）。
/// 尺寸只读头里的字段，不解码像素：解压炸弹的防线不在本函数。
pub fn sniff_image(head: &[u8]) -> Option<(ImageFormat, u32, u32)> {
    let (format, dims) = if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        (ImageFormat::Png, png_dims(head)?)
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        (ImageFormat::Gif, gif_dims(head)?)
    } else if head.starts_with(b"BM") {
        (ImageFormat::Bmp, bmp_dims(head)?)
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(&b"WEBP"[..]) {
        (ImageFormat::WebP, webp_dims(head)?)
    } else if head.starts_with(b"II\x2a\x00") {
        // 小端 TIFF
        (ImageFormat::Tiff, tiff_dims(head, true)?)
    } else if head.starts_with(b"MM\x00\x2a") {
        // 大端 TIFF
        (ImageFormat::Tiff, tiff_dims(head, false)?)
    } else if head.starts_with(b"\xFF\xD8") {
        (ImageFormat::Jpeg, jpeg_dims(head)?)
    } else {
        return None;
    };

    // 任一维为 0 的「图片」没有像素可看，直接判无效（防畸形头被当成合法图往下传）。
    let (w, h) = dims;
    if w == 0 || h == 0 {
        return None;
    }
    Some((format, w, h))
}

/// PNG：`IHDR` 恒是第一个 chunk，宽高是 chunk 体内的两个大端 u32。
///
/// 文件布局：8 字节魔数 + 4 字节 chunk 长度 + `IHDR`(12..16) + 宽(16..20) + 高(20..24)。
fn png_dims(head: &[u8]) -> Option<(u32, u32)> {
    if head.get(12..16)? != b"IHDR".as_slice() {
        return None;
    }
    Some((be32(head.get(16..)?)?, be32(head.get(20..)?)?))
}

/// GIF：逻辑屏幕宽高是小端 u16（GIF 没有 32 位尺寸）。
fn gif_dims(head: &[u8]) -> Option<(u32, u32)> {
    let w = le16(head.get(6..)?)?;
    let h = le16(head.get(8..)?)?;
    Some((u32::from(w), u32::from(h)))
}

/// BMP：`BITMAPINFOHEADER` 的宽高是有符号 32 位小端，取绝对值。
///
/// 高度为负表示 top-down 位图（行序从上到下），其**绝对值**才是像素行数。
/// 偏移是文件偏移：`BM`(0..2) + `BITMAPFILEHEADER` 余下 12 字节 + DIB 头长度(14..18)
/// + 宽(18..22) + 高(22..26)。
fn bmp_dims(head: &[u8]) -> Option<(u32, u32)> {
    let w = le_i32(head.get(18..)?)?;
    let h = le_i32(head.get(22..)?)?;
    Some((w.unsigned_abs(), h.unsigned_abs()))
}

/// WebP：`RIFF` + `WEBP` 之后按子 chunk 分三种容器，宽高各在各自子头里。
///
/// 子格式签名在 12..16；各子格式字段的偏移都是**文件偏移**。
fn webp_dims(head: &[u8]) -> Option<(u32, u32)> {
    match head.get(12..16)? {
        // 有损：帧头里的 14 位宽 / 14 位高各带 2 位缩放，故用 `& 0x3FFF` 截出量级。
        b"VP8 " => {
            // 关键帧起始码 9d 01 2a 在 23..26；缺它就不是 VP8 关键帧头，尺寸字段无意义。
            if head.get(23..26)? != b"\x9d\x01\x2a" {
                return None;
            }
            let w = le16(head.get(26..28)?)? & 0x3FFF;
            let h = le16(head.get(28..30)?)? & 0x3FFF;
            Some((u32::from(w), u32::from(h)))
        }
        // 无损：签名 0x2F 之后的 4 字节位打包，宽-1 占低 14 位、高-1 占高 14 位。
        b"VP8L" => {
            let bits = le32(head.get(21..)?)?;
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        // 扩展：flags 之后是两个 24 位小端的「canvas 宽-1 / 高-1」。
        b"VP8X" => Some((le24(head.get(24..)?)? + 1, le24(head.get(27..)?)? + 1)),
        _ => None,
    }
}

/// TIFF：IFD0 里 tag 256 / 257 是宽高，`little` 决定字节序。
///
/// 布局：4 字节签名后的 4 字节是 IFD0 的**文件偏移**（相对文件起点，不是相对签名之后），
/// IFD0 自身是条目数（u16）+ 每项 12 字节的条目（tag / 类型 / 数量 / 值）。
/// 只有 SHORT(3) 与 LONG(4) 是内联在值字段里的标量 —— 其余类型（如 RATIONAL）一律不猜。
///
/// 条目区必须**完整**落入给定的头里：SHORT 的值只占值字段的低 2 字节，若只看这 2 字节，
/// 被截断的头也会给出一个看似合理的尺寸。要求整个 IFD0 条目区在头内，才满足
/// 「头不足 ⇒ `None`」的总契约。
fn tiff_dims(head: &[u8], little: bool) -> Option<(u32, u32)> {
    let read16 = |b: &[u8]| -> Option<u16> {
        let arr: [u8; 2] = b.get(..2)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(arr)
        } else {
            u16::from_be_bytes(arr)
        })
    };
    let read32 = |b: &[u8]| -> Option<u32> {
        let arr: [u8; 4] = b.get(..4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(arr)
        } else {
            u32::from_be_bytes(arr)
        })
    };

    let ifd = usize::try_from(read32(head.get(4..)?)?).ok()?;
    let ifd = head.get(ifd..)?;
    let count = usize::from(read16(ifd)?);
    // 先确认条目区完整（每项 12 字节），再去逐条取值。
    if ifd.len() < 2 + count * 12 {
        return None;
    }
    let mut width = None;
    let mut height = None;
    for i in 0..count {
        let entry = ifd.get(2 + i * 12..)?;
        let tag = read16(entry)?;
        let kind = read16(entry.get(2..)?)?;
        let value = match kind {
            3 => read16(entry.get(8..)?).map(u32::from),
            4 => read32(entry.get(8..)?),
            _ => None,
        };
        match tag {
            256 => width = value,
            257 => height = value,
            _ => {}
        }
    }
    Some((width?, height?))
}

/// JPEG：逐段走到 `SOFn`（`0xC0`–`0xCF`，排除 `C4` DHT / `C8` JPG / `CC` DAC），
/// 段内 +5..7 是高、+7..9 是宽（大端 u16，相对标记起始偏移）。
fn jpeg_dims(head: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 3 < head.len() {
        if *head.get(i)? != 0xFF {
            return None;
        }
        let marker = *head.get(i + 1)?;
        // 填充字节（连续 0xff）与无载荷的独立段都要跳过，否则长度解析会错位。
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if marker == 0x01 || marker == 0xD8 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        let len = usize::from(be16(head.get(i + 2..)?)?);
        let is_sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            // 段体：1 字节精度 + 2 字节高 + 2 字节宽。
            let seg = head.get(i + 4..)?;
            let h = be16(seg.get(1..)?)?;
            let w = be16(seg.get(3..)?)?;
            return Some((u32::from(w), u32::from(h)));
        }
        if len < 2 {
            return None;
        }
        i += 2 + len;
    }
    None
}

fn be16(b: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(..2)?.try_into().ok()?))
}

fn be32(b: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(..4)?.try_into().ok()?))
}

fn le16(b: &[u8]) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(..2)?.try_into().ok()?))
}

fn le32(b: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(..4)?.try_into().ok()?))
}

fn le_i32(b: &[u8]) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(..4)?.try_into().ok()?))
}

/// 24 位小端（WebP 扩展格式的 canvas 尺寸字段）。
fn le24(b: &[u8]) -> Option<u32> {
    let [b0, b1, b2] = b.get(..3)?.try_into().ok()?;
    Some(u32::from(b0) | u32::from(b1) << 8 | u32::from(b2) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 夹具：手工构造的**最小**合法头 ----
    //
    // 每个夹具的长度都恰是嗅探所需的最少字节数：这样「逐字节截断」的用例里，
    // 任何真前缀（长度 < len）都必然返回 `None`，不需要额外的长度阈值。

    fn png_head(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&13u32.to_be_bytes()); // chunk 长度（嗅探不读）
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v
    }

    fn jpeg_head(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08]; // SOI + SOF0 + 精度
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v
    }

    fn gif_head(w: u16, h: u16) -> Vec<u8> {
        let mut v = b"GIF89a".to_vec();
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v
    }

    /// 有损 WebP（`VP8 `）：起始码 9d 01 2a 之后是两个 14 位小端尺寸。
    fn webp_vp8_head(w: u16, h: u16) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&0u32.to_le_bytes()); // RIFF 长度（嗅探不读）
        v.extend_from_slice(b"WEBP");
        v.extend_from_slice(b"VP8 ");
        v.extend_from_slice(&10u32.to_le_bytes()); // chunk 长度（嗅探不读）
        v.extend_from_slice(&[0x00, 0x00, 0x00]); // 帧标签
        v.extend_from_slice(&[0x9D, 0x01, 0x2A]); // 关键帧起始码
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v
    }

    /// 无损 WebP（`VP8L`）：签名 0x2f 之后 4 字节位打包，宽-1 在低 14 位、高-1 在其上。
    fn webp_vp8l_head(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(b"WEBP");
        v.extend_from_slice(b"VP8L");
        v.extend_from_slice(&5u32.to_le_bytes());
        v.push(0x2F);
        let bits = ((w - 1) & 0x3FFF) | (((h - 1) & 0x3FFF) << 14);
        v.extend_from_slice(&bits.to_le_bytes());
        v
    }

    /// 扩展 WebP（`VP8X`）：flags 之后是 24 位小端的 canvas 宽-1 / 高-1。
    fn webp_vp8x_head(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(b"WEBP");
        v.extend_from_slice(b"VP8X");
        v.extend_from_slice(&10u32.to_le_bytes());
        v.extend_from_slice(&[0u8; 4]); // flags + reserved
        v.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
        v.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
        v
    }

    /// BMP：`BM` + 文件头其余 12 字节 + DIB 头长度 + 宽 + 高（后两者小端 i32）。
    fn bmp_head(w: i32, h: i32) -> Vec<u8> {
        let mut v = b"BM".to_vec();
        v.extend_from_slice(&[0u8; 12]);
        v.extend_from_slice(&40u32.to_le_bytes());
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v
    }

    /// TIFF：签名 + IFD0 偏移(8) + 条目数(2) + tag 256 / 257 两条。
    /// `kind` 是值类型：3 = SHORT（内联 2 字节）或 4 = LONG（内联 4 字节）。
    fn tiff_head(w: u32, h: u32, little: bool, kind: u16) -> Vec<u8> {
        fn u16b(v: u16, little: bool) -> [u8; 2] {
            if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        }
        fn u32b(v: u32, little: bool) -> [u8; 4] {
            if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        }
        let mut v = if little {
            b"II\x2a\x00".to_vec()
        } else {
            b"MM\x00\x2a".to_vec()
        };
        v.extend_from_slice(&u32b(8, little)); // IFD0 偏移
        v.extend_from_slice(&u16b(2, little)); // 条目数
        for (tag, value) in [(256u16, w), (257u16, h)] {
            v.extend_from_slice(&u16b(tag, little));
            v.extend_from_slice(&u16b(kind, little));
            v.extend_from_slice(&u32b(1, little)); // 数量
            let mut field = [0u8; 4];
            if kind == 3 {
                field[..2].copy_from_slice(&u16b(value as u16, little));
            } else {
                field.copy_from_slice(&u32b(value, little));
            }
            v.extend_from_slice(&field);
        }
        v
    }

    // ---- 正例 ----

    #[test]
    fn sniffs_all_six_formats() {
        let cases: Vec<(Vec<u8>, (ImageFormat, u32, u32))> = vec![
            (png_head(32, 16), (ImageFormat::Png, 32, 16)),
            (jpeg_head(100, 50), (ImageFormat::Jpeg, 100, 50)),
            (gif_head(640, 480), (ImageFormat::Gif, 640, 480)),
            (webp_vp8_head(300, 200), (ImageFormat::WebP, 300, 200)),
            (webp_vp8l_head(1024, 768), (ImageFormat::WebP, 1024, 768)),
            (webp_vp8x_head(4000, 3000), (ImageFormat::WebP, 4000, 3000)),
            (bmp_head(7, 9), (ImageFormat::Bmp, 7, 9)),
            (
                tiff_head(1280, 720, true, 3),
                (ImageFormat::Tiff, 1280, 720),
            ),
            (
                tiff_head(1280, 720, false, 4),
                (ImageFormat::Tiff, 1280, 720),
            ),
        ];
        for (head, want) in cases {
            assert_eq!(sniff_image(&head), Some(want), "头：{head:02X?}");
        }
    }

    /// BMP 的高度为负表示 top-down，像素行数是其绝对值。
    #[test]
    fn bmp_negative_height_means_top_down() {
        assert_eq!(
            sniff_image(&bmp_head(4, -8)),
            Some((ImageFormat::Bmp, 4, 8))
        );
    }

    /// VP8 的宽高字段带 2 位水平/垂直缩放，尺寸只在低 14 位 ⇒ 必须掩掉高 2 位。
    #[test]
    fn webp_vp8_requires_start_code() {
        // 起始码 9d 01 2a 差一个字节 ⇒ 不是 VP8 关键帧头，尺寸字段不可信。
        let mut head = webp_vp8_head(300, 200);
        head[25] = 0x2B;
        assert_eq!(
            head.get(23..26),
            Some(&b"\x9d\x01\x2b"[..]),
            "起始码应已被改坏"
        );
        assert_eq!(sniff_image(&head), None);
    }

    #[test]
    fn webp_vp8_masks_scale_bits() {
        assert_eq!(
            sniff_image(&webp_vp8_head(300 | 0xC000, 200 | 0x4000)),
            Some((ImageFormat::WebP, 300, 200))
        );
    }

    // ---- 反例 ----

    #[test]
    fn rejects_empty_and_non_image_heads() {
        assert_eq!(sniff_image(&[]), None);
        assert_eq!(sniff_image(b"hello world"), None);
        // 只差一个字节的近似魔数不应被认成 PNG。
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\x0b"), None);
    }

    #[test]
    fn rejects_truncated_heads() {
        let heads = [
            png_head(32, 16),
            jpeg_head(100, 50),
            gif_head(640, 480),
            webp_vp8_head(300, 200),
            webp_vp8l_head(1024, 768),
            webp_vp8x_head(4000, 3000),
            bmp_head(7, 9),
            tiff_head(1280, 720, true, 3),
            tiff_head(1280, 720, false, 4),
        ];
        for head in heads {
            let half = &head[..head.len() / 2];
            assert_eq!(sniff_image(half), None, "半截头：{half:02X?}");
        }
    }

    /// 尺寸为 0 的头一律判无效（防畸形文件把 0x0 当成合法图往下传）。
    #[test]
    fn rejects_zero_dimensions() {
        assert_eq!(sniff_image(&png_head(0, 16)), None);
        assert_eq!(sniff_image(&png_head(16, 0)), None);
        assert_eq!(sniff_image(&jpeg_head(0, 50)), None);
        assert_eq!(sniff_image(&jpeg_head(100, 0)), None);
        assert_eq!(sniff_image(&gif_head(0, 480)), None);
        assert_eq!(sniff_image(&webp_vp8_head(300, 0)), None);
        // VP8L / VP8X 的字段存的是「尺寸-1」，故最小可表达尺寸是 1x1；零尺寸只能由
        // 直接存尺寸的格式（PNG / JPEG / GIF / BMP / TIFF）构造出来。
        assert_eq!(sniff_image(&bmp_head(0, 9)), None);
        assert_eq!(sniff_image(&bmp_head(7, 0)), None);
        assert_eq!(sniff_image(&tiff_head(0, 720, true, 3)), None);
        assert_eq!(sniff_image(&tiff_head(1280, 0, false, 4)), None);
    }

    /// 判据是**内容**，不是扩展名：`.png` 后缀的文件实为 JPEG 时必须判成 `Jpeg`。
    #[test]
    fn jpeg_bytes_behind_png_name_are_jpeg() {
        let head = jpeg_head(64, 64);
        let sniffed = sniff_image(&head).expect("合法 JPEG 头");
        assert_eq!(sniffed.0, ImageFormat::Jpeg, "不得按扩展名判成 Png");
        assert_ne!(sniffed.0, ImageFormat::Png);
    }

    // ---- 类型契约 ----

    #[test]
    fn mime_strings() {
        assert_eq!(ImageFormat::Png.mime(), "image/png");
        assert_eq!(ImageFormat::Jpeg.mime(), "image/jpeg");
        assert_eq!(ImageFormat::Gif.mime(), "image/gif");
        assert_eq!(ImageFormat::WebP.mime(), "image/webp");
        assert_eq!(ImageFormat::Bmp.mime(), "image/bmp");
        assert_eq!(ImageFormat::Tiff.mime(), "image/tiff");
    }

    /// 视觉支持面：只有 PNG / JPEG / GIF / WebP 四种可入视觉请求。
    #[test]
    fn vision_support_gate() {
        for f in [
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::Gif,
            ImageFormat::WebP,
        ] {
            assert!(f.is_vision_supported(), "{f:?} 应当可入视觉请求");
        }
        for f in [ImageFormat::Bmp, ImageFormat::Tiff] {
            assert!(!f.is_vision_supported(), "{f:?} 不应可入视觉请求");
        }
    }

    /// serde 形态是**对外契约**：落盘小写，且 `ImageDetail` 缺省为 `High`。
    #[test]
    fn serde_form_is_lowercase() {
        assert_eq!(
            serde_json::to_string(&ImageDetail::High).unwrap(),
            "\"high\""
        );
        assert_eq!(ImageDetail::default(), ImageDetail::High);
        for (detail, wire) in [
            (ImageDetail::Low, "\"low\""),
            (ImageDetail::High, "\"high\""),
            (ImageDetail::Original, "\"original\""),
            (ImageDetail::Auto, "\"auto\""),
        ] {
            let json = serde_json::to_string(&detail).unwrap();
            assert_eq!(json, wire);
            assert_eq!(serde_json::from_str::<ImageDetail>(&json).unwrap(), detail);
        }

        for (format, wire) in [
            (ImageFormat::Png, "\"png\""),
            (ImageFormat::Jpeg, "\"jpeg\""),
            (ImageFormat::Gif, "\"gif\""),
            (ImageFormat::WebP, "\"webp\""),
            (ImageFormat::Bmp, "\"bmp\""),
            (ImageFormat::Tiff, "\"tiff\""),
        ] {
            let json = serde_json::to_string(&format).unwrap();
            assert_eq!(json, wire);
            assert_eq!(serde_json::from_str::<ImageFormat>(&json).unwrap(), format);
        }
    }

    // ---- 不 panic 的证明 ----

    /// 把每种格式的合法头逐字节截断到 0..len，**全部**返回 `None` 且不 panic。
    ///
    /// 夹具长度恰为嗅探所需的最小值 ⇒ 任何真前缀都缺字段，`None` 是唯一正确结果；
    /// 同时这条用例把「越界访问」这类 panic 源全部走一遍。
    #[test]
    fn every_prefix_of_a_valid_head_is_rejected_without_panicking() {
        let heads = [
            png_head(32, 16),
            jpeg_head(100, 50),
            gif_head(640, 480),
            webp_vp8_head(300, 200),
            webp_vp8l_head(1024, 768),
            webp_vp8x_head(4000, 3000),
            bmp_head(7, 9),
            tiff_head(1280, 720, true, 3),
            tiff_head(1280, 720, false, 4),
        ];
        for head in heads {
            // 全长是合法头，必须认得出来（否则下面的 `None` 断言没有意义）。
            assert!(sniff_image(&head).is_some(), "全长头应可判定：{head:02X?}");
            for n in 0..head.len() {
                assert_eq!(sniff_image(&head[..n]), None, "长度 {n} 的前缀应当被拒");
            }
        }
    }
}
