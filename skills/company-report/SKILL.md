---
name: company-report
description: 从巨潮资讯取回 A 股/港股的年报、半年报、季报 PDF，解析成带章节锚点的 Markdown 并抽取表格为 CSV。Use when 需要阅读公司财报、下载年报/季报、提取财务表格、做财务数据分析，或用户提到财报、年报、季报、巨潮、公告、利润表、资产负债表、现金流量表。
---

# Company Report

## Quick start

```bash
bash <skill 目录>/scripts/setup.sh          # 一次性：建 venv 装 pdfplumber
python3 <skill 目录>/scripts/fetch-report.py 600519 --out ./reports
```

产物：

```
reports/600519/2025-annual/
├── meta.json              元数据（代码/期次/来源/公告 ID/页数）
├── 2025-annual.md         全文，一级章节带 ## 锚点
└── tables/table-01.csv …  加 --csv 时每张表一份
```

**入参**：6 位 A 股代码（`600519`）、5 位港股代码（`09633`）或公司名（`贵州茅台`、`农夫山泉`）。

## 常用组合

```bash
# 指定年份与报告类型
fetch-report.py 600519 --out ./reports --kind annual --year 2025
fetch-report.py 600519 --out ./reports --kind semi
fetch-report.py 600519 --out ./reports --kind quarter --year 2026

# 提取表格（做财务分析时必带）
fetch-report.py 600519 --out ./reports --year 2025 --csv

# 强制重新下载（默认按 meta.json 缓存，同公司同期次只下一次）
fetch-report.py 600519 --out ./reports --year 2025 --force
```

| 选项 | 默认 | 说明 |
|---|---|---|
| `--kind` | `annual` | `annual` / `semi` / `quarter` |
| `--year` | 最近一期 | 报告年份（非披露年份） |
| `--csv` | 关 | 表格另存 CSV；**做计算时开** |
| `--force` | 关 | 忽略缓存 |

## 用产出做分析

**数字从 CSV 取，不要从 Markdown 抄。** Markdown 表格是给人读的；CSV 才是计算输入，列名完整、无合并单元格残留。

```bash
# 定位某科目在哪张表
grep -l "资产总计" reports/600519/2025-annual/tables/*.csv

# 读表
head -3 reports/600519/2025-annual/tables/table-80.csv
```

正文查证用 `grep` 定位章节，再 `read offset=`：

```bash
grep -n "^## " reports/600519/2025-annual/2025-annual.md   # 看目录
grep -n "直销" reports/600519/2025-annual/2025-annual.md
```

## 已知边界

| 情况 | 行为 |
|---|---|
| A 股 | 章节按「第 N 节」识别，稳定 |
| 港股 | 走国际财报格式，章节按固定命名白名单识别（`財務摘要` / `董事長致辭` / `綜合財務狀況表` …） |
| 港股排版 | 中英双栏并列，正文与译文交错，**这是原文件形态**，不是解析错误 |
| 摘要 / 英文版 / 更正稿 | 自动排除，优先取正文简体版 |
| 扫描件（无文本层） | **明确报错**，不做 OCR。先用 `pdf-analysis` 的 `--ocr` 转成带文本层 PDF |
| 合并 + 母公司两套报表 | 都会抽到，靠正文位置区分；找科目时注意有两份同名行 |

## 故障排查

```bash
# 环境是否就绪
bash <skill 目录>/scripts/setup.sh --check        # PDF_READY=1 表示可用
```

| 症状 | 原因 | 处置 |
|---|---|---|
| `未找到「X」` | 名称不在巨潮检索库 | 改用代码 |
| `下载内容不是 PDF` | 该公告不是 PDF 形态 | 换 `--kind` 或 `--year` |
| `PDF 无可提取文本` | 扫描件 | 走 `pdf-analysis` 的 OCR 路径 |
| 表格数是 0 | 该 PDF 无框线表格 | 读正文；或换 `--year` 看别期 |
