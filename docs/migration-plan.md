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

   存储用 `Mutex<BTreeMap<&'static str, Entry>>`，读写都是强类型。

   实现时发现一处必须偏离本节草案：`Arc<dyn Trait>` 无法直接 coerce 成 `Arc<dyn Any>`
   （Rust 不允许跨无关 trait 的 upcast），所以表里存的是 `Arc<ArcHolder<S>>`，`ArcHolder`
   只是 `struct ArcHolder<S: ?Sized>(Arc<S>)`。解析时多一层解包，代价是一次指针间接。

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

### 完成记录（§3.2 第 5 步）

2026-11 完成。实际与计划的差异：

| 项 | 计划 | 实际 | 原因 |
|---|---|---|---|
| 表结构 | `HashMap` | `BTreeMap` | 需要按 id 有序遍历做诊断输出 |
| 类型擦除 | `Arc<dyn Any>` 直接存 | `Arc<ArcHolder<S>>` 包一层 | `Arc<dyn Trait>` 无法 upcast 到 `Arc<dyn Any>` |
| 按值发布 | `provide(service: K::Service)` | 只保留 `register(Arc<K::Service>)` | `K::Service` 是 `dyn Trait`，按值传不进去；`CoerceUnsized` 不稳定 |
| 拆除时的服务可见性 | 未规定 | 先跑 disposer，再清表 | 对齐上游 fiber：插件拆除时依赖图仍完整 |

验收三条全部满足，见 `crates/girder-core/tests/registry.rs`（12 个集成测试 + 3 个文档测试）。
`cargo fmt --check`、`cargo clippy -D warnings`、`cargo test` 均通过。

**留给阶段 1 的已知缺口**：`Scope::dispose` 一次性排空 disposer 列表，因此 disposer 在
拆除过程中新注册的 effect 不会被本轮执行（`an_effect_registered_during_disposal_still_runs`
记录了这个行为）。上游 fiber 会因为插件拆除确实注册新 effect 而需要每轮重查，阶段 1 必须
补上。

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

### 完成记录（§3.2 第 5 步）

完成。三条验收均有测试覆盖，见 `crates/girder-core/tests/fiber.rs`（13 个）与
`tests/fiber_drain.rs`（1 个并发压力测试）。

实际与计划的差异：

| 项 | 计划 | 实际 | 原因 |
|---|---|---|---|
| 依赖等待机制 | `tokio::sync::watch` 或依赖计数 + `Notify` | `Notify` + 先注册兴趣再检查条件 | `watch` 携带值是多余的；只需要"变了"这个信号 |
| 插件体签名 | 未规定 | `Fn() -> BoxFut<Result<Disposer, BodyError>>` | 依赖变化要重跑 callback（对应上游 reload），所以不能是 `FnOnce` |
| 状态观察 | 事件总线 `internal/status` | `Fiber::subscribe()` 返回 channel | 事件总线属于阶段 2，现在开接口会锁死它的形状 |
| 配置处理 | fiber 拥有 config | 完全不做 | 配置层属于阶段 3 |
| 插件体返回值 | 未规定 | `Result<Disposer, BodyError>` | 需要表达"插件启动失败"，否则 `Failed` 状态永远用不上 |

**两处踩到的坑，值得记下来：**

1. `MutexGuard` 跨 `.await`。第一版在 `start` 里持锁调用 `dispose().await`，编译报
   `not Send`。后来 `clippy::await_holding_lock` 又在 `observe_services` 里抓到一处。
   规则记牢：**先定夺、后 await**，判断在锁内，动作在锁外。

2. 依赖消失时不能只改状态。第一版把 `state` 直接置回 `Pending`，结果 `unload` 走
   `!is_active()` 分支提前返回，**插件的 disposer 从未执行——资源泄漏**。正确做法是
   依赖消失时驱动完整拆除流程（`unload_inner`），disposer 跑完再落到 `Pending`。
   这个 bug 是测试 `a_provider_waits_for_every_dependent_before_releasing` 抓出来的。

3. `Failed` 状态一开始是死代码。插件体签名原本返回 `Disposer`，失败无从表达，状态机里
   的 `Failed` 分支永远不会被走到。改成返回 `Result` 后补了两个测试
   （`a_failing_body_reports_failed_and_surfaces_the_error`、
   `a_failed_fiber_can_be_retried`），确认失败可上报、且可以重试。**教训：状态枚举里
   有分支却没有任何路径到达它，说明接口缺了一个返回值。**

**关于 `Notify` 的竞态**：`notify_waiters` 在 `enable()` 之后、`await` 之前发出会丢失。
实现里先 `notified()` 再 `enable()` 再检查条件，`fiber_drain.rs` 用 200 轮 × 16 并发
dependent 反复冲击这个窗口，靠超时而非断言失败来暴露挂起。

**`Send` 传染的第一次实测结论**：阶段 1 没有遇到必须单线程的 seam。`PluginBody` 要求
`Send + Sync`，但插件体内部是异步 future，天然可以跨线程。这个风险要等阶段 4 接
浏览器 / PTY 时才真正暴露。

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

### 完成记录（§3.2 第 5 步）

完成。前置工作「事件清单」见 [event-inventory.md](event-inventory.md)，实测**97 个事件**
（不是计划估的 179 处——那 179 处里多数扩展的是 `Context` 而非 `Events`），
模式分布 emit 68 / waterfall 17 / parallel 6 / bail 4 / serial 2，收敛为 46 个 trait。

测试见 `crates/girder-core/tests/events.rs`（16 个），四条验收全部覆盖。

实际与计划的差异：

| 项 | 计划 | 实际 | 原因 |
|---|---|---|---|
| `Chain` 存储 | `Arc<[Arc<dyn Waterfall<T>>]>` | `Arc<[Layer]>` + `usize` 游标 + 独立 terminal | 与原设计一致；`terminal` 独立成字段消除了空列表分支 |
| `Next` 容器 | 未规定 | `Box<dyn FnOnce>` | `Arc<dyn FnOnce>` **无法调用**——调它要把值移出 `Arc`。这是实测踩到的坑 |
| 事件参数约束 | `Send + 'static` | `Send + Sync + Clone` | `next` 在层的 future 里跨 `await` 持有，必须 `Sync`；每个监听器各拿一份，必须 `Clone` |
| 监听器存储 | `HashMap<String, Vec<...>>` | 每事件一个 `Mutex<Slots<L>>`，整体 `Any` 擦除 | 按事件类型分别存储，派发时只 downcast 一次而非每个监听器一次 |
| 删除方式 | — | `RemoveById` 擦除句柄与存储分离 | 按 id 删除不该需要知道监听器类型 |

**踩到的坑：**

1. `Arc<dyn FnOnce>` 不能调用。第一版把 `Next` 写成 `Arc<dyn FnOnce ...>`，编译器报
   `cannot move out of an Arc`。`FnOnce` 意味着消费自身，而 `Arc` 只给共享借用，两者
   不兼容。改成 `Box` 后正常——续接体本来就只归一层所有。

2. `Sync` 约束是推出来的，不是猜的。先按 `Send` 写，编译报 `A cannot be shared between
   threads safely`；加 `Sync` 后 pass。原因写在对应的 trait 文档里：`next()` 在层的
   future 中被跨 `await` 持有。

3. `EventBus::parallel` 的聚合错误没做完。上游用 `Promise.allSettled` +
   `AggregateError` 收集全部失败；Rust 侧当前用 `join_all` 等全部结束，但**只**在
   监听器 panic 时才向上传播，`DispatchError::Listeners` 从未被构造。这是真实缺口，
   等阶段 4 有真实并行监听器（如 `session/flush`）时补。

**`Send` 传染第二次实测**：`EventBus` 整体要求 `Send + Sync`，所有事件参数与监听器
都被传染。没有遇到必须单线程的事件，但 `Parallel` 和 `Serial` 的 future 都要求 `Send`，
这会在阶段 4 接 LLM 流式响应时成为约束（流必须 `Send`）。

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

### 完成记录（§3.2 第 5 步）

完成。测试见 `crates/girder-core/tests/config.rs`（36 个），样本是**未修改的上游**
`packages/bundle/base/cordis.patch.yml`（548 行）与其转换版。

**第一条验收有一个必须说清的转折**：上游原文件**不能**被解析，因为它含 16 处 `!!js`。
所以「能解析 `base/cordis.patch.yml`」这条验收，实际成立了两次：
一次是**拒绝**原文件（`the_real_bundle_patch_is_rejected_because_it_embeds_javascript`），
一次是**解析**其转换版（`the_converted_bundle_patch_parses`，99 行 insert）。
这两条都必须在，因为拒绝才是本阶段的核心决定——如果只测转换版能过，就等于假装
`!!js` 问题不存在。

### `!!js` 的四种形态与替代

扫描全部 bundle 的 61 处 `!!js`，只有四种形态，无第五种：

| 上游 | 出现次数 | Rust 替代 |
|---|---|---|
| `!ctx.get('profileContext')` | 5 | `when: { not_service: profileContext }` |
| `process.platform === 'win32'` / `!== ...` | 4 | `when: { platform: linux }` / `{ not_platform: windows }` |
| `process.env.X ?? 'default'` | 5 | `"${env.X:-default}"` 插值 |
| `(env.X ?? 'y') === 'z' ? a : b` | 1 | `when: { env: X, equals: z }` + 插值 |

另有 `process.cwd()`、`dshHomePath('x')`、`'${platform}'` 等取值形态，对应 `${cwd}`、
`${home}`、`${platform}`。

### 实际与计划的差异

| 项 | 计划 | 实际 | 原因 |
|---|---|---|---|
| `!!js` 替代 | 固定插值语法 | 插值 + 结构化 `when` 条件 | 只靠插值表达不了布尔条件；`when` 是数据而非表达式，可静态校验 |
| 配置校验 | `serde` + `schemars` | `serde` + `deny_unknown_fields` | `schemars` 用于生成 JSON Schema（阶段 6 RPC 契约），本阶段不需要 |
| patch 层叠 | 「按 row id 替换」 | 替换 + 同名守卫 + 未命中告警 | 上游 `PatchOptions.name` 是安全阀，漏了它会让 patch 打到错误插件上 |
| 未知字段报错 | 指出行号和字段名 | `serde` 报字段名；**行号由 YAML 解析器给出，不在我们控制内** | 见下 |

**一处没有做到计划要求的地方**：「未知字段报错，错误信息指出行号和字段名」。当前
`serde_norway` 的错误信息包含字段名与列号，但**测试只断言了字段名**，没有断言行号。
原因是 serde 的行号信息依赖解析器实现，没有稳定保证。这条验收算部分满足，已记在这里
而不是绕过。

**踩到的坑：**

1. `Entry.config` 既要能装 group 的子条目列表，又要能装任意插件配置。第一版用
   `serde_json::Value` 统装，结果 group 子条目丢失了类型。现在按形状区分：数组元素
   全是带 `id`/`name` 的对象才当条目列表，否则当普通 JSON——和上游的判断方式一致。

2. `when.env` 最初只判断变量存在与否，语义太弱。原表达式是
   `(env ?? 'x') === 'y'`，需要比较值。补了 `equals` 字段后语义才等价。

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

### 完成记录（§3.2 第 5 步）——决策点评估

接口草图见 [phase4-interfaces.md](phase4-interfaces.md)，测试见 `crates/girder-core/tests/slice.rs`
（18 个），验收命令是 `crates/girder-cli`。

**验收状态：通过，但有一处必须说清的限定。**

`girder "列出当前目录的文件"` 跑通了完整路径：

```
$ cargo run -q -p girder-cli -- "list files" --verbose
[tool] list
[tool] done
Here is what I found in the current directory.
{"type":"user/message",...}
{"type":"turn/start","turn":1}
{"type":"assistant/message","content":[{"type":"tool-call",...}]}
{"type":"tool/call",...}
{"type":"tool/result","content":[{"type":"text","text":".github\ndocs\n..."}]}
{"type":"turn/end","turn":1,"reason":"tool-calls"}
{"type":"turn/start","turn":2}
{"type":"assistant/message","content":[{"type":"text","text":"Here is what I found..."}]}
{"type":"turn/end","turn":2,"reason":"stop"}
```

**限定**：provider 是离线的（`OfflineProvider`），不是真实 DeepSeek API。理由写在这个
crate 的模块文档里——真实 provider 需要 API key 与网络，而一个动不了的验收命令没有价值。
所以这次验证的是**机制**（流式、工具分发、会话落盘、turns 循环），不是**模型质量**。
接入真实 HTTP provider 是阶段 5-6 的事。

### 决策点三问的实测答案

**问题一：`Send` 约束是否可接受？有没有 seam 必须单线程**

**可接受，且没有发现必须单线程的 seam。** 但有代价，具体三处：

| 地方 | 约束 | 代价 |
|---|---|---|
| `LlmProvider::stream` 返回的 `Stream` | 必须 `Send` | provider 内部状态必须 `Send`；未来接浏览器/PTY 时会是问题 |
| `PluginBody` | `Send + Sync` | 插件体是 async future，天然满足 |
| `EventBus` 全部监听器 | `Send + Sync` | 事件参数与监听器全被传染 |

阶段 4 本身没遇到障碍，因为 DeepSeek provider 是纯 HTTP。**但这个结论只对阶段 4 成立**：
阶段 5 接 PTY（`portable-pty` 的 handle 非 `Send`）时，计划 §2.8 预判的隔离方案会真正被用到。
不能把这里的「可接受」外推到阶段 5。

**问题二：`BoxFut` 的 clone 开销是否可接受**

**没有出现可测量的开销**，原因是设计上 `BoxFut` 只在 seam 边界出现：

- seam 内部（如 `AssistantMessage::from_chunks` 折 chunk）用具体类型，不走 `BoxFut`
- `ToolBody` 是 `Arc<dyn Fn> -> BoxFut`，每次调用一次 `Arc::clone` + 一次 `Box`
- 事件监听器注册时 `Arc` 一次，派发时 clone `Arc`

工具调用是每次一个 `Box` 分配，量级在「每次工具调用」而非「每个 token」。
**没有实测出性能问题，也没有做基准测试**——计划要求的「是否可接受」目前只有定性判断，
没有数字。标为未完成的验证项。

**问题三：上游 API 变动是否已让部分设计过时**

**没有。** 阶段 0-4 的四个 seam 全部基于我在 `vendor/cordis` 与 `packages/*/src` 读到的实现，
没有依赖任何「可能会变」的接口。真正的变动风险在于：上游 `session` 有 60+ 事件变体，
我只实现了 6 个；如果上游继续加变体，我这边是**补枚举变体**而非重构，成本线性。

### 与计划的差异

| 项 | 计划 | 实际 | 原因 |
|---|---|---|---|
| 循环接口 | `run() -> BoxFut<Result<()>>` | `run() -> impl Stream<Item = LoopStep>` | 草图阶段就发现「一个 future 藏住全部中间态」无法测试也无法流式打印 |
| 事件变体 | 6 个最小集 | 6 个，完全一致 | — |
| `ContentBlock` | 上游 7 个变体 | 5 个（去掉 `tool-addition`/`tool-removal`） | 动态工具增删属于阶段 7，切片不需要 |
| 真实 provider | 计划隐含要求 | 离线 provider + 真实 HTTP 留待阶段 5 | 见上方限定 |

### 踩到的坑

1. **`BlockStart` 会吞掉累积文本。** 第一版在 `block-start` 时往 slot 里放一个空的
   `Text { text: "" }`，结果 `block-end` 没来时，`or_else` 看到 slot 已是 `Some`，
   累积的文本被丢弃，组装出空消息。改成只记录**类型**，内容一律从 delta 累积。
   这个 bug 会让「流式输出」静默变成空字符串，是切片验收里最危险的一类。

2. **`rename_all = "kebab-case"` 生成 `turn-start`，而上游是 `turn/start`。** 会话日志的
   tag 是线上格式，写错等于和所有已有日志不兼容。改成每个变体显式 `#[serde(rename)]`。

3. **clippy 抓出 `run` 有 142 行。** 拆成 `run` + `turn_stream` + `tool_calls` 三个函数后
   都是 100 行以内。这个阈值在这类状态机上是有效的信号。

### 止损判断

计划 §3.5 的三个止损条件：

1. **阶段 4 跑不通** → 跑通了
2. **上游 API 变动导致设计过时两次以上** → 零次
3. **Typert 无法重建** → 未到阶段 6

**结论：不触发止损，可以进入阶段 5。** 但上述「问题二」缺基准数字、「问题一」的结论
不能外推到阶段 5，这两点必须在阶段 5 开始时重新验证。

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

### 完成记录（§3.2 第 5 步）

完成。`crates/girder-core/src/exec.rs`，测试见 `crates/girder-core/tests/exec.rs`（21 个）。

四条验收全部有测试，且都是**真的验证而非空断言**：

| 验收 | 测试 | 验证方式 |
|---|---|---|
| 带管道的命令能跑 | `a_command_with_a_pipe_runs`、`a_multi_stage_pipeline_runs` | `echo hello &#124; tr a-z A-Z` 断言输出 `HELLO` |
| 超时杀掉整个进程组 | `a_timeout_kills_the_whole_process_group` | **断言孙进程已死**，不是只断言 shell 退出 |
| 沙箱拒绝越权写 | `a_declared_write_outside_the_scope_is_refused_before_spawning`、`traversal_out_of_the_scope_is_refused` | 越权路径在 spawn 前被拒；`..` 逃逸被解析后拒绝 |
| 无僵尸进程 | `a_completed_command_leaves_no_zombie`、`a_killed_command_leaves_no_zombie` | 读 `/proc/<pid>/stat` 统计本进程的 `Z` 状态子进程 |

### 验收命令实测

```bash
$ cargo run -q -p girder-cli -- "list files" --tool bash
[tool] bash
[tool] done (call-1)
Here is what I found in the current directory.
```

会话日志显示真实执行了 `ls -1 | head -5`，返回 5 行文件列表。**这是带管道的真实 shell 命令**，
不是模拟。

### 关键实现决定

**1. 进程组必须用 `pre_exec`，代价是 `unsafe`。**

`sh -c "sleep 30 & wait"` 会启动孙进程。只杀 `sh` 的 pid，孙进程会变成孤儿并继续持有管道。
唯一可移植的解法是在 `exec` 前调用 `setpgid(0,0)` 让子进程成为组长，然后 `kill(-pgid, SIG)`。

这需要 `CommandExt::pre_exec`，而它是 `unsafe` 的。工作区的 `unsafe_code = "forbid"`
因此改成 `"deny"`：`forbid` 无法在局部豁免，`deny` 可以，且每处豁免必须带 `reason`。
这是本阶段唯一的 `unsafe`，共两处（`run_shell` 和 `run_bash`）。

**2. 必须先 SIGTERM 再 SIGKILL。**

只发 SIGTERM 时，忽略该信号的 shell 会存活到超时之后。代码里 SIGTERM 后等 50ms 再 SIGKILL。

**3. 管道必须与 wait 并发读取。**

如果先 `wait` 再读 pipe，输出超过 64KB 就会填满缓冲区，子进程阻塞在写，父进程阻塞在 wait——
死锁。测试 `large_output_does_not_deadlock` 用 20,000 行输出专门验证这一点。

### 与计划的差异

| 项 | 计划 | 实际 | 原因 |
|---|---|---|---|
| 沙箱 | seccomp/landlock（Linux）、seatbelt（macOS） | **路径策略 + spawn 前声明检查** | 内核级隔离未实现，见下方缺口 |
| `unsafe_code` lint | `forbid` | `deny` + 两处带 reason 的豁免 | `pre_exec` 是设进程组的唯一办法 |
| 平台 | 未限定 | 仅 POSIX | Windows 进程树需要 `taskkill /T /F`，未实现 |

### 明确未做到的（重要）

**沙箱不是内核级隔离。** 当前的 `PathPolicy` 只检查**声明**的写路径，且检查发生在进程创建**之前**。
一个不声明就直接写的命令**不会被阻止**。这意味着：

> 用 `--tool bash` 跑这个 CLI **不是**对模型的安全隔离。它只在开发环境成立。

计划 §2.8 要求的 seccomp/landlock 没有实现。正确形状是定义一个 `Sandbox` trait，其 `spawn`
在 `exec` 前应用 landlock，`PathPolicy` 作为它的配置。现在只写了配置侧，没有内核侧。

这条缺口必须写在这里而不是绕过，因为「沙箱拒绝越权写」这条验收**字面上通过了**
（拒绝声明越权的写），但**安全语义上没有达到计划意图**。

### `Send` 传染的第二次实测

**结论修正了阶段 4 的判断。** 阶段 4 说「没有发现必须单线程的 seam」，那是因为 llm 是纯 HTTP。
阶段 5 实测：`std::process::Child` 和 `tokio::process::Child` 都是 `Send`，所以**执行层也没有**
引入非 `Send` 的 seam。

真正会出问题的仍是计划预判的 PTY（`portable-pty` 的 `MasterPty` 非 `Send`）与浏览器自动化。
本阶段没有做 PTY，所以那个风险**依然未被验证**。

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

### 完成记录（§3.2 第 5 步）

范围先定了，见 [phase6-scope.md](phase6-scope.md)。实现是 `crates/girder-rpc`，测试 20 个
（9 个协议单测 + 11 个传输集成测试）。

### 查清的事实（计划里没有的）

读 `packages/api/gateway/src` 后确认了三件事：

1. **传输是一条 WebSocket 多路复用端点** `/api/remote.mux`，所有逻辑流共用。
2. **四种下行帧**：`ready` / `emit` / `waterfall` / `cancel`，`{ type }` 判别联合。
3. **`InvokeRemoteRequest.args` 是 `Record<string, unknown>`**——**线路上完全没有类型信息**。
   类型安全全部来自 Typert 在**编译期**生成的描述符。

第 3 点是最关键的发现：它意味着**Rust 可以实现这个协议而不需要复刻 Typert 的生成链路**，
因为线上本来就是无类型 JSON。计划说「要重新设计整个类型安全 RPC 生成链路」，更准确的说法是
「要实现协议很容易，要拿回编译期保证很难」。

### 实际做了什么

| 项 | 状态 |
|---|---|
| `/api/remote.mux` WebSocket 端点 | ✅ 实测 HTTP 101 + `ready` 帧 |
| 四种下行帧 + 一种上行结果 | ✅ 字段名与上游一致（`clientId`/`eventId`/`agentId`） |
| waterfall 下发与结果关联 | ✅ `ClientHandle::ask` + `PendingWaterfalls` |
| 断线重连 | ✅ 新 generation、旧 pending 被 drain、陈旧结果被忽略 |
| JSON Schema 导出 | ✅ `girder schema` 输出，含全部四种消息类型 |
| Typert 描述符生成 | ❌ 未做 |
| `Remote` 方法命名空间 | ❌ 未做 |
| 认证 / `PeerScope` 校验 | ❌ 未做，仅透传 |

实测握手：

```
$ girder serve --bind 127.0.0.1:8791
girder rpc listening on ws://127.0.0.1:8791/api/remote.mux

HTTP/1.1 101 Switching Protocols
FRAME: {"type":"ready","clientId":"gen-1","host":{"home":"/data/home/jacksonyhe"}}
```

### 必须说清的限定

**「阶段 6 完成」不等于「Web GUI 能用了」。**

计划 §2.9 的验收写的是「Web GUI 能列出 session、发消息、看流式回复」。本次达成的是
**协议层连通**（端点、帧、重连语义），不是**功能可用**。一个真实的 DSH 客户端调用的是
Typert 生成的 `remote.session.list()` 这类方法，本 crate 没有实现这些命名空间。

所以本轮验收是：
- ✅ 连接建立、`ready` 帧、帧往返、重连语义一致
- ❌ 真实 GUI 列出 session
- ❌ 真实 GUI 发消息看流式回复

这条差别写在这里而不是只在 README，因为「GUI 能用」是计划里这一阶段存在的理由。

### 与计划的差异

| 项 | 计划 | 实际 | 原因 |
|---|---|---|---|
| 范围 | 完整 GUI 对接 | 协议层连通 | 完整对接需要 Typert 生成链路，计划自己说「没有现成方案」 |
| 类型安全 | 重建生成链路 | `schemars` 导出 schema，两侧对照 | 先证明连接成立，再决定类型安全投入 |
| 前端 | 保留 | **未改动** | 见下方风险 |

### 踩到的坑

1. **`pending` 状态写成了两份。** 第一版 `SessionState` 有 `pending` 字段，`ClientHandle`
   又用 `Arc::new(Mutex::clone(...))` 造了一份新的——断线时 drain 的是其中一份，
   另一份仍认为有未完成 waterfall。改成单一 `Arc<Mutex<...>>` 共享。

2. **`rename_all` 会把 `clientId` 变成 `client_id`。** 上游协议用 camelCase，Rust 惯用
   snake_case。必须逐字段 `#[serde(rename)]`。测试 `a_waterfall_frame_has_the_upstream_field_names`
   专门钉住这一点——写错的话真实客户端会直接解析失败。

3. **`pkill -f "girder-cli.*serve"` 把自己的 shell 也杀了**（命令行里含同样字符串）。
   这是操作失误，不是代码问题，但值得记：验证脚本里的进程匹配要更精确。

### 未缓解的风险

**前端字段改动不会被 Rust 端发现。** `schemars` 导出的 schema 是事后对照工具，
不是编译期约束。若 TS 端把 `sessionId` 改成 `sessionID`，Rust 端只会收到一个陌生字段，
不会报错。缓解需要 CI 里跑 schema diff，本阶段没有接。

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

### 完成记录（§3.2 第 5 步）

范围与实测见 [phase7-scope.md](phase7-scope.md)。测试 18 个，见
`crates/girder-core/tests/delegation.rs`。

**先于代码的一个实测发现**：这台机器**没有 Landlock**。

```
$ grep -i landlock /boot/config-$(uname -r)
# CONFIG_SECURITY_LANDLOCK is not set

$ syscall(444, NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)
landlock unavailable, errno=Function not implemented (os error 38)
```

内核 6.6.47，但编译时没开该选项。这一条决定了阶段 7 的做法：**内核级沙箱可以写，但不能声称验证过**。
写一个测不了的安全机制比不写更危险。

### 实际做了什么

| 项 | 状态 |
|---|---|
| `subagent` 进程内驱动 + 深度限制 | ✅ 完成并测试 |
| 深度到顶时从工具集中移除 `subagent` | ✅ 完成并测试 |
| `Sandbox` trait + `Enforcement` 分级 | ✅ 接口完成 |
| `landlock_abi()` 探测 | ✅ 完成（本机返回 `None`，如实报告） |
| 内核级沙箱实现 | ❌ 未做（本机不可验证） |
| 外部 CLI driver（claude-code / codex / acp） | ❌ 未做（需要那些 CLI） |
| 子 agent 暂停/恢复控制协议 | ❌ 未做 |

### 关键设计：深度限制是结构性的

一个能派生 agent 的 `subagent` 工具是**无界递归**——每层都花 token，模型一直委托就一直花。
所以：

1. **深度在派生之前检查**，不是让子 agent 自己遵守。测试
   `a_delegation_at_the_ceiling_is_refused_without_calling_the_model` 用了一个会 panic 的
   provider，证明拒绝发生在任何模型调用之前。
2. **到顶时把 `subagent` 从工具集中移除**（`DepthLimitedTools`），不是只在执行时拒绝。
   模型无法调用一个没被提供的工具——它连名字都猜不出来，执行器会报 unknown tool。
3. **深度来自调用方 context，不来自工具参数**。否则模型可以给自己加预算。

### `Enforcement` 为什么是接口的一部分

`AdvisorySandbox` 和未来的 `LandlockSandbox` 在类型上完全一样。唯一区别是
`enforcement()` 的返回值。如果它只是文档里的一句话，调用方无从知道自己是否被保护。

```rust
pub enum Enforcement {
    Advisory,                              // 只检查声明的路径，不阻止越权写
    Kernel { mechanism: &'static str },    // 内核拒绝越权访问
}
```

测试 `the_advisory_sandbox_says_it_is_advisory` 断言它报告 `Advisory` 且
`describe()` 含 `not enforced`。

### 与阶段 5 那个缺口的对照

阶段 5 记录的缺口是「沙箱不是内核级隔离」。阶段 7 **没有补上这个缺口**，而是把它
**变成了可检查的接口**：现在任何调用方都能问 `enforcement()` 并得到一个诚实答案。
补上内核实现仍然需要一台支持 Landlock 的机器。

### 踩到的坑

1. **`ToolRegistry::register` 原本要 `&mut self`，但 subagent 工具需要 context，
   context 需要工具集**——循环依赖写不出来。改成 `&self` + 内部 `Mutex`。
   代价是每次注册加锁（不是每次查找）。

2. **子 agent 的文本会被累加而不是替换。** 第一版把每个 turn 的 `TextDelta` 都
   `push_str`，结果「最终回答」变成所有 turn 的拼接。正确做法是用最后一个
   `AssistantMessage` 的文本替换。这类 bug 会让子 agent 的输出看起来包含思考过程。

## experimental 里建议直接放弃的

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

## 2.11 补充：真实 provider（原计划未列入，实施中补上）

阶段 4 的验收用的是离线 provider，我在当时的记录里写明「验证的是机制，不是模型质量」。
这一轮补上了真实 provider：`crates/girder-core/src/llm_deepseek.rs`。

### 线格式来源

读上游 `packages/llm/llm-deepseek` 确认，它用的是 **Anthropic Messages API 格式**，不是
OpenAI chat-completions 格式：

| 项 | 值 |
|---|---|
| 端点 | `{base}/messages`，默认 base 是 `https://api.deepseek.com/anthropic` |
| 必需头 | `x-api-key`、`anthropic-version: 2023-06-01`、`accept: text/event-stream` |
| 工具角色 | 没有 `tool` role，工具结果放进 `user` 轮的 `tool_result` 块 |
| 工具定义 | `input_schema` 而非 `parameters` |
| 流 | SSE，事件名 `message_start` / `content_block_start` / `content_block_delta` / `content_block_stop` / `message_delta` / `message_stop` |

### 验证到什么程度

**26 个测试**，用**本地 mock HTTP 服务器**验证：

- 请求体形状（`messages` 数组、`role` 映射、`tool_use` 的 `input` 是解析后的对象而非字符串）
- SSE 解析器（跨 chunk 分割、多字节字符在边界被切开、CRLF、注释行）
- chunk 翻译（text / thinking / tool_use 三种块、`stop_reason` 映射）
- 截断的流被报为错误而不是当成完整回答
- HTTP 401 携带状态码与响应体
- 取消的请求不上网

**没有验证到的**：真实 API 调用。本环境没有 `DEEPSEEK_API_KEY`，所以「能对接 api.deepseek.com」
是一个**未验证的声明**。代码写成只差一次 HTTP 调用，不是差一次重构。

CLI 现在会如实报告：

```
$ girder "list files"
[provider] offline (set DEEPSEEK_API_KEY to use a real model)
```

有 key 时打印 `[provider] live: <model>`。

### 踩到的坑

1. **`text_delta` 的字段是 `text`，`thinking_delta` 的字段是 `thinking`**——不是同一个键名。
   按 `text` 读 thinking 会得到空字符串，推理内容静默消失。

2. **`content_block_stop` 不能带累积后的完整块。** 第一版在 stop 时构造完整的
   `ContentBlock`，结果和 `block-end` 的组装逻辑打架，文本重复。正确做法是发空块占位，
   内容由 delta 累积。

3. **`translate` 长到 123 行被 clippy 拦下。** 按事件类型拆成 7 个方法后每个都在 30 行内。
   这次拆分不是为过 lint，是因为原来那个 `match` 分支确实该分开——每个事件的处理和
   它用的字段都是独立的。

## 2.12 补充：事件推送（修掉阶段 6 的一个错误形状）

阶段 6 我写了 `forward_events`，用每 50ms 轮询 `active_events()` 的方式把事件告诉客户端。
这一轮发现那是**错的形状，不只是低效**：

`active_events()` 返回「哪些事件名当前有监听器」。它回答不了任何一个真实问题：

- **拿不到参数。** 客户端收到的 `emit` 帧 `args` 永远是空数组。
- **同一次派发重复不了。** 一个事件触发两次，轮询只能看到它「存在」，看不到两次。
- **停不下来的噪声。** 每 50ms 一次全表扫描，即使什么都没发生。

根本原因是**事件总线没有推送通道**。这轮补上：

### 新增 `EventBus::observe`

```rust
pub fn observe(&self, events: Option<Vec<&'static str>>)
    -> UnboundedReceiver<ObservedEvent>;

pub struct ObservedEvent {
    pub name: &'static str,
    pub args: serde_json::Value,
}
```

五个 dispatch 路径（`emit` / `parallel` / `serial` / `bail` / `waterfall`）在监听器跑完后
都会调用 `announce`。选在**之后**而不是之前，因为观察者要看的是「发生了什么」，不是
「即将发生什么」；`serial`/`bail` 即使某个监听器 bail 了也照样通知——事件确实发生过。

### 对接口的影响

`Event::Args` 和 `Waterfall::Args` 增加了 `Serialize` 约束。这是必要的收紧：要把参数推给
外部观察者，它必须能被序列化。编译器立刻抓出三个没派生 `Serialize` 的工具事件参数
（`ToolExecution`、`PostExecuteArgs`、`ToolResult`），补上即可。

### 修掉的一个真实竞态

第一版 `forward_events` 在 `tokio::spawn` 的任务**内部**调用 `observe`。调用方无法知道订阅
何时生效，于是「派发 → 观察者」之间存在窗口，事件会被丢掉。测试
`a_registered_event_reaches_the_client_with_its_arguments` 直接超时失败抓到了它。

修法是**先订阅、再 spawn**。这不是测试技巧，是 API 正确性：一个订阅接口必须让调用方在
返回后立即可以信任它已经生效。

### CLI

`girder serve --watch <event>` 现在能用，可重复。事件名是 `&'static str` 常量，运行时字符串
无法直接变成它，所以加了 `known_events()` 查找表。**未知事件名报错并列出可用名称**，而不是
静默丢弃——否则一个拼写错误会表现为「这个事件从来不触发」。

```
$ girder serve --watch tools/pre-execute
girder rpc listening on ws://127.0.0.1:8801/api/remote.mux (watching: tools/pre-execute)

$ girder serve --watch nope/event
[watch] unknown event `nope/event`; known names: tools/pre-execute, tools/execute, tools/post-execute
```

### 测试

新增 10 个（7 个总线观察者 + 3 个端到端转发）。总数 194。

关键几条：
- `an_observer_sees_the_same_event_twice` — 轮询做不到的那条
- `an_observer_can_watch_a_subset` — 过滤生效
- `a_dropped_observer_is_pruned` — 关闭的通道在下次派发时被清理，不泄漏
- `a_registered_event_reaches_the_client_with_its_arguments` — 参数完整到达
- `an_empty_watch_list_forwards_nothing` — 默认不转发，避免把宿主内部事件泄漏给任何连上的 socket

## 2.13 补充：Remote 方法表（半补阶段 6 的缺口）

阶段 6 的记录写着「没有 `Remote` 方法命名空间，所以真实 GUI 连不上」。这轮补上了方法表。

### 上游实际有多少

扫 `packages/api/*/src/index.ts` 的 `@Remote(...)` 装饰器：

| 命名空间 | 方法数 |
|---|---|
| `session` | 20 |
| `workspace` | 11 |
| `job` | 3 |
| `terminal` / `account` | 各 2 |
| `workspace-files` / `remote-access` | 各 1 |

**共 40 个。** 全部实现是阶段 7 之后的事，这轮做其中**用户可见路径上最短的一段**：
`session.list` 与 `session.prompt`。

### 三个错误码，而不是一个

这是本轮最主要的设计判断。客户端必须能分辨：

| code | 含义 | 客户端该做什么 |
|---|---|---|
| `UNKNOWN_METHOD` | 名字打错了 | 改代码 |
| `NOT_IMPLEMENTED` | 上游有，本 build 没做 | 降级或提示「此功能未实现」 |
| `INVALID_ARGUMENTS` | 参数类型不对 | 修请求 |

合并成一个错误会让**半成品客户端看起来像坏了**，而不是像没做完。而「叫什么名字才算存在」
无法从查不到推断出来，所以 `declare_unimplemented` 维护一份显式清单。

### 实测

```
$ girder serve --bind 127.0.0.1:8806
girder rpc exposing 2 Remote method(s): session.list, session.prompt

session.list   → {"status":"ok","value":{"items":[{"sessionId":"demo","blank":false,...}]}}
session.search → code NOT_IMPLEMENTED
session.lst    → code UNKNOWN_METHOD
session.prompt → {"status":"ok","value":{"accepted":true,"turn":1}}
```

### `session.list` 是真实数据

`DirectoryCatalog` 扫描会话目录，文件名当 id，mtime 当 `updatedAt`，事件内容读出来判断
`blank`。实测：先跑一次 `girder "list files"` 生成 `demo.jsonl.zst`，再 `session.list`
返回 `blank:false`（因为日志里确实有 user message）。

### 诚实的边界

**GUI 还是连不上。** 它调 `session.create` / `session.page` / `session.stream`，
全部 `NOT_IMPLEMENTED`。差别是：之前「什么都不能调」，现在「能调两个、其余的会说缺什么」。
后者对排查有用，对使用还不够。README 写的是「22 个方法里实现了 2 个」，不是「GUI 能用」。

### 踩到的坑

1. **`FixedCatalog` 不排序，但测试假设了排序。** `list_marks_a_session_without_messages_as_blank`
   断言 `items[0]` 是某个特定会话，实际顺序由 catalog 决定。改成按 id 查找——
   排序是 catalog 的职责，方法只保持顺序。

2. **`Arc::clone` 在嵌套 `move` 闭包里类型推断失败。** 编译器报
   `expected &Arc<_,_>, found Arc<dyn SessionCatalog>`。加显式类型标注解决，
   不是靠猜。

## 2.14 补充：Remote 方法扩到五个

上一轮（2.13）做了 `session.list` 与 `session.prompt`，结论是「能列不能开」。
这轮补了 `create` / `page` / `events`，现在**能开新会话、能读历史**。

### 三个新方法的关键语义

**`create` 必须幂等。** 同一个 id 重复创建不能追加事件——否则客户端重试一次，会话历史
就多一个空 turn。测试 `create_twice_on_a_directory_reuses_the_existing_log` 断言第二次
create 后日志**仍是 1 条事件**。这不是优化，是正确性。

**`page` 的序列号是 1-based 日志位置。** 客户端把 `throughSeq` 钉住以读固定切面，
`beforeSeq` 排除该记录及其之后，`maxMessages` 从切面**往前**取（历史视图从最新往上滚）。
`cursor` 返回已返回记录的最后一个位置，`hasMore` 描述切面**之前**是否还有。

**`events` 与 `page` 同实现。** 上游两者并存，`page` 已标记 deprecated 但仍被现有 Web 客户端
调用。删掉它会让老客户端失效，收益为零。

### 边界处理

| 情况 | 行为 | 理由 |
|---|---|---|
| 未知 session 的 page | 空页 + `cursor:0` | 被别的主机删掉的会话应渲染为空白，不是错误横幅 |
| `throughSeq` 超过日志长度 | 钳到末尾 | 客户端钉的切面可能来自更长的日志 |
| `subagent` 地址 | `INVALID_ARGUMENTS` | 本 build 没有子 agent 注册表，按 parent/child 读会读错日志 |
| 缺 `sessionId` | `INVALID_ARGUMENTS` | 无法选择日志 |

### 实测

```
create {"sessionId":"alpha"} → {"sessionId":"alpha"}
list                         → items:[{sessionId:"alpha", blank:true, ...}]
page   {address:alpha}       → {cursor:1, hasMore:false, records:[{type:"event",...}]}
create {"sessionId":"alpha"} → 同 id，日志仍 1 条事件（文件仍 40 字节）
```

### 与真 GUI 的距离

还有 15 个方法没做，其中 GUI 打开会话后**必需**的是：

- `session.selectModel` / `session.modelCatalog` — 选模型
- `session.stream` / `session.observeEvent` — 流式订阅（`mode: 'stream'`）
- `session.projections` — 侧栏状态

所以现在的状态是：**能列出、能打开、能读历史，但不能发消息**。
`prompt` 虽然返回 `accepted:true`，本 build 没有 agent 可挂，事件不会真的产生。

### 踩到的坑

**`register_session_methods` 长到 127 行被 clippy 拦下。** 按方法族拆成
`register_list` / `register_create` / `register_history` / `register_prompt`，
每个 30 行左右。和之前几轮一样，这个阈值在这类「一堆相似闭包」的代码上是有效信号——
拆开后每个函数的职责立刻清楚了。

上一轮（2.13）做了 `session.list` 与 `session.prompt`，结论是「能列不能开」。
这轮补了 `create` / `page` / `events`，现在**能开新会话、能读历史**。

### 三个新方法的关键语义

**`create` 必须幂等。** 同一个 id 重复创建不能追加事件——否则客户端重试一次，会话历史
就多一个空 turn。测试 `create_twice_on_a_directory_reuses_the_existing_log` 断言第二次
create 后日志**仍是 1 条事件**。这不是优化，是正确性。

**`page` 的序列号是 1-based 日志位置。** 客户端把 `throughSeq` 钉住以读固定切面，
`beforeSeq` 排除该记录及其之后，`maxMessages` 从切面**往前**取（历史视图从最新往上滚）。
`cursor` 返回已返回记录的最后一个位置，`hasMore` 描述切面**之前**是否还有。

**`events` 与 `page` 同实现。** 上游两者并存，`page` 已标记 deprecated 但仍被现有 Web 客户端
调用。删掉它会让老客户端失效，收益为零。

### 边界处理

| 情况 | 行为 | 理由 |
|---|---|---|
| 未知 session 的 page | 空页 + `cursor:0` | 被别的主机删掉的会话应渲染为空白，不是错误横幅 |
| `throughSeq` 超过日志长度 | 钳到末尾 | 客户端钉的切面可能来自更长的日志 |
| `subagent` 地址 | `INVALID_ARGUMENTS` | 本 build 没有子 agent 注册表，按 parent/child 读会读错日志 |
| 缺 `sessionId` | `INVALID_ARGUMENTS` | 无法选择日志 |

### 实测

```
create {"sessionId":"alpha"} → {"sessionId":"alpha"}
list                         → items:[{sessionId:"alpha", blank:true, ...}]
page   {address:alpha}       → {cursor:1, hasMore:false, records:[{type:"event",...}]}
create {"sessionId":"alpha"} → 同 id，日志仍 1 条事件（文件仍 40 字节）
```

### 与真 GUI 的距离

还有 15 个方法没做，其中 GUI 打开会话后**必需**的是：

- `session.selectModel` / `session.modelCatalog` — 选模型
- `session.stream` / `session.observeEvent` — 流式订阅（`mode: 'stream'`）
- `session.projections` — 侧栏状态

所以现在的状态是：**能列出、能打开、能读历史，但不能发消息**。
`prompt` 虽然返回 `accepted:true`，本 build 没有 agent 可挂，事件不会真的产生。

### 踩到的坑

**`register_session_methods` 长到 127 行被 clippy 拦下。** 按方法族拆成
`register_list` / `register_create` / `register_history` / `register_prompt`，
每个 30 行左右。和之前几轮一样，这个阈值在这类「一堆相似闭包」的代码上是有效信号——
拆开后每个函数的职责立刻清楚了。

## 2.15 补充：prompt 真的会跑一轮

2.14 的记录里写着：`prompt` 返回 `accepted:true`，但「本 build 没有 agent 可挂，事件不会真的产生」。
这是所有实现里最「说谎」的一处——客户端收到 200 就以为在跑。这轮修掉了。

### `AgentRuntime`

新增 `crates/girder-core/src/runtime.rs`：

- 持有 provider 与工具集，管理**每个 session 同时只有一个 turn**
- 把 loop 的步骤发布成 `agent/turn` 事件（`turn-started` / `text-delta` / `tool-started` /
  `tool-finished` / `turn-ended`）
- `prompt` 先写用户消息、再 spawn，实现「立即返回、异步执行」

### 两条必须守住的约束

**并发 prompt 必须拒绝。** 同一会话两个 turn 会交错追加消息，模型会看到一段从未发生过的对话。
测试 `a_second_prompt_while_running_is_refused` 断言第二个 prompt 报
`already running a turn`。

**turn 结束必须释放会话。** 失败或完成都要清掉运行标记，否则一次失败会把会话永久锁死。
测试 `the_runtime_clears_a_session_when_the_turn_ends` 覆盖。

### 端到端抓到的真实 bug

第一版跑通后翻日志，发现**两条相同的 `user/message`**——`AgentRuntime::prompt` 和
`agent_loop::run` 各写了一次。

修复：给 `LoopContext` 加 `record_prompt: bool`。直接调用方（CLI）保持 `true`，
RPC runtime 设为 `false`（它已经在 spawn 前写过）。测试
`the_prompt_is_recorded_exactly_once` 断言日志里恰好一条。

**这类 bug 只有真实端到端才会暴露。** 单元测试里 runtime 和 loop 各自都被正确调用，
只有把两者串起来才看见重复。

### 实测：全链路通了

```
$ girder serve --watch agent/turn --bind 127.0.0.1:8831

create {"sessionId":"watch"} → ok
prompt {"sessionId":"watch","text":"list files"} → {"accepted":true,"turn":1}

WebSocket 收到 8 帧：
  {"type":"ready","clientId":"gen-1",...}
  {"type":"emit","event":"agent/turn","args":[{"type":"turn-started","turn":1}]}
  {"type":"emit","event":"agent/turn","args":[{"type":"tool-started","name":"list"}]}
  {"type":"emit","event":"agent/turn","args":[{"type":"tool-finished","isError":false}]}
  {"type":"emit","event":"agent/turn","args":[{"type":"turn-ended","reason":"tool-calls"}]}
  {"type":"emit","event":"agent/turn","args":[{"type":"turn-started","turn":2}]}
  {"type":"emit","event":"agent/turn","args":[{"type":"text-delta","text":"Here is what I found..."}]}
  {"type":"emit","event":"agent/turn","args":[{"type":"turn-ended","reason":"stop"}]}
```

**RPC prompt → 模型 → 工具调用 → 结果写回 → 第二轮 → 文本推送到 WebSocket**，
整条路径都通了。

### 仍然没做到的

- **`session.stream` / `observeEvent`**：上游用 `mode: 'stream'` 的专用通道，本 build 靠总线
  推送代替。语义等价但协议不同，真客户端不会用这条路径。
- **`session.cancel`**：`AgentRuntime::cancel` 已实现并有测试，但没有对应的 RPC 方法。
- **模型选择**：`selectModel` / `modelCatalog` 仍未实现。

### 踩到的坑

**`main` 长到 105 行被 clippy 拦下。** serve 分支抽成 `serve()`，再拆出
`resolve_watched` / `session_catalog` / `resolve_provider`。这是我第四次被同一个阈值拦下，
每次拆完都更清楚。

## 2.16 补充：session.cancel 与一个跳号的 turn

### session.cancel

`AgentRuntime::cancel` 本来就有并有测试，这轮加上 RPC 方法，方法总数到 6 个。

两条语义决定：

**`accepted: true` 是确认请求，不是确认完成。** 取消在下一个步骤边界生效，所以返回
`accepted`；需要确认已停止的客户端等 `turn-ended` 事件。

**没有 turn 在跑时也返回 `accepted: true`。** 上游契约是确认。一个刚好在 turn 结束时发出
取消的客户端不该因为「赢了这个竞态」而收到错误——那会让客户端去处理一个它无法处理的竞态。

### 端到端抓到的第二个 bug：turn 号跳号

第一版用「日志里 `TurnStart` 数量 + 1」算 turn 号。端到端实测：

```
prompt 1 → turn: 2
prompt 2 → turn: 4
prompt 3 → turn: 5
```

原因：`agent_loop::run` 每个模型轮次也写一条 `TurnStart`。一个两轮的 turn（先调工具再回答）
留下两条记录，下一次 prompt 就跳过 2 个号。

**turn 的正确定义是「这是第几次对话」**，即用户消息数量，不是 `TurnStart` 的条数——后者包含
loop 自己的内部轮次。修正为统计 `UserMessage`，实测变成 `1 → 2 → 3`。

测试 `turn_numbers_count_prompts_not_log_records` 钉住。

**这个 bug 单元测试抓不到**，因为单元测试里一次 turn 通常只有一轮。只有端到端跑一个
「调工具再回答」的两轮 turn，才会让 `TurnStart` 数量与 prompt 数量脱钩。

### 一次误判

上一轮端到端里我看到「第二个 prompt 返回 accepted 而没有被拒」，一度以为是 bug。重测后发现是
**时序问题**：离线 provider 太快，500ms 时第一个 turn 已经跑完，根本没有并发。

立即发送的第二个 prompt 确实被拒：

```
prompt 1: ok, turn 1
prompt 2 (immediate): error "session `b` is already running a turn"
```

记这一条是因为**「测试通过」和「测试在测我以为的东西」是两件事**。差一点就把一个正确的
实现改成错的。

## 2.17 补充：一轮质量审查，抓到两个数据损坏 bug

连续六轮加功能后，这轮回头审查。**用探针实测而不是读代码猜测**，结果三个怀疑里两个成立，
且都是数据损坏级别。

### Bug 1：并发 append 丢失事件

`SessionStore::append` 是「解码整个 zstd 帧 → 追加一条 → 重写」。两个并发 append 会
互相覆盖，而且**共用同一个 `.tmp` 中转文件**。

探针实测（8 线程 × 10 条 = 应为 80 条）：

```
expected 80 events, got 12
write errors: 41
  session io: No such file or directory (os error 2)
  session io: Unknown frame descriptor      ← 帧本身被写坏了
```

不只是丢数据，**日志帧会损坏**，那意味着整个会话不可恢复。

**修复**：按路径的进程级互斥（`path_lock`），加中转文件名带唯一后缀。
重测：80 条全部写入，0 错误。

**为什么按路径而不是按实例**：`SessionStore` 很便宜（RPC 层每次调用都 new 一个），
实例级 mutex 什么也串不住。

### Bug 2：并发 create 破坏幂等性

`create` 是「查存在 → 不存在则写」。两个并发调用都会看到「不存在」，各写一条 `TurnStart`。

探针实测（8 个并发 create 同一个 id）：

```
concurrent creates succeeded: 8/8
events in log: 8 (a fresh session should have 1)
```

**修复**：用 `OpenOptions::create_new` 让「查+建」在文件系统层原子。输的一方落到读赢家写的日志。
重测：8 个全部成功，日志 1 条事件。

### 优化：list 不再解码全部日志

`list` 原本读取并解压**每个会话的全部事件**只为算一个 `blank` 布尔值。实测 200 个会话时
解压了 10,200 条事件。

改为 `has_user_message()`：流式读到第一条 user message 就停。`SessionRecord` 新增 `blank`
字段，`list` 不再携带 events。

**诚实的数字**：优化前 7.8ms，优化后 3.8ms（消息在前）/ 6.5ms（消息在后）。
不是数量级提升，因为 zstd 是流式解压，取到后面的事件仍要读完整帧。真正的收益是
**解码事件数从 10,200 降到 0**。

### 一处过时的测试

`a_directory_of_logs_lists_one_row_per_file` 原断言是
`records.iter().all(|r| !r.events.is_empty())`——「事件必须被读取」。这恰好钉住了我刚修掉的
低效行为。改成断言新语义（`events` 为空、`blank` 正确）。

**记这一条**：优化改变语义时，旧测试可能不是在保护正确性，而是在保护旧实现。

### 教训

前几轮我一直在加功能，每轮都通过测试。但这两个 bug 从阶段 4 就存在，一直没被触发，
因为测试从没并发写过同一个会话。**「测试全绿」不等于「没有 bug」，只等于「已写的测试通过」。**

## 2.18 补充：观察者背压（补上 2.17 留下的第三个隐患）

2.17 结尾我列了两处「查过但没改」的，第一处是「`agent/turn` 在没人监听时事件会堆积」。
这轮验证了它，**成立**，并修掉。

### 探针实测

```
dropped receiver: observers left = 0          ← 这个没问题
unread receiver: buffered 100000 events       ← 这个有问题
slow reader: got 50 of 50
```

接收者被 drop 时会正确清理，但**接收者活着却不读时，10 万条事件全部留在内存**。

`observe()` 用的是 `unbounded_channel`。对实时事件流这是错的：派发以模型速度到来，
消费以网络速度进行，无界队列等于把「一个慢客户端」变成「无界内存」。

### 修法与一个设计判断

改成有界通道（4096 容量），满时**丢弃该观察者**而不是丢弃最旧的事件。

为什么丢观察者而不是丢事件：这些是客户端**按顺序渲染**的 turn 事件。丢中间一条会让客户端
看到「turn 结束了但没开始」。丢观察者则表现为连接关闭——那是客户端能处理的状态。

顺带补了 `ObservedEvent` 的导出。之前 `observe()` 是 pub 但返回类型没导出，外部代码
根本没法接住它——这是个 API 缺口，探针编译失败才暴露。

### 验证

三条新测试：

| 测试 | 断言 |
|---|---|
| `an_unread_observer_is_dropped_rather_than_buffered_without_bound` | 发 10,000 条后观察者被丢弃，且**收到的是连续前缀**（不会中间断层） |
| `a_consumer_that_keeps_up_observes_every_dispatch` | 健康消费者一条不丢，且不被丢弃 |
| `a_dropped_receiver_is_pruned_on_the_next_dispatch` | 清理不依赖消费者协作退订 |

端到端复验：正常 WebSocket 客户端仍收到全部 7 个事件帧
（`turn-started → tool-started → tool-finished → turn-ended → turn-started → text-delta → turn-ended`），
有界通道没有破坏正常路径。

### 仍未处理的

2.17 列的第二处：**`SessionStore::rewrite` 是 O(文件)**，每条事件重写整个日志。
这是已知设计债务，写在代码注释和模块文档里，没在这轮动——修它需要改成追加式帧或
未压缩工作文件 + 定期压缩，是独立的一次改动。

## 2.19 补充：日志改为多帧追加（还掉最后一处已知债务）

2.17 和 2.18 都提到一处未修的债务：`SessionStore::rewrite` 是 O(文件)，每条事件重写整个日志。
这轮处理掉了。

### 先量化，再动手

用 4KB 消息实测（每次 append 的平均耗时）：

| 日志规模 | 重写式 | 追加式（改后） |
|---|---|---|
| 50 条 | 0.61ms | 0.078ms |
| 200 条 | 1.76ms | 0.078ms |
| 500 条 | 4.28ms | 0.078ms |
| 2000 条 | 20.1ms | 0.078ms |

重写式是 **O(n)**，整体 O(n²)：2000 条消息的会话，每条新消息要重写全部，单次 20ms。
追加式**与规模无关**。

### 修法：zstd 拼接帧

一个 append 写一个独立的 zstd 帧，追加到文件末尾。zstd 解码器会把拼接的帧当作一条流，
所以读取端不用改——`read_all` 和 `read_file` 本来就是流式解码。

先用探针验证这个前提成立（两个独立 `encode_all` 拼接后，`decode_all` 与流式 decoder
都能正确解出两段），再改代码。

### 一个必须说清的取舍

**追加牺牲了崩溃原子性。** 原来「写临时文件 + rename」保证：崩溃时要么是旧文件，要么是新文件，
不会截断。改成追加后，写到一半崩溃会留下撕裂的尾部，丢失正在写的那条事件。

这个取舍可接受，因为：格式本来就容忍截断尾部（模块文档写了），读取端把解析失败的行当作内容
而不是错误。丢一条事件比丢整个会话好。

### 第二个发现：`sync_data` 是真正的瓶颈

改完多帧追加后测出来仍是 2-3ms，与规模无关但比预期高。追下去发现是 `sync_data()`：
每次约 3ms，是压缩+写入的 20 倍。

**去掉了每事件 sync**，并新增显式的 `sync()` 方法。理由是：`sync_data` 只防**操作系统崩溃**，
不防**进程崩溃**——后者数据已在 page cache 里，写操作本身就能存活。会话日志最关心的正是后者。

改后：**78µs**，比最初的 3.37ms 快 43 倍。

需要前一种保证的调用方（关机、检查点）显式调 `sync()`，那是它该自己决定的节奏。

### 一次静默失败的替换

第一次改 `append` 时，我的文本替换**没有匹配上**（`cargo fmt` 重排过 `lock.lock()` 的换行），
而 Python 的 `str.replace` 在无匹配时**静默返回原串**。

结果：代码没改，性能没变，而我是从「数字完全没动」才发现的。第二次改用索引定位并断言
`"One compressed frame per append" in content` 才确认生效。

**教训**：文本替换必须验证结果，不能假设调用成功。

### 删掉的死代码

`rewrite` 方法在改造后无人调用，clippy 报 `never used`。删掉了——留着会让读代码的人以为
还有一条重写路径。

### 测试

新增 4 个：
- `a_log_is_readable_after_many_appended_frames` — 50 个帧全部读出
- `appending_does_not_rewrite_the_whole_log` — 文件只增不减，30 条小事件 < 4KB
- `an_explicit_sync_flushes_the_log` — `sync()` 对存在与不存在的文件都正常
- `has_user_message_stops_at_the_first_message` — 流式提前终止

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

**文档状态**：迁移计划基线。阶段 0-7 已完成，各阶段的限定见对应「完成记录」：阶段 6 只完成协议层（GUI 功能对接未做）；阶段 7 的沙箱只做到接口（内核隔离因本机无 Landlock 未实现）。实际情况见 2.2 / 2.4 / 2.5 / 2.6 / 2.7 / 2.8 / 2.9 / 2.10 节。
