# 截图工具：需求、实测与选型

> 状态：**未裁决** —— 见文末「未澄清问题」。
> 性质：`arch-explore` 产出（现状 + 约束 + 候选），不含方案设计。
> 日期：2026-11-06（本机 macOS 15 / arm64 实测）

---

## 1. 需求有两条，不是一条

`docs/goals.md:18` 的 G2 只要**页面渲染截图**；另有**屏幕捕获**需求（让 agent 自己拍当前屏幕）。
两者能力面不同，不可合并成一个后端：

| | 页面渲染截图 | 屏幕捕获 |
|---|---|---|
| 输入 | HTML 文件 / URL | 无（拍当前屏幕） |
| 后端 | `chrome-headless-shell --screenshot` | `screencapture` / `grim` / `import` |
| 平台差异 | **无**（一条命令两平台通用） | **三套工具 × 三套权限模型** |
| 权限 | 不涉及 | macOS TCC / Wayland portal |
| 无头环境 | ✅ 可用 | ❌ 物理不存在 |
| 目标场景 | G2（`goals.md:18`）、G5 报告（`goals.md:21`、`:128`） | 无对应目标场景 |

`docs/goals.md:90` 估页面截图是 **(M)**；实测后应为 **(S)** —— 见 §2。

---

## 2. 页面渲染截图：实测（结论：S）

```
chrome-headless-shell        1.28s   187 MB   ← 无需 --headless 参数
完整 Chrome.app              5.76s   330 MB   ← 需 --headless
qlmanage -t (macOS 自带)     —        —        ← 固定 800×800 方形缩略图，无视口、无全页
screencapture (macOS 自带)   —        —        ← 屏幕抓取，不是渲染
safaridriver                 —        —        ← /usr/sbin 下不存在（实为 Cryptexes 内）
```

- **两个平台命令完全一致**，故这一项没有平台分支。
- Playwright 已在 `~/Library/Caches/ms-playwright/chromium_headless_shell-*/` 缓存同款二进制,
  **可直接当二进制调用**，不必经 Node API。
- ⚠️ **Chrome 132 已移除 old headless**：用 `chrome-headless-shell` 时不加 `--headless`；
  用完整 Chrome 时至少 `--headless=new`。
- ⚠️ `--window-size` 是固定视口，页面更长会被裁。全页需先取 `scrollHeight` 再重截（两遍）。

**Rust 侧无“自带”方案**：`std` 无 HTML 渲染器。生态两条路都不划算 ——
CDP 类（`chromiumoxide` / `headless_chrome`）**仍需同一份浏览器二进制**，只是多一层依赖；
纯 Rust 渲染器（`blitz` beta、`hyper-render` v0.2.2 仅 813 下载）**不执行 JavaScript**，
对 JS 渲染页面会截出空图。

⇒ 这是 **skill + 脚本**形态，不进 `adapters/tools-basic`（不破 `apps/coding-agent/src/toolset.rs:648-661`
的前缀指纹锚，符合 `goals.md:94` 的边界）。

---

## 3. 屏幕捕获：实测（macOS，本机有权限）

```
screencapture -x -t png     0.12s   3420×2214   1.2 MiB   ✅ 内容正确，可被 view_image 读取
swift -e '...CGPreflightScreenCaptureAccess()'   0.27s   ✅ 权限可预检
```

### 决定性事实：**无权限时静默成功**

`CGPreflightScreenCaptureAccess() == false` 时，`screencapture` 仍 **exit 0 且写出一张图**（壁纸/空白）。
写不进去（只读目录）时**也 exit 0 且无图**。两者都靠退出码漏判 ⇒ **必须自己验后置条件**。

这与本工具要防的失效模式完全一致：模型拿到一张「成功」的假图，却说不出屏幕上有什么。

### 原型实测踩到的 4 个坑（全部为 shell 层）

写 shell 原型时 20 分钟内连踩 4 个，前两个是**静默失败**：

| 坑 | 后果 |
|---|---|
| `case "$(f)" in` 读一个只 `return` 不 `echo` 的函数 | **权限判定整个被静默跳过**，照常出假图 |
| `swift` 实际输出 `true`/`false`（非 `granted`/`denied`） | 永久落到 `*` 分支 |
| 权限函数内全局变量 `out` 覆盖输出路径 `$out` | 截图写到错误路径 |
| `$out（` —— bash 3.2 在 `set -u` 下把中文括号吞进变量名（`out\xef`） | `unbound variable` |

⇒ 这是「该不该从 shell 搬进 Rust」的**依据**，不是偏好：前两条用穷尽 `match` 在类型层就防住了。

---

## 4. 候选后端对比

| 方案 | 形态 | 下载量 | 许可 | 关键问题 |
|---|---|---|---|---|
| **系统命令** | CLI | — | — | 零依赖、可重复构建最好；三平台三套工具、需权限预检 |
| `xcap` v0.9.8 | **库** | 277 万 | Apache-2.0 | ⚠️ 见下 |
| `screencapturekit` v11.1.2 | 库（macOS） | 326 万 | — | 仅 macOS，要 12.3+ |
| `ashpd` v0.14.0 | 库（XDG portal） | 1997 万 | MIT | 仅 Linux，Wayland 正规路线 |
| `libwayshot` v0.9.0 | 库 | 38 万 | — | **仅 wlroots**（Sway 等），GNOME 不支持 |
| `scap` v0.1.0-beta.1 | 库 | 6 万 | — | beta，2025-08 后停更 |

带 CLI 的开源工具都不合用：`flameshot`（GUI 优先）、`scrot`（仅 X11）、`grimshot`（仅 Sway）、
`screen_capture_lite`（C++ 库，无 CLI）。**跨平台 CLI 形态是空白** —— 这一层无论如何要自己写。

### `xcap` 的两个扣分项（均已核源码）

**① 不是 CLI，是纯库**：无 `[[bin]]` 段、无 `src/bin/`；`fn main` 只在 `build.rs` 与 `examples/` 下。
`cargo install xcap` 装不出可执行文件。⇒ 加依赖**不等于**获得工具。

**② 全屏截图不做权限判定**（实测缺口）：

```rust
// src/macos/impl_window.rs:163 —— 只有窗口枚举里检查，且仅 log::warn!
if !CGPreflightScreenCaptureAccess() { log::warn!("Screen Recording permission not granted. ..."); }

// src/macos/impl_monitor.rs:205 —— 全屏截图路径，完全没有检查
pub fn capture_image(&self) -> XCapResult<RgbaImage> { ... }
```

⇒ 无权限时返回 `Ok(RgbaImage)` 的黑图/壁纸图，即**静默失败**。

**③ 构建可重复性**：Linux 依赖锁在 git 分支上，且仓库**无 `Cargo.lock`**
（`Cargo.toml:71`/`:76`/`:87`：`ashpd`、`libwayshot-xcap`、`xcb` 全为 `branch = "main"`；
`xcb` 用分支是为了绕 RUSTSEC-2026-0194/0195）。另需 6 个 Linux `-dev` 包
（`libxcb1-dev` `libxrandr-dev` `libdbus-1-dev` `libpipewire-0.3-dev` `libwayland-dev` `libegl-dev`）。

`xcap` 唯一优势：**一份代码覆盖三平台 API 分支**（xcb / pipewire / objc2 直连，不 spawn 外部命令）。

---

## 5. 倾向（待裁决，不是结论）

`apps/yushan-shot/`（workspace 成员，**不进工具表**）+ `skills/screenshot/` 引导调用：

- `shot page <url|file>` —— `chrome-headless-shell`，与权限无关
- `shot screen` —— 捕获，Rust 侧只负责**权限预检 + 后置校验**（文件存在 / 非空 / 非全黑），
  捕获动作仍 spawn 系统命令 ⇒ 不引 git 依赖，构建可重复

代价：新增 1 个 workspace 成员。不进工具表的理由与页面截图同（前缀锚 + `goals.md:94` 边界）。

---

## 6. 未澄清问题

1. **接受新增 workspace 成员 `apps/yushan-shot/` 吗？** 若否，退路是
   `skills/screenshot/scripts/` 自举（先例：`skills/pdf-analysis/SKILL.md:32` 的 `setup.sh`），
   但那就进不了 `./scripts/par-test.sh` 与变异测试体系 —— 而「无权限不出假图」正是最需要变异测试守住的不变量。
2. **依赖取舍**：只关心 macOS ⇒ 纯 FFI（`objc2-core-graphics`，约 20 行）比 `xcap` 更轻；
   三平台都要 ⇒ `xcap` 省代码，但要接受 §4 的 git 分支依赖与 6 个 `-dev` 包。
3. **屏幕捕获要不要限定平台**？无头环境（SSH / 容器 / CI / Termux）应**明确报错**而非降级；
   是否在文档层面写死「仅桌面环境可用」。
4. **是否与页面截图合成一个 skill**（两个子命令）还是拆两个 —— 影响 `description` 的可写法
   （`apps/coding-agent/src/skills.rs:22`：命令名 = 目录名）。
