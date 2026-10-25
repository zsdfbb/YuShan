# Bugfix — Bug 修复流程

## 适用场景
- 已知问题定位修复（有明确 bug 描述）
- 不需要重新设计，直接定位原因并修复

## 执行阶段

```
跳过 Write 阶段
Phase 3  Execute: 基线 → 通常 1–2 个单元（多为串行，修复之间常有依赖）→ 分档 review
Phase 4  Finalize: 归档
```

## 注意事项

- design-plan 的"背景"和"设计"部分直接在 subagent 提示中说明
- 修复 subagent 必须包含：定位原因 → 编写修复 → 添加回归测试 → 跑全部测试
- review subagent 重点检查：修复是否正确、测试是否覆盖了原始 bug 场景
- **bugfix 默认按 deep 档 review** —— 改动既有逻辑，回归风险高
- 基线（`scripts/baseline.sh`）不可跳过 —— 只有拿到字节级 diff，才能证明"只改了该改的地方"
