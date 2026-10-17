//! `--help` 文案：**纯文本常量**。
//!
//! 零 IO、零状态、零副作用、零依赖 —— 不读环境变量、不读 `all_commands()`、不碰文件系统。
//! 文案是**手写**的静态字符串：与真实 CLI 语义可能漂移，由 `main.rs` 里的
//! `parse_args` 侧单测与文案内容单测共同兜底。

/// 帮助文档全文（四段：用法 / 环境变量 / TUI 斜杠命令 / 示例）。
const HELP_TEXT: &str = r#"ys-coding-agent —— 轻量、组件化的 Agent 运行时

用法：
  ys-coding-agent                       启动交互式 TUI（默认）
  ys-coding-agent -p <任务>              单次 print 模式（边生成边打印文本增量）
  ys-coding-agent --json <任务>          JSON 事件模式（逐行 Envelope）
  ys-coding-agent --resume [<id>]        恢复既有会话（默认新会话；裸 --resume 列出候选并选择，
                                        --resume <id> 指定会话 id 或文件路径）
  ys-coding-agent worktree <动词> …      create [name] | attach [name] | list | remove <name>
  ys-coding-agent --help | -h | help     打印本帮助后退出（不读凭证、不建运行时）

公共 flag：
  --debug         打开结构化执行日志（YUSHAN_DEBUG 环境变量优先于本 flag）
  --resume [<id>] 恢复既有会话；空格形态 <id>，旧的 `--resume=<id>` 已移除
  --stats         把背压统计摘要写到 stderr（仅 `-p` / `--json` 的 flag 区）
  -h              `--help` 的别名

环境变量：
  YUSHAN_API_BASE              模型 API base URL
  YUSHAN_API_KEY               API 凭证
  YUSHAN_MODEL                 模型名（覆盖 env / meta / 首个有凭证者）
  YUSHAN_REASONING_EFFORT      思考档位：minimal / low / medium / high / xhigh / max，默认 high；
                               仅 provider 声明支持时随请求发送
  YUSHAN_CHANNEL_CAPACITY      信道容量（下限 16，默认 1024）
  YUSHAN_SESSIONS_DIR          会话目录覆盖
  YUSHAN_LOG_DIR               日志目录覆盖
  YUSHAN_DEBUG                 打开 --debug（1/true/yes/on）；非空其他值关闭，优先于命令行 flag
  YUSHAN_DEBUG_ARGS            工具参数摘要档位：none / head / full，默认 head
  YUSHAN_SEARCH_SEARXNG_URL    搜索后端：SearXNG
  YUSHAN_SEARCH_DEEPSEEK_KEY   搜索后端：DeepSeek（亦认 DEEPSEEK_API_KEY，命名空间名优先）
  YUSHAN_SEARCH_TAVILY_KEY     搜索后端：Tavily（亦认 TAVILY_API_KEY，命名空间名优先）
  YUSHAN_SEARCH_BOCHA_KEY      搜索后端：Bocha
  YUSHAN_SEARCH_FIRECRAWL_KEY  搜索后端：Firecrawl
  YUSHAN_FETCHED_DIR           webfetch 落盘目录覆盖（须为绝对路径）
  YUSHAN_SUBAGENTS_DIR         子 Agent 产物目录覆盖（只写不删）
  YUSHAN_WORKTREES_DIR         受管工作树父目录覆盖（默认 $HOME/.yushan/worktrees）
  搜索后端各自配置才进链，彼此独立、不互斥。

TUI 斜杠命令：
  以下斜杠命令仅在交互式 TUI 内可用，入口：先启动 TUI 然后输入 /。
  /help [command]      显示可用命令
  /status              显示当前配置与状态
  /skills              查看并选择一个 skill
  /copy                复制最后一条回复（v1：写进对话区，手动选中）
  /quit                退出
  /thinking [on|off]   开关思考内容的显示
  /effort [off|minimal|low|medium|high|xhigh|max|default]   设置模型思考档位
  /model [model_name]  切换模型（无参时浮层选择）
  /login               登录 provider 并保存凭证
  /logout              登出并清除凭证
  /new                 开新会话
  /resume [id]         恢复既有会话（无参列出并选择）
  /compact             压缩上下文
  /export [path]       导出会话到文件

示例：
  ys-coding-agent -p "解释这个仓库的架构"
  ys-coding-agent --json --stats "跑一遍测试"
  ys-coding-agent --resume 2026-10-07-abc
"#;

/// 帮助文档全文（供 `main` 在 `parse_args` 之后立即早退打印）。
pub fn help_text() -> &'static str {
    HELP_TEXT
}
