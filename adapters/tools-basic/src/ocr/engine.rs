//! OCR 后端的**探测**、argv 构造、候选链，以及结果落盘（设计 `docs/arch/ocr-tool/design.md`
//! §2 / §3.4）。
//!
//! 本模块只做「有哪些后端、怎么叫它、结果写哪」；**不 spawn**（spawn 归 `mod.rs` 走
//! `exec::run_bounded`），**不解析**（解析归 `tsv.rs`）。故它是**同步、零 tokio**的。
//!
//! # 为什么是 `enum Kind` + 穷尽 `match`，不是 `dyn OcrBackend`
//!
//! 判据（design §2.1 补精确，本仓沿用它）：
//!
//! > 差异若落在「有界输入 → 有界输出的纯函数」里，即使代码形状不同，仍按数据差异处理
//! > （表 + 穷尽 `match`）；只有当差异跨出纯函数（持状态 / 需签名 / 需 crate 外实现 /
//! > 需在单次调用内多态派发）时，才升级为 trait。
//!
//! 两个后端的差异全是「名字、argv 形状、`lang` 是否生效」这类**数据**：`match` 漏臂是编译
//! 错误，而 `Vec<Box<dyn>>` 漏填会静默少一个后端。故 `Kind` **刻意不加** `#[non_exhaustive]`
//! —— 本 crate 内的穷尽性是设计要点，加在最内层反而把它挡掉。
//!
//! **加第三后端的改动面**：1 个枚举变体 + 3 个 `match` 臂 + 1 行顺序表。零个新解析函数
//! （线上格式统一为 TSV，见 `tsv.rs` 模块头的列序契约）。
//!
//! # 探测**纯路径扫描**：不 fork、不跑 `--version`
//!
//! 与 [`SearchBackends`](crate::exec::SearchBackends) 同口径（ADR-0015）：只复用
//! `exec::find_in_path` 的目录扫描。macOS 上 `swift` 存在 **≠** Vision 能用（TCC 权限、
//! 沙箱、部分系统版本对部分格式直接失败），而探测无从知道 —— 那正是候选链存在的原因：
//! 「探到的都试一遍，成功即停」（§2.2）。

use std::io;
use std::path::{Path, PathBuf};

use crate::exec::find_in_path;

/// Vision 段的可执行名。真机上是 `swiftc` 编译出的缓存二进制，连同 `swift`（源码直跑）
/// 一并接受：两者都能消费 `[image]` 这一形态的 argv。
const VISION_NAMES: &[&str] = &["vision", "swift"];
/// Tesseract 段的候选可执行名。
const TESSERACT_NAMES: &[&str] = &["tesseract"];

/// macOS 的候选链：Vision 优先（无外部依赖、快一个数量级），Tesseract 兜底
/// （TCC / 解码失败时它仍可能成）。长度 ≤ 2。
const ORDER_MACOS: &[Kind] = &[Kind::Vision, Kind::Tesseract];
/// 其余平台的候选链：只有 Tesseract。
const ORDER_OTHER: &[Kind] = &[Kind::Tesseract];

/// `--psm`：固定 6（「对单行图安全、对多行图不崩」）。`psm 7` 在多行图上**静默吐空**，
/// 是读图工具最坏的失败模式（模型判定「图里没字」而非「参数错了」），故不进参数面。
const PSM: &str = "6";

/// 落盘目录名（`<temp>/yushan-ocr/`）。结果与将来的 `bin/`（`swiftc` 产物）同根：
/// 两者都是「下一轮就可能失效」的派生物，分两个根只会让清理语义分叉。
const DIR_NAME: &str = "yushan-ocr";
/// 落盘文件扩展名（**含点**）。
const SUFFIX: &str = ".txt";
/// `hash` 的 hex 位数（FNV-1a 64 位 = 8 字节 = 16 个 hex 字符）。
const HASH_HEX_LEN: usize = 16;

/// 后端种类。
///
/// **不加 `#[non_exhaustive]`**：本 crate 内三条 `match` 必须穷尽，新增变体时让编译器指出
/// 全部待改点（见模块头）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Tesseract（`tesseract <img> stdout -l <lang> --psm 6 tsv`）。
    Tesseract,
    /// macOS Vision（Swift 侧归一化坐标，吐同列序 TSV）。
    Vision,
}

impl Kind {
    /// 头行 `via=` 的取值 —— 也是引擎名本身（**唯一真值**，`mod.rs` 不得另写一份）。
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Tesseract => "tesseract",
            Kind::Vision => "vision",
        }
    }

    /// 该后端的候选可执行名（探测用）。
    fn names(self) -> &'static [&'static str] {
        match self {
            Kind::Tesseract => TESSERACT_NAMES,
            Kind::Vision => VISION_NAMES,
        }
    }

    /// 平台候选链（**只此一份**，顺序即优先级）。
    fn order() -> &'static [Kind] {
        if cfg!(target_os = "macos") {
            ORDER_MACOS
        } else {
            ORDER_OTHER
        }
    }
}

/// 一次探测的结果：哪些后端可用。
///
/// 是**值**而非 `OnceLock`：注入式构造（[`OcrEngine::detect_from`]）让探测结果在测试内
/// 确定性复现，也让「不装任何引擎的开发机」能跑全部用例（假二进制即可）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OcrEngine {
    /// 已探到的可执行路径。`None` ⇒ 该后端不可用。
    tesseract: Option<PathBuf>,
    vision: Option<PathBuf>,
}

impl OcrEngine {
    /// 按**进程 `PATH`** 探测（生产路径）。
    pub fn detect() -> Self {
        Self::detect_from(&std::env::var("PATH").unwrap_or_default())
    }

    /// 按给定 `PATH` 字符串探测（**注入式**：不读进程 env）。
    ///
    /// **纯路径扫描**：目录顺序即优先级，命中「是文件 + 带可执行位」即返回。
    pub fn detect_from(path: &str) -> Self {
        let dirs: Vec<PathBuf> = std::env::split_paths(path).collect();
        Self {
            tesseract: find_in_path(&dirs, Kind::Tesseract.names()),
            vision: find_in_path(&dirs, Kind::Vision.names()),
        }
    }

    /// 候选链：mac = `[Vision, Tesseract]`，其余 = `[Tesseract]`。
    ///
    /// 只返回**探到的**后端，并保持链序（`mod.rs` 逐候选 spawn，成功即停）。
    pub fn candidates(&self) -> Vec<Kind> {
        Kind::order()
            .iter()
            .copied()
            .filter(|k| self.prog(*k).is_some())
            .collect()
    }

    /// 两平台安装命令提示（候选链为空时用）。
    ///
    /// 两平台**都给**（而不是按 `cfg!` 只给当前平台）：OCR 的失败常常是「换了台机器 /
    /// 远程开发机」，报错正文里同时有 `xcode-select --install` 与 tesseract 安装命令，
    /// 模型和人都无需再追问一次「我这是哪个平台」。
    pub fn install_hint() -> &'static str {
        "install one OCR backend: macOS `xcode-select --install` (enables Vision); \
         Debian/Ubuntu `sudo apt install tesseract-ocr tesseract-ocr-chi-sim`; \
         macOS `brew install tesseract tesseract-lang`"
    }

    /// 构造该后端的 argv（**不含 prog 本体**）。
    ///
    /// - Tesseract：`[image, stdout, -l, lang, --psm, 6, tsv]`
    /// - Vision：`[image]`（语言由系统自动识别，无语言旋钮 —— 故 `lang` **不被使用**，
    ///   调用方据此在头行打 `n/a`，见 [`OcrEngine::lang_affects_cache`]）
    pub fn argv(&self, kind: Kind, image: &Path, lang: &str) -> Vec<String> {
        let image = image.display().to_string();
        match kind {
            // `stdout` **必须紧跟 image**：tesseract 的用法是
            // `tesseract imagename outputbase [options...] [configfile...]`，第二个位置参数
            // 是输出基名 —— 缺了它，`-l` 就被当成文件基名，结果写进 `./-l.tsv`，其余参数
            // 被当成 configfile（stderr 报 `Can't open ...` 但不致命，语言退化成默认），
            // 而 **exit 仍是 0、stdout 全空**。`stdout` 放进 options 之后同样无效。
            Kind::Tesseract => vec![
                image,
                "stdout".to_owned(),
                "-l".to_owned(),
                lang.to_owned(),
                "--psm".to_owned(),
                PSM.to_owned(),
                "tsv".to_owned(),
            ],
            Kind::Vision => vec![image],
        }
    }

    /// 该后端的 prog 路径（`None` ⇒ 不可用）。
    pub fn prog(&self, kind: Kind) -> Option<PathBuf> {
        match kind {
            Kind::Tesseract => self.tesseract.clone(),
            Kind::Vision => self.vision.clone(),
        }
    }

    /// `lang` 是否参与缓存键（Tesseract = `true`，Vision = `false`）。
    ///
    /// Vision 不消费 `lang`，若仍把它算进哈希，同一张图按 `lang` 会落多份**内容完全相同**
    /// 的文件 —— 且模型会误以为参数生效了。
    pub fn lang_affects_cache(kind: Kind) -> bool {
        match kind {
            Kind::Tesseract => true,
            Kind::Vision => false,
        }
    }

    /// 头行里该后端**应显示**的 `lang`（Tesseract = 生效值，Vision = `n/a`）。
    ///
    /// 不打 `n/a` 而回显入参，模型就会以为 `lang` 在 Vision 下生效了。
    pub fn header_lang(kind: Kind, lang: &str) -> &str {
        match kind {
            Kind::Tesseract => lang,
            Kind::Vision => "n/a",
        }
    }
}

/// 落盘。**全量写入**，内容哈希命名，原子写（唯一 tmp 名 + `rename`）。
pub struct Spill;

impl Spill {
    /// OCR 落盘目录：`std::env::temp_dir()/yushan-ocr/`。
    ///
    /// **不建 `YUSHAN_OCR_DIR`**：目录走构造器注入（比 env 强：无 `env_lock`、无并行污染、
    /// 无 `EnvRestore`）。本函数只给**默认值**。
    pub fn dir() -> PathBuf {
        std::env::temp_dir().join(DIR_NAME)
    }

    /// 把 `content` **原样**（不截断）落到 `<ocr_dir>/<stem>-<16hex>.txt`，返回**绝对路径**。
    ///
    /// - **全量**：入参是什么就写什么。调用方若要先截断再写，就会得到一个「模型以为读全了」
    ///   的半个文件 —— 截断只允许发生在**回显**上，不发生在写入上。
    /// - **幂等**：hash 取自 `image_bytes ‖ lang ‖ granularity`，同图同参 ⇒ 同名 ⇒ `rename`
    ///   覆盖旧文件。Vision 下 `lang` 不参与（[`OcrEngine::lang_affects_cache`]）。
    /// - **原子**：先写唯一 tmp（含 `pid` + 进程内原子序号，并行安全），再 `rename`。
    ///   读者永不见半个文件 —— 这也是 `is_concurrency_safe = true` 的前提。
    /// - 目录不存在就建：系统清理临时目录后工具不该永久失效。
    pub fn write(
        ocr_dir: &Path,
        stem: &str,
        image: &Path,
        lang: &str,
        kind: Kind,
        word_boxes: bool,
        content: &str,
    ) -> io::Result<PathBuf> {
        std::fs::create_dir_all(ocr_dir)?;

        let name = file_name(stem, image, lang, kind, word_boxes);
        let target = ocr_dir.join(&name);
        let tmp = unique_tmp_path(ocr_dir, &name);

        if let Err(e) = std::fs::write(&tmp, content) {
            // tmp 可能已经建出来了一半：best-effort 清掉，别给目录留垃圾。
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(&tmp, &target) {
            // 同上：rename 失败时 tmp 还在（比如跨设备挂载）。
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }

        Ok(target)
    }
}

/// `<stem>-<16hex>.txt` —— 落盘文件名的**唯一定义**。
///
/// hash 的对象是 `image_bytes ‖ lang ‖ granularity`：图像**内容**而非路径（同一张图换个
/// 文件名不该多一份），`lang` 只在它真正生效的后端上参与，`granularity` 区分
/// 行级 / 词级（同一张图的两种输出**内容不同**，绝不能共用一个文件）。
///
/// `lang` 与 `granularity` 之间插 `\0` 分隔：否则 `lang="a", word_boxes=true` 与
/// `lang="a\0w"` 这类拼接会撞到同一个字节流。
fn file_name(stem: &str, image: &Path, lang: &str, kind: Kind, word_boxes: bool) -> String {
    let mut buf = image_bytes(image);
    if OcrEngine::lang_affects_cache(kind) {
        buf.extend_from_slice(lang.as_bytes());
    }
    buf.push(0);
    buf.push(if word_boxes { b'w' } else { b'l' });

    format!(
        "{stem}-{:0width$x}{SUFFIX}",
        fnv1a64(&buf),
        width = HASH_HEX_LEN
    )
}

/// 读图像内容用于哈希。
///
/// 读不到（文件在校验与落盘之间被删 / 权限变了）时**退化为空字节**而非报错：落盘是结果的
/// 投递方式，它不该因为「哈希算不出来」而失败 —— 此时所有同类失败共用一个名字，覆盖写仍然
/// 安全（内容就是结果本身）。目录名与空图不会撞车，因为 stem 里带图名。
fn image_bytes(image: &Path) -> Vec<u8> {
    std::fs::read(image).unwrap_or_default()
}

/// FNV-1a 64 位。**手写**，与 `tools-web::store::fnv1a64` 同算法、同理由：
/// `std` 的 `DefaultHasher` 输出**跨 Rust 版本不保证稳定**（实现允许变、也确实变过）——
/// 同一张图在升级工具链后会落到**另一个文件**，目录里堆满「同一张图的另一份」。
///
/// 这是本仓**唯一允许的重复实现**：`tools-web` 是另一个 crate，跨 crate 复用它会给
/// `tools-basic` 引入反向依赖。8 行、零依赖、永久稳定，比一条错的依赖边便宜。
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

/// 唯一 tmp 路径：`<name>.<pid>.<进程内原子序号>.tmp`。
///
/// pid + 原子序号两者都要：多实例（不同 pid）与并行测试（同 pid 多线程）都会同时写同名
/// 目标 —— 共用一个 tmp 名会互相截断，或让 `rename` 读到半个文件。
fn unique_tmp_path(dir: &Path, name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("{name}.{}.{seq}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一次性临时目录（名字含 `pid` + 原子序号，并行安全）。
    fn dir(tag: &str) -> PathBuf {
        let dir = crate::test_support::test_dir(&format!("ocr_engine_{tag}"));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 写一张内容任意的「图」——哈希只读字节，不看扩展名。
    fn image(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("写假图");
        path
    }

    // ── 探测 ──

    #[test]
    fn file_name_shape_is_stem_plus_16hex() {
        let name = file_name(
            "shot",
            Path::new("/nope.png"),
            "eng",
            Kind::Tesseract,
            false,
        );
        let (stem, rest) = name.rsplit_once('-').expect("必有 hash 段");
        assert_eq!(stem, "shot");
        let hash = rest.strip_suffix(SUFFIX).expect("必有扩展名");
        assert_eq!(hash.len(), HASH_HEX_LEN);
        assert!(
            hash.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }

    #[test]
    fn detect_from_empty_path_has_no_candidates() {
        let empty = dir("empty_path");
        let engine = OcrEngine::detect_from(&empty.display().to_string());
        assert_eq!(engine.candidates(), Vec::new());
        // 链空 ⇒ 调用方必须能拿到两平台的安装命令。
        assert!(OcrEngine::install_hint().contains("xcode-select --install"));
        assert!(OcrEngine::install_hint().contains("tesseract"));
        std::fs::remove_dir_all(&empty).ok();
    }

    #[cfg(unix)]
    #[test]
    fn detect_from_finds_tesseract() {
        let bin = dir("tess");
        crate::test_support::fake_bin(&bin, "tesseract", "exit 0");

        let engine = OcrEngine::detect_from(&bin.display().to_string());
        assert!(
            engine.candidates().contains(&Kind::Tesseract),
            "探到的链 {:?} 应含 Tesseract",
            engine.candidates()
        );
        assert_eq!(engine.prog(Kind::Tesseract), Some(bin.join("tesseract")));
        std::fs::remove_dir_all(&bin).ok();
    }

    /// 探测是**纯路径扫描**：探到假二进制的存在，但**绝不执行它**。
    #[cfg(unix)]
    #[test]
    fn detect_from_does_not_execute_the_backend() {
        let bin = dir("no_fork");
        let marker = bin.join("ran");
        crate::test_support::fake_bin(&bin, "tesseract", &format!("touch '{}'", marker.display()));

        let engine = OcrEngine::detect_from(&bin.display().to_string());
        assert!(engine.candidates().contains(&Kind::Tesseract));
        assert!(
            !marker.exists(),
            "探测不应 fork 假二进制（--version 也不行）"
        );
        std::fs::remove_dir_all(&bin).ok();
    }

    // ── argv ──

    /// tesseract 的 argv 是**固定形态**（`--psm 6`、`tsv` 收尾不可漂移）。
    ///
    /// `stdout` 必须紧跟 image：第二个位置参数是 outputbase，缺了它结果会写进文件而
    /// stdout 全空（见 [`OcrEngine::argv`] 的注释）。
    #[test]
    fn tesseract_argv_is_fixed_shape() {
        let engine = OcrEngine::detect_from("");
        assert_eq!(
            engine.argv(Kind::Tesseract, Path::new("/tmp/shot.png"), "chi_sim+eng"),
            vec![
                "/tmp/shot.png",
                "stdout",
                "-l",
                "chi_sim+eng",
                "--psm",
                "6",
                "tsv"
            ]
        );
    }

    /// 回归：`stdout` 必须在 `-l` **之前**、且**紧邻** image —— 少写一个位置参数就整条失效。
    ///
    /// 少了它 ⇒ tesseract 把 `-l` 当 outputbase，结果写进 `./-l.tsv`、stdout 全空、exit 0
    /// （静默失败）；挪到 options 之后 ⇒ 同样无效。两种都不会被退出码发现。
    #[test]
    fn tesseract_stdout_is_the_second_positional() {
        let engine = OcrEngine::detect_from("");
        let argv = engine.argv(Kind::Tesseract, Path::new("/tmp/shot.png"), "eng");

        assert_eq!(argv.first().map(String::as_str), Some("/tmp/shot.png"));
        assert_eq!(
            argv.get(1).map(String::as_str),
            Some("stdout"),
            "第二个位置参数必须是 stdout（outputbase）：{argv:?}"
        );
        let stdout_at = argv
            .iter()
            .position(|a| a == "stdout")
            .unwrap_or_else(|| panic!("argv 里没有 stdout：{argv:?}"));
        let lang_at = argv
            .iter()
            .position(|a| a == "-l")
            .unwrap_or_else(|| panic!("argv 里没有 -l：{argv:?}"));
        assert!(stdout_at < lang_at, "stdout 必须出现在 -l 之前：{argv:?}");
    }

    #[test]
    fn vision_argv_ignores_lang() {
        let engine = OcrEngine::detect_from("");
        assert_eq!(
            engine.argv(Kind::Vision, Path::new("/tmp/shot.png"), "eng"),
            vec!["/tmp/shot.png"]
        );
    }

    // ── lang 作用域（§2.3 三处必须一致）──

    #[test]
    fn lang_scope_three_places() {
        assert!(OcrEngine::lang_affects_cache(Kind::Tesseract));
        assert!(!OcrEngine::lang_affects_cache(Kind::Vision));
        assert_eq!(OcrEngine::header_lang(Kind::Tesseract, "eng"), "eng");
        assert_eq!(OcrEngine::header_lang(Kind::Vision, "eng"), "n/a");
    }

    // ── 落盘 ──

    #[test]
    fn write_is_idempotent_for_same_inputs() {
        let d = dir("idem");
        let img = image(&d, "shot.png", b"not really a png");

        let a = Spill::write(&d, "shot", &img, "chi_sim+eng", Kind::Tesseract, false, "x")
            .expect("第一次落盘");
        let b = Spill::write(&d, "shot", &img, "chi_sim+eng", Kind::Tesseract, false, "y")
            .expect("第二次落盘");
        assert_eq!(a, b, "同图同参应落同一个文件（覆盖写）");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn lang_changes_path_for_tesseract_but_not_for_vision() {
        let d = dir("lang_scope");
        let img = image(&d, "shot.png", b"bytes");

        let t_eng = Spill::write(&d, "shot", &img, "eng", Kind::Tesseract, false, "x").unwrap();
        let t_chi = Spill::write(&d, "shot", &img, "chi_sim", Kind::Tesseract, false, "x").unwrap();
        assert_ne!(t_eng, t_chi, "Tesseract 下 lang 进哈希");

        let v_eng = Spill::write(&d, "shot", &img, "eng", Kind::Vision, false, "x").unwrap();
        let v_chi = Spill::write(&d, "shot", &img, "chi_sim", Kind::Vision, false, "x").unwrap();
        assert_eq!(v_eng, v_chi, "Vision 不消费 lang ⇒ 不得按 lang 落多份");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn granularity_is_part_of_the_hash() {
        let d = dir("granularity");
        let img = image(&d, "shot.png", b"bytes");

        let line = Spill::write(&d, "shot", &img, "eng", Kind::Tesseract, false, "l").unwrap();
        let word = Spill::write(&d, "shot", &img, "eng", Kind::Tesseract, true, "w").unwrap();
        assert_ne!(line, word, "行级与词级内容不同，绝不能共用一个文件");
        std::fs::remove_dir_all(&d).ok();
    }

    /// 全量写：**逐字节**等于入参，包括末尾无换行与含换行的两种形态。
    #[test]
    fn write_preserves_content_byte_for_byte() {
        let d = dir("verbatim");
        let img = image(&d, "shot.png", b"bytes");

        for (i, content) in ["# no trailing newline", "# with one\n"].iter().enumerate() {
            let lang = format!("lang{i}");
            let path = Spill::write(&d, "shot", &img, &lang, Kind::Tesseract, false, content)
                .expect("落盘");
            let read = std::fs::read(&path).expect("读回");
            assert_eq!(read, content.as_bytes(), "文件内容必须逐字节等于入参");
        }
        std::fs::remove_dir_all(&d).ok();
    }

    /// 原子写：写完即可完整读到，且目录里不留 `*.tmp`。
    #[test]
    fn write_leaves_no_partial_file() {
        let d = dir("atomic");
        let img = image(&d, "shot.png", b"bytes");
        let content = "1 (0,0,10,10) conf_min:90 hello\n2 (0,20,10,10) conf_min:80 world\n";

        let path = Spill::write(&d, "shot", &img, "eng", Kind::Tesseract, true, content).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        assert!(path.is_absolute(), "回给模型的必须是绝对路径：{path:?}");

        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不该留 tmp 残留：{leftovers:?}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn write_creates_missing_dir() {
        let root = dir("mkdir");
        let img = image(&root, "shot.png", b"bytes");
        let ocr_dir = root.join("nested/ocr");
        assert!(!ocr_dir.exists());

        let path =
            Spill::write(&ocr_dir, "shot", &img, "eng", Kind::Tesseract, false, "x").unwrap();
        assert!(path.is_file());
        std::fs::remove_dir_all(&root).ok();
    }
}
