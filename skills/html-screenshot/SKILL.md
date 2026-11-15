---
name: html-screenshot
description: 把 HTML 文件或网页 URL 渲染成 PNG 截图，支持整页、元素、高 DPI。Use when 需要把 HTML 变成图片、给网页/报告截图、检查页面渲染效果、验收 ECharts 等图表是否画对、或用户提到截图、截屏、渲染成图、页面长什么样、看图验收。
---

# html-screenshot

把 HTML 文件或 URL 渲染成 PNG。**只截页面渲染，不截屏幕** ——
拍桌面/窗口是另一条需求（三套系统工具 × 三套权限模型），本 skill 不做。

脚本：`scripts/shot.py`（`<skill 目录>` 指本 SKILL.md 所在目录）

## Quick start

```bash
P="<skill 目录>/scripts/shot.py"

# 整页截图（默认，2x DPI）
python3 "$P" report.html -o out.png

# 只截视口
python3 "$P" report.html -o out.png --no-full-page

# 只截某个元素
python3 "$P" report.html -o chart.png --selector "#chart"

# 网页
python3 "$P" https://example.com -o page.png
```

依赖：**仅标准库**（Python 3.8+）。需要本机有 Chrome 系浏览器，脚本自动探测
（Playwright 缓存的 `chrome-headless-shell` 优先，其次系统 Chrome/Chromium/Edge）。
没有时用 `HTML_SHOT_BROWSER=<路径>` 指定。**不需要装 playwright、不需要 venv。**

成功退出码 0，打印 `路径 宽x高 大小 非白占比`。

## 参数

| 参数 | 默认 | 说明 |
|---|---|---|
| `-o/--output` | 必填 | 输出 PNG 路径，目录不存在会建 |
| `--width` | 1280 | 视口宽度 |
| `--height` | 900 | 视口高度（整页模式下只影响首屏布局） |
| `--scale` | 2.0 | 设备像素比。2 出 2x 图，报告用；`1` 出原尺寸 |
| `--no-full-page` | — | 只截视口 |
| `--selector` | — | 只截该 CSS 选择器匹配的元素（优先于整页） |
| `--wait-ms` | 600 | load 后额外等待毫秒 |
| `--wait-for` | — | 等该选择器出现再截（比定时更可靠） |
| `--timeout` | 20 | 等 load 的超时秒数 |
| `--browser` | 自动 | 指定浏览器可执行文件 |

## 三个必须知道的失败模式

这个脚本**宁可报错也不出假图**，因为「拿到一张成功的空图」比失败更糟。
三条防线：

### 1. 全白截图一律判失败

不存在的文件、JS 没跑完、选择器指向不可见元素 —— Chrome 在这些情况下
**仍然 exit 0 并写出一张白图**。脚本截图后算非白像素占比，`< 0.1%` 就删文件并报错。

实测对照：正常页面 1%~37%，空白页 **0.00%**。

看到「渲染结果几乎全白」时：加大 `--wait-ms`（JS 渲染的页面给 2000~5000），
或用 `--wait-for '#chart canvas'` 等图表真正画出来。

### 2. 导航失败由 CDP 捕获，不看退出码

`file://` 找不到文件、域名解析失败都会返回 `errorText`，脚本直接报错退出。

### 3. 整页高度实测，不是猜的

Chrome 的 `cssContentSize` 会被视口高度**撑大**（内容只有 400px 的页面
在 900px 视口下报 900）。脚本测量前把视口压到 60px，拿到真实高度再截。

超过 20000px 会截断并告警（Chrome 纹理工位上限，超出会出空白条带）。

## 常见组合

```bash
# ECharts / Chart.js：等图表画完再截
python3 "$P" chart.html -o chart.png --wait-ms 3000

# 等具体元素出现（更可靠）
python3 "$P" chart.html -o chart.png --wait-for "#chart canvas"

# 长报告：整页 + 2x，再缩放到合适体积
python3 "$P" report.html -o full.png
python3 -c "from PIL import Image; im=Image.open('full.png'); im.resize((im.width//2,im.height//2)).save('full-1x.png')"

# 单页 PDF（用 sips，macOS 自带）
python3 "$P" report.html -o r.png && sips -s format pdf r.png --out r.pdf

# 高而窄的移动端页面
python3 "$P" page.html -o m.png --width 390 --height 844
```

## 用产出做分析

截完用 `view_image` 看。**这是本 skill 的落点** —— 只看 HTML 源码看不出
图表元素重叠、标题被裁、图例遮挡，这些事实**只存在于渲染后的像素里**。

适合截图验收的场景：

- 报告类 HTML（含 ECharts / Chart.js 图表）的最终形态
- 页面布局是否错位、文字是否溢出、中文字体是否正常
- 表格是否挤在一起、数字对齐是否对

**不适合**的场景：能用文本/日志判断的问题。字符网格类（TUI）用测试断言，
比截图快几个数量级且可变异测试。

## 故障排查

| 现象 | 原因与处理 |
|---|---|
| `未找到 Chrome 系浏览器` | 装 Chrome，或 `HTML_SHOT_BROWSER=/path/to/chrome` |
| `导航失败（net::ERR_FILE_NOT_FOUND）` | 路径写错；脚本先检查文件存在性 |
| `渲染结果几乎全白` | JS 没跑完 → 加 `--wait-ms 3000`；或页面本来就空 |
| `选择器未匹配到元素` | 选择器写错，或用 `--wait-for` 等它出现 |
| `页面 JS 抛异常` | 页面自身报错，先修页面 |
| 超时 | 加 `--timeout 60`；或目标站点很慢 |
| 图太大 | 降 `--scale 1`，或截后用 PIL 缩 |

## 已知边界

- **不做屏幕捕获**。无头环境（SSH / 容器 / CI）物理上不存在屏幕。
- **不执行点击、滚动、填表**。要交互式自动化请直接用 playwright。
- **字体依赖系统**。容器里缺中文字体会出方框，装 `fontconfig` + 中文字体。
- **不做 PDF 直接输出**。先出 PNG 再转（`sips`），避免引额外依赖。
- 整页高度上限 20000px。
