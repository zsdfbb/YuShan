//! SSRF 守卫 —— **纯函数、零 IO**（设计 `docs/arch/tools-web/design.md` §1.4）。
//!
//! 唯一入口 [`vet_url`]：判一个 URL 是否允许被抓取。`Err` 的文案**直接进
//! `is_error` 工具结果**（模型可见），故一律用英文短句 + 说明理由。
//!
//! # ⚠️ 硬规则：本守卫**只作用于模型给的 URL**
//!
//! **绝不作用于配置里的后端地址** —— 自建 SearXNG 常在 `http://127.0.0.1:8888`，
//! 用户配的 provider 端点同理。对配置地址跑本守卫会把最常见的自建部署直接判死。
//! 落地方式：`websearch` 的链上请求**不经** `vet_url`；只有 `webfetch` 的模型入参
//! （以及 `build_client` 的重定向策略，见 `net.rs`）经它。
//!
//! # 判据（逐条）
//!
//! | # | 拒绝 | 为什么 |
//! |---|---|---|
//! | 1 | scheme 不在 `{http, https}` | `file:` / `data:` / `ftp:` 都不该由模型驱动 |
//! | 2 | **任何 userinfo**（`user@host`） | `http://trusted@169.254.169.254/` 一网打尽 |
//! | 3 | `localhost` / `*.localhost` / `*.local` / **无点单标签** | 闭掉 `http://internal-wiki/` |
//! | 4 | 私有 / 保留 / 回环 / 链路本地 IP 字面量 | 见 [`vet_v4`] / [`vet_v6`] |
//! | 5 | `::ffff:a.b.c.d`（IPv4-mapped）**递归按 v4 再判** | 最易漏的真实绕过点 |
//!
//! # 诚实边界（必须写清楚，否则会被当成安全边界用）
//!
//! - **只判字面量，不解析 DNS** → `http://evil.example/`（解析到 `127.0.0.1`）、
//!   `http://127.0.0.1.nip.io/` 都**挡不住**（DNS rebinding）。
//! - 它是**输入校验**，不是安全边界。真隔离由沙箱 / 网络策略那一层解决（见 `CLAUDE.md`
//!   「不做安全」）。
//! - 但有一条**免费的好处**：`url` crate 对 http/https 走 WHATWG 主机解析，故
//!   `http://2130706433/` / `http://0x7f.0.0.1/` 这类「十进制 / 十六进制 IP」在
//!   `host_str()` 里**已被规范化成 `127.0.0.1`**，无需我们额外处理。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use reqwest::Url;

/// 判定一个**模型给的** URL 是否允许出网抓取。生产路径：**不放行任何回环地址**。
///
/// `Ok(())` = 允许；`Err(理由)` = 拒绝（理由可直接展示给模型）。
/// 详见模块文档的判据表与诚实边界。
pub fn vet_url(u: &Url) -> Result<(), String> {
    vet_url_with(u, false)
}

/// [`vet_url`] 的可配置版本 —— `allow_loopback` **只**放行回环（`127.0.0.0/8` / `::1` /
/// `localhost`），**不放行**私有段、链路本地（`169.254/16`）、`0/8` 等任何其它判据。
///
/// 存在的唯一理由是**测试**：`webfetch` 的端到端测试要打本机假服务器，而假服务器
/// 只能在回环上。生产路径一律用 [`vet_url`]（= `allow_loopback = false`）。
///
/// 这是**构造参数而非 env** 的原因见设计 §1.4：环境变量可被配置改变，而构造参数
/// 只有代码能传 —— 「这道闸不可由用户配置绕过」于是成为**类型上**的事实。
pub fn vet_url_with(u: &Url, allow_loopback: bool) -> Result<(), String> {
    match u.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "blocked: scheme `{other}` is not allowed (only http/https)"
            ));
        }
    }

    // 任何 userinfo 都拒：`http://trusted@169.254.169.254/` 的 host 是后者，
    // 而人眼读到的常常是前者。不去「看它像不像内网」—— 直接全拒，零解释空间。
    if !u.username().is_empty() || u.password().is_some() {
        return Err(
            "blocked: URL contains userinfo (`user@host`) — the text before `@` is not the host"
                .to_string(),
        );
    }

    let Some(host) = u.host_str() else {
        return Err("blocked: URL has no host".to_string());
    };

    // IPv6 字面量在 `url` 的 `host_str()` 里带方括号（`[::1]`）→ 剥掉再解析。
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);

    if let Ok(ip) = bare.parse::<IpAddr>() {
        return vet_ip(ip, allow_loopback).map_err(|why| format!("blocked: {why}"));
    }

    vet_domain(host, allow_loopback).map_err(|why| format!("blocked: {why}"))
}

/// 主机名判据。**IP 字面量不走这里**（`vet_url_with` 已先行分派）。
fn vet_domain(host: &str, allow_loopback: bool) -> Result<(), String> {
    // 末尾的根点（`localhost.` = `localhost`）与大小写都要先归一 ——
    // 否则 `http://LOCALHOST./` 会从 `ends_with(".localhost")` 底下溜过去。
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty() {
        return Err("empty host".to_string());
    }

    let is_localhost = h == "localhost" || h.ends_with(".localhost");
    if is_localhost {
        return if allow_loopback {
            Ok(())
        } else {
            Err(format!("`{h}` is loopback"))
        };
    }
    if h.ends_with(".local") {
        return Err(format!(
            "`{h}` is in the `*.local` (mDNS / LAN discovery) space"
        ));
    }
    // 无点的单标签主机：`internal-wiki` / `router` / `metadata` 这类内网名字。
    // 公网域名**必然**含点（否则根域之外无法解析）。
    if !h.contains('.') {
        return Err(format!(
            "`{h}` is a single-label host (no dot) — intranet hostnames are not fetchable"
        ));
    }
    Ok(())
}

/// IP 字面量分派。
fn vet_ip(ip: IpAddr, allow_loopback: bool) -> Result<(), String> {
    match ip {
        IpAddr::V4(v4) => vet_v4(v4, allow_loopback),
        IpAddr::V6(v6) => vet_v6(v6, allow_loopback),
    }
}

/// IPv4 段表。判据与设计 §1.4 的行**逐条对应**（`Err` 里带上段名，便于模型自查）。
///
/// 明写区间而不用 `Ipv4Addr::is_private()`：后者的集合**小于**这里要拒的集合
/// （缺 `100.64/10`、`192.0.0/24`、`198.18/15`）。
fn vet_v4(ip: Ipv4Addr, allow_loopback: bool) -> Result<(), String> {
    if ip.is_loopback() && allow_loopback {
        return Ok(());
    }
    let blocked = match ip.octets() {
        [0, ..] => Some("0.0.0.0/8 (this network)"),
        [10, ..] => Some("10.0.0.0/8 (private)"),
        [100, 64..=127, ..] => Some("100.64.0.0/10 (CGNAT)"),
        [127, ..] => Some("127.0.0.0/8 (loopback)"),
        [169, 254, ..] => Some("169.254.0.0/16 (link-local / cloud metadata)"),
        [172, 16..=31, ..] => Some("172.16.0.0/12 (private)"),
        [192, 0, 0, ..] => Some("192.0.0.0/24 (IETF protocol assignments)"),
        [192, 168, ..] => Some("192.168.0.0/16 (private)"),
        [198, 18..=19, ..] => Some("198.18.0.0/15 (benchmarking)"),
        [224..=239, ..] => Some("224.0.0.0/4 (multicast)"),
        [240..=255, ..] => Some("240.0.0.0/4 (reserved / broadcast)"),
        _ => None,
    };
    match blocked {
        Some(range) => Err(format!("`{ip}` is in {range}")),
        None => Ok(()),
    }
}

/// IPv6 判据。**顺序是关键**，见下面 `to_ipv4()` 那段的注释。
fn vet_v6(ip: Ipv6Addr, allow_loopback: bool) -> Result<(), String> {
    if ip.is_unspecified() {
        return Err("`::` (unspecified address)".to_string());
    }
    if ip.is_loopback() {
        return if allow_loopback {
            Ok(())
        } else {
            Err("`::1` is loopback".to_string())
        };
    }

    // ⚠️ **最易漏的真实绕过点**：`::ffff:a.b.c.d`（IPv4-mapped）在 TCP 层就是那个
    // v4 地址 —— 只看 v6 判据（fc00::/7 等）会让 `[::ffff:169.254.169.254]` 直接放行。
    // `to_ipv4()` 同时覆盖 mapped（`::ffff:…`）与已废弃的 compatible（`::a.b.c.d`）两种
    // 写法，比 `to_ipv4_mapped()` **更宽** —— 这正是这里想要的方向。
    // 必须放在 fc00::/7 等判据**之前**，且必须在 `::` / `::1` 之后
    // （`::1` 的 segments 也满足 compatible 形状，先判它才有准确的文案）。
    if let Some(v4) = ip.to_ipv4() {
        return vet_v4(v4, allow_loopback)
            .map_err(|why| format!("`{ip}` is IPv4-mapped (`::ffff:…`) and maps to {why}"));
    }

    let seg = ip.segments();
    if seg[0] & 0xff00 == 0xff00 {
        return Err(format!("`{ip}` is in ff00::/8 (multicast)"));
    }
    if seg[0] & 0xfe00 == 0xfc00 {
        return Err(format!("`{ip}` is in fc00::/7 (unique local)"));
    }
    if seg[0] & 0xffc0 == 0xfe80 {
        return Err(format!("`{ip}` is in fe80::/10 (link-local)"));
    }
    Ok(())
}
