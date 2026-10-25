# sequential-workflow — EXAMPLES

四个真实业务场景下、字段已填好的样例。所有占位符（`[功能名称]`、`[路径]` 等）都用具体内容替换。

每个样例对应一个 phase 的产出，可直接参照格式书写。

---

## Example design-plan

<a id="design-plan"></a>

样例背景：为部署流水线添加 Slack 通知服务（失败时发到 `#deploys` 频道，成功时静默）。

文件：`[docs/design-plans/YYYY-MM-DD-<feature>.md]`

```markdown
# Slack 通知集成 设计计划

## 背景
- 当前状态: 部署失败只在 CI 控制台输出，团队成员需主动查看
- 为什么需要这个变更:
  - 失败时关键人 (on-call) 不知道, 平均响应时间 30+ 分钟
  - 已影响 3 次 SLO 违规 (Q2)
  - 解决方案: 失败时通过 Slack webhook 推送告警到 `#deploys`

## 设计
- 整体方案:
  - 新增 `SlackNotifier` 服务, 接收事件 (`on_failure`, `on_success`)
  - `DeployRunner` 在 lifecycle hook 里调用 notifier
  - Webhook URL 从环境变量 `SLACK_WEBHOOK_URL` 读取, 缺失则降级为 stdout
- 架构决策:
  - 选 webhook 而非 full bot API: 不需要安装 app, 最小权限
  - notifier 作为独立 service, 而非 `DeployRunner` 内联: 单元测试容易, 后续可扩展其他 channel
- 数据流 / 接口变更:
  ```
  DeployRunner.on_step_complete(step) →
    if step.failed: SlackNotifier.send(alert_payload)
    else: skip
  ```
  - `SlackNotifier.send(message: str, severity: Literal["info", "warn", "error"]) -> bool`

## 涉及文件
- 新增: `src/services/slack_notifier.py`
- 新增: `tests/services/test_slack_notifier.py`
- 修改: `src/ci/runner.py` (在 lifecycle hook 调用 notifier)
- 修改: `src/services/__init__.py` (导出 SlackNotifier)
- 修改: `.env.example` (新增 `SLACK_WEBHOOK_URL` 注释)

## 测试策略

### 测试合同（Phase 3 验收依据）

> 由用户签字确认，或经公约 C.3 自动放行判据确认（六条全绿）。

> 每条需求一条验收条款。Phase 3 review subagent 按本节逐条跑命令 / 核对输出。

#### N1: 部署失败时通过 Slack 推送告警到 #deploys
- **验收方法**: `pytest tests/integration/test_deploy_with_slack.py::test_failure_alerts_slack -v` — 期望 1 passed 且 mock webhook 收到 payload 包含 "severity": "error"
- **优先级**: 必测
- **验收人**: review subagent

#### N2: 5xx 响应重试 3 次后放弃
- **验收方法**: `pytest tests/services/test_slack_notifier.py::test_5xx_retry_3_times -v` — 期望 1 passed 且 mock 日志含 3 条 "retry N/3"
- **优先级**: 必测
- **验收人**: review subagent

#### N3: Webhook URL 缺失时不抛异常, 降级 stdout
- **验收方法**: `pytest tests/services/test_slack_notifier.py::test_url_missing_fallback_stdout -v` — 期望 1 passed 且 captured stdout 含告警文本
- **优先级**: 必测
- **验收人**: review subagent

#### N4: SlackNotifier 单元测试覆盖率 ≥ 90%
- **验收方法**: `pytest --cov=src.services.slack_notifier tests/services/test_slack_notifier.py` — 期望 ≥ 90%
- **优先级**: 可推迟
- **验收人**: review subagent

#### N5: Webhook URL 错误时, 日志中包含脱敏后的 URL (前 8 位 + `***`)
- **验收方法**: `pytest tests/services/test_slack_notifier.py::test_url_masked_in_logs -v` — 期望 1 passed 且 captured log 含 `https://hooks.***`
- **优先级**: 必测
- **验收人**: review subagent

#### N6: 在 staging 手动触发失败部署, 确认 Slack #deploys 频道收到告警
- **验收方法**: 人工 — 在 staging 触发失败部署, 检查 #deploys 频道 30s 内出现告警消息
- **优先级**: MANUAL_ACK_REQUIRED
- **验收人**: 用户

### 测试框架与命令
- 单元测试: `pytest tests/services/test_slack_notifier.py -v`
- 集成测试: `pytest tests/integration/test_deploy_with_slack.py -v`
- 类型检查: `mypy src/services/`
- Lint: `ruff check src/services/ tests/`

## 波及文档
- `docs/design-final/notifications.md` — 新建, 描述通知服务的统一接口
- `docs/design-final/deployment.md` — 追加章节, 描述失败时通知流程
- README.md — 追加 Slack webhook 配置说明

## 风险与注意事项
- Webhook URL 泄漏: 日志中必须脱敏 (只显示前 8 位 + `***`)
- 5xx 响应重试时需用指数退避, 避免压垮 Slack
- 失败时 notifier 自身失败不能影响部署结果 (catch all exception)
- 频率限制: Slack webhook 1 msg/s, 短时间大量失败可能触发 429
```

---

## Example exec-plan

<a id="exec-plan"></a>

对应上面的 design-plan。**4 个 task 成对出现（2 个 impl + 2 个 test）**，按目录划分无依赖可并发。

文件：`[docs/exec-plans/YYYY-MM-DD-<feature>.md]`

```markdown
# Slack 通知集成 执行计划

> 每个任务按目录划分, 同一目录下的所有变更由一个 agent 完成。
> **测试任务 (type=test) 是一等公民** — 必须显式创建、必须完成、必须配对。
> **依赖声明是并发调度的依据** — Phase 3 据此合并单元并划分波次。

## 上下文引用

参考设计计划: `[docs/design-plans/YYYY-MM-DD-<feature>.md 路径]`
参考验证策略: design-plan「测试策略」节（Phase 1 的测试合同）

## 基线

- **基线 ref**: `a1b2c3d`（Phase 3 开始时由 `scripts/baseline.sh` 建立）
- 用途: deep review 执行 `git diff a1b2c3d -- <files>` 取字节级证据（含 `??` 新文件）

## 任务清单

### Task 1: src/services/ — SlackNotifier 实现 [type=impl]
- **涉及目录**: `src/services/`
- **涉及文件**:
  - `src/services/slack_notifier.py` (新建, ~80 行)
  - `src/services/__init__.py` (修改, 导出)
- **描述**: 实现 `SlackNotifier` 类 (HTTP POST + 指数退避重试 + URL 缺失降级)
- **关联测试任务**: T2
- **依赖**: 无

### Task 2: tests/services/ — SlackNotifier 单元测试 [type=test]
- **涉及目录**: `tests/services/`
- **涉及文件**:
  - `tests/services/test_slack_notifier.py` (新建, ~150 行)
- **描述**: 覆盖正常路径 / 5xx 指数退避重试 / URL 缺失降级 / 4xx 不重试 4 个场景
- **验证方法**:
  - `pytest tests/services/test_slack_notifier.py -v` — 期望 12+ passed
  - `pytest --cov=src.services.slack_notifier tests/services/test_slack_notifier.py` — 期望 ≥ 90%
  - `mypy src/services/slack_notifier.py` — 期望 No issues
  - `ruff check src/services/slack_notifier.py tests/services/test_slack_notifier.py` — 期望 All checks passed
- **关联 impl**: T1
- **依赖**: 无（与 T1 同单元，见 R4）

### Task 3: src/ci/ — DeployRunner 集成 [type=impl]
- **涉及目录**: `src/ci/`, 项目根
- **涉及文件**:
  - `src/ci/runner.py` (修改, lifecycle hook 添加 notifier 调用)
  - `.env.example` (修改, 添加 SLACK_WEBHOOK_URL 注释)
- **描述**: 在 `DeployRunner` 的 `on_step_complete` hook 调用 `SlackNotifier`
- **依赖**: T1 (需要 SlackNotifier 已存在)
- **关联测试任务**: T4

### Task 4: tests/integration/ — 部署集成测试 + 手动验收 [type=test, 含 MANUAL_ACK_REQUIRED]
- **涉及目录**: `tests/integration/`, `tests/ci/`
- **涉及文件**:
  - `tests/integration/test_deploy_with_slack.py` (新建, ~80 行, mock webhook endpoint)
- **描述**: 集成测试覆盖失败时调用 notifier; 同时验证现有 CI 测试无回归
- **验证方法**:
  - `pytest tests/integration/test_deploy_with_slack.py -v` — 期望 3+ passed
  - `pytest tests/ci/test_runner.py -v` — 期望全过 (无回归)
  - `mypy src/ci/runner.py` — 期望 No issues
  - `ruff check src/ci/runner.py tests/integration/test_deploy_with_slack.py` — 期望 All checks passed
- **关联 impl**: T3
- **依赖**: 无（与 T3 同单元，见 R4）
- **MANUAL_ACK_REQUIRED** (Phase 4 报告须原样保留, 由用户勾选):
  - [ ] 在 staging 环境手动触发失败部署, 确认 Slack `#deploys` 频道收到告警
  - [ ] Webhook URL 错误时, 日志中的错误信息包含脱敏后的 URL (前 8 位 + `***`)

## 波次计划（Phase 3 计算后回填）

| 单元 | 含任务 | 涉及文件 | 依赖 | 波次 | review 档 |
|------|--------|---------|------|------|-----------|
| U1 | T1, T2 | `src/services/slack_notifier.py`, `src/services/__init__.py`, `tests/services/test_slack_notifier.py` | 无 | W0 | deep |
| U2 | T3, T4 | `src/ci/runner.py`, `.env.example`, `tests/integration/test_deploy_with_slack.py` | U1 | W1 | shallow |

> 本例只有 2 个单元、且 U2 强依赖 U1 → **无并发收益**，实际按 W0 → W1 顺序执行。
> 并发收益出现在 ≥4 个互不依赖的单元时。如实报告"本例无并发收益"比强行凑并发更有价值。

## 验证清单
- [ ] **T1+T2、T3+T4 (impl, test) 配对均已 completed**（Phase 3 review 放行条件）
- [ ] 运行 `pytest tests/services/ tests/integration/ -v` — 全过
- [ ] 运行 `pytest tests/ --cov=src` — 全过, 整体覆盖率不下降
- [ ] 运行 `mypy src/` — 通过
- [ ] 运行 `ruff check .` — 通过
- [ ] 运行 `pytest tests/ci/test_runner.py` — 现有 CI 测试无回归
- [ ] T4 的 MANUAL_ACK_REQUIRED 项已由用户在 Phase 4 报告确认
```

**拆分与调度逻辑**:
- **R4 配对**: T1+T2 合成 U1, T3+T4 合成 U2 —— impl 与其 test 留在同一单元, 上下文连续
- **R3 预算**: 每个单元 3 个文件, 均在 ≤5 文件预算内
- **R1 冲突检测**: U1 与 U2 文件集不相交；但 U2 依赖 U1, 故分属 W0 / W1, 顺序执行
- **review 分档**: U1 含新模块 + 重试/降级逻辑 → `deep`（独立 review）；U2 是接线 + 配置 → `shallow`（批量）
- T4 示范 `MANUAL_ACK_REQUIRED`: 自动化测试覆盖 mock endpoint, staging 真实告警由用户在 Phase 4 确认
- 每个 test 任务的「验证方法」列出具体命令和期望信号, subagent 完成后逐项跑、逐项报
- 验证清单收尾时跑全量回归, 并把 (impl, test) 配对 completed 状态作为首要放行条件

---

## Example report

<a id="report"></a>

对应上面 design-plan / exec-plan 的执行结果。**已实现的修改报告**。

文件：`[docs/reports/YYYY-MM-DD-<feature>-report.md]`

```markdown
# Slack 通知集成 修改报告

## 变更摘要
为部署流水线添加 Slack 失败告警。新增 `SlackNotifier` 服务类, `DeployRunner` 在 lifecycle hook 调用, webhook URL 缺失时降级 stdout。共 5 个文件, +213 / -8 行, 单元测试覆盖率 94% (目标 90%), 集成测试覆盖 3 个场景。

## 波次执行摘要

| 单元 | 含任务 | 波次 | review 档 | 轮次 | 结果 |
|------|--------|------|-----------|------|------|
| U1 | T1, T2 | W0 | deep | 1 | ✅ 通过 |
| U2 | T3, T4 | W1 | shallow | 1 | ✅ 通过 |

- **基线 ref**: `a1b2c3d`（工作区干净，直接取 HEAD，未产生额外 commit）
- **并发收益**: 本例仅 2 个单元且 U2 强依赖 U1 → 无并发，按 W0 → W1 顺序执行
- **基线全量 diff**: `git diff a1b2c3d --stat` = 6 个文件，与 design-plan「涉及文件」一致，**无计划外改动**
- **subagent 中断**: 0 次

## 文件变更清单
| 操作 | 文件 | 说明 |
|------|------|------|
| A | src/services/slack_notifier.py | SlackNotifier 实现 (HTTP POST + 重试 + 降级) |
| M | src/services/__init__.py | 导出 SlackNotifier |
| A | tests/services/test_slack_notifier.py | 12 个测试, 覆盖正常/重试/降级路径 |
| M | src/ci/runner.py | on_step_complete hook 调用 notifier |
| A | tests/integration/test_deploy_with_slack.py | 3 个集成测试, mock webhook endpoint |
| M | .env.example | 追加 SLACK_WEBHOOK_URL 注释 |

## 测试结果
| 类型 | 命令 | 结果 |
|------|------|------|
| UT (新) | pytest tests/services/test_slack_notifier.py | ✅ 12 passed |
| IT (新) | pytest tests/integration/test_deploy_with_slack.py | ✅ 3 passed |
| UT (回归) | pytest tests/ci/test_runner.py | ✅ 47 passed (无回归) |
| 全量 | pytest tests/ | ✅ 215 passed |
| 覆盖率 (新) | pytest --cov=src.services.slack_notifier | ✅ 94% (≥ 90% 目标) |
| 整体覆盖率 | pytest --cov=src | ✅ 87% (维持, +0.2pp) |
| Type | mypy src/ | ✅ No issues |
| Lint | ruff check . | ✅ All checks passed |

## 关键决策
- **重试策略**: 指数退避 (1s, 2s, 4s), 最多 3 次, 总耗时 ≤ 7s。考虑过 5 次, 但失败反馈延迟超过 10s 时 on-call 已经开始排查, 通知价值降低
- **URL 脱敏**: 日志中只显示前 8 位 + `***` (e.g., `https://hooks.***`). 没用 hash 是为了便于人眼识别"是不是这个环境的 webhook"
- **失败时 notifier 自身失败处理**: `try/except Exception`, 记录到 `logger.exception`, 永不让通知失败中断部署
- **没做的事** (后续可加): 成功时静默可改为发"success"消息 (本期用户只要失败告警)
- **stage 环境手动验证**: 触发失败部署后, 12s 内 Slack 收到告警, 链接跳转正确

## 审查结果（review subagent 报告，按 design-plan「测试合同」逐条核对）

| 条款 | 优先级 | 验收命令 | 结果 | 证据 |
|------|--------|----------|------|------|
| N1: 失败时 Slack 推送告警 | 必测 | `pytest tests/integration/test_deploy_with_slack.py::test_failure_alerts_slack -v` | ✅ PASS | 1 passed, mock webhook 收到 payload 含 "severity": "error" |
| N2: 5xx 重试 3 次 | 必测 | `pytest tests/services/test_slack_notifier.py::test_5xx_retry_3_times -v` | ✅ PASS | 1 passed, 日志含 3 条 retry N/3 |
| N3: URL 缺失降级 stdout | 必测 | `pytest tests/services/test_slack_notifier.py::test_url_missing_fallback_stdout -v` | ✅ PASS | 1 passed, captured stdout 含告警文本 |
| N4: 覆盖率 ≥ 90% | 可推迟 | `pytest --cov=src.services.slack_notifier tests/services/test_slack_notifier.py` | ✅ PASS | 94% (≥ 90% 目标) |
| N5: URL 脱敏日志 | 必测 | `pytest tests/services/test_slack_notifier.py::test_url_masked_in_logs -v` | ✅ PASS | 1 passed, log 含 `https://hooks.***` |
| N6: staging 手动触发告警 | MANUAL_ACK_REQUIRED | 人工 | ⏳ SKIP | 等用户在 Phase 4 阅读时勾选 |

**验收循环次数**: 所有必测条款 1 次通过, 未触发重派纪律。
**剩余人工验收项**: N6 等用户在 Phase 4 报告勾选。
```

---

## Example review report

<a id="review-report"></a>

对应上面 design-plan 的「测试合同」+ exec-plan 的实现结果。**review subagent 跑出来的审查报告**（review subagent 不写文件，直接返回 markdown，由 orchestrator 归档到 Phase 3 report 中）。

```markdown
# 审查报告 — Slack 通知集成

> 审查模式报告。按 design-plan 「测试策略」节合同条款逐条核对。
> 审查人: review subagent
> 审查时间: 2026-06-12 16:42

## 合同条款汇总

| 条款 | 优先级 | 验收命令 | 结果 | 证据 |
|------|--------|----------|------|------|
| N1: 失败时 Slack 推送告警 | 必测 | `pytest ...::test_failure_alerts_slack -v` | ✅ PASS | 1 passed, mock webhook payload 含 "severity":"error" |
| N2: 5xx 重试 3 次 | 必测 | `pytest ...::test_5xx_retry_3_times -v` | ✅ PASS | 1 passed, log: "retry 1/3", "retry 2/3", "retry 3/3" |
| N3: URL 缺失降级 stdout | 必测 | `pytest ...::test_url_missing_fallback_stdout -v` | ✅ PASS | 1 passed, captured stdout 含 "[DEPLOY-FAIL] ..." |
| N4: 覆盖率 ≥ 90% | 可推迟 | `pytest --cov=src.services.slack_notifier ...` | ✅ PASS | 94% (≥ 90% 目标) |
| N5: URL 脱敏日志 | 必测 | `pytest ...::test_url_masked_in_logs -v` | ✅ PASS | 1 passed, log 含 "https://hooks.***" |
| N6: staging 手动触发告警 | MANUAL_ACK_REQUIRED | 人工 | ⏳ SKIP | 等用户勾选 |

## 功能审查结论

实现与 design-plan 一致。`SlackNotifier` 在部署失败时正确调用 webhook，5xx 响应按指数退避重试 3 次，URL 缺失时降级 stdout。无安全隐患。

## 测试审查结论

测试覆盖了正常路径、重试降级、URL 脱敏、5xx 退避、缺失回退等场景。测试可通过，非弱测试。覆盖率 94% ≥ 90% 目标。

## 审查判定

✅ **通过** — 功能和测试均符合要求，无阻塞问题。

**关键点**:
- review subagent **不修改任何业务文件**, 只读审查 + 跑命令验证 + 贴证据
- 三种结果: ✅ PASS / ❌ FAIL（功能或测试不通过）/ ⏳ SKIP（MANUAL_ACK_REQUIRED）
- ❌ FAIL → 主 agent 重派实现 subagent（最多 3 轮）
- 审查报告由 orchestrator 收集，作为 Phase 3 report 的一部分
- `shallow` 档的批量 review 报告省略「合同条款汇总」逐条表，改为 `单元 | 命令 | 结果 | 判定` 一表，且只列 P0/P1
```

---

## Example reflection

<a id="reflection"></a>

样例背景: 跑完上面 Slack 通知任务, 触发反思 (因为 Phase 3.6 整体 review 时发现一处 type 注解不严格, subagent 自己修复了 — 属于"执行流畅但 workflow 设计有优化空间"触发条件)。

文件：`[~/.yushan/skills/sequential-workflow/learned/YYYY-MM-DD-HHMM.md]`

```markdown
# 2026-06-03 14:30 — sequential-workflow 反思

## 本次任务类型
- 6 文件修改 (5 新建 / 3 修改) 的 mid-size 功能
- 后端服务 + 集成测试, 涉及 2 个目录
- 总耗时 ~2 小时, 全自动跑通 (无 subagent 中断)

## 触发阶段
- Phase 3 (Execute) — 整体 review 时 subagent 自己发现 type 注解问题, 重跑了一次
- 不是"挂掉", 是 subagent 自审时改了一个小问题

## 问题描述
- workflow 假设: subagent 在 `<verification>` 里勾完所有 checkbox 就完成了
- 实际情况: subagent 的自审 (instructions step 7) 触发了额外修复, 修复后又跑了一遍测试
- 体验上: 这是好事 (subagent 严谨), 但 orchestrator 收到"自审修复"信号时, 没有标准处理路径 — 这次是接受了, 下次可能要重新跑 review

## 根因
- Phase 3 实现 subagent 的判定分支只区分 "✅ 通过" 和 "❌ 中断/未通过"
- 缺少中间态: "✅ 通过但有 subagent 自审修复" — 需要 orchestrator 决定是接受还是再 review 一次

## 对 workflow 的改进建议
1. **自审结果显式化**: subagent 返回消息模板里加一行 `self_audit_fixes: <list of changes made during self-audit>`, 哪怕为空
2. **orchestrator 判定分支扩展**:
   - 旧: ✅ 通过 / ❌ 中断
   - 新: ✅ 干净通过 / ✅ 通过但有自审修复 (轻量再 review 关键文件) / ❌ 中断
3. **不是阻塞项**: 当前 subagent 自审修复几乎都是好的, 不需要每次都重 review。可以默认接受, 只在 fix 数 ≥ 2 时升级为"重 review"

## 参考案例
<!-- 模板形式 (引用本次任务产生的具体文件路径), 不是依赖某条历史记录 -->
- design-plan: `docs/design-plans/<本任务 slug>.md`
- exec-plan: `docs/exec-plans/<本任务 slug>.md`
- report: `docs/reports/<本任务 slug>-report.md`
- (可选) subagent 自审修复的位置: `<文件:行号> (<修改内容>)`
```
