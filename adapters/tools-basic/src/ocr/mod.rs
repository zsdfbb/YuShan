//! `ocr` 工具：把图片里的文字**连同位置信息**取成文本（设计 `docs/arch/ocr-tool/design.md`）。
//!
//! 本模块是**编排层**：spec、参数解析与校验、notice 拼装、可观测性字段。它**不含后端细节**
//! —— 探测 / argv / 落盘在 [`engine`]，解析 / 过滤 / 渲染在 [`tsv`]。
//!
//! # 流水线
//!
//! [`Tool::call`] 的顺序固定（design §关键数据流 ①②③④⑤）：参数校验 → 引擎探测 →
//! 逐候选 spawn（**成功即停**，全失败则拼接每个候选的原因）→ 解析 / 过滤 / 渲染 →
//! 内联还是落盘。参数面（[`Args`]、[`OcrTool::spec`]）与校验（design §3.3 / §7 的
//! **文件存在性三态**）已定型 —— 改流水线时不要动它们的既有语义。
//!
//! 头行的 `WxH` 取自 [`image_dims`]（**只读文件头，不解码像素**，故零新增依赖）：
//! `w,h` 是坐标系的一部分，用结果坐标的上界冒充原图尺寸会让「右上角」这类判断静默错位。
//!
//! # 为什么参数是「意图」而不是「引擎旋钮」
//!
//! `--psm` / `--oem` / `dpi` **不暴露**（design §2.5：`psm 7` 在多行图上静默吐空，是读图
//! 工具最坏的失败模式）。参数面只有三个字段，加第三后端时接口不变。
//!
//! # `Tool::timeout()` 刻意不实现（**承重**，design §3.1）
//!
//! `None` ⇒ 外层回落到 `RunLimits::bash_timeout`（默认 300s），内层用 `search::DEADLINE`
//! （60s）。外层预算到点是把工具 future **drop** 掉，届时只有 `kill_on_drop` 生效（只杀
//! 直接子进程，进程组不清理）；杀组逻辑只在内层超时路径上跑。内外层相等 ⇒ 外层先赢 ⇒
//! `swiftc` 编出的 `swift-frontend` 孙进程漏网成孤儿。故实现 `timeout()` 时必须从
//! `ctx.tool_budget` 推导内层（照 `bash.rs::effective_deadline`，外层 − 余量），绝不能直接
//! 返回与内层 `DEADLINE` 相等的值。

pub mod engine;
pub mod tsv;

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec};

use self::engine::{Kind, OcrEngine, Spill};
use self::tsv::{FilterStats, Granularity, Header};
use crate::exec::{run_bounded, Bounded, Stop};
use ys_tool::{head, Truncation, MAX_BYTES, MAX_LINES, SOFT_RESULT_BYTES};

/// `lang` 的默认值（design §2.4：7 图逐图 CER 实测，`chi_sim+eng` 7/7 不劣于 `eng`）。
const DEFAULT_LANG: &str = "chi_sim+eng";

/// 参数结构 —— schema（[`OcrTool::spec`]）与解析的**同一份定义**。
///
/// `lang` 与 `word_boxes` 走字段级 `default`：它们有设计规定的缺省值，且缺省必须与
/// [`Default`] 逐字一致（`Default` 是唯一真值，字段属性只是把它接到 serde 上）。
/// `path` **无默认**：`required: ["path"]` 的语义要在解析层也成立 —— 缺 `path` 时
/// 走 [`ToolError::InvalidInput`]，而不是静默取空串。
#[derive(Debug, Clone, Deserialize)]
struct Args {
    /// 图像路径。相对路径以 workspace 根为解析根（与 `read` / `ls` 同口径）。
    path: PathBuf,
    /// 语种参数。**Vision 后端下被忽略**（该后端无语言旋钮，头行打 `n/a`，见 engine §2.3）。
    #[serde(default = "default_lang")]
    lang: String,
    /// 词级坐标。`true` 时恒落盘（词级内联会被误读成「词表只有这么多行」）。
    #[serde(default)]
    word_boxes: bool,
}

/// 落盘路径的**临时性自述**（design §3.4）。
///
/// 不说这句，`--resume` 之后 `read` 该路径得到 `not found`，模型无法区分「被系统清理」与
/// 「OCR 失败」——前者该重新 OCR，后者该检查引擎。
const TEMP_FILE_WARNING: &str = "that path is a temp file — the system may clean it up at any \
     time; `read` or `cp` it now if you need it later";

/// 失败原因摘要的字符上限（stderr 可能是一整页栈）。
const FAILURE_DETAIL_CHARS: usize = 240;

/// 读图像头所需的前缀字节数（TIFF 的 IFD0 可能离文件头较远，故给足）。
const DIM_HEAD_BYTES: usize = 256 * 1024;

/// `String` 的 `Default` 是空串，不是设计缺省值 ⇒ 单独给出。
fn default_lang() -> String {
    DEFAULT_LANG.to_owned()
}

impl Default for Args {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            lang: default_lang(),
            word_boxes: false,
        }
    }
}

/// `ocr` 工具。
///
/// 字段私有：`workspace` 是相对路径的解析根，测试与将来的装配端可经
/// [`OcrTool::with_workspace`] 注入（**构造器注入**优于 env：无 `env_lock`、无并行污染）。
pub struct OcrTool {
    workspace: PathBuf,
    /// 探测结果（**值**）：生产路径是 [`OcrEngine::detect`]，测试经
    /// [`OcrTool::with_engine`] 注入假后端目录 —— 开发机不装任何引擎也能跑全部用例。
    engine: OcrEngine,
}

impl OcrTool {
    /// 生产默认：相对路径以进程 cwd 为解析根，引擎按进程 `PATH` 探测。
    pub fn new() -> Self {
        Self {
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            engine: OcrEngine::detect(),
        }
    }

    /// 换相对路径的解析根（照 `ReadTool::new(workspace)` 的既有形态）。
    pub fn with_workspace(mut self, workspace: PathBuf) -> Self {
        self.workspace = workspace;
        self
    }

    /// 换引擎探测结果（**构造器注入**：无 `env_lock`、无并行污染，见 design §3.4）。
    pub fn with_engine(mut self, engine: OcrEngine) -> Self {
        self.engine = engine;
        self
    }
}

impl Default for OcrTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for OcrTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "ocr",
            "Extract the text in an image together with its pixel positions — the only way to \
             see what an image contains here; set word_boxes for per-word coordinates",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the image (png/jpg/jpeg/webp/gif/bmp/tiff). \
                                        Relative paths resolve against the workspace root."
                    },
                    "lang": {
                        "type": "string",
                        "description": "Tesseract language pack, e.g. `eng` or `chi_sim+eng` \
                                        (default). Ignored by the macOS Vision backend, which \
                                        detects the language itself."
                    },
                    "word_boxes": {
                        "type": "boolean",
                        "description": "Return per-word coordinates instead of per-line ones \
                                        (default false). The result is then always written to a \
                                        temp file; Vision degrades to line boxes split on spaces."
                    }
                },
                // 只有 `path` 必填：另外两个有设计缺省值，列进 `required` 会逼模型
                // 每次重复默认值（工具表是常驻付费的）。
                "required": ["path"]
            }),
        )
    }

    /// 只读输入 + 幂等落盘（内容哈希命名 + 原子写）⇒ 与 `read` 同类，可与批内其它
    /// 并发安全的工具并行（design §7）。
    fn is_concurrency_safe(&self) -> bool {
        true
    }

    // **刻意不实现 `timeout()`** —— 保持默认 `None`。理由见模块头（design §3.1 承重）。

    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let args: Args = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("invalid arguments: {e}")))?;

        let image = match validate(&self.workspace, &args.path) {
            Ok(image) => image,
            Err(message) => {
                return Ok(ToolResult {
                    content: message,
                    is_error: true,
                });
            }
        };

        // ① 探测（启动期一次纯路径扫描，不 fork）。链空 ⇒ 明确说「一个后端都没有」。
        let candidates = self.engine.candidates();
        if candidates.is_empty() {
            return Ok(ToolResult {
                content: format!(
                    "ocr: no OCR backend found on PATH — cannot read {}. {}",
                    image.display(),
                    OcrEngine::install_hint()
                ),
                is_error: true,
            });
        }

        // ② 逐候选 spawn，成功即停。失败原因**逐个累积**：只报最后一个会让「Vision 挂了 +
        // tesseract 没装」退化成「tesseract 没装」，模型永远查不出前者（design §2.2）。
        let mut failures: Vec<String> = Vec::new();
        let mut attempt = None;
        for kind in candidates {
            let Some(prog) = self.engine.prog(kind) else {
                continue;
            };
            let argv = self.engine.argv(kind, &image, &args.lang);
            let bounded = run_bounded(
                &prog,
                &argv,
                &self.workspace,
                crate::search::BYTE_CAP,
                crate::search::DEADLINE,
                ctx.boundary,
            )
            .await?;

            match attempt_error(&bounded, kind, &prog) {
                Some(reason) => failures.push(reason),
                None => {
                    attempt = Some((kind, bounded));
                    break;
                }
            }
        }

        let Some((kind, bounded)) = attempt else {
            return Ok(ToolResult {
                content: format!(
                    "ocr: every OCR backend failed on {}. {}",
                    image.display(),
                    failures.join(" | ")
                ),
                is_error: true,
            });
        };

        // ③ 解析 + 过滤（纯函数）。空结果的**四**种成因靠 stdout 是否有任何输出 +
        // `FilterStats` + stderr 区分（§3.2）。
        let parsed = tsv::parse_tsv(&String::from_utf8_lossy(&bounded.stdout));
        let (lines, stats) = tsv::filter(parsed);
        let stderr = String::from_utf8_lossy(&bounded.stderr);
        // 「语言包缺失」是**惰性发现**、不进进程状态（§7）：每次调用都重新扫 stderr。
        let lang_missing = stderr.contains("Failed loading language");
        // **静默失败**：`exit 0` 但 stdout 一个字节都没有 ⇒ 引擎没跑通（例：Tesseract 的 argv
        // 漏了 outputbase，结果写进文件而 stdout 全空）。exit code 在这里**不是判据** ——
        // 「图里真没字」也是 0，两者只能靠「有没有任何输出」区分。
        //
        // 判据按后端分：Tesseract 的 TSV 模式**即使图中无字**也会吐头行 + level 1-3（页 /
        // 块 / 段），故空 stdout 不可能是「真没字」；`vision.swift` 不吐头行，空图本来就是
        // 零字节，把它算成失败会误杀正常结果 ⇒ 该档只对 Tesseract 成立。
        //
        // "没有输出" = 没有**任何一个非空白字节**：只剩一个 `\n` 同样是没输出（否则一个
        // 空行就能绕过本档，把静默失败退化成「图里没字」）。
        let silent_failure = is_silent_failure(kind, &bounded.stdout);

        // ④ 渲染。头行的 `lang=` 走 `header_lang`：Vision 下回显入参会因果倒置。
        let (width, height) = image_dims(&image);
        let name = image
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| image.display().to_string());
        let word_boxes = args.word_boxes;
        let granularity = if word_boxes {
            Granularity::Word
        } else {
            Granularity::Line
        };
        // `FilterStats::kept` 把词级与行级行**一起**计数（它只负责「丢掉哪些」），而 `lines=`
        // 必须等于本粒度实际渲染的行数 —— 否则头行的 `lines=` 与下文行数对不上，模型无法
        // 判断模型是否读全。故按粒度重数一遍。
        let rendered_rows = visible_rows(&lines, granularity);
        let header = Header {
            name: &name,
            width,
            height,
            via: kind.as_str(),
            lang: OcrEngine::header_lang(kind, &args.lang),
            lines: rendered_rows,
            filtered: stats.filtered,
            degraded: lang_missing.then_some(DEGRADED_LANG_MISSING),
        };
        let rendered = tsv::render_at(&header, &lines, granularity);

        // ⑤ 用量控制（§4.3）：`word_boxes` 恒落盘；否则 8 KiB 是**软**阈值。
        //
        // 静默失败**整份替换**渲染结果：此时 stdout 是空的，剩下的只有一行头行 + 一条
        // notice —— 那个头行会被读成「引擎跑通了，只是图里没字」（`lines=0 filtered=0`）。
        let content = if silent_failure {
            silent_failure_message(&name, stderr.as_bytes(), lang_missing)
        } else {
            let mut notices = empty_result_notices(rendered_rows, &stats, lang_missing);
            let content = if let Some(path) = spill_if_needed(
                &rendered,
                word_boxes,
                &image,
                &args.lang,
                kind,
                &mut notices,
            ) {
                let shown = head(&rendered, MAX_LINES, MAX_BYTES);
                let mut content = if word_boxes {
                    format!(
                        "{} word boxes written to {}\n\n{}",
                        rendered_rows,
                        path.display(),
                        shown.content
                    )
                } else {
                    format!("written to {}\n\n{}", path.display(), shown.content)
                };
                record_truncation(&mut content, &shown, &mut notices, rendered.len());
                content
            } else {
                rendered.clone()
            };
            let mut content = content;
            ys_tool::append_notices(&mut content, &notices);
            content
        };
        Ok(ToolResult {
            content,
            is_error: silent_failure,
        })
    }
}

/// `degraded=` 的原因之一（design §1 头行）。
const DEGRADED_LANG_MISSING: &str = "lang-missing";

/// 单次尝试是否算失败：`None` ⇒ 采用本后端；`Some(原因)` ⇒ 记录并试下一个。
///
/// `stop != Eof` 是**我们杀的**（超限 / 中止 / 超时），此时 `code` 不承载语义 ⇒ 一律算失败，
/// 且原因要说清是哪一种，否则模型会把「超时」当成「引擎坏了」（`find.rs` 同口径）。
fn attempt_error(bounded: &Bounded, kind: Kind, prog: &Path) -> Option<String> {
    let label = kind.as_str();
    match bounded.stop {
        Stop::TimedOut => Some(format!(
            "{label} timed out after {}s",
            crate::search::DEADLINE.as_secs()
        )),
        Stop::Aborted => Some(format!("{label} was aborted")),
        Stop::Cap => Some(format!(
            "{label} produced more than {} KiB of TSV",
            crate::search::BYTE_CAP.div_ceil(1024)
        )),
        Stop::Eof if bounded.code == Some(0) => None,
        Stop::Eof => {
            let code = match bounded.code {
                Some(code) => format!("exit {code}"),
                None => "killed by a signal".to_owned(),
            };
            let detail = summarize_stderr(&bounded.stderr);
            Some(if detail.is_empty() {
                format!("{label} ({}) failed: {code}", prog.display())
            } else {
                format!("{label} ({}) failed: {code}: {detail}", prog.display())
            })
        }
    }
}

/// stderr 的**单行**摘要（压空白 + 截断）—— 多行原文会把错误正文撑成一页。
fn summarize_stderr(stderr: &[u8]) -> String {
    let flattened = String::from_utf8_lossy(stderr)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if flattened.chars().count() <= FAILURE_DETAIL_CHARS {
        return flattened;
    }
    let head: String = flattened.chars().take(FAILURE_DETAIL_CHARS).collect();
    format!("{head}…")
}

/// 某粒度实际可见的行数（与 [`tsv::render_at`] 的挑选规则一致）。
fn visible_rows(lines: &[tsv::OcrLine], granularity: Granularity) -> usize {
    lines
        .iter()
        .filter(|line| match granularity {
            Granularity::Line => line.word_no.is_none(),
            Granularity::Word => line.word_no.is_some(),
        })
        .count()
}

/// 静默失败判据：`exit 0` 但 stdout **没有非空白字节** ⇒ 引擎没跑通。
///
/// 只对 **Tesseract** 成立：它的 TSV 模式即使图中无字也会吐头行 + level 1-3（页 / 块 / 段），
/// 故空 stdout 不可能是「真没字」；`vision.swift` 不吐头行，空图本来就是零字节，把它算成
/// 失败会误杀 Vision 的正常「图里没字」。
///
/// 「没有输出」= 没有任何非空白字节：只剩一个 `\n` 同样是没输出，否则一个空行就能绕过本档、
/// 把静默失败退化成「图里没字」。
///
/// 提成纯函数使判据可脱离平台候选链测试 —— [`Kind::order`] 在非 macOS 上不含 Vision，
/// 端到端用例在那些平台上跑不到 Vision 分支。
fn is_silent_failure(kind: Kind, stdout: &[u8]) -> bool {
    matches!(kind, Kind::Tesseract) && !stdout.iter().any(|byte| !byte.is_ascii_whitespace())
}

/// 空结果**第四**种成因的文案：`exit 0` 但引擎 stdout 一个字节都没有（静默失败）。
///
/// **不得**说「图中没字」—— 那会把「引擎没跑通」说成「图里真没东西」，模型据此会
/// 放弃重试或改成手工描述图片。措辞必须把成因钉在引擎上，并交出 stderr（唯一线索）。
fn silent_failure_message(name: &str, stderr: &[u8], lang_missing: bool) -> String {
    let detail = summarize_stderr(stderr);
    let mut message = format!(
        "ocr: the OCR engine produced no output at all for {name} — it exited successfully but \
         wrote nothing to stdout, so the text was not read (this is an engine failure, not an \
         image without text)"
    );
    if !detail.is_empty() {
        message.push_str(&format!(": {detail}"));
    }
    if lang_missing {
        message.push_str(&format!(". {}", OcrEngine::install_hint()));
    }
    message
}

/// 空结果三分的 notice（design §3.2，**不可合并**）。
///
/// 三种成因在头行上的读数不同：真没字（`lines=0 filtered=0`）、全被滤（`lines=0
/// filtered>0`）、缺语言包（stderr 有话说）。合并成同一句会让模型对第一种反复重试。
///
/// **第四种成因（引擎 stdout 全空）不在这里** —— 它不再是「空结果 + notice」而是错误，
/// 见 [`silent_failure_message`]：此时若走本条函数，notice 会声称「图里真没字」。
fn empty_result_notices(
    rendered_rows: usize,
    stats: &FilterStats,
    lang_missing: bool,
) -> Vec<String> {
    let mut notices = Vec::new();
    if rendered_rows == 0 {
        if stats.filtered == 0 {
            notices.push(
                "no text detected in the image (nothing was filtered out either — the image \
                 really has no readable text)"
                    .to_owned(),
            );
        } else {
            notices.push(format!(
                "no text detected: all {} candidate rows were dropped as too narrow (width < 6 \
                 px, i.e. strokes and quotes) — retry with word_boxes=true to see them",
                stats.filtered
            ));
        }
    }
    if lang_missing {
        notices.push(format!(
            "the engine reported 'Failed loading language' — the language pack is probably \
             missing, so text was recognised with whatever packs are installed. {}",
            OcrEngine::install_hint()
        ));
    }
    notices
}

/// 落盘判定 + 半文件防护（§4.3）。返回 `Some(路径)` ⇒ 已落盘且内联必须截断。
///
/// **顺序是承重的**：写入的永远是 [`rendered`] 全量，截断只发生在其后的回显上。
/// `word_boxes` 恒落盘（词级内联会被误读成「词表只有这么多行」）。
fn spill_if_needed(
    rendered: &str,
    word_boxes: bool,
    image: &Path,
    lang: &str,
    kind: Kind,
    notices: &mut Vec<String>,
) -> Option<PathBuf> {
    if !word_boxes && rendered.len() <= SOFT_RESULT_BYTES {
        return None;
    }

    let stem = image
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_owned());
    let ocr_dir = Spill::dir();
    match Spill::write(&ocr_dir, &stem, image, lang, kind, word_boxes, rendered) {
        Ok(path) => {
            notices.push(TEMP_FILE_WARNING.to_owned());
            Some(path)
        }
        Err(e) => {
            // 落盘失败不能吞成成功：此时回显就是**唯一**的一份产物，必须让截断可见。
            notices.push(format!(
                "could not write the full result to {}: {e}",
                ocr_dir.display()
            ));
            None
        }
    }
}

/// 把「被截断」这件事说清楚：文件是全量、回显只到硬闸门（§4.3）。
fn record_truncation(
    content: &mut String,
    shown: &Truncation,
    notices: &mut Vec<String>,
    total_bytes: usize,
) {
    if shown.hit.is_none() {
        return;
    }
    content.push_str(&format!(
        "\n[truncated: showing lines 1-{} of {}. The file above is complete.]",
        shown.shown_lines, shown.total_lines
    ));
    notices.push(ys_tool::size_notice(total_bytes, None));
}

/// 读图像头得出 `(W, H)`；任何不认识 / 读不到的形态都返回 `(0, 0)`。
///
/// **只读文件头，不解码像素**：解码要引图像库（违零新增依赖），而解压炸弹的防线靠
/// `metadata.len()`（见 [`validate`]）；头行里的尺寸只是为了给坐标系一个参照。
/// 支持 PNG / JPEG / GIF / BMP / WebP / TIFF 六种（与 `spec` 的扩展名白名单同集）。
fn image_dims(image: &Path) -> (u32, u32) {
    use std::io::Read;

    let Ok(mut file) = std::fs::File::open(image) else {
        return (0, 0);
    };
    let mut head = vec![0u8; DIM_HEAD_BYTES];
    let Ok(read) = file.read(&mut head) else {
        return (0, 0);
    };
    head.truncate(read);
    dims_from_header(&head).unwrap_or((0, 0))
}

/// [`image_dims`] 的纯函数内核：前缀字节 → `(W, H)`。
fn dims_from_header(head: &[u8]) -> Option<(u32, u32)> {
    if let Some(rest) = head.strip_prefix(b"\x89PNG\r\n\x1a\n") {
        // IHDR 恒是第一个 chunk：4 字节长度 + "IHDR" + 宽高各 4 字节大端。
        let body = rest.get(4..)?;
        if body.get(..4)? != b"IHDR" {
            return None;
        }
        let w = u32::from_be_bytes(body.get(4..8)?.try_into().ok()?);
        let h = u32::from_be_bytes(body.get(8..12)?.try_into().ok()?);
        return Some((w, h));
    }
    if let Some(rest) = head
        .strip_prefix(b"GIF87a")
        .or_else(|| head.strip_prefix(b"GIF89a"))
    {
        // 逻辑屏幕宽高是小端 u16（GIF 没有 32 位尺寸）。
        let w = u16::from_le_bytes(rest.get(..2)?.try_into().ok()?);
        let h = u16::from_le_bytes(rest.get(2..4)?.try_into().ok()?);
        return Some((u32::from(w), u32::from(h)));
    }
    if let Some(rest) = head.strip_prefix(b"BM") {
        // BITMAPINFOHEADER：宽高是有符号 32 位小端，高度为负表示自上而下。
        let w = i32::from_le_bytes(rest.get(18..22)?.try_into().ok()?);
        let h = i32::from_le_bytes(rest.get(22..26)?.try_into().ok()?);
        return Some((w.unsigned_abs(), h.unsigned_abs()));
    }
    if head.get(..4) == Some(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        return webp_dims(head.get(12..)?);
    }
    if let Some(rest) = head.strip_prefix(b"II\x2a\x00") {
        return tiff_dims(rest, true);
    }
    if let Some(rest) = head.strip_prefix(b"MM\x00\x2a") {
        return tiff_dims(rest, false);
    }
    jpeg_dims(head)
}

/// WebP 三种容器：`VP8 `（有损）/ `VP8L`（无损）/ `VP8X`（扩展），宽高都在各自头里。
fn webp_dims(rest: &[u8]) -> Option<(u32, u32)> {
    match rest.get(..4)? {
        b"VP8 " => {
            let frame = rest.get(8..)?;
            // 关键帧起始码 9d 01 2a 之后紧跟 14 位宽、14 位高（小端，各含 2 位缩放）。
            if frame.get(3..6)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            let w = u16::from_le_bytes(frame.get(6..8)?.try_into().ok()?) & 0x3fff;
            let h = u16::from_le_bytes(frame.get(8..10)?.try_into().ok()?) & 0x3fff;
            Some((u32::from(w), u32::from(h)))
        }
        b"VP8L" => {
            // 签名 0x2f 之后 28 位：宽-1 与高-1 各 14 位。
            let bits = u32::from_le_bytes(rest.get(9..13)?.try_into().ok()?);
            if rest.get(8) != Some(&0x2f) {
                return None;
            }
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        // `VP8X`：24 位标志 + 24 位画布宽-1 + 24 位画布高-1。
        b"VP8X" => {
            let canvas = rest.get(12..18)?;
            let w = u32::from(canvas[0]) | u32::from(canvas[1]) << 8 | u32::from(canvas[2]) << 16;
            let h = u32::from(canvas[3]) | u32::from(canvas[4]) << 8 | u32::from(canvas[5]) << 16;
            Some((w + 1, h + 1))
        }
        _ => None,
    }
}

/// TIFF：IFD0 里 tag 256 / 257 是宽高（`little` 决定字节序）。
fn tiff_dims(rest: &[u8], little: bool) -> Option<(u32, u32)> {
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

    let ifd = usize::try_from(read32(rest)?).ok()?;
    let ifd = rest.get(ifd..)?;
    let count = usize::from(read16(ifd)?);
    let mut dims = (None, None);
    for i in 0..count {
        let entry = ifd.get(2 + i * 12..)?;
        let tag = read16(entry)?;
        let kind = read16(&entry[2..])?;
        // 只有 SHORT(3) / LONG(4) 的尺寸是内联在值字段里的标量，其余形态一律不猜。
        let value = match kind {
            3 => read16(&entry[8..]).map(u32::from),
            4 => read32(&entry[8..]),
            _ => None,
        };
        match tag {
            256 => dims.0 = value,
            257 => dims.1 = value,
            _ => {}
        }
    }
    Some((dims.0?, dims.1?))
}

/// JPEG：逐个段走到 `SOFn`（`0xC0`–`0xCF`，跳过 `C4`/`C8`/`CC`），高宽是其后 5 字节的 2+2。
fn jpeg_dims(head: &[u8]) -> Option<(u32, u32)> {
    if head.get(..2)? != [0xff, 0xd8] {
        return None;
    }
    let mut i = 2;
    while i + 3 < head.len() {
        if head[i] != 0xff {
            return None;
        }
        let marker = head[i + 1];
        // 填充字节（连续 0xff）与无载荷的独立段都要跳过，否则长度解析会错位。
        if marker == 0xff {
            i += 1;
            continue;
        }
        if marker == 0xd8 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            i += 2;
            continue;
        }
        let len = usize::from(u16::from_be_bytes(head.get(i + 2..i + 4)?.try_into().ok()?));
        let is_sof = (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
        if is_sof {
            let seg = head.get(i + 4..)?;
            let h = u16::from_be_bytes(seg.get(1..3)?.try_into().ok()?);
            let w = u16::from_be_bytes(seg.get(3..5)?.try_into().ok()?);
            return Some((u32::from(w), u32::from(h)));
        }
        if len < 2 {
            return None;
        }
        i += 2 + len;
    }
    None
}

/// 校验 `path` 并返回**生效路径**（相对路径按 `workspace` 解析）。
///
/// 返回 `Err(文案)` 的三态取自 design §3.3：空 / 不存在 / 不是常规文件。文案**必须**说出是
/// 哪一种并回显路径 —— 模型据此区分「路径写错」与「工具坏了」，否则只会盲重试或换参数。
///
/// `workspace` 总是参与拼接（`Path::join` 对绝对路径原样返回）⇒ 两条路径共用一次解析。
fn validate(workspace: &Path, raw: &Path) -> Result<PathBuf, String> {
    if raw.as_os_str().is_empty() {
        return Err(
            "ocr: 'path' is empty — pass the path of an image file, e.g. ocr(path=\"shot.png\")"
                .to_owned(),
        );
    }

    let image = workspace.join(raw);
    // `metadata` 跟随符号链接 ⇒ 指向目录的链接也归入「不是常规文件」，与用户直觉一致。
    let meta = match std::fs::metadata(&image) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("ocr: image not found: {}", image.display()));
        }
        Err(e) => {
            return Err(format!("ocr: cannot stat {}: {e}", image.display()));
        }
    };
    if !meta.is_file() {
        return Err(format!(
            "ocr: not a regular file: {} (directories, FIFOs and devices cannot be read as \
             images — pass the image file inside it)",
            image.display()
        ));
    }

    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一次性临时目录（名字含 `pid` + 进程内原子序号 ⇒ 并行安全）。
    fn dir(tag: &str) -> PathBuf {
        let dir = crate::test_support::test_dir(&format!("ocr_tool_{tag}"));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn tool(dir: &Path) -> OcrTool {
        OcrTool::new().with_workspace(dir.to_path_buf())
    }

    async fn run(tool: &OcrTool, input: Value) -> ToolResult {
        tool.call(input, crate::test_support::ctx())
            .await
            .expect("校验失败走 error 结果，不抛 Err")
    }

    /// 必填字段名是 `path`，**不是** `image`（design §1 的旧名残留会静默漂移契约）。
    #[test]
    fn schema_requires_path_and_does_not_mention_image() {
        let spec = OcrTool::new().spec();
        assert_eq!(spec.name, "ocr");

        let required = spec.parameters["required"]
            .as_array()
            .expect("schema 必须有 required 数组");
        assert_eq!(required.len(), 1, "只有 `path` 必填，其余两个有缺省值");
        assert_eq!(required[0].as_str(), Some("path"));

        let props = spec.parameters["properties"]
            .as_object()
            .expect("schema 必须有 properties");
        for key in ["path", "lang", "word_boxes"] {
            assert!(props.contains_key(key), "schema 缺字段 `{key}`");
        }
        assert_eq!(props.len(), 3, "参数面只有 3 个字段（design §1）");
        assert!(
            !props.contains_key("image"),
            "必填字段名是 `path`，不是 `image`"
        );
    }

    /// 缺 `path` ⇒ 解析失败（`path` 无默认值，`required` 在解析层同样成立）。
    #[tokio::test]
    async fn missing_path_fails_to_parse() {
        let dir = dir("missing_path");
        let err = tool(&dir)
            .call(json!({ "lang": "eng" }), crate::test_support::ctx())
            .await
            .expect_err("缺 path 应解析失败");
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "应为 InvalidInput，实际 {err:?}"
        );
        assert!(
            err.to_string().contains("path"),
            "文案要点名缺的字段：{err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空串 `path` ⇒ error 结果（不是 panic，也不是 `Err`），文案说明是哪一种情况。
    #[tokio::test]
    async fn empty_path_is_an_error_result() {
        let dir = dir("empty_path");
        let result = run(&tool(&dir), json!({ "path": "" })).await;
        assert!(result.is_error, "空 path 应是 error 结果：{result:?}");
        assert!(
            result.content.contains("'path' is empty"),
            "文案要说出「为空」：{}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件不存在 ⇒ error 结果，且**回显该路径**（模型据此判断是路径写错）。
    #[tokio::test]
    async fn nonexistent_path_is_an_error_result_echoing_the_path() {
        let dir = dir("not_found");
        let missing = dir.join("shot.png");
        let result = run(&tool(&dir), json!({ "path": "shot.png" })).await;

        assert!(result.is_error, "不存在的文件应是 error 结果");
        assert!(
            result.content.contains(&missing.display().to_string()),
            "文案必须回显生效路径 {}：{}",
            missing.display(),
            result.content
        );
        assert!(
            result.content.contains("not found"),
            "文案要说出「不存在」：{}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目录 ⇒ error 结果（`is_file()` 为假），文案与「不存在」区分开。
    #[tokio::test]
    async fn directory_is_rejected_as_not_a_regular_file() {
        let dir = dir("is_dir");
        let result = run(&tool(&dir), json!({ "path": "." })).await;

        assert!(result.is_error, "目录应是 error 结果");
        assert!(
            result.content.contains("not a regular file"),
            "文案要说出「不是常规文件」：{}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `lang` 的缺省值是 `chi_sim+eng`（design §2.4），且 `Default` 是唯一真值。
    #[test]
    fn default_lang_is_chi_sim_plus_eng() {
        assert_eq!(Args::default().lang, DEFAULT_LANG);
        assert_eq!(DEFAULT_LANG, "chi_sim+eng");
        assert!(!Args::default().word_boxes);
    }

    /// 只读输入 + 幂等落盘 ⇒ 批内可并发（design §7）。
    #[test]
    fn is_concurrency_safe() {
        assert!(OcrTool::new().is_concurrency_safe());
    }

    /// **承重**（design §3.1）：不实现 `timeout()` ⇒ 外层回落到 `bash_timeout`（300s），
    /// 内层 60s 严格早于外层，内层超时才走「杀进程组」那条路径。
    #[test]
    fn timeout_is_not_declared() {
        assert!(
            OcrTool::new().timeout().is_none(),
            "OcrTool 必须不声明 timeout（见模块头 / design §3.1）"
        );
    }

    // ---------------------------------------------------------------------
    // 流水线（假 `tesseract` 走 `test_support::fake_bin`：开发机不装引擎也能跑）
    // ---------------------------------------------------------------------

    /// 一张**只读头就够**的假 PNG：签名 + IHDR（宽高）—— 让头行的 `WxH` 有确定值。
    ///
    /// 尾随的 `noise` 用来让同一次测试运行内的**内容哈希**各不相同：落盘名取自
    /// `image_bytes ‖ lang ‖ granularity`，两份字节相同的图会命中同一个文件。
    fn write_png(dir: &Path, name: &str, w: u32, h: u32, noise: &str) -> PathBuf {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 13]);
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&w.to_be_bytes());
        bytes.extend_from_slice(&h.to_be_bytes());
        bytes.extend_from_slice(noise.as_bytes());
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("写假 PNG");
        path
    }

    /// 在 `bin` 目录造假 `tesseract`，并把该目录当作**唯一**的 `PATH` 注入探测结果。
    fn tool_with_fake_engine(dir: &Path, script_body: &str) -> OcrTool {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("建假 bin 目录");
        crate::test_support::fake_bin(&bin, "tesseract", script_body);
        OcrTool::new()
            .with_workspace(dir.to_path_buf())
            .with_engine(OcrEngine::detect_from(&bin.to_string_lossy()))
    }

    /// 两行 TSV（行级 + 各自一个词）—— 与 `tsv.rs` 的 golden fixture 同数值。
    fn two_line_tsv() -> String {
        [
            "4\t1\t1\t1\t1\t1\t62\t49\t890\t28\t96\t第三章 配置文件说明",
            "5\t1\t1\t1\t1\t1\t62\t49\t890\t28\t96\t第三章",
            "4\t1\t1\t1\t2\t1\t61\t116\t906\t26\t91\t应用程序启动时会依次读取以下三个位置的配置文件，",
            "5\t1\t1\t1\t2\t1\t61\t116\t906\t26\t91\t应用程序启动时会依次读取以下三个位置的配置文件，",
        ]
        .join("\n")
    }

    /// 把任意 TSV 固定输出的假引擎脚本。
    fn echoing_tsv(tsv: &str) -> String {
        format!("cat <<'YUSHAN_TSV_EOF'\n{tsv}\nYUSHAN_TSV_EOF\n")
    }

    /// 正常路径：假引擎吐 2 行 TSV ⇒ 头行正确、两行行级记录、`is_error == false`。
    ///
    /// 头行文案是 design §1 的**逐字节**契约（`WxH` 来自 PNG 文件头，不是结果坐标的上界）。
    #[tokio::test]
    async fn pipeline_renders_two_line_records_with_correct_header() {
        let dir = dir("pipeline_ok");
        write_png(&dir, "shot.png", 1200, 900, "pipeline_ok");
        let tool = tool_with_fake_engine(&dir, &echoing_tsv(&two_line_tsv()));

        let result = run(&tool, json!({ "path": "shot.png" })).await;
        assert!(!result.is_error, "成功的 OCR 不是 error：{result:?}");

        let content = &result.content;
        assert!(
            content.starts_with(
                "# shot.png 1200x900 via=tesseract lang=chi_sim+eng lines=2 filtered=0"
            ),
            "头行与 design §1 不符：{content}"
        );
        assert!(
            content.contains("\n1 (62,49,890,28) conf_min:96 第三章 配置文件说明\n"),
            "第一行记录不对：{content}"
        );
        assert!(
            content.contains(
                "\n2 (61,116,906,26) conf_min:91 应用程序启动时会依次读取以下三个位置的配置文件，\n"
            ),
            "第二行记录不对：{content}"
        );
        // 内联分支：8 KiB 内不落盘，结果里不该出现临时路径。
        assert!(!content.contains("written to"), "小结果应内联：{content}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `word_boxes=true` ⇒ **恒落盘**（即使结果很小），且落盘文件是**未截断的全量**。
    ///
    /// 变异测试的靶心：把写入改成「先 `head` 再写」⇒ 本用例的逐字节比对变红。
    #[tokio::test]
    async fn word_boxes_always_spills_and_file_is_the_full_render() {
        let dir = dir("word_boxes_spill");
        let image = write_png(&dir, "shot.png", 1200, 900, "word_boxes_spill");
        let tool = tool_with_fake_engine(&dir, &echoing_tsv(&two_line_tsv()));

        let result = run(&tool, json!({ "path": "shot.png", "word_boxes": true })).await;
        assert!(!result.is_error, "word_boxes 的成功路径：{result:?}");
        assert!(
            result.content.contains("word boxes written to"),
            "word_boxes 必须落盘并说明：{}",
            result.content
        );

        // 期望值由**纯函数**独立算出（不复用流水线的产物），才是真正的逐字节比对。
        let parsed = tsv::parse_tsv(&two_line_tsv());
        let (lines, stats) = tsv::filter(parsed);
        let expected = tsv::render_at(
            &Header {
                name: "shot.png",
                width: 1200,
                height: 900,
                via: "tesseract",
                lang: "chi_sim+eng",
                lines: 2,
                filtered: 0,
                degraded: None,
            },
            &lines,
            Granularity::Word,
        );
        assert_eq!(stats.filtered, 0, "两行都不该被滤");

        let target = spill_path_from(&result.content);
        let written = std::fs::read_to_string(&target).expect("落盘文件应存在");
        assert_eq!(
            written, expected,
            "落盘必须是未截断的全量（且格式与内联完全一致）"
        );
        assert!(
            result.content.contains("1.1 (62,49,890,28) conf:96 第三章"),
            "词级内联形态：{}",
            result.content
        );
        assert!(
            result.content.contains("temp file"),
            "落盘路径必须自述临时性（design §3.4）：{}",
            result.content
        );

        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = image;
    }

    /// 引擎全不可用 ⇒ error 结果，且文案含**两平台**安装命令（design §2.2 / §7）。
    #[tokio::test]
    async fn no_engine_available_is_an_error_with_install_hint() {
        let dir = dir("no_engine");
        write_png(&dir, "shot.png", 40, 40, "no_engine");
        // 空目录 ⇒ 链为空。
        let empty_bin = dir.join("empty_bin");
        std::fs::create_dir_all(&empty_bin).expect("建空 bin 目录");
        let tool = OcrTool::new()
            .with_workspace(dir.to_path_buf())
            .with_engine(OcrEngine::detect_from(&empty_bin.to_string_lossy()));

        let result = run(&tool, json!({ "path": "shot.png" })).await;
        assert!(result.is_error, "无可用后端应是 error 结果：{result:?}");
        for needle in [
            "no OCR backend",
            "xcode-select --install",
            "apt install tesseract-ocr",
            "brew install tesseract",
        ] {
            assert!(
                result.content.contains(needle),
                "安装提示缺 `{needle}`：{}",
                result.content
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 全部候选失败 ⇒ error 结果，且**每个**后端的原因都在（design §2.2 / §7）。
    ///
    /// 只报最后一个会让「Vision 挂了 + tesseract 没装」退化成「tesseract 没装」。
    #[tokio::test]
    async fn all_backends_failing_reports_every_reason() {
        let dir = dir("all_fail");
        write_png(&dir, "shot.png", 40, 40, "all_fail");
        let tool = tool_with_fake_engine(
            &dir,
            "echo 'tesseract: cannot open data file /opt/lang/chi_sim.traineddata' >&2\nexit 3\n",
        );

        let result = run(&tool, json!({ "path": "shot.png" })).await;
        assert!(result.is_error, "全失败应是 error 结果：{result:?}");
        assert!(
            result.content.contains("tesseract") && result.content.contains("exit 3"),
            "要带后端名与退出码：{}",
            result.content
        );
        assert!(
            result.content.contains("cannot open data file"),
            "要带 stderr 摘要：{}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空结果三分之二：`lines == 0 && filtered == 0` **且有 TSV 输出** ⇒ **成功**说
    /// 「图中未检出文字」。
    ///
    /// 假引擎必须吐 tesseract 的**真实无字形态**（头行 + level 1-3 的页/块/段记录）：
    /// 真 tesseract 即使图是空白图也吐这两行，一字节不吐是引擎静默失败（见下一条用例）。
    #[tokio::test]
    async fn empty_output_is_a_success_saying_no_text_was_detected() {
        let dir = dir("empty_output");
        write_png(&dir, "blank.png", 30, 30, "empty_output");
        let tsv = [
            "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext",
            "1\t1\t0\t0\t0\t0\t0\t0\t30\t30\t-1\t",
            "2\t1\t1\t0\t0\t0\t0\t0\t30\t30\t-1\t",
            "3\t1\t1\t1\t0\t0\t0\t0\t30\t30\t-1\t",
        ]
        .join("\n");
        let tool = tool_with_fake_engine(&dir, &echoing_tsv(&tsv));

        let result = run(&tool, json!({ "path": "blank.png" })).await;
        assert!(!result.is_error, "「图里没字」不是 error：{result:?}");
        assert!(
            result
                .content
                .starts_with("# blank.png 30x30 via=tesseract"),
            "头行要带 `lines=0 filtered=0`：{}",
            result.content
        );
        assert!(
            result.content.contains("no text detected in the image"),
            "要明说图中未检出文字：{}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空结果第四档（**静默失败**）：`exit 0` 但 stdout **一个字节都没有** ⇒ error，且措辞
    /// 把成因钉在引擎上（**不得**说「图中没字」，见 `silent_failure_message`）。
    ///
    /// 这条是 Tesseract argv 漏 `outputbase` 那一类 bug 的现场：结果被写进文件、stdout
    /// 全空、退出码 0 —— exit code 在这里完全不可用，判据只能是「有没有任何输出」。
    #[tokio::test]
    async fn silent_engine_with_no_output_is_an_error_not_an_empty_image() {
        let dir = dir("silent_engine");
        write_png(&dir, "shot.png", 40, 40, "silent_engine");
        let tool = tool_with_fake_engine(
            &dir,
            "echo \"read_params_file: Can't open tsv\" >&2\nexit 0\n",
        );

        let result = run(&tool, json!({ "path": "shot.png" })).await;
        assert!(
            result.is_error,
            "引擎一字节不吐是失败，不是空结果：{result:?}"
        );
        assert!(
            result.content.contains("produced no output at all"),
            "要说出「引擎没有任何输出」：{}",
            result.content
        );
        assert!(
            result
                .content
                .contains("engine failure, not an image without text"),
            "要把成因钉在引擎上：{}",
            result.content
        );
        assert!(
            !result.content.contains("no text detected"),
            "不得把它说成「图中没字」：{}",
            result.content
        );
        assert!(
            result.content.contains("read_params_file"),
            "要带上 stderr（唯一线索）：{}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只看空白的 stdout（只有一个换行）不能绕过静默失败档 —— 判据是「没有任何非空白
    /// 字节」，不是「字节数为 0」。
    #[tokio::test]
    async fn whitespace_only_output_still_counts_as_no_output() {
        let dir = dir("blank_line_output");
        write_png(&dir, "shot.png", 40, 40, "blank_line_output");
        let tool = tool_with_fake_engine(&dir, "printf '\\n'\nexit 0\n");

        let result = run(&tool, json!({ "path": "shot.png" })).await;
        assert!(result.is_error, "只剩空行也是引擎失败：{result:?}");
        assert!(
            result.content.contains("produced no output at all")
                && !result.content.contains("no text detected"),
            "措辞必须把成因钉在引擎上：{}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 静默失败判据（[`is_silent_failure`]）**只对 Tesseract 成立**，与平台无关。
    ///
    /// 这是下面那条 macOS 端到端用例在**所有平台**上的替身：`Kind::order` 在非 macOS 上
    /// 不含 Vision，端到端到不了该分支，但判据本身必须钉死 —— 否则「零字节 = 失败」会误杀
    /// Vision 的正常「图里没字」。
    #[test]
    fn silent_failure_is_tesseract_only() {
        // Tesseract：零字节 / 只有空白 ⇒ 静默失败（引擎没跑通）。
        for blank in [b"".as_slice(), b"\n", b" \n\t\r\n"] {
            assert!(
                is_silent_failure(Kind::Tesseract, blank),
                "Tesseract 的零字节/纯空白输出应判为静默失败：{blank:?}"
            );
        }
        // 只要有**一个**非空白字节就不算静默 —— 头行也是真输出。
        assert!(
            !is_silent_failure(Kind::Tesseract, b"level\tpage_num\n"),
            "有任何非空白字节 = 引擎吐了东西，不是静默失败"
        );
        // Vision：零字节是**正常空结果**，不得判失败。
        for blank in [b"".as_slice(), b"\n", b"  \n"] {
            assert!(
                !is_silent_failure(Kind::Vision, blank),
                "Vision 的零字节输出是正常空结果，不得判成失败：{blank:?}"
            );
        }
    }

    /// 静默失败档**只对 Tesseract** 成立：`vision.swift` 不吐头行，空白图本来就是零字节
    /// 输出 —— 把它判成失败会把 Vision 的正常「图里没字」误报成引擎坏了。
    ///
    /// 限 macOS：本用例靠 `detect_from` 走**平台候选链**，而 [`Kind::order`] 在非 macOS 上
    /// 只有 Tesseract ⇒ 探不到 Vision、根本到不了该分支。判据本身与平台无关，由
    /// [`silent_failure_is_tesseract_only`] 钉住。
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn zero_byte_vision_output_is_still_a_plain_empty_result() {
        let dir = dir("silent_vision");
        write_png(&dir, "blank.png", 30, 30, "silent_vision");
        // 只放 `vision`：候选链里没有 tesseract，走的必然是 Vision 档。
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("建假 bin 目录");
        crate::test_support::fake_bin(&bin, "vision", "exit 0\n");
        let tool = OcrTool::new()
            .with_workspace(dir.to_path_buf())
            .with_engine(OcrEngine::detect_from(&bin.to_string_lossy()));

        let result = run(&tool, json!({ "path": "blank.png" })).await;
        assert!(
            !result.is_error,
            "Vision 的零字节输出是正常空结果：{result:?}"
        );
        assert!(
            result.content.contains("via=vision") && result.content.contains("lines=0"),
            "头行要如实报 lines=0：{}",
            result.content
        );
        assert!(
            result.content.contains("no text detected in the image"),
            "要明说图中未检出文字：{}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空结果三分之三：`lines == 0 && filtered > 0`（全被 `width<6` 滤掉）⇒ 成功 + 建议
    /// `word_boxes=true`。与上一条**不可合并**：成因不同，处置也不同。
    #[tokio::test]
    async fn all_rows_filtered_suggests_word_boxes() {
        let dir = dir("all_filtered");
        write_png(&dir, "thin.png", 30, 30, "all_filtered");
        let tsv = [
            "4\t1\t1\t1\t1\t1\t5\t5\t3\t20\t88\t1",
            "4\t1\t1\t1\t2\t1\t5\t40\t2\t20\t80\t\"",
        ]
        .join("\n");
        let tool = tool_with_fake_engine(&dir, &echoing_tsv(&tsv));

        let result = run(&tool, json!({ "path": "thin.png" })).await;
        assert!(!result.is_error, "全被滤不是 error：{result:?}");
        assert!(
            result.content.contains("filtered=2"),
            "头行要如实计数：{}",
            result.content
        );
        assert!(
            result.content.contains("word_boxes=true"),
            "要建议用 word_boxes 复取：{}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 语言包缺失：stderr 含 `Failed loading language` ⇒ **成功**结果 + `degraded=lang-missing`
    /// （不是 error —— 引擎确实跑出了结果，只是用了别的语言包）。
    #[tokio::test]
    async fn missing_language_pack_degrades_instead_of_failing() {
        let dir = dir("lang_missing");
        write_png(&dir, "shot.png", 100, 60, "lang_missing");
        let script = format!(
            "echo 'Failed loading language \'chi_sim\'\nTesseract could not load any languages' >&2\n{}",
            echoing_tsv(&two_line_tsv())
        );
        let tool = tool_with_fake_engine(&dir, &script);

        let result = run(&tool, json!({ "path": "shot.png" })).await;
        assert!(
            !result.is_error,
            "缺语言包是降级，不是失败（design §3.2）：{result:?}"
        );
        assert!(
            result.content.contains("degraded=lang-missing"),
            "头行要标降级：{}",
            result.content
        );
        assert!(
            result.content.contains("Failed loading language"),
            "notice 要引用引擎原话：{}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **最关键的一条**（design §4.3）：渲染结果 > 硬闸门 ⇒ 内联被截断（≤ 硬闸门），
    /// 而**落盘是全量** —— 断言 `落盘内容.len() > 内联.len()`。
    ///
    /// **数据必须越过 50 KiB，不能只越过 8 KiB 软阈值**：软阈值只决定「是否落盘」，
    /// 内联是否被截断取决于 [`MAX_BYTES`] / [`MAX_LINES`]。8 KiB–50 KiB 区间内落盘是
    /// 保险、内联是全量 ⇒ 此时 `落盘 > 内联` 必然不成立（见下一条用例）。
    ///
    /// 变异测试的靶心：把「落盘内容」改成先 `head` 再写，或把内联的 `head` 换成软阈值
    /// ⇒ 本用例变红。
    #[tokio::test]
    async fn oversized_result_truncates_inline_but_spills_everything() {
        let dir = dir("oversized");
        write_png(&dir, "long.png", 1200, 2000, "oversized_result");
        // 900 行 × ~240 字节 ≈ 210 KiB —— 越过 50 KiB 硬闸门（行数仍 < 2000），
        // 故内联**必定**被 `head` 截断，而落盘是全量。
        let script = "pad=$(printf '%0200d' 0)\ni=0\nwhile [ \"$i\" -lt 900 ]; do \
                      printf '4\\t1\\t1\\t1\\t1\\t1\\t10\\t10\\t200\\t20\\t90\\tline %s %s\\n' \"$i\" \"$pad\"; \
                      i=$((i+1)); \
                    done\n";
        let tool = tool_with_fake_engine(&dir, script);

        let result = run(&tool, json!({ "path": "long.png" })).await;
        assert!(!result.is_error, "大结果不是 error：{result:?}");

        let target = spill_path_from(&result.content);
        let written = std::fs::read_to_string(&target).expect("落盘文件应存在");
        let inline = inline_body(&result.content);

        assert!(
            written.len() > SOFT_RESULT_BYTES,
            "落盘应是全量（> 8 KiB），实际 {} 字节",
            written.len()
        );
        assert!(
            written.len() > inline.len(),
            "落盘必须大于内联：落盘 {} vs 内联 {}",
            written.len(),
            inline.len()
        );
        // `head_lines` 的字节预算**不含**行分隔符（与 `read` 逐字节一致），故内联正文最多
        // 超出一个「每行一个 `\n`」的余量。
        // 下界同样承重：内联取的是落盘正文的**头**（`head` 保首行无条件），只靠「内联 ≤ 硬闸门」
        // 一条断言在「把 MAX_BYTES 误改成 SOFT_RESULT_BYTES」时仍会成立 —— 那是更宽的错，
        // 不是更窄的错。
        assert!(
            inline.len() >= MAX_BYTES && inline.len() <= MAX_BYTES + MAX_LINES,
            "内联应恰好填满硬闸门 {MAX_BYTES}（+ 行分隔符余量），实际 {} —— 太小说明截断阈值\
             不是 MAX_BYTES",
            inline.len()
        );
        assert!(
            inline.lines().count() <= MAX_LINES,
            "内联行数不得超硬闸门 {MAX_LINES}"
        );
        assert!(
            result.content.contains("The file above is complete"),
            "要说明落盘文件是全量：{}",
            result.content
        );
        // 900 行行级记录全在文件里 —— 证明写的是**未截断**的渲染结果。
        assert_eq!(written.lines().count(), 901, "头行 + 900 行记录都应落盘");

        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 渲染在 8 KiB–50 KiB 之间时，落盘是保险而非必需 —— 内联就是全量（design §4.3）。
    ///
    /// 这条防止有人把 `spill_if_needed` 的 8 KiB 软阈值误当成截断阈值（「一超 8 KiB 就
    /// 截断」）：那样内联会比落盘短，本用例变红。
    #[tokio::test]
    async fn result_between_soft_and_hard_limit_spills_without_truncating_inline() {
        let dir = dir("midband");
        write_png(&dir, "mid.png", 1200, 2000, "midband_result");
        // 200 行 × ~67 字节 ≈ 13 KiB —— 稳在 [8 KiB, 50 KiB) 区间内，行数也远小于 2000。
        let script = "pad=$(printf '%030d' 0)\ni=0\nwhile [ \"$i\" -lt 200 ]; do \
                      printf '4\\t1\\t1\\t1\\t1\\t1\\t10\\t10\\t200\\t20\\t90\\tline %s %s\\n' \"$i\" \"$pad\"; \
                      i=$((i+1)); \
                    done\n";
        let tool = tool_with_fake_engine(&dir, script);

        let result = run(&tool, json!({ "path": "mid.png" })).await;
        assert!(!result.is_error, "中等结果不是 error：{result:?}");

        let target = spill_path_from(&result.content);
        let written = std::fs::read_to_string(&target).expect("落盘文件应存在");
        // 路径行与正文之间是 `\n\n`，`inline_body` 会把那个空隔行当成正文首行 —— 去掉它
        // 才能与文件内容**逐字节**比。
        let inline = inline_body(&result.content);
        let inline = inline.trim_start_matches('\n');

        // 前提：数据确实落在两阈值之间（否则本用例测不到那个区间，会退化成废话）。
        assert!(
            written.len() > SOFT_RESULT_BYTES && written.len() < MAX_BYTES,
            "数据应落在 [软阈值 {SOFT_RESULT_BYTES}, 硬闸门 {MAX_BYTES}) 之间，实际 {} 字节",
            written.len()
        );
        assert_eq!(
            inline.len(),
            written.len(),
            "8 KiB–50 KiB 区间内联不截断：内联 {} vs 落盘 {}",
            inline.len(),
            written.len()
        );
        assert_eq!(written.lines().count(), 201, "头行 + 200 行记录都应落盘");

        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 从结果正文里取出落盘路径（`written to <path>` 那一行的行尾）。
    fn spill_path_from(content: &str) -> PathBuf {
        let line = content
            .lines()
            .find(|line| line.contains("written to "))
            .unwrap_or_else(|| panic!("结果里应有落盘路径：{content}"));
        let start = line.find("written to ").expect("已确认含该子串") + "written to ".len();
        PathBuf::from(line[start..].trim())
    }

    /// 内联正文 = 落盘路径行之后的全部内容（截止到 notice 段）。
    ///
    /// 行尾的 `[truncated: …]` 与末尾的 `[notice. notice]` 都属于**元信息**，不是内联的
    /// OCR 正文 —— 要是不剔除，一个只剩 3 行的内联会被它们撑得比落盘还「长」。
    fn inline_body(content: &str) -> String {
        let mut seen_path = false;
        let mut body: Vec<&str> = Vec::new();
        for line in content.lines() {
            if !seen_path {
                seen_path = line.contains("written to ");
                continue;
            }
            let meta = (line.starts_with('[') && line.ends_with(']'))
                || (line.starts_with("[truncated: ") && line.ends_with(']'));
            if meta {
                break;
            }
            body.push(line);
        }
        body.join("\n")
    }
}
