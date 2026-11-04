//! 出站图片解析：本地路径 → `data:` URL。**本仓唯一读图字节的地方**（design.md §3.4）。
//!
//! 为什么唯一的 IO 点在这里：只有适配器认识 wire 格式（`image_url.url` 要的是 data URL），
//! 而会话里只存路径（design.md §3.7）。工具层因此永不读像素，像素字节也不跨 crate 边界。
//!
//! 三条硬约束：
//!
//! - **锁边界**（design.md §3.4）：读盘、嗅探、base64 编码全部在锁外；锁内只做查表与插入。
//!   因此用 [`std::sync::Mutex`] 就够（锁内无 `.await`，不存在 guard 跨 await 的 `Send` 问题）。
//! - **体积判据是编码后的字节数**（C8 / D18）：32 MiB 指 `base64` 之后（含 `data:` 前缀）
//!   的字节数，不是原图字节数。原图 24 MiB 编码后是 32 MiB + 前缀 ⇒ 超限。
//! - **缓存键含 `(path, mtime, len)`**：原图被改写后必须新增条目，不得命中旧内容。

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::UNIX_EPOCH;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ys_core::sniff_image;

/// 格式嗅探读的文件头字节数，与 `adapters/tools-basic/src/ocr/mod.rs` 的 `DIM_HEAD_BYTES`
/// 同口径（TIFF 的 IFD0 可能离文件头较远，故给足）。
pub const HEAD_BYTES: usize = 256 * 1024;

/// 单图上限：**编码后** 32 MiB（含 `data:` 前缀）。
pub const DEFAULT_SINGLE_MAX_BYTES: usize = 32 * 1024 * 1024;

/// 缓存总字节上限：128 MiB（base64 后约为原图 1.37 倍 ⇒ 约 93 MiB 原图存量）。
pub const DEFAULT_TOTAL_MAX_BYTES: usize = 128 * 1024 * 1024;

/// 缓存条目上限：与总字节上限任一超出即按 LRU 淘汰。
pub const DEFAULT_MAX_ENTRIES: usize = 64;

/// 单请求**累计**预算：编码后图片总字节上限 48 MiB。
///
/// 这是**真正的**上限（D19）：单图 32 MiB × 2 = 64 MiB 已经超了它，所以两条限额
/// 必须协调 —— 逐消息累计，超限时**当前及之后**的图降为占位。
pub const DEFAULT_REQUEST_BUDGET_BYTES: usize = 48 * 1024 * 1024;

/// 单请求图片数上限。
pub const DEFAULT_MAX_IMAGES_PER_REQUEST: usize = 600;

/// `resolve_image` 的失败原因。
///
/// 每个变体都对应一句给模型看的占位文案（`translate_request` 侧渲染）；错误本身
/// **不**包含像素数据、也不超过数百字节。
///
/// 变体分三档语义，因为模型据此要做的事不同：读不到 ⇒ 检查路径；不是图 ⇒ 换文件；
/// 是图但端点不收（`bmp` / `tiff`）⇒ 改用 `ocr`。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    /// 打不开 / 读不出 / 不是普通文件。`reason` 是 OS 或本模块的原始描述
    /// （如 `No such file or directory (os error 2)`）。
    #[error("could not read `{path}`: {reason}")]
    ReadFailed { path: String, reason: String },
    /// 内容不是任何已知的图像容器（头不足、魔数不匹配、尺寸字段异常）。
    #[error("`{path}` is not a recognized image: {reason}")]
    NotAnImage { path: String, reason: String },
    /// 认得是图，但像素格式不适合直接送进视觉请求（如 `bmp` / `tiff`）。
    #[error("unsupported image `{mime}` at `{path}`")]
    UnsupportedFormat { path: String, mime: String },
    /// 单图超限。`bytes` 是**编码后的实际字节数**（`base64` 载荷在出错路径上仍是精确值），
    /// `limit` 是当前生效的 [`MediaLimits::single_max_bytes`]。
    #[error("image is too large: {bytes} > {limit} bytes")]
    SingleTooLarge { bytes: usize, limit: usize },
}

/// 缓存上界与单图上限。
///
/// 三者都可注入：真实上限（32 MiB / 128 MiB）在正例里无法构造夹具，测试一律用小值
/// 验证同一套逻辑（C8.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaLimits {
    /// 单图编码后字节上限（含 `data:` 前缀）。
    pub single_max_bytes: usize,
    /// 缓存里全部 data URL 的字节总和上限。
    pub total_max_bytes: usize,
    /// 缓存条目数上限。
    pub max_entries: usize,
    /// 单请求**累计**字节预算（编码后，D19）。
    pub request_budget_bytes: usize,
    /// 单请求图片数上限。
    pub max_images_per_request: usize,
}

impl Default for MediaLimits {
    fn default() -> Self {
        Self {
            single_max_bytes: DEFAULT_SINGLE_MAX_BYTES,
            total_max_bytes: DEFAULT_TOTAL_MAX_BYTES,
            max_entries: DEFAULT_MAX_ENTRIES,
            request_budget_bytes: DEFAULT_REQUEST_BUDGET_BYTES,
            max_images_per_request: DEFAULT_MAX_IMAGES_PER_REQUEST,
        }
    }
}

/// 一次 [`MediaCache::stats`] 的快照（测试与诊断用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaCacheStats {
    /// 命中缓存的次数。
    pub hits: u64,
    /// 未命中且**真的编码了一遍**的次数；并发同键会各计一次（设计允许的重复）。
    pub computes: u64,
    /// 因并发下前人已插入同键而丢弃自己那份的次数。
    pub lost_races: u64,
}

/// 路径 → data URL 的 LRU 缓存。
///
/// 键含 `(path, mtime, len)`：同一路径被改写后**新增**条目而不是替换，旧条目靠 LRU 淘汰。
/// 这样「读到的内容」与「用来查表的元数据」始终对应同一版文件。
#[derive(Debug)]
pub struct MediaCache {
    limits: MediaLimits,
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    entries: HashMap<Key, Entry>,
    /// 当前缓存里全部 data URL 的字节总和。
    total_bytes: usize,
    /// 逻辑时钟，只用来定 LRU 序（不用 `Instant`：比较大小即可，无需真实时间）。
    clock: u64,
    stats: MediaCacheStats,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    path: String,
    /// 修改时间（自 UNIX_EPOCH 起的秒 + 纳秒）；取不到时为 `(0, 0)`。
    mtime: (u64, u32),
    len: u64,
}

#[derive(Debug)]
struct Entry {
    /// 命中即 `Arc::clone` 复用，零拷贝。
    url: Arc<str>,
    /// 最近一次使用时的 [`Inner::clock`] 值。
    last_used: u64,
}

impl Key {
    fn new(path: &str, meta: &fs::Metadata) -> Self {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or((0, 0), |d| (d.as_secs(), d.subsec_nanos()));
        Self {
            path: path.to_owned(),
            mtime,
            len: meta.len(),
        }
    }
}

impl Default for MediaCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaCache {
    /// 默认上限（32 MiB / 128 MiB / 64 条）。
    pub fn new() -> Self {
        Self::with_limits(MediaLimits::default())
    }

    /// 注入上限。测试与「累计预算」类用例走这条路径，避免真造十几 MB 的文件。
    pub fn with_limits(limits: MediaLimits) -> Self {
        Self {
            limits,
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                total_bytes: 0,
                clock: 0,
                stats: MediaCacheStats::default(),
            }),
        }
    }

    /// 当前生效的上限。
    pub fn limits(&self) -> MediaLimits {
        self.limits
    }

    /// 缓存条目数（测试用来验证淘汰）。
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// [`Self::len`] 的配对（`clippy::len_without_is_empty`）。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 缓存里 data URL 的字节总和（测试用来验证总字节上界）。
    pub fn total_bytes(&self) -> usize {
        self.lock().total_bytes
    }

    /// 命中 / 计算 / 丢弃的计数快照。
    pub fn stats(&self) -> MediaCacheStats {
        self.lock().stats
    }

    /// 一次加锁内完成「查表 + 刷新 LRU」。**不做 IO**。
    fn lookup(&self, key: &Key) -> Option<Arc<str>> {
        let mut inner = self.lock();
        inner.clock += 1;
        let tick = inner.clock;
        let Some(entry) = inner.entries.get_mut(key) else {
            inner.stats.computes += 1;
            return None;
        };
        entry.last_used = tick;
        let url = Arc::clone(&entry.url);
        inner.stats.hits += 1;
        Some(url)
    }

    /// 一次加锁内完成「插入 + 淘汰」。**不做 IO**。
    ///
    /// 若并发下前人已插入同键，丢弃自己那份、返回已有的（内容相同，白算一次是
    /// 设计允许的代价 —— 换来锁内不做重活）。
    fn insert(&self, key: Key, url: Arc<str>) -> Arc<str> {
        // 单条就超总量上限者不缓存：它一进表就会被自己挤出去，白付一次淘汰。
        if url.len() > self.limits.total_max_bytes || self.limits.max_entries == 0 {
            return url;
        }

        let mut inner = self.lock();
        inner.clock += 1;
        let tick = inner.clock;

        if let Some(existing) = inner.entries.get_mut(&key) {
            existing.last_used = tick;
            let url = Arc::clone(&existing.url);
            inner.stats.lost_races += 1;
            return url;
        }

        inner.total_bytes += url.len();
        inner.entries.insert(
            key,
            Entry {
                url: Arc::clone(&url),
                last_used: tick,
            },
        );
        inner.evict(self.limits);
        url
    }

    /// 中毒的锁照常取用：锁内只碰内存结构，没有「写了一半」的不变量；为一次 panic
    /// 让此后所有看图永久失败，是比中毒本身更坏的结局。
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Inner {
    fn evict(&mut self, limits: MediaLimits) {
        while self.total_bytes > limits.total_max_bytes || self.entries.len() > limits.max_entries {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                // 空表必然不超限；留着 break 只为让循环条件与不变量解耦。
                break;
            };
            if let Some(entry) = self.entries.remove(&victim) {
                self.total_bytes -= entry.url.len();
            }
        }
    }
}

/// 把本地图片读成可直接塞进 `image_url.url` 的 data URL。
///
/// 锁边界是承重的：命中缓存的快路径在**一次**加锁内完成查表与 LRU 刷新；
/// 未命中时读盘、嗅探、编码全在锁外；只有插入在这里加第二次锁。
///
/// 返回 `Arc<str>` 而不是 `String`：同一张图在多条消息、多回合里重复出现时，
/// 只是同一个引用被克隆。
pub fn resolve_image(path: &str, cache: &MediaCache) -> Result<Arc<str>, MediaError> {
    let limit = cache.limits.single_max_bytes;

    let meta = fs::metadata(path).map_err(|e| MediaError::ReadFailed {
        path: path.to_owned(),
        reason: e.to_string(),
    })?;
    if meta.is_dir() {
        return Err(MediaError::ReadFailed {
            path: path.to_owned(),
            reason: "is a directory, not a file".to_owned(),
        });
    }

    let key = Key::new(path, &meta);
    if let Some(hit) = cache.lookup(&key) {
        return Ok(hit);
    }

    // ---- 锁外：只读文件头，先判「明显超限」与「是不是图」----
    let mut file = fs::File::open(path).map_err(|e| MediaError::ReadFailed {
        path: path.to_owned(),
        reason: e.to_string(),
    })?;

    let mut raw = Vec::new();
    read_bounded(&mut file, &mut raw, HEAD_BYTES, path)?;
    if raw.is_empty() {
        return Err(MediaError::NotAnImage {
            path: path.to_owned(),
            reason: "file is empty".to_owned(),
        });
    }
    let Some((format, _width, _height)) = sniff_image(&raw) else {
        return Err(MediaError::NotAnImage {
            path: path.to_owned(),
            reason: format!("unrecognized image header (checked {HEAD_BYTES} bytes)"),
        });
    };
    if !format.is_vision_supported() {
        return Err(MediaError::UnsupportedFormat {
            path: path.to_owned(),
            mime: format.mime().to_owned(),
        });
    }
    let mime = format.mime();

    // 判据是编码后的字节数（C8 / D18），且按 `metadata` 给出的原图长度先算一遍：
    // 这一步省掉的是「注定被拒的图仍先分配 32+ MiB 的 String」。
    let expected_wire_len = wire_len(meta.len() as usize, mime);
    if expected_wire_len > limit {
        return Err(MediaError::SingleTooLarge {
            bytes: expected_wire_len,
            limit,
        });
    }

    // ---- 锁外：读余下部分并编码 ----
    // 上限取 `limit`：base64 膨胀只增不减 ⇒ 原图字节数一旦超过 `limit` 必然超限，
    // 因此读盘分配也有界（封顶 32 MiB），不随文件大小线性膨胀。
    let remaining = limit.saturating_sub(raw.len());
    read_bounded(&mut file, &mut raw, remaining, path)?;
    if raw.len() > limit {
        return Err(MediaError::SingleTooLarge {
            bytes: wire_len(raw.len(), mime),
            limit,
        });
    }

    let prefix_len = "data:".len() + mime.len() + ";base64,".len();
    let mut url = String::with_capacity(prefix_len + base64_len(raw.len()));
    url.push_str("data:");
    url.push_str(mime);
    url.push_str(";base64,");
    STANDARD.encode_string(&raw, &mut url);
    drop(raw);

    // 精确判定以**实际编码后**的长度为准：文件在 `metadata` 与读盘之间长大时，
    // 上面的预估不准，这里是最终关口。
    if url.len() > limit {
        return Err(MediaError::SingleTooLarge {
            bytes: url.len(),
            limit,
        });
    }

    let url: Arc<str> = Arc::from(url);

    // ---- 锁内：只插入 ----
    // 内容对应的元数据在读取期间可能已变：变了就不再缓存（否则会用陈旧键命中新字节）。
    // 重 stat 只在未命中路径上发生，代价远小于刚才那次读盘。
    let fresh = fs::metadata(path)
        .map(|m| Key::new(path, &m))
        .unwrap_or_else(|_| key.clone());
    if fresh != key {
        return Ok(url);
    }
    Ok(cache.insert(key, url))
}

/// 把 `file` 的至多 `cap` 字节追加进 `buf`。
///
/// 预留容量封顶 256 KiB：调用方可能要求读到 32 MiB，但按需增长比一次性预留更省
/// （被拒的图往往只读几 KiB 就返回）。
fn read_bounded(
    file: &mut fs::File,
    buf: &mut Vec<u8>,
    cap: usize,
    path: &str,
) -> Result<(), MediaError> {
    let before = buf.len();
    buf.reserve(cap.min(HEAD_BYTES));
    file.take(cap as u64)
        .read_to_end(buf)
        .map_err(|e| MediaError::ReadFailed {
            path: path.to_owned(),
            reason: e.to_string(),
        })?;
    debug_assert!(buf.len() - before <= cap);
    Ok(())
}

/// `base64` 编码后的载荷字节数（不含 `data:` 前缀）。
///
/// 标准字母表 + padding：`4 * ceil(raw / 3)`。它是 `raw` 的纯函数，因此拒绝路径上的
/// `bytes` 不必真编码就能给出精确值。
fn base64_len(raw: usize) -> usize {
    raw.div_ceil(3).saturating_mul(4)
}

/// 一张 `raw` 字节的图在 wire 上的完整长度（`data:` 前缀 + base64 载荷）。
fn wire_len(raw: usize, mime: &str) -> usize {
    "data:".len() + mime.len() + ";base64,".len() + base64_len(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime};

    /// 临时目录名含**进程内原子序号 + `process::id()`**（并行安全，见 `AGENTS.md`）。
    fn test_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "model_openai_media_test_{id}_{}_{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 合法 PNG 头（魔数 + IHDR + 宽高）。
    fn png_head(width: u32, height: u32) -> Vec<u8> {
        let mut head = b"\x89PNG\r\n\x1a\n".to_vec();
        head.extend_from_slice(&13u32.to_be_bytes());
        head.extend_from_slice(b"IHDR");
        head.extend_from_slice(&width.to_be_bytes());
        head.extend_from_slice(&height.to_be_bytes());
        head
    }

    /// 造一个「PNG 头 + 填充」的文件。
    fn write_png(dir: &Path, name: &str, total: usize) -> PathBuf {
        write_png_filled(dir, name, total, 0x5A)
    }

    /// 同上，但填充值可指定（用来造**同长度不同内容**的文件）。
    fn write_png_filled(dir: &Path, name: &str, total: usize, fill: u8) -> PathBuf {
        let mut bytes = png_head(8, 6);
        assert!(total > bytes.len(), "total 必须大于头长");
        bytes.resize(total, fill);
        let path = dir.join(name);
        fs::write(&path, &bytes).unwrap();
        path
    }

    fn small_limits(single: usize) -> MediaLimits {
        MediaLimits {
            single_max_bytes: single,
            ..MediaLimits::default()
        }
    }

    fn path_str(p: &Path) -> String {
        p.to_str().unwrap().to_owned()
    }

    /// 正例：PNG 路径 → `data:image/png;base64,…`，且载荷是原图字节的 base64。
    #[test]
    fn png_resolves_to_data_url() {
        let dir = test_dir();
        let path = write_png(&dir, "a.png", 400);
        let raw = fs::read(&path).unwrap();

        let cache = MediaCache::new();
        let url = resolve_image(&path_str(&path), &cache).unwrap();

        assert!(url.starts_with("data:image/png;base64,"), "url = {url}");
        let payload = url.strip_prefix("data:image/png;base64,").unwrap();
        assert_eq!(
            payload,
            STANDARD.encode(&raw),
            "载荷必须是文件字节的 base64"
        );
        assert!(!payload.contains('\n'), "不得带换行：端点会 400");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 缓存命中：第二次调用返回**同一个** `Arc`（指针相等），且是一次真实的查表命中
    ///（`stats().hits` 涨一）。
    ///
    /// `ptr_eq` 单看不够：即使从不查表、每次都重算，插入路径的「前人已有 ⇒ 用已有的」
    /// 也会让两次拿到同一个 `Arc`。所以必须同时断言命中计数 —— 那才是「未重算」的证据。
    ///
    /// 变异：把 `resolve_image` 里的 `cache.lookup` 调用删掉（每次都重算）⇒ `hits` 停在 0，
    /// 本条变红。
    #[test]
    fn second_resolve_hits_the_cache_without_recomputing() {
        let dir = test_dir();
        let path = write_png(&dir, "hit.png", 512);
        let cache = MediaCache::new();

        let first = resolve_image(&path_str(&path), &cache).unwrap();
        assert_eq!(
            cache.stats(),
            MediaCacheStats {
                hits: 0,
                computes: 1,
                lost_races: 0
            },
            "首次必然未命中"
        );

        let second = resolve_image(&path_str(&path), &cache).unwrap();

        assert!(Arc::ptr_eq(&first, &second), "命中缓存必须零拷贝复用");
        assert_eq!(
            cache.stats(),
            MediaCacheStats {
                hits: 1,
                computes: 1,
                lost_races: 0
            },
            "第二次必须是查表命中，不得重算"
        );
        assert_eq!(cache.len(), 1, "同键只留一条");
        assert_eq!(cache.total_bytes(), first.len());

        let _ = fs::remove_dir_all(&dir);
    }

    /// **缓存键含 `mtime`**：只把 mtime 往后拨 5s、**长度不变**、内容换掉 ⇒ 必须重新编码。
    ///
    /// 用例特意保持长度不变，所以 `len` 键没变；能不能看到新内容完全取决于
    /// `mtime` 在不在键里。变异：从 `Key` 里去掉 `mtime` ⇒ 命中旧条目、返回旧内容
    /// ⇒ 本条变红。
    #[test]
    fn mtime_is_part_of_the_cache_key() {
        let dir = test_dir();
        let path = write_png(&dir, "touch.png", 300);
        let cache = MediaCache::new();
        let p = path_str(&path);

        let first = resolve_image(&p, &cache).unwrap();

        // 同长度、不同填充 ⇒ 只有 mtime（与内容本身）能区分。
        write_png_filled(&dir, "touch.png", 300, 0xA5);
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(
            fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(5)),
        )
        .unwrap();
        drop(file);
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            300,
            "本用例必须保持长度不变，否则测的是 len 键"
        );

        let second = resolve_image(&p, &cache).unwrap();

        assert_ne!(
            first.as_ref(),
            second.as_ref(),
            "mtime 变了必须重新编码，不得命中旧条目"
        );
        assert_eq!(
            second.strip_prefix("data:image/png;base64,").unwrap(),
            STANDARD.encode(fs::read(&path).unwrap()),
            "必须是新内容"
        );
        assert_eq!(cache.len(), 2, "旧条目新增淘汰对象，不是被替换");
        assert_eq!(cache.total_bytes(), first.len() + second.len());

        let _ = fs::remove_dir_all(&dir);
    }

    /// **缓存键含 `len`**：mtime 人为保持不变、内容变短 ⇒ 也必须重新编码。
    ///
    /// 真实场景：同一 mtime 粒度内覆写文件（截断再写，时间戳不动）。变异：从 `Key`
    /// 里去掉 `len` ⇒ 命中旧条目、拿到更长的旧 data URL ⇒ 本条变红。
    #[test]
    fn len_is_part_of_the_cache_key() {
        let dir = test_dir();
        let path = write_png(&dir, "truncate.png", 400);
        let cache = MediaCache::new();
        let p = path_str(&path);
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();

        let first = resolve_image(&p, &cache).unwrap();

        write_png(&dir, "truncate.png", 300);
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(mtime))
            .unwrap();
        drop(file);
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            mtime,
            "本用例必须保持 mtime 不变，否则测的是 mtime 键"
        );

        let second = resolve_image(&p, &cache).unwrap();

        assert_eq!(
            second.strip_prefix("data:image/png;base64,").unwrap(),
            STANDARD.encode(fs::read(&path).unwrap()),
            "len 变了必须重新编码"
        );
        assert_ne!(second.len(), first.len(), "新内容更短");
        assert_eq!(cache.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    /// **C8 反向用例**：判据是编码后的字节数，不是原图字节数。
    ///
    /// 上限 500 B ⇒ 原图最多 350 B（base64 后 468 B + 22 B 前缀 = 490 ≤ 500）。
    /// - 原图 350 B：通过（编码后 490 B 仍在上限内）
    /// - 原图 354 B：拒绝（原图仍小于 500 B，但编码后 494 B + 22 = 516 B > 500）
    ///
    /// 若有人把判据改回 `raw.len() > limit`，第二条断言变红。
    ///
    /// 真实尺寸的同一条算术：24 MiB（25165824 B）编码后恰是 32 MiB 载荷，加 22 B 前缀
    /// ⇒ 33554454 B > 32 MiB(33554432) ⇒ 超限。见
    /// [`d18_real_24_mib_image_exceeds_the_default_limit`]。
    #[test]
    fn limit_is_judged_on_encoded_bytes_not_raw_bytes() {
        let dir = test_dir();
        let limit = 500;
        let cache = MediaCache::with_limits(small_limits(limit));

        // 1) 原图 350 B（> 100 B 的嗅探头，走到编码路径）且编码后仍在上限内。
        let ok_path = write_png(&dir, "fits.png", 350);
        let url = resolve_image(&path_str(&ok_path), &cache).expect("编码后 490 B ≤ 500 B");
        assert_eq!(url.len(), 490, "22 前缀 + 468 载荷");

        // 2) 原图 358 B < 上限，但编码后 480 B + 22 = 502 B > 500 B。
        let over_path = write_png(&dir, "over.png", 358);
        let err = resolve_image(&path_str(&over_path), &cache).unwrap_err();

        assert!(
            358 < limit,
            "前提：原图字节数在上限之内（按原图判就会放过它）"
        );
        assert_eq!(
            err,
            MediaError::SingleTooLarge { bytes: 502, limit },
            "bytes 必须是编码后的精确长度（480 B 载荷 + 22 B 前缀）"
        );
        assert_eq!(cache.len(), 1, "被拒的图不得进缓存");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 编码后闸门（`if url.len() > limit`）的**唯一**覆盖：文件在 `stat` 与读盘之间长大。
    ///
    /// 构造用命名管道（FIFO）：`metadata().len() == 0` ⇒ 编码前按原图长度的预估必然放行；
    /// 实际字节数任意 ⇒ 只剩编码后那道闸门能拒。
    ///
    /// 算术（`limit = 307 200`，`HEAD_BYTES = 262 144`）：第一次读满 `HEAD_BYTES`，第二次读
    /// `limit - HEAD_BYTES` ⇒ `raw.len()` **恰好** `= limit` ⇒ `raw.len() > limit` 那道闸门
    /// **不触发**；编码后 `wire_len(307200, "image/png") = 409 622 > limit` ⇒ 编码后闸门抓住。
    ///
    /// 变异自检：删掉编码后的 `if url.len() > limit { … }` ⇒ 本用例拿到 `Ok`，红。
    #[cfg(unix)]
    #[test]
    fn truncated_read_is_caught_by_the_encoded_size_gate() {
        use std::io::Write;
        use std::process::Command;

        let dir = test_dir();
        let fifo = dir.join("pipe.png");
        match Command::new("mkfifo").arg(&fifo).status() {
            Ok(status) if status.success() => {}
            other => {
                // 非 Unix 或无 `mkfifo`：留一条记录而不是静默通过。
                eprintln!("skip: mkfifo unavailable ({other:?})");
                let _ = fs::remove_dir_all(&dir);
                return;
            }
        }

        let limit = 300 * 1024; // > HEAD_BYTES：见上面的算术
        let cache = MediaCache::with_limits(small_limits(limit));

        let writer = {
            let fifo = fifo.clone();
            std::thread::spawn(move || {
                // 阻塞式打开：读端是**本进程的另一线程**，两端会合不依赖外部消费者。
                let mut file = fs::File::create(&fifo).expect("open fifo for writing");
                // 先写合法 PNG 头（嗅探要过），再灌到远超 limit。
                let mut bytes = png_head(8, 6);
                bytes.resize(400_000, 0x5A);
                // 读端只消费到 limit 就收手：余下的写入拿到 EPIPE —— 属预期，故忽略错误。
                let _ = file.write_all(&bytes);
            })
        };

        let err = resolve_image(&path_str(&fifo), &cache).unwrap_err();
        let _ = writer.join();

        assert_eq!(
            err,
            MediaError::SingleTooLarge {
                bytes: wire_len(limit, "image/png"),
                limit,
            },
            "读到的原图字节数恰好等于 limit，只有编码后的长度超限"
        );
        assert_eq!(cache.len(), 0, "被拒的图不得进缓存");

        let _ = fs::remove_dir_all(&dir);
    }

    /// D18 的**真实体积**算术：24 MiB 原图在默认 32 MiB 上限下必须判超限。
    ///
    /// 先造一个真的 24 MiB PNG（头 + 填充），用默认上限的缓存解一次；再造一张 22.25 MiB
    /// 的，证明「编码后仍在上限内确实能过」—— 所以拒绝不是别的原因。
    /// `resolve_image` 对超限文件只读到上限就判，不会把 24 MiB 拉进第二个缓冲区。
    ///
    /// 变异：把判据改成 `raw > limit`（按原图判）⇒ 24 MiB < 32 MiB 会通过，本条变红。
    #[test]
    fn d18_real_24_mib_image_exceeds_the_default_limit() {
        let dir = test_dir();
        let big = 24 * 1024 * 1024;
        let path = write_png(&dir, "24mib.png", big);
        let cache = MediaCache::new();

        let err = resolve_image(&path_str(&path), &cache).unwrap_err();
        assert_eq!(
            err,
            MediaError::SingleTooLarge {
                bytes: 33_554_454,
                limit: DEFAULT_SINGLE_MAX_BYTES
            },
            "24 MiB 原图编码后是 32 MiB 载荷 + 22 B 前缀"
        );
        assert_eq!(cache.len(), 0, "超限的图不得进缓存");

        // 对照：同一张图缩到 24 MiB 以内（编码后 30.7 MiB）就能过。
        let small = 23_331_500;
        let ok = write_png(&dir, "fits.png", small);
        let url = resolve_image(&path_str(&ok), &cache).expect("30.7 MiB 编码后仍在上限内");
        assert!(url.len() < DEFAULT_SINGLE_MAX_BYTES);
        assert_eq!(url.len(), wire_len(small, "image/png"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// 非图片内容 ⇒ `NotAnImage`（不是 `UnsupportedFormat`，两者给模型的信息不同）。
    #[test]
    fn text_file_is_not_an_image() {
        let dir = test_dir();
        let path = dir.join("notes.txt");
        fs::write(&path, b"hello, this is not an image at all").unwrap();

        let err = resolve_image(&path_str(&path), &MediaCache::new()).unwrap_err();

        assert!(
            matches!(err, MediaError::NotAnImage { .. }),
            "err = {err:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// BMP 能被嗅探出尺寸，但视觉端点不收 ⇒ `UnsupportedFormat` + MIME，且**不缓存**。
    #[test]
    fn bmp_is_recognized_but_unsupported() {
        let dir = test_dir();
        let path = dir.join("old.bmp");
        // BM + 14 字节 file header + 40 字节 DIB 头（宽 4 / 高 8，小端）。
        let mut bytes = b"BM".to_vec();
        bytes.resize(18, 0);
        bytes.extend_from_slice(&4i32.to_le_bytes());
        bytes.extend_from_slice(&8i32.to_le_bytes());
        fs::write(&path, &bytes).unwrap();

        let cache = MediaCache::new();
        let err = resolve_image(&path_str(&path), &cache).unwrap_err();

        assert_eq!(
            err,
            MediaError::UnsupportedFormat {
                path: path_str(&path),
                mime: "image/bmp".to_owned()
            }
        );
        assert_eq!(cache.len(), 0);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 路径不存在 / 是目录 ⇒ `ReadFailed`，原因里带 OS 原文（占位文案要能看出
    /// `No such file`）。
    #[test]
    fn missing_path_and_directory_fail_with_a_reason() {
        let dir = test_dir();
        let missing = dir.join("nope.png");

        let err = resolve_image(&path_str(&missing), &MediaCache::new()).unwrap_err();
        match err {
            MediaError::ReadFailed { reason, .. } => {
                assert!(reason.contains("No such file"), "reason = {reason}");
            }
            other => panic!("期望 ReadFailed，得到 {other:?}"),
        }

        let err = resolve_image(&path_str(&dir), &MediaCache::new()).unwrap_err();
        assert!(
            matches!(err, MediaError::ReadFailed { .. }),
            "目录不是图，err = {err:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 空文件 ⇒ `NotAnImage`（不得因为 `buf.is_empty()` 被当成「读成功、嗅探失败以外的某种状态」）。
    #[test]
    fn empty_file_is_not_an_image() {
        let dir = test_dir();
        let path = dir.join("empty.png");
        fs::write(&path, b"").unwrap();

        let err = resolve_image(&path_str(&path), &MediaCache::new()).unwrap_err();
        assert!(
            matches!(err, MediaError::NotAnImage { .. }),
            "err = {err:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// **缓存上界**：条目数与总字节任一超出即按 LRU 淘汰，且淘汰的是最久未用者。
    ///
    /// 用注入的小上限（2 条 / 600 B）验证同一套逻辑，不真造 128 MiB。
    /// 变异：删掉 `evict` ⇒ `len()` 与 `total_bytes()` 双双越界，两条断言变红。
    #[test]
    fn cache_evicts_by_entry_count_and_total_bytes() {
        let dir = test_dir();
        let cache = MediaCache::with_limits(MediaLimits {
            single_max_bytes: DEFAULT_SINGLE_MAX_BYTES,
            total_max_bytes: 600,
            max_entries: 2,
            ..MediaLimits::default()
        });

        let mut paths = Vec::new();
        for i in 0..4 {
            let path = write_png(&dir, &format!("e{i}.png"), 200);
            let url = resolve_image(&path_str(&path), &cache).unwrap();
            assert!(url.len() < 600, "单条须小于总量上限，否则测不到淘汰");
            paths.push(path);
        }

        assert!(cache.len() <= 2, "条目数上界：{} > 2", cache.len());
        assert!(
            cache.total_bytes() <= 600,
            "总字节上界：{} > 600",
            cache.total_bytes()
        );

        // 最后写入的那条必须还在（LRU：刚用过的最不该被淘汰）。
        let last = resolve_image(&path_str(&paths[3]), &cache).unwrap();
        assert!(last.starts_with("data:image/png;base64,"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// 单条本身超过总字节上限 ⇒ 不缓存（否则一进表就被自己挤出去，白付一次淘汰）。
    #[test]
    fn an_entry_larger_than_the_total_budget_is_not_cached() {
        let dir = test_dir();
        let path = write_png(&dir, "big.png", 2000);
        let cache = MediaCache::with_limits(MediaLimits {
            single_max_bytes: DEFAULT_SINGLE_MAX_BYTES,
            total_max_bytes: 100,
            max_entries: 4,
            ..MediaLimits::default()
        });

        let url = resolve_image(&path_str(&path), &cache).unwrap();

        assert!(url.len() > 100);
        assert_eq!(cache.len(), 0, "超总量的条目不得进缓存");
        assert_eq!(cache.total_bytes(), 0);

        let _ = fs::remove_dir_all(&dir);
    }

    /// **锁外 IO**：两个线程同时解析同一路径都能完成（不死锁），允许重复计算。
    ///
    /// 它守的是「不死锁、不悬垂、同键只留一条」；锁**粒度**（锁内不做 IO）由代码
    /// 结构与 §3.2 的注释约束，用例本身分不出来。
    #[test]
    fn concurrent_same_key_requests_both_complete() {
        let dir = test_dir();
        let path = write_png(&dir, "race.png", 900);
        let cache = Arc::new(MediaCache::new());
        let key = path_str(&path);

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let key = key.clone();
                std::thread::spawn(move || resolve_image(&key, &cache))
            })
            .collect();

        let mut urls = Vec::new();
        for handle in handles {
            let url = handle.join().expect("线程不得 panic").unwrap();
            assert!(url.starts_with("data:image/png;base64,"));
            urls.push(url);
        }
        assert_eq!(cache.len(), 1, "同键并发写只应留下一条");
        assert_eq!(
            cache.total_bytes(),
            Arc::clone(&urls[0]).len(),
            "四条并发算出的字节相同，总量只计一份"
        );
        // 所有返回的 data URL 内容相同（谁赢都一样），但只有最后插入的那份留在表里。
        for url in &urls {
            assert_eq!(url.as_ref(), urls[0].as_ref(), "同键并发的结果必须一致");
        }
        assert!(Arc::ptr_eq(&urls[0], &urls[1]), "后到者拿到的是已有的那份");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 编码结果的 `bytes` 与 `wire_len` 在真实读取上一致：拒绝路径报的数字不是估算，
    /// 与实际编码长度逐字节对得上。
    #[test]
    fn reported_bytes_match_the_actual_encoded_length() {
        let dir = test_dir();
        let raw_len = 1500;
        let path = write_png(&dir, "exact.png", raw_len);
        let cache = MediaCache::with_limits(small_limits(10));

        let err = resolve_image(&path_str(&path), &cache).unwrap_err();
        let MediaError::SingleTooLarge { bytes, limit } = err else {
            panic!("期望 SingleTooLarge");
        };
        assert_eq!(limit, 10);
        assert_eq!(bytes, wire_len(raw_len, "image/png"));

        // 真编码一遍对照（小文件，代价可忽略）。
        let mut expected = String::from("data:image/png;base64,");
        STANDARD.encode_string(fs::read(&path).unwrap(), &mut expected);
        assert_eq!(
            bytes,
            expected.len(),
            "拒绝时报的字节数必须与实际编码长度一致"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// **锁边界**（design.md §3.4，承重）：读盘与 base64 编码全部在锁外，锁内只做
    /// 查表与插入。
    ///
    /// 这条纪律靠**代码结构**守：能取到 `MutexGuard` 的只有 [`MediaCache::lookup`] /
    /// [`MediaCache::insert`] / 几个计数器，而它们不碰 `std::fs`。用例把这一点钉死，
    /// 因为把读盘挪进锁内不会让任何行为测试变红（只会在并发看图时变慢）。
    ///
    /// 变异：在 `lookup` 或 `insert` 体内加一句 `fs::metadata("x")` ⇒ 本条变红。
    #[test]
    fn no_filesystem_call_inside_the_lock_holders() {
        fn body(src: &str, name: &str) -> String {
            let start = src
                .find(&format!("fn {name}"))
                .unwrap_or_else(|| panic!("找不到 fn {name}"));
            let rest = &src[start..];
            let open = rest.find('{').expect("函数体应有 '{'");
            let mut depth = 0usize;
            for (i, ch) in rest[open..].char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            return rest[open..open + i + 1].to_owned();
                        }
                    }
                    _ => {}
                }
            }
            panic!("fn {name} 的函数体未闭合")
        }

        let src = include_str!("media.rs");
        for name in ["lookup(", "insert(", "evict("] {
            let body = body(src, name.trim_end_matches('('));
            for bad in ["std::fs", "fs::", "File::", "read_to_end", "base64"] {
                assert!(
                    !body.contains(bad),
                    "`{name}` 是锁内代码，不得出现 `{bad}`（锁内不做 IO）：\n{body}"
                );
            }
        }
    }

    /// 读盘有界 + **超限时报的是整张图的真实尺寸**。
    ///
    /// 2 MiB 的文件、4 KiB 上限：编码前按 `metadata.len()` 预估就判超限 ⇒ 只读嗅探头
    /// 一个缓冲区，不把文件读进来。错误里的 `bytes` 是**整张图**编码后的长度
    /// （2 792 630 B），不是「只读了 4 KiB 所以报 5 484 B」—— 占位文案里的
    /// `(2.7 MiB > 4 KiB)` 必须是模型能据以决策的真实尺寸。
    ///
    /// 变异：删掉编码前那一步预估（只靠编码后的精确判定）⇒ `bytes` 变成截断后的
    /// `5484`，本条变红。
    #[test]
    fn oversized_file_is_rejected_before_encoding_with_its_true_size() {
        let dir = test_dir();
        let path = write_png(&dir, "huge.png", 2 * 1024 * 1024);
        let cache = MediaCache::with_limits(small_limits(4096));

        let err = resolve_image(&path_str(&path), &cache).unwrap_err();

        assert_eq!(
            err,
            MediaError::SingleTooLarge {
                bytes: wire_len(2 * 1024 * 1024, "image/png"),
                limit: 4096
            },
            "必须报整张图的编码后尺寸，而不是只读到的前 4 KiB"
        );
        assert_eq!(cache.len(), 0);

        let _ = fs::remove_dir_all(&dir);
    }
}
