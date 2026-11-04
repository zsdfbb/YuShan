#!/usr/bin/env python3
"""图像输入（vision input）链路的**变异测试**：注入 bug ⇒ 期望测试变红 ⇒ 还原。

为什么需要它：图像通路横跨 `ys-core` / `ys-loop` / 适配器 / TUI 四层，而它的失败形态
**几乎全是静默的** —— 图被丢掉不报错、被计成 0 token 不报错、消息整条消失只表现为
`"messages":[]`、XML/JSON 形态不对只是 provider 悄悄不认。这些 bug 都**不会**让一个
「全绿」的测试套件失败，除非有人专门去注入它们。AGENTS.md 约定「关键逻辑要做变异测试」，
这个脚本就是图像链路的尺子（design.md §11 步 12）。

设计要点（与 `cancellation-mutation.py` 同形）：
  - **一次跑完全部 mutant**，不逐个手动驱动；跑完给一张汇总表。
  - 每个 mutant 跑**包级**测试（`cargo test -p <pkg>`）而非单个具名用例 ——
    覆盖更强，且能报出「哪些用例抓到了它」。
  - 注入用**精确文本替换**（不是行号/正则），`old` 不唯一或不存在就**报错退出**，
    绝不允许「注入没生效 ⇒ 测试绿 ⇒ 假通过」。
  - 每个 mutant 前**快照**、跑完**还原**；`finally` 兜底，异常也不留脏工作区。
    还原靠**进程内快照**，不用 `git checkout` ⇒ 允许带着未提交改动运行。

**为什么没有 `as_text()` 的穷尽 match 变异**（design.md §11 步 12 提到的 C6）：把
`ContentBlock::Image` 臂并进 `_ => None` 后**运行期行为不变**（两者都返回 `None`），
`cargo test -p ys-core` 全绿 —— 实测确认无任何用例能抓。它是「新变体必须表态」的
**类型层契约**，不能被运行期变异测试观测；写进来只会是一条永远绿的假 mutant。
同理，本脚本里没有 `compile_is_catch` 式的条目：图像链路的守护全在运行期 ——
`ContentBlock` 是 `#[non_exhaustive]`，所有消费点**本来就必须**有通配臂，
注入只会静默改变行为而不是编译失败。`Mutant.compile_is_catch` 字段保留（与模板同形），
当前无条目使用。

用法：
    python3 scripts/vision-mutation.py            # 跑全部
    python3 scripts/vision-mutation.py --list     # 只列清单
    python3 scripts/vision-mutation.py 1 4 7      # 只跑指定编号
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


@dataclass
class Mutant:
    """一处注入。

    `edits` 里的 `old`/`new` 必须是**精确**文本；`old` 在目标文件里不唯一或不存在时，
    注入直接失败（不算「测试通过」）。
    """

    name: str
    package: str
    desc: str
    edits: list[tuple[str, str, str]] = field(default_factory=list)
    #: 期望被抓住的用例名片段；为空表示「该 mutant 只要求整包不绿」。
    #:
    #: 命中它时状态为 `red`；变红了但**没命中**片段则为 `wrong-catcher` ——
    #: 那是「测试偶然红了」，不算这条防线生效。
    expect_red: str = ""
    #: 本变异的守护**就在类型层**：注入使它**编译失败**即为被抓住。
    compile_is_catch: bool = False
    #: 类型层守护的靶子：编译错误里必须出现它，否则说明注入错在别处。
    compile_must_mention: str = ""


# 全部待注入文件（跑完统一还原）。
TARGETS = [
    "adapters/model-openai-compatible/src/lib.rs",
    "adapters/model-openai-compatible/src/media.rs",
    "adapters/model-openai-compatible/src/request.rs",
    "adapters/model-openai-compatible/src/compat.rs",
    "apps/coding-agent/src/app_loop.rs",
    "crates/ys-loop/src/basic.rs",
    "crates/ys-loop/src/token.rs",
    "crates/ys-loop/src/turn_budget.rs",
    "crates/ys-tui-coding/src/app.rs",
    "crates/ys-tui-coding/src/transcript.rs",
]

ADAPTER_LIB = "adapters/model-openai-compatible/src/lib.rs"
ADAPTER_MEDIA = "adapters/model-openai-compatible/src/media.rs"
ADAPTER_REQUEST = "adapters/model-openai-compatible/src/request.rs"
ADAPTER_COMPAT = "adapters/model-openai-compatible/src/compat.rs"
LOOP_BASIC = "crates/ys-loop/src/basic.rs"
LOOP_TOKEN = "crates/ys-loop/src/token.rs"
LOOP_BUDGET = "crates/ys-loop/src/turn_budget.rs"
TUI_APP = "crates/ys-tui-coding/src/app.rs"
TUI_TRANSCRIPT = "crates/ys-tui-coding/src/transcript.rs"
APP_LOOP = "apps/coding-agent/src/app_loop.rs"


MUTANTS: list[Mutant] = [
    # ------------------------------------------------------------ K2 的两半
    Mutant(
        name="K2 上半：`Image` 臂改回 `_ => {}`（不产出 image_parts）",
        package="ys-model-openai-compat",
        desc="advance：图块落进通配臂 ⇒ 线上没有任何 image_url，也不给占位（静默丢图）",
        expect_red="message_count_is_conserved",
        edits=[
            (
                ADAPTER_LIB,
                """                ContentBlock::Image { path, detail } => {
                    // **判定顺序不得调换**（design.md §5 / C8.4）：""",
                """                ContentBlock::Image { path, detail } if false => {
                    // **判定顺序不得调换**（design.md §5 / C8.4）：""",
            )
        ],
    ),
    Mutant(
        name="K2 下半：推送条件去掉 `|| !image_parts.is_empty()`",
        package="ys-model-openai-compat",
        desc="只含 Image 的消息没有文本也没有工具调用 ⇒ 整条消失（`\"messages\":[]`）",
        expect_red="image_only_message_is_not_dropped",
        edits=[
            (
                ADAPTER_LIB,
                """        if !text_content.is_empty() || !tool_calls.is_empty() || !image_parts.is_empty() {""",
                """        if !text_content.is_empty() || !tool_calls.is_empty() {""",
            )
        ],
    ),
    # ------------------------------------------------------------ K3 估算器
    Mutant(
        name="K3 上半：`token.rs` 的 `Image` 分支删掉（回到 `_ => 0`）",
        package="ys-loop",
        desc="图片被算成 0 token ⇒ 上下文预算看不见图，压缩就永远不会因图触发",
        expect_red="test_estimate_tokens_counts_image_block",
        edits=[
            (
                LOOP_TOKEN,
                """            ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,\n""",
                "",
            )
        ],
    ),
    Mutant(
        name="K3 下半（反向）：`turn_budget.rs` 的 `_ => 0` 改成计入 IMAGE_TOKEN_ESTIMATE",
        package="ys-loop",
        desc="图片计入回合预算分子但那部分降不掉 ⇒ 每回合空转重试降级",
        expect_red="plan_elisions_empty_when_overage_is_caused_only_by_images",
        edits=[
            (
                LOOP_BUDGET,
                """            _ => 0,\n""",
                """            ContentBlock::Image { .. } => crate::token::IMAGE_TOKEN_ESTIMATE,\n            _ => 0,\n""",
            )
        ],
    ),
    # ------------------------------------------------------------ A 档静默丢图
    Mutant(
        name="A1：`basic.rs` 折叠时不追加 `Image` 块",
        package="ys-loop",
        desc="工具产出的图片附件被丢弃 ⇒ 模型永远看不见用户让它看的那张图",
        expect_red="collapse_appends_images_to_the_same_tool_result_message",
        edits=[
            (
                LOOP_BASIC,
                """                for attachment in media {
                    let MediaAttachment::Image { path, detail } = attachment;
                    tool_results_for_message.push(ContentBlock::Image { path, detail });
                }""",
                """                for attachment in media {
                    let MediaAttachment::Image { .. } = attachment;
                }""",
            )
        ],
    ),
    Mutant(
        name="A2：`basic.rs` 把图片另起一条消息（而非同一条）",
        package="ys-loop",
        desc="消息数与工具调用数不再一一对应 ⇒ 多数 provider 对此敏感",
        expect_red="collapse_appends_images_to_the_same_tool_result_message",
        edits=[
            (
                LOOP_BASIC,
                """                for attachment in media {
                    let MediaAttachment::Image { path, detail } = attachment;
                    tool_results_for_message.push(ContentBlock::Image { path, detail });
                }""",
                """                for attachment in media {
                    let MediaAttachment::Image { path, detail } = attachment;
                    ctx.session
                        .append(Message {
                            role: Role::User,
                            content: vec![ContentBlock::Image { path, detail }],
                        })
                        .await
                        .map_err(|_| LoopError::Event(ys_core::EventError::SendFailed))?;
                }""",
            )
        ],
    ),
    Mutant(
        name="A3：`app.rs` 的 `message_text` 的 `Image` 臂改回丢弃",
        package="ys-tui-coding",
        desc="用户带图消息在 transcript 里只剩文本（或空行）⇒ 界面看起来图没被接收",
        expect_red="u9_message_text_renders_image_marker_with_and_without_detail",
        edits=[
            (
                TUI_APP,
                """            ContentBlock::Image { path, detail } => parts.push(image_marker(path, *detail)),\n""",
                "",
            )
        ],
    ),
    Mutant(
        name="A4：`transcript.rs` 回放 assistant 侧 `Image` 臂改回 `None`",
        package="ys-tui-coding",
        desc="`--resume` 后 assistant 行里的图消失 ⇒ 回放与 live 画成不一样的历史",
        expect_red="u4_replay_lines_must_render_image_block",
        edits=[
            (
                TUI_TRANSCRIPT,
                """                        ContentBlock::Image { path, detail } => {
                            Some(crate::app::image_marker(path, *detail))
                        }
""",
                "",
            )
        ],
    ),
    Mutant(
        name="A5：`expand_skill_invocation` 改回只建一个 `Text` 块",
        package="ys-coding-agent",
        desc="`/demo` + 截图 ⇒ 展开后图片静默消失（无编译错误、无运行期告警）",
        expect_red="skill_expansion_keeps_image_block",
        edits=[
            (
                APP_LOOP,
                """            for block in &mut message.content {
                if let ContentBlock::Text { text } = block {
                    *text = expanded;
                    break;
                }
            }""",
                """            message.content = vec![ContentBlock::Text { text: expanded }];""",
            )
        ],
    ),
    # ------------------------------------------------------------ 适配器 media
    Mutant(
        name="M1：单图判据改回按原图判（`raw > limit`，两处闸门都改）",
        package="ys-model-openai-compat",
        desc="24 MiB 原图（编码后 32 MiB+22）会被放行 ⇒ 端点 400 或静默截断",
        # 两处都改才对得上「按原图判」这个 bug：只改后一处时，前面按
        # `metadata.len()` 做的预估仍会先拒 —— 那不是要注入的语义。
        expect_red="limit_is_judged_on_encoded_bytes_not_raw_bytes",
        edits=[
            (
                ADAPTER_MEDIA,
                """    if expected_wire_len > limit {""",
                """    let _ = expected_wire_len;
    if false {""",
            ),
            (
                ADAPTER_MEDIA,
                """    let prefix_len = "data:".len() + mime.len() + ";base64,".len();
    let mut url = String::with_capacity(prefix_len + base64_len(raw.len()));
    url.push_str("data:");
    url.push_str(mime);
    url.push_str(";base64,");
    STANDARD.encode_string(&raw, &mut url);
    drop(raw);

    // 精确判定以**实际编码后**的长度为准：文件在 `metadata` 与读盘之间长大时，
    // 上面的预估不准，这里是最终关口。
    if url.len() > limit {
        return Err(MediaError::SingleTooLarge {
            bytes: url.len(),
            limit,
        });
    }
""",
                """    if raw.len() > limit {
        return Err(MediaError::SingleTooLarge {
            bytes: wire_len(raw.len(), mime),
            limit,
        });
    }

    let prefix_len = "data:".len() + mime.len() + ";base64,".len();
    let mut url = String::with_capacity(prefix_len + base64_len(raw.len()));
    url.push_str("data:");
    url.push_str(mime);
    url.push_str(";base64,");
    STANDARD.encode_string(&raw, &mut url);
    drop(raw);
""",
            ),
        ],
    ),
    Mutant(
        name="M2：缓存键去掉 `mtime`",
        package="ys-model-openai-compat",
        desc="同长度改写原图 ⇒ 命中旧条目，送出去的是上一版字节",
        expect_red="mtime_is_part_of_the_cache_key",
        edits=[
            (
                ADAPTER_MEDIA,
                """            path: path.to_owned(),
            mtime,
            len: meta.len(),""",
                """            path: path.to_owned(),
            mtime: (0, 0),
            len: meta.len(),""",
            )
        ],
    ),
    Mutant(
        name="M3：缓存键去掉 `len`",
        package="ys-model-openai-compat",
        desc="长度变化的改写只靠 mtime 区分 ⇒ mtime 精度不足时命中旧内容",
        expect_red="len_is_part_of_the_cache_key",
        edits=[
            (
                ADAPTER_MEDIA,
                """            path: path.to_owned(),
            mtime,
            len: meta.len(),""",
                """            path: path.to_owned(),
            mtime,
            len: 0,""",
            )
        ],
    ),
    Mutant(
        name="M4：`Content` 去掉 `#[serde(untagged)]`",
        package="ys-model-openai-compat",
        desc="纯文本序列化成 `{\"Text\":\"hi\"}` 而非 `\"hi\"` ⇒ 所有 provider 的请求形态变了",
        expect_red="pure_text_content_serializes_as_a_json_string",
        edits=[(ADAPTER_REQUEST, "#[serde(untagged)]\n", "")],
    ),
    Mutant(
        name="M5：`ImageUrl.detail` 去掉 `skip_serializing_if`",
        package="ys-model-openai-compat",
        desc="`detail: None` 时写出 `\"detail\":null` ⇒ 与既有请求逐字节不同，端点可能 400",
        expect_red="image_part_omits_absent_detail",
        edits=[
            (
                ADAPTER_REQUEST,
                """    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,""",
                """    pub detail: Option<String>,""",
            )
        ],
    ),
    # ------------------------------------------------------------ 占位与门控
    Mutant(
        name="G1：门控 `supports_image` 恒 `true`（fail-open）",
        package="ys-model-openai-compat",
        desc="认不出视觉能力的模型也收到图 ⇒ 400 或静默丢内容（应为 fail-closed）",
        expect_red="image_gate_needs_both_layers",
        edits=[
            (
                ADAPTER_COMPAT,
                """        self.supports_image_input && is_vision_model(model)\n""",
                """        let _ = (self.supports_image_input, is_vision_model(model));
        true\n""",
            )
        ],
    ),
    Mutant(
        name="G2：「模型不支持」占位改成带路径（每图一条）",
        package="ys-model-openai-compat",
        desc="三张图 ⇒ 三句同样的话 ⇒ 纯烧 token，且模型对此无能为力",
        expect_red="unsupported_placeholder_appears_once_per_batch",
        edits=[
            (
                ADAPTER_LIB,
                """                        if !unsupported_notice_sent {
                            unsupported_notice_sent = true;
                            image_parts.push(ContentPart::Text {
                                text: UNSUPPORTED_IMAGE_PLACEHOLDER.to_string(),
                            });
                        }
                        continue;""",
                """                        unsupported_notice_sent = true;
                        image_parts.push(ContentPart::Text {
                            text: media_error_placeholder(
                                path,
                                &MediaError::UnsupportedFormat {
                                    path: path.clone(),
                                    mime: "image/png".to_string(),
                                },
                            ),
                        });
                        continue;""",
            )
        ],
    ),
    # ------------------------------------------------------------ C9（序列化缓存）
    Mutant(
        name="C9：删掉剥 `stream_options` 后的 `*body_cache = None;`",
        package="ys-model-openai-compat",
        desc="重发带上仍含该字段的陈旧字节 ⇒ 端点再回 400，回退形同失效",
        expect_red="stream_options_fallback_resends_a_body_without_the_field",
        edits=[(ADAPTER_LIB, """            *body_cache = None;\n""", "")],
    ),
    # ------------------------------------------------------------ :360 编码后闸门
    Mutant(
        name="E1：删掉 `:360` 的编码后体积闸门",
        package="ys-model-openai-compat",
        desc="文件在 stat 与读盘之间长大 ⇒ 超限图被当正常图送上线",
        expect_red="truncated_read_is_caught_by_the_encoded_size_gate",
        edits=[
            (
                ADAPTER_MEDIA,
                """    if url.len() > limit {
        return Err(MediaError::SingleTooLarge {
            bytes: url.len(),
            limit,
        });
    }
""",
                "",
            )
        ],
    ),
]


class InjectionError(RuntimeError):
    """注入本身失败（文本不匹配 / 不唯一）—— 绝不能当成「测试通过」。"""


def apply_edits(edits: list[tuple[str, str, str]]) -> None:
    """按精确文本替换注入。`old` 必须**恰好命中一次**。

    逐个文件读改写：同一文件的多处 edit 顺序应用，第二处看到的是第一处之后的内容
    （因此同一文件的多处 edit 不得互相覆盖）。
    """
    for rel, old, new in edits:
        path = ROOT / rel
        src = path.read_text(encoding="utf-8")
        count = src.count(old)
        if count == 0:
            raise InjectionError(f"{rel}: 找不到待替换文本（源码已变？）\n---\n{old[:200]}")
        if count > 1:
            raise InjectionError(f"{rel}: 待替换文本命中 {count} 次（须唯一）")
        path.write_text(src.replace(old, new), encoding="utf-8")


def restore(snapshot: dict[str, str]) -> None:
    for rel, content in snapshot.items():
        path = ROOT / rel
        if path.read_text(encoding="utf-8") != content:
            path.write_text(content, encoding="utf-8")


def verify_restored(snapshot: dict[str, str]) -> None:
    """还原后的硬校验：每份目标文件必须与快照逐字节相同。

    为什么值得一条独立检查：一条没还原干净的源码会让**后面每个 mutant 的结果都是假的**
    （注入命中旧文本、或文件已被改过）。宁可当场硬失败，也不要输出一张错位的汇总表。
    """
    for rel, content in snapshot.items():
        actual = (ROOT / rel).read_text(encoding="utf-8")
        if actual != content:
            raise RuntimeError(f"还原失败：{rel} 与快照不一致（后续结果不可信）")


def run_package_tests(package: str) -> tuple[str, list[str], str]:
    """跑整包测试。

    返回 (状态, 失败的用例名列表, 编译错误正文)：
      - ``"red"``   —— 编译通过且**有**用例失败（真正的「被抓住」）。
      - ``"green"`` —— 编译通过且全绿（**覆盖缺口**）。
      - ``"build-failed"`` —— 编译不过。对普通变异是「注入写法有问题、不算被抓住」；
        对 [`Mutant.compile_is_catch`] 的变异则**正是被抓住**（类型层守护）。
      - ``"errored"`` —— 跑起来了但非零退出、且没有 FAILED 行（测试二进制崩溃/挂住）。
    """
    proc = subprocess.run(
        # `--no-fail-fast`：cargo 默认在第一个失败的测试**二进制**处收手 ⇒ 单测红了就
        # 永远看不到集成测试的结果，报告里的「抓住它的用例」会残缺。
        [
            "cargo",
            "test",
            "-p",
            package,
            "--no-fail-fast",
            "--",
            "--test-threads=1",
        ],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    failed: list[str] = []
    for line in proc.stdout.splitlines():
        m = re.match(r"^test\s+(\S+)\s+\.\.\. FAILED", line)
        if m:
            failed.append(m.group(1))
    if failed:
        return "red", failed, ""
    # 编译失败：stderr 里会有 cargo/rustc 的编译错误标记。
    if "could not compile" in proc.stderr or re.search(r"^error\[E", proc.stderr, re.M):
        return "build-failed", [], proc.stderr
    if "running " not in proc.stdout:
        return "build-failed", [], proc.stderr
    if proc.returncode != 0:
        # 跑起来了但非零退出、且没抓到 FAILED 行（如某个测试二进制挂掉）。
        return "errored", [], ""
    return "green", [], ""


def preflight(snapshot: dict[str, str]) -> list[str]:
    """跑前把所有 `old` 锚点全部试一遍：任何一条不命中就**在跑第一个 mutant 之前**报出来。

    为什么提前：锚点失效意味着源码变了而本脚本没跟上（正是拖选失守的形态）。
    一路跑到中途才失败会白花几分钟，且容易让人以为「只是那条 mutant 不行」。
    这里只**检查**不写下 —— 真注入仍逐条实时做（同一文件的多处 edit 有先后依赖）。
    注意：每个 mutant 各自从原始快照独立试，**不**把前面的结果累加给后面的。
    """
    problems: list[str] = []
    for i, m in enumerate(MUTANTS, 1):
        # **每个 mutant 从原始快照开始**：mutant 之间共享锚点（如 `basic.rs` 的折叠循环、
        # `media.rs` 的 Key 构造），累积应用会让先跑的 mutant 抢走后面要用的原文。
        probe = dict(snapshot)
        for rel, old, new in m.edits:
            count = probe[rel].count(old)
            if count != 1:
                problems.append(
                    f"[{i}] {m.name}\n    {rel}: 锚点命中 {count} 次（须恰好 1 次）\n    {old.splitlines()[0][:120]}"
                )
                break
            probe[rel] = probe[rel].replace(old, new)
    return problems


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("ids", nargs="*", help="只跑指定编号（1 起）")
    ap.add_argument("--list", action="store_true", help="只列清单")
    args = ap.parse_args()

    if args.list:
        for i, m in enumerate(MUTANTS, 1):
            print(f"{i:2}. [{m.package}] {m.name}")
            print(f"    {m.desc}")
            if m.expect_red:
                print(f"    期望红：{m.expect_red}")
        return 0

    selected = MUTANTS
    if args.ids:
        try:
            wanted = {int(x) for x in args.ids}
        except ValueError:
            print("编号必须是整数", file=sys.stderr)
            return 2
        selected = [m for i, m in enumerate(MUTANTS, 1) if i in wanted]
        if not selected:
            print("没有匹配的编号", file=sys.stderr)
            return 2

    snapshot = {rel: (ROOT / rel).read_text(encoding="utf-8") for rel in TARGETS}

    problems = preflight(snapshot)
    if problems:
        print("锚点自检失败 —— 源码已变而本脚本没跟上：\n", file=sys.stderr)
        for p in problems:
            print(f"  {p}\n", file=sys.stderr)
        return 1

    results: list[tuple[Mutant, str, list[str], str]] = []

    try:
        for i, m in enumerate(MUTANTS, 1):
            if m not in selected:
                continue
            print(f"\n=== [{i}] {m.name} ===", flush=True)
            try:
                apply_edits(m.edits)
            except InjectionError as err:
                restore(snapshot)
                print(f"  注入失败：{err}", file=sys.stderr)
                results.append((m, "injection-failed", [], ""))
                continue
            status, failed, build_err = run_package_tests(m.package)
            restore(snapshot)
            verify_restored(snapshot)
            if status == "red":
                hit = [f for f in failed if m.expect_red in f] if m.expect_red else failed
                if m.expect_red and not hit:
                    print(
                        f"  ! 变红了，但**不是**期望的用例（期望含 {m.expect_red!r}）："
                        f"{', '.join(failed[:5])}"
                    )
                    status = "wrong-catcher"
                else:
                    print(f"  ✓ 被抓住（{len(failed)} 个用例红）：{', '.join(failed[:5])}")
            elif status == "build-failed" and m.compile_is_catch:
                # 类型层守护：编译失败**就是**被抓住，但必须确认错误来自预期靶子 ——
                # 否则「注入写错了」也会被当成守护生效。
                if m.compile_must_mention and m.compile_must_mention not in build_err:
                    print(
                        f"  ! 编译失败，但错误里没有预期靶子 {m.compile_must_mention!r}"
                        " —— 可能是注入写法问题"
                    )
                else:
                    print(f"  ✓ 被编译器拦住（类型层守护）：{m.compile_must_mention}")
                    status = "compile-caught"
            elif status == "build-failed":
                print("  ! 编译失败 —— 注入写法有问题，不算被抓住")
                print("    " + "\n    ".join(build_err.splitlines()[:12]))
            elif status == "errored":
                print("  ! 测试非零退出但无 FAILED 行（崩溃/挂住）—— 需人工看")
            else:
                print("  ✗ 未被抓住 —— 覆盖缺口！")
            results.append((m, status, failed, "ok"))
    finally:
        restore(snapshot)
        verify_restored(snapshot)

    print("\n" + "=" * 78)
    print("图像输入变异测试汇总")
    print("=" * 78)
    caught = 0
    missed: list[str] = []
    for m, status, failed, _ in results:
        mark = {"red": "✓", "compile-caught": "✓"}.get(status, "✗" if status == "green" else "!")
        if status in ("red", "compile-caught"):
            caught += 1
        else:
            missed.append(m.name)
        print(f"{mark} [{m.package}] {m.name}")
        if failed:
            print(f"    抓住它的用例：{', '.join(failed[:3])}" + (" …" if len(failed) > 3 else ""))
        elif status == "compile-caught":
            print("    编译被拦住（类型层守护，无运行期用例可抓）")
        elif status == "injection-failed":
            print("    注入失败（源码已变，需更新本脚本）")
        elif status == "build-failed":
            print("    编译失败（注入写法有问题，不算被抓住）")
        elif status == "errored":
            print("    测试非零退出但无 FAILED 行（崩溃/挂住）—— 需人工看")
        elif status == "wrong-catcher":
            print(f"    变红了但不是期望的用例（期望含 {m.expect_red!r}）")
        else:
            print("    无用例变红 —— 这条逻辑没有被测试罩住")
    print("=" * 78)
    print(f"{caught}/{len(results)} 处变异被测试抓住")
    if missed:
        print("未被抓住：")
        for name in missed:
            print(f"  - {name}")
    return 0 if caught == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
