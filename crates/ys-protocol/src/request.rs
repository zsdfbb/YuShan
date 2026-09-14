use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ys_core::Message;

use crate::Envelope;

/// UI → app 的能力请求（设计 §6）。
///
/// 在**回合边界**被 app 线程消费（`recv().await`）。命令的用户可见行为
/// （提示什么、问什么）归 UI；能力的实现归 app —— 漏实现是**编译错误**
/// （`match` 必须穷尽），不会静默漂移。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// agent 的输入（一条用户消息）。
    Prompt(Message),
    /// 切换当前模型。
    SetModel { model: String },
    /// 登录一个 provider 并持久化凭证。
    ///
    /// `api_base` 是**用户显式提供的** API base（`/login custom` 这类没有内置 base
    /// 的 provider 才需要）—— `None` 表示「按 app 侧的三级回退解析」（内置 base →
    /// `config.api_base`）。UI 只负责问，不问就 `None`，**绝不替 app 猜一个 URL**。
    ///
    /// `#[serde(default)]`：这是后加的字段，无此字段的旧 JSON（老日志 / 老 UI）
    /// 必须仍能反序列化成 `None`。
    Login {
        provider: String,
        api_key: String,
        #[serde(default)]
        api_base: Option<String>,
    },
    /// 登出当前 provider。
    Logout,
    /// 开新会话（换 `Session` + 空队列）。
    NewSession,
    /// 压缩上下文。
    Compact,
    /// 导出会话：`Some(path)` 为用户显式指定的路径，`None` 表示**由 app 侧
    /// 决定默认落点**（设计 §6「会话文件在 app 侧」）—— UI 不替 app 猜路径。
    Export { path: Option<PathBuf> },
}

/// app → UI 的出站消息（设计 §6）。
///
/// `V` 是**产品视图**（coding agent 用 `CodingView`）：协议只提供通用壳，
/// 视图类型归各产品 TUI crate。故这里用标准 derive —— 自动为 `V` 加上
/// `Serialize` / `Deserialize` 约束，`ys-protocol` 不必认识任何具体视图。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Outbound<V> {
    /// 事件（已封壳，含 source / turn）。
    Event(Envelope),
    /// 视图快照。
    View(V),
    /// 命令输出 → transcript。
    Output(String),
    /// 退出。
    Quit,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use ys_core::{ContentBlock, Role, ToolCallId};

    use crate::Boundary;

    /// 本地测试视图 —— 只用标准 derive 实现 serde，用来实例化
    /// `Outbound<TestView>`，验证泛型 derive 为 `V` 加上的
    /// `Serialize` / `Deserialize` 约束正确（`ys-protocol` 不必认识具体视图）。
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct TestView {
        n: u32,
    }

    fn text_message(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// 通用断言：值 → JSON → 值，必须与原值相等；顺带记录 JSON 形态。
    fn assert_roundtrip<T>(value: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(*value, back, "roundtrip 不一致，json = {json}");
    }

    #[test]
    fn test_request_prompt_text_roundtrip() {
        assert_roundtrip(&Request::Prompt(text_message(Role::User, "hello")));
    }

    #[test]
    fn test_request_prompt_tooluse_roundtrip() {
        // Message 里含 ToolUse 块：走 ys_core 的 serde，arguments 是 Value
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "calling".into(),
                },
                ContentBlock::ToolUse {
                    id: ToolCallId("call_1".into()),
                    name: "bash".into(),
                    arguments: json!({ "cmd": "ls", "flag": true }),
                },
            ],
        };
        assert_roundtrip(&Request::Prompt(msg));
    }

    #[test]
    fn test_request_set_model_roundtrip() {
        assert_roundtrip(&Request::SetModel {
            model: "gpt-4o".into(),
        });
    }

    /// `Login` 的两种 `api_base` 都要能过 serde：`Some` = 用户显式填的 URL
    /// （`custom`），`None` = 交给 app 侧回退。漏测任一种，app 侧消费端就可能
    /// 在真实信道上炸。
    #[test]
    fn test_request_login_roundtrip() {
        assert_roundtrip(&Request::Login {
            provider: "openai".into(),
            api_key: "sk-secret".into(),
            api_base: Some("https://api.example.com".into()),
        });
        assert_roundtrip(&Request::Login {
            provider: "deepseek".into(),
            api_key: "sk-secret".into(),
            api_base: None,
        });
    }

    /// **向后兼容**：老形态的 JSON（没有 `api_base` 键）必须仍能反序列化，
    /// 且落为 `None` —— 否则一次协议加字段就会让老 UI / 老日志在
    /// `serde_json::from_*` 处炸掉。
    #[test]
    fn test_request_login_without_api_base_key_deserializes_to_none() {
        let v = json!({ "Login": { "provider": "deepseek", "api_key": "sk-1" } });
        let req: Request = serde_json::from_value(v).unwrap();
        assert_eq!(
            req,
            Request::Login {
                provider: "deepseek".into(),
                api_key: "sk-1".into(),
                api_base: None,
            }
        );
    }

    #[test]
    fn test_request_unit_variants_roundtrip() {
        assert_roundtrip(&Request::Logout);
        assert_roundtrip(&Request::NewSession);
        assert_roundtrip(&Request::Compact);
    }

    /// `Export.path` 的两种形态都要能过 serde：`Some` = 用户显式路径，
    /// `None` = 默认落点由 app 侧决定。二者编码不同（`null` vs 字符串），
    /// 漏测任一种都会让 app 侧消费端在真实信道上炸。
    #[test]
    fn test_request_export_path_roundtrip() {
        assert_roundtrip(&Request::Export {
            path: Some(PathBuf::from("/tmp/out.jsonl")),
        });
        assert_roundtrip(&Request::Export { path: None });
    }

    #[test]
    fn test_outbound_event_roundtrip() {
        let env = Envelope::new(
            crate::Source::agent(),
            2,
            ys_event::AgentEvent::ModelTextDelta { text: "hi".into() },
        );
        assert_roundtrip(&Outbound::<TestView>::Event(env));
    }

    #[test]
    fn test_outbound_view_roundtrip() {
        assert_roundtrip(&Outbound::<TestView>::View(TestView { n: 42 }));
    }

    #[test]
    fn test_outbound_output_roundtrip() {
        assert_roundtrip(&Outbound::<TestView>::Output("done".into()));
    }

    #[test]
    fn test_outbound_quit_roundtrip() {
        assert_roundtrip(&Outbound::<TestView>::Quit);
    }

    #[test]
    fn test_boundary_steer_roundtrip() {
        assert_roundtrip(&Boundary::Steer(text_message(Role::User, "steer me")));
    }

    #[test]
    fn test_boundary_abort_serde_shape() {
        // 实际形态（serde 默认外部标签 + 单元变体）：裸字符串 `"Abort"`。
        // 用 Value 比较而非字符串匹配，避免依赖 JSON 空白/转义细节。
        let v = serde_json::to_value(Boundary::Abort).unwrap();
        assert_eq!(v, json!("Abort"), "Abort 序列化形态变化：{v}");
        assert_eq!(
            serde_json::from_value::<Boundary>(v).unwrap(),
            Boundary::Abort
        );
    }
}
