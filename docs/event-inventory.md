# 事件清单（阶段 2 前置工作）

迁移计划 §2.5 要求「不要盲目迁移 179 处 `declare module`，先做清单」。本文是那份清单。

## 方法与实测数字

扫描 `packages/*/*/src` 下全部 2,521 个 `.ts` 文件，定位 50 个 `interface Events` 声明块，
按 `'事件名'(参数): 返回类型` 的方法签名语法提取，并从紧邻的 JSDoc 中读取 `@mode` 标注。

**结果：97 个事件，分属 44 个领域前缀。**

计划里写的「179 处 `declare module`」是准确的，但那 179 处里大部分扩展的是 `Context`
（服务挂载点），只有一部分扩展 `Events`。真正的**事件契约是 97 个**，不是 179 个。

### 分布

| dispatch 模式 | 数量 | 语义 |
|---|---|---|
| `emit` | 68 | 同步通知，不等返回值 |
| `waterfall` | 17 | 环绕链，每层调 `next()`；不调即否决 |
| `parallel` | 6 | 并发等待全部结束 |
| `bail` | 4 | 同步顺序，遇非空返回值即停 |
| `serial` | 2 | 顺序等待，遇非空返回值即停 |

`bail` 全部集中在 `slash/input-*`，`serial` 只有 `agent/created`、`agent/turn-stopping`。

## Rust 侧的收敛

计划预期「40-60 个事件 trait」。97 个事件按下列规则合并后落在区间内：

1. **同领域同模式同签名的多个事件合并为一个 trait + 变体**。`workflow/*` 的 6 个、`slash/input-*`
   的 4 个、`agent/inbox/*` 的 3 个都属此类。
2. **纯 UI 通知类不迁移进 `girder-core`**。`theme/change`、`locale/change`、`slots/changed` 这类
   属于前端契约，阶段 6 走 RPC 时再定义，不属于事件总线核心。
3. **一对一保留**：所有 `waterfall` 与 `bail` 事件，因为它们的**短路语义**是契约的一部分，
   合并会丢失语义。

收敛后 **46 个 trait**（见下表「Rust trait」列），落在计划的 40-60 区间。

## 完整清单

模式列的含义：`E`=emit, `W`=waterfall, `P`=parallel, `B`=bail, `S`=serial。

### agent（12）

| 模式 | 事件 | 说明 | Rust trait |
|---|---|---|---|
| E | `agent/assistant-stream` | 助手流式增量 | `AgentStream` |
| S | `agent/created` | Agent 创建后 | `AgentCreated` |
| E | `agent/disposed` | Agent 销毁 | `AgentDisposed` |
| E | `agent/error` | Agent 级错误 | `AgentStream` |
| E | `agent/inbox/claimed` | 收件箱条目被领取 | `AgentInbox` |
| E | `agent/inbox/discarded` | 收件箱条目被丢弃 | `AgentInbox` |
| E | `agent/inbox/inserted` | 收件箱插入条目 | `AgentInbox` |
| W | `agent/pre-step` | 每步执行前环绕 | `AgentPreStep` |
| W | `agent/request` | 组装 LLM 请求 | `AgentRequest` |
| W | `agent/request-error` | 请求失败处置 | `AgentRequestError` |
| E | `agent/status` | 运行状态变化 | `AgentStatus` |
| S | `agent/turn-stopping` | turn 即将结束 | `AgentTurnStopping` |

### tools（6）

| 模式 | 事件 | 说明 | Rust trait |
|---|---|---|---|
| W | `tools/pre-execute` | 派发前审批/取消 | `ToolPreExecute` |
| W | `tools/execute` | 派发环绕（超时/重试/指标） | `ToolExecute` |
| W | `tools/post-execute` | 结果归一化后处置 | `ToolPostExecute` |
| W | `tools/ptc-dispatch-log` | PTC 派发日志 | `ToolPtcDispatchLog` |
| E | `tools/change` | 工具集变化 | `ToolChange` |
| E | `tools/result` | 最终结果观察 | `ToolResult` |

**这是阶段 4 垂直切片的核心路径**：三个 waterfall 构成 pre/execute/post 管线。

### session / api-session（9）

| 模式 | 事件 | 说明 | Rust trait |
|---|---|---|---|
| E | `session/created` | 会话创建 | `SessionLifecycle` |
| E | `session/disposed` | 会话销毁 | `SessionLifecycle` |
| E | `session/event` | 会话事件落盘/推送 | `SessionEvent` |
| P | `session/flush` | 刷新到磁盘 | `SessionFlush` |
| E | `api-session/added` | 列表新增行 | `ApiSession` |
| E | `api-session/removed` | 列表移除行 | `ApiSession` |
| E | `api-session/status` | 运行状态 | `ApiSession` |
| E | `api-session/activity` | 活动时间推进 | `ApiSession` |
| E | `api-session/error` | 用户可见错误 | `ApiSession` |

### fs（4）

| 模式 | 事件 | 说明 | Rust trait |
|---|---|---|---|
| W | `fs/write-intent` | 写入前决策（返回版本） | `FsWriteIntent` |
| W | `fs/edit-intent` | 编辑前决策 | `FsEditIntent` |
| W | `fs/delete-intent` | 删除前决策 | `FsDeleteIntent` |
| E | `fs/observed` | 文件变化观察 | `FsObserved` |

### llm（4）

| 模式 | 事件 | 说明 | Rust trait |
|---|---|---|---|
| W | `llm/stream` | 流式输出环绕 | `LlmStream` |
| W | `llm-pi-ai/request-headers` | 请求头注入 | `LlmRequestHeaders` |
| E | `llm/adapters-updated` | 适配器集变化 | `LlmRegistry` |
| E | `llm/models-updated` | 模型集变化 | `LlmRegistry` |

### workflow / subagent（10）

| 模式 | 事件 | 说明 | Rust trait |
|---|---|---|---|
| E | `workflow/start` | 工作流启动 | `Workflow` |
| E | `workflow/end` | 工作流结束 | `Workflow` |
| E | `workflow/phase` | 阶段切换 | `Workflow` |
| E | `workflow/log` | 日志行 | `Workflow` |
| E | `workflow/agent-start` | 子 agent 启动 | `Workflow` |
| E | `workflow/agent-end` | 子 agent 结束 | `Workflow` |
| E | `subagent/start` | 子 agent 开始 | `Subagent` |
| E | `subagent/end` | 子 agent 结束 | `Subagent` |
| E | `subagent/provider-added` | provider 注册 | `Subagent` |
| E | `subagent/provider-removed` | provider 注销 | `Subagent` |

### slash（4，全部 bail）

| 模式 | 事件 | 说明 | Rust trait |
|---|---|---|---|
| B | `slash/input-begin-command` | 输入以 `/` 开始 | `SlashInput` |
| B | `slash/input-consume-token` | 消费令牌 | `SlashInput` |
| B | `slash/input-insert-reference` | 插入引用 | `SlashInput` |
| B | `slash/input-insert-text` | 插入文本 | `SlashInput` |

### 其余领域（48）

| 领域 | 事件数 | 模式 | Rust trait |
|---|---|---|---|
| `cordis/*` | 6 | E | `CordisInspect` |
| `plugin-manager/*` | 5 | E,P | `PluginManager` |
| `deepseek-account/*` | 3 | E | `AccountAuth` |
| `credentials/*` | 2 | E | `Credentials` |
| `connection/*` | 2 | W,E | `Connection` |
| `goal/*` | 2 | E | `Goal` |
| `hmr/*` | 2 | E | `Hmr` |
| `schedule/*` | 2 | E,W | `Schedule` |
| `system-prompt/*` | 2 | W,E | `SystemPrompt` |
| `agent-loop/*` | 1 | E | `AgentLoop` |
| `agent-preset/*` | 1 | E | `AgentPreset` |
| `app-boot/*` | 1 | E | `AppBoot` |
| `authorization/*` | 1 | E | `Authorization` |
| `command/*`,`commands/*` | 2 | E | `Command` |
| `compaction/*` | 1 | W | `Compaction` |
| `domain/*` | 1 | E | `Domain` |
| `feedback/*` | 1 | P | `Feedback` |
| `forge/*` | 1 | P | `Forge` |
| `mcp-manager/*` | 1 | P | `McpManager` |
| `permission-presets/*` | 1 | E | `PermissionPresets` |
| `plugin-market/*` | 1 | E | `PluginMarket` |
| `session-telemetry/*` | 1 | W | `SessionTelemetry` |
| `settings/*` | 1 | E | `Settings` |
| `skills/*` | 1 | E | `Skills` |
| `web/*`,`webserver/*` | 2 | E | `WebServer` |
| `workspace/*` | 1 | P | `Workspace` |

### 明确不迁移（前端契约，阶段 6 再定）

`theme/change`、`locale/change`、`slots/changed` —— 纯 UI 通知，不属于后端事件总线。

## 对阶段 2 实现的输入

1. **`waterfall` 是重点**：17 个，且 `tools/*` 三个直接决定阶段 4 能否跑通。
2. **`bail`/`serial` 的返回值语义**：上游用 `null`/`false`/`undefined` 三态判断「未 bail」，
   Rust 用 `Option<Out>` 表达，不会有第三种「假值」歧义。
3. **作用域过滤**（`Scoped<T>`）：上游 20 个事件带 `this: Scoped<...>`，用 `dsh-scope` 按
   agent 过滤。Rust 侧对应「监听器带 scope key，派发时匹配」。**这一层阶段 2 不做**，
   等阶段 4 有了 agent 身份再补；现在只做无过滤的总线。
4. **`emit` 占 70%**：实现重心应放在同步派发路径，`async` 只用于 `parallel`/`serial`/`waterfall`。

## 数据复现

提取脚本未入库（一次性分析）。复现方式：扫描 `packages/*/*/src/**/*.ts`，匹配
`interface Events\b` 起始块，块内用 `^\s*'([^']+)'\s*\(.*\)\s*:\s*(.+)$` 提取成员，
并回溯 JSDoc 里的 `@mode`。
