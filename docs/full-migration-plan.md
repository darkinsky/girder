# DSH 全量 Rust 迁移方案

本文是 [Cordis 迁移方案](cordis-migration.md) 的扩展：Cordis 只是最底层，DSH 还依赖大量 TypeScript 与 Node.js 特性。本文逐条判断这些特性的 Rust 可实现性，并给出整体迁移路径。

数据均来自本仓库实测（`git ls-files` 统计 + 依赖清单解析），关键机制都在 Rust 里编译验证过。

## 一、项目规模：先看清迁移对象

原始统计（含测试、含 experimental）：

| 区域 | TS/TSX 行数 |
|---|---|
| `packages/client`（GUI） | 381,740 |
| `packages/experimental` | 227,765 |
| `apps`（web/desktop/cli） | 142,942 |
| `packages/api` | 68,202 |
| `packages/session` | 52,152 |
| `packages/core` | 49,256 |
| `packages/llm` | 41,929 |
| `packages/boot` + `typert` + `sdk` | 65,975 |
| `packages/subagent/context/util` | 66,617 |
| `fs/shell/subprocess/sandbox` | 62,272 |
| **总计** | **1,499,977** |

**但真正要迁移的远比这个小。** 关键拆分：

- 排除 `client` 和 `experimental` 后的 host 源码（非测试）**只有 248,144 行**。
- non-client/experimental 的**测试代码有 368,188 行**——比源码还多，这是测试驱动开发的特征，迁移时不能简单丢弃，但也不该按 1:1 重写。
- `client` 源码 180,825 行，是 React。

所以**迁移对象不是 150 万行，而是约 25 万行 host 源码**。这个数字才是有意义的工程量基准。按 Rust 相对 TS 通常 0.8–1.2 行数比，目标约 20–30 万行 Rust。

## 二、TS/JS 特性依赖清单与判断

下列用量统计默认覆盖 `packages/*/*/src/`（产品源码）。少数条目注明"全仓"时包含测试与 fixtures。这个区分很重要：测试代码在迁移中是被重写而非被移植的对象。

### 2.1 已确认**可以合理实现**（附实测代码）

#### ① 依赖注入 / 服务注册（Cordis 核心）→ ServiceKey

详见 [Cordis 迁移方案](cordis-migration.md#二l1服务注册表)。已编译验证：类型化 `ServiceKey` + 擦除表 + 强类型访问器。

#### ② 事件总线 5 种 dispatch → trait + 泛型

已验证。关键约束：泛型方法不 object-safe，`Events` 必须是具体 struct。

#### ③ Waterfall 环绕中间件 → 索引游标链

已验证，输出 `waterfall("a") = <a!>` / `waterfall("") = vetoed`。

#### ④ effect 逆序回收 → `FnOnce` disposer + 显式 async dispose

已验证，输出顺序 `write → read → bash`。

#### ⑤ 品牌类型（`dsh-brand` 66 个包依赖）→ newtype

TS 的品牌类型是纯编译期幻影，Rust 的 newtype 是**更强**的版本——它连运行时都是不同类型：

```rust
pub struct SessionId(String);
pub struct ToolCallId(String);
fn takes_session(_: &SessionId) {}
// takes_session(&ToolCallId::new("x"));  // 编译错误
```

实测通过。这条是**比上游更好**，不是妥协。

#### ⑥ zod / schemastery（86 + 170 个包）→ serde + schemars

实测同时做到三件事：
```rust
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentConfig {
    pub provider: String,
    #[schemars(range(min = 1, max = 128))]
    pub max_parallel_tool_calls: u32,
}
```
- 未知字段拒绝：`rejected: unknown field 'unknown', expected one of ...`
- JSON Schema 生成：带 `min`/`max`/`additionalProperties: false`
- 默认值、可选字段、命名转换全部生效

**结论：一套 serde 派生同时取代 zod 和 schemastery**，而且 schema 可以直接喂给前端。

#### ⑦ Session 事件判别联合 → serde 内部标签枚举

先确定了这个结构确实是"按 `type` 判别的扁平联合"：

```rust
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum SessionEvent {
    #[serde(rename = "turn/start")]
    TurnStart { seq: Seq, turn: u32 },
    #[serde(rename = "tool/call")]
    ToolCall { seq: Seq, call_id: String, name: String, arguments: String },
}
```

实测输出 `{"type":"tool/call","seq":1,"callId":"c1",...}`，未知变体正确报错 `unknown variant 'turn/end'`。

**这比 TS 版本强**：TS 里加一个事件变体，漏改某个 switch 会静默走 default；Rust 里 `match` 穷尽性检查会强制所有消费者处理新变体。

#### ⑧ 异步生成器 / `for await` → `Stream`

257 处 `AsyncIterable`/`AsyncGenerator` 类型、87 个 async generator、72 处 `for await`。

Rust 方案：`impl Stream<Item = T>` + `futures::StreamExt`。实测可用。

差异：TS 的 `async function*` 里可以 `yield` 任意位置，Rust 的 `Stream` 要么用 `async-stream` crate 的 `stream!` 宏保持相似写法，要么手工实现 `Stream`。**推荐 `async-stream`**，它用宏还原了 `yield` 语法。

`yield*` 委托（27 处）→ `.chain()` 或 `stream!` 里 `for await`。

#### ⑨ 取消（1885 处 `AbortSignal`）→ `CancellationToken`

这是**用量最大的单项特性**。实测验证了真实中断：

```
items before cancel: 4
tail: ["c2", "CANCELLED"]
child cancelled with parent: true
```

`tokio_util::sync::CancellationToken` 提供：
- `token.cancel()` 主动取消
- `child_token()` 父子传播（对应 `AbortSignal.any`）
- 在 `tokio::select!` 里精确中断慢流

**关键差异**：JS 的 `AbortSignal` 可以作为参数随处传递并被多处置为监听；Rust 的 `CancellationToken` 是 `Clone + Send + Sync` 的句柄，语义更接近但**不会自动随作用域销毁**——需要在 fiber 卸载时显式 `cancel()`。这个显式性反而是好事，避免了 JS 里"忘了解绑监听器"的泄漏。

#### ⑩ `AsyncLocalStorage`（34 处）→ `tokio::task_local!`

实测：
```
same task : agent-42
spawned   : <none>
```

**这是我发现的第二个真坑**：`task_local!` **不会自动传播到 `tokio::spawn` 出去的子任务**，而 Node 的 `AsyncLocalStorage` 会通过 async context 自动传播。

DSH 用它的地方是 `packages/core/agent/src/index.ts` 的 `initiators`——追踪"哪个 Agent 发起了当前操作"。这种"隐式上下文传播"在 Rust 里必须**显式传递**：

```rust
agent.spawn_child(|| { /* 显式传入 initiator */ })
```

这是**语义降级**，不是等价替换。好在这类用法只有 34 处，且集中在 agent 初始化路径，可以逐个改成显式参数。**这一条必须写进方案，否则会低估工作量。**

#### ⑪ CommonJS 懒加载（`dsh-lazy-require`）→ `OnceCell`

7 个包依赖。`tokio::sync::OnceCell` 或 `std::sync::OnceLock` 直接对应。

#### ⑫ 插件按名加载（`import(specifier)` + loader）→ 链接期清单

这是**我认为最难但可以解决的一项**。TS 里插件是运行时按名字 `import` 的模块；Rust 没有运行时模块导入。

实测方案（`inventory` crate）：
```
llm-deepseek     enabled=false
tool-bash        enabled=true
  -> bash tool
```

用链接期注册表把所有插件编译进二进制，配置里按名字启用。**代价**：失去"运行时安装新插件"能力——用户不能 `dsh plugin install` 一个新 Rust 二进制之外的插件。

这直接决定了 **bundle/profile 体系要不要保留**。我的判断见第六节。

### 2.2 **必须放弃或降级**的

| 特性 | 用量 | 判断 | 替代 |
|---|---|---|---|
| `Proxy` + `ctx.xxx` 动态属性 | 14 处 `new Proxy`，撑起 153 个服务键 | **放弃** | 显式 `ctx.xxx()` |
| `declare module` 类型合并 | 全仓 386 处；src 内 179 处指向 cordis | **放弃** | 每事件一个 trait |
| 接口合并 | 3242 处 `export interface`（多为普通声明，非合并） | 少量需处理 | trait + impl |
| `!!js` YAML 表达式 | base bundle 里就有 | **放弃** | 固定插值 + 加载期校验 |
| 运行时插件安装 | loader + 7 包 | **降级** | 编译期清单 |
| HMR 热替换 | boot/hmr | **第一版不做** | 改配置重启 |
| TS 装饰器（`@Remote`） | 109 处 | **放弃语法** | 宏 `#[remote]` 或手工注册 |
| `using` / `Symbol.dispose` | 58 处 | 语法放弃 | 显式 `drop` 或 `scopeguard` |
| `structuredClone` | 52 处 | 可行但要注意 | `serde` 往返 或 `Clone` |
| `WeakMap`/`WeakSet` | 109 处 | 可行 | `Weak<T>`（仅 `Arc`）+ 重新设计 |
| 动态 `import()` | 36 处 | 部分可行 | 链接期清单 + 少量 `dlopen` |

#### 关于 `structuredClone` 的坑

52 处用于"深拷贝配置/行数据"。Rust 里 `Clone` 是浅拷贝语义，要深拷贝必须每个类型手写或靠 `serde_json` 往返。**但注意**：TS 版 `structuredClone` 会**丢弃函数**（`app-boot` 的注释明确说了"Patches are copied with `structuredClone`, so they hold no functions"）。Rust 里本来就不能在数据里放函数，所以这个约束天然满足——**比上游更简单**。

#### 关于 `WeakMap` 的坑

109 处。Rust 的 `Weak<T>` 只对 `Arc` 有效，而且需要显式 `upgrade()`。TS 的 `WeakMap<Object, T>` 是"按对象身份关联任意数据"，Rust 里没有直接对应物。需要**逐个重新设计**——好在这些用法大多在缓存和观察者追踪里，Rust 的架构通常不需要它们。

### 2.3 **完全不需要迁移**的

| 项 | 原因 |
|---|---|
| React / TSX（832 个文件，685 在 client） | GUI 保留在 Web 侧，见第五节 |
| Vite 构建 | 前端工具链不变 |
| Electron | 桌面壳不变 |
| Node.js Worker（34 处 `worker_threads`） | 用 `tokio::spawn` + 线程池替代 |
| `AsyncLocalStorage` 的部分用法 | 见 2.1⑩，改成显式传递 |

## 三、Node.js 平台 API 对应表

| Node API | 用量 | Rust 对应 | 备注 |
|---|---|---|---|
| `node:fs` (含 promises) | 286 | `tokio::fs` / `std::fs` | 同步 API 在 Rust 里更快，优先 `std::fs` |
| `node:path` | 188 | `std::path` | 语义差异：Windows 分隔符要测 |
| `node:child_process` | 31 | `tokio::process::Command` | 需注意僵尸进程回收 |
| `node:crypto` | 101 | `sha2`/`ring`/`aws-lc-rs` | 逐算法对应 |
| `node:os` | 48 | `sysinfo` / `std::env` | |
| `node:http` | 52 | `axum` / `hyper` | Web 服务端 |
| `node:stream` | 43 | `tokio::io` + `futures::Stream` | |
| `node:net` | 19 | `tokio::net` | |
| `node:zlib` | 20 | `flate2` / `zstd` | session 日志是 zstd 压缩的 |
| `node:worker_threads` | 34 | `tokio::spawn` / `rayon` | |
| `async_hooks` | 34 | **无直接对应** | 见 2.1⑩ |

`node:zlib` 这条要特别注意：session 用 `session.vN.jsonl.zstd`。Rust 的 `zstd` crate 是官方绑定的，兼容性没问题。

## 四、客户端的迁移问题（最大的决策点）

`packages/client` 38 万行、685 个 tsx、91 个包有 client face、79 个包 devDep 依赖 React 18。

三个选项：

| 选项 | 做法 | 代价 | 评价 |
|---|---|---|---|
| A. 保留 Web GUI | Rust 做服务端，前端不动 | 需要稳定 RPC 协议 | **推荐** |
| B. Rust 重写 GUI | egui / Dioxus / Leptos | 38 万行重做，且 Rust GUI 生态不成熟 | 不建议 |
| C. Tauri 混合 | Rust 后端 + 现有 React 前端 | 需要 Node 运行时或改 RPC | 中期可选 |

**推荐 A**。理由：

1. DSH 的前端已经是"服务端 + RPC"架构（`packages/api/gateway` 是两侧 Typert RPC endpoint，`packages/client/connection` 是浏览器侧 wire 层）。**这个分层本来就是 Rust 后端能直接复用的**。
2. Rust 重写 GUI 意味着同时迁移 38 万行前端 + 25 万行后端，风险叠加。
3. 保持前端不动，迁移期间可以**逐 seam 替换后端**，前端无感知——这是最有价值的降险手段。

代价：RPC 协议必须保持兼容。Typert 是 TS 类型驱动的，Rust 侧要手工维护对应的 JSON Schema / 消息类型。这是**必须接受的重复维护成本**，但比重写整个 GUI 便宜得多。

## 五、整体迁移路径

### 阶段划分（按"每阶段有可运行验收物"）

| 阶段 | 内容 | 验收 | 估算 |
|---|---|---|---|
| 0 | `girder-core`：ServiceKey / Registry / effect | greeter 示例 | 已完成 |
| 1 | Fiber 状态机 + 依赖等待 + 拆除顺序 | 单测覆盖 provider-waits-dependent | 2-3 周 |
| 2 | Events 五种模式 + 事件 trait 体系 | tools/pre-execute 真实签名 | 3-4 周 |
| 3 | 配置层：serde + schemars + YAML 加载 | 用真实 cordis.patch.yml 解析 | 2 周 |
| 4 | **垂直切片**：llm + session + tools + loop | 端到端一次对话 | 8-12 周 |
| 5 | 执行层：fs / shell / subprocess / sandbox | 跑一次 bash 工具调用 | 6-8 周 |
| 6 | API 层 + RPC，接现有前端 | Web GUI 连上 Rust 后端 | 6-8 周 |
| 7 | 补齐剩余 seam（subagent/team/goal/schedule…） | 按需 | 持续 |

**阶段 4 是判断点**：8-12 周后如果垂直切片跑通，说明 Cordis 的 Rust 化成立，可以放心推进；如果跑不通，损失可控。

### 与 [Cordis 方案](cordis-migration.md) 的关系

Cordis 方案覆盖阶段 0-2，本文扩展到 3-7。两份文档不重复：Cordis 方案讲框架内部机制，本文讲框架之外的迁移面。

## 六、必须现在决定的三件事

这三个决定会贯穿整个迁移，后期改代价极高。

### 决定一：bundle/profile 体系保留还是放弃？

上游的 profile/bundle 是"运行时按 YAML 组合插件树"，依赖动态 `import`。

- **保留**：需要把插件做成动态库（`abi_stable` / `stabby`）或 WASM（`wasmtime`），复杂度陡增，且 Rust ABI 稳定性是长期痛点。
- **放弃**：编译期清单 + 配置文件按名启用。用户只能启用编译进来的插件，不能装第三方插件。

**建议放弃运行时安装，保留配置组合。** 即：配置文件仍决定"启用哪些插件、什么配置"，但可用插件集在编译期固定。这保留了 90% 的灵活性（改配置就能换 provider、换沙箱），放弃了插件市场。

`inventory` 实测可行，这是支撑该决定的依据。

### 决定二：`ctx.xxx` 全部改显式，还是做一个兼容层？

**153 个不同的 `Context` 服务键**，由 182 个源文件的 `declare module '@deepseek-ai/cordis'` 贡献（src 内共 179 处此类声明）。全仓库 `ctx.<name>` 属性读取共 149 种。

- **全显式**：`ctx.tools()`，所有调用点要改。
- **兼容层**：`ctx.get::<Tools>()`，保留一点动态性但失去类型安全。

**建议全显式。** 这是 Rust 迁移最大的价值来源——153 个服务键全部获得编译期检查。用 `ctx.get::<Tools>()` 等于把 TS 的弱点搬过来。

### 决定三：GUI 保留还是重写？

见第四节。**建议保留 Web GUI，Rust 只做后端。**

## 七、风险清单（按"会不会让方案失效"排序）

1. **`AsyncLocalStorage` 的语义差距**（34 处）。`task_local!` 不传播到 spawned task，实测确认。如果某个用法深度依赖隐式传播，需要重新设计调用链。**风险最高的单项**，因为它影响 agent 初始化的核心路径。

2. **`Send` 传染性**。`BoxFut` 要求 `Send`，迫使所有服务 `Send + Sync`。DSH 里持有非 Send 句柄的服务（浏览器、PTY、Electron 绑定）需要 `LocalSet` 或独立线程 + channel。**阶段 4 就会暴露**。

3. **动态插件加载的丧失**。决定一没定好，会在阶段 6 才发现 profile 体系要重做。

4. **153 个服务键的手工迁移量**。不是技术风险，是工作量风险。每个都要判断"该不该保留、该叫什么名字"。

5. **测试代码 36 万行**。比源码还多。不能简单丢弃（它们编码了行为契约），也不该 1:1 重写。建议：迁移一个 seam 时，只看该 seam 的测试，逐条判断"这条行为在 Rust 里还需不需要"。

6. **`WeakMap` 的 109 处**。Rust 没有直接对应物，需要逐个重新设计。好在多数是缓存场景，Rust 架构通常不需要。

## 八、投入产出判断

诚实地说：**这是一项 12-18 个月、2-4 人全职的工程。**

值得做的理由：
- 25 万行 host 代码的启动时间和内存占用会显著下降
- 153 个服务键获得编译期检查，这是上游最大的结构性弱点
- 单二进制分发，无 Node 运行时依赖
- 取消语义（1885 处 `AbortSignal`）在 Rust 里更可靠

不值得做的理由：
- 上游是 developer preview，**API 每周都在变**。追着一个移动目标重写，迁移完可能已经过时
- Typert（3 万行）是 TS 编译器驱动的类型投影，Rust 侧要重新设计整个类型安全 RPC 的生成链路，这部分**没有现成方案**
- Rust GUI 生态不成熟，等于永久放弃桌面端的原生体验演进

**我的建议**：如果目标是"验证 Rust 化的可行性"，做到阶段 4（垂直切片）就该停下评估。如果目标是"完整替代"，先解决第八节第一条——**上游的变动率**才是这个项目最大的不确定性，不是技术可行性。

## 九、附：本次实测验证清单

所有结论都经本机编译运行确认，非推断：

| 验证项 | 结果 |
|---|---|
| ServiceKey 类型化 + 擦除表 | ✅ 编译通过 |
| Waterfall 索引游标链 | ✅ `<a!>` / `vetoed` |
| effect 逆序回收 | ✅ write→read→bash |
| 泛型方法 object-safe | ❌ E0038，验证了必须用具体 struct |
| `BoxFut` 生命周期 | ❌ E0582，验证了必须消除生命周期 |
| Stream + 取消真实中断 | ✅ 4 项后 CANCELLED |
| `task_local!` 跨 spawn | ❌ 不传播，验证了语义降级 |
| newtype 品牌类型 | ✅ 编译期拒绝 |
| serde + schemars 替代 zod | ✅ schema + 拒绝未知字段 |
| Session 判别联合 | ✅ 正确序列化 + 拒绝未知变体 |
| `inventory` 链接期插件清单 | ✅ 按名启用 |
