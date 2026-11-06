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

### 仍然没做到的

**真实 GUI 还是连不上。** 它调用 `session.create` / `session.page` / `session.stream` 等
2 个之外的方法，全部返回 `NOT_IMPLEMENTED`。所以这条限定只补了一半：现在**能列出会话**，
但**不能开新会话、不能流转**。

差别在于：之前是「什么都不能调」，现在是「能调两个、其余的会说缺什么」。后者对排查有用，
对使用还不够。
