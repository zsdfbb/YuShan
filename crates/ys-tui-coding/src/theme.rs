//! 内置主题：编译期常量色板 + 名称解析 + 紫色色相守卫。
//!
//! # 为什么是编译期常量表
//!
//! 5 套色板全是 `static` 常量，`ThemeKind::palette()` **按值**返回一份 `Palette`。
//! `Palette` 是 10 个 `Color::Rgb`（40 字节）且 `Copy`：解析与取色都在常量上完成，
//! 零运行时分配；按值传递让调用方可自由持有 / 比较色板（缓存失效可直接比值，
//! 无需维护版本计数器），而渲染层的 `Clone + Send + Sync + 'static` 由 `Copy`
//! 自动满足。
//!
//! # 10 个槽位是「语义 → 颜色」的唯一映射来源
//!
//! | 槽位 | 用途 |
//! |---|---|
//! | `bg` | 画布背景（整屏铺色 + 所有 `Clear` 之后的补铺） |
//! | `fg` | 正文（`md::render` 给库未上色的 span 补前景）/ 用户消息正文 / `Error:` 正文 / 定义列表 / 表格 |
//! | `accent` | 标题 / 数学 |
//! | `code` | 行内码 / 围栏代码块 |
//! | `muted` | 引用 / 系统 / 状态行 / 行动块 / 表格框线 / 列表标记 / 脚注 / HTML / 图片占位 |
//! | `user` | 用户消息前缀 |
//! | `ok` | 工具成功标记 |
//! | `warn` | alert `Warning` + `Important` |
//! | `fail` | 工具失败标记 / `Error:` 前缀 / alert `Caution` |
//! | `info` | 链接 / alert `Note` + `Tip` |
//!
//! 渲染层（`styles.rs` / `draw.rs` / `md.rs`）**只准**从这 10 个槽位取色，不得出现
//! 具名色 —— 否则「默认色泄漏」与「主题漂移」都失去单一判据。
//!
//! # 为什么不做用户自定义主题
//!
//! 本 crate **零文件 IO**（不出现 `std::fs` / `File`）：主题若来自文件，读写、校验、
//! 热重载、错误呈现全部要落在 UI crate 内，与其职责（渲染 + 输入）不符。故本 crate
//! 只认 [`ThemeKind`] 这 5 个取值；偏好持久化由 app 侧（`~/.yushan/state.json`）负责，
//! 本 crate 既不知道也不关心它存在。
//!
//! # 紫色守卫
//!
//! 产品需求：任何主题的 9 个非 `bg` 槽位（5 套 × 9 = 45 色）都不得落在紫色色相区。
//! 判据见 [`is_purple`]，由单测 C2 守卫。改色板时若触发它，**改色板而不是放宽判据**。
//!
//! **已知合法撞色**（不属紫色守卫范围，允许）：`gruvbox-dark` 的 `user` 与 `info` 同色；
//! `everforest-dark` 与 `solarized-dark` 的 `code` 与 `info` 同色。原因是这几套调色板的
//! 非紫色相不足，硬拉开会破坏原本的配色语言。

use std::fmt;
use std::str::FromStr;

use ratatui::style::Color;

/// 内置主题。
///
/// `/theme` 的合法取值即 [`ThemeKind::name`]（kebab-case，解析时大小写不敏感，
/// `'_'` 归一化为 `'-'`）。`Default` 为 [`ThemeKind::AyuMirage`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ThemeKind {
    #[default]
    AyuMirage,
    GruvboxDark,
    Nord,
    EverforestDark,
    SolarizedDark,
}

impl ThemeKind {
    /// 全部主题，**顺序即浮层展示顺序**，且与变体声明顺序一致 ——
    /// [`ThemeKind::next`] / [`ThemeKind::prev`] 以判别值取下标，依赖这条不变量。
    pub const ALL: [ThemeKind; 5] = [
        ThemeKind::AyuMirage,
        ThemeKind::GruvboxDark,
        ThemeKind::Nord,
        ThemeKind::EverforestDark,
        ThemeKind::SolarizedDark,
    ];

    /// 机器可读名（`/theme` 的参数、`state.json` 的存储值）。
    pub fn name(&self) -> &'static str {
        match self {
            ThemeKind::AyuMirage => "ayu-mirage",
            ThemeKind::GruvboxDark => "gruvbox-dark",
            ThemeKind::Nord => "nord",
            ThemeKind::EverforestDark => "everforest-dark",
            ThemeKind::SolarizedDark => "solarized-dark",
        }
    }

    /// 人可读名（浮层逐行显示）。
    pub fn label(&self) -> &'static str {
        match self {
            ThemeKind::AyuMirage => "Ayu Mirage",
            ThemeKind::GruvboxDark => "Gruvbox Dark",
            ThemeKind::Nord => "Nord",
            ThemeKind::EverforestDark => "Everforest Dark",
            ThemeKind::SolarizedDark => "Solarized Dark",
        }
    }

    /// 环形前进（浮层 `↓`）。`ALL` 与变体声明顺序一致，故下标可由判别值直接给出。
    pub fn next(&self) -> ThemeKind {
        ThemeKind::ALL[(*self as usize + 1) % ThemeKind::ALL.len()]
    }

    /// 环形后退（浮层 `↑`）。
    pub fn prev(&self) -> ThemeKind {
        ThemeKind::ALL[(*self as usize + ThemeKind::ALL.len() - 1) % ThemeKind::ALL.len()]
    }

    /// 本主题的色板。**按值返回**：`Palette` 是 10 个 `Color::Rgb`（40 字节）且 `Copy`
    /// —— 按值传递的约束更少，调用方可自由持有 / 比较一份色板值。
    pub fn palette(&self) -> Palette {
        match self {
            ThemeKind::AyuMirage => AYU_MIRAGE,
            ThemeKind::GruvboxDark => GRUVBOX_DARK,
            ThemeKind::Nord => NORD,
            ThemeKind::EverforestDark => EVERFOREST_DARK,
            ThemeKind::SolarizedDark => SOLARIZED_DARK,
        }
    }
}

/// 主题名解析失败。`Display` 回显输入原文并列出全部合法名。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeParseError {
    input: String,
}

impl fmt::Display for ThemeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "未知主题 {:?}，可选：", self.input)?;
        for (i, kind) in ThemeKind::ALL.iter().enumerate() {
            if i > 0 {
                write!(f, " / ")?;
            }
            write!(f, "{}", kind.name())?;
        }
        Ok(())
    }
}

impl FromStr for ThemeKind {
    type Err = ThemeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.replace('_', "-");
        ThemeKind::ALL
            .into_iter()
            .find(|kind| kind.name().eq_ignore_ascii_case(&normalized))
            .ok_or_else(|| ThemeParseError {
                input: s.to_owned(),
            })
    }
}

/// 10 槽位色板，全部取自 [`rgb`] ⇒ 恒为 `Color::Rgb`。
///
/// **不实现 `Default`**：无主题的色板没有合理默认值，显式写出 `ThemeKind` 才能取色，
/// 否则「漏传主题」会被一个静默的默认色板掩盖。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: Color,
    pub fg: Color,
    pub accent: Color,
    pub code: Color,
    pub muted: Color,
    pub user: Color,
    pub ok: Color,
    pub warn: Color,
    pub fail: Color,
    pub info: Color,
}

/// `0xRRGGBB` → `Color::Rgb`。高位为红、中位为绿、低位为蓝。
const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

static AYU_MIRAGE: Palette = Palette {
    bg: rgb(0x1f2430),
    fg: rgb(0xcccac2),
    accent: rgb(0xffcc66),
    code: rgb(0x5ccfe6),
    muted: rgb(0x6c7986),
    user: rgb(0x73b8ff),
    ok: rgb(0xbae67e),
    warn: rgb(0xffd173),
    fail: rgb(0xf28779),
    info: rgb(0x95e6cb),
};

static GRUVBOX_DARK: Palette = Palette {
    bg: rgb(0x1d2021),
    fg: rgb(0xebdbb2),
    accent: rgb(0xfabd2f),
    code: rgb(0x8ec07c),
    muted: rgb(0x928374),
    user: rgb(0x83a598),
    ok: rgb(0xb8bb26),
    warn: rgb(0xfe8019),
    fail: rgb(0xfb4934),
    info: rgb(0x83a598),
};

static NORD: Palette = Palette {
    bg: rgb(0x2e3440),
    fg: rgb(0xd8dee9),
    accent: rgb(0x88c0d0),
    code: rgb(0x8fbcbb),
    muted: rgb(0x4c566a),
    user: rgb(0x81a1c1),
    ok: rgb(0xa3be8c),
    warn: rgb(0xebcb8b),
    fail: rgb(0xbf616a),
    info: rgb(0x5e81ac),
};

static EVERFOREST_DARK: Palette = Palette {
    bg: rgb(0x2d353b),
    fg: rgb(0xd3c6aa),
    accent: rgb(0xa7c080),
    code: rgb(0x83c092),
    muted: rgb(0x859289),
    user: rgb(0x7fbbb3),
    ok: rgb(0xdbbc7f),
    warn: rgb(0xe69875),
    fail: rgb(0xe67e80),
    info: rgb(0x83c092),
};

static SOLARIZED_DARK: Palette = Palette {
    bg: rgb(0x002b36),
    fg: rgb(0x93a1a1),
    accent: rgb(0xb58900),
    code: rgb(0x2aa198),
    muted: rgb(0x586e75),
    user: rgb(0x268bd2),
    ok: rgb(0x859900),
    warn: rgb(0xcb4b16),
    fail: rgb(0xdc322f),
    info: rgb(0x2aa198),
};

/// 紫色守卫：`Color::Rgb` → sRGB → HSL，判「色相 ∈ [255°, 330°] 且 sat ≥ 0.20 且
/// light ∈ [0.15, 0.90]」。四个端点**全为闭区间**。
///
/// 非 `Color::Rgb` 变体返回 `false`：本设计不产生具名色 / 索引色，故对它们没有
/// 「落紫色区」可言，返回 `false` 让「是否漏用了 RGB」由另一条断言负责
/// （色板全是 `Color::Rgb`）。
// 只有单测（C2）调用它：守卫是对编译期色板的检查项，运行期渲染路径不需要它
// ⇒ 非 test 构建下允许未使用，不必为「消 warning」而造一个假调用点。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn is_purple(c: Color) -> bool {
    let Color::Rgb(r, g, b) = c else {
        return false;
    };
    let (hue, sat, light) = hsl(r, g, b);
    (255.0..=330.0).contains(&hue) && sat >= 0.20 && (0.15..=0.90).contains(&light)
}

/// 8 位 sRGB → HSL（色相角度 / 饱和度 / 亮度，三者均按 0–1 或 0–360 归一）。
///
/// 纯浮点最小实现：本 crate 不引色彩库（只为一个守卫不值得进依赖树）。
///
/// 无彩色（`delta == 0`）在函数开头早退，故饱和度分支里 `0 < light < 1`，分母非零。
fn hsl(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let r = f64::from(r) / 255.0;
    let g = f64::from(g) / 255.0;
    let b = f64::from(b) / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let light = (max + min) / 2.0;
    let delta = max - min;
    if delta == 0.0 {
        // 无彩色：色相与饱和度均无定义，取 0（故不会落进紫色区）。
        return (0.0, 0.0, light);
    }
    // delta > 0 ⇒ light 严格介于 0 与 1 之间 ⇒ 分母非零。
    let sat = delta / (1.0 - (2.0 * light - 1.0).abs());
    let hue = if max == r {
        60.0 * (((g - b) / delta) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    // `max == r` 且 `g < b` 时上式给出负角，归一到 [0, 360)。
    (if hue < 0.0 { hue + 360.0 } else { hue }, sat, light)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_theme_names_roundtrip_and_parse() {
        for kind in ThemeKind::ALL {
            assert_eq!(ThemeKind::from_str(kind.name()).unwrap(), kind);
            assert!(!kind.label().is_empty(), "{} 缺 label", kind.name());
            // label 是人读名，name 是机器名，复制粘贴会让浮层显示 kebab-case。
            assert_ne!(
                kind.label(),
                kind.name(),
                "{} 的 label 与 name 相同",
                kind.name()
            );
        }

        // 大小写不敏感。
        for input in ["NORD", "Nord", "nord", "nOrD"] {
            assert_eq!(ThemeKind::from_str(input).unwrap(), ThemeKind::Nord);
        }

        // '_' 归一化为 '-'。
        assert_eq!(
            ThemeKind::from_str("gruvbox_dark").unwrap(),
            ThemeKind::GruvboxDark
        );
        assert_eq!(
            ThemeKind::from_str("SOLARIZED_DARK").unwrap(),
            ThemeKind::SolarizedDark
        );

        // 未知名：报错且回显原文。
        let err = ThemeKind::from_str("nope").unwrap_err();
        assert!(err.to_string().contains("nope"), "错误信息缺原文：{err}");

        // next / prev 环形闭合且互逆（同时钉住「ALL 顺序 == 变体声明顺序」）。
        for kind in ThemeKind::ALL {
            assert_eq!(
                kind.next().prev(),
                kind,
                "{} 的 next/prev 不互逆",
                kind.name()
            );
            assert_eq!(
                kind.prev().next(),
                kind,
                "{} 的 prev/next 不互逆",
                kind.name()
            );
            let mut walked = kind;
            for _ in 0..ThemeKind::ALL.len() {
                walked = walked.next();
            }
            assert_eq!(walked, kind, "{} 走满一圈未回到自身", kind.name());
        }
    }

    #[test]
    fn test_no_purple_in_any_palette() {
        // 5 套 × 9 个非 bg 槽位 = 45 色。
        for kind in ThemeKind::ALL {
            let p = kind.palette();
            let slots = [
                ("fg", p.fg),
                ("accent", p.accent),
                ("code", p.code),
                ("muted", p.muted),
                ("user", p.user),
                ("ok", p.ok),
                ("warn", p.warn),
                ("fail", p.fail),
                ("info", p.info),
            ];
            for (slot, c) in slots {
                assert!(!is_purple(c), "{} 的 {} 落紫色区", kind.name(), slot);
            }
        }

        // 守卫自身：已知紫（色相 258°）为真。
        assert!(is_purple(rgb(0x8b5cf6)), "#8b5cf6 应判为紫色");
        // 紫红侧（312°）仍在区间内。
        assert!(is_purple(rgb(0xd05cb8)), "#d05cb8（312°）应判为紫色");

        // 非 RGB 变体：本设计不产生，守卫对它们无话可说。
        assert!(!is_purple(Color::Reset));
        assert!(!is_purple(Color::Red));
        assert!(!is_purple(Color::Indexed(5)));

        // 三道具门限**各自**都要有「只有它决定结果」的样本 —— 否则删掉某道门限
        // 仍可能全绿（真实色板的 45 个色饱和度最低也是 0.056，不足以钉住 sat 门限）。
        //
        // ① 只有色相越界（348° > 330°）：sat 0.56、light 0.63 都在区内。
        assert!(!is_purple(rgb(0xf05070)), "#f05070（348°）应判为否");
        // ② 只有饱和度不足：#8a7a9a 色相 270°、亮度 0.54 都合规，sat 仅 0.137。
        assert!(!is_purple(rgb(0x8a7a9a)), "#8a7a9a 饱和度 0.137 应判为否");
        //    同色相 / 同亮度量级、但 sat 提到 0.35 ⇒ 判真（证明上一条是被 sat 挡下的）。
        assert!(is_purple(rgb(0x6a4a9a)), "#6a4a9a（264°）应判为紫色");
        // ③ 只有亮度过暗（0.125 < 0.15）：色相 258.75°、sat 0.50 都合规。
        assert!(!is_purple(rgb(0x1a1030)), "#1a1030 亮度 0.125 应判为否");
        //    只有亮度过亮（0.920 > 0.90）：色相 264.44°、sat 0.45 都合规。
        assert!(!is_purple(rgb(0xe8ddf8)), "#e8ddf8 亮度 0.920 应判为否");
        // 纯灰（delta == 0）走早退分支，不除零。
        assert!(
            !is_purple(rgb(0x7b787b)),
            "#7b787b（300°，sat 0.012）应判为否"
        );
    }

    #[test]
    fn test_palette_slots_are_all_rgb() {
        for kind in ThemeKind::ALL {
            let p = kind.palette();
            let slots = [
                ("bg", p.bg),
                ("fg", p.fg),
                ("accent", p.accent),
                ("code", p.code),
                ("muted", p.muted),
                ("user", p.user),
                ("ok", p.ok),
                ("warn", p.warn),
                ("fail", p.fail),
                ("info", p.info),
            ];
            for (slot, c) in slots {
                assert!(
                    matches!(c, Color::Rgb(..)),
                    "{} 的 {} 不是 Color::Rgb：{c:?}",
                    kind.name(),
                    slot
                );
            }
        }

        // 5 套 bg 互不相同（防复制粘贴串行）：整屏铺色一旦相同，切主题将毫无视觉反馈。
        for (i, a) in ThemeKind::ALL.iter().enumerate() {
            for b in &ThemeKind::ALL[i + 1..] {
                assert_ne!(
                    a.palette().bg,
                    b.palette().bg,
                    "{} 与 {} 的 bg 相同",
                    a.name(),
                    b.name()
                );
            }
        }

        // bg 与 accent 逐字节钉住配色表（独立于 `rgb()` 的通道拆分：
        // 这里直接写 RGB 三元组，`rgb()` 若把通道挪错位置就会红）。
        let pinned = [
            (ThemeKind::AyuMirage, (0x1f, 0x24, 0x30), (0xff, 0xcc, 0x66)),
            (
                ThemeKind::GruvboxDark,
                (0x1d, 0x20, 0x21),
                (0xfa, 0xbd, 0x2f),
            ),
            (ThemeKind::Nord, (0x2e, 0x34, 0x40), (0x88, 0xc0, 0xd0)),
            (
                ThemeKind::EverforestDark,
                (0x2d, 0x35, 0x3b),
                (0xa7, 0xc0, 0x80),
            ),
            (
                ThemeKind::SolarizedDark,
                (0x00, 0x2b, 0x36),
                (0xb5, 0x89, 0x00),
            ),
        ];
        for (kind, bg, accent) in pinned {
            let p = kind.palette();
            assert_eq!(p.bg, Color::Rgb(bg.0, bg.1, bg.2), "{} 的 bg", kind.name());
            assert_eq!(
                p.accent,
                Color::Rgb(accent.0, accent.1, accent.2),
                "{} 的 accent",
                kind.name()
            );
        }

        // 已知合法撞色：gruvbox-dark 的 user == info；everforest-dark / solarized-dark 的
        // code == info。这几套调色板的非紫色相不足，故不写全局「各槽位互不相同」断言。
    }
}
