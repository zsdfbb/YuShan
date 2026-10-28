# `edit` 工具 — As-built

> 性质：**实际建成记录**。唯一真相是 `adapters/tools-basic/src/edit.rs`（下称 `edit.rs`，
> 行号均指该文件）。与 `read`/`write`/`bash`/`grep`/`find`/`ls` 同属 `ys-tools-basic`。
> 日期：2026-10-29

## 一句话

`edit` 在**同一份原文**上做一次或多次定点替换（`path` + `edits[]`）。匹配分两个空间：先按原始
字节**精确**查找；精确 miss 才进**归一化视图**（NFKC + 引号/破折号/特殊空格折叠 + 逐行去行尾
空白）。命中一律映射回**原文区间**，替换只在原文空间里做 —— 命中区间之外的字节（零宽字符、弯
引号、行尾空白、BOM）逐字节保留。任何一层判定「没有产生改动」都**报错且不写文件**。

## 职责与边界

- **做**：读文件 → 匹配 → 单次写入 → 报结果与本批次须告知的事实。
- **不做安全 / 权限 / 沙箱**：路径直接 `workspace.join(&path)`（`edit.rs:1077`），不做 workspace
  逃逸检查。唯一与路径相关的行为是**提示**：workspace 内的绝对路径会在结果里追加一条「同一位置的
  相对路径写法」（`edit.rs:1074` 判定，`edit.rs:1132` 的 `attach_notice` 附上）。
- **不做并发控制**：`EditTool` 不实现 `is_concurrency_safe`，取 `Tool::is_concurrency_safe` 的默认
  `false`（`crates/ys-tool/src/lib.rs:53`）。循环层把它当**串行屏障**：`split_segments` 只在连续一段
  并发安全工具上组 `Segment::Parallel`，非安全者自成 `Segment::Serial`
  （`crates/ys-loop/src/basic.rs:316`、`350`–`362`）—— 即 `edit` 永不与任何工具并发，也不会把两侧的
  只读段粘在一起。工具表这一侧有精确集合断言守着：并发安全者恰是 `read`/`grep`/`find`/`ls`
  （`apps/coding-agent/src/toolset.rs:517`，用例 `only_read_only_tools_are_concurrency_safe`）。
- **不做 diff**：`ToolResult` 只有 `content` / `is_error` 两个字段（`crates/ys-core/src/tool.rs:14`），
  结果是一行计数 + notice，不带 patch。

## 落点

```
adapters/tools-basic/src/edit.rs            全部实现（归一化、视图、匹配、应用、解析、测试）
crates/ys-tool/src/notice.rs                append_notices —— notice 拼接的唯一实现
crates/ys-tool/src/lib.rs                   Tool::is_concurrency_safe 默认 false
apps/coding-agent/src/toolset.rs            并发准入表（edit 必须在「不安全」一侧）
```

## 参数契约

工具 spec 见 `edit.rs:1032`：`required` 只有 `path`（`edit.rs:1067`）。`edits` 用 `oneOf` 声明「数组
或单个对象」两种形态，`oldText` 与 `newText` 都必填（`edit.rs:1051`），顶层也保留
`oldText`/`newText` 两个 legacy 字段（`edit.rs:1064`）。

`parse_edits`（`edit.rs:969`）接受四种形态：

| 入参形态 | 行为 | 行号 |
|---|---|---|
| `edits: [{oldText,newText}, …]` | 标准数组，按下标逐条解析 | `edit.rs:973` |
| `edits: {oldText,newText}` | 单个对象 = 一条 edit | `edit.rs:978` |
| `edits: "[{…}]"` / `"{…}"` | **字符串**经 `serde_json::from_str`；数组即数组，单对象包成一条 | `edit.rs:979` |
| 顶层 `oldText` + `newText` | legacy，**追加为最后一条**（可与 `edits` 共存） | `edit.rs:1008` |

拒绝路径：`edits: null` / 缺字段 ⇒ 落到「至少一条」报错（`edit.rs:1000`、`1017`）；数字等其它标量
⇒ `'edits' must be an array …`（`edit.rs:1001`）；字符串不是合法 JSON ⇒ 明确报 `not valid JSON`
（`edit.rs:980`）；数组元素不是对象（如 `["b"]`）⇒ 报 `missing 'oldText'`；字符串形态解析出标量
⇒ `'edits' JSON must be an array …`（`edit.rs:993`）。

**缺 `newText` 一律报错，删除必须显式 `""`**。数组形态（`edit.rs:954`）、字符串形态、legacy 顶层
（`edit.rs:1010`）共用同一个 `missing_new_text_error`（`edit.rs:938`），三者行为不会漂移。理由写在
`edit.rs:934`：模型漏字段而被默认当成空串，就会**静默删掉**文件里的一段内容。

## 匹配管线

顺序由 `apply_edits_to_normalized_content`（`edit.rs:782`）钉死：**校验全部 `oldText` 非空 →
逐条在自己的空间里定位并按同一空间计数 → 排序查重叠 → 统一在原文空间替换**。任何一步失败都
不写文件。

### 1. 精确优先，且不触发归一化

每条 edit 先 `content.find(old)`（`edit.rs:815`）。命中后只做一件事：用
`count_occurrences(content, old)`（`edit.rs:816` → `199`）在**精确空间**计数，`> 1` 即歧义报错
（`edit.rs:817`）。精确唯一 ⇒ 直接成 `MatchedEdit { fuzzy: false }`（`edit.rs:820`），**完全不构造
归一化视图**。

这条「精确命中按精确口径计数」是硬要求：若唯一性一律在归一化空间上算，同批里另一条要归一化的
edit 会把「原文恰好只出现一次的 `"p  \n"`」算成 2 次（原文件的 `"p  \n"` 与 `"p\n"` 归一化后同形），
模型加多少上下文都无解。护栏用例 `exact_uniqueness_survives_a_fuzzy_sibling_edit`
（`edit.rs:1727`）与 `exact_occurrence_wins_over_trailing_whitespace_sibling`（`edit.rs:1909`）；
变异「唯一性一律走归一化空间」实测让 5 条用例变红（另 3 条是视图构造与空白口径的连带影响）。

同理，**纯空白 `oldText` 的拒绝放在精确 miss 之后**（`edit.rs:832`）：`"缩进从 tab 改成空格"` 是精确
唯一、完全可判定的请求，一律拒绝会让它无法表达（`edit.rs:780`）。用例 `unique_blank_old_text_is_applied`
（`edit.rs:1640`，四族：tab / NBSP / 全角空格 / 四空格）与 `blank_old_text_that_never_matches_is_reported_as_blank`
（`edit.rs:1695`）。

### 2. 归一化空间

归一化 = **NFKC + 单字符折叠 + 逐行去行尾空白**，三件事都在同一条实现上：

- `map_char`（`edit.rs:54`）列出字符集：单引号 `U+2018..201B` → `'`（`edit.rs:57`）；双引号
  `U+201C..201F` → `"`（`edit.rs:59`）；连字符/破折号/减号 `U+2010..2015` 与 `U+2212` → `-`（`edit.rs:61`–`62`）；
  特殊空格 `U+00A0`、`U+2002..200A`、`U+202F`、`U+205F`、`U+3000` → 空格（`edit.rs:64`）。
  ASCII 不在任何映射表里，因此恒等。
- NFKC 与簇切分在 `collect_units`（`edit.rs:121`）：一个单元 = 一个归一化字符 + 它在原文里的区间。
  **同一 NFKC 簇展开出的多个单元共享同一段原文区间**（`edit.rs:112`、`127`）。
- `joins_cluster`（`edit.rs:73`）判定 `next` 是否并入当前簇：组合字符（`canonical_combining_class != 0`）
  必然并入；否则靠 `compose` 试合（谚文 L+V+T、字母+重音）。ASCII 不可能是任何归一化等价物的
  后半部分，直接返回 `false`。
- **逐行只去行尾空白，不做整体 `trim`**（`edit.rs:151` 的 `while out.last()…is_whitespace()`）：行首
  缩进参与匹配语义。变异改成整体 `trim` ⇒ `fuzzy_match_keeps_indent_of_untouched_lines`
  （`edit.rs:1509`）与 `unit_normalization_matches_whole_string_normalization`（`edit.rs:2684`）变红；
  反向护栏是 `leading_indent_in_old_text_is_not_stripped`（`edit.rs:1961`，`oldText` 多带缩进必须
  miss）。
- 去空白在**归一化字符**上判定，所以 NBSP 先变普通空格再被去掉（`edit.rs:150` 注释）。零宽字符
  （`U+200B`/`200C`/`200D`/`FEFF`）**不属于**空白、不被删除
  （`fuzzy_normalize_keeps_zero_width_characters`，`edit.rs:2656`），否则 emoji 的 ZWJ 序列会被拆散。

`normalize_for_fuzzy_match`（`edit.rs:179`）只用于**短串** `oldText`；文件内容走 `NormalizedView`
（`edit.rs:335`），后者对纯 ASCII 行有整行快路径（`edit.rs:402`）。两者语义必须一致，护栏是
`unit_normalization_matches_whole_string_normalization`（`edit.rs:2684`）—— 它用一份**独立的整串
NFKC 参照实现**（`normalize_reference`，`edit.rs:2669`）逐样例比对视图内容。

### 3. atomic 簇：兼容字符不切半用

NFKC 把 `ﬁ` 展开成 `fi`、`℃` → `°C`、`½` → `1⁄2`、`㎏` → `kg`。这类簇的原文区间**无法只覆盖其中
一个展开字符**（两个展开字符共享同一段原文），因此该簇被标成 `atomic`（`edit.rs:114`，标记在
`edit.rs:129`–`133` 设置）。

`find`（`edit.rs:471`）在拿到命中区间后检查两端所在运行：若该运行 `atomic` 且命中端点落在簇内部
（`cluster_start < norm_start || cluster_end > norm_end`，`edit.rs:494`）⇒ 返回
`FuzzyFind::PartialCluster`（`edit.rs:316` 的语义注释），由调用方转成 `partial_cluster_error`
（`edit.rs:712`）—— 错误里给出簇的 `U+XXXX` 码点并提示 `include the whole character in oldText`。
**这是拒绝而不是降级**：整簇替换会吞掉同簇其余字节（破坏「区间之外逐字节保留」），半簇切片会把
字符切碎（`edit.rs:313`、`708`）。把整个字符写进 `oldText` 则可以正常编辑
（`whole_compatibility_cluster_can_be_edited`，`edit.rs:2255`）。变异「不给展开簇打 atomic 标记」
⇒ `atomic_cluster_is_only_matched_whole`（`edit.rs:2833`）与 `partial_compatibility_cluster_is_rejected`
（`edit.rs:2221`）变红。

### 4. 归一化空间的唯一性

计数与命中**必须同一空间**：`view.count(old_needle)`（`edit.rs:845` → `456`）与
`view.find(old_needle)`（`edit.rs:849` → `471`）都跑在 `view.normalized` 上，因此「报的次数」与
「能不能命中」不会分叉。变异让计数退化成常量 1 ⇒ `fuzzy_match_multiple_locations_error`
（`edit.rs:1442`）变红。

## 应用管线

1. **所有 `edits[]` 对同一份 base 匹配，非增量**。`matched` 全部收集完之后才
   `apply_matched(content, &matched, path)`（`edit.rs:886` → `746`）；替换文本里出现的内容不会被同批
   其它 edit 再次匹配（`all_edits_match_against_original_content`，`edit.rs:1881`）。这条也写进了模型
   可见的 schema 描述（`edit.rs:1041`）。
2. **按 `matchIndex` 升序排序**（`edit.rs:871`），随后 `apply_matched` 用 `cursor` 单向推进
   （`edit.rs:748`–`770`）—— 等价于「倒序应用」的游标写法，避免反复搬移。
3. **重叠即报错**：排序后只需与「前一个」比较（`edit.rs:874`），
   `previous.match_index + previous.match_len > current.match_index` ⇒ 报
   `edits[{a}] and edits[{b}] overlap in {path}`，带上**两个** edit 序号（`edit.rs:878`）。用例
   `overlapping_edits_are_rejected`（`edit.rs:1551`）同时断言两个序号都出现。
4. **端点校验在替换时再做一次**：`is_char_boundary(start)` / `is_char_boundary(end)` /
   `end <= original.len()` 任一不满足 ⇒ `invalid_range_error`（`edit.rs:735`、`754`），不 panic
   （`edit.rs:745` 注释）。

## 区间外字节保留

模糊命中时，替换在**原文空间**发生，`result` 只由三段拼成：`original[cursor..start]` +
`new_text` + 之后 `original[cursor..]`（`edit.rs:766`–`770`）。因此命中区间之外的弯引号、NBSP、
ZWSP/ZWJ/BOM 逐字节保留（`bytes_outside_the_match_are_preserved`，`edit.rs:1831`；同
`emoji_zwj_sequence_survives_a_sibling_fuzzy_edit`，`edit.rs:2525`）。

端点落在字符边界上由 `Run` 表（`edit.rs:301`）保证：

- `Run` 记录一段「归一化区间 ↔ 原文区间」。`direct == true` 表示这段原文与归一化**逐字节相同**，
  偏移可内插（`orig_start + (pos - norm_start)`，`edit.rs:573`）；`direct == false` 时这段只含一个
  单元，映射一律取整段边界（`edit.rs:576`、`589`–`593`），端点绝不落进字符内部。
- 相邻直通单元合并成长运行（`register_run`，`edit.rs:350`、`362`–`371`），所以运行表规模与**行数**
  同阶，不与字节数同阶（`edit.rs:300`、`386`；运行表规模的断言见 `edit.rs:2727`–`2738`）。
- 右端点用 `orig_end_at`（`edit.rs:583`）—— 取「`pos` 之前那个单元」的终点，因此 `pos` 恰为单元
  终点时不会连带吞掉其后被去掉的行尾空白（`mapped_range_covers_exactly_the_matched_bytes`，
  `edit.rs:2785`）。
- 端点覆盖整簇时才允许替换（见上节 atomic 规则）；`PartialCluster` 是唯一的例外通道，且它是错误。

## 无改动的三层判定

| 层 | 判据 | 行号 | 报错文案 |
|---|---|---|---|
| 请求层 | `oldText` 与 `newText` **字节**相等 | `edit.rs:839` | `No changes made … newText leaves the matched text identical` |
| 结果层 | 拼回的内容与归一化 base 相同 | `edit.rs:890` | `No changes made …`（多 edit 版文案另写） |
| 落盘层 | 最终字节（含 BOM 与换行还原）与磁盘原文相同 | `edit.rs:1095` | 同上 |

任一命中都报错且**不写文件**。判据刻意只落在**原文/请求**层面，不在归一化空间里：`"foo\nbar"` →
`"foo \nbar"` 是真实的空白改写（原文行尾两个空格），必须成功
（`normalized_equality_is_not_a_no_op`，`edit.rs:1608`）。反向护栏是 `normalizing_no_op_is_rejected`
（`edit.rs:1589`）：原文是 `foo<NBSP>bar` + 两个行尾空格，模型两边都写 `foo bar` —— 落到原文上只会
抹平 NBSP 与行尾空白，属于纯 no-op。

实测变异记录：删「请求层」判定 ⇒ `normalizing_no_op_is_rejected` 变红；把请求层与结果层同时删掉，
或把结果层与落盘层同时删掉 ⇒ `identical_replacement_is_rejected`（`edit.rs:1570`）变红。
**单删结果层或单删落盘层未见用例变红** —— 剩下那层把它兜住了。删除任一层都会放宽
「不静默改写」这条契约，所以三层都保留。

## 换行与 BOM

读入顺序（`edit.rs:1082`–`1087`）：`split_bom` → `detect_line_ending` → `normalize_to_lf` →
处理 → `restore_line_endings` → 按需装回 BOM。

- **BOM**：`split_bom`（`edit.rs:279`）剥离开头的 `U+FEFF` 并报告，写回时按原样恢复
  （`edit.rs:1088`）。BOM 不参与匹配 —— 模型不会在 `oldText` 里带不可见字符。护栏 `bom_is_preserved`
  （`edit.rs:2473`）。
- **风格判定**：`detect_line_ending`（`edit.rs:245`）取**第一个**换行符。`\r\n` 与 `\n` 按首字节位置
  比较（`edit.rs:252`–`257`），不是按数量投票。
- **孤立的 `\r` 只在全文没有任何 `\n` 时才是候选**（`edit.rs:258`）。否则 CSV 字段内嵌的回车、
  Windows 工具输出、源码字符串字面量里的 `\r` 都会把整文件判成 CR，落盘时把全文换行改写
  （`edit.rs:242`）。两条护栏：`line_ending_comes_from_first_newline`（`edit.rs:2814`，纯函数层）与
  `isolated_cr_does_not_change_the_line_ending`（`edit.rs:2160`，真实文件层 —— CRLF 骨架必须留 ≥3 个
  `\r\n`）。变异「孤立 `\r` 无条件当选」⇒ 这两条都变红。
- **绝不产生 `\r\r\n`**：`restore_line_endings`（`edit.rs:270`）的入参必须是 `normalize_to_lf` 的产物
  （不含 `\r`），所以 `\n` → `\r\n` 不会与既有 `\r` 叠加。`normalize_to_lf`（`edit.rs:188`）先折叠
  `\r\n` 再折叠孤立 `\r`，无 `\r` 时返回 `Cow::Borrowed` 不分配。护栏
  `crlf_file_edit_produces_no_double_cr`（`edit.rs:1459`）、`lf_file_edit_stays_lf`（`edit.rs:1482`）；
  删掉 `\r` 折叠 ⇒ 4 条用例变红（含 `mixed_line_endings_follow_the_first_newline`、`untouched_lines_keep_original_bytes`）。
- `\r`-only 文件是**独立一档** `LineEnding::Cr`（`edit.rs:236`），编辑后仍是 `\r`
  （`lone_cr_file_keeps_cr_endings`，`edit.rs:2188`）。

## 结果通知

成功结果主体是一行 `Successfully replaced N block(s) in {path}.`（`edit.rs:1128`），随后
`ys_tool::append_notices`（`edit.rs:1131`）把若干条 notice 拼成 `\n\n[…]`（`. ` 分隔，
`crates/ys-tool/src/notice.rs:14`）。notice 分两类：

1. **归一化命中**：每条 `fuzzy == true` 的 edit 各一条
   `{label}: matched {:?} by normalizing whitespace and quotes (not an exact match)`
   （`edit.rs:1115`），`{label}` 在单条时是 `edit`、多条时是 `edit: edits[i]`（`edit.rs:1109`）。
   取样用 `snippet(…, 120)` 按字符边界截断（`edit.rs:912`）。notice 按 `edit_index` 排序
   （`edit.rs:903`），覆盖**所有**走归一化命中的 edit（`every_fuzzy_hit_gets_a_notice`，
   `edit.rs:1764`；变异「只报第一条」⇒ 变红）；同批里精确命中的那条不带 notice
   （`exact_hit_in_a_mixed_batch_gets_no_notice`，`edit.rs:1796`）。
2. **被并入命中区间的行尾空白**：每个被吞的空白区间**各报一条**
   `{label}: also removed trailing whitespace {…} at the end of a matched line`（`edit.rs:1121`）。
   合并成一条会把两行的空白拼成一串、误报成同一个位置（`edit.rs:1118`）。数据源是视图的
   `trimmed_whitespace`（`edit.rs:342`）：这些字节不在归一化内容里，因此永不属于任何
   `oldText` 的重现，但当 `oldText` 以 `\n` 收尾时它们**落在**命中的原文区间之内
   （`removed_trailing_whitespace`，`edit.rs:515`；多行版本见 `multi_line_hit_reports_every_trailing_whitespace_run`，
   `edit.rs:2869`）。

行尾空白必须揭示的理由是 Markdown：行尾两空格即 `<br>`（`edit.rs:21`、`638`）。命中区间没碰到它们
时**不得**建这条 notice，否则提示本身成噪音（`untouched_trailing_whitespace_is_not_reported`，
`edit.rs:2317`）。

## 性能契约

- **同一份内容的归一化视图最多构造一次**：`view: Option<NormalizedView>` 在循环外声明
  （`edit.rs:809`），循环内 `get_or_insert_with`（`edit.rs:844`）。
- **纯精确批次零视图构造**：精确分支在 `edit.rs:815`–`829` 就 `continue`，从不触碰 `view`。
- 两条护栏用 `#[cfg(test)]` 的线程局部计数器测（`edit.rs:217`–`230`）：
  `exact_only_batch_builds_no_view`（`edit.rs:2381`，断言构造数 `== 0`）、
  `fuzzy_batch_builds_the_view_once`（`edit.rs:2411`，断言 `== 1`）；`exact_path_leaves_surrounding_bytes_untouched`
  （`edit.rs:2603`）再钉一次精确路径的 `== 0`。变异 `always_build_view` ⇒ 两条「精确路径 0 构造」
  用例都变红；`rebuild_view_per_edit` ⇒ 只管「只建一次」那条变红（均已实测）。

**实测数字**（一次性临时基准，**未入库**；release 与 debug 分开跑，单线程内 `Instant` 计时，
含文件读写；取多次运行中的最优值）：

| 场景（约 1.2 MiB） | edits | exact | fuzzy | 视图构造数 |
|---|---|---|---|---|
| 20 000 行纯 ASCII（1 240 000 B） | 1 / 4 / 16 | 2.7 / 5.1 / 10.3 ms | — | 0 |
| 同上，但 `oldText` 侧需折叠（`–`→`-`） | 1 / 4 / 16 | — | 6.5 / 8.1 / 21.1 ms | 1 |
| 20 000 行每行含一个 NBSP（1 260 000 B） | 1 / 4 / 16 | — | 62 / 65 / 75 ms | 1 |

`NormalizedView::new` 单独计时：纯 ASCII 文件 1.8 ms（运行表 1 个 `Run`），每行一个 NBSP 的文件
**61.6 ms**（运行表 40 001 个 `Run`）—— 可见模糊路径的成本几乎全在**视图构造**，与 `edits` 条数
近乎无关（16 条只比 1 条多 13 ms）。debug 构建同场景 fuzzy 约 855–928 ms，是 release 的十倍量级。

上述数字只能当作「模糊路径的成本全在视图构造」的量级证据：绝对值随机器与取法浮动（独立复测
ASCII 纯精确路径得到 0.36 / 1.30 / 5.13 ms，与本表差 2–7 倍），**不是 CI 门槛**。可回归的只有两条
构造次数护栏。

## 测试矩阵

`edit::tests` 共 **70** 条（`#[test]` + `#[tokio::test]`）。「变异」列标 ✅ 的已实测注入并变红，
标 — 的未单独注入（多数由同组其它用例共同覆盖同一实现点）。

### 职责与装配

| 用例 | 覆盖点 | 变异 |
|---|---|---|
| `test_test_dir_is_unique` | 临时目录名含原子序号 + pid，并行测试不撞名 | — |
| `test_edit_exact_match` | 单条精确替换，结果含 `1 block` | — |
| `absolute_workspace_path_adds_relative_hint` | workspace 内绝对路径追加相对路径事实 | — |

### 参数契约与形态

| 用例 | 覆盖点 | 变异 |
|---|---|---|
| `test_edit_batch` | `edits[]` 数组两条 | — |
| `test_edit_legacy_format` | 顶层 `oldText`/`newText` | — |
| `edits_as_json_string_are_parsed` | 字符串形态 → 数组 | — |
| `edits_as_single_object_are_parsed` | `edits` 为单对象 | — |
| `edits_as_json_string_with_single_object_is_parsed` | 字符串里是单对象 | — |
| `unparsable_edits_string_is_rejected` | 字符串非合法 JSON ⇒ `not valid JSON` | — |
| `empty_edits_array_is_rejected` | `edits: []` ⇒ 「至少一条」 | — |
| `null_edits_is_rejected` | `edits: null` 同上报错 | — |
| `numeric_edits_is_rejected` | 标量 ⇒ `must be an array` | — |
| `non_object_edit_element_is_rejected` | 元素是字符串 ⇒ `missing 'oldText'` | — |
| `missing_new_text_is_rejected` | 数组元素缺 `newText` 报错 | ✅ 默认成 `""` ⇒ 变红 |
| `legacy_missing_new_text_is_rejected` | legacy 顶层缺 `newText` 报错 | — |
| `explicit_empty_new_text_deletes` | 显式 `""` 是删除 | — |

### 匹配：精确优先 / 唯一性口径

| 用例 | 覆盖点 | 变异 |
|---|---|---|
| `test_edit_not_found` | 找不到 ⇒ 错误 | — |
| `test_edit_multiple_matches_error` | 精确多处 ⇒ 歧义 | ✅ 计数恒 1 ⇒ 变红 |
| `exact_uniqueness_survives_a_fuzzy_sibling_edit` | 精确唯一不被同批模糊 edit 影响 | ✅ 唯一性一律走归一化 ⇒ 变红 |
| `exact_occurrence_wins_over_trailing_whitespace_sibling` | 两处仅行尾空白不同时改精确那处 | ✅ 同上 ⇒ 变红 |
| `fuzzy_match_multiple_locations_error` | 归一化空间多处 ⇒ 歧义 | ✅ 计数恒 1 ⇒ 变红 |
| `whitespace_only_old_text_is_rejected` | 纯空白精确多处 ⇒ 歧义（不是「只有空白」） | ✅ 空白拒绝前置 ⇒ 变红 |
| `blank_old_text_that_never_matches_is_reported_as_blank` | 精确 miss 且归一化为空 ⇒ 「只有空白」 | ✅ 删该分支 ⇒ 变红 |
| `zero_width_only_old_text_is_counted` | 零宽字符参与计数（是「3 times」） | ✅ 计数恒 1 ⇒ 变红 |
| `unique_blank_old_text_is_applied` | 精确唯一的空白可编辑（tab/NBSP/全角/四空格） | ✅ 空白拒绝前置 ⇒ 变红 |
| `empty_old_text_is_rejected` | 空 `oldText` 显式拒绝 | ✅ 删检查 ⇒ 变红 |
| `empty_file_reports_not_found` | 空文件报 not found 而非 panic | — |

### 匹配：归一化与原文保留

| 用例 | 覆盖点 | 变异 |
|---|---|---|
| `fuzzy_match_uses_actual_matched_length` | 替换长度按**实际命中**算（不是 `oldText.len()`） | — |
| `fuzzy_match_multibyte_old_text_uses_normalized_length` | 弯引号 3 B → 1 B 时长度按映射计 | — |
| `fuzzy_match_range_keeps_char_boundaries` | NBSP 命中端点仍落字符边界 | — |
| `fuzzy_match_preserves_original_boundary_whitespace` | `oldText` 多带的尾部空白不属于命中区间 | — |
| `fuzzy_match_keeps_indent_of_untouched_lines` | 逐行只去行尾 ⇒ 行首缩进保留 | ✅ 整体 `trim` ⇒ 变红 |
| `leading_indent_in_old_text_is_not_stripped` | `oldText` 带行首空白不算命中 | — |
| `bytes_outside_the_match_are_preserved` | ZWSP/弯引号/NBSP/ZWJ/BOM 逐字节保留 | — |
| `trailing_whitespace_after_the_match_is_preserved` | 命中停在 `bar`，行尾两空格不受影响 | — |
| `untouched_lines_keep_original_bytes` | 未触及行与原文字节相同（含 CRLF、多字节） | ✅ 删 `\r` 折叠 ⇒ 变红 |
| `emoji_zwj_sequence_survives_a_sibling_fuzzy_edit` | 同行编辑不拆 ZWJ 序列 | — |
| `combining_and_wide_chars_keep_boundaries_on_fuzzy_edit` | decomposed `é` 命中 composed 写法 | — |
| `zero_width_needle_matches_exactly` | 含 ZWSP 的 `oldText` 精确命中且无 notice | — |
| `fuzzy_normalize_maps_quotes_dashes_and_spaces` | `map_char` 全表抽查 + 只去行尾 | — |
| `fuzzy_normalize_keeps_zero_width_characters` | 零宽字符不进删除集 | — |
| `normalization_is_idempotent` | 归一化幂等 | ✅ 保留行尾空白 ⇒ 变红 |
| `unit_normalization_matches_whole_string_normalization` | 逐簇 ≡ 整串 NFKC；端点落边界；运行表规模 | ✅ 整体 `trim` ⇒ 变红 |
| `mapped_range_covers_exactly_the_matched_bytes` | 命中区间精确覆盖原文片段 | — |
| `count_occurrences_is_non_overlapping` | 非重叠计数、空 needle 计 0 | ✅ 恒 1 ⇒ 变红 |

### atomic 簇

| 用例 | 覆盖点 | 变异 |
|---|---|---|
| `partial_compatibility_cluster_is_rejected` | `ﬁ`/`℃`/`½` 半簇 ⇒ 报错带码点 + 自修提示 | ✅ 不打 atomic 标记 ⇒ 变红 |
| `whole_compatibility_cluster_can_be_edited` | 整簇写进 `oldText` 可正常编辑 | — |
| `atomic_cluster_is_only_matched_whole` | 纯函数层：半簇 `PartialCluster`、整簇 `Hit` | ✅ 同上 ⇒ 变红 |

### 应用管线

| 用例 | 覆盖点 | 变异 |
|---|---|---|
| `all_edits_match_against_original_content` | 所有 edit 对同一 base 匹配，非增量 | — |
| `overlapping_edits_are_rejected` | 重叠报错且含两个 edit 序号 | ✅ 删重叠检查 ⇒ 变红 |

### 无改动判定

| 用例 | 覆盖点 | 变异 |
|---|---|---|
| `identical_replacement_is_rejected` | `oldText == newText` ⇒ `No changes made` | ✅ 结果层与落盘层同时删 ⇒ 变红（单删任一层不变红） |
| `normalizing_no_op_is_rejected` | 归一化后同形的纯 no-op 拒绝 | ✅ 删请求层 ⇒ 变红 |
| `normalized_equality_is_not_a_no_op` | 归一化同形但原文上真实改写 ⇒ 必须成功 | — |

### 换行与 BOM

| 用例 | 覆盖点 | 变异 |
|---|---|---|
| `test_edit_preserves_crlf` | CRLF 文件仍含 `\r\n` | — |
| `crlf_file_edit_produces_no_double_cr` | 不产生 `\r\r`，逐字节等于期望 | ✅ 删 `\r` 折叠 ⇒ 变红 |
| `lf_file_edit_stays_lf` | LF 文件不混入 `\r` | — |
| `mixed_line_endings_follow_the_first_newline` | 混合文件统一成首个换行符风格（两向） | ✅ 同上 ⇒ 变红 |
| `isolated_cr_does_not_change_the_line_ending` | CRLF 文件里的裸 `\r` 不改写整文件风格 | ✅ 裸 `\r` 无条件候选 ⇒ 变红；✅ 删折叠 ⇒ 变红 |
| `lone_cr_file_keeps_cr_endings` | `\r`-only 文件编辑后仍全 `\r`、无 `\n` | — |
| `line_ending_comes_from_first_newline` | 纯函数层：首个换行符决定风格 + 裸 `\r` 规则 | ✅ 同上 ⇒ 变红 |
| `bom_is_preserved` | BOM 原样保留且不参与匹配 | ✅ 不装回 ⇒ 变红 |

### 结果通知

| 用例 | 覆盖点 | 变异 |
|---|---|---|
| `every_fuzzy_hit_gets_a_notice` | 每条模糊命中各一条 notice | ✅ 只留第一条 ⇒ 变红 |
| `exact_hit_in_a_mixed_batch_gets_no_notice` | 精确命中不带 notice | — |
| `swallowed_trailing_whitespace_is_reported` | 被吞的行尾空白点名（`"  "`） | ✅ 不报 ⇒ 变红 |
| `untouched_trailing_whitespace_is_not_reported` | 没碰到就不报（避免噪音） | — |
| `exact_match_with_trailing_whitespace_keeps_it_in_range` | 精确命中自带行尾空白 ⇒ 不报 | — |
| `multi_line_hit_reports_every_trailing_whitespace_run` | 跨行命中逐处报，不拼成一串 | ✅ 保留行尾空白 ⇒ 变红 |

### 性能护栏

| 用例 | 覆盖点 | 变异 |
|---|---|---|
| `exact_only_batch_builds_no_view` | 纯精确批次构造数 0 | ✅ `always_build_view` ⇒ 变红 |
| `exact_path_leaves_surrounding_bytes_untouched` | 精确路径 0 构造 + 周边字节不动 | ✅ 同上 ⇒ 变红 |
| `fuzzy_batch_builds_the_view_once` | 一批模糊 edit 只建一次视图 | ✅ `rebuild_view_per_edit` ⇒ 变红 |

## 已知取舍 / 未做的事

1. **混合换行文件会被统一成首个换行符的风格**。`normalize_to_lf` 抹平全部差异，写回只按一个
   `ending` 还原（`edit.rs:1084`、`1087`）。这是「绝不产生 `\r\r\n`」的代价；
   `mixed_line_endings_follow_the_first_newline`（`edit.rs:2438`）把两个方向都钉成期望行为，不是缺口
   而是契约。
2. **CRLF 文件里的孤立 `\r` 会被归一到文件风格**。`edit.rs:2176` 明说：CSV 字段内嵌的回车属于内容，
   但读入时已被折叠成 `\n`，写回时变成 `\r\n` —— 文件里那个字段的值被改写了。要保住它必须在
   LF 空间里为每个 `\r` 保留身份，与「统一归一化」直接冲突，当前不做。
3. **无文件写队列 / 无锁**。同一批里多个 `edit` 各自成 `Segment::Serial`、按 `pending` 顺序执行
   （`crates/ys-loop/src/basic.rs:350`–`362`），所以批内不会互相覆盖；但跨批、跨回合、跨进程没有任何
   协调：读-改-写之间没有比对 mtime 或内容指纹，后写者覆盖先写者。工具层不做乐观并发控制。
4. **无 diff / patch 字段**。结果只有 `Successfully replaced N block(s) …` 与 notice
   （`edit.rs:1128`）；`ToolResult` 没有结构化字段可放 hunk（`crates/ys-core/src/tool.rs:14`）。模型要靠
   自己记得改了什么 —— 需要精确 diff 时应改用 `read` 复核。
5. **无 TUI 预览 / 审批**。`Tool::call` 直接读文件、写盘（`edit.rs:1078`、`1099`），`ToolContext` 参数
   未被使用（`_ctx`，`edit.rs:1072`），没有「先给用户看再确认」的环节。审批属上层项目职责。
6. **`snippet` 只按字节截 120**（`edit.rs:912`），notice 里的取样可能停在行中间；它只用于让模型
   认出「这是哪一处」，不承担精确回显。
7. **无改动判定的两处覆盖空隙**：结果层（`edit.rs:890`）与落盘层（`edit.rs:1095`）各自单独删除时，
   现有 70 条用例都不变红（见「无改动的三层判定」）。两层都保留 —— 它们分别对应不同输入类别。
8. **`atomic` 只由「单个 NFKC 簇展开出多个字符」触发**（`edit.rs:126`–`133`）。因此跨两个独立字符的
   `oldText`（如 `k` + `g`）不算簇，即使它与 `㎏` 的展开结果同形：精确路径优先命中原文字面，精确
   miss 后按常规唯一性口径判定。簇判定只服务「映射不切碎字符」，不做跨字符的语义消歧。
