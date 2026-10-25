# Quick — 轻量快速流程

## 适用场景
- 小改动、简单功能
- 涉及文件少（≤3 个）
- 不需要独立的 Write 阶段，设计内容可以合并到 subagent 提示中
- **自动放行最常命中的方案** —— 设计已就绪且核实通过时，Phase 1 免签字直接开工

## 执行阶段

```
跳过 Write 阶段
Phase 3  Execute: 基线 → 通常只有 1 个单元 → 实现 subagent → review subagent
Phase 4  Finalize: 归档
```

## 注意事项

- design-plan 的内容直接在 subagent 提示的 `<design_context>` 块中给出
- 无需创建 `docs/design-plans/` 和 `docs/exec-plans/`
- 基线（`scripts/baseline.sh`）仍不可跳过 —— 单单元同样需要字节级证据
- review 环节不可跳过；单单元时按 [3.4](../REFERENCE.md#phase-3) 的规则定 deep 或 shallow
