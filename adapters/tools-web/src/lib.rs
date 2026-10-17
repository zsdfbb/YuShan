//! `tools-web` —— **带网络**的工具适配器：`websearch`（多后端链式搜索）与
//! `webfetch`（抓取网页 → Markdown → 落盘 → 指针）。
//!
//! # 与 `tools-basic` 的分工
//!
//! | | `tools-basic` | 本 crate |
//! |---|---|---|
//! | 网络 | **无**（只有本机文件与子进程） | **有**（`reqwest`） |
//! | 失败面 | 本机文件系统 / 子进程 | 对端 HTTP、超时、TLS、SSRF |
//!
//! 二者**刻意分开**：`tools-basic` 保持无网络，是为了让"只要本机能力"的装配
//! （离线、审计严格、不信任出网的环境）能只依赖前者而**在类型上**拿不到网络。
//!
//! # `WebConfig` 是**注入值**，不是全局单例
//!
//! 先例 [`SearchBackends`]（`tools-basic`）：配置是**值**而不是 `OnceLock`/`static`
//! → 降级路径可以**确定性测试**（构造一个 `offline()` 或空后端的配置即可断言提示文案），
//! 不必改进程级 env、也不必赌初始化顺序。同理：
//!
//! - [`WebConfig::from_env`] —— 生产路径，读 env 一次并**冻结**（`backends` 启动期定死）
//! - [`WebConfig::with_client`] —— 测试注入（先例 `OpenAICompatibleModel::with_client`）
//! - [`WebConfig::offline`] —— HTTP 栈不可用；工具**仍注册**，降级发生在**调用期**
//!
//! # 两条硬约束
//!
//! 1. **本 crate 绝不 `print`**（与 `toolset.rs` 同款源码守卫）。诊断文案一律由
//!    [`WebConfig::degraded_hint`] **产出**，由 app 层决定去处（stderr / `Outbound`）。
//! 2. **任何落盘路径都必须是绝对路径**（见 [`FetchedDir`]）—— 相对路径解析到
//!    **CWD**，而 coding agent 的 CWD **就是用户仓库**。
//!
//! # 后续切片的落点
//!
//! 已落地：`guard.rs`（SSRF 纯函数）/ `net.rs`（`build_client` + `fetch_capped`）/
//! `convert.rs`（`htmd`，**整块输入、不流式**）/ `store.rs`（原子写 + 清理）/
//! `backends.rs`（`const TABLE` + `plan`/`parse` 两个纯函数，见 [`resolve_backends`]）/
//! `search.rs`（[`WebSearchTool`]：链驱动 + 预算切片）/ `fetch.rs`（[`WebFetchTool`]：
//! 抓 → 分派 → 转 → 落盘 → 头 + 指针）。
//!
//! > **UTF-8 边界（本项目已因此出过血）**：若将来把 `fetch_capped` 改成**流式**
//! > 解码，多字节字符会被 TCP 分片切开 → 必须**字节级缓冲、只在完整行/字符上解码**。
//! > 当前设计收完整体再交给 `htmd`（它要求整块输入），**天然规避**这条。
//!
//! [`SearchBackends`]: https://docs.rs/ys-tools-basic

mod backends;
mod convert;
mod fetch;
mod guard;
mod net;
mod search;
mod store;

use std::path::{Path, PathBuf};

use reqwest::Client;

pub use crate::backends::{
    MAX_LIMIT, MIN_LIMIT, SearchHit, kind_slug, parse, plan, resolve_backends,
};
pub use crate::convert::{SKIP_TAGS, html_to_markdown};
pub use crate::fetch::{
    ContentKind, FETCH_BUDGET, FETCH_MAX_BYTES, SUMMARIZE_BUDGET, WebFetchTool, classify_content,
    decode_body,
};
pub use crate::guard::{vet_url, vet_url_with};
pub use crate::net::{
    CONNECT_TIMEOUT, FetchError, Fetched, MAX_REDIRECTS, build_client, build_client_with,
    fetch_capped, redirect_policy,
};
pub use crate::search::{
    CHAIN_BUDGET, ChainOutcome, MIN_SLICE, SNIPPET_MAX_CHARS, WebSearchTool, render_chain_outcome,
    run_chain, slice_for,
};
pub use crate::store::{SweepStats, fetched_file_name, sweep, write_fetched};
pub use reqwest::Url;

/// SearXNG 实例地址（须开 `json` 格式；自建实例常在 `http://127.0.0.1:8888`）。
///
/// ⚠️ SSRF 校验**只作用于模型给的 URL，绝不作用于这里** —— 自建 SearXNG 就在回环上。
pub const ENV_SEARXNG_URL: &str = "YUSHAN_SEARCH_SEARXNG_URL";
/// Tavily API key。
pub const ENV_TAVILY_KEY: &str = "YUSHAN_SEARCH_TAVILY_KEY";
/// Tavily 的**生态通用** API key 环境变量名（官方 SDK 与多数 Tavily 工具用它）。
///
/// 作为 [`ENV_TAVILY_KEY`] 的**回退**读取：已为别的 Tavily 工具配过 key 的用户
/// 不必再复制一份到本项目命名空间。两者都设时 [`ENV_TAVILY_KEY`] 优先。
pub const ENV_TAVILY_KEY_ALIAS: &str = "TAVILY_API_KEY";
/// Bocha（博查）API key。
pub const ENV_BOCHA_KEY: &str = "YUSHAN_SEARCH_BOCHA_KEY";
/// Firecrawl API key —— **必需**（官方 API 文档要求 `Authorization: Bearer`，无 key 回 401
/// 「The API key was not provided」）。**修订（2026-09-18）**：初稿把它当作 keyless
/// （据一篇第三方对比文），后果见 `backends::Row::keyless` 的修订记录与 ADR-0016。
pub const ENV_FIRECRAWL_KEY: &str = "YUSHAN_SEARCH_FIRECRAWL_KEY";
/// DeepSeek Search API key。
pub const ENV_DEEPSEEK_KEY: &str = "YUSHAN_SEARCH_DEEPSEEK_KEY";
/// DeepSeek 的**生态通用** API key 环境变量名。
///
/// 作为 [`ENV_DEEPSEEK_KEY`] 的**回退**读取：已为别的 DeepSeek 工具配过 key 的用户
/// 不必再复制一份到本项目命名空间。两者都设时 [`ENV_DEEPSEEK_KEY`] 优先。
pub const ENV_DEEPSEEK_KEY_ALIAS: &str = "DEEPSEEK_API_KEY";
/// 抓取结果落盘目录的**显式覆盖**（测试靠它隔离）。
pub const ENV_FETCHED_DIR: &str = "YUSHAN_FETCHED_DIR";

/// 搜索后端的身份。`plan` / `parse` 按它**穷尽分派**（漏改编译报错，不静默漂移）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Deepseek,
    Searxng,
    Tavily,
    Bocha,
    Firecrawl,
}

/// 启动期由常量表解析、**冻结**的后端 —— 「链确定」的载体。
///
/// 未配 key 的行在**构造期**就被剔除：调用期不再有「这个后端能不能用」的分支。
#[derive(Debug, Clone)]
pub struct Backend {
    pub kind: Kind,
    pub endpoint: String,
    pub key: Option<String>,
}

/// 落盘目录的**档位**。
///
/// 带档位而非裸 `PathBuf`，是为了让「清理只对 [`Primary`] 生效」成为
/// **类型上的保证**（[`FetchedDir::is_cleanable`]），而不是一句约定 ——
/// 用户显式指定的目录**只写不删**。
///
/// [`Primary`]: FetchedDir::Primary
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchedDir {
    /// 档 1：`$YUSHAN_FETCHED_DIR` —— 用户显式指定。**只写不删**。
    Override(PathBuf),
    /// 档 2：`<HOME>/.yushan/fetched` —— 主目录。稳定、跨会话可 grep、重启仍在。
    /// **清理只对这一档生效**。相对路径的**任何**档位都不是合法取值。
    Primary(PathBuf),
    /// 档 3：`<temp_dir>/yushan-fetched-<pid>` —— 绝对、总是可写；可能被 OS 清、不跨重启。
    Temp(PathBuf),
    /// 档 4：都不行 → **不落盘**（只返回头部 + 明确 notice）。
    ///
    /// 这是**诚实的降级**，不是「内存文件」：内存里的"文件"没有消费者
    /// （`read`/`grep` 够不着）。
    None,
}

impl FetchedDir {
    /// 落盘路径；[`FetchedDir::None`] 档返回 `None`。
    ///
    /// 返回的路径**保证是绝对路径**（`resolve_fetched_dir` 已挡掉相对取值）。
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Override(p) | Self::Primary(p) | Self::Temp(p) => Some(p),
            Self::None => None,
        }
    }

    /// 本档是否允许清理 —— **仅 [`FetchedDir::Primary`] 为真**。
    pub fn is_cleanable(&self) -> bool {
        matches!(self, Self::Primary(_))
    }
}

/// web 工具的全部配置 —— **一个可注入的值**（不是 `static`）。
///
/// 字段私有：路径解析与后端表的**规则**留在这里，调用方只拿构造器与访问器。
///
/// `Clone` 是**给装配用的**：`build_tools` 只有一个 `WebConfig`，而
/// [`WebSearchTool`] 与 [`WebFetchTool`] 各持一份（`Client` 内部是 `Arc`，克隆
/// 只复制指针 —— 连接池与 UA 策略仍是同一份）。不这么做，装配点就得把配置按
/// 工具恰好需要的字段拆开，于是「配置是一个值」这条在接缝处就散了。
#[derive(Debug, Clone)]
pub struct WebConfig {
    /// `None` = HTTP 栈不可用。工具**照常注册**，调用期明确报错。
    client: Option<Client>,
    /// 启动期解析、冻结；调用期不变（未配 key 的行已被剔除）。
    backends: Vec<Backend>,
    /// 解析结果带**档位**，不是裸 `PathBuf`（见 [`FetchedDir`]）。
    fetched: FetchedDir,
    /// 落盘目录解析时的**降级说明**（如 `Override` 档不可用）。
    ///
    /// 存构造期的真相而非在 [`WebConfig::degraded_hint`] 里重读 env：
    /// 提示必须描述「当时发生了什么」，不能随调用时的 env 变化。
    fetched_note: Option<String>,
}

impl WebConfig {
    /// 生产路径：读 env 表 + 解析落盘目录一次，之后**冻结**。
    ///
    /// **无副作用到"清理"为止** —— 启动期只创建目录，**绝不删除任何东西**
    /// （清理由 `main` 在装配序列里显式调，且只对 [`FetchedDir::Primary`] 生效）。
    pub fn from_env() -> Self {
        let (fetched, fetched_note) = resolve_fetched_dir();
        Self {
            // `Err`（TLS 后端初始化失败等）→ `offline()` 同款的降级：工具照常注册，
            // 调用期明确报错。**不在启动期 panic**（见 `net::build_client` 文档）。
            client: build_client().ok(),
            // 由 `const TABLE` + env key 解析一次，此后**冻结**（未配 key 的行已被剔除）。
            backends: resolve_backends(|key| std::env::var(key).ok()),
            fetched,
            fetched_note,
        }
    }

    /// 测试注入：用调用方提供的 `Client`。
    ///
    /// 便于注入 `.no_proxy()` / 自定义超时（如测试里直连回环假服务器），
    /// 从而不必改进程级 env —— 避免同一测试二进制内并行执行时相互污染。
    pub fn with_client(client: Client, fetched: FetchedDir) -> Self {
        Self {
            client: Some(client),
            backends: Vec::new(),
            fetched,
            fetched_note: None,
        }
    }

    /// 链注入：**[`with_client`] / [`offline`] 的链为空**（它们刻意不读 env），
    /// 而「链怎么驱动」的测试（降级 / 空结果停 / 预算切片）需要一条**受控的**链。
    ///
    /// 存在的理由与 `with_client` 同源：不该为了造一条链去改进程 env（那是数据竞争）。
    ///
    /// [`with_client`]: WebConfig::with_client
    /// [`offline`]: WebConfig::offline
    pub fn with_backends(mut self, backends: Vec<Backend>) -> Self {
        self.backends = backends;
        self
    }

    /// HTTP 栈不可用（未编译进 / 初始化失败 / 显式禁网）。
    ///
    /// 工具**仍然注册**：按登录态增减工具会让模型看到**会变的工具面**，
    /// 进而把"工具消失"误读成"我记错了"。降级改为**调用期**的明确错误结果。
    pub fn offline(fetched: FetchedDir) -> Self {
        Self {
            client: None,
            backends: Vec::new(),
            fetched,
            fetched_note: None,
        }
    }

    /// HTTP 客户端；`None` 表示 web 工具不可用。
    pub fn client(&self) -> Option<&Client> {
        self.client.as_ref()
    }

    /// 已冻结的后端链（顺序即尝试顺序）。
    pub fn backends(&self) -> &[Backend] {
        &self.backends
    }

    /// 落盘目录档位。
    pub fn fetched(&self) -> &FetchedDir {
        &self.fetched
    }

    /// 降级说明 —— **只产文案，绝不 print**。
    ///
    /// 多条问题以 `\n` 连接；一切正常返回 `None`。调用方（app 层）决定去处：
    /// 启动期去 stderr，或（有 TUI 时）包成一行提示。
    pub fn degraded_hint(&self) -> Option<String> {
        let mut issues: Vec<String> = Vec::new();

        if self.client.is_none() {
            issues.push(
                "web 工具不可用：HTTP 客户端未初始化（websearch / webfetch 调用期会明确报错）"
                    .to_string(),
            );
        }
        if self.backends.is_empty() {
            issues.push(format!(
                "未配置任何搜索后端：websearch 不可用。请设置 {ENV_SEARXNG_URL}，\
                 或 {ENV_DEEPSEEK_KEY}（或通用名 {ENV_DEEPSEEK_KEY_ALIAS}）/ \
                 {ENV_TAVILY_KEY}（或通用名 {ENV_TAVILY_KEY_ALIAS}）/ {ENV_BOCHA_KEY} / \
                 {ENV_FIRECRAWL_KEY} 之一"
            ));
        }
        if let Some(note) = &self.fetched_note {
            issues.push(note.clone());
        }

        if issues.is_empty() {
            None
        } else {
            Some(issues.join("\n"))
        }
    }
}

/// 解析落盘目录：**尝试创建，失败则降级**（纯路径拼接会静默拿到一个写不进去的目录）。
///
/// ```text
/// 1. $YUSHAN_FETCHED_DIR                          → Override
/// 2. <HOME>/.yushan/fetched                       → Primary
/// 3. std::env::temp_dir()/yushan-fetched-<pid>    → Temp
/// 4. 都不行                                        → None
/// ```
///
/// 返回 `(档位, 降级说明)` —— 第二项只在**用户显式配置被放弃**（相对路径 / 建不出来）
/// 或**四档全败**时非空，且会被 [`WebConfig::degraded_hint`] 原样带出（**不静默丢配置**）。
/// 两者**同时**成立时按 `\n` **拼接**（两条都保留，绝不互相覆盖 —— 见末尾的全败分支）。
///
/// **硬规则：任何一档返回的路径都是绝对路径。** 相对路径会解析到 **CWD**，
/// 而 coding agent 的 CWD 就是**用户仓库** —— 且 `.gitignore` 里有 `tmp/`，
/// 这种污染连 `git status` 都看不见，比看得见的污染更危险。
pub fn resolve_fetched_dir() -> (FetchedDir, Option<String>) {
    let mut note: Option<String> = None;

    if let Some(raw) = std::env::var_os(ENV_FETCHED_DIR) {
        let path = PathBuf::from(&raw);
        if !path.is_absolute() {
            // 显式配置是相对路径 → **拒收**（绝不把它按 CWD 展开）。
            note = Some(format!(
                "{ENV_FETCHED_DIR}={} 是相对路径，已忽略（相对路径会落到当前工作目录，\
                 即用户仓库）—— 改用主目录",
                path.display()
            ));
        } else if create_dir_all(&path) {
            return (FetchedDir::Override(path), None);
        } else {
            // `HOME` 存在但只读是真实的（容器、只读挂载）；用户显式设了却建不出来，
            // 必须**说出来**而不是静默换目录。
            note = Some(format!(
                "{ENV_FETCHED_DIR}={} 不可用（无法创建目录）—— 已降级到主目录",
                path.display()
            ));
        }
    }

    if let Some(path) = home_fetched_dir()
        && create_dir_all(&path)
    {
        return (FetchedDir::Primary(path), note);
    }
    if let Some(path) = temp_fetched_dir()
        && create_dir_all(&path)
    {
        return (FetchedDir::Temp(path), note);
    }

    // 四档全败。**既有的 note 与「无可用落盘目录」两条都要留下**：
    // 早先用 `note.unwrap_or_else(…)` 会在 note 存在时把后者整个丢掉，于是提示
    // 只报 Override 那条 —— 而它的文案（「…已忽略 → 改用主目录」/「…已降级到
    // 主目录」）在此时**是假的**（根本没有主目录可用）。不静默、不说假话。
    let no_dir = "无可用落盘目录（主目录与临时目录均不可写）—— 抓取结果不会落盘";
    let note = match note {
        Some(note) => format!("{note}\n{no_dir}"),
        None => no_dir.to_string(),
    };
    (FetchedDir::None, Some(note))
}

/// 主档：`<HOME>/.yushan/fetched`（无 HOME / HOME 为空或相对 → `None`）。
fn home_fetched_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let home = PathBuf::from(home);
    // `HOME=""` → `PathBuf::from("")` 是**相对**的；缺这道闸会拼出 `.yushan/fetched`。
    if !home.is_absolute() {
        return None;
    }
    Some(home.join(".yushan").join("fetched"))
}

/// 次档：`<temp_dir>/yushan-fetched-<pid>`。
///
/// 用 `std::env::temp_dir()` 而非硬编码 `/tmp`：macOS 下它是 per-user 的
/// `/var/folders/…/T`；Linux 下是共享 `/tmp` → 目录名带 `<pid>` 使其不可预测。
fn temp_fetched_dir() -> Option<PathBuf> {
    let base = std::env::temp_dir();
    // `TMPDIR` 被设成相对路径时 `temp_dir()` 会**原样返回**它 —— 同样缺一不可。
    if !base.is_absolute() {
        return None;
    }
    Some(base.join(format!("yushan-fetched-{}", std::process::id())))
}

/// 目录不存在则自建；返回是否**可用**。失败（含路径上是普通文件）即 `false`。
fn create_dir_all(path: &Path) -> bool {
    std::fs::create_dir_all(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, MutexGuard};

    // ---------- 本文件自带的小设施 ----------
    //
    // 本 crate 跨 crate 用不了 `apps/coding-agent` 的 `test_env`（`pub(crate)` +
    // bin crate），但进程级 env 改写与并行测试的 `getenv` 是**数据竞争**，
    // 必须串行化 —— 故照 `logging.rs::temp_dir` 的先例在本文件自带一份最小设施。

    /// 全局串行锁：**每个**测试都先拿到它，env 的读写才不交叉。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_lock() -> MutexGuard<'static, ()> {
        // 别的测试 panic 过 → 锁中毒；env 没有「半改」不变量，取回即可。
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// RAII：析构时把捕获的变量**恢复**（含「本来就不存在」的情形）。
    ///
    /// ⚠️ 必须**先于** `env_lock` 的 guard 析构（局部变量按声明逆序析构：
    /// `let _lock = …; let _env = …;` → `_env` 先落），恢复时仍持锁。
    struct EnvRestore(Vec<(&'static str, Option<OsString>)>);

    impl EnvRestore {
        fn capture(keys: &[&'static str]) -> Self {
            Self(keys.iter().map(|k| (*k, std::env::var_os(k))).collect())
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                // SAFETY: 调用方持有 `ENV_LOCK`，独占这些变量的改动与恢复。
                unsafe {
                    match value {
                        Some(v) => std::env::set_var(key, v),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    /// 本模块会读写的全部 env 键（恢复集合；`USERPROFILE` 是 `HOME` 的后备）。
    const TEST_KEYS: &[&str] = &[ENV_FETCHED_DIR, "HOME", "TMPDIR", "USERPROFILE"];

    /// 搜索后端的全部 env 键 —— `from_env` 经 `backends::resolve_backends` **真的会消费它们**，
    /// 故凡是要断言链的**形状**的测试都必须先把这些键清掉（否则本机配了 key 就会变红），
    /// 并用 `EnvRestore::capture(BACKEND_KEYS)` 把清掉的动作在同一测试内**复原**。
    const BACKEND_KEYS: &[&str] = &[
        ENV_SEARXNG_URL,
        ENV_DEEPSEEK_KEY,
        ENV_DEEPSEEK_KEY_ALIAS,
        ENV_TAVILY_KEY,
        ENV_TAVILY_KEY_ALIAS,
        ENV_BOCHA_KEY,
        ENV_FIRECRAWL_KEY,
    ];

    /// 清空全部后端 env 键（调用方须持有 `ENV_LOCK` 且已 `capture(BACKEND_KEYS)`）。
    fn clear_backend_keys() {
        // SAFETY: 调用方持有 `ENV_LOCK`。
        unsafe {
            for key in BACKEND_KEYS {
                std::env::remove_var(key);
            }
        }
    }

    /// 测试目录名含**进程内原子序号 + `process::id()`**：并行测试不得撞名
    /// （macOS 的 `SystemTime::as_nanos()` 只到微秒，撞名会互删）。
    fn test_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "yushan_tools_web_{name}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 一次性设好解析所依赖的三个键。`home: None` = 无 HOME（`USERPROFILE` 一并删，
    /// 免得它作为后备被捡起来）。
    fn set_env(dir: Option<&Path>, home: Option<&str>, tmpdir: Option<&str>) {
        // SAFETY: 调用方持有 `ENV_LOCK`。
        unsafe {
            match dir {
                Some(p) => std::env::set_var(ENV_FETCHED_DIR, p),
                None => std::env::remove_var(ENV_FETCHED_DIR),
            }
            match home {
                Some(h) => {
                    std::env::set_var("HOME", h);
                    std::env::remove_var("USERPROFILE");
                }
                None => {
                    std::env::remove_var("HOME");
                    std::env::remove_var("USERPROFILE");
                }
            }
            match tmpdir {
                Some(t) => std::env::set_var("TMPDIR", t),
                None => std::env::remove_var("TMPDIR"),
            }
        }
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    // ---------- S9：WebConfig 的构造器与降级提示 ----------

    /// **五个后端 env 全空**时 `from_env` 的链形状 —— **链为空**，且**有一句提示**。
    ///
    /// ⚠️ 这条断言**改过两次**，两次都是「文档/事实对不上」被抓出来的：
    ///
    /// 1. T3 写下的是「链必须为空」；
    /// 2. T9 把它改成了「只剩 keyless 的 firecrawl」 —— 当时 `TABLE` 据一篇第三方对比文把
    ///    Firecrawl 标成 `keyless: true`，于是 `from_env` 的链**永不为空**，
    ///    `degraded_hint` 里那句「未配置任何搜索后端」在 `from_env` 路径上**不可达**；
    /// 3. **修订（2026-09-18，T9 review P1）**：Firecrawl 官方 API 文档要求 `Authorization`
    ///    （无 key 回 401），`keyless` 改回 `false`。零配置用户**不该**被推到一条
    ///    **自己从没选过**的厂商的 401 上、且链上无后备 —— 故回到最初的语义：
    ///    **链为空 ⇒ 必须有一句可执行的提示**。
    ///
    /// 提示文案由 `degraded_hint` 产出（本 crate 绝不 print），去处由 app 层决定。
    ///
    /// `from_env` 顺带钉住本切片唯一在用的 env 键 `ENV_FETCHED_DIR`。
    #[test]
    fn test_from_env_without_backend_env_yields_empty_chain_and_hint() {
        let _lock = env_lock();
        let _env = EnvRestore::capture(TEST_KEYS);
        let _backends = EnvRestore::capture(BACKEND_KEYS);
        // 落盘目录指向临时目录：既隔离，也**避免测试创建真实的 `~/.yushan/fetched`**。
        let dir = test_dir("from_env");
        clear_backend_keys();
        set_env(Some(&dir), None, None);

        let web = WebConfig::from_env();

        assert_eq!(
            web.fetched(),
            &FetchedDir::Override(dir.clone()),
            "`from_env` 必须读取 {ENV_FETCHED_DIR}"
        );
        assert!(web.client().is_some(), "正常环境下 HTTP 栈应可初始化");

        // 五行都被剔除：四家托管要 key、searxng 要实例地址 —— **一个都没配 ⇒ 什么都不猜**。
        assert!(
            web.backends().is_empty(),
            "零配置（五个 `YUSHAN_SEARCH_*` 全空）⇒ 链为空：{:?}",
            web.backends()
        );

        // 链空 + client 可用 → 只剩下「未配置任何搜索后端」这一条，且**必须说出来**。
        let hint = web
            .degraded_hint()
            .expect("零配置 ⇒ 链为空 ⇒ 必须有提示（否则用户一句提示都看不到）");
        assert!(hint.contains("未配置任何搜索后端"), "{hint}");
        assert!(
            hint.contains(ENV_SEARXNG_URL)
                && hint.contains(ENV_DEEPSEEK_KEY)
                && hint.contains(ENV_DEEPSEEK_KEY_ALIAS)
                && hint.contains(ENV_TAVILY_KEY),
            "提示要能回答「下一步配什么」（含 DeepSeek 及其通用名）：{hint}"
        );
        assert!(
            !hint.contains("HTTP 客户端未初始化"),
            "client 可用时不得报它：{hint}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `offline()`：HTTP 栈不可用 —— 工具仍注册，但提示里**同时**出现
    /// 「client 缺失」与「后端为空」两条，**各自独立、不互相覆盖**。
    #[test]
    fn test_offline_hint_mentions_client_and_backends_independently() {
        let web = WebConfig::offline(FetchedDir::None);

        assert!(web.client().is_none());
        assert_eq!(web.fetched(), &FetchedDir::None);
        assert!(web.backends().is_empty());

        let hint = web.degraded_hint().expect("offline + 空链 → 必须有提示");
        assert!(hint.contains("HTTP 客户端未初始化"), "{hint}");
        assert!(hint.contains("未配置任何搜索后端"), "{hint}");
        // 两条**并存**（后者不得把前者覆盖掉）。
        assert!(hint.lines().count() >= 2, "应是拼接的多条问题：{hint:?}");
    }

    /// `with_client()`：有 client 就不该再报 client 那条 —— 两条问题**互不牵连**。
    #[test]
    fn test_with_client_hint_omits_client_issue_but_keeps_backends() {
        let web = WebConfig::with_client(Client::new(), FetchedDir::None);

        assert!(web.client().is_some());

        let hint = web.degraded_hint().expect("后端链空 → 仍有提示");
        assert!(
            !hint.contains("HTTP 客户端未初始化"),
            "client 可用时不得报它：{hint}"
        );
        assert!(hint.contains("未配置任何搜索后端"), "{hint}");
    }

    // ---------- S5：四档解析 ----------

    /// 档 1：`$YUSHAN_FETCHED_DIR` 为绝对路径 → `Override`，**无**降级说明。
    #[test]
    fn test_resolve_override_tier_without_note() {
        let _lock = env_lock();
        let _env = EnvRestore::capture(TEST_KEYS);
        let dir = test_dir("override");

        set_env(Some(&dir), None, None);
        let (tier, note) = resolve_fetched_dir();

        assert_eq!(tier, FetchedDir::Override(dir.clone()));
        assert_eq!(note, None, "用户配置被采纳 → 不该有降级说明");
        assert!(tier.path().unwrap().is_absolute(), "结果必为绝对路径");
        assert!(!tier.is_cleanable(), "`Override` 只写不删");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 档 2：无 `$YUSHAN_FETCHED_DIR`、HOME 为绝对 → `Primary`（可清理的那档）。
    #[test]
    fn test_resolve_primary_tier_under_absolute_home() {
        let _lock = env_lock();
        let _env = EnvRestore::capture(TEST_KEYS);
        let home = test_dir("primary_home");

        set_env(None, Some(home.to_str().unwrap()), None);
        let (tier, note) = resolve_fetched_dir();

        assert_eq!(
            tier,
            FetchedDir::Primary(home.join(".yushan").join("fetched"))
        );
        assert_eq!(note, None, "自动降级（非用户配置）不该产生说明");
        assert!(tier.path().unwrap().is_absolute());
        assert!(tier.is_cleanable(), "清理**只**对 `Primary` 生效");

        let _ = std::fs::remove_dir_all(&home);
    }

    /// 档 3：`HOME=""`（相对）→ 跳过主档，落 `Temp`；目录名带 `<pid>`（唯一）。
    #[test]
    fn test_resolve_temp_tier_when_home_is_relative() {
        let _lock = env_lock();
        let _env = EnvRestore::capture(TEST_KEYS);
        let base = test_dir("temp_base");

        set_env(None, Some(""), Some(base.to_str().unwrap()));
        let (tier, note) = resolve_fetched_dir();

        match &tier {
            FetchedDir::Temp(p) => {
                assert!(p.is_absolute(), "结果必为绝对路径：{}", p.display());
                assert!(p.starts_with(&base), "应落在 TMPDIR 下：{}", p.display());
                assert_eq!(
                    p.file_name().unwrap().to_str().unwrap(),
                    format!("yushan-fetched-{}", std::process::id()),
                    "目录名须带 pid（唯一）"
                );
            }
            other => panic!("`HOME=\"\"` 是相对路径 → 必须降级到 Temp，实得 {other:?}"),
        }
        assert_eq!(note, None);
        assert!(!tier.is_cleanable());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 相对 `TMPDIR` 是**真实坑**：`temp_dir()` 会把它原样返回 → 必须被
    /// `is_absolute()` 闸挡住，**绝不能在 CWD（= 仓库）里真建出目录**。
    #[test]
    fn test_relative_tmpdir_is_rejected_and_creates_nothing() {
        let _lock = env_lock();
        let _env = EnvRestore::capture(TEST_KEYS);
        let rel_tmp = format!("yushan_rel_tmpdir_{}", std::process::id());

        // 前提：CWD 里本来没有这个名字（否则下面的断言证明不了「没建」）。
        assert!(
            !Path::new(&rel_tmp).exists(),
            "CWD 里不该预先存在 {rel_tmp}"
        );

        set_env(None, Some(""), Some(&rel_tmp));
        let (tier, note) = resolve_fetched_dir();

        assert_eq!(tier, FetchedDir::None, "两档都不可用 → 宁可不落盘");
        assert!(tier.path().is_none());
        assert!(
            note.unwrap().contains("无可用落盘目录"),
            "全败必须明说没有落盘目录"
        );
        assert!(
            !Path::new(&rel_tmp).exists(),
            "相对 TMPDIR 下不得真的建目录（会落进用户仓库）"
        );
    }

    /// **四档全败**（T3 review 的假话 bug）：`$YUSHAN_FETCHED_DIR` 相对 **且**
    /// `HOME=""` **且** `TMPDIR` 相对。
    ///
    /// 旧实现用 `note.unwrap_or_else(…)` —— note 存在时会把「无可用落盘目录」
    /// 整条丢掉，于是提示只说 Override 那条（「…已忽略 → 改用主目录」），
    /// 而主目录根本不存在：**报了假话**。两条信息必须**都在**。
    ///
    /// 变异：把末尾的 `match note` 改回 `note.unwrap_or_else(|| no_dir…)` → 本测试红。
    #[test]
    fn test_all_tiers_fail_keeps_both_override_note_and_no_dir_notice() {
        let _lock = env_lock();
        let _env = EnvRestore::capture(TEST_KEYS);
        let rel_dir = format!("yushan_rel_fetched_{}", std::process::id());
        let rel_tmp = format!("yushan_rel_tmp_{}", std::process::id());

        set_env(Some(Path::new(&rel_dir)), Some(""), Some(&rel_tmp));
        let (tier, note) = resolve_fetched_dir();

        assert_eq!(tier, FetchedDir::None);
        let note = note.expect("四档全败必须给说明");

        // ① 用户显式配置被丢弃这件事没被吞掉。
        assert!(note.contains(ENV_FETCHED_DIR), "须点名被忽略的 env：{note}");
        assert!(note.contains("已忽略"), "须明说被忽略：{note}");
        // ② 「无路可走」这件事也没被吞掉。
        assert!(
            note.contains("无可用落盘目录"),
            "全败必须明说没有落盘目录：{note}"
        );
        // 两条**拼接**（不是后者覆盖前者）。
        assert!(note.lines().count() >= 2, "应是多条信息拼接：{note:?}");

        assert!(!Path::new(&rel_dir).exists());
        assert!(!Path::new(&rel_tmp).exists());
    }

    /// `Override` 档**不可用**（绝对路径但建不出来，如父路径是普通文件）→
    /// 降级到主档**并明说**（不静默丢配置）。
    #[test]
    fn test_unusable_override_degrades_to_primary_with_note() {
        let _lock = env_lock();
        let _env = EnvRestore::capture(TEST_KEYS);
        let home = test_dir("override_blocked");
        // 以普通文件当父目录 → `create_dir_all` 必然失败。
        let blocker = home.join("blocker");
        std::fs::write(&blocker, b"not a dir").unwrap();

        set_env(
            Some(&blocker.join("fetched")),
            Some(home.to_str().unwrap()),
            None,
        );
        let (tier, note) = resolve_fetched_dir();

        assert_eq!(
            tier,
            FetchedDir::Primary(home.join(".yushan").join("fetched")),
            "应降级到主档"
        );
        let note = note.expect("用户显式配置被放弃 → 必须明说");
        assert!(note.contains(ENV_FETCHED_DIR), "{note}");
        assert!(note.contains("不可用"), "{note}");
        assert!(tier.path().unwrap().is_absolute());

        let _ = std::fs::remove_dir_all(&home);
    }

    /// 降级说明必须**原样带出**到 `degraded_hint` —— 否则「不静默丢配置」
    /// 只停在解析层，用户根本看不到。
    ///
    /// 这条测试刻意造成**两条问题同时存在**（用户显式配的落盘目录被放弃 + 后端 env 全空）
    /// 并断言**两条都在**：提示是**拼接**的，谁也不许覆盖谁（与
    /// [`test_offline_hint_mentions_client_and_backends_independently`] 同一个不变量）。
    #[test]
    fn test_degraded_hint_carries_fetched_note() {
        let _lock = env_lock();
        let _env = EnvRestore::capture(TEST_KEYS);
        let _backends = EnvRestore::capture(BACKEND_KEYS);
        let home = test_dir("hint_note");
        let blocker = home.join("blocker");
        std::fs::write(&blocker, b"not a dir").unwrap();
        // 清掉后端 env 后端链必为空 —— 这里要的是「不依赖本机配了什么 key」的确定性，
        // 而不是「链非空」。
        clear_backend_keys();

        set_env(
            Some(&blocker.join("fetched")),
            Some(home.to_str().unwrap()),
            None,
        );
        let web = WebConfig::from_env();

        assert!(matches!(web.fetched(), FetchedDir::Primary(_)));
        assert!(web.backends().is_empty(), "五个后端 env 已清空 ⇒ 链为空");

        let hint = web
            .degraded_hint()
            .expect("有降级说明 → 必须有提示（不能因为链非空就把用户配置被丢弃这件事吞掉）");
        // ① 用户显式配置被放弃这件事**没被吞掉**。
        assert!(hint.contains(ENV_FETCHED_DIR), "{hint}");
        assert!(hint.contains("不可用"), "{hint}");
        // ② 后端链为空这件事**也在**（两条必须并存、不得互相覆盖）。
        assert!(hint.contains("未配置任何搜索后端"), "{hint}");
        assert!(hint.lines().count() >= 2, "应是拼接的多条问题：{hint:?}");

        let _ = std::fs::remove_dir_all(&home);
    }

    /// **不变式**：任何档位返回的路径都必须是**绝对**路径；无档可用时宁可不落盘，
    /// 也绝不产出相对路径（相对路径会解析到 CWD，而 CWD 就是用户仓库）。
    #[test]
    fn test_every_resolved_tier_yields_an_absolute_path() {
        let _lock = env_lock();
        let _env = EnvRestore::capture(TEST_KEYS);
        let abs = test_dir("abs_invariant");
        let abs_home = abs.to_str().unwrap();
        let abs_tmp = abs.join("tmpbase");
        std::fs::create_dir_all(&abs_tmp).unwrap();
        let abs_tmp = abs_tmp.to_str().unwrap();

        /// 场景：(名字, `$YUSHAN_FETCHED_DIR`, `HOME`, `TMPDIR`)。
        type Case<'a> = (&'a str, Option<&'a Path>, Option<&'a str>, Option<&'a str>);
        let cases: [Case; 5] = [
            ("Override 绝对路径", Some(abs.as_path()), None, None),
            ("HOME 绝对", None, Some(abs_home), None),
            (
                "HOME 绝对 + TMPDIR 绝对",
                None,
                Some(abs_home),
                Some(abs_tmp),
            ),
            ("HOME=\"\"（相对）→ Temp", None, Some(""), Some(abs_tmp)),
            ("无 HOME → Temp", None, None, Some(abs_tmp)),
        ];
        for (name, dir, home, tmpdir) in cases {
            set_env(dir, home, tmpdir);
            let (tier, _) = resolve_fetched_dir();
            let path = tier
                .path()
                .unwrap_or_else(|| panic!("{name}：该档应有路径"));
            assert!(
                path.is_absolute(),
                "{name}：必须绝对，实得 {}",
                path.display()
            );
        }

        // 两个真实坑同时出现 → 无档可用（也不许退化成相对路径）。
        set_env(None, Some(""), Some("rel_tmp"));
        assert_eq!(resolve_fetched_dir().0, FetchedDir::None);

        let _ = std::fs::remove_dir_all(&abs);
    }

    /// `FetchedDir` 的访问器契约：`path()` 的档位对应 + **只有 `Primary` 可清理**。
    #[test]
    fn test_fetched_dir_accessors() {
        let p = PathBuf::from("/abs/fetched");
        assert_eq!(FetchedDir::Override(p.clone()).path(), Some(p.as_path()));
        assert_eq!(FetchedDir::Primary(p.clone()).path(), Some(p.as_path()));
        assert_eq!(FetchedDir::Temp(p.clone()).path(), Some(p.as_path()));
        assert_eq!(FetchedDir::None.path(), None);

        assert!(FetchedDir::Primary(p.clone()).is_cleanable());
        assert!(!FetchedDir::Override(p.clone()).is_cleanable());
        assert!(!FetchedDir::Temp(p.clone()).is_cleanable());
        assert!(!FetchedDir::None.is_cleanable());
    }

    /// **语义钉死（有意为之，不是漏判）**：`resolve_fetched_dir` 只判「**能创建**」，
    /// **不探测可写性** —— 已存在但只读的目录会被判为可用并选中
    /// （`create_dir_all` 对已存在目录返回 `Ok`，见 T3 review 问题 2）。
    ///
    /// 计划的字面要求就是「尝试创建，失败则降级」，设计也没要求探测可写性；
    /// 真正写不进去会在 `store::write_fetched` 的**写**路径上明确报错。
    /// 把它写成测试，免得以后与那条错误路径打架。
    #[cfg(unix)]
    #[test]
    fn test_existing_but_readonly_dir_counts_as_available() {
        let _lock = env_lock();
        let _env = EnvRestore::capture(TEST_KEYS);
        let dir = test_dir("readonly_override");
        set_mode(&dir, 0o555);

        set_env(Some(&dir), None, None);
        let (tier, note) = resolve_fetched_dir();

        assert_eq!(
            tier,
            FetchedDir::Override(dir.clone()),
            "存在即判可用（只判可创建，不判可写）"
        );
        assert_eq!(note, None);

        // 复原权限，否则目录删不掉。
        set_mode(&dir, 0o755);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
