//! S2 / S2b / S3 —— SSRF 守卫表 + `fetch_capped` 四态（超时 / 中止 / 重定向 / 状态码）
//! 与两条有界读取契约（`Content-Length` 不当分配指令、越界即停不排空）。
//!
//! # 本文件同时是 `tests/common/mod.rs` 的**接线点**
//!
//! 在它出现之前，`tests/` 下没有任何顶层文件 `mod common;` —— 那份假服务器
//! **根本不参与编译**（`cargo test -p ys-tools-web --no-run` 只产出一个
//! `unittests src/lib.rs`），改 `MockReply` 字段、升级 reqwest 都不会变红。
//! 判据：本文件存在后应出现**第二个**测试二进制（`tests/network.rs`）。
//!
//! # 副作用
//!
//! 本文件**不碰文件系统**：不建临时目录、不写 HOME / TMPDIR（`fetch_capped` 只收发
//! 字节；假服务器只 bind 回环的临时端口）。故没有「临时目录名带 pid + 序号」的需要。
//!
//! # 两个 client 的分工
//!
//! - 测 **redirect 策略**的用例用 `common::policy_client(allow_loopback)`：
//!   重定向策略**取自生产**（`ys_tools_web::redirect_policy`，与 `build_client` 同一份），
//!   但 client 上加了 `.no_proxy()`；**其余用例用 `common::no_proxy_client`**。
//! - 为什么 redirect 用例也必须绕代理：**策略与代理正交**。生产 client（`build_client`）
//!   **必须**尊重进程代理（企业环境靠它出网），而在**有系统 HTTP 代理的机器上**，
//!   打 `127.0.0.1` 假服务器的请求会被代理截走（拿到的是代理的 503 而非真重定向），
//!   这三条用例于是**必红** —— 与策略本身对不对无关。
//!   在测试里设 `NO_PROXY` env 是**掩盖**（且进程级 env 改写与并行测试数据竞争），
//!   故这里只关代理，策略一字不改。
//! - 这两类 client 都**不经** `build_client()`，因此本文件不依赖进程代理环境。

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{MockReply, MockServer, guarded, no_proxy_client, policy_client};
use ys_protocol::{Boundary, BoundarySource, QueueBoundarySource};
use ys_tools_web::{FetchError, MAX_REDIRECTS, Url, fetch_capped, vet_url, vet_url_with};

/// 抓取用的字节上限（足够装下本文件造的每一个响应体）。
const CAP: usize = 1 << 20;

/// 解析一个 URL 并跑**生产入口** [`vet_url`]。
fn vet(u: &str) -> Result<(), String> {
    let url = Url::parse(u).unwrap_or_else(|e| panic!("测试自身写错：`{u}` 不是合法 URL：{e}"));
    vet_url(&url)
}

/// 同上，但跑可配置版本。
fn vet_loopback_ok(u: &str) -> Result<(), String> {
    let url = Url::parse(u).unwrap_or_else(|e| panic!("测试自身写错：`{u}` 不是合法 URL：{e}"));
    vet_url_with(&url, true)
}

// ─────────────────────────── S2：SSRF 表 ───────────────────────────

/// 拒绝表：`(URL, 这一行钉住的判据)`。删掉 `guard.rs` 里对应的分支 → 该行变红。
const REJECTED: &[(&str, &str)] = &[
    // ① scheme 白名单
    ("file:///etc/passwd", "scheme 不在 {http,https}"),
    ("ftp://example.com/pub", "scheme 不在 {http,https}"),
    // ② 任何 userinfo（`http://trusted@169.254.169.254/` 一网打尽）
    ("http://user:pass@example.com/", "URL 内凭据"),
    (
        "http://trusted@169.254.169.254/",
        "URL 内凭据（人眼读到的是 `trusted`）",
    ),
    // ③ localhost / *.localhost / *.local / 无点单标签
    ("http://localhost/", "localhost"),
    ("http://sub.localhost/", "*.localhost"),
    ("http://x.local/", "*.local（mDNS / LAN 发现）"),
    ("http://internal-wiki/", "无点单标签（内网主机名）"),
    ("http://metadata/", "无点单标签"),
    // ④ IPv4 段表（逐条对应 vet_v4 的 match 臂）
    ("http://0.0.0.0/", "0.0.0.0/8"),
    ("http://10.0.0.1/", "10.0.0.0/8 私有"),
    ("http://100.64.0.1/", "100.64.0.0/10 CGNAT"),
    ("http://127.0.0.1/", "127.0.0.0/8 回环"),
    ("http://127.1.2.3/", "127.0.0.0/8 回环（非 .0.1 写法）"),
    (
        "http://169.254.169.254/",
        "169.254.0.0/16 链路本地 / 云元数据",
    ),
    ("http://172.16.0.1/", "172.16.0.0/12 私有"),
    ("http://192.0.0.1/", "192.0.0.0/24 IETF 协议分配"),
    ("http://192.168.1.1/", "192.168.0.0/16 私有"),
    ("http://198.18.0.1/", "198.18.0.0/15 基准测试"),
    ("http://224.0.0.1/", "224.0.0.0/4 组播"),
    ("http://240.0.0.1/", "240.0.0.0/4 保留"),
    // ⑤ IPv6 段判定
    ("http://[::1]/", "::1 回环"),
    ("http://[fe80::1]/", "fe80::/10 链路本地"),
    ("http://[fc00::1]/", "fc00::/7 唯一本地"),
    ("http://[ff02::1]/", "ff00::/8 组播"),
    // ⑥ ⭐ IPv4-mapped —— 递归按 v4 再判（T5 review 的变异靶子：删这段 → 本行 + 下一行变红）
    ("http://[::ffff:169.254.169.254]/", "::ffff: 映射到链路本地"),
    ("http://[::ffff:127.0.0.1]/", "::ffff: 映射到回环"),
    // ⑦ 免费的好处：`url` crate 的 WHATWG 主机解析把十进制 / 十六进制 IP 规范化
    ("http://2130706433/", "十进制 IP（已规范化为 127.0.0.1）"),
    ("http://0x7f.0.0.1/", "十六进制 IP（已规范化为 127.0.0.1）"),
];

/// 放行表 —— 守卫不得误伤正常公网 URL（否则工具直接不可用）。
const ALLOWED: &[&str] = &[
    "https://example.com/",
    "https://docs.rs/htmd/0.5.0/htmd/",
    "https://1.1.1.1/",
    "https://[2606:4700::1111]/",
    "http://example.com:8080/a/b?c=d#e",
];

/// 守卫**不抛异常、只回理由**；理由是非空英文短句（会直接进 `is_error` 结果，模型可见）。
#[test]
fn test_ssrf_table() {
    for (u, why) in REJECTED {
        match vet(u) {
            Ok(()) => panic!("`{u}` 必须被拒（{why}）—— 对应判据分支被删或失效"),
            Err(reason) => assert!(!reason.is_empty(), "`{u}` 的拒绝理由不得为空"),
        }
    }
    for u in ALLOWED {
        if let Err(reason) = vet(u) {
            panic!("`{u}` 必须放行（正常公网 URL）：{reason}");
        }
    }
}

/// ⭐ `::ffff:a.b.c.d`（IPv4-mapped）**递归按 v4 判**。
///
/// 单看 v6 判据（fc00::/7 / fe80::/10 …）会把这些全部放行 —— 而 TCP 层连的就是那个
/// v4 地址。本用例用**一对**期望把「递归到了 v4」钉死：
///
/// - `[::ffff:127.0.0.1]`：生产入口拒；`allow_loopback=true` 时**放行**
///   → 说明确实走进了 v4 的回环分支（v6 段判定里没有「映射到回环」这回事）；
/// - `[::ffff:169.254.169.254]`：**两种模式下都拒** → 说明递归的是完整 v4 段表，
///   不是「只要是映射地址就放/就拒」。
///
/// 变异：删掉 `vet_v6` 里的 `to_ipv4()` 递归 → 上面两条全绿变红。
#[test]
fn test_ipv4_mapped_is_judged_as_v4_recursively() {
    let loopback_mapped = "http://[::ffff:127.0.0.1]/";
    assert!(
        vet(loopback_mapped).is_err(),
        "生产入口不得放行 `::ffff:127.0.0.1`（TCP 层就是 127.0.0.1）"
    );
    assert!(
        vet_loopback_ok(loopback_mapped).is_ok(),
        "`allow_loopback` 下 `::ffff:127.0.0.1` 应放行 —— 否则说明没按 v4 递归判"
    );

    let metadata_mapped = "http://[::ffff:169.254.169.254]/";
    let reason = vet(metadata_mapped).expect_err("映射到链路本地必须拒");
    assert!(
        reason.contains("IPv4-mapped") && reason.contains("169.254"),
        "理由要点明映射关系与 v4 段名：{reason}"
    );
    assert!(
        vet_loopback_ok(metadata_mapped).is_err(),
        "`allow_loopback` 只放松回环，不得放松链路本地"
    );
}

// ─────────────────── S2b：守卫不误伤自建后端 ───────────────────

/// `allow_loopback` **只**放松回环，且**只**能由构造参数打开（没有任何 env 能开它）。
///
/// 完整形态（`websearch` 在 `YUSHAN_SEARCH_SEARXNG_URL=http://127.0.0.1:PORT` 下
/// 正常工作）要等工具本身落地 → 归 T12。这里只钉「机制」：
///
/// 1. `vet_url`（生产入口）**恒拒**回环 —— 自建后端能工作靠的是**绕开**它（配置地址
///    不经守卫），不是靠它放行；
/// 2. 回环的三种写法在 `allow_loopback=true` 下都放行；
/// 3. 私网 / 链路本地 / `.local` / 单标签在 `allow_loopback=true` 下**照拒**；
/// 4. 源码级断言：`guard.rs` / `net.rs` 里没有任何**运行时 env 读取** ——
///    「这道闸不可由用户配置绕过」于是是**类型上**的事实，而不是一句约定。
#[test]
fn test_allow_loopback_is_a_constructor_param_only() {
    // 1 + 2：回环三种写法
    for u in [
        "http://127.0.0.1:8888/",
        "http://[::1]:8888/",
        "http://localhost:8888/",
        "http://sub.localhost:8888/",
    ] {
        assert!(vet(u).is_err(), "生产入口必须拒回环：{u}");
        assert!(
            vet_loopback_ok(u).is_ok(),
            "自建后端形态（回环）应能放行：{u}"
        );
    }

    // 3：放松面**仅**回环
    for u in [
        "http://10.0.0.1/",
        "http://192.168.1.1/",
        "http://169.254.169.254/",
        "http://100.64.0.1/",
        "http://[fe80::1]/",
        "http://[fc00::1]/",
        "http://internal-wiki/",
        "http://x.local/",
        "file:///etc/passwd",
    ] {
        assert!(
            vet_loopback_ok(u).is_err(),
            "`allow_loopback` 不得放松非回环判据：{u}"
        );
    }

    // 4：源码里没有运行时 env 读取（`env!("CARGO_PKG_VERSION")` 是编译期宏，不匹配）。
    for (name, src) in [
        ("guard.rs", include_str!("../src/guard.rs")),
        ("net.rs", include_str!("../src/net.rs")),
    ] {
        assert!(
            !src.contains("std::env::"),
            "{name} 里出现了 `std::env::` —— 守卫的开关必须只有构造参数（env 可被配置改变）"
        );
        assert!(
            !src.contains("env::var"),
            "{name} 里出现了 `env::var` —— 同上"
        );
    }
}

// ───────────────────── S3：`fetch_capped` 四态 ─────────────────────

/// ① `TimedOut`：对端挂起 30s，预算 300ms。
///
/// 变异：把 `fetch_capped` 的 `timeout(budget, …)` 去掉 → 本用例撞 `guarded` 的
/// 20s 护栏 → **panic（红）而不是挂死**（这正是 `guarded` 存在的理由）。
#[tokio::test]
async fn test_fetch_capped_times_out_on_stalled_server() {
    let server = MockServer::start(vec![
        MockReply::html("late").with_delay(Duration::from_secs(30)),
    ]);
    let client = no_proxy_client();
    let budget = Duration::from_millis(300);

    let started = Instant::now();
    let r = guarded(
        "timeout",
        fetch_capped(client.get(server.url("slow")), CAP, budget, None),
    )
    .await;
    let elapsed = started.elapsed();

    assert_eq!(r.unwrap_err(), FetchError::TimedOut);
    assert!(
        elapsed >= Duration::from_millis(250),
        "不得早于预算收场：{elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "预算 300ms，实际 {elapsed:?} —— 预算没生效"
    );
}

/// ② `Aborted`：**对端一个字节都不发**（delay 30s），中止必须独立于「数据到达」收场。
///
/// 两种投递时机都要成立：
/// - **调用前**已置位 → `select!` 的 `biased` 第一分支立刻命中；
/// - **飞行中**投递 → 200ms 后的下一次 `is_aborted()` 轮询命中。
///
/// 变异：删掉 `fetch_capped` 的 `wait_aborted` 分支 → 两者都会撞满 10s 预算 →
/// 得 `TimedOut` → 红（本用例不依赖 `guarded`，但预算 < 护栏，故不会挂死）。
#[tokio::test]
async fn test_fetch_capped_aborted_while_server_sends_nothing() {
    // (a) 调用前已置位
    {
        let server = MockServer::start(vec![
            MockReply::html("never").with_delay(Duration::from_secs(30)),
        ]);
        let boundary = QueueBoundarySource::new();
        boundary.push(Boundary::Abort);
        let src: &dyn BoundarySource = &boundary;

        let started = Instant::now();
        let r = guarded(
            "abort pre-set",
            fetch_capped(
                no_proxy_client().get(server.url("hang")),
                CAP,
                Duration::from_secs(10),
                Some(src),
            ),
        )
        .await;

        assert_eq!(r.unwrap_err(), FetchError::Aborted);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "预置中止必须立刻收场，实际 {:?}",
            started.elapsed()
        );
    }

    // (b) 飞行中投递
    {
        let server = MockServer::start(vec![
            MockReply::html("never").with_delay(Duration::from_secs(30)),
        ]);
        let boundary = Arc::new(QueueBoundarySource::new());
        let producer = Arc::clone(&boundary);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            producer.push(Boundary::Abort);
        });
        let src: &dyn BoundarySource = &*boundary;

        let started = Instant::now();
        let r = guarded(
            "abort mid-flight",
            fetch_capped(
                no_proxy_client().get(server.url("hang")),
                CAP,
                Duration::from_secs(10),
                Some(src),
            ),
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(r.unwrap_err(), FetchError::Aborted);
        assert!(
            elapsed >= Duration::from_millis(150),
            "中止要等 200ms 的投递，实际 {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "轮询间隔 100ms，应在投递后的下一轮收场，实际 {elapsed:?}"
        );
        assert_eq!(server.request_count(), 1, "请求已发出，不是没连上");
    }
}

/// ③ `Status`：非 2xx **不读体**，带着状态码与**最终 URL** 返回（搜索链要按它列理由）。
#[tokio::test]
async fn test_non_2xx_is_status_error_with_final_url() {
    let server = MockServer::start(vec![MockReply::new(404, "text/html", "<h1>nope</h1>")]);
    let r = guarded(
        "status",
        fetch_capped(
            no_proxy_client().get(server.url("missing")),
            CAP,
            Duration::from_secs(5),
            None,
        ),
    )
    .await;

    match r {
        Err(FetchError::Status { status, url }) => {
            assert_eq!(status, 404);
            assert_eq!(url, server.url("missing"), "须带最终 URL");
        }
        other => panic!("404 应是 `Status`，实得 {other:?}"),
    }
}

/// ④ `Redirect`：**每一跳都重跑 SSRF 守卫**（生产策略）。
///
/// 302 → `/ok`（同服务器的回环 URL）在严格策略下必须**被拒**，且**不得真的发出**
/// 第二跳；换成 `169.254.169.254`（重定向才是云元数据的主要绕过路径）同样被拒。
///
/// client 用 [`policy_client`]：redirect 策略**就是生产那份**
/// （`build_client` 内部同样调 `redirect_policy(false)`），只有代理被关掉 ——
/// 否则本机有系统 HTTP 代理时，请求会被代理截走（拿到 503），本用例与策略对不对无关地变红。
///
/// 变异：把 `redirect_policy` 里的 `vet_url_with` 判据去掉 → 本用例红
/// （回环跳会跟随成功 → `Ok` 而非 `Redirect`）。
#[tokio::test]
async fn test_redirect_target_is_vetted_every_hop() {
    let client = policy_client(false);

    // (a) 302 → 回环
    let loop_hop = MockServer::start(vec![
        MockReply::redirect(302, "/ok"),
        MockReply::html("<h1>ok</h1>"),
    ]);
    let started = Instant::now();
    let r = guarded(
        "strict redirect to loopback",
        fetch_capped(
            client.get(loop_hop.url("start")),
            CAP,
            Duration::from_secs(5),
            None,
        ),
    )
    .await;

    match r {
        Err(FetchError::Redirect(why)) => assert!(
            why.contains("SSRF guard"),
            "理由须说明是守卫拦的（`Redirect` 的 Display 不印 source，得靠 err_text 拼）：{why}"
        ),
        other => panic!("302 → 回环必须被守卫拒，实得 {other:?}"),
    }
    assert_eq!(loop_hop.request_count(), 1, "被拒的跳不得真的发出去");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "守卫在策略里同步否决，不该等网络"
    );

    // (b) 302 → 云元数据（同一守卫的另一条判决路径）
    let meta = MockServer::start(vec![MockReply::redirect(
        302,
        "http://169.254.169.254/latest/meta-data/",
    )]);
    let r = guarded(
        "strict redirect to metadata",
        fetch_capped(
            client.get(meta.url("go")),
            CAP,
            Duration::from_secs(2),
            None,
        ),
    )
    .await;
    assert!(
        matches!(r, Err(FetchError::Redirect(_))),
        "302 → 169.254.169.254 必须被守卫拒，实得 {r:?}"
    );
    assert_eq!(meta.request_count(), 1);
}

/// 上一条的**反证**：换宽松策略（`policy_client(true)` —— 即 `allow_loopback=true` 的
/// **同一份**生产策略）打**同一形状**的回环跳，必须能跟到底 —— 于是「上一用例是**守卫**
/// 拦的，不是别的原因（超时 / 连接失败 / 策略不支持相对 Location）」才是被证明的，
/// 而不是被假设的。
#[tokio::test]
async fn test_allow_loopback_client_follows_loopback_redirect() {
    let lenient = policy_client(true);

    let server = MockServer::start(vec![
        MockReply::redirect(302, "/ok"),
        MockReply::html("followed"),
    ]);
    let f = guarded(
        "lenient follow",
        fetch_capped(
            lenient.get(server.url("start")),
            CAP,
            Duration::from_secs(5),
            None,
        ),
    )
    .await
    .expect("`allow_loopback=true` 应能跟随回环跳");

    assert_eq!(
        f.final_url,
        server.url("ok"),
        "`final_url` 是重定向后的 URL"
    );
    assert_eq!(f.body, b"followed".to_vec());
    assert_eq!(server.request_count(), 2, "初始 + 一跳");

    // 但 `allow_loopback` **不等于**「不管了」：链路本地同样被拒（无第二跳）。
    let meta = MockServer::start(vec![MockReply::redirect(302, "http://169.254.169.254/x")]);
    let r = guarded(
        "lenient metadata",
        fetch_capped(
            lenient.get(meta.url("go")),
            CAP,
            Duration::from_secs(2),
            None,
        ),
    )
    .await;
    assert!(
        matches!(r, Err(FetchError::Redirect(_))),
        "`allow_loopback` 不得放松链路本地，实得 {r:?}"
    );
    assert_eq!(meta.request_count(), 1);
}

/// 跳数上限：`/loop` 自重定向（相对 `Location` 解析回自身）→ 必须收敛在
/// `1 + MAX_REDIRECTS` 次请求上，报 `Redirect` 而不是无限打下去。
///
/// `previous()` 的口径含**初始请求**，故策略判的是 `> MAX_REDIRECTS`
/// （不是 `>=`）—— 本用例把这个 off-by-one 钉死。
///
/// client 是 `policy_client(true)`：策略取自生产（跳数上限就在策略里），只关代理。
#[tokio::test]
async fn test_redirect_hops_are_bounded() {
    let lenient = policy_client(true);
    let server = MockServer::start(vec![MockReply::redirect(302, "/loop")]);

    let r = guarded(
        "redirect loop",
        fetch_capped(
            lenient.get(server.url("loop")),
            CAP,
            Duration::from_secs(10),
            None,
        ),
    )
    .await;

    match r {
        Err(FetchError::Redirect(why)) => {
            assert!(
                why.contains("too many redirects"),
                "理由须说明是跳数：{why}"
            );
        }
        other => panic!("自重定向必须收敛为 `Redirect`，实得 {other:?}"),
    }
    assert_eq!(
        server.request_count(),
        1 + MAX_REDIRECTS,
        "初始请求 + 最多 {MAX_REDIRECTS} 跳"
    );
}

// ───────────── 有界读取的两条契约（设计 §1.3 ④ 的重点） ─────────────

/// `Content-Length` 是**声明**，不是分配指令。
///
/// 服务端谎报一个巨大的长度、只发几个字节然后挂住（`hold_after`）。正确实现把它
/// 夹在 `cap` / `PREALLOC_MAX` 之下 → 读到挂起 → 撞预算 `TimedOut`。
///
/// 变异：把预分配改成 `Vec::with_capacity(len)` → `i64::MAX` 那行会
/// `capacity overflow` **panic**（超过 `isize::MAX`，必然炸；而 4 GiB 那行在 macOS
/// 上可能只是惰性映射、并不炸，故**真正抓变异的是 `i64::MAX` 那行**）→ 本用例红。
///
/// ⚠️ 上限不是随便取的：hyper 自己把 `Content-Length > u64::MAX - 2` 判为
/// `Parse::TooLarge`（`body/length.rs` 的 `MAX_LEN`），`u64::MAX` 根本到不了本 crate；
/// `i64::MAX` 既在 hyper 的上限之内，又远超任何合理的分配企图。
#[tokio::test]
async fn test_huge_content_length_is_not_an_allocation_instruction() {
    for declared in [i64::MAX as u64, 4 * 1024 * 1024 * 1024] {
        let server = MockServer::start(vec![
            MockReply::text("tiny")
                .with_declared_length(declared)
                .with_hold_after(Duration::from_secs(30)),
        ]);
        let r = guarded(
            "huge content-length",
            fetch_capped(
                no_proxy_client().get(server.url("huge")),
                CAP,
                Duration::from_millis(400),
                None,
            ),
        )
        .await;

        // 不 panic / 不 OOM / 不 abort：按声明分配会在这里炸掉整个测试进程。
        assert_eq!(
            r.unwrap_err(),
            FetchError::TimedOut,
            "声明 Content-Length={declared}：应按预算收场（体只有 4 字节，等到挂起）"
        );
    }
}

/// **越界即停、不排空**：`cap` 小于响应体时立刻返回，**不把剩下的读完**。
///
/// 服务端声明一个巨大的长度、先发 `cap + 100` 字节、然后**停住不发也不关**
/// （`hold_after` 30s），预算 3s。不排空的实现读到越界那一 chunk 就 `break` →
/// 毫秒级 `Ok(capped=true)`；**排空的实现**会等剩下那 30s → 撞预算 `TimedOut` → 红。
///
/// ⚠️ 与 `exec::run_bounded`（子进程必须排空，否则管道写满死锁）**刻意相反** ——
/// HTTP 对端不会因不被读而卡死。
#[tokio::test]
async fn test_cap_stops_reading_without_draining() {
    let cap = 1024usize;
    let server = MockServer::start(vec![
        MockReply::text(vec![b'a'; cap + 100])
            .with_declared_length(4 * 1024 * 1024 * 1024)
            .with_hold_after(Duration::from_secs(30)),
    ]);

    let started = Instant::now();
    let f = guarded(
        "cap without drain",
        fetch_capped(
            no_proxy_client().get(server.url("big")),
            cap,
            Duration::from_secs(3),
            None,
        ),
    )
    .await
    .expect("越界即停 → 必须在预算内返回 Ok");
    let elapsed = started.elapsed();

    assert!(f.capped, "体 > cap，必须标记 capped");
    assert_eq!(f.body.len(), cap, "恰好留下前 cap 字节");
    assert!(f.body.iter().all(|&b| b == b'a'));
    assert!(
        elapsed < Duration::from_secs(1),
        "越界即停必须立刻收场，实际 {elapsed:?} —— 排空会撞满 3s 预算"
    );
}

/// `cap` 边界：`chunk.len() > room` 是**严格大于** ——
/// **恰好 cap 字节** = 读完了（`capped == false`），**cap + 1 字节**才是被截断
/// （`capped == true`）。把 `>` 改成 `>=` → 前者变红。
#[tokio::test]
async fn test_cap_boundary_exact_cap_is_not_truncation() {
    for (len, expect_capped) in [(CAP_BOUNDARY, false), (CAP_BOUNDARY + 1, true)] {
        let server = MockServer::start(vec![MockReply::text(vec![b'x'; len])]);
        let f = guarded(
            "cap boundary",
            fetch_capped(
                no_proxy_client().get(server.url("body")),
                CAP_BOUNDARY,
                Duration::from_secs(10),
                None,
            ),
        )
        .await
        .unwrap_or_else(|e| panic!("{len} 字节的响应应能读完：{e:?}"));

        assert_eq!(f.body.len(), CAP_BOUNDARY, "留下的永远是前 cap 字节");
        assert_eq!(
            f.capped, expect_capped,
            "体 {len} 字节 / cap {CAP_BOUNDARY}：capped 应为 {expect_capped}"
        );
    }
}

/// 边界用例专用的 cap（故意小到让「恰好 / 多 1 字节」都是真实存在的两种响应）。
const CAP_BOUNDARY: usize = 4096;
