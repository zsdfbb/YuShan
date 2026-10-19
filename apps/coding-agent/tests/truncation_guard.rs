//! 截断保护的端到端验收（ADR-0020，测试矩阵 **N11**）。
//!
//! # 它复现的真实故障
//!
//! 模型被输出预算掐断时（provider 给 `finish_reason: "length"`），它**已经发出**
//! 的工具调用参数很可能是残缺的。旧行为照常执行 —— 于是一次「写文件写到一半」
//! 就变成**拿半个 payload 覆盖用户的文件**：内容被清空或写坏，且不可恢复。
//!
//! ADR-0020 的修法是**整批作废**：`finish_reason.is_truncated()` 为真时，那一批
//! 工具调用**一个都不执行**，全部合成 `is_error` 结果回喂模型（配对完整性不破），
//! 并发出一条非终局的 `ResponseTruncated`；循环继续，让模型下一轮改小步。
//!
//! # 场景（本地回环 mock，按请求号吐**脚本化** SSE）
//!
//! | 请求 | 内容 |
//! |---|---|
//! | 第 1 次（轮 1） | 两个 `write` 调用：`index 0` 参数**残缺**（JSON 故意不闭合）；`index 1` 参数**完整**。终止 chunk 的 `finish_reason` = `"length"` |
//! | 第 2 次（轮 2） | 纯文本 + `finish_reason: "stop"` |
//!
//! 两个写得动的目标是：
//!
//! - `victim.txt` —— **预先存在、装着用户的内容**（回归核心：不得被覆盖）；
//! - `partial.txt` —— 原本不存在（残缺那次调用的目标：不得被创建）。
//!
//! # 为什么这一批里必须有一个「参数完整」的调用（关键，别删）
//!
//! 适配器对**非法 JSON** 的参数是**降级成 `Value::Null`** 的
//! （`ToolCallAccumulator::into_content_block`）。因此若整批只有一个残缺调用，
//! 就算把 guard 删掉，`write` 也只会因「missing 'path'」报错 —— 文件照样不会
//! 出现，本用例**抓不到**回归（已实测：删 guard 后仅此一条调用时测试仍绿）。
//!
//! 真实的多文件写入恰好长这样：模型先发一个完整的 `write`（文件 A），发第二个
//! `write`（文件 B）时被掐断。ADR-0020 的语义是**整批作废**（而不是「只丢坏的、
//! 留下好的」），所以参数完整的那个也**必须不执行** —— `victim.txt` 就是钉住
//! 这条「整批」语义的哨兵，也是让变异真正变红的那一条。
//!
//! # 变异自检（已实测）
//!
//! 把 `crates/ys-loop/src/basic.rs` 的 `if response.finish_reason.is_truncated() {`
//! 改成 `if false {` → 第 1 批工具照常执行 → `victim.txt` 被写成
//! `"OVERWRITTEN BY A TRUNCATED TOOL CALL"` → 断言 1 **变红**。实测失败输出（节选）：
//!
//! ```text
//! assertion `left == right` failed: 被截断的那一批工具调用一个都不许执行：残缺的 write 会覆盖用户的文件！
//!   left: "OVERWRITTEN BY A TRUNCATED TOOL CALL"
//!  right: "ORIGINAL USER CONTENT — 必须原样存活\n"
//! ```
//!
//! 还原 guard 后本用例复绿（`cargo test -p ys-coding-agent --test truncation_guard`）。
//! 同一趟变异下，残缺那次调用（`arguments` 非法 JSON → 适配器降级为 `null`）只会
//! 报 `Invalid input: missing 'path'` —— 这正是「必须有一个参数完整的同批调用」
//! 的原因，见上一节。
//!
//! # 隔离
//!
//! 临时目录名含**进程内原子序号 + `process::id()`**（`SystemTime::as_nanos()` 在
//! macOS 只到微秒，并行测试会撞名 → 互删文件）。子进程的 `HOME` 指向**独立**临时
//! 目录 —— 绝不碰真实 `~/.yushan`。
//!
//! **不改写进程级环境变量**：所有 env 都经 `Command::env()` 注入**子进程**，故无需
//! `test_env::env_lock`（那是给同 binary 内 `set_var` 的用例用的）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

/// 预置的用户内容 —— 截断保护成立时它必须**一字不改**。
const USER_CONTENT: &str = "ORIGINAL USER CONTENT — 必须原样存活\n";

/// 若 guard 失效，完整的那个 `write` 会把它写进 `victim.txt`。
const OVERWRITE_SENTINEL: &str = "OVERWRITTEN BY A TRUNCATED TOOL CALL";

// ---------------------------------------------------------------------------
// 临时目录（进程内原子序号 + pid，并行安全）
// ---------------------------------------------------------------------------

/// 本用例独占的临时目录。调用方负责删除。
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "yushan_truncation_guard_{tag}_{}_{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// 脚本化 SSE
// ---------------------------------------------------------------------------

/// 一行的 SSE 帧（`data: <json>` + 空行分隔）。
fn frame(v: &Value) -> String {
    format!("data: {v}\n\n")
}

/// 一个只带单个 tool call 的 delta chunk（参数按**原文**透传 —— 此处刻意不做
/// JSON 合法性检查：provider 就是这么吐的）。
fn tool_call_chunk(index: u32, id: &str, arguments: &str) -> Value {
    json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": index,
                    "id": id,
                    "function": { "name": "write", "arguments": arguments }
                }]
            }
        }]
    })
}

/// 终止 chunk（只给 `finish_reason`，不带 `delta` —— 与真实端点一致）。
fn finish_chunk(reason: &str) -> Value {
    json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }] })
}

/// 残缺参数：`path` 完整，`content` 的字符串**没有闭合** —— 被掐断时的真实形态。
fn truncated_arguments(path: &Path) -> String {
    format!(
        "{{\"path\":{},\"content\":\"partial conte",
        serde_json::to_string(&path.to_string_lossy()).unwrap()
    )
}

/// 完整参数（合法 JSON）—— 同批的第二个调用。
fn complete_arguments(path: &Path) -> String {
    json!({
        "path": path.to_string_lossy(),
        "content": OVERWRITE_SENTINEL,
    })
    .to_string()
}

/// 轮 1：一批两个 `write`，响应整体被 `length` 截断。
fn truncated_round(partial: &Path, victim: &Path) -> String {
    let mut sse = String::new();
    // index 0：残缺 JSON（故意不闭合）。
    sse.push_str(&frame(&tool_call_chunk(
        0,
        "call_partial",
        &truncated_arguments(partial),
    )));
    // index 1：合法 JSON —— 见文件头「为什么这一批里必须有一个参数完整的调用」。
    sse.push_str(&frame(&tool_call_chunk(
        1,
        "call_victim",
        &complete_arguments(victim),
    )));
    // 整批的终止原因：截断。
    sse.push_str(&frame(&finish_chunk("length")));
    sse.push_str("data: [DONE]\n\n");
    sse
}

/// 轮 2：纯文本正常收尾（工具批被作废后循环会走到这里）。
fn recovered_round() -> String {
    let mut sse = String::new();
    sse.push_str(&frame(&json!({
        "choices": [{ "index": 0, "delta": { "content": "recovered" } }]
    })));
    sse.push_str(&frame(&finish_chunk("stop")));
    sse.push_str("data: [DONE]\n\n");
    sse
}

/// 轮 1（纯文本截断场景）：只吐半句文本，终止原因 `length`。
fn truncated_text_round() -> String {
    let mut sse = String::new();
    sse.push_str(&frame(&json!({
        "choices": [{ "index": 0, "delta": { "content": "first half" } }]
    })));
    sse.push_str(&frame(&finish_chunk("length")));
    sse.push_str("data: [DONE]\n\n");
    sse
}

// ---------------------------------------------------------------------------
// 回环 mock HTTP 服务（每个连接处理一个请求，按请求号取脚本）
// ---------------------------------------------------------------------------

/// 回环地址上的极简 HTTP/1.1 mock：**第 n 个请求回第 n 段脚本**。
///
/// 用 std 手写而非引入测试依赖（与 `stream_usage_fallback.rs` 同款）；
/// `Connection: close` 保证 reqwest 每次请求新开连接，从而两轮可被分别脚本化。
struct MockServer {
    port: u16,
    bodies: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(script: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let bodies_thread = bodies.clone();
        let stop_thread = stop.clone();

        let handle = std::thread::spawn(move || {
            let mut served = 0usize;
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // 超出脚本长度的请求回最后一段（正常路径不该发生）。
                        let body = script
                            .get(served)
                            .or_else(|| script.last())
                            .cloned()
                            .unwrap_or_default();
                        served += 1;
                        let _ = serve_once(stream, &body, &bodies_thread);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            port,
            bodies,
            stop,
            handle: Some(handle),
        }
    }

    /// api_base（code 会在其后追加 `/chat/completions`）。
    fn api_base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn request_count(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }

    /// 第 `n` 次（0 基）请求的原始 body —— 用来断言「回喂给模型的消息里确实带了
    /// 续写催促」。
    fn request_body(&self, n: usize) -> String {
        self.bodies.lock().unwrap()[n].clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 处理一个连接：读请求行/头/体，记录 body，回一段 SSE。
fn serve_once(
    mut stream: TcpStream,
    sse: &str,
    bodies: &Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    // 监听 socket 是非阻塞的（accept 轮询 stop 标志需要）。在 macOS/BSD 上
    // `accept()` 返回的连接**会继承**监听 socket 的 O_NONBLOCK，导致下面的
    // `set_read_timeout` 被忽略、`read` 在客户端字节到达前就返回 EAGAIN。
    // 显式复位为阻塞模式，让 `set_read_timeout` 真正生效。
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length = headers
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);
    bodies
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(&body).to_string());

    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        sse.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(sse.as_bytes())?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// `--json` 输出解析与子进程驱动
// ---------------------------------------------------------------------------

/// 取 `--json` 输出中**所有**同名事件的 payload（顺序即事件顺序）。
fn event_payloads(stdout: &str, name: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter_map(|line| {
            let v: Value = serde_json::from_str(line).ok()?;
            v.get("event")?.get(name).cloned()
        })
        .collect()
}

/// 以 env-var 配置（→ custom provider）启动二进制，跑 `--json <task>`。
///
/// env 全部经 `Command::env()` 注入**子进程**，不碰本进程环境。
fn run_json(api_base: &str, task: &str, home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ys-coding-agent"))
        .args(["--json", task])
        .env_remove("YUSHAN_LOG_DIR")
        .env("YUSHAN_DEBUG", "0")
        .env("YUSHAN_API_BASE", api_base)
        .env("YUSHAN_API_KEY", "test-key")
        .env("YUSHAN_MODEL", "test-model")
        // 隔离 HOME：绝不读写真实 `~/.yushan`（auth / state / 会话）。
        .env("HOME", home)
        // 回环地址需绕过代理；同时清掉可能存在的代理变量。
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .output()
        .expect("spawn ys-coding-agent")
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

/// **N11 —— 被截断的那一批工具调用一个都不执行**（端到端，经真实适配器 + 真实
/// CLI + 真实循环）。
///
/// 变异：把 `crates/ys-loop/src/basic.rs` 的
/// `if response.finish_reason.is_truncated() {` 改成 `if false {`（或整块删掉）
/// ⇒ `victim.txt` 被覆盖成 [`OVERWRITE_SENTINEL`] ⇒ 断言 1 变红。
#[test]
fn truncated_tool_batch_is_never_executed() {
    let work = unique_dir("work");
    let home = unique_dir("home");
    let victim = work.join("victim.txt");
    let partial = work.join("partial.txt");

    // 预先存在、装着用户内容的文件 —— 回归的哨兵。
    std::fs::write(&victim, USER_CONTENT).unwrap();

    let server = MockServer::start(vec![truncated_round(&partial, &victim), recovered_round()]);

    let out = run_json(&server.api_base(), "write the file", &home);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // ── 断言 1（回归核心）：整批工具调用**一个都没执行** ──────────────────
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        USER_CONTENT,
        "被截断的那一批工具调用一个都不许执行：残缺的 write 会覆盖用户的文件！\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !partial.exists(),
        "残缺参数那次调用不得创建任何文件：{} 竟然存在\nstdout:\n{stdout}",
        partial.display()
    );

    // ── 前提：残缺 JSON 确实到了适配器（否则本用例没在测它以为在测的东西）──
    // 适配器把非法参数降级为 `Value::Null` 时把警告写进日志；它是「index 0
    // 的参数真的是残缺 JSON」的直接证据，同时不得污染 stderr/TUI。
    let log =
        std::fs::read_to_string(home.join(".yushan/logs/yushan.log")).expect("应写入 yushan.log");
    assert!(
        log.contains("not valid JSON"),
        "前提不成立：适配器日志未报「参数非法 JSON」，说明残缺调用没被构造出来\nlog:\n{log}"
    );
    assert!(
        !stderr.contains("not valid JSON"),
        "参数修复警告不得直接写 stderr\nstderr:\n{stderr}"
    );

    // ── 断言 2：广播了 ResponseTruncated，且计数覆盖整批 ──────────────────
    let truncated = event_payloads(&stdout, "ResponseTruncated");
    assert_eq!(
        truncated.len(),
        1,
        "应恰有一条 ResponseTruncated\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let dropped = truncated[0]["dropped_tool_calls"]
        .as_u64()
        .unwrap_or_else(|| panic!("dropped_tool_calls 缺失：{}", truncated[0]));
    assert!(
        dropped >= 1,
        "dropped_tool_calls 应 >= 1，实得 {dropped}\nstdout:\n{stdout}"
    );
    assert_eq!(
        dropped, 2,
        "整批（2 个调用）都该被作废，而不是只丢残缺的那个\nstdout:\n{stdout}"
    );

    // ── 断言 3：终局事件仍是 RunFinished（不是 RunFailed），进程正常退出 ──
    assert!(
        out.status.success(),
        "截断不是失败：进程应正常退出\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        event_payloads(&stdout, "RunFailed").is_empty(),
        "截断不得以 RunFailed 收场\nstdout:\n{stdout}"
    );
    let finished = event_payloads(&stdout, "RunFinished");
    assert_eq!(
        finished.len(),
        1,
        "应恰有一条 RunFinished（消费循环没死锁、没把截断当失败）\nstdout:\n{stdout}"
    );

    // ── 断言 4：工具批作废后循环**继续**到第 2 轮，最终 Completed ──────────
    // 第 2 轮由 mock 脚本正常收尾（`finish_reason: "stop"` + 一段文本），
    // 故终局是 Completed —— 而不是 Truncated。
    assert_eq!(
        finished[0]["stop_reason"], "Completed",
        "工具批作废后循环应继续到第 2 轮并正常收尾\nstdout:\n{stdout}"
    );
    assert!(
        event_payloads(&stdout, "ModelTextDelta")
            .iter()
            .any(|d| d["text"] == "recovered"),
        "第 2 轮的文本应出现在事件流里（证明循环真的走到了下一轮）\nstdout:\n{stdout}"
    );
    assert_eq!(
        server.request_count(),
        2,
        "应恰好两次模型调用：轮 1（截断）+ 轮 2（收尾）"
    );

    // ── 断言 5：回喂给模型的是 is_error 的 ToolResult，文案含 truncated ────
    let error_results: Vec<Value> = event_payloads(&stdout, "ToolResult")
        .into_iter()
        .filter(|p| p["result"]["is_error"] == Value::Bool(true))
        .collect();
    assert!(
        !error_results.is_empty(),
        "整批必须合成 is_error 结果回喂（ADR-0001 配对完整性）\nstdout:\n{stdout}"
    );
    assert!(
        error_results.iter().all(|p| {
            let content = p["result"]["content"].as_str().unwrap_or_default();
            content.to_lowercase().contains("truncated")
        }),
        "被作废的工具结果应说明是截断所致，实得：{error_results:?}"
    );

    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&home);
}

/// **纯文本截断 ⇒ 续写一次并跑完**（端到端，经真实适配器 + 真实 CLI + 真实循环）。
///
/// 复现用户报告的故障：第 1 轮模型吐半句文本即触顶（`finish_reason: "length"`、
/// 无工具调用）—— 旧行为在此**直接终结回合**、任务烂尾。修法（ADR-0035）是回喂一条
/// 续写催促并 `continue`，让模型接着把任务做完。
///
/// 变异：把 `crates/ys-loop/src/basic.rs` Step 8b 的空 `tool_calls` 分支改回
/// **无条件** `return StopReason::Truncated` ⇒ 只发生 1 次模型调用、终局是
/// `Truncated` ⇒ 断言 2/3/4 全部变红。
#[test]
fn truncated_text_turn_continues_to_completion() {
    let work = unique_dir("work_text");
    let home = unique_dir("home_text");

    let server = MockServer::start(vec![truncated_text_round(), recovered_round()]);

    let out = run_json(&server.api_base(), "answer the question", &home);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // ── 断言 1：截断不是失败，进程正常退出，且广播了 dropped == 0 的 ResponseTruncated ──
    assert!(
        out.status.success(),
        "截断不是失败：进程应正常退出\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let truncated = event_payloads(&stdout, "ResponseTruncated");
    assert_eq!(
        truncated.len(),
        1,
        "纯文本截断应恰有一条 ResponseTruncated\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        truncated[0]["dropped_tool_calls"], 0,
        "纯文本截断的 dropped_tool_calls 必须是 0（没有工具批）\nstdout:\n{stdout}"
    );
    assert!(
        event_payloads(&stdout, "RunFailed").is_empty(),
        "截断不得以 RunFailed 收场\nstdout:\n{stdout}"
    );

    // ── 断言 2：循环**继续**到第 2 轮 —— 恰两次模型调用，两段文本都出现 ─────────
    assert_eq!(
        server.request_count(),
        2,
        "截断后应回喂续写提示并再调一次模型：轮 1（截断）+ 轮 2（收尾）\nstdout:\n{stdout}"
    );
    let deltas = event_payloads(&stdout, "ModelTextDelta");
    assert!(
        deltas.iter().any(|d| d["text"] == "first half"),
        "第 1 轮被截断的半句应出现在事件流\nstdout:\n{stdout}"
    );
    assert!(
        deltas.iter().any(|d| d["text"] == "recovered"),
        "第 2 轮的文本应出现（证明循环真的走到了下一轮）\nstdout:\n{stdout}"
    );

    // ── 断言 3：终局是 Completed（不是 Truncated）──────────────────────────
    let finished = event_payloads(&stdout, "RunFinished");
    assert_eq!(
        finished.len(),
        1,
        "应恰有一条 RunFinished\nstdout:\n{stdout}"
    );
    assert_eq!(
        finished[0]["stop_reason"], "Completed",
        "续写完成后应正常收尾，而不是 Truncated\nstdout:\n{stdout}"
    );

    // ── 断言 4：第 2 次请求确实带上了续写催促（回喂生效，不是靠别的路径巧合续跑）──
    let second_body = server.request_body(1);
    assert!(
        second_body.contains("cut off by the output token limit"),
        "第 2 次请求应含续写催促文案（证明 TRUNCATED_CONTINUE_PROMPT 回喂给了模型）\n\
         第二次请求 body:\n{second_body}"
    );

    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&home);
}
