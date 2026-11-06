# 阶段 6 范围与取舍

迁移计划 §2.9 的目标是「现有 Web GUI 连上 Rust 后端」。这个目标之前，必须先回答一个
计划里没有回答的问题：**RPC 协议到底是什么形状，Rust 侧要不要一比一复刻？**

本文是先查清事实、再定范围的结论。

## 查清的事实

### 传输层

- **一个 WebSocket 多路复用端点**：`/api/remote.mux`（`REMOTE_STREAM_MUX_PATH`），
  所有 Typert Remote 逻辑流共用这一条物理连接。
- **HTTP 用于一元调用和事件回传**：`$events/result` 之类的内部端点。
- Web 服务器是 `node:http`，路由按精确路径与前缀注册，无框架。

### 消息形状

有四种下行帧，都是 `{ type: ... }` 判别联合：

| type | 用途 |
|---|---|
| `ready` | 连接建立，携带 `clientId` 与 host 信息 |
| `emit` | 一条 Cordis 事件通知：`{ event, args }` |
| `waterfall` | 一个待决的 waterfall：`{ event, eventId, agentId, request }` |
| `cancel` | 取消一个待决 waterfall：`{ eventId }` |

上行有 `RemoteEventResult`：`{ clientId, eventId, outcome }`，`outcome.kind` 是
`next` / `result` / `rejected` 三态。

### 请求形状

```ts
interface InvokeRemoteRequest {
  namespace: string   // 如 'session'
  method: string      // 如 'list'
  args: Record<string, unknown>
  peer: PeerScope     // 调用方身份
  signal?: AbortSignal
}
```

**关键发现**：`args` 是 `Record<string, unknown>`——**运行时无类型的**。类型安全完全来自
Typert 在**编译期**生成的描述符。也就是说，线路上没有类型信息，类型是 TS 编译器在两端的
一个投影。

## 这意味着什么

计划 §2.9 说「Typert 是 TS 编译器驱动的类型投影，Rust 侧要重新设计整个类型安全 RPC 生成链路，
这部分没有现成方案，是阶段 6 的最大风险」。

**实测确认了这个判断，而且比计划描述得更具体**：

- 线路本身是**无类型 JSON**，所以 Rust 侧**可以实现**它——不需要复刻 Typert 的生成链路。
- 但**失去的是编译期保证**：TS 端改一个字段名，Rust 端不会编译失败，只会在运行时
  多一个 `unknown` 字段或少一个字段。
- 唯一能拿回部分保证的办法是 **schema 为中心的契约**：Rust 用 `schemars` 生成 JSON Schema，
  TS 端从同一份 schema 生成类型，双方从同一份产物出发。

## 本阶段的范围决定

**不做完整 Typert 复刻，做最小可连通后端。**

理由：完整复刻意味着重建编译期类型投影链路（Typert 3 万行），这是计划明确标记为
「最大风险」且「没有现成方案」的部分。在验证「前端能否连上 Rust 后端」这个目标上，
它是不必要的。先证明连接成立，再决定类型安全投入多少。

具体做：

1. **`/api/remote.mux` WebSocket 端点**——接受连接，发 `ready`，收发四种帧
2. **`$events` 事件流**——把 Rust 事件总线的事件投影成 `emit` 帧推给客户端
3. **断线重连语义**——重连后状态一致（计划验收要求）
4. **JSON Schema 导出**——`schemars` 生成 RPC 消息的 schema，作为前后端契约的单一来源

不做：

- Typert 描述符生成（编译期类型投影）
- 完整的 `Remote` 方法命名空间（`session.*`、`workspace.*` 等几十个方法）
- 客户端 `remote-stream.ts` 的完整语义（journal / snapshot 流）
- 认证与权限（`PeerScope` 只透传，不校验）

## 为什么这样划分是对的

计划 §2.9 自己给了理由：「保持前端不动，迁移期间可以逐 seam 替换后端，**这是最有价值的
降险手段**」。

「逐 seam 替换」的前提是**连接先通**。连接不通，前端一行都动不了，后面所有 seam 的替换
都无从谈起。所以连接是第一步，类型安全是第二步。

反过来，如果先做类型投影，会在一个尚未验证的连接上投入数周——而连接一旦不通（比如
WebSocket 握手的某处语义不一致），这些投入全部作废。

## 已知会留下的缺口

即使本阶段完成，下面这些仍然缺失，必须写清楚：

1. **字段名改动不会被编译发现。** TS 端把 `sessionId` 改成 `sessionID`，Rust 端只会收到一个
   陌生的字段。缓解手段是 schema 对比测试（本阶段做），但覆盖不到手工写的序列化代码。
2. **`PeerScope` 不做校验。** 任何客户端可以声称自己是任何 peer。认证属于部署问题，
   但必须记住：这不是安全的实现。
3. **没有 `Remote` 方法命名空间。** 前端调用 `remote.session.list()` 这类生成方法时，
   Rust 端没有对应实现。所以本阶段的「连通」是**协议连通**，不是**功能可用**。

第 3 点尤其重要：本阶段完成后，用真实 DSH Web GUI 连这个后端**不会正常工作**，因为
GUI 要求的是完整的 Typert 方法集。本阶段的验收是**协议层连通 + 重连语义**，用
一个协议测试客户端验证，而不是用真实 GUI。

这个限定必须写进 README，否则「阶段 6 完成」会被误读为「GUI 能用了」。

## 后续：Remote 方法表（补上第 3 点的一半）

上面第 3 点说「没有 `Remote` 方法命名空间」。后来补上了，见
`crates/girder-core/src/remote.rs`。

### 实现了什么

| 方法 | 状态 |
|---|---|
| `session.list` | ✅ 返回真实会话行 |
| `session.prompt` | ✅ 接受并确认（本 build 没有 agent 注册表可挂） |
| 其余 20 个 `session.*` 方法 | ❌ 声明为 `NOT_IMPLEMENTED` |
| `workspace.*`（11 个） | ❌ 未注册 |

### 三个错误码的区别为什么重要

客户端必须能分辨三种失败，否则半成品客户端看起来像坏了，而不是像没做完：

| code | 含义 |
|---|---|
| `UNKNOWN_METHOD` | 名字打错了 |
| `NOT_IMPLEMENTED` | 方法存在（上游有），本 build 没做 |
| `INVALID_ARGUMENTS` | 参数类型不对 |

实现里 `declare_unimplemented` 维护一份**显式清单**，因为这个区别无法从「查不到」推断出来。

### 实测

```
$ girder serve --bind 127.0.0.1:8806
girder rpc exposing 2 Remote method(s): session.list, session.prompt

POST /api/remote.invoke {"namespace":"session","method":"list","args":{},"peer":"p"}
→ {"status":"ok","value":{"items":[{"sessionId":"demo","blank":false,"updatedAt":...}]}}

POST ... {"method":"search"}
→ {"status":"error","error":{"code":"NOT_IMPLEMENTED","message":"`session.search` is not implemented in this build"}}

POST ... {"method":"lst"}
→ {"status":"error","error":{"code":"UNKNOWN_METHOD","message":"unknown method `session.lst`"}}
```

### 后续二：create / page / events

又补了三个方法，现在共 **5 个**：

| 方法 | 状态 |
|---|---|
| `session.list` | ✅ |
| `session.create` | ✅ 幂等：同 id 重复创建不追加事件 |
| `session.page` | ✅ 分页读取，`cursor` / `hasMore` 语义对齐上游 |
| `session.events` | ✅ 与 `page` 同一实现（上游两者并存，`page` 已弃用但仍被调用） |
| `session.prompt` | ✅ 接受并确认 |
| 其余 15 个 | ❌ `NOT_IMPLEMENTED` |

**分页语义**：序列号是日志中 1-based 的位置，客户端把 `throughSeq` 钉住以读固定的切面。
`maxMessages` 从切面**往前**取，因为历史视图是从最新往上滚。`beforeSeq` 排除该记录及其之后。

**`create` 的幂等性**通过测试钉住：`create_twice_on_a_directory_reuses_the_existing_log`
断言第二次 create 后日志仍是 1 条事件。这不是可选的优化——客户端重试 create 时如果追加
一条 `TurnStart`，会话历史就会多出一个空 turn。

### 实测

```
$ girder serve --bind 127.0.0.1:8810
girder rpc exposing 5 Remote method(s): session.create, session.events, session.list,
                                        session.page, session.prompt

create {"sessionId":"alpha"} → {"sessionId":"alpha"}
list                         → items:[{sessionId:"alpha", blank:true, ...}]
page   {address:alpha}       → {cursor:1, hasMore:false, records:[{type:"event", event:{type:"turn/start"}}]}
create {"sessionId":"alpha"} → 同一个 id，日志仍是 1 条事件
```

### 后续三：prompt 真的会跑一轮

之前 `session.prompt` 返回 `accepted:true` 却什么都不做——这是所有实现里最「说谎」的一处。
现在它真的启动 `agent_loop`。

**`AgentRuntime` 的职责**：持有 provider 与工具集，管理「每个 session 同时只有一个 turn」，
把 loop 的步骤发布到事件总线（`agent/turn` 事件）。

**为什么必须拒绝并发 prompt**：同一会话两个 turn 会交错追加消息，模型会看到一段从未发生过的
对话。测试 `a_second_prompt_while_running_is_refused` 钉住这条。

**为什么 prompt 先写日志再 spawn**：prompt 必须持久化，即使模型永不回答。但这也引入了一个
bug——见下。

### 一个被端到端测试抓到的真实 bug

第一版跑通后翻日志，发现**两条相同的 `user/message`**：

```
turn/start
user/message   ← runtime 写的
user/message   ← agent_loop::run 又写了一次
turn/start
...
```

`AgentRuntime::prompt` 和 `agent_loop::run` 各写了一次。修复是给 `LoopContext` 加
`record_prompt: bool`：直接调用方（CLI）保持 true，RPC runtime 设为 false。

加测试 `the_prompt_is_recorded_exactly_once` 钉住，断言日志里恰好一条。

**这个 bug 只有跑真实端到端才会暴露**——单元测试里 runtime 和 loop 各自都被正确调用，
只有把两者串起来才看见重复。

### 实测

```
$ GIRDER_SESSIONS=/tmp/gp girder serve --bind 127.0.0.1:8820
girder rpc exposing 5 Remote method(s): session.create, ...

create {"sessionId":"v2"}              → ok
prompt {"sessionId":"v2","text":"list files"} → {"accepted":true,"turn":2}
prompt 同一会话（运行中）                → error: "already running a turn"

page {address:v2} → 10 条事件：
  turn/start, user/message, turn/start, assistant/message(tool-call),
  tool/call, tool/result, turn/end, turn/start, assistant/message, turn/end
```

即：RPC 发的 prompt 真的驱动了模型 → 调工具 → 结果写回 → 第二轮回答。

### 仍然没做到的

- **流式推送**：事件发到总线了，但 `serve` 默认 `--watch` 为空，所以客户端看不到。
  需要 `--watch agent/turn` 才能收。`known_events()` 目前只列工具事件，没列 `agent/turn`。
- **`session.stream` / `observeEvent`**：上游用 `mode: 'stream'` 的专用通道，本 build 没有。
- **`session.cancel`**：`AgentRuntime::cancel` 已实现，但没有对应的 RPC 方法。
- **模型选择**：`selectModel` / `modelCatalog` 仍未实现。
