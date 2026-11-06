# 阶段 4 接口草图

迁移计划 §3.3 要求：**在写垂直切片代码前，先做接口草图**，列出 `llm` / `session` / `tools` / `loop`
四个 seam 的 `ServiceKey` 定义、涉及的事件 trait，并检查有没有写不出来的接口。

本文是那份草图。目的不是实现，而是**在投入 8-12 周之前，确认这四个接口写得出来**。

## 结论先行

四个 seam 都能写出来，但有**三处必须在动手前定下来**，否则会返工：

1. **`AbortSignal` → `CancellationToken` 的传播方式**：上游 1885 处 `AbortSignal`，工具执行要求
   「异步工作必须观察或转发 `exec.signal`」。Rust 里这是**每个异步函数都要显式传** `&CancellationToken`，
   不是隐式上下文。阶段 4 的每个 seam 都要接受一个 token 参数。
2. **`AsyncLocalStorage` 的 34 处必须改显式传参**：上游用它追踪「哪个 Agent 发起了当前操作」。
   Rust 的 `task_local!` 不跨 `tokio::spawn`（已实测），所以 `agent id` 必须作为参数出现在每个
   工具调用和 LLM 请求里。
3. **流式响应的 `Send` 约束**：`llm/stream` 返回 `AsyncIterable<StreamChunk>`，Rust 侧是
   `BoxFut<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>>`。整个链路必须 `Send`，包括 provider 的
   内部状态。这是 `Send` 传染最集中的地方。

## 1. llm seam

```rust
/// 一次 LLM 调用的完整配置。
pub struct LlmRequest {
    pub model: String,
    pub messages: Vec<RequestMessage>,
    pub tools: Vec<ToolSchema>,
    pub system_prompt: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

/// 流式增量。
pub enum StreamChunk {
    BlockStart { index: usize, block_type: ContentBlockType },
    TextDelta { index: usize, text: String },
    ReasoningDelta { index: usize, text: String },
    ToolCallDelta { index: usize, id: ToolCallId, name: Option<String>, arguments_delta: String },
    BlockEnd { index: usize, block: ContentBlock },
    Usage(TokenUsage),
    Finish { reason: FinishReason },
}

pub trait LlmProvider: Send + Sync {
    /// 流式生成。必须观察 `cancel`：调用方 Ctrl-C 时，未完成的流要被丢弃而非泄漏。
    fn stream(
        &self,
        request: LlmRequest,
        cancel: CancellationToken,
    ) -> BoxFut<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>>;
}

pub struct LlmKey;
impl ServiceKey for LlmKey {
    type Service = dyn LlmProvider;
    const ID: &'static str = "llm";
}
```

**写得出来吗？** 能。`Stream + Send` 是标准的 `futures` 用法。
**风险点**：`ContentBlock` 是 7 个变体的判别联合，`StreamChunk` 引用它。用 `#[serde(tag = "type")]` 直接对应。

**事件 trait**：`LlmStream`（waterfall，17 个之一）、`LlmRegistry`（emit）。

## 2. session seam

```rust
/// 会话事件，落盘单位。是最小变体集，不是全部 60+。
#[serde(tag = "type")]
pub enum SessionEvent {
    #[serde(rename = "turn/start")]
    TurnStart { turn: u64 },
    #[serde(rename = "user/message")]
    UserMessage { content: Vec<ContentBlock> },
    #[serde(rename = "assistant/message")]
    AssistantMessage { content: Vec<ContentBlock> },
    #[serde(rename = "tool/call")]
    ToolCall { id: ToolCallId, name: String, arguments: String },
    #[serde(rename = "tool/result")]
    ToolResult { id: ToolCallId, content: Vec<ContentBlock>, is_error: bool },
    #[serde(rename = "turn/end")]
    TurnEnd { turn: u64, reason: FinishReason },
}

pub trait SessionStore: Send + Sync {
    /// 追加一条事件并落盘。JSONL + zstd。
    fn append(&self, event: SessionEvent) -> BoxFut<Result<(), SessionError>>;
    /// 按序读回全部事件。
    fn replay(&self) -> BoxFut<Result<Vec<SessionEvent>, SessionError>>;
}

pub struct SessionKey;
impl ServiceKey for SessionKey {
    type Service = dyn SessionStore;
    const ID: &'static str = "session";
}
```

**写得出来吗？** 能。判别联合 + `#[serde(tag = "type")]` 已验证过（阶段 0 的附录检查）。
**风险点**：上游 `session/event` 是 emit 事件，18 个领域里最大的之一。阶段 4 只做最小变体集，
新变体靠 `match` 穷尽性在编译期提醒。

## 3. tools seam

```rust
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema，直接透传给模型。
    pub parameters: serde_json::Value,
    /// 执行。必须观察 `cancel`。
    pub execute: Arc<dyn Fn(serde_json::Value, ToolRunContext) -> BoxFut<ToolResult> + Send + Sync>,
}

pub struct ToolRunContext {
    pub call_id: ToolCallId,
    /// 显式传入，替代上游的 AsyncLocalStorage。
    pub agent_id: AgentId,
    pub cancel: CancellationToken,
}

pub enum ToolResult {
    Success(serde_json::Value),
    Failure { message: String, is_error: bool },
}

pub trait ToolRegistry: Send + Sync {
    fn schemas(&self) -> Vec<ToolSchema>;
    fn get(&self, name: &str) -> Option<Arc<ToolDefinition>>;
}

pub struct ToolsKey;
impl ServiceKey for ToolsKey {
    type Service = dyn ToolRegistry;
    const ID: &'static str = "tools";
}
```

**写得出来吗？** 能，但有一处**必须现在决定**：`execute` 用 `Arc<dyn Fn>` 还是 `async fn` trait。

- `Arc<dyn Fn>` + `BoxFut`：object-safe，能存进 `HashMap`，但每次调用要 clone `Arc`。
- `async fn` in trait（Rust 1.75+）：不能直接 object-safe，需要 `#[async_trait]` 或泛型，泛型无法存进注册表。

**结论：用 `Arc<dyn Fn> -> BoxFut`。** 这是计划 §1.3 约束一的直接后果。

**事件 trait**：`ToolPreExecute` / `ToolExecute` / `ToolPostExecute` 三个 waterfall，这是阶段 4 的核心路径。

## 4. loop seam

```rust
/// 一次对话的主循环：assembling → streaming → tool dispatch → repeat。
pub trait AgentLoop: Send + Sync {
    fn run(
        &self,
        prompt: String,
        cancel: CancellationToken,
    ) -> BoxFut<Result<(), LoopError>>;
}
```

**写得出来吗？** 能，但这个接口**太空**——它把整个循环藏在一个 future 里，无法测试中间状态。
计划要求「中途 Ctrl-C 能干净取消」，这需要一个能观察每步的能力。

**建议改成**：`run` 返回一个 `Stream<Item = LoopStep>`，调用方可以逐条观察：

```rust
pub enum LoopStep {
    TurnStarted { turn: u64 },
    AssistantDelta(String),
    ToolCallStarted { name: String },
    ToolCallFinished { name: String, is_error: bool },
    TurnEnded { turn: u64 },
}

fn run(&self, prompt: String, cancel: CancellationToken)
    -> BoxFut<Pin<Box<dyn Stream<Item = Result<LoopStep, LoopError>> + Send>>>;
```

这样 CLI 只需 `while let Some(step) = stream.next().await` 打印，测试也能断言中间序列。

## 三处必须现在定的决定

### 决定一：取消如何传播

上游靠 `AbortSignal` 的隐式传播：任何持有 `exec.signal` 的异步函数都能注册 listener。
Rust 必须显式。三个选项：

| 方案 | 代价 | 判断 |
|---|---|---|
| 每个异步函数加 `cancel: &CancellationToken` 参数 | 签名到处变长 | **选这个**，显式且可测 |
| `task_local!` 存 token | 不跨 `tokio::spawn`（已实测） | 否 |
| 结构体字段持有 token | 生命周期复杂，clone 语义模糊 | 否 |

### 决定二：`agent_id` 如何到达工具和 LLM

同样是 `AsyncLocalStorage` 的替代。结论同上：放进 `ToolRunContext` 和 `LlmRequest`，
不放进任何隐式上下文。

### 决定三：`BoxFut` 的 clone 开销

计划 §2.7 决策点要求评估「`BoxFut` 的 clone 开销是否可接受」。实测数据要等实现出来，
但可以先定的原则：**`BoxFut` 只在 seam 边界出现，seam 内部用具体类型**。这样 hot path
（流式 chunk 处理）不走 `BoxFut`。

## 写不出来的接口

**目前没有。** 四个 seam 都能用现有原语表达。唯一需要额外注意的是：

- `LlmProvider::stream` 返回的 `Stream` 必须 `Send`，这会传染到 provider 内部状态。
  如果某个 provider（如未来的浏览器自动化 provider）持有非 `Send` 句柄，需要 `LocalSet`。
  这不是阶段 4 的问题（DeepSeek provider 是纯 HTTP），但要在阶段 5 记住。

## 下一步

按计划 §3.2 的五步流程：**写接口 → 写测试 → 实现 → 验证 → 记录偏差**。

接口已在本文确定。下一步是实现 `llm` + `session` 两个 seam 的类型与单测，
然后 `tools`，最后 `loop` 和 CLI。
