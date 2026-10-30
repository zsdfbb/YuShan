# ys-coding-agent

> 仓库里**唯一**的可执行程序：把 `ys-*` 那套运行时组装成一个能日常用的 coding agent。
> 架构与分层见 [`docs/architecture.md`](../../docs/architecture.md)，仓库总览见 [根 `README.md`](../../README.md)。

本文件只讲**怎么用**：怎么启动、三种模式的区别、TUI 里有哪些命令和键位、文件落在哪。
想改它（加工具 / 加命令 / 动协议）看 [`CONTRIBUTING.md`](../../CONTRIBUTING.md)。

## 目录

- [构建与启动](#构建与启动)
- [三种模式](#三种模式)
- [首次使用：登录与选模型](#首次使用登录与选模型)
- [TUI 内置命令](#tui-内置命令)
- [TUI 键位](#tui-键位)
- [CLI 参数](#cli-参数)
- [worktree 子命令](#worktree-子命令)
- [环境变量](#环境变量)
- [文件与目录](#文件与目录)
- [退出码与输出约定](#退出码与输出约定)
- [常见问题](#常见问题)

## 构建与启动

```console
$ ./scripts/cargo-build.sh build --release     # 别直接用 cargo build，见根 CONTRIBUTING「环境」
$ ./target/release/ys-coding-agent             # 交互式 TUI（默认）
```

开发期也可以 `cargo run -p ys-coding-agent`（同上，官方入口是脚本）。
需要 Rust 1.88+（edition 2024）。

**工作区是当前目录**：所有文件工具（`read` / `write` / `edit` / `bash` / `grep` / `find` / `ls`）
都以启动时的 CWD 为根；project 级 skill 也从 `<cwd>/.yushan/skills` 扫。

## 三种模式

| 模式 | 启动方式 | stdout 是什么 | 适合 |
|---|---|---|---|
| **TUI** | `ys-coding-agent` | 无（画面走 alt-screen） | 交互干活、看过程、翻历史 |
| **Print** | `ys-coding-agent -p "<任务>"` | 模型正文的**文本增量**，边生成边打印 | 一次性任务、管道、肉眼观察 |
| **JSON** | `ys-coding-agent --json "<任务>"` | 每行一个 `Envelope` JSON | 给别的程序吃事件流 |

```console
$ ys-coding-agent                                   # 交互
$ ys-coding-agent -p "解释这个仓库的架构"            # 单次
$ ys-coding-agent --json "跑一遍测试" | jq .event    # 事件流
$ ys-coding-agent --json --stats "..." 2>stats.log  # 背压统计单独进 stderr
```

- **TUI 只在终端里起**：`stdin` / `stdout` 不是终端（CI、管道、重定向）时不会启动 ——
  命令照常执行完并把结果打到 stderr，只是没有界面。
- `-p` 与 `--json` 都是**一次性单回合**，跑完即退出。裸 `--resume`（交互式列表选择）
  在管道下会打印候选后要求改用 `--resume <id>`；带 id 的形态可以照常用于脚本。

## 首次使用：登录与选模型

首次启动没有凭证也能进界面，状态行会显示未配置模型。在 TUI 里：

```
/login     选 provider（deepseek / minimax / opencode-go / glm / glm-coding / mimo 有内置端点；
           custom 这类没内置 base 的会再问一句 URL）→ 填 API key
/model     无参开浮层选模型；也可 /model <name> 直接点名
```

`glm`（标准 API）与 `glm-coding`（Coding Plan 专属端点）是**两个独立 provider**：
端点不同、API key **不通用**，各登各的。两者都没有模型列表接口，模型名来自内置种子
（`glm-5.3` / `glm-5.3-flash`），登录过程不发网络请求。

`mimo`（小米 MiMo）有模型列表接口，但那个目录把**语音模型排在最前**（`mimo-v2.5-asr` 等，
在 chat 端点上必然报错）⇒ 本仓按能力类型过滤，目录里只剩 chat 模型
（`mimo-v2.6-flash` / `mimo-v2.6-pro`）。详见 `docs/design-final/mimo.md`。

凭证写入 `~/.yushan/auth.json`（Unix 上权限收紧），**之后启动自动加载**。
`/logout` 清除。

不想开界面时用环境变量（优先级见[环境变量](#环境变量)）：

```console
$ export YUSHAN_API_BASE=https://api.deepseek.com
$ export YUSHAN_API_KEY=sk-...
$ export YUSHAN_MODEL=deepseek-chat
$ ./target/release/ys-coding-agent -p "你好"
```

同样形态换成 GLM 标准 API（编码套餐把 base 换成
`https://open.bigmodel.cn/api/coding/paas/v4`）：

```console
$ export YUSHAN_API_BASE=https://open.bigmodel.cn/api/paas/v4
$ export YUSHAN_API_KEY=sk-...
$ export YUSHAN_MODEL=glm-5.3
$ ./target/release/ys-coding-agent -p "你好"
```

注意：env 直连没有 provider 名（`provider` 恒为 `None`）⇒ 不走该家的方言，思考档位
`YUSHAN_REASONING_EFFORT` **不会**随请求发送、思考内容也不解析。需要这些时用 `/login`。

拉不到模型列表（目录接口失败）不会卡住：agent 降级启动，用已保存的 provider / model，
必要时提示你用 `/model <provider>/<name>` 点名。

## TUI 内置命令

以 `/` 开头提交即为命令。**内置命令优先于 skill**：装一条同名 skill 不会覆盖它们
（`/skills` 里能看到 skill 清单，选中只把 `/<name> ` 填进输入框，参数自己补）。

| 命令 | 说明 |
|---|---|
| `/help [command]` | 命令索引；带命令名只显示该条 |
| `/keybind` | 完整键位表（分段：输入编辑 / 提交退出 / 滚动 / 剪贴板 / 鼠标） |
| `/status` | 当前 provider / model / 思考档位 / token 累计 / 轮数 / 消息数 / 存活时长与执行时长 / 已登录 provider / 已知 provider / context window / 会话文件 / 工具表 / skill 条数 / cwd |
| `/skills` | 查看并选择一个 skill（回填输入框，不发请求） |
| `/copy` | 把最后一条回复的 **Markdown 原文**写入系统剪贴板 |
| `/quit` | 退出 |
| `/thinking [on\|off]` | 开关思考内容（reasoning）的显示；无参 toggle |
| `/effort [off\|minimal\|low\|medium\|high\|xhigh\|max\|default]` | 设置思考档位；`default` 清除设置，无参显示当前档位与用法 |
| `/theme [name]` | 切换配色；无参开浮层（带实时预览）。可选：`ayu-mirage`(默认) / `gruvbox-dark` / `nord` / `everforest-dark` / `solarized-dark` |
| `/model [model_name]` | 切换模型；无参开浮层 |
| `/login` | 选 provider → 填 key →（无内置 base 时）填 API base，保存凭证 |
| `/logout` | 登出并清除凭证 |
| `/new` | 开新会话（旧的已落盘，可从 `/resume` 找回） |
| `/clear` | 清空对话上下文并清屏；旧会话文件保留，可从 `/resume` 找回 |
| `/resume [id]` | 无参：列出既有会话并选择；带 id：恢复指定会话 |
| `/compact` | 压缩上下文；**回合中**发出则在下一个轮边界执行。会先给一行开始提示，结束后报告估算 token 前后值 |
| `/export [path]` | 复制会话文件到 `path`；省略则等于「确认已落盘」（不会自拷贝） |
| `/<skill 目录名> [参数]` | 展开该 skill 正文再发给模型（内含 skill，正文按需加载） |

**不是命令的 `/xxx`**：命中 skill 名 ⇒ 展开该 skill 正文再发给模型；否则回一行
「未知命令」+ 命令表，不发请求。

### skill 怎么用

skill 是 `SKILL.md` 目录，启动期扫描两条根（顺序即优先级）：

```
<cwd>/.yushan/skills/<name>/SKILL.md      项目级，优先
~/.yushan/skills/<name>/SKILL.md          用户级
```

- 命令名 = **目录名**，frontmatter 的 `name` 只作显示标签；`description` 必填。
- 模型侧：`name` + `description` + **绝对路径**常驻系统提示（正文不占上下文），
  需要时才用 `read` 打开。
- 用户侧：`/<目录名> [参数]`。三条入口（TUI / `-p` / `--json`）行为一致。
- 目录只在**启动期扫一次** ⇒ 新装 skill 要重启 agent。
- 仓库自带的开发用 skill 用 `./scripts/install-skills.sh` 安装。

## TUI 键位

`/keybind` 是正本，这里只列高频的：

| 键 | 作用 |
|---|---|
| `Enter` | 提交 |
| `Shift+Enter` / `Alt+Enter` / `Ctrl+J` | 换行（无键盘增强协议的终端用 `Ctrl+J`） |
| `Tab` | 补全浮层（输入以 `/` 开头时）；浮层内 `Tab`/`Enter` 填入、`↑↓` 选择 |
| `↑` / `↓` | 先在输入框内按视觉行移光标；到首/末行才取历史 / 滚动对话 |
| `PageUp` / `PageDown` | 翻页（一页 = 视窗高度 − 1 行） |
| `Ctrl+↑` / `Ctrl+Home` | 跳到最早消息 |
| `Ctrl+↓` / `Ctrl+End` | 跳到最新消息 |
| 滚轮 / 触控板 | 逐段滚动对话（不影响输入历史） |
| `←` / `→` | 移动输入光标（`Ctrl`/`⌥` + 方向 或 `⌥b`/`⌥f` 按词移动） |
| `Home` / `End` / `Ctrl+E` | 输入当前行的行首 / 行尾 |
| `Ctrl+A` | 全选输入框（随后 `Backspace` 清空） |
| `Ctrl+V` | 粘贴系统剪贴板（读不到时用终端的 `Ctrl+Shift+V`） |
| `Shift` + 方向键 | 扩选；端点重合即收起 |
| `Ctrl+C` | 有选区时复制；否则取消回合，**再按一次退出** |
| `Esc` | 回合中取消回合；空闲时清空输入 |
| `Ctrl+D` | 退出 |
| 左键拖选（对话区） | 选中对话内容，松手即复制；`Shift`+拖选 = 终端原生选择 |

终端限制：不绑 `⌘` 组合（终端自己消费）；不绑 `⌥+↓`（Terminal.app 把它发成 `ESC[B`，与 `↓` 同形）。

## CLI 参数

```
ys-coding-agent                          交互式 TUI（默认；无参数时忽略多余裸词）
ys-coding-agent -p [flags] <任务>         单次 print
ys-coding-agent --json [flags] <任务>     JSON 事件
ys-coding-agent --resume [<id>]          恢复会话（裸 --resume = 列表选择）
ys-coding-agent worktree <动词> …        见下一节
ys-coding-agent --help | -h | help       打印帮助后退出（不读凭证、不建运行时）
```

公共 flag：

| flag | 说明 |
|---|---|
| `--debug` | 兼容旧脚本。结构化执行日志**默认已开启**；要关闭请设 `YUSHAN_DEBUG=0` |
| `--stats` | 把背压统计摘要写到 stderr（有背压时本来也会打；`--stats` 是无条件打） |
| `--resume <id>` | 恢复指定会话，`<id>` 是会话文件名 stem 或 **文件路径**；`--resume=<id>` 已移除 |
| `-h` | `--help` 别名 |

**flag 的位置规则**：

- **交互式**：`--debug` / `--resume [<id>]` 只看**开头**的 flag 区，两者顺序无关、可重复（last-wins）；
  碰到第一个非 flag 裸词就停止剥离，后面的内容不参与解析。
- **`-p` / `--json`**：flag 必须紧跟在模式 flag 之后、任务文本之前。任务文本一旦开始，
  后面的 `--stats` / `--debug` 就是文本的一部分（`-p "任务 --debug"` 原样发给模型）。
- `--resume` 取**紧随其后**的 token 作为 id，但仅当它后面**还留着任务文本**；
  否则它是裸 `--resume`（列表选择），紧跟的 token 归任务。

## worktree 子命令

一个需求一棵树，树放在 `~/.yushan/worktrees/<仓库路径>/<name>/`（`YUSHAN_WORKTREES_DIR` 可改），
分支名 `worktree-<name>`。

```console
$ ys-coding-agent worktree create feat-a          # 建树 + 以新树为工作区启动 TUI
$ ys-coding-agent worktree create -p "做个任务"    # 自动命名 + 单次执行
$ ys-coding-agent worktree list                   # 名字<TAB>路径<TAB>分支<TAB>clean|dirty
$ ys-coding-agent worktree attach feat-a          # 在既有树里启动（可配 -p / --json / --resume）
$ ys-coding-agent worktree remove feat-a          # 删树（分支保留）
```

- `create` 失败是**硬失败**（非零退出 + 原因 + 退路）；`attach` 失败则保留共享工作区继续启动。
- 非终端环境里 `create` 建完树就退出（不打 TUI）；要在那棵树里开会话，用 `worktree attach <name>`。
- `--resume` 会读会话 meta 里记录的工作区：没有显式 `create`/`attach` 且树还在 ⇒ 自动回到那棵树。
- 名字参数可选；首 token 以 `-` 开头即视为无名（`worktree create -p "任务"` 不会建出名为 `-p` 的树）。

## 环境变量

| 变量 | 作用 |
|---|---|
| `YUSHAN_API_BASE` / `YUSHAN_API_KEY` / `YUSHAN_MODEL` | 模型端点 / 凭证 / 模型名 |
| `YUSHAN_REASONING_EFFORT` | 思考档位 `minimal`…`max`，默认 `high`；经 provider 级 + 模型级两层门控后才随请求发送（GLM 还会折叠到该代际接受的档位） |
| `YUSHAN_CHANNEL_CAPACITY` | 事件信道容量，默认 1024，下限 16（低于下限会 clamp 并在 stderr 警告） |
| `YUSHAN_SESSIONS_DIR` | 会话目录覆盖（默认 `~/.yushan/sessions`） |
| `YUSHAN_LOG_DIR` | 结构化日志目录覆盖（默认 `~/.yushan/logs`） |
| `YUSHAN_DEBUG` | 日志开关：**未设置/空串 = 开启**；`1/true/yes/on` 开启，其余非空值关闭。优先于 `--debug` |
| `YUSHAN_DEBUG_ARGS` | 工具参数摘要档位 `none` / `head` / `full`，默认 `head`；脱敏在所有档位生效 |
| `YUSHAN_SEARCH_SEARXNG_URL` | 搜索后端：SearXNG 实例地址 |
| `YUSHAN_SEARCH_DEEPSEEK_KEY` | 搜索后端：DeepSeek（亦认 `DEEPSEEK_API_KEY`，命名空间名优先） |
| `YUSHAN_SEARCH_TAVILY_KEY` | 搜索后端：Tavily（亦认 `TAVILY_API_KEY`，命名空间名优先） |
| `YUSHAN_SEARCH_BOCHA_KEY` | 搜索后端：Bocha |
| `YUSHAN_SEARCH_FIRECRAWL_KEY` | 搜索后端：Firecrawl |
| `YUSHAN_FETCHED_DIR` | `webfetch` 落盘目录覆盖（须为绝对路径） |
| `YUSHAN_SUBAGENTS_DIR` | 子 agent 产物目录覆盖（只写不删） |
| `YUSHAN_WORKTREES_DIR` | 受管工作树父目录覆盖 |

搜索后端**各自配置才进链**，彼此独立、不互斥；一个都没配时 `websearch` 仍留在工具表里，
调用期返回明确的「未配置后端」错误（工具面恒定，不会时有时无）。

skill **没有任何环境变量**：扫描根固定为 `<cwd>/.yushan/skills` + `~/.yushan/skills`。

## 文件与目录

```
~/.yushan/
├── auth.json         provider 凭证（/login 写，/logout 清；Unix 权限收紧）
├── config.json       用户覆盖：context_window、压缩比例等
├── sessions/         会话 *.jsonl（会话 id = 文件名 stem，--resume 用它）
├── logs/             debug-<pid>-<ts>.jsonl（默认保留 7 天，启动期清理超龄）
├── skills/           用户级 skill
├── worktrees/        受管 git 工作树
├── subagents/        子 agent 产物
└── fetched/          webfetch 抓取落盘
```

会话文件是 JSONL，`/export` 就是复制它。退出时 stderr 打两行，可直接用于恢复：

```
session: 2026-10-10-abc
worktree: feat-a
```

## 退出码与输出约定

`-p` / `--json` 一次性模式的退出码按「**有没有产出能交差**」判：

| stop reason | 退出码 |
|---|---|
| `Cancelled`（墙钟预算用尽）、`NoOutput`（模型没给出可见内容） | `1` |
| `Completed` / `MaxRounds` / `Truncated`（至少有输出） | `0` |

**stdout 只放数据**：`-p` 放正文增量，`--json` 放逐行 `Envelope`。
启动提示、最后一条回复的排版、背压统计、错误说明**全部走 stderr**，所以
`ys-coding-agent --json "..." > events.jsonl` 是干净的。

`--json` 的每一行是 `{"source","turn","event"}`，`event` 是 `AgentEvent`
（`RoundStarted` / `ModelTextDelta` / `ToolCall` / `ToolResult` / `RoundUsage` /
`RunFinished` / `RunFailed` / `ContextCompacted` …）。

## 常见问题

**「没有可用模型」/ 一发请求就报未配置**
没登录或没选模型。TUI 里 `/login`（可带 `(no model)` 启动去修），
或用环境变量 `YUSHAN_API_BASE` / `YUSHAN_API_KEY` / `YUSHAN_MODEL`。
一次性模式（`-p` / `--json`）没有后续输入窗口，**缺模型直接非零退出**并把原因写到 stderr。
空模型名不会被当成「随便挑一个」。

**新装的 skill 看不到**
skill 目录只在启动期扫一次，重启 agent；再用 `/skills` 或 `/help` 确认。也检查
`description` 是否为空（空则整条不加载）。

**`websearch` 报未配置后端**
配上任意一个搜索后端（见[环境变量](#环境变量)）。提示是**调用期**返回的错误结果，
工具本身一直在表里（工具面恒定）；重启后提示也会在启动期出现。

**TUI 里粘贴不生效**
`Ctrl+V` 靠本机工具读剪贴板（`wl-paste` / `xsel` / `xclip` / `pbpaste`）；没装或 SSH 远端时读不到，
用终端自己的 `Ctrl+Shift+V`。复制方向上 `Ctrl+C` / 拖选会**同时**走本机工具和 OSC 52。

**想给 agent 做脚本化集成**
用 `--json` 吃事件流，不要解析 `-p` 的 stdout（那是给人看的增量文本）。
需要稳定契约时读 [`docs/architecture.md`](../../docs/architecture.md) 的事件一节。
