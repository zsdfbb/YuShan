#!/usr/bin/env python3
"""巨潮财报取回与解析：公告检索 -> PDF 下载 -> Markdown + CSV。

设计取舍：
- **只用标准库 + pdfplumber**。不依赖 akshare（它背后是同一批 HTTP 接口，
  但会拉进 pandas 等一整套依赖）。巨潮的检索/详情接口本来就是公开 HTTP。
- **正文用 pdfminer 的 extract_text，表格用 pdfplumber 的 extract_tables**。
  两者底层同为 pdfminer；后者多了边框/空白线版面分析，能还原合并表头。
  正文不需要版面分析，用轻的那条路径。
- **不做 OCR**。A 股与港股财报均为数字型 PDF（有文本层），OCR 只会引入
  字形误识别风险。脚本检测到无文本层时明确报错，交由用户决定。

输出布局（`--out` 之下）：

    <code>/<period>-<kind>/
    ├── meta.json          元数据（七列：as_of/period_type/source/...）
    ├── <period>-<kind>.md 全文，章节带 ## 锚点
    └── tables/
        └── *.csv          检测到的表格，每个一份
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import re
import sys
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

CNINFO = "http://www.cninfo.com.cn"
UA = {"User-Agent": "Mozilla/5.0 (compatible; company-report/1.0)"}

# 公告类型 -> 巨潮 category 过滤值。空串表示不过滤。
# 港股（column=hke）不认这套 category，改由标题关键词过滤。
CATEGORY = {
    "annual": "category_ndbg_szsh",
    "semi": "category_bndbg_szsh",
    "quarter": "category_yjdbg_szsh",
}

# 标题关键词。A 股与港股用语不同（年报/年報、半年度报告/中期报告），两套都要认。
TITLE_HINT = {
    "annual": ["年度报告", "年度報告", "年報", "年报", "Annual Report", "全年業績", "全年业绩"],
    "semi": ["半年度报告", "半年度報告", "中期报告", "中期報告", "中期業績", "中期业绩"],
    "quarter": ["季度报告", "季度報告", "第三季度", "第一季度"],
}

# 需要排除的变体：摘要、英文版、更正、已取消。默认优先选正文简体版。
EXCLUDE_HINT = ["摘要", "英文", "English", "更正", "取消", "已取消", "補充", "补充"]


def die(msg: str) -> "NoReturn":
    print(f"错误: {msg}", file=sys.stderr)
    raise SystemExit(1)


def _post(url: str, data: dict) -> str:
    body = urllib.parse.urlencode(data).encode()
    req = urllib.request.Request(
        url, data=body,
        headers={**UA, "Content-Type": "application/x-www-form-urlencoded"},
    )
    with urllib.request.urlopen(req, timeout=30) as r:
        return r.read().decode("utf-8", "ignore")


# ─────────────────────────── 第 1 步：代码/名称 -> 证券标识 ───────────────────────────


def resolve(query: str) -> dict:
    """把用户输入解析成 {code, orgId, name, column}。

    `column` 决定后续走 A 股还是港股分支：A 股 szse/sse 共用 szse 通道，
    港股固定 hke。解析不到就明确报错——静默返回空会让上层瞎猜代码。
    """
    raw = _post(f"{CNINFO}/new/information/topSearch/query",
                {"keyWord": query, "maxNum": "10"})
    try:
        hits = json.loads(raw)
    except json.JSONDecodeError:
        die(f"证券检索返回非 JSON：{raw[:120]}")
    if not hits:
        die(f"未找到「{query}」。请改用 6 位代码（A 股如 600519，港股如 09633）。")

    # 精确代码优先；否则取第一条（巨潮按相关度排序）。
    pick = next((h for h in hits if str(h.get("code")) == query), hits[0])
    code = str(pick.get("code") or "")
    if not code:
        die(f"检索结果缺少代码：{pick}")
    # 港股代码为 5 位数字；A 股为 6 位。
    column = "hke" if len(code) <= 5 else "szse"
    return {"code": code, "orgId": pick.get("orgId", ""),
            "name": pick.get("zwjc") or pick.get("ggjc") or "", "column": column}


# ─────────────────────────── 第 2 步：检索公告 ───────────────────────────


def list_announcements(sec: dict, kind: str, start: str, end: str) -> list[dict]:
    data = {
        "pageNum": "1", "pageSize": "30", "column": sec["column"], "plate": "",
        "stock": f"{sec['code']},{sec['orgId']}", "tabName": "fulltext",
        "searchkey": "", "secid": "",
        "category": CATEGORY.get(kind, "") if sec["column"] != "hke" else "",
        "trade": "", "seDate": f"{start}~{end}",
        "sortName": "", "sortType": "", "isHLtitle": "true",
    }
    j = json.loads(_post(f"{CNINFO}/new/hisAnnouncement/query", data))
    return j.get("announcements") or []


def pick_best(anns: list[dict], kind: str) -> dict:
    """挑出目标报告。排除摘要/英文版/更正稿，优先简体正文。"""
    hints = TITLE_HINT[kind]
    cand = [a for a in anns
            if any(h in (a.get("announcementTitle") or "") for h in hints)]
    if not cand:
        die(f"未找到 {kind} 类型的公告。可用标题示例："
            f"{[a.get('announcementTitle') for a in anns[:5]]}")
    primary = [a for a in cand
               if not any(x in (a.get("announcementTitle") or "") for x in EXCLUDE_HINT)]
    return (primary or cand)[0]


# ─────────────────────────── 第 3 步：下载 ───────────────────────────


def download(adjunct_url: str, dest: Path) -> int:
    url = f"http://static.cninfo.com.cn/{adjunct_url.lstrip('/')}"
    req = urllib.request.Request(url, headers=UA)
    with urllib.request.urlopen(req, timeout=120) as r:
        blob = r.read()
    # 魔数校验：不信 Content-Type，也不信扩展名。
    if blob[:4] != b"%PDF":
        die(f"下载内容不是 PDF（前 8 字节 {blob[:8]!r}）：{url}")
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_bytes(blob)
    return len(blob)


# ─────────────────────────── 第 4 步：解析 ───────────────────────────

# A 股一级章节：「第 N 节 xxx」。
# 不能把「一、二、」这种编号段落也算进来：它们在年报里出现 100+ 次，
# 且多数是正文小标题（如「二、公司全体董事出席董事会会议。」），
# 会让目录变成噪声。
SECTION_CN = re.compile(
    r"^\s*(第[一二三四五六七八九十]+节\s*\S.{0,40}"
    r"|(?:Section|SECTION)\s+\d+.{0,40})\s*$"
)

# 港股走国际财报格式，**没有「第 N 节」**。它的章节是固定命名的独立标题行，
# 且常与英文标题同页并列。用白名单比启发式可靠：港股年报章节就这十来个。
SECTION_HK = re.compile(
    r"^\s*("
    r"董事長致辭|主席報告|首席執行官報告|管理層討論與分析|業務回顧|財務回顧"
    r"|董事會報告|企業管治報告|環境、社會及管治報告|可持續發展報告"
    r"|董事及高級管理層|董事會及高級管理層"
    r"|獨立核數師報告|核數師報告"
    r"|綜合財務狀況表|綜合損益表|綜合現金流量表|綜合權益變動表"
    r"|財務報表附註|財務報表"
    r"|五年財務摘要|財務摘要"
    r"|(?:CHAIRMAN.S STATEMENT|MANAGEMENT DISCUSSION AND ANALYSIS"
    r"|DIRECTORS. REPORT|CORPORATE GOVERNANCE REPORT"
    r"|INDEPENDENT AUDITOR.S REPORT|FINANCIAL STATEMENTS"
    r"|CONSOLIDATED STATEMENT OF FINANCIAL POSITION"
    r"|CONSOLIDATED STATEMENT OF PROFIT OR LOSS|FIVE-YEAR FINANCIAL SUMMARY)"
    r")\s*$"
)


def _md_table(rows: list[list[str]]) -> list[str]:
    """列表 -> GitHub 风格 markdown 表格。第一行作表头（缺分隔行会被渲染器忽略）。"""
    if not rows:
        return []
    ncol = max(len(r) for r in rows)
    pad = lambda r: [c.replace("|", "\\|") for c in r] + [""] * (ncol - len(r))
    out = ["", "| " + " | ".join(pad(rows[0])) + " |",
           "| " + " | ".join(["---"] * ncol) + " |"]
    out += ["| " + " | ".join(pad(r)) + " |" for r in rows[1:]]
    out.append("")
    return out


def to_markdown(pdf_path: Path) -> tuple[str, list[list[list[str]]], int]:
    """返回 (markdown, 所有表格, 页数)。

    表格按 **bbox 位置**内联到正文里：`find_tables()` 给出 y 区间，
    `extract_text_lines()` 给出每行 top。落在表格区间内的正文行跳过
    （其内容已由表格承载），到表格 top 处插入 markdown 表格。
    这样同一份内容只出现一次，且保序。
    """
    import pdfplumber

    md: list[str] = []
    tables: list[list[list[str]]] = []
    with pdfplumber.open(str(pdf_path)) as pdf:
        npages = len(pdf.pages)
        for page in pdf.pages:
            found = page.find_tables()
            # `emitted` 与 `blocks` 下标对齐：表格插入后置 True，避免重复插入。
            # 不能靠 `rows[:] = []` 标记——`tables` 与 `blocks` 必须指向**不同**
            # 的 list，否则清空会把存给 CSV 的那份也一起清掉。
            blocks: list[tuple[float, float, list[list[str]], bool]] = []
            for t in found:
                rows = [[(c or "").replace("\n", " ").strip() for c in row]
                        for row in t.extract()]
                if len(rows) < 2 or max((len(r) for r in rows), default=0) < 2:
                    continue
                blocks.append((t.bbox[1], t.bbox[3], rows, False))
                tables.append(rows)

            lines = page.extract_text_lines()
            for ln in lines:
                top = ln["top"]
                hit = next(((i, b) for i, b in enumerate(blocks)
                            if b[0] - 2 <= top <= b[1] + 2), None)
                if hit is not None:
                    idx, blk = hit
                    # 表格首行所在的位置插入整张表，其余行跳过。
                    if not blk[3] and abs(top - (blk[0] - 2)) < 60:
                        md += _md_table(blk[2])
                        blocks[idx] = (blk[0], blk[1], blk[2], True)
                    continue
                s = ln["text"].rstrip()
                if not s.strip():
                    md.append("")
                elif SECTION_CN.match(s) or SECTION_HK.match(s):
                    md.append(f"\n## {s.strip()}\n")
                else:
                    md.append(s)
            md.append(f"\n<!-- page {page.page_number} -->\n")
    return "\n".join(md), tables, npages


def write_csvs(tables: list[list[list[str]]], out_dir: Path) -> list[str]:
    out_dir.mkdir(parents=True, exist_ok=True)
    written: list[str] = []
    for i, tbl in enumerate(tables, 1):
        # 只有 ≥2 列且 ≥2 行才算真表格；单列多是段落误判。
        if len(tbl) < 2 or max((len(r) for r in tbl), default=0) < 2:
            continue
        name = f"table-{i:02d}.csv"
        with (out_dir / name).open("w", newline="", encoding="utf-8") as f:
            csv.writer(f).writerows(tbl)
        written.append(name)
    return written


# ─────────────────────────── 主流程 ───────────────────────────


def main() -> None:
    ap = argparse.ArgumentParser(description="巨潮财报 -> Markdown/CSV")
    ap.add_argument("company", help="6 位代码或公司名（如 600519 / 贵州茅台 / 09633）")
    ap.add_argument("--out", required=True, help="输出根目录")
    ap.add_argument("--kind", default="annual",
                    choices=["annual", "semi", "quarter"], help="报告类型，默认 annual")
    ap.add_argument("--year", default="", help="年份（如 2025）；留空取最近一期")
    ap.add_argument("--csv", action="store_true", help="额外抽取表格到 CSV")
    ap.add_argument("--force", action="store_true", help="忽略缓存，强制重新下载")
    args = ap.parse_args()

    sec = resolve(args.company)
    year = args.year or str(datetime.now().year)
    # 报告有滞后：2025 年报通常在 2026 上半年披露。检索窗口前后各放宽一年。
    start = f"{int(year) - 1}-01-01"
    end = f"{int(year) + 1}-12-31"

    anns = list_announcements(sec, args.kind, start, end)
    target = pick_best(anns, args.kind)
    title = target.get("announcementTitle") or ""
    period = args.year or (target.get("announcementTime") and
                           str(datetime.fromtimestamp(
                               target["announcementTime"] / 1000, timezone.utc).year)) or year

    root = Path(args.out).expanduser() / sec["code"] / f"{period}-{args.kind}"
    root.mkdir(parents=True, exist_ok=True)

    meta_path = root / "meta.json"
    if meta_path.exists() and not args.force:
        print(f"已存在，跳过下载：{meta_path}（--force 可覆盖）")
        return

    pdf_path = root / f"{period}-{args.kind}.pdf"
    size = download(target["adjunctUrl"], pdf_path)
    print(f"已下载 {size / 1024:.0f} KB -> {pdf_path}", file=sys.stderr)

    md, tables, npages = to_markdown(pdf_path)
    if not md.strip():
        die("PDF 无可提取文本（可能是扫描件）。本工具不做 OCR，"
            "请先用 pdf-analysis skill 的 --ocr 转为带文本层的 PDF。")

    md_path = root / f"{period}-{args.kind}.md"
    md_path.write_text(md, encoding="utf-8")
    csv_names = write_csvs(tables, root / "tables") if args.csv else []

    meta = {
        "as_of": datetime.now(timezone.utc).astimezone().isoformat(timespec="seconds"),
        "code": sec["code"],
        "name": sec["name"],
        "market": "HK" if sec["column"] == "hke" else "A",
        "kind": args.kind,
        "period": period,
        "title": title,
        "source": "cninfo",
        "announcement_id": target.get("announcementId"),
        "adjunct_url": target.get("adjunctUrl"),
        "pages": npages,
        "pdf_bytes": size,
        "markdown_chars": len(md),
        "tables": len(tables),
        "csv": csv_names,
        "fetched_via": "company-report/1.0",
    }
    meta_path.write_text(json.dumps(meta, ensure_ascii=False, indent=2), encoding="utf-8")

    print(f"完成：{md_path}（{npages} 页，{len(md)} 字符，表格 {len(tables)} 个）")


if __name__ == "__main__":
    main()
