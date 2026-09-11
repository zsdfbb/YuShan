//! 端到端测试：agent 用全部 4 个工具（Write、Bash、Edit、Read）完成任务。
//!
//! 要求：YUSHAN_API_BASE 与 YUSHAN_API_KEY 环境变量。
//! 运行方式：
//!   YUSHAN_API_BASE=https://api.deepseek.com YUSHAN_API_KEY=sk-xxx \
//!     cargo test -p yushan-coding-agent e2e -- --ignored --nocapture

use agent_event::NoopEventSink;
use agent_loop::AgentInput;
use agent_model_openai_compatible::{
    OpenAICompatibleConfig, OpenAICompatibleModel, compat::ProviderCompat,
};
use agent_runtime::AgentBuilder;
use agent_session::MemorySession;
use agent_tools_basic::{BashTool, EditTool, ReadTool, WriteTool};
use std::path::PathBuf;

fn workdir() -> PathBuf {
    let dir = std::env::temp_dir().join("yushan_e2e_test");
    // 清理之前的运行残留
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
#[ignore] // 需要真实 API 凭证。运行：cargo test -- --ignored
async fn e2e_four_tools_calculator() {
    let api_base = std::env::var("YUSHAN_API_BASE")
        .or_else(|_| std::env::var("OPENAI_API_BASE"))
        .expect("Set YUSHAN_API_BASE or OPENAI_API_BASE env var");
    let api_key = std::env::var("YUSHAN_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .expect("Set YUSHAN_API_KEY or OPENAI_API_KEY env var");

    let wd = workdir();

    // 用低温构建真实 agent 以保证确定性
    let model = OpenAICompatibleModel::new(OpenAICompatibleConfig {
        api_base,
        api_key,
        model: "deepseek-chat".into(),
        max_tokens: Some(4096),
        temperature: Some(0.0),
        compat: ProviderCompat::deepseek(),
    });

    let task = format!(
        "Create a file at {}/calc.py with exactly this content:\n\
         def add(a, b):\n    return a + b\n\n\
         Then run: python3 -c \"from calc import add; assert add(2,3)==5; print('OK')\"\n\
         Then edit the file to add a multiply function after the add function:\n\
         def multiply(a, b):\n    return a * b\n\n\
         Then run: python3 -c \"from calc import multiply; assert multiply(4,5)==20; print('OK')\"",
        wd.display()
    );

    let mut agent = AgentBuilder::new()
        .model(model)
        .tool(ReadTool::new(wd.clone()))
        .tool(WriteTool::new(wd.clone()))
        .tool(EditTool::new(wd.clone()))
        .tool(BashTool::new(wd.clone()))
        .session(MemorySession::new())
        .events(NoopEventSink)
        .build()
        .expect("failed to build agent");

    let input = AgentInput::text(&task);
    let result = tokio::time::timeout(std::time::Duration::from_secs(120), agent.run_turn(input))
        .await
        .expect("test timed out after 120s")
        .expect("agent run_turn failed");

    // 验证 agent 产出了响应
    assert!(
        result.final_message.is_some(),
        "agent should produce a final message"
    );

    // 验证 calc.py 已创建且含两个函数
    let calc_py = wd.join("calc.py");
    assert!(
        calc_py.exists(),
        "calc.py should exist at {}",
        calc_py.display()
    );

    let content = std::fs::read_to_string(&calc_py).expect("failed to read calc.py");
    assert!(
        content.contains("def add(a, b)") || content.contains("def add("),
        "calc.py should contain add function. Content:\n{content}"
    );
    assert!(
        content.contains("def multiply(a, b)") || content.contains("def multiply("),
        "calc.py should contain multiply function. Content:\n{content}"
    );

    // 清理
    let _ = std::fs::remove_dir_all(&wd);
}
