# Girder：DSH 的 Rust 迁移文档

本文是唯一的迁移文档，取代此前分散的 Cordis 方案与全量方案。它包含三部分：

1. **判断**：Rust 能实现什么、不能实现什么，以及为什么
2. **计划**：分阶段、可验收的迁移路线
3. **执行**：每一步具体做什么

所有技术结论都在本机编译运行验证过，验证清单见附录。

---

# 第一部分：迁移判断

## 1.1 迁移对象到底有多大

用 `git ls-files` 逐文件统计（行数含注释与空行）：

| 区域 | 源码 | 测试 | 合计 | 文件数 |
|---|---|---|---|---|
| host（`packages/` 除 client/experimental） | 271,739 | 370,305 | 642,044 | 2,453 |
| client（Web GUI） | 180,825 | 200,915 | 381,740 | 2,021 |
| experimental | 119,562 | 108,203 | 227,765 | 1,232 |
| apps | 113,202 | 29,740 | 142,942 | 738 |
| **合计** | **685,328** | **709,163** | **1,394,491** | **6,444** |

**关键结论：真正的迁移对象是 host 的 271,739 行源码。** 其余：

- **client 的 18 万行源码保留不迁移**（见 2.3 决策三）
- experimental 是"不作产品承诺"的原型，排在最后，部分直接放弃
- 测试 70 万行是重写对象，不是移植对象

## 1.2 三层技术判断

把 DSH 依赖的技术分成三层，逐层判断。

### 第一层：可以一比一迁移

这些是 TS 的"表达方式"，Rust 有更强或等价的对应物。

| TS 特性 | 用量（src） | Rust 方案 | 验证 |
|---|---|---|---|
| 品牌类型（`dsh-brand`） | 65 处 `brandString` | newtype | ✅ 编译期拒绝混用，比 TS 幻影类型更强 |
| zod + schemastery | 86 + 170 个包 | `serde` + `schemars` | ✅ 一套派生取代两者，且能生成 JSON Schema |
| Session 判别联合 | 60+ 事件变体 | `#[serde(tag = "type")]` 枚举 | ✅ 未知变体报错；`match` 穷尽性强制处理新变体 |
| 异步生成器 | 87 个 `async function*`、72 处 `for await` | `Stream` + `async-stream` 宏 | ✅ 可用 |
| 取消 | **1885 处** `AbortSignal` | `CancellationToken` | ✅ 实测 4 项后真实中断，父子令牌自动传播 |
| 依赖注入 / 服务注册 | 153 个服务键 | `ServiceKey` trait + 擦除表 | ✅ 编译通过 |
| 事件总线 5 种模式 | 285 处 `ctx.on` | trait + 泛型 | ✅ waterfall 短路语义正确 |
| effect 逆序回收 | 485 处 `ctx.effect` | `FnOnce` disposer + 显式 async dispose | ✅ write→read→bash |
| 按名加载插件 | loader + 7 包 | `inventory` 链接期清单 | ✅ 按名启用成功 |
| CommonJS 懒加载 | 7 包 | `OnceLock` / `OnceCell` | 直接对应 |

### 第二层：必须降级

这两项 Rust 无法等价表达，是整个迁移的主要风险。

#### ① `AsyncLocalStorage`（34 处）——隐式上下文不传播

实测：

```
same task : agent-42
spawned   : <none>
```

`tokio::task_local!` **不会传播到 `tokio::spawn` 出去的子任务**，而 Node 的 `AsyncLocalStorage` 会通过 async context 自动传播。

DSH 用它追踪"哪个 Agent 发起了当前操作"（`packages/core/agent/src/index.ts` 的 `initiators`、`initiatorRuns`）。**必须改成显式传参**：`agent.spawn_child(initiator, || ...)`。

这是语义降级，不是等价替换。好在只有 34 处且集中在 agent 初始化路径。

#### ② 运行时插件安装——直接失去

Rust 没有运行时模块导入。`inventory` 能把插件编译进来、按配置启用，但用户**不能安装第三方插件**。

这决定了 profile/bundle 体系必须重做（见 2.3 决策一）。

### 第三层：必须放弃

| TS 机制 | 用量 | 原因 | 替代 |
|---|---|---|---|
| `ctx.xxx` 动态属性 | 撑起 153 个服务键 | Rust 无 Proxy，只能 downcast，摧毁类型检查 | 显式 `ctx.tools()` |
| `declare module` 类型合并 | src 内 179 处 | Rust 无全局声明合并 | 每事件一个 trait impl |
| `!!js` YAML 表达式 | bundle 内广泛使用 | 无对应物，且是安全风险 | 固定插值 + 加载期校验 |
| TS 装饰器 `@Remote` | 109 处 | 无装饰器语法 | 属性宏或手工注册 |
| `using` / `Symbol.dispose` | 58 处 | 无语言级支持 | 显式 `drop` 或 `scopeguard` |
| HMR 热替换 | boot/hmr | 依赖完全干净的拆除 | 第一版不做 |
| `WeakMap` / `WeakSet` | 109 处 | `Weak<T>` 只对 `Arc` 有效 | 逐个重新设计（多为缓存，Rust 架构通常不需要） |

`structuredClone`（52 处）是个例外：TS 版会丢弃函数（`app-boot` 注释明确说了这一点），而 Rust 数据里本来就不能放函数，**这个约束天然满足**。

## 1.3 三条硬约束（实测发现）

这些不是判断，是编译器和运行时的既定事实，方案必须绕开。

**约束一：泛型方法不 object-safe**（E0038）

```rust
// 编译失败：`Events` 不能做成 `dyn Events` 服务
trait Events {
    fn emit<E: EmitEvent>(&self, args: E::Args);
}
```

→ 事件总线必须是具体 struct，通过 `Arc<Events>` 共享。

**约束二：`BoxFut` 带生命周期会触发 E0582**

```rust
// 编译失败
type Base<T> = dyn for<'a> Fn(T) -> BoxFut<'a, T> + Send + Sync;
```

→ `Chain` 必须完全自有（`Arc<[...]>` + `usize`），`BoxFut<T>` 保持单参数。

**约束三：`Send` 传染**

`BoxFut` 要求 `Send`，迫使所有服务 `Send + Sync`。DSH 里持有非 Send 句柄的服务（浏览器、PTY、Electron）需要 `LocalSet` 或独立线程 + channel。

## 1.4 平台 API 对应表

按源码用量降序：

| Node API | 用量 | Rust 方案 | 注意 |
|---|---|---|---|
| `node:fs` | 195 | `std::fs` / `tokio::fs` | 同步更快，优先 `std::fs` |
| `node:path` | 188 | `std::path` | Windows 分隔符要测 |
| `node:crypto` | 101 | `sha2` / `ring` / `aws-lc-rs` | 逐算法对应 |
| `node:http` | 52 | `axum` / `hyper` | Web 服务端 |
| `node:os` | 48 | `sysinfo` / `std::env` | |
| `node:stream` | 43 | `tokio::io` + `futures::Stream` | |
| `node:child_process` | 31 | `tokio::process::Command` | 僵尸进程回收 |
| `node:worker_threads` | 30 | `tokio::spawn` / `rayon` | |
| `node:zlib` | 20 | `zstd` crate | session 日志是 zstd 压缩 |
| `node:net` | 19 | `tokio::net` | |
| `async_hooks` | 34 | **无对应** | 见 1.2① |
| YAML | 9 | `serde_norway` | `serde_yaml` 已弃用 |

## 1.5 整体判断

**技术上可行，但是 12-18 个月、2-4 人全职的工程。**

最大的不确定性**不是技术，是上游变动率**：DSH 是 developer preview，README 明确写了 "THERE WILL BE COMPATIBILITY-BREAKING CHANGES"。

因此计划的核心原则是：**先用垂直切片验证可行性，再决定是否全量投入。**

---

# 第二部分：迁移计划

## 2.1 总览

八个阶段，每个阶段有明确验收物。**阶段 4 是决策点**。

| 阶段 | 内容 | 验收物 | 预估 |
|---|---|---|---|
| 0 | 项目骨架 + 核心原语 | 服务注册跑通 | 1 周 |
| 1 | Fiber 生命周期 | 拆除顺序测试通过 | 2-3 周 |
| 2 | 事件总线 | waterfall 短路测试通过 | 3-4 周 |
| 3 | 配置层 | 真实 patch YAML 可解析 | 2 周 |
| **4** | **垂直切片** | **端到端一次对话** | **8-12 周** |
| 5 | 执行层 | bash 工具调用跑通 | 6-8 周 |
| 6 | RPC + 接前端 | Web GUI 连上 Rust 后端 | 6-8 周 |
| 7 | 补齐剩余 seam | 按需 | 持续 |

**阶段 4 之后停下来评估**：切片跑通说明方案成立；跑不通，损失控制在 4 个月内。

## 2.2 阶段 0：项目骨架与核心原语

**目标**：建立 workspace 结构，实现服务注册的最小闭环。

### 工作项

1. **workspace 与 crate 划分**

   ```text
   crates/
     girder-core/      # ServiceKey、Registry、effect、fiber、events
     girder-llm/       # 阶段 4
     girder-session/   # 阶段 4
     girder-tools/     # 阶段 4
     girder-cli/       # 阶段 6
   ```

   建议先只建 `girder-core`，其余在对应阶段建。

2. **ServiceKey + 类型擦除表**

   ```rust
   pub trait ServiceKey {
       type Service: ?Sized + Send + Sync + 'static;
       const ID: &'static str;
   }
   ```

   存储用 `Mutex<HashMap<&'static str, Arc<dyn Any + Send + Sync>>>`，读写都是强类型。

3. **effect 作用域与逆序回收**

   ```rust
   pub type BoxDisposer = Box<dyn FnOnce() -> BoxFut<()> + Send>;
   pub struct Scope { disposers: Mutex<Vec<(&'static str, BoxDisposer)>> }
   ```

   用 `FnOnce` 而非 `Fn`：上游靠 `disposing` 标志位防重复销毁，Rust 用类型直接保证。

4. **CI**：`cargo fmt --check` + `cargo clippy -D warnings` + `cargo test`

### 验收

- greeter 示例：注册一个服务并解析回来
- 拆除顺序单测：注册 A、B、C，验证销毁顺序是 C、B、A
- 重复注册同一 ID 报错，且错误信息含双方插件名

### 风险

无。这一步是纯实现。

## 2.3 阶段 0 结束前必须定的三个决策

这三个决定贯穿全程，后期改代价极高。

### 决策一：bundle/profile 保留还是放弃？

上游用 YAML 在运行时按名组合插件树，依赖动态 `import`。

- **保留**：插件做成动态库（`abi_stable`/`stabby`）或 WASM（`wasmtime`），复杂度陡增
- **放弃**：编译期清单 + 配置按名启用

**建议：放弃运行时安装，保留配置组合。** 配置仍决定启用哪些插件、什么配置，但可用插件集编译期固定。保留 90% 灵活性，放弃插件市场。

### 决策二：`ctx.xxx` 全改显式还是做兼容层？

- **全显式**：`ctx.tools()`，所有调用点要改
- **兼容层**：`ctx.get::<Tools>()`，保留动态性但失去类型安全

**建议全显式。** 用兼容层等于把 TS 的弱点搬过来，迁移就没意义了。

### 决策三：GUI 保留还是重写？

见 2.9。

**建议保留 Web GUI，Rust 只做后端。**

## 2.4 阶段 1：Fiber 生命周期

**目标**：实现"依赖满足才加载、依赖变化重载、卸载等 dependent 先撤"。

### 为什么这一步最难

上游 `fiber.ts` 820 行，处理的是异步状态机的重入安全。核心难点：

```ts
// 卸载前自旋等待所有 dependent 清空
while ([...this.provisions].some(impl => impl.dependents.size)) {
  await new Promise<void>((resolve) => this._release = resolve);
}
```

### 工作项

1. **状态机**：`Pending / Loading / Active / Failed / Unloading / Disposed`，可直接搬

2. **依赖等待**：**不要照搬自旋**。用 `tokio::sync::watch` 或显式依赖计数 + `Notify`，在 dependent 清零时精确唤醒。自旋在 tokio 下会占用 executor slot

3. **显式异步拆除**：**不要用 `Drop` 承担异步清理**。用 `async fn dispose(&self)` 由 registry 统一调用；`Drop` 只做兜底（检测到未拆除时 `error!`）

4. **依赖图**：记录 provider → dependents，用于拆除排序

### 验收

- provider 卸载时，先等所有 dependent 卸载完再释放
- 依赖消失后 fiber 回到 Pending，依赖恢复后重新加载
- 重入：加载过程中触发卸载，不 panic、不泄漏

### 风险

`Send` 传染在这里第一次暴露。如果某个 seam 本质单线程，需要提前设计隔离方案。

## 2.5 阶段 2：事件总线

**目标**：实现 5 种 dispatch 模式 + 事件类型体系。

### 工作项

1. **事件类型用 trait 承载**

   ```rust
   pub trait WaterfallEvent: Send + Sync + 'static {
       const NAME: &'static str;
       type In: Send + 'static;
       type Out: Send + 'static;
   }
   ```

   事件名和参数类型绑死在一处，写错是编译错误。对应上游 179 处 `declare module`，**但要先做事件清单**（见下）。

2. **Waterfall 用索引游标链**

   ```rust
   pub struct Chain<T: Send + 'static> {
       layers: Arc<[Arc<dyn Waterfall<T>>]>,
       index: usize,
       terminal: Arc<dyn Terminal<T>>,
   }
   ```

   三个决策：`Arc<[...]>` 避免克隆列表；索引游标消除生命周期（绕开 E0582）；`terminal` 单独抽象，避免空列表特殊分支。

3. **其余四种模式**：`emit` 同步不等、`parallel` 并发等、`serial` 顺序等遇 bail 停、`bail` 同步遇 bail 停

   `serial`/`bail` 返回值用 `Option<E::Out>` 而非上游的 `null`/`false`/`undefined` 三态判断。

4. **按 fiber 过滤与自动回收**：监听器注册即 effect，随 fiber 卸载自动移除

### 关键前置工作：事件清单

**不要盲目迁移 179 处 `declare module`。** 很多是对同一事件的不同视角。先做清单：

- 遍历 `packages/*/*/src/`，提取所有事件名
- 按领域分组（session / agent / tools / llm / fs / …）
- 标注每个事件的 dispatch 模式
- 判断哪些是真正不同的契约、哪些可以合并

预期结果是 **40-60 个事件 trait**，而非 179 个。

### 验收

- waterfall 短路：第一层不调用 `next` 时后续不执行
- waterfall 委托：每层都调用时按序执行到底
- serial/bail 遇第一个 bail 值停止
- 监听器随 fiber 卸载自动移除

## 2.6 阶段 3：配置层

**目标**：用真实的 `cordis.patch.yml` 验证配置能解析。

### 工作项

1. **YAML 加载**：用 `serde_norway`（`serde_yaml` 已弃用）

2. **配置类型**：`#[derive(Deserialize, JsonSchema)]` + `#[serde(deny_unknown_fields)]`

3. **`!!js` 表达式替代**：上游用它做"配置里嵌 JS 表达式"，如 `disabled: !!js "!ctx.get('profileContext')"`。改为固定插值语法（`${env.HOME}` 之类），**加载期报错而非运行期**

4. **patch 层叠**：profiles → bundles → 用户 patch → `--patch` 覆盖，按 row id 替换

### 验收

- 能解析 `packages/bundle/base/cordis.patch.yml`
- 层叠顺序正确：后面的 patch 覆盖前面的同 id 行
- 未知字段报错，错误信息指出行号和字段名

## 2.7 阶段 4：垂直切片（决策点）

**目标**：`llm + session + tools + loop` 端到端跑通一次真实对话。

### 为什么选这条切片

它覆盖了所有关键 seam，且是最短的可验证路径：

| 组件 | 验证什么 |
|---|---|
| `llm` + 一个 provider | 流式 + 取消 + `Send` 约束 |
| `session` | 判别联合 + JSONL + zstd |
| `tools` | waterfall 管线（pre/execute/post） |
| `loop` | Fiber 生命周期 + 事件驱动 |

### 工作项

1. **llm seam**：`ServiceKey` 定义 + DeepSeek provider 适配 + `Stream` 流式 + `CancellationToken` 取消

2. **session**：`SessionEvent` 枚举（先做最小变体集：`turn/start`、`user/message`、`assistant/message`、`tool/call`、`tool/result`、`turn/end`）+ JSONL 读写 + zstd 压缩

3. **tools**：注册表 + 三个 waterfall（pre-execute / execute / post-execute）+ 至少一个工具（bash 或 read）

4. **loop**：assembling → streaming → tool dispatch → repeat

5. **CLI**：能发一条消息并打印流式输出

### 验收

```bash
$ girder "列出当前目录的文件"
# 流式输出模型回复 → 调用 bash 工具 → 返回结果 → 继续输出
```

- 中途 Ctrl-C 能干净取消（验证取消语义）
- session 文件写盘且能被重新读回

### 决策点

**这里停下来评估三件事：**

1. `Send` 约束是否可接受？有没有 seam 必须单线程
2. `BoxFut` 的 clone 开销是否可接受
3. 上游 API 变动是否已经让部分设计过时

**通过才继续，不通过就止损。**

## 2.8 阶段 5：执行层

**目标**：bash / 文件 / 沙箱能力完整可用。

| 组件 | 内容 | 要点 |
|---|---|---|
| `fs` | 文件读写、watcher | `std::fs` 优先，注意编码 |
| `shell` | bash/zsh 执行 | 进程组、信号、超时 |
| `subprocess` | 子进程管理 | **僵尸进程回收**、stdout/stderr 流式 |
| `sandbox` | 进程隔离 | Linux: seccomp/landlock；macOS: seatbelt |

### 验收

- bash 工具能执行带管道的命令
- 超时能杀掉整个进程组
- 沙箱拒绝越权写操作
- 无僵尸进程残留

## 2.9 阶段 6：RPC 与前端对接

**目标**：现有 Web GUI 连上 Rust 后端。

### 关键判断：前端保留

`packages/client` 18 万行源码，685 个 tsx，60 多个 React UI 包。**不重写。**

理由：
1. DSH 本来就是"服务端 + RPC"架构（`api/gateway` 是两侧 Typert RPC endpoint），这个分层天然可复用
2. 保持前端不动，迁移期间可以逐 seam 替换后端，前端无感知——**这是最有价值的降险手段**
3. Rust GUI 生态（egui/Dioxus/Iced）不成熟，重写等于永久放弃桌面端演进

### 工作项

1. **RPC 协议对齐**：Typert 是 TS 类型驱动的，Rust 侧要手工维护对应消息类型。用 `schemars` 生成 JSON Schema，与前端契约对齐

2. **HTTP/WS 服务**：`axum` + `tokio-tungstenite`

3. **会话流**：`session/event` 的推送与重连

### 验收

- Web GUI 能列出 session、发消息、看流式回复
- 断线重连后状态一致

### 已知成本

Typert（3 万行）是 TS 编译器驱动的类型投影，Rust 侧要**重新设计整个类型安全 RPC 生成链路**。这部分**没有现成方案**，是阶段 6 的最大风险。

## 2.10 阶段 7：补齐剩余 seam

主路径稳定后，按需迁移：

| 优先级 | 组件 | 说明 |
|---|---|---|
| 高 | `subagent` | 子 agent 委托 |
| 高 | `fs`/`sandbox` 完整策略 | 安全相关 |
| 中 | `context`/`compaction` | 上下文压缩 |
| 中 | `mcp` | 外部工具 |
| 中 | `skill` | 技能加载 |
| 低 | `team`/`goal`/`schedule` | 协作与定时 |
| 最后 | experimental | 逐个评估，部分直接放弃 |

**experimental 里建议直接放弃的**：

- `webworker-runtime`（23.8k）：把 harness 跑在浏览器 Worker 里，Rust 下基本失去意义
- `liveshare`（55.9k）：涉及独立部署的服务器，仅在确实需要共享功能时才做

## 2.11 里程碑与人力

| 里程碑 | 累计时间 | 人力 |
|---|---|---|
| M1: 核心框架可用（阶段 0-2） | 2-2.5 个月 | 1-2 人 |
| M2: 垂直切片跑通（阶段 3-4） | 4-5 个月 | 2-3 人 |
| M3: 可替代现有后端（阶段 5-6） | 8-10 个月 | 3-4 人 |
| M4: 功能对齐（阶段 7） | 12-18 个月 | 3-4 人 |

## 2.12 技术栈选型

已核实所有 crate 存在且维护中：

| 领域 | crate | 版本 |
|---|---|---|
| 异步运行时 | `tokio` | 1.54 |
| 取消 | `tokio-util`（`CancellationToken`） | 0.7 |
| 流 | `futures`、`async-stream` | 0.3 |
| 序列化 | `serde`、`serde_json` | 1.x |
| Schema | `schemars` | 1.2 |
| 错误 | `thiserror`（库）、`anyhow`（应用） | 2.0 |
| 插件清单 | `inventory` | 0.3 |
| 日志 | `tracing` | 0.1 |
| 压缩 | `zstd` | 0.14 |
| CLI | `clap` | 4.6 |
| HTTP | `axum` | 0.8 |
| HTTP 客户端 | `reqwest` | 0.13 |
| YAML | `serde_norway` | 0.9 |

---

# 第三部分：执行

## 3.1 第一步做什么

**从阶段 0 开始，只做三件事：**

1. 建 workspace 与 `girder-core` crate
2. 实现 `ServiceKey` + 类型擦除表
3. 实现 `Scope` 与逆序回收

**先不做**：fiber 状态机、事件总线、配置层。这些都要等核心原语稳定。

## 3.2 每个阶段的固定流程

1. **写接口** → 2. **写测试** → 3. **实现** → 4. **验证** → 5. **记录偏差**

第 5 步很重要：每阶段结束记录"实际与计划的差异"，作为后续阶段的输入。

## 3.3 阶段 4 之前必须完成的准备

在写垂直切片代码前，先做**接口草图**：

- 列出 llm / session / tools / loop 四个 seam 的 `ServiceKey` 定义
- 列出这条路径涉及的事件 trait
- 检查有没有写不出来的接口

**这一步产出的草图会决定 `girder-core` 是否需要返工，比继续写框架代码更有价值。**

## 3.4 风险登记册

按"会不会让方案失效"排序：

| # | 风险 | 影响 | 暴露时点 | 缓解 |
|---|---|---|---|---|
| 1 | `AsyncLocalStorage` 语义差距（34 处） | 高 | 阶段 4 | 改显式传参，先评估影响面 |
| 2 | `Send` 传染 | 高 | 阶段 4 | 提前识别单线程 seam，设计隔离 |
| 3 | 上游 API 变动率 | 高 | 全程 | 阶段 4 后评估是否值得继续 |
| 4 | Typert 类型投影无现成方案 | 中高 | 阶段 6 | 手工维护 schema，接受重复成本 |
| 5 | 动态插件加载丧失 | 中 | 阶段 3 | 决策一提前定 |
| 6 | 153 个服务键手工迁移 | 中 | 全程 | 不是技术风险，是工作量 |
| 7 | `WeakMap` 109 处重新设计 | 中 | 阶段 5-7 | 多数是缓存，Rust 架构通常不需要 |
| 8 | 测试重写量 37 万行 | 中 | 全程 | 迁移一个 seam 只看该 seam 的测试 |

## 3.5 止损条件

明确写下什么情况下停止投入：

1. **阶段 4 跑不通**，且根因是需要大规模重写 `Send` 约束 → 停止
2. **上游 API 变动导致阶段 4 的设计过时超过两次** → 重新评估
3. **阶段 6 发现 Typert 无法在合理成本内重建** → 考虑保留 Node 侧 RPC 网关

---

# 附录：验证清单

所有结论均经本机编译运行确认：

| 验证项 | 结果 |
|---|---|
| ServiceKey 类型化 + 擦除表 | ✅ 编译通过 |
| Waterfall 索引游标链 | ✅ `<a!>` / `vetoed` 短路正确 |
| effect 逆序回收 | ✅ write→read→bash |
| 泛型方法 object-safe | ❌ E0038（确认必须用具体 struct） |
| `BoxFut` 带生命周期 | ❌ E0582（确认必须消除生命周期） |
| Stream + 取消真实中断 | ✅ 4 项后 CANCELLED |
| `task_local!` 跨 `tokio::spawn` | ❌ 不传播（确认语义降级） |
| newtype 品牌类型 | ✅ 编译期拒绝混用 |
| serde + schemars 替代 zod | ✅ schema + 拒绝未知字段 |
| Session 判别联合 | ✅ 正确序列化 + 拒绝未知变体 |
| `inventory` 链接期插件清单 | ✅ 按名启用成功 |
| crate 可用性 | ✅ 13 个 crate 全部存在 |

---

**文档状态**：迁移计划基线。阶段 0 完成后更新 2.2 节的实际情况，阶段 4 结束后更新整体可行性判断。
