//! `translate_request` 的图像通路：**K2 的两半**与六条降级占位。
//!
//! 为什么这些用例必须在**集成测试**里：`translate_request` 是 `pub`，而本文件断言的是
//! **线上形态**（`messages[]` 里到底有没有 `image_url`、占位串是否逐字一致）——
//! 那是模型实际看到的东西，不是内部状态的推论。
//!
//! 三条承重不变量（违反即返工）：
//!
//! 1. **消息数守恒**：入站 N 条非空消息（含**仅含 Image** 的）⇒ 出站 `messages.len() >= N`。
//!    K2 是两处：`:1311` 的显式 `Image` 臂**和** `:1316` 的推送条件；只改前者时
//!    仅含 Image 的消息仍整条不出场（`"messages":[]`）。
//! 2. **占位绝不写回** `ContentBlock`（D11）：占位只存在于**出站临时副本**，
//!    否则原图补齐后永不重试。
//! 3. **门控 fail-closed**：识别不出模型 ⇒ 不发图，走占位。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ys_core::{ContentBlock, ImageDetail, Message, Role};
use ys_model::ModelRequest;
use ys_model_openai_compat::compat::ProviderCompat;
use ys_model_openai_compat::media::{MediaCache, MediaLimits};
use ys_model_openai_compat::request::{ChatCompletionRequest, Content, ContentPart};
use ys_model_openai_compat::{OpenAICompatibleConfig, translate_request};

// ---------------------------------------------------------------- 夹具

/// 临时目录名含**进程内原子序号 + `process::id()`**（并行安全，见 `AGENTS.md`）。
fn test_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "model_openai_translate_image_{id}_{}_{seq}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 合法 PNG 头（魔数 + IHDR + 宽高），后面按需填充。
fn png_bytes(total: usize) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&8u32.to_be_bytes());
    bytes.extend_from_slice(&6u32.to_be_bytes());
    assert!(total > bytes.len(), "total 必须大于头长");
    bytes.resize(total, 0x5A);
    bytes
}

fn write_png(dir: &Path, name: &str, total: usize) -> String {
    let path = dir.join(name);
    std::fs::write(&path, png_bytes(total)).unwrap();
    path.to_str().unwrap().to_owned()
}

fn path_str(p: &Path) -> String {
    p.to_str().unwrap().to_owned()
}

/// 一个「能读图」的 provider：能力位开 + 模型 id 可识别。
fn vision_config(model: &str) -> OpenAICompatibleConfig {
    OpenAICompatibleConfig {
        api_base: "http://localhost".into(),
        api_key: "test".into(),
        model: model.into(),
        max_tokens: None,
        temperature: None,
        reasoning_effort: None,
        disable_thinking: false,
        compat: ProviderCompat::deepseek(),
    }
}

fn image(path: &str, detail: Option<ImageDetail>) -> ContentBlock {
    ContentBlock::Image {
        path: path.into(),
        detail,
    }
}

fn text(s: &str) -> ContentBlock {
    ContentBlock::Text { text: s.into() }
}

fn request(messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        messages: Arc::new(messages),
        tools: Arc::new(vec![]),
        system: None,
        max_tokens: None,
        temperature: None,
        boundary: None,
        thinking: None,
    }
}

fn user_msg(content: Vec<ContentBlock>) -> Message {
    Message {
        role: Role::User,
        content,
    }
}

/// 把一个 `Content` 摊平成「文本片段 + 图像 URL」两列，便于逐条断言。
fn split(content: &Option<Content>) -> (Vec<String>, Vec<String>) {
    match content {
        None => (vec![], vec![]),
        Some(Content::Text(t)) => (vec![t.clone()], vec![]),
        Some(Content::Parts(parts)) => {
            let mut texts = Vec::new();
            let mut urls = Vec::new();
            for part in parts {
                match part {
                    ContentPart::Text { text } => texts.push(text.clone()),
                    ContentPart::ImageUrl { image_url } => urls.push(image_url.url.clone()),
                }
            }
            (texts, urls)
        }
    }
}

/// 请求体里出现的全部文本片段（跨消息）。
fn all_texts(req: &ChatCompletionRequest) -> Vec<String> {
    req.messages
        .iter()
        .flat_map(|m| split(&m.content).0)
        .collect()
}

/// 请求体里出现的全部图像 URL（跨消息）。
fn all_image_urls(req: &ChatCompletionRequest) -> Vec<String> {
    req.messages
        .iter()
        .flat_map(|m| split(&m.content).1)
        .collect()
}

/// 序列化后的请求体（断言「没有 image_url」时看**线上形态**，不是结构体）。
fn wire(req: &ChatCompletionRequest) -> String {
    serde_json::to_string(req).unwrap()
}

// ---------------------------------------------------------------- 消息数守恒（P0）

/// **K2 的另一半**：仅含 `Image` 块的消息必须出站。
///
/// 这是本单元的核心回归点。只改 `:1311`（`_ => {}` → 显式 `Image` 臂）而**不改**
/// `:1316` 的推送条件（`text_content` / `tool_calls` 都为空）时，这条消息整条不出场
/// ⇒ 序列化成 `"messages":[]`，模型收到空上下文却没有任何报错。
///
/// 变异：删掉 `|| !image_parts.is_empty()` ⇒ 本用例变红（`messages` 为空）。
#[test]
fn image_only_message_is_not_dropped() {
    let dir = test_dir();
    let p = write_png(&dir, "only.png", 400);

    let req = request(vec![user_msg(vec![image(&p, None)])]);
    let out = translate_request(&req, &vision_config("deepseek-flash"), &MediaCache::new())
        .expect("翻译不应失败");

    assert_eq!(
        out.messages.len(),
        1,
        "仅含 Image 的消息不能整条消失：{}",
        wire(&out)
    );
    assert_eq!(out.messages[0].role, "user");
    assert_eq!(all_image_urls(&out).len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 消息数守恒：入站 N 条非空消息（文本 / 工具 / 图片混合）⇒ 出站 ≥ N。
///
/// 上限不给死：`ToolResult` 会额外展开成一条 `role:"tool"` 消息，故只断言下界。
#[test]
fn message_count_is_conserved() {
    let dir = test_dir();
    let a = write_png(&dir, "a.png", 400);
    let b = write_png(&dir, "b.png", 400);

    let req = request(vec![
        user_msg(vec![text("look at this")]),
        user_msg(vec![image(&a, None)]),
        user_msg(vec![text("and these two"), image(&b, None)]),
    ]);
    let out = translate_request(&req, &vision_config("deepseek-flash"), &MediaCache::new())
        .expect("翻译不应失败");

    assert!(
        out.messages.len() >= 3,
        "入站 3 条非空消息，出站只有 {} 条：{}",
        out.messages.len(),
        wire(&out)
    );
    assert_eq!(all_image_urls(&out).len(), 2, "两幅图都要到场");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 纯文本请求的**线上字节不变**：`content` 仍是字符串，不是块数组。
///
/// 这是 U5 的 golden 在本文件里的守卫 —— 加图像通路不得改变既有请求形态。
#[test]
fn text_only_request_keeps_string_content() {
    let req = request(vec![user_msg(vec![text("hello")])]);
    let out = translate_request(&req, &vision_config("deepseek-flash"), &MediaCache::new())
        .expect("翻译不应失败");
    assert!(wire(&out).contains(r#""content":"hello""#));
    assert!(!wire(&out).contains("image_url"));
}

// ---------------------------------------------------------------- 不支持模型（门控）

/// 不支持图像的模型：**没有** `image_url`，但**有**占位文本。
#[test]
fn unsupported_model_gets_placeholder_and_no_image() {
    let dir = test_dir();
    let p = write_png(&dir, "x.png", 400);

    // `deepseek-chat` 在 `deepseek` 方言下能力位为真，但 id 识别不出视觉能力。
    let req = request(vec![user_msg(vec![text("see"), image(&p, None)])]);
    let out = translate_request(&req, &vision_config("deepseek-chat"), &MediaCache::new())
        .expect("翻译不应失败");

    assert!(all_image_urls(&out).is_empty(), "不得出现 image_url");
    assert!(!wire(&out).contains("image_url"), "线上不得有该字段");
    let texts = all_texts(&out);
    assert!(
        texts
            .iter()
            .any(|t| t == "image content omitted because you do not support image input"),
        "缺占位：{texts:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// **同一批内只发一次**：三张图 ⇒ 占位只出现一次（逐图重复路径/句子只是烧 token）。
///
/// 变异：把去重标志删掉 ⇒ 占位出现三次，本用例变红。
#[test]
fn unsupported_placeholder_appears_once_per_batch() {
    let dir = test_dir();
    let paths: Vec<String> = (0..3)
        .map(|i| write_png(&dir, &format!("m{i}.png"), 400))
        .collect();

    let content = paths.iter().map(|p| image(p, None)).collect();
    let req = request(vec![user_msg(content)]);
    let out = translate_request(&req, &vision_config("glm-5.3"), &MediaCache::new())
        .expect("翻译不应失败");

    let hits = all_texts(&out)
        .iter()
        .filter(|t| *t == "image content omitted because you do not support image input")
        .count();
    assert_eq!(hits, 1, "占位应只出现一次：{:?}", all_texts(&out));
    assert!(all_image_urls(&out).is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// provider 能力位关闭 ⇒ 即使模型 id 看着能读图也不发（第一层门控生效）。
#[test]
fn provider_bit_off_disables_images_even_for_vision_model() {
    let dir = test_dir();
    let p = write_png(&dir, "y.png", 400);

    let mut config = vision_config("deepseek-flash");
    config.compat = ProviderCompat::standard();
    let req = request(vec![user_msg(vec![image(&p, None)])]);
    let out = translate_request(&req, &config, &MediaCache::new()).expect("翻译不应失败");

    assert!(all_image_urls(&out).is_empty());
    assert!(
        all_texts(&out)
            .iter()
            .any(|t| t == "image content omitted because you do not support image input")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- 六条占位逐字

/// 读不到：``yushan could not read the local image at `{path}`: {error}``。
#[test]
fn placeholder_read_failed_is_verbatim() {
    let missing = "/nonexistent/yushan-absent-image.png";
    let req = request(vec![user_msg(vec![image(missing, None)])]);
    let out = translate_request(&req, &vision_config("deepseek-flash"), &MediaCache::new())
        .expect("翻译不应失败");

    let texts = all_texts(&out);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(
        texts[0].starts_with(
            "yushan could not read the local image at `/nonexistent/yushan-absent-image.png`: "
        ),
        "逐字不符：{:?}",
        texts[0]
    );
    // 原因必须到场（模型据此判断是路径错还是权限错）。
    assert!(
        texts[0].contains("No such file") || texts[0].contains("os error"),
        "缺底层原因：{:?}",
        texts[0]
    );
    assert!(all_image_urls(&out).is_empty());
}

/// 非法（不是图）：``Image located at `{path}` is invalid: {error}``。
#[test]
fn placeholder_invalid_image_is_verbatim() {
    let dir = test_dir();
    let path = dir.join("not-an-image.txt");
    std::fs::write(&path, b"just some text, definitely not a png").unwrap();
    let p = path_str(&path);

    let req = request(vec![user_msg(vec![image(&p, None)])]);
    let out = translate_request(&req, &vision_config("deepseek-flash"), &MediaCache::new())
        .expect("翻译不应失败");

    let texts = all_texts(&out);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(
        texts[0].starts_with(&format!("Image located at `{p}` is invalid: ")),
        "逐字不符：{:?}",
        texts[0]
    );
    assert!(all_image_urls(&out).is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 格式不支持：``yushan cannot attach image at `{path}`: unsupported image `{mime}`.``
///
/// BMP 能被嗅探出尺寸，但不进视觉请求 ⇒ 走这条而非「非法」。
#[test]
fn placeholder_unsupported_format_is_verbatim() {
    let dir = test_dir();
    let path = dir.join("old.bmp");
    // BM + 14 字节 file header + 40 字节 DIB 头（宽 4 / 高 8，小端）。
    let mut bytes = b"BM".to_vec();
    bytes.resize(18, 0);
    bytes.extend_from_slice(&40u32.to_le_bytes());
    bytes.extend_from_slice(&4i32.to_le_bytes());
    bytes.extend_from_slice(&8i32.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();
    let p = path_str(&path);

    let req = request(vec![user_msg(vec![image(&p, None)])]);
    let out = translate_request(&req, &vision_config("deepseek-flash"), &MediaCache::new())
        .expect("翻译不应失败");

    let texts = all_texts(&out);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert_eq!(
        texts[0],
        format!("yushan cannot attach image at `{p}`: unsupported image `image/bmp`.")
    );
    assert!(all_image_urls(&out).is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 单图超限（**编码后**）：``yushan cannot attach image at `{path}`: too large ({size} > 32 MiB).``
///
/// 用小上限注入（512 B），不真造 32 MiB（C8.6：逻辑与尺寸无关）。
#[test]
fn placeholder_single_too_large_is_verbatim() {
    let dir = test_dir();
    let p = write_png(&dir, "big.png", 4096);

    let cache = MediaCache::with_limits(MediaLimits {
        single_max_bytes: 512,
        ..MediaLimits::default()
    });
    let req = request(vec![user_msg(vec![image(&p, None)])]);
    let out =
        translate_request(&req, &vision_config("deepseek-flash"), &cache).expect("翻译不应失败");

    let texts = all_texts(&out);
    assert_eq!(texts.len(), 1, "{texts:?}");
    // 逐字形态：前缀 + `{size} > {limit}`；数值随注入上限变化，故只钉形态与单位。
    let expected_prefix = format!("yushan cannot attach image at `{p}`: too large (");
    assert!(
        texts[0].starts_with(&expected_prefix),
        "逐字不符：{:?}",
        texts[0]
    );
    assert!(
        texts[0].ends_with(" > 512 B)."),
        "上限口径不符：{:?}",
        texts[0]
    );
    assert!(all_image_urls(&out).is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 累计超预算：``[image too large: {path} (would exceed the 48 MiB request budget)]``
///
/// C8.6 的注入版：预算改到 4 KiB，两张各 1536 B 原图的 PNG ⇒ **第二张**降级。
/// 第一张仍须正常出图（预算计数器不给降级图记账）。
///
/// 算术必须摆明（否则夹具前提一错就看起来像生产 bug）：
/// 原图 1536 B ⇒ base64 `ceil(1536/3)*4` = 2048 B，加 `data:image/png;base64,` 22 B
/// ⇒ **2070 B**/张。单张 2070 ≤ 4096 通过；两张 4140 > 4096 ⇒ 第二张降级。
#[test]
fn placeholder_request_budget_is_verbatim_and_second_image_degrades() {
    let dir = test_dir();
    let a = write_png(&dir, "first.png", 1536);
    let b = write_png(&dir, "second.png", 1536);

    let cache = MediaCache::with_limits(MediaLimits {
        request_budget_bytes: 4096,
        ..MediaLimits::default()
    });
    let req = request(vec![user_msg(vec![image(&a, None), image(&b, None)])]);
    let out =
        translate_request(&req, &vision_config("deepseek-flash"), &cache).expect("翻译不应失败");

    let urls = all_image_urls(&out);
    assert_eq!(urls.len(), 1, "第一张必须出图、第二张降级：{}", wire(&out));
    assert_eq!(
        urls[0].len(),
        2070,
        "夹具算术变了：单张应为 2070 B，实际 {}",
        urls[0].len()
    );
    let texts = all_texts(&out);
    assert_eq!(
        texts,
        vec![format!(
            "[image too large: {b} (would exceed the 4 KiB request budget)]"
        )],
        "逐字不符"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 图数超限：``[image too large: {path} (over the 600 images per request limit)]``
///
/// 上限注入成 2，第三张降级（不真造 600 张）。
#[test]
fn placeholder_image_count_is_verbatim() {
    let dir = test_dir();
    let paths: Vec<String> = (0..3)
        .map(|i| write_png(&dir, &format!("c{i}.png"), 400))
        .collect();

    let cache = MediaCache::with_limits(MediaLimits {
        max_images_per_request: 2,
        ..MediaLimits::default()
    });
    let req = request(vec![user_msg(
        paths.iter().map(|p| image(p, None)).collect(),
    )]);
    let out =
        translate_request(&req, &vision_config("deepseek-flash"), &cache).expect("翻译不应失败");

    assert_eq!(all_image_urls(&out).len(), 2, "{}", wire(&out));
    assert_eq!(
        all_texts(&out),
        vec![format!(
            "[image too large: {} (over the 2 images per request limit)]",
            paths[2]
        )],
        "逐字不符"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- 占位不写回（D11）

/// **占位只存在于出站临时副本**：调用后原 `ModelRequest` 里的 `Image` 块仍在、未被改写。
///
/// 若把占位写回 `ContentBlock`，降级就会被固化进会话 —— 原图补齐后再也不会重试送图。
///
/// 变异：把 `image_parts.push(...)` 改成 `text_content.push_str(...)` 之外的写回路径
/// （如 `msg.content` 的 `into_iter` 可变迭代）⇒ 本条与「原图补齐后重试」都会变红。
#[test]
fn placeholder_is_not_written_back_to_the_request() {
    let dir = test_dir();
    let good = write_png(&dir, "good.png", 400);

    let original = vec![
        user_msg(vec![image(&good, Some(ImageDetail::High))]),
        user_msg(vec![image("/nonexistent/missing.png", None)]),
    ];
    let req = request(original.clone());

    let cache = MediaCache::with_limits(MediaLimits {
        single_max_bytes: 512,
        ..MediaLimits::default()
    });
    let _out =
        translate_request(&req, &vision_config("deepseek-flash"), &cache).expect("翻译不应失败");

    // 入参未被改动：仍是两条、每条的块集合与内容逐字相同。
    assert_eq!(req.messages.len(), 2, "入站消息数被改写");
    for (i, msg) in req.messages.iter().enumerate() {
        assert_eq!(msg.content.len(), 1, "第 {i} 条的块数被改写");
        assert!(
            matches!(msg.content[0], ContentBlock::Image { .. }),
            "第 {i} 条不再是 Image 块：{:?}",
            msg.content
        );
    }
    assert_eq!(*req.messages, original, "入站消息内容被改写");

    // 具体言之：读不到的那条路径仍是原路径，不是占位串。
    let ContentBlock::Image { path, .. } = &req.messages[1].content[0] else {
        panic!("不是 Image 块");
    };
    assert_eq!(path, "/nonexistent/missing.png");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- 细节档

/// `detail` 经门控后发出：`High` 原样，`None` 不发该字段。
#[test]
fn detail_is_sent_only_when_requested_and_supported() {
    let dir = test_dir();
    let p = write_png(&dir, "d.png", 400);

    let req = request(vec![user_msg(vec![image(&p, Some(ImageDetail::High))])]);
    let out = translate_request(&req, &vision_config("deepseek-flash"), &MediaCache::new())
        .expect("翻译不应失败");
    let json = wire(&out);
    assert!(json.contains(r#""detail":"high""#), "{json}");

    let req = request(vec![user_msg(vec![image(&p, None)])]);
    let out = translate_request(&req, &vision_config("deepseek-flash"), &MediaCache::new())
        .expect("翻译不应失败");
    assert!(!wire(&out).contains("detail"), "{}", wire(&out));

    let _ = std::fs::remove_dir_all(&dir);
}
