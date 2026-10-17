# DeepSeek 搜索后端适配 — 修改报告

> 日期：2026-10-17 ｜ 编排方案：quick ｜ 基线：`f0d993e1aa5a5f1acf202f7c56108c75ab810beb`
> 设计决策（用户确认）：链序 `DeepSeek → Tavily → Bocha → Firecrawl → SearXNG`；
> 凭证 `YUSHAN_SEARCH_DEEPSEEK_KEY` 优先、回退 `DEEPSEEK_API_KEY`，不读 `YUSHAN_API_KEY`。

## 一句话

`websearch` 的搜索后端从 4 家增至 **5 家**：新增 DeepSeek Search，排在托管组首位；
该行仅在配置 key 时进链，因此不改变既有用户的链序，也不改变「零配置 ⇒ 链为空」的语义。

## 变更

### 实现

- `adapters/tools-web/src/lib.rs`：新增 `ENV_DEEPSEEK_KEY` / `ENV_DEEPSEEK_KEY_ALIAS`、
  `Kind::Deepseek`、降级提示文案；测试 env 清理集合同步。
- `adapters/tools-web/src/backends.rs`：`DEEPSEEK_ROOT`、`DEEPSEEK_KEY_ENVS`、`TABLE` 首行、
  `kind_slug` / `request_url` / `plan` / `parse` 的 DeepSeek 臂、`deepseek_hits`。
- `adapters/tools-web/src/search.rs`：调用期「无后端」文案点名新 env；链序注释同步。
- `apps/coding-agent/src/*`：**零改动**（链由 `WebConfig::from_env` 解析，工具面仍为 9 个）。

### 协议映射

- 请求：`POST https://api.deepseek.com/v1/tools/search`，`Authorization: Bearer <key>`，
  body `{query, count, need_content:false}`；`count` 走统一 `limit` 的 `1..=20` clamp。
- 响应：`results[]{title,url,snippet,summary}`；`title`/`url` 直接映射，`snippet` 取 `summary`、
  缺失时回退 `snippet`；`rerank_score` / `authority` / `site_name` / `date` / `content` 忽略。
- `results: []` ⇒ `Ok(0)`（真没结果，**停链**）；缺 `results` / `error` 信封 ⇒ `Err`（**换下一家**）。
- 不做自动重试；`Abort` 立即停链；30s 总预算与其他后端一致。

### 测试与 fixture

- 新增 `adapters/tools-web/tests/fixtures/deepseek_{ok,empty,error}.json`（fixture 总数 14 → 17）。
- `tests/mapper.rs`：五家枚举、DeepSeek 映射三态、`summary` 优先/`snippet` 回退、
  `plan` 的方法/路径/鉴权/请求体/clamp、无 key 点名 env、端点路径形状。
- `tests/contracts.rs`：env 名快照、通用名回退/项目名优先、完整链序逐字。
- `tests/tools.rs`：DeepSeek 链胜出与 `summary` 渲染（含请求体断言）、
  链为空时降级文案点名各 env；`SEARCH_ENV_KEYS` 与回环用例清理同步。
- `tests/harvest.rs`：新增 1 条 `#[ignore]` 真实采集（共 5 条）。
- `apps/coding-agent/tests/{degraded_search,resume_cli,session_cli,skills_cli,budget_exit,debug_log}.rs`：
  每个子进程隔离补 `YUSHAN_SEARCH_DEEPSEEK_KEY` + `DEEPSEEK_API_KEY`。

### 文档

- `AGENTS.md`：env 清单。
- `docs/design-final/tools-web.md`：链序、凭证、fixture/测试计数，新增「2026-10-17 增补」小节。
- `docs/adr/0016-web-tools-design.md`：修订记录、决策 2 链序、Consequences。

## 验证

| 命令 | 结果 |
|---|---|
| `cargo test --workspace --quiet` | **1257 passed / 0 failed / 12 ignored** |
| `cargo test -p ys-tools-web --lib` | 20 passed |
| `cargo test -p ys-tools-web --test mapper` | 39 passed |
| `cargo test -p ys-tools-web --test contracts` | 8 passed |
| `cargo test -p ys-tools-web --test tools` | 37 passed |
| `cargo test -p ys-tools-web --test harvest -- --ignored --list` | 5 tests |
| `cargo fmt --check -p ys-tools-web` | 通过（未运行裸 `cargo fmt`） |
| `cargo clippy --all-targets` | 仅有**基线既有**告警，见下 |

**clippy 说明**：当前工具链对未修改文件报出既有 lint（`ys-protocol/src/lifecycle.rs` 的
`derivable_impls`；`tools-basic` 的 `collapsible_if`；`tools-web/src/store.rs` 的 `filter_next`；
`tools-web/tests/harvest.rs` 的 `collapsible_if` —— 后者已在基线快照中直接核对为既有；
`apps/coding-agent` 的若干既有告警）。本轮新增代码无告警；本轮曾引入的 1 条
`skills_cli.rs` 文档注释告警已修复并复核归零。

**关于一次全量失败**：首轮 `cargo test --workspace` 在
`app_loop::tests::every_request_variant_is_handled` 上 5s 超时；该用例单独运行
345 passed / 2.51s，基线快照（`git archive f0d993e`）345 passed / 2.19s，
且 `apps/coding-agent/src/app_loop.rs` 本轮零修改 ⇒ 判定为**并发负载下的既有定时敏感用例**。
空闲环境重跑全量通过。

## Review 结论

- 单元 A（核心实现）：**deep review 通过**，无 P0/P1；四个变异靶子
  （summary/snippet 顺序、`need_content` 缺失、Err→Ok(0)、DeepSeek 行移位）全部被杀。
- 单元 B（app 测试隔离）：**shallow review 通过**。
- 单元 C（tools/harvest/README）：**shallow review 通过**。
- 文档 + P2 修正：初判 1 条 P1（ADR 中「4 家」计数），修正后复核**通过**。

## 残留 / 风险

1. `docs/arch/tools-web/context.md` 等**历史设计归档**仍写旧四家链序；它们是设计过程快照，
   不在本切片范围。当前事实以 `docs/design-final/tools-web.md` 与代码为准。
2. 上述 clippy 既有告警与本轮无关，未扩大改动。
3. `harvest_deepseek` 需真实 key + 网络，本轮未执行（`#[ignore]`）；fixtures 仍为按官方文档手写，
   待有 key 时用 harvest 替换为真实响应。
4. 明确不做：不接入 DeepSeek Fetch API；不加 DeepSeek 端点覆盖 env。
