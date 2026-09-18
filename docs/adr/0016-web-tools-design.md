# 0016 — web 工具：落盘+指针、后端链用表而非 trait、引入 `htmd`、SSRF 反转

给 coding agent 补 `websearch` / `webfetch` 的四个决定：① `webfetch` **落盘 + 头 + 指针**（而非截断或小模型总结）；
② 搜索后端链用**一张常量表 + 两个 `match`**，**不建 trait**；③ 引入 **`htmd`**（本项目**第一次主动破例引依赖**，
且 `regex` 是硬否决项）；④ **SSRF 从「不做」反转为「做」**（它是输入校验，不是权限边界）。

## Status

accepted（2026-09-17）· **修订（2026-09-18）**：Firecrawl 由「keyless」改为**需 key**（见「决策 2」与「Consequences」的修订记录）。

## 背景

- 上游：`docs/arch/tools-web/context.md`（约束 + 已定决策）、`docs/arch/tools-web/design.md`（融合方案）、
  `docs/arch/tools-align-pi/web-research.md`（四家同类实现调研）。
- 关键约束：**工具拿不到 `Model`**（`Tool::call` 只收 `ToolContext`）→ Claude Code 式「fetch 后交给小模型总结」
  **需要专门设计**（注入 `Arc<dyn Model>` + 重排装配顺序）。
- 关键约束：`-p`/`--json` 下 `boundary` 恒为 `None`（**B2**）→ 超时是唯一刹车。
- 上一个 ADR（**0015**）刚立「不引 `regex`/`ignore`/`globset`/`walkdir`」。

## 决策

**1. `webfetch` = 抓取 → Markdown → 落盘 → 头 + 指针**

写 `~/.yushan/fetched/<slug>-<FNV哈希>.md`（`YUSHAN_FETCHED_DIR` 可覆盖），返回**头部** + **完整绝对路径**；
头部与 notice **刻意与 `read` 同构**（2000 行/50KB + `Use offset=N to continue`）。

- **`read`/`grep` 能直接读它** —— `self.workspace.join(path)` 遇**绝对路径整体替换**（`Path::join` 语义），无需改 read/grep。
- **铁律：绝不写进 workspace**（污染用户仓库 / 出现在 `git status` / 可能被误提交）。
- **文件内容前 2 行是警示 + Source URL**，且**头算进 `head()` 的内容** → ① `offset` 与**文件行号 1:1 对齐**；
  ② `read ~/.yushan/fetched/x.md` 也**看得到** `[untrusted web content]` 警示（否则绕过 webfetch 的警示）。
- **清理**：启动期按年龄（7 天）删本工具命名的文件，best-effort、静默（照 `logging.rs`）。
- **落盘目录解析**：`YUSHAN_FETCHED_DIR` → `<HOME>/.yushan/fetched` → `temp_dir()/yushan-fetched-<pid>` → **都不行则不落盘（只回头部 + 明说）**。
  解析方式为「**尝试创建、失败降级**」（`HOME` 存在但只读是真实的）；**绝不产生相对路径** —— 相对路径解析到 CWD，而 coding agent 的 CWD 就是用户仓库，
  且 `.gitignore` 里有 `tmp/` 会让这种污染**连 `git status` 都看不见**。
  **同一契约适用既有 bug**：`main.rs::sessions_dir()` 用相对路径回退 → 无 HOME 时会话文件落进用户仓库，本切片**同批修**。

**2. 搜索后端链：一张 `const TABLE` + `plan()`/`parse()` 两个 `match`，不建 trait**

```
已配 key 的 provider（tavily → bocha → firecrawl） → searxng
```
- **调用链是确定的**（"多后端，但链确定"）：**启动期构造一次并冻结**，顺序**完全由常量表决定**，
  代码里**没有任何动态选择**（不按延迟、不按历史成功率、不按随机），**不做并行竞速/hedging**（否则"这次用了谁"不确定）。
  ⚠️ **诚实边界**：能让**尝试顺序**确定，**不能让"谁成功"确定**（网络随机）——目标是把不确定性**局限在外部**并让它**可见**：
  启动期 stderr 列出链、调用时用了非第一顺位就明说 `[via searxng]`、全失败时逐条列理由。
- **顺序的分界线是「用户有没有显式配置」**（2026-09-18 修订）：前三家都是**托管搜索 API**（配了 key ⇒ 想要那一家的质量），
  返回的就是搜索结果本身；**SearXNG 是元搜索**——它自己还要再转发给上游引擎（多一跳、结果质量取决于该实例开了哪些引擎、
  有没有被上游限流，**我们无法评估**）→ 排**末位**。
  ⚠️ **旧理由已失效**：初稿曾写「用户自建的 SearXNG 比第三方**免费额度**可靠，故 searxng 排在 firecrawl 之前」——
  Firecrawl 转入「需 key 的托管服务」组后**不再是免费额度**，那条比较的另一个对象消失了；留 searxng 在它前面就只是
  「自建比收费可靠」这一条**未经验证**的断言，且与「配了 key 说明用户想要那一家的质量」自相矛盾。
- **降级只认「错误/限额」，空结果不降级**（真没结果的查询在 N 个后端各跑一遍是白等）。
  补充：**「响应形状不对」（缺少结果数组）算错误、不算空结果** —— 否则一家改个字段名，整条链再也不会去试别人。
- **链有总预算（30s）+ 每后端自适应切片、下限 8s** —— 否则 4 个后端各 60s = 最坏 4 分钟，而 `-p`/`--json` 没有中止通道。
  切片规则：`slice = min(remaining, max(MIN_SLICE, remaining / backends_left))`；**`remaining < MIN_SLICE` 就不再尝试**（记 `budget exhausted`）。
  **抬高下限（8s）是为了让"链确定"落到可断言的形态**：原设计的小下限会让每个后端拿到的预算强烈依赖前几个的耗时 → 同一查询这次成功、下次超时。
  抬高后「被尝试的后端**至少 8s**」成为**确定下界**，不确定性被压到"是否被尝试"一格；写进测试。
- **工具恒注册、降级在调用期**：若按登录/配置增减工具，**prompt 前缀会随 `/login` 抖动 → prompt cache 全废**。

**3. 引入 `htmd`（唯一的真·新增依赖）**

- **`regex` 是硬否决项**：`fast_html2md`（内存最优，1.84%）与 `html-to-markdown-rs`（下载量最大）都拉 `regex`
  → 与 ADR-0015 正面冲突，**排除**。`htmd` 的树是 `html5ever + markup5ever_rcdom + phf`，无 regex，
  且提供 `HtmlToMarkdown::builder()` + 自定义 element handler（**能跳 boilerplate**）。
  旁证：CodeWhale 在踩过 readability+html5ever **双栈编译冲突**后最终选它。
- **并立规则：本仓库只允许存在一个 HTML 解析栈**（双栈冲突是 CodeWhale 真踩过的坑）。
- **`htmd` 是 `0.5.x`，而 0.x 无 SemVer 兼容保证** → 配套三条"少修改"措施：**隔离壳**（换 crate 只改 `convert.rs`）、
  **golden 测试**（5 类真实页面冻结成 fixture，断言"导航/脚本消失、正文完整"）、**版本钉到小版本**（升级必须过 golden）。
- 依赖只进 `tools-web` 一个 crate；`tools-basic`/`ys-*` 的依赖图不变。

**4. SSRF 做（~35 行）；DDG 抓取不做**

- scheme 白名单 + 拒 URL 内凭据 + 拒 `localhost`/`.local`/单标签 + IP 段判定（含 `::ffff:a.b.c.d` 递归按 v4 判）。
- **只作用于模型给的 URL，绝不作用于配置里的后端地址**（自建 SearXNG 常在 `127.0.0.1`）。
- **DDG 不做**：第三方评估为 unofficial/experimental，云/共享 IP 被激进限流，易**静默给错答案**。
- 免 key 走 **SearXNG**（只认 `YUSHAN_SEARXNG_URL`，**产品不自带公共实例**）。

## Considered Options

- **`webfetch` 直接截断回喂**。**否决** —— 内容**永久丢失**（模型看不到被截掉的部分，也无法续读）。
- **Claude Code 式「带 `prompt`、小模型总结后只回答案」**。**当前做不到**：`Tool::call` 拿不到 `Model`；
  要做需注入 `Arc<dyn Model>` + 重排装配顺序（`build_tools` 早于模型构建）。落盘方案用**零改造**拿到了
  「上下文有界」这个主要收益，且额外得到「内容不丢 + 可 grep」。
- **`fast_html2md`（lol_html 流式，峰值内存 1/25）**。**否决** —— 拉 `regex`。且 5 MiB 下载 cap 下，
  `htmd` 的 ~2.5MB 峰值可接受，「流式转换」的收益不成立（我们本来就要持有完整 Markdown 才能落盘）。
- **给后端建 `SearchBackend` trait**。**否决** —— 逐条核对，4 家只差 method / URL / 鉴权头 / 请求体 /
  响应路径 / 字段名，**全是数据**，一张表 + 两个穷尽 `match` 即可（漏改会**编译报错**，不静默漂移）。
  **这与 ADR-0015 否决「纯 Rust 搜索后端」不是相反结论，而是同一个判断：差异是否只是数据。**
- **provider 原生搜索排第一**（CodeWhale 的做法，Kimi/智谱）。**本轮不做** —— 当前 provider 目录
  （deepseek/minimax/custom）没有可用的搜索端点；且它会让**工具行为随「登录哪个模型」变化**，
  与「工具面必须稳定（prompt cache）」冲突。
- **后端地址也过 SSRF 闸**。**否决** —— 会打死自建 SearXNG（`127.0.0.1:8888`）。

## Consequences

- **ADR-0015 的「不引依赖」被主动破例一次**（`htmd` + html5ever 树）：编译时间与供应链面上升。缓解 =
  单调用点 + 只进一个 crate + prototype 前置 + 「一个解析栈」规则。**破例理由与 `regex` 否决线一并记录**，
  否则下次换 crate 的人会重新踩。
- **SSRF 是字面量级的**：不解析 DNS → **挡不住 DNS rebinding**。它是**输入校验、不是安全边界**；
  与「runtime 不做安全」不冲突（那条讲的是沙箱/权限/多租户这类**权限边界**）。**不得被当成防护栏宣传。**
- **注入防护是安慰剂级的**：一句静态警示 + 写进落盘文件头，**不引 `regex`、不做扫描器、不改写内容**
  —— 定位是**给模型归因信号**，不是防护。
- **工具数 7 → 9**：触及 9 处（`EXPECTED_ORDER` / `vec!` / 3 处 `assert_eq!(len,7)` / `prompt.rs` 的
  snippet + guideline + 2 条断言）；每轮固定 +60~120 token，**净负外部性**。
- **`read` 的绝对路径穿透是隐性契约**：webfetch 的指针可用性依赖 `Path::join` 遇绝对路径替换。
  若将来给 `read` 加「workspace 根包含检查」（一个合理的加固），**webfetch 会静默失效** →
  必须有跨工具测试（webfetch 返回的路径直接喂 `ReadTool`，断言续读行号对齐）并写进 as-built。
- **降级路径行为更宽**：同一次查询在不同机器/时刻可能走不同后端、不同质量。缓解 = 逐后端理由进 notice +
  启动期 stderr 一行；**不能消除**。
- **`Firecrawl keyless` 会被误读成「免 key」——已改（修订，2026-09-18）**：官方 API 文档明写认证**必须**带
  `Authorization`（错误码表：「401 | The API key was not provided」）。初稿据一篇**第三方对比文**把它当作 keyless
  （大概率是把「免费额度」误读成「免 key」）→ **零配置用户的链因此非空**（只剩 firecrawl），`degraded_hint` 里那句
  「未配置任何搜索后端」**不可达**：用户一句提示都没有，首次 `websearch` 撞上一个**他从没选过**的厂商的 401、链上无后备。
  这**正面违反本 ADR 自己那句「Firecrawl keyless 绝不能是唯一路径」**。**改为需 key（`YUSHAN_SEARCH_FIRECRAWL_KEY`），
  零配置链恢复为空**；该行归入「已配 key 的 provider」组，searxng 随之退到末位（见「决策 2」）。
- 参考：`docs/arch/tools-web/design.md`（融合方案与对比矩阵）、`context.md`（约束）。
