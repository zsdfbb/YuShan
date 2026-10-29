# 搜索工具（切片一）— As-built

> 性质：**实际建成记录**。设计见 `docs/design-plans/completed/2026-09-16-search-tools.md`，
> 决策见 `docs/adr/0015-search-tools-via-external-binaries.md`，过程见 `docs/reports/2026-09-17-search-tools-report.md`。
> 日期：2026-09-17

## 一句话

coding agent 的工具从 4 个增至 **7 个**（`read`/`write`/`edit`/`bash` + `grep`/`find`/`ls`）。
搜索工具**优先外挂环境已有的 `rg`/`fd`**，缺失时**降级** POSIX `grep`/`find`，**绝不下载**。

## 落点

```
crates/ys-tool/src/truncate.rs       截断的唯一实现（Limit/Truncation/head/head_lines/tail/line）
crates/ys-tool/src/registry.rs       specs 按输入顺序收集（B1）
crates/ys-runtime/src/builder.rs     AgentBuilder::tools(Vec<Box<dyn Tool>>)
adapters/tools-basic/src/exec.rs     SearchBackends（探测）+ run_bounded（有界子进程）
adapters/tools-basic/src/{grep,find,ls}.rs
adapters/tools-basic/src/{bash,read}.rs  改用 truncate；bash 补 kill_on_drop（B3）
apps/coding-agent/src/toolset.rs     唯一装配接缝（探测一次 + 建工具表 + 降级提示）
apps/coding-agent/src/prompt.rs      build_system_prompt(cwd, &[ToolSpec])，清单从 registry 派生
```

## 关键行为（实现期定死，勿凭直觉改）

**三个上限**（单位不同）：`limit` 条命中（模型可见，默认 100）/ `byte_cap` 1 MiB（内部，`stdout.len() >= byte_cap` 判定，**不依赖 `stop == Cap`**）/ `truncate::head` 2000 行·50KB（输出）。各自 notice 都要报出。

**退出码：先看 `stop` 再看 `code`**。`stop != Eof`（我们杀的：Cap/Aborted/TimedOut）→ 忽略 `code`、**不是错误**；`1` = 无命中（**不是错误**）；`Eof + code 0` 但零 match 事件 → **格式漂移报错**；`Eof + >=2` → `is_error=true` + stderr 前 4 KiB。

**子进程**：一律**直接 spawn 二进制**（`rg`/`grep`/`fd`/`find`），**绝不 `sh -c`**。spawn 时进**独立进程组**，达上限/中止/超时杀的是**整个进程组** —— 只杀直接子进程会让 `sh -c '… &'` 形态的孙进程漏网成 `ppid=1` 的孤儿，并攥住管道写端使排空任务等不到 EOF。唯一未覆盖的仍是「直接子进程**自然退出**、孙进程仍攥着写端」那条路径（无「我们杀的」这一步，进程组不可安全定位），故**不 fork 依旧是调用前提**。argv **恒带路径参数**。两个管道都排空到 EOF（防死锁）。工具自带 **60s 兜底 deadline**（因 `-p`/`--json` 下 `boundary` 恒为 `None`）。

**探测**：`SearchBackends::detect_from(path)` 纯 PATH 扫描 + 可执行位检查，**不 spawn、不探版本**；注入式（非 `OnceLock`）→ 降级路径可确定性测试。`fd` 认 Debian 别名 `fdfind`。

**降级语义**：排除目录 `.git`/`target`/`node_modules`/`dist`/`build`/`.venv` **只作用于降级路径**（rg/fd 自带 `.gitignore`）。原生侧**加 `--hidden` 并用 `--glob=!.git/` 挡对象库** —— 两个参数是一对，只出现任一个都是错的（单独 `--hidden` 搜进 `.git/`；单独排除则 `.github/workflows/*.yml` 等隐藏路径不可搜）。含 `/` 的 glob 在 rg 下归一为 `**/` 前缀；以 `/` 开头的 glob 明确报错。降级时启动期 stderr 一行提示。

**`ls`**：纯 `read_dir`，**不起子进程**；扫描上限 20000 条；大小写不敏感排序；目录带 `/`；含 dotfile。

**提示词**：清单由 `&[ToolSpec]` 派生（顺序即工具表顺序）；snippet/guideline 仍留 `prompt.rs` 按 name 查表（`ToolSpec` **未加字段**）；有漂移测试守「每个工具都有非兜底 snippet」。

## 已知缺口

1. `find.rs`：以 `/` 开头的 glob（`/src/*.rs`）经 `fd --full-path` 仍静默返回空（罕见路径）。
2. `bash` 的 `kill_on_drop` 只杀直接子进程；无 boundary 超时路径对 `sh -c` 的**孙进程**不覆盖。
3. ~~切片二（`websearch`/`webfetch`）未启动~~ —— **已完成**（2026-09-18）：as-built 见 **`docs/design-final/tools-web.md`**，
   决策见 **ADR-0016**；两个未决项已定：搜索后端链用一张 `const TABLE` + 两个 `match`（不建 trait），`HTML→Markdown` 引入 `htmd`。
4. 变异测试靠手工注入（`cargo-mutants` 未安装）。

## 测试资产

- `crates/ys-tool/src/truncate.rs`：19 条（含 `tail` 字节闸为约束、`head` 首行保留、UTF-8 字符切分）
- `adapters/tools-basic/src/exec.rs`：13 条（四态 + stderr 狂写不挂 + **越限杀满进程组** + 探测 + F1 护栏）
- `adapters/tools-basic/src/{grep,find,ls}.rs`：30 条（退出码四态、F1、原生解析用**假二进制注入**、降级排除、glob、ls 上限）
- `apps/coding-agent/src/toolset.rs`：5 条（顺序、穿透 agent、降级提示、不 print 源码扫描）
- `apps/coding-agent/src/prompt.rs`：5 条（派生顺序、漂移、snippet 字面量）
- `apps/coding-agent/tests/{integration,degraded_search}.rs`：grep 经循环端到端；CLI 降级（stderr 有提示 + **stdout 为空**）
