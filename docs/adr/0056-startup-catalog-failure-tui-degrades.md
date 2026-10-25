# 0056 — 启动期模型目录拉取失败：TUI 降级，一次性模式硬失败

模型名只来自端点目录，代码里没有内置默认值。启动期「凭证齐全但模型名为空」时必须同步
拉一次 `/v1/models` 取第一条。**拉不到时**：TUI 以「无模型」状态照常起屏，把失败原因
作为启动期提示投进 transcript；`-p` / `--json` 仍然早失败并给出可操作的错误。

## Status

accepted（2026-10-25）

## 背景

- **模型名只有一个来源**：该端点的 `/v1/models` 目录（`design-final/coding-agent-tui.md`
  的 M4）。启动期这条同步拉取是「用户没点名模型」时唯一可能等网络的路径。
- **「有凭证、无模型名」是合法的持久化状态**：`/login` 拉不到目录时只存凭证、模型名留空
  （`capabilities.rs::login`），并告诉用户稍后用 `/model` 点名。
- **旧行为在这条路径上直接退出**。对 env 直连它是合理的（用户能改 `YUSHAN_MODEL` 后重开），
  但 TUI 的修复手段恰好都在**起屏之后**：`/login` 重新登录、`/model <provider>/<name>`
  点名模型。启动失败使这两条路都不可达 —— **错误文案建议的操作被错误本身堵死**。
- **运行时本来就支持「无模型」**：`Agent::run_turn` 返回
  `No model configured. Use /login to configure an API provider.`
  （`crates/ys-runtime/src/agent.rs`），TUI 状态行显示 `(no model)`（`ys-tui-coding`）。
  缺的不是能力，只是启动路径没有把它放行。

触发场景（实测）：`auth.json` 里某 provider 的凭证被拒（401）、`model` 为空，
`state.json` 的 last-active provider 又没有凭证 ⇒ 解析落到该 provider ⇒ 目录拉取 401
⇒ 进程在起屏前退出。

## 决策

### 1. 失败按模式分流

`main::catalog_failure(base, reason, interactive)`：

| 模式 | 结果 | 理由 |
|---|---|---|
| `Mode::Interactive` | `Ok(Some(提示))`；提示并入启动期提示，排在最前 | 起屏后用户才有 `/login` / `/model` 两条修复路径 |
| `Mode::Print` / `Mode::Json` | `Err(catalog_unavailable(...))` | 没有交互窗口，缺模型必然跑不了；早失败并可操作 |

提示文案点名端点、原因（鉴权失败 / 网络不可达 / 空目录）与两条修复路径。它走既有的
**输出三分**通道：TUI → `Outbound::Output`（transcript 第一条），一次性模式 → stderr。

### 2. 空模型名不得构造模型实例

生产 factory（`main::build_configured_model`）在 `cfg.model.is_empty()` 时返回 `None`。
接线器因此进入真正的「未配置模型」状态：

- 状态行显示 `(no model)`；
- 直接发消息由运行时的 `No model configured` 拦下；
- **绝不会**拿空模型名向端点发请求（那只会换来 400）。

空名被当作「未配置」而不是「一个名字为空的已配置模型」—— 后者会让 UI 与运行时都
误以为可以开回合。

## Considered Options

- **TUI 也硬失败，要求用户设置 `YUSHAN_MODEL` 后重启**：可行，但把一次可交互修复变成
  一次进程重启；且用户此刻往往不知道正确的模型名（目录正是拿不到的东西）。**不采纳**。
- **完全不做启动期同步拉取，只靠起屏后的后台拉取**：起屏更快，但「凭证有效、目录可达」
  这一最常见情形也会以无模型状态启动，多一步 `/model`；后台目录到达只写缓存、不自动
  采纳第一条。**不采纳**。
- **拉不到时猜一个模型名**：会过时 / 不存在 / 不兼容，选中即 400；与「不写死模型名」
  的决策正面冲突。**不采纳**。

## Consequences

- TUI 在「凭证被拒 / 网络不可达 / 空目录」时仍能启动。用户看到一条明确的启动期提示，
  可用 `/login`（重新登录并拉目录）或 `/model <provider>/<name>` 修复。
- 同一失败在 TUI 是提示、在一次性模式是退出码 1 —— **分流是模式相关的**，这是有意为之。
- 目录成功时行为不变：取第一条当起始模型并并入缓存。
- 启动期同步拉取仍是「有凭证、无模型名」时唯一等网络的路径。

## 测试

- `startup_catalog_failure_degrades_only_in_interactive_mode` —— 交互式返回提示且不写
  模型名；一次性返回 `Err`。变异：交互式分支改 `Err` / 一次性分支改 `Ok` ⇒ 分别变红。
- `startup_catalog_success_adopts_first_and_empty_catalog_degrades` —— 成功取第一条；
  空目录与拉不到同规。
- `configured_model_requires_non_empty_model_name` —— 空名返回 `None`。变异：删掉空名
  守卫 ⇒ 变红。
- `with_catalog_hint_prepends_and_preserves_existing` —— 提示前置且不覆盖既有启动期提示。

## 参考

- As-built：[`docs/design-final/coding-agent-tui.md`](../design-final/coding-agent-tui.md) §7、§8.2 M4
- 模型名唯一来源的历史决策：commit `4e94239`（feat(model): 删掉写死的模型名）
- `/login` 在拿不到目录时只存凭证：`apps/coding-agent/src/capabilities.rs::login`
- 无模型运行时语义：`crates/ys-runtime/src/agent.rs`
