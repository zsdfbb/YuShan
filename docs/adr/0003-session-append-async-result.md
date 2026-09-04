# 0003 — Session::append 定为 async + Result，messages() 保持同步切片视图

设计草图曾把 Session 写成 `append(&mut self, Message)` 无返回值。定稿为 `async fn append(&mut self, message: Message) -> Result<(), SessionError>`：第 2 步的 JSONL 持久化是追加写穿，写入可能失败；若 v0 按草图定签名，持久化落地时必须破坏性修改该 trait 及所有实现与调用点。`messages() -> &[Message]` 保持同步：会话以内存视图呈现，启动时全量加载，持久化是实现细节。

理由：trait 签名是本项目承诺稳定的接口（第 2 步适配器、第 4 步动态插件都要实现它），一次定对好过两次改；代价仅是 MemorySession 的 append 多一层 Result 包装。

## Consequences

- Session Store 一律采用「启动全量加载 + 追加写穿」策略；超大会话的懒加载留待真实需求出现再演进接口。
