//! 落盘与清理（设计 §1.2 的「store 原子写」、§1.6 的「目录与清理」）。
//!
//! # 本模块**绝不碰 workspace**
//!
//! 每个函数都收一个**调用方给的目录**，只在那里读写：
//!
//! - [`write_fetched`] 只写 `<dir>/…`；
//! - [`sweep`] 只**非递归**地删 `<dir>` 下**名字符合本工具命名模式**的普通文件。
//!
//! 没有任何一处「默认目录」「当前目录」的推断 —— 相对路径会解析到 **CWD**，
//! 而 coding agent 的 CWD **就是用户仓库**（且 `.gitignore` 里有 `tmp/`，
//! 这种污染连 `git status` 都看不见）。目录的**解析**归 `lib.rs::resolve_fetched_dir`。
//!
//! # 清理只对 `Primary` 档生效 —— 但那由**调用方**保证
//!
//! [`sweep`] 收的是**显式目录**，它无从知道这个目录是哪一档。档位→可否清理的映射
//! 在类型上由 [`FetchedDir::is_cleanable`] 表达，由 app 层（T13 的 `main`）在调用前
//! 看过再调。本模块**不**重做这个判断（重做一次就是第二个可能漂移的真值）。
//!
//! [`FetchedDir::is_cleanable`]: crate::FetchedDir::is_cleanable
//!
//! # 命名：`<slug>-<16hex>.md`
//!
//! - `slug` = URL 路径的**末个非空片段**（`/a/b` → `b`、`/a/b/` → `b`；路径为空或
//!   只有 `/` 时退回 host），清洗到 `[a-z0-9-]`、截 40 字符 ——
//!   为的是「一眼能认出这是哪个页面」，而**不是**唯一性。
//! - `hash` = **手写 FNV-1a 64 位**的 16 位 hex，**对象是重定向后的最终 URL**
//!   （与文件里的 `Source:` 行、与工具回显的 URL 一致）→ 同一 URL 重复抓取
//!   **落到同一个文件**（覆盖写，而不是目录膨胀）。
//!
//! ⚠️ **为什么手写 FNV 而不是 `DefaultHasher`**：`std` 的 `DefaultHasher`
//! 输出**跨 Rust 版本不保证稳定**（实现允许变、也确实变过）—— 同一 URL 在升级
//! 工具链后会落到**另一个文件**，目录里堆满"同一篇文章的另一份"。`sha2` 能解决
//! 但那是新依赖。FNV-1a 只有 8 行、零依赖、**永久稳定**：换版本、换机器、
//! 换平台都是同一个数。
//!
//! # 原子写：唯一 tmp 名 + rename
//!
//! tmp 名带 **`<pid>` 与进程内原子序号**：多实例、并行测试同时抓同一个 URL 时，
//! 共用一个 tmp 名会**互相截断**（后写的把先写的截掉）或让 `rename` 读到半个文件。
//! 这是本项目的硬约定（与 `JsonlSession` 的原子写同款）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use reqwest::Url;

/// `slug` 的字符上限（截断后）。
const SLUG_MAX: usize = 40;

/// `hash` 的 hex 位数（FNV-1a 64 位 = 8 字节 = 16 个 hex 字符）。
const HASH_HEX_LEN: usize = 16;

/// 落盘文件扩展名（**含点**）。也是 [`sweep`] 的识别依据之一。
const SUFFIX: &str = ".md";

/// slug 洗不出任何字符时的兜底 —— 保证文件名与 [`sweep`] 的模式始终匹配。
const SLUG_FALLBACK: &str = "page";

/// 一次 [`sweep`] 的结果。
///
/// 只报「做了什么」，**不报错误** —— 清理是 best-effort（见 [`sweep`]）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepStats {
    /// 名字匹配本工具模式、且被看过 `mtime` 的普通文件数。
    pub scanned: usize,
    /// 实际删掉的文件数。
    pub removed: usize,
    /// 删掉文件的原大小之和（仅供提示文案参考）。
    pub bytes_freed: u64,
}

/// 把 `content` 落到 `<dir>/<slug>-<16hex>.md`，返回**最终路径**。
///
/// - `final_url` 必须是**重定向之后**的最终 URL（`net::Fetched::final_url`）——
///   hash 与 slug 都由它算，文件里的 `Source:` 行也用它，三者必须同源。
/// - **原子**：先写唯一 tmp，再 `rename` 到目标。`rename` 在同一文件系统上是原子的，
///   读者（模型的 `read`）永远看不到半个文件。
/// - **幂等**：同一 URL 重复抓取 → 同一个目标名 → `rename` **覆盖**旧文件
///   （Unix 语义）。这是刻意的：抓两次同一页应当只有一份文件。
/// - 目录不存在就建（[`std::fs::create_dir_all`]）：`Temp` 档**可能被 OS 清掉**，
///   而工具不该因为「系统清了临时目录」就永久失效。建的**只有 `dir` 本身**，
///   它的父目录不在管辖范围。
///
/// 失败返回 IO 错误**原样**（含是写失败还是 rename 失败）—— 由调用方决定
/// 渲染成什么（`fetch.rs` 会把「落盘失败」说成明确 notice，而不是假装成功）。
pub async fn write_fetched(dir: &Path, final_url: &str, content: &str) -> std::io::Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;

    let name = fetched_file_name(final_url);
    let target = dir.join(&name);
    let tmp = unique_tmp_path(dir, &name);

    if let Err(e) = tokio::fs::write(&tmp, content).await {
        // tmp 可能已经建出来了一半：best-effort 清掉，别给目录留垃圾。
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    if let Err(e) = tokio::fs::rename(&tmp, &target).await {
        // 同上：rename 失败时 tmp 还在（比如跨设备挂载），同样别留垃圾。
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }

    Ok(target)
}

/// 清理 `dir` 下**本工具命名模式**且 `mtime` 早于 `now - max_age` 的文件。
///
/// 判据三条**同时**满足才删：
///
/// 1. 名字匹配 `<slug>-<16hex>.md`（见 [`is_fetched_name`]）—— 用户放在同一目录里的
///    别的文件**绝不**会被误删；
/// 2. 是**普通文件**（`file_type()` 不 follow 符号链接 → 符号链接、目录都被跳过）；
/// 3. `mtime < now - max_age`（时钟倒流/未来时间戳自然不满足）。
///
/// **不递归**；**任何 IO 错误静默**（读目录、取元数据、删除失败一律跳过）——
/// 照 `apps/coding-agent/src/logging.rs` 的 best-effort 风格：清理是**附带动作**，
/// 它永远不该成为一次抓取失败的原因。
///
/// `now - max_age` 下溢（`max_age` 大到超出 `SystemTime` 的表示范围）时**一个都不删**，
/// 而不是把阈值当成 UNIX 纪元。
pub fn sweep(dir: &Path, max_age: Duration) -> SweepStats {
    let mut stats = SweepStats::default();

    let Some(cutoff) = SystemTime::now().checked_sub(max_age) else {
        return stats;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return stats;
    };

    for entry in entries.flatten() {
        // 取不到类型就跳过（单项的 IO 失败不该中断整个清扫）。
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // `file_type()` 不 follow 符号链接 → 这里同时排除了目录与符号链接。
        if !file_type.is_file() {
            continue;
        }

        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_fetched_name(name) {
            continue;
        }
        stats.scanned += 1;

        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        if mtime >= cutoff {
            continue;
        }

        if std::fs::remove_file(entry.path()).is_ok() {
            stats.removed += 1;
            stats.bytes_freed += meta.len();
        }
    }

    stats
}

/// `<slug>-<16hex>.md` —— 落盘文件名的**唯一定义**（[`write_fetched`] 与
/// [`is_fetched_name`] 必须同时改，否则清理会认不出自己写的文件）。
pub fn fetched_file_name(final_url: &str) -> String {
    format!(
        "{}-{:016x}{SUFFIX}",
        slug_of(final_url),
        fnv1a64(final_url.as_bytes())
    )
}

/// 从 URL 取「路径末个非空片段（无则 host）」并清洗成 `[a-z0-9-]`。
///
/// 尾斜杠/查询串/锚不影响结果（`/a/b/` 与 `/a/b?q=1` 都取 `b`）—— 只取路径片段、
/// 且在 `next_back()` 前滤掉空段（理由见函数内注释）。
///
/// 解析失败（理论上不会：`final_url` 来自 `reqwest::Url::to_string`）时退化成
/// 对整个字符串做清洗 —— **绝不 panic**，也绝不产出空 slug。
fn slug_of(url: &str) -> String {
    let raw = match Url::parse(url) {
        Ok(parsed) => {
            // 末段：路径里最后一个**非空**片段（`/a/b` → `b`、`/a/b/` → `b`）。
            // ⚠️ 空段必须在 `next_back()` **之前**滤掉：尾斜杠路径（`/a/b/`）的最后一个
            // 片段是**空串**，若先取末段再滤空，就会把它丢掉并**误退回 host**
            // （`https://example.com/a/b/` → `example-com`，可读性设计目的落空）。
            // 路径为空/只有 `/`（此时唯一片段也是空）→ 退回 host。
            let last = parsed
                .path_segments()
                .and_then(|segs| segs.rev().find(|s| !s.is_empty()))
                .map(str::to_owned);
            last.unwrap_or_else(|| parsed.host_str().unwrap_or_default().to_owned())
        }
        Err(_) => url.to_owned(),
    };

    let slug = sanitize_slug(&raw);
    if slug.is_empty() {
        SLUG_FALLBACK.to_owned()
    } else {
        slug
    }
}

/// 清洗到 `[a-z0-9-]`：其余字符**折叠成单个 `-`**，截 [`SLUG_MAX`]。
///
/// 折叠（而不是逐个丢弃）是为了让 `a.b` 与 `a-b` 都不粘连成 `ab`；
/// 连续的非字母数字只产出一个 `-`，否则 `a%20%20b` 会变成一长串连字符。
/// 首/尾的 `-` **天然不会出现** —— `-` 只在「已经攒到下一个字母数字」时才落，
/// 故不需要再 `trim` 一次（`sanitize_slug` 的返回值要么空、要么以字母数字收尾）。
fn sanitize_slug(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(SLUG_MAX));
    let mut pending_dash = false;

    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
            if out.len() >= SLUG_MAX {
                break;
            }
        } else {
            // 非字母数字（含非 ASCII、`.`、`%`、`_`…）：先记下，等下一个字母数字再落。
            pending_dash = true;
        }
    }

    out
}

/// FNV-1a 64 位。**手写**（不是 `DefaultHasher`）的理由见模块文档。
fn fnv1a64(bytes: &[u8]) -> u64 {
    /// FNV 规范的 offset basis（64 位）。
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    /// FNV 规范的 prime（64 位）。
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// 唯一 tmp 路径：`<name>.<pid>.<进程内原子序号>.tmp`（模块文档的「原子写」）。
fn unique_tmp_path(dir: &Path, name: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("{name}.{}.{seq}.tmp", std::process::id()))
}

/// 名字是否匹配本工具的 `<slug>-<16hex>.md`（[`sweep`] 的唯一识别依据）。
///
/// `-` 用 [`str::rsplit_once`] 从**右**切：slug 本身就可以含 `-`，只有最后一段是 hash。
fn is_fetched_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(SUFFIX) else {
        return false;
    };
    let Some((slug, hash)) = stem.rsplit_once('-') else {
        return false;
    };

    // hash：恰好 16 位、全小写 hex（大写是我们不会产出的形状）。
    if hash.len() != HASH_HEX_LEN
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return false;
    }

    // slug：非空、不超长、全部落在 [a-z0-9-]。
    !slug.is_empty()
        && slug.len() <= SLUG_MAX
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
