//! `view_image` 工具：声明「这张图该看像素」，把**引用**交给适配器（设计 `docs/arch/vision-input/design.md` §3.3）。
//!
//! # 本工具不碰图像字节（D2）
//!
//! 它只做三件不花钱的事：解析路径、校验形态、读**文件头**（≤ [`HEAD_BYTES`]）嗅探格式与尺寸，
//! 然后返回一个 [`MediaAttachment::Image`]。像素的读取与 base64 编码**只**发生在适配器出站时
//! —— 那是全链路唯一的 IO 点。工具层若自己编码，`ToolResult.content` 会变成几 MB 的
//! data URL 灌进会话历史与调试日志（C4）。
//!
//! 于是 `content` 恒为**一行有界元信息**：
//!
//! ```text
//! image: /tmp/a.png (image/png, 1920×1080, 1.4 MiB) — attached as pixels, not text
//! ```
//!
//! # 失败只报事实，不做降级
//!
//! 不存在 / 不是常规文件 / 嗅探不出图片 / 编码后超 [`MAX_BYTES`] ⇒ `is_error: true` + 可读原因，
//! 且 `media` **必须为空**（`is_error` 为真时 `media` 恒空是跨层硬不变量）。
//! 降级文案（`[image unreadable: …]` 之类）由适配器在出站副本上生成，工具层不越界。
//!
//! # 与 `ocr` 并存
//!
//! 两者职责不同：本工具给**像素**，`ocr` 给**文字 + 像素坐标**。description 只陈述自己的
//! 适用场景，不声明独家 —— 那句话会进系统提示的常驻工具清单。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use ys_core::{ImageDetail, MediaAttachment, ToolResult, sniff_image};
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec};

use crate::workspace_path::{absolute_notice, attach_notice};

/// 嗅探时最多读的文件头字节数（与 `ocr::DIM_HEAD_BYTES` 同口径）。
///
/// 六种格式的尺寸字段都在前 64 字节内；256 KiB 是宽裕的上界，同时保证**任何**大小的
/// 图片都只读一次有界的前缀 —— 40 MiB 的文件与 4 KiB 的文件代价相同。
const HEAD_BYTES: usize = 256 * 1024;

/// 单图体积硬上限，与 `model-openai-compatible::media` 的 `DEFAULT_SINGLE_MAX_BYTES` 同值。
///
/// 判据是 **base64 编码后**的线上长度（不是磁盘上的原图字节数）—— 编码膨胀 4/3，
/// 因此工具放行的图一定不会被出站时的体积闸门降级成占位。
///
/// 工具层先拒一次是为了**早点**把事实告诉模型（它可改走 `ocr` 或换图），而不是等出站时
/// 才发现超限。适配器仍会独立复核 —— 那边是不受工具约束的最终闸门。
const MAX_BYTES: u64 = 32 * 1024 * 1024;

/// `data:` URL 的线上长度（base64 膨胀 4/3 + 前缀）。
///
/// 这是 `model-openai-compatible::media::resolve_image` 判 32 MiB 用的**同一口径**：两边
/// 的算术必须逐字相同（含 `data:` / mime / `;base64,` 三段前缀），否则会出现「工具报成功、
/// 出站却降级成占位」的自相矛盾。
fn wire_len(raw: u64, mime: &str) -> u64 {
    raw.div_ceil(3) * 4 + "data:".len() as u64 + mime.len() as u64 + ";base64,".len() as u64
}

/// `detail` schema 接受的**全部**取值（D14：工具层收窄，协议层仍是四档）。
///
/// `low` 与 `auto` 不进 schema：对「我要看清这张图」而言 `low` 没有意义，缺省 `high` 已
/// 覆盖「没想法」的情形。传 `low` ⇒ [`ToolError::InvalidInput`]（模型可据此自我纠正）。
const DETAIL_VALUES: [&str; 2] = ["high", "original"];

pub struct ViewImageTool {
    workspace: PathBuf,
}

impl ViewImageTool {
    /// 工作区根：相对路径以它为解析根，与 `read` / `ocr` 同口径。
    pub fn with_workspace(mut self, workspace: PathBuf) -> Self {
        self.workspace = workspace;
        self
    }

    fn new_with(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

/// 缺省工作区 = 进程 cwd（与 `read` 的 `Default` 同规：装配端总会显式传入真实根）。
impl Default for ViewImageTool {
    fn default() -> Self {
        Self::new_with(PathBuf::from("."))
    }
}

#[async_trait::async_trait]
impl Tool for ViewImageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "view_image",
            // 逐字（C2 原则：陈述自己的场景，不声明独家；第二句消解「拿 URL 来试」的歧义）。
            "View a local image file from the filesystem when visual inspection is needed. \
             Use this for images already available on disk.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to a local image file. Relative paths resolve \
                                        against the workspace root (URLs are not accepted)."
                    },
                    "detail": {
                        "type": "string",
                        "enum": DETAIL_VALUES,
                        "description": "How much visual detail the model should receive \
                                        (default `high`)."
                    }
                },
                // 只有 `path` 必填：`detail` 有设计缺省值，列进 `required` 会逼模型每次
                // 重复默认值（工具表是常驻付费的）。
                "required": ["path"]
            }),
        )
    }

    /// 只读：读文件头 + `metadata`，不写任何位置 ⇒ 与 `read` / `ls` 同组。
    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let raw = input["path"]
            .as_str()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                ToolError::InvalidInput(
                    "view_image: missing 'path' — pass a local image path, e.g. \
                     view_image(path=\"shot.png\")"
                        .to_owned(),
                )
            })?;

        let detail = parse_detail(input.get("detail"))?;
        let path = resolve(&self.workspace, raw);
        let path_notice = absolute_notice("view_image", raw, &self.workspace);

        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(failure(format!(
                    "view_image: image not found: {}",
                    path.display()
                )));
            }
            Err(e) => {
                return Ok(failure(format!(
                    "view_image: cannot stat {}: {e}",
                    path.display()
                )));
            }
        };
        // `metadata` 跟随符号链接 ⇒ 指向目录的链接也归入「不是常规文件」，与用户直觉一致。
        if !meta.is_file() {
            return Ok(failure(format!(
                "view_image: not a regular file: {} (directories, FIFOs and devices cannot be \
                 viewed as images — pass the image file inside it)",
                path.display()
            )));
        }
        let len = meta.len();

        // 只读有界前缀：上界取 `min(HEAD_BYTES, len)`，让「20 字节的文件」不去读 256 KiB。
        // 体积闸门要等嗅探出 mime 才能算（见下），因此这里先读头 —— 代价恒定（≤ [`HEAD_BYTES`]）。
        let head = match read_head(&path, len as usize) {
            Ok(head) => head,
            Err(e) => {
                return Ok(failure(format!(
                    "view_image: cannot read {}: {e}",
                    path.display()
                )));
            }
        };
        let Some((format, width, height)) = sniff_image(&head) else {
            return Ok(failure(format!(
                "view_image: not a supported image: {} (supported: png, jpeg, gif, webp, bmp, \
                 tiff)",
                path.display()
            )));
        };

        // 体积闸门**按编码后**判（与适配器同口径）：原图在上限内、编码后超限的图必须在这里
        // 就被拒，否则模型会先看到「成功」、再在出站时看到「超限」两个矛盾的数字。
        // 前缀长度依赖 `mime`，故这一步只能排在嗅探之后。
        let wire = wire_len(len, format.mime());
        if wire > MAX_BYTES {
            // 精确字节数一并给出：边界附近两个体量会舍入成同一个 `mib` 读数
            //（24 MiB 原图 ⇒ 33,554,454 B vs 上限 33,554,432 B，都是「32.0 MiB」）。
            return Ok(failure(format!(
                "view_image: image too large: {} ({} once base64-encoded ({} B) > {} ({} B); the \
                 file on disk is {})",
                path.display(),
                mib(wire),
                wire,
                mib(MAX_BYTES),
                MAX_BYTES,
                mib(len)
            )));
        }

        // 文案里报**原图**体积：人读的是磁盘上那个文件，报编码后的数会让它对不上 `ls -l`。
        // 判据用编码后、读数用原图 —— 两者不同正是要判编码后的原因。
        // `format` 同时给文案的 MIME 与上面的前缀长度，一处取值、不会算出两个结果。
        // `is_vision_supported` 为假的格式（bmp/tiff）仍照常交出附件 —— 是否发得出去是
        // 适配器的能力判决，工具层不替它预判（那会变成两处判断）。
        let content = format!(
            "image: {} ({}, {}×{}, {}) — attached as pixels, not text",
            path.display(),
            format.mime(),
            width,
            height,
            mib(len)
        );
        Ok(attach_notice(
            ToolResult {
                content,
                is_error: false,
                media: vec![MediaAttachment::Image {
                    // 原样保留调用方给的路径串：适配器与 TUI 都按它回读/显示，改写会让
                    // 用户看到的路径与它输入的不一致。
                    path: raw.to_owned(),
                    detail: Some(detail),
                }],
            },
            path_notice.as_deref(),
        ))
    }
}

/// 失败结果：`is_error: true` 且 **`media` 恒空**（跨层硬不变量）。
fn failure(reason: String) -> ToolResult {
    ToolResult::text(reason, true)
}

/// `detail` 只接受 [`DETAIL_VALUES`]，缺省 [`ImageDetail::High`]。
///
/// 显式传 `low` / `auto` 一律 `InvalidInput`：让模型看见「这个工具不吃这一档」，
/// 而不是静默当成 `high`（静默降级会让它以为自己要求的档位被满足了）。
fn parse_detail(value: Option<&Value>) -> Result<ImageDetail, ToolError> {
    match value {
        None | Some(Value::Null) => Ok(ImageDetail::High),
        Some(Value::String(s)) if s == "high" => Ok(ImageDetail::High),
        Some(Value::String(s)) if s == "original" => Ok(ImageDetail::Original),
        Some(other) => Err(ToolError::InvalidInput(format!(
            "view_image: 'detail' must be one of {DETAIL_VALUES:?} (default \"high\"), got {other}"
        ))),
    }
}

/// 相对路径以 workspace 根为根；绝对路径原样使用。
fn resolve(workspace: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

/// 读文件头的前 [`HEAD_BYTES`] 字节（不超过 `len`）。
fn read_head(path: &Path, len: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut head = vec![0u8; len.min(HEAD_BYTES)];
    let mut filled = 0;
    // `read` 可能短读（管道/特殊文件），故循环到填满或 EOF。
    while filled < head.len() {
        match file.read(&mut head[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    head.truncate(filled);
    Ok(head)
}

/// 人类可读的体量（`1.4 MiB` / `512 B`），与 C4 的样例句式一致。
fn mib(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < MIB as u64 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ctx, test_dir};

    fn tool(dir: &Path) -> ViewImageTool {
        ViewImageTool::new_with(dir.to_path_buf())
    }

    /// 造一个「合法 PNG 头 + 垃圾尾部」的文件：头给出尺寸，尾部让总体积可控。
    ///
    /// 与 `model-openai-compatible::media` 的夹具同构（IHDR 是 PNG 的第一个 chunk，
    /// 宽高是两个大端 u32）。
    fn write_png(dir: &Path, name: &str, width: u32, height: u32, total: usize) -> PathBuf {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        assert!(total > bytes.len(), "total 必须大于头长");
        bytes.resize(total, 0x5A);
        let path = dir.join(name);
        std::fs::write(&path, &bytes).unwrap();
        path
    }

    /// **C4 三条断言（同一用例）**：`content` 不含 `base64` / `data:`、体积有界、`media` 恰 1 条。
    ///
    /// 夹具是 **22 MiB** 的「合法 PNG 头 + 垃圾尾部」—— 即本工具愿意接收的**最大一档**：
    /// 判据是**编码后**的长度（上限 [`MAX_BYTES`] = 32 MiB），22 MiB 编码后 ≈ 29.3 MiB 仍在
    /// 上限内，再大（如 24 MiB ⇒ 32 MiB + 22 B）就会落到失败分支、根本拿不到 `content`。
    /// 原设计写的是 40 MiB，那与上限直接冲突；本条要证实的是**「体量再大，`content` 也不
    /// 跟着涨」**，22 MiB 与 40 MiB 在这个方向上等价。
    ///
    /// 体积写大是**故意**的：它与断言 2 构成一对 —— 若哪天有人把实现改成「读全文件再
    /// base64」，`content` 会涨到 ~29 MB，两条断言同时爆。用小文件测不出这个方向
    /// （小图即使编码了也只有几 KB）。
    ///
    /// 变异：`content` 改成 `format!("data:image/png;base64,{}", …)` ⇒ 断言 1、2 同时变红；
    /// `media` 写成 `vec![]` ⇒ 断言 3 变红。
    #[test]
    fn content_is_bounded_metadata_and_media_holds_the_reference() {
        let started = std::time::Instant::now();
        let dir = test_dir("view_image_big");
        std::fs::create_dir_all(&dir).unwrap();
        const BIG: usize = 22 * 1024 * 1024;
        let path = write_png(&dir, "big.png", 1920, 1080, BIG);
        assert!(path.is_file(), "夹具应落盘");

        let result = run(tool(&dir), json!({ "path": "big.png" }));

        // ① 绝不是 data URL。
        assert!(
            !result.content.contains("base64"),
            "content 不得含 base64：{}",
            result.content
        );
        assert!(
            !result.content.contains("data:"),
            "content 不得含 data:：{}",
            result.content
        );
        // 判据是编码后：22 MiB 编码后 ≈ 29.3 MiB 仍在上限内。
        let wire = wire_len(BIG as u64, "image/png");
        assert!(
            wire < MAX_BYTES,
            "前提：夹具编码后必须仍在上限内（{wire} < {MAX_BYTES}）"
        );
        // ② 有界：路径 + 百字节量级。40 MiB 的文件也不得把界突破。
        assert!(
            result.content.len() < 500,
            "content 必须是一行有界元信息（实测 {} 字节）：{}",
            result.content.len(),
            result.content
        );
        assert!(
            !result.content.contains('\n'),
            "content 必须只有一行：{}",
            result.content
        );
        // ③ 媒体恰一条，且指向调用方给的路径。
        assert!(!result.is_error, "合法 PNG 不该失败：{}", result.content);
        assert_eq!(result.media.len(), 1, "media 恰有 1 条");
        assert_eq!(
            result.media[0],
            MediaAttachment::Image {
                path: "big.png".to_owned(),
                detail: Some(ImageDetail::High),
            },
            "media 只带路径与 detail，不带字节"
        );

        println!(
            "C4 夹具（22 MiB 原图 / 编码后 {}）耗时 {:?}；content = {:?}（{} 字节）",
            mib(wire),
            started.elapsed(),
            result.content,
            result.content.len()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `detail` schema 的枚举值**恰为** `["high","original"]`，且 `required` 只有 `path`。
    ///
    /// 变异：把 `DETAIL_VALUES` 换成三档（含 `low`）⇒ 本用例变红。
    #[test]
    fn spec_detail_enum_is_exactly_high_and_original() {
        let spec = ViewImageTool::default().spec();
        assert_eq!(spec.name, "view_image");
        assert_eq!(
            spec.description,
            "View a local image file from the filesystem when visual inspection is needed. \
             Use this for images already available on disk.",
            "描述逐字（陈述自己的场景，不声明独家）"
        );

        let detail = &spec.parameters["properties"]["detail"];
        assert_eq!(
            detail["enum"],
            json!(["high", "original"]),
            "工具层收窄到两档（low/auto 不进 schema）"
        );
        assert_eq!(spec.parameters["required"], json!(["path"]));
        println!(
            "view_image 的 parameters schema：{}",
            serde_json::to_string_pretty(&spec.parameters).unwrap()
        );
        assert!(
            spec.parameters["properties"]["path"]["description"]
                .as_str()
                .unwrap()
                .contains("workspace root"),
            "path 描述须说明相对 workspace 根解析"
        );
    }

    /// 缺省 `high`；`original` 原样透传。
    #[test]
    fn detail_defaults_to_high_and_accepts_original() {
        let dir = test_dir("view_image_detail");
        std::fs::create_dir_all(&dir).unwrap();
        let _ = write_png(&dir, "a.png", 8, 6, 64);

        let default = run(tool(&dir), json!({ "path": "a.png" }));
        assert_eq!(
            default.media[0],
            MediaAttachment::Image {
                path: "a.png".into(),
                detail: Some(ImageDetail::High),
            }
        );

        let original = run(tool(&dir), json!({ "path": "a.png", "detail": "original" }));
        assert_eq!(
            original.media[0],
            MediaAttachment::Image {
                path: "a.png".into(),
                detail: Some(ImageDetail::Original),
            }
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 传 `low` / `auto` / 未知值 ⇒ `InvalidInput`（不是静默当成 `high`）。
    ///
    /// 变异：`parse_detail` 里加一条 `_ => Ok(ImageDetail::Low)` 兜底 ⇒ 本用例变红。
    #[test]
    fn low_and_auto_and_unknown_are_invalid_input() {
        for bad in ["low", "auto", "HIGH", ""] {
            let err = call_err(
                tool(Path::new(".")),
                json!({ "path": "a.png", "detail": bad }),
            );
            println!("detail={bad:?} ⇒ {err:?}");
            assert!(
                matches!(err, ToolError::InvalidInput(_)),
                "detail={bad:?} 应判 InvalidInput，实得 {err:?}"
            );
        }
    }

    /// 相对路径以 **workspace 根**为根（不是进程 cwd）。
    ///
    /// 变异：`resolve` 里恒用 `raw` 而不 join workspace ⇒ 本用例变红。
    #[test]
    fn relative_path_resolves_against_workspace_root() {
        let dir = test_dir("view_image_rel");
        std::fs::create_dir_all(dir.join("shots")).unwrap();
        let _ = write_png(&dir.join("shots"), "a.png", 640, 480, 128);

        let result = run(tool(&dir), json!({ "path": "shots/a.png" }));
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("shots/a.png"),
            "元信息里的路径是 workspace 相对的原始串：{}",
            result.content
        );
        assert!(
            result.content.contains("640×480"),
            "尺寸来自文件头：{}",
            result.content
        );
        assert!(result.content.contains("image/png"), "{}", result.content);

        // 从别处起工具就不该找得到它（证明解析根确实是 workspace）。
        let other = test_dir("view_image_rel_other");
        std::fs::create_dir_all(&other).unwrap();
        let missing = run(tool(&other), json!({ "path": "shots/a.png" }));
        assert!(
            missing.is_error,
            "workspace 外不应命中：{}",
            missing.content
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    /// 失败路径统一形态：`is_error: true` **且 `media` 恒空**（跨层硬不变量）。
    ///
    /// 覆盖四条：不存在 / 不是常规文件 / 非图片 / 编码后超 32 MiB。
    ///
    /// 变异：任一失败分支返回 `media: vec![…]` ⇒ 本用例变红；某分支写成 `is_error: false`
    /// ⇒ 对应断言变红。
    #[test]
    fn failures_report_facts_with_empty_media() {
        let dir = test_dir("view_image_fail");
        std::fs::create_dir_all(dir.join("adir")).unwrap();
        std::fs::write(dir.join("notes.txt"), b"just text, no magic bytes").unwrap();
        // 头合法但**编码后**超 32 MiB 上限：24 MiB 原图编码后恰是 32 MiB 载荷 + 22 B 前缀。
        // 造 24 MiB 而不是 32 MiB+：判据在编码后，超过 24 MiB 就足够触线。
        let _ = write_png(&dir, "huge.png", 100, 100, 24 * 1024 * 1024);

        let cases = [
            ("missing.png", "not found"),
            ("adir", "not a regular file"),
            ("notes.txt", "not a supported image"),
            ("huge.png", "image too large"),
        ];
        for (path, expected) in cases {
            let result = run(tool(&dir), json!({ "path": path }));
            println!(
                "失败路径 {path}: is_error=true, media={:?}, content={:?}",
                result.media, result.content
            );
            assert!(result.is_error, "{path} 应判失败：{}", result.content);
            assert!(
                result.media.is_empty(),
                "{path} 失败时 media 必须为空（is_error ⇒ 无附件），实得 {:?}",
                result.media
            );
            assert!(
                result.content.contains(expected),
                "{path} 的原因应含 {expected:?}：{}",
                result.content
            );
        }

        // 缺 `path` 是**参数**错误（不是结果错误）：走 `Err` 而不是 `is_error`。
        let err = call_err(tool(&dir), json!({}));
        assert!(matches!(err, ToolError::InvalidInput(_)), "实得 {err:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **反向用例（体积判据的口径）**：原图在上限内、**编码后**超限 ⇒ 必须拒绝。
    ///
    /// 24 MiB 原图（25,165,824 B）base64 后恰是 32 MiB 载荷，加 22 B 前缀
    /// ⇒ 33,554,454 B > 32 MiB（33,554,432）⇒ 拒。若按原图判，24 MiB < 32 MiB 会放它过去，
    /// 然后出站时被适配器的同一闸门降级成占位 —— 模型先看到「成功」再看到「超限」。
    ///
    /// 变异：把判据改回 `raw > MAX_BYTES` ⇒ 本用例变红（会放过它）。
    #[test]
    fn rejects_when_encoded_size_exceeds_limit_though_raw_does_not() {
        let dir = test_dir("view_image_wire");
        std::fs::create_dir_all(&dir).unwrap();
        const RAW: usize = 24 * 1024 * 1024;
        let path = write_png(&dir, "fits-on-disk.png", 1920, 1080, RAW);
        let raw = std::fs::metadata(&path).unwrap().len();
        let wire = wire_len(raw, "image/png");

        // 前提：按原图判会放过它，按编码后判必须拒。
        assert!(
            raw < MAX_BYTES,
            "前提：原图字节数在上限之内（{raw} < {MAX_BYTES}），按原图判就会放过它"
        );
        assert_eq!(wire, 33_554_454, "24 MiB 编码后 = 32 MiB 载荷 + 22 B 前缀");
        assert!(wire > MAX_BYTES, "编码后必须超限：{wire} > {MAX_BYTES}");

        let result = run(tool(&dir), json!({ "path": "fits-on-disk.png" }));

        assert!(
            result.is_error,
            "编码后超限必须拒绝，实得：{}",
            result.content
        );
        assert!(
            result.media.is_empty(),
            "失败时 media 恒空，实得 {:?}",
            result.media
        );
        // 文案报的是**编码后**的数（32.0 MiB），不是磁盘上的原图数（24.0 MiB）。
        assert!(
            result
                .content
                .contains(&format!("{} once base64-encoded", mib(wire))),
            "体积必须按编码后报：{}",
            result.content
        );
        // 双侧都显示「32.0 MiB」，只有精确字节数能区开 ⇒ 文案必须带上它，否则模型会拼命重试。
        assert!(
            result.content.contains(&format!("{wire} B"))
                && result.content.contains(&format!("{MAX_BYTES} B")),
            "边界附近两个体量舍入后同值，必须附精确字节数：{}",
            result.content
        );
        assert!(
            !result
                .content
                .contains(&format!("{} once base64-encoded", mib(raw))),
            "不得把原图大小当成编码后的数：{}",
            result.content
        );
        println!(
            "编码后超限：原图 {raw} B（{}）/ 编码后 {wire} B（{}）⇒ {}",
            mib(raw),
            mib(wire),
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只读工具 ⇒ `is_concurrency_safe()` 为真（与 `read` / `ls` 同组）。
    #[test]
    fn is_concurrency_safe_because_read_only() {
        assert!(ViewImageTool::default().is_concurrency_safe());
    }

    /// 后台运行时驱动一次 `call`。
    fn run(tool: ViewImageTool, input: Value) -> ToolResult {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async { tool.call(input, ctx()).await.unwrap() })
    }

    /// 期望 `Err` 的调用（参数校验失败）。
    fn call_err(tool: ViewImageTool, input: Value) -> ToolError {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async { tool.call(input, ctx()).await })
            .expect_err("应返回 InvalidInput")
    }
}
