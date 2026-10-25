---
name: pdf-analysis
description: |
  Extract and analyze PDF content — text, tables, and scanned pages. Prefers a local text extractor, falls back to MarkItDown pip install. Use when the user mentions a PDF file, asks to read/summarize/analyze a .pdf, or needs content pulled out of a PDF report, paper, contract, or form.
---

# PDF 分析

## 一句话流程

**先确认文件** → **探测可用的提取器** → **分页提取到落盘文件** → **用 `read` 续读分析**。

不要一次性把整个 PDF 读进上下文。PDF 抽出的文本通常是页数的数十倍，一次性读会：
① 打爆上下文；② 后面的页永远看不到；③ 无法回答「第 7 页说了什么」这类追问。

## 第 1 步：确认文件

```bash
ls -la "<path>.pdf"
```

**先验证它真是 PDF**（扩展名会说谎）。PDF 魔数是文件开头四个字节 `%PDF`：

```bash
head -c 4 "<path>.pdf" | od -c | head -1
```

如果是加密 PDF（`head -c 4` 正常但提取报密码错），见下面「加密 / 扫描件」。

## 第 2 步：探测提取器（**skill 自带安装脚本**）

本 skill 自带 `scripts/setup.sh` —— **检测、安装、验证都在它里面**，不要自己拼包管理器命令。

```bash
# <skill 目录> 就是本 SKILL.md 的 location 的父目录
bash <skill 目录>/scripts/setup.sh --check
```

`--check` 是**只读**的（不装任何东西），头两行固定为机器可读格式：

```
PDF_BACKEND=pdftotext     # 已就绪的后端；没有则为空
PDF_READY=1               # 1 = 可以提取；0 = 不能（此时退出码 3）
```

### 按 `PDF_READY` 分派

- **`PDF_READY=1`** ⇒ 直接用 `PDF_BACKEND` 指示的工具，跳到第 3 步。
- **`PDF_READY=0`** ⇒ 跑安装：

```bash
bash <skill 目录>/scripts/setup.sh                        # 装第一个可用后端
bash <skill 目录>/scripts/setup.sh --backend pdfminer     # 指定后端
bash <skill 目录>/scripts/setup.sh --dry-run              # 只看看会跑什么
bash <skill 目录>/scripts/setup.sh --ocr                  # 顺带检查/安装 OCR
```

`setup.sh` 的行为（你要据此向用户解释）：

| 情况 | 脚本行为 | 你该怎么说 |
|---|---|---|
| 已装 | 直接成功，不重复装 | 「已就绪」 |
| 需要 brew / pip | **直接装**（不需提权） | 装完报告用了哪个后端 |
| 需要 `apt-get` 等系统包 | **打印命令后退出码 4，不擅自 sudo** | 把命令原样转给用户，请其执行后继续 |
| 装完仍探测不到 | 非零退出 + 明确报错 | 如实报错，**不要**假装可用 |

> **为什么安装写进 skill**：模型实测经常「发现问题 → 停下来问用户 → 用户说随便 → 又停下来」。
> `setup.sh` 把这条路一次走完，且**装完自己验一次**（不相信安装命令的退出码）。

### 后端优先级

| 优先 | 后端 | 安装代价 | 为什么排这里 |
|---|---|---|---|
| 1 | **`pdftotext`**（Poppler） | 系统包 | 最快、无 Python 启动开销。**首选** |
| 2 | **`mutool`**（MuPDF） | 系统包 | 能处理部分 `pdftotext` 失败的字体 |
| 3 | **`pdfminer`** | **纯 pip，无系统依赖** | 无包管理器时的退路；**MarkItDown 的 PDF 后端就是它** |
| 4 | **`markitdown`** | pip（含 pdfplumber） | 只在需要 Office 格式时才值（见下） |

> 后两项装进 skill 自己的 `.venv`（`scripts/.venv`），**不污染系统 Python**，也绕开
> Homebrew / Debian 12+ 的 PEP 668 `externally-managed-environment`。装完的调用要写
> venv 里的绝对路径，`setup.sh` 会把该命令打出来。

### 什么时候才用 MarkItDown

MarkItDown 的价值在**多格式通吃**（PDF / DOCX / XLSX / PPTX 一套接口），**不在 PDF 本身**——
它的 PDF 转换器内部就是 pdfplumber + pdfminer 的回退组合，对纯 PDF 相比直接用 `pdfminer`
没有质量优势，却多拉一层包装与 `pdfplumber`（含 `pdfminer`）。

- **只处理 PDF** ⇒ 用第 1-3 项，别装 MarkItDown。
- **同时有 Word/Excel/PPT** ⇒ `--backend markitdown`，一次装齐。

## 第 3 步：提取到落盘文件（关键）

### 3a. 按页提取（首选，支持「第 N 页」追问）

用 `pdftotext` 的 `-f` / `-l` 取页范围。**先拿总页数**：

```bash
pdfinfo "<path>.pdf" | grep -i pages
# 没有 pdfinfo 时的后备：数分页符
pdftotext "<path>.pdf" - | tr -cd '\f' | wc -c
```

然后**按批提取**（每批 10-20 页，按文档密度调整）：

```bash
mkdir -p .yushan/pdf
pdftotext -f 1 -l 15 -layout "<path>.pdf" .yushan/pdf/p1-15.txt
```

- **`-layout`** 保留原始排版（表格、多栏）。**默认就先加它**；如果结果里出现大量断裂的
  短行（多栏文档被横向拼接），改用不带 `-layout` 的版本对比一次。
- **`-enc UTF-8`** 显式指定编码（默认通常是 UTF-8，但某些旧版 poppler 会跟 locale 走）。
- 输出**必须落到文件**，不要 `| head` —— 管道截断会让模型以为文档就这么短。

用 `mutool` 时等价命令：

```bash
mutool draw -F txt -o .yushan/pdf/p1-15.txt "<path>.pdf" 1-15
```

用 `pdfminer` 时（按页循环，因为 CLI 不支持页范围）：

```bash
python3 - <<'PY'
from pdfminer.high_level import extract_text
text = extract_text("<path>.pdf", page_numbers=list(range(0, 15)))  # 0-based
open(".yushan/pdf/p1-15.txt", "w").write(text)
PY
```

### 3b. 一定要报出来的三件事

提取后**立刻检查**产出，并在回答里说明：

```bash
wc -l -c .yushan/pdf/p1-15.txt
```

1. **产出了多少**（行数 / 字节）—— 让后续的 `read` 知道规模。
2. **是否为空**（0 字节 ⇒ 极可能是**扫描件**，见第 4 步）。
3. **是否有替换字符**（`grep -c $'\uFFFD' file`）—— 出现 U+FFFD 说明字体缺 ToUnicode
   CMap，文本**不可信**，需要换提取器或改走 OCR。

## 第 4 步：用 `read` 续读分析

**始终用 `read` 打开落盘文件，不要用 `bash cat`**——`read` 带行号、统一截断、`offset` 续读：

```text
read path=".yushan/pdf/p1-15.txt"
```

- 文件长 ⇒ 按 `offset` 续读，直到覆盖需要的页。
- 需要定位某个主题 ⇒ 用 `grep` 在那个 txt 上搜，再 `read offset=` 看上下文。
  **这比反复读全文便宜得多。**

### 落盘位置

- **默认 `.yushan/pdf/`**（相对 workspace 根）。与 `.yushan/skills/` 同级，语义清楚，
  且通常已在 `.gitignore` 里。
- **绝不写 `/tmp`**：跨会话丢失，且下次追问「刚才那个 PDF」时文件已经没了。
- **绝不写 workspace 根**：会污染用户仓库、出现在 `git status`、可能被误提交。
- 用户仓库没有 `.yushan/` 时先建目录；若用户的 `.gitignore` 没盖住 `.yushan/`，
  **提醒一句**要不要加，但不要擅自改 `.gitignore`。

## 加密 / 扫描件 / 图片型 PDF

### 加密 PDF

```bash
pdfinfo "<path>.pdf" 2>&1 | grep -i encrypted
```

- **有打开密码** ⇒ 直接告诉用户「需要密码」，**不要尝试破解**。
- **只有权限密码**（能打开但不能复制/打印）⇒ `pdftotext` 通常仍可提取，先试。
  失败时用 `qpdf --decrypt in.pdf out.pdf` 处理（`qpdf` 不在 `setup.sh` 的后端表里，
  缺了就报给用户装：`brew install qpdf` / `apt install qpdf`）。

### 扫描件（无文本层）

**判断**：提取出 0 字节、或每页只有零星几个字符，但页数正常。

这是**图片型 PDF**——文本层根本不存在，任何文本提取器都拿不到内容。**只有 OCR 能解**：

```bash
# 先看本机有没有 OCR（只查不装）
bash <skill 目录>/scripts/setup.sh --check --ocr | grep PDF_OCR

# 没有则装（同样走 skill 的脚本，不要自己拼包管理器命令）
bash <skill 目录>/scripts/setup.sh --ocr
```

| 工具 | 说明 |
|---|---|
| `ocrmypdf` | 一条命令搞定（内部串 tesseract），**推荐** |
| `tesseract` | 需要先 `pdftoppm` 把页转成图片 |

```bash
# ocrmypdf：加文本层后再提取（最省事）
ocrmypdf --skip-text "<path>.pdf" .yushan/pdf/ocr.pdf
pdftotext -layout .yushan/pdf/ocr.pdf .yushan/pdf/ocr.txt

# 或纯 tesseract 路线
pdftoppm -r 300 -png "<path>.pdf" .yushan/pdf/page
tesseract .yushan/pdf/page-01.png stdout -l chi_sim+eng
```

- **300 DPI 起步**，低于 200 DPI 中文识别率会明显劣化。
- `-l chi_sim+eng` 需要装对应语言包（`tesseract-lang`）。
- **OCR 结果必须标注不确定性**：告诉用户「这是 OCR 结果，数字与专有名词可能有误」。
  **绝不把 OCR 出来的数字当作精确值直接引用**——尤其是金额、百分比、日期。

## 常见坑

| 症状 | 原因 | 处置 |
|---|---|---|
| 输出全是 `\uFFFD` | 字体缺 ToUnicode CMap | 换 `mutool`；或 OCR；或告知用户该页不可靠 |
| 中文变成乱码/拉丁字母 | 编码或 CID 字体映射问题 | 显式 `-enc UTF-8`；换 `mutool` |
| 多栏文档文字串行 | 默认按几何位置拼接 | 去掉 `-layout` 再试；或按页拆得更细 |
| 表格塌成一行 | 文本提取器没有表格结构 | 用 `pdftotext -layout`；复杂表格用 `pdfplumber` 的 `extract_tables()` |
| 页数对不上 | 有些 PDF 有隐藏页/封面 | 以 `pdfinfo` 为准，在回答里注明页码基准 |
| 提取很慢 | 上千页 | 先只提取目录页与前几页，确认值得再全量 |

## 回答用户时的报告口径

- **说清页码基准**：「第 7 页」指 PDF 第 7 页还是文档自己标的页码——两者常不一致。
- **说清覆盖范围**：「我读了第 1-15 页（共 42 页）」比「我看了这个 PDF」诚实。
- **区分原文与推断**：引号里的是原文；其余是你的概括。
- **扫描件/OCR 要明说**：见上。
