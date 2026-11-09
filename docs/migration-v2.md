# 全量迁移计划（v2）

取代 `migration-plan.md` 的阶段划分。那份文档回答的是「可行性如何」，这份回答的是「怎么做完」。

## 实测基线

用 `git ls-files` 逐文件统计 `packages/*/*/src/**/*.ts`，排除 `client`（前端保留）、
`experimental`（计划建议部分放弃）、`test-support`（测试基建）：

| | 行数 | 文件数 |
|---|---|---|
| 迁移目标（host 源码） | **212,921** | **978** |
| vendor 框架（cordis/loader/include 等） | ~6,700 | 38 |
| 已迁 Rust 源码 | 8,977 | 22 |
| 已完成比例 | **~4%** | — |

## 现状的准确定位

已完成的不是「八个阶段」，而是**骨架 + 一条垂直切片**：

| 已有 | 覆盖的上游部分 | 完成度判断 |
|---|---|---|
| `service.rs` / `registry.rs` | cordis 服务注册 | 骨架，缺 isolate/intercept |
| `fiber.rs` | cordis fiber 生命周期 | 核心状态机，缺 config 校验、reload |
| `events.rs` | cordis 事件总线 | 五种模式齐全，缺作用域过滤 |
| `config.rs` | cordis loader 的 patch 层叠 | patch 语义对齐，缺 bundle 加载 |
| `llm.rs` / `llm_deepseek.rs` | `llm` 15k 行中的一种 provider | ~4% |
| `session.rs` | `session` 18.6k 行中的读写 | ~3% |
| `tools.rs` / `agent_loop.rs` | 工具管线 + 循环 | 骨架 |
| `remote.rs` / `runtime.rs` | `api` 12.5k 中的 6 个方法 | ~5% |
| `exec.rs` | `subprocess` + `shell` | bash 一种，无沙箱内核 |
| `subagent.rs` | `subagent` 9.6k 中的进程内路径 | ~10% |

**没碰的大块**：`typert`（10.9k）、`extensions`（14.1k）、`boot`（13.1k）、
`session-query`（5.2k）、`context`（4.6k）、`host`（4.5k）、`web`（3.7k）、
`ptc-runtime`（3.6k）、以及 `session`/`llm`/`api` 的绝大部分。

## 分层原则

迁移必须**自底向上**，因为上层编译依赖下层。按依赖深度分五层：

```
L0  框架基座    cordis 语义 → Rust 原语（多数已有）
L1  值域与工具  brand / values / util / schemastery / json
L2  基础设施    fs / subprocess / shell / sandbox / storage
L3  领域核心    llm / session / context / compaction / tools
L4  编排        agent / agent-loop / subagent / workflow / goal / schedule
L5  对外        api / typert / web / host / boot / extensions
```

每层内部按「被依赖次数」排序：先做被依赖多的。

## 里程碑（以验收物为准，不以时间）

### M0 — 基座补齐（L0）

已有 `ServiceKey`/`Scope`/`Fiber`/`EventBus`，缺：

- **服务隔离与拦截**：cordis 的 `isolate()` / `intercept()`。当前 registry 是全局表，
  无法表达「同一服务在不同插件树下有不同实现」。**这是 L4 多 agent 的前置条件。**
- **作用域事件过滤**：上游 20 个事件带 `Scoped<T>`，按 agent 过滤。当前总线无过滤。
- **配置 schema 校验**：`schemastery` 的对应物。当前 `config.rs` 只验证 patch 结构。

**验收**：两个隔离子树能各自提供同名服务且互不干扰；带 scope 的事件只到达匹配的监听器。

### M1 — 值域层（L1）

上游 `brand`（65 处 `brandString`）、`util-values`、`util`（7.8k）、`schemastery`（1.0k）、
`json-value`。

**验收**：品牌类型体系（`SessionId`/`ToolCallId`/`AgentId` 等）编译期不可混用；
JSON 无损往返测试通过。

### M2 — 基础设施（L2）

`fs`（7.3k）、`subprocess`（6.7k）、`shell`（4.7k）、`sandbox`（3.8k）、`storage`（2.6k）。

当前 `exec.rs` 是一个 bash 工具，需要拆成：
- `fs` seam：读写、watcher
- `subprocess` seam：进程管理、stdout/stderr 流、僵尸回收
- `shell` seam：bash/zsh、进程组、信号
- `sandbox` trait + **内核实现**（landlock/seatbelt；本机不可验证，需另一环境）

**验收**：计划 §2.8 的四条（管道、进程组超时、沙箱拒绝、无僵尸）在真实内核隔离下通过。

### M3 — 领域核心（L3）

这是工作量最大的一层，也是计划里「阶段 4 切片」的完整版：

| 子项 | 上游 | 关键内容 |
|---|---|---|
| `llm` 完整 | 15.0k | 多 provider、消息/内容块全变体、重试、token 计量、模型发现 |
| `session` 完整 | 18.6k | 60+ 事件变体、fork、repair、surface、request-header |
| `session-query` | 5.2k | 会话检索 |
| `context` | 4.6k | 上下文组装、system-prompt |
| `compaction` | 2.9k | 上下文压缩 |
| `attachment` | 2.3k | 图片/文件附件 |
| `tools` 完整 | — | 全部内置工具（read/write/edit/glob/grep/todo/web…） |

**验收**：一次真实对话可在压缩、附件、多 provider 切换下完成。

### M4 — 编排（L4）

`agent`（14.5k 的 core 里大部分）、`agent-loop`、`subagent` 完整（9.6k）、
`workflow`（2.3k）、`goal`（2.5k）、`schedule`（4.1k）、`team`（1.3k）、`jobs`（2.4k）。

**验收**：子 agent 三种外部 driver（ACP/claude-code/codex）可工作；定时任务与目标追踪可用。

### M5 — 对外接口（L5）

| 子项 | 上游 | 风险 |
|---|---|---|
| `typert` | 10.9k | **最高**。TS 编译期类型投影，Rust 无对应物 |
| `api` | 12.5k | 剩余 34 个 Remote 方法 + 流式订阅 |
| `web` / `host` | 8.2k | HTTP 服务、静态资源 |
| `boot` | 13.1k | bundle/profile 加载、插件组合 |
| `extensions` | 14.1k | cordis 运行时集成（可能整体放弃） |

**验收**：真实 DSH Web GUI 连上 Rust 后端并完成一次完整对话。

### M6 — 剩余与收尾

`credentials`（3.3k）、`interaction`（3.0k）、`mcp`（2.9k）、`skill`（2.8k）、
`lsp`（2.4k）、`ssh`（2.1k）、`terminal`（2.0k）、`acp`（1.8k）、`sdk`（1.6k）、
其余小包。

## `typert` 的处理（最大风险，需要单独决策）

上游 10.9k 行做的是：TS 装饰器 + 编译器插件，把服务方法签名投影成客户端类型。

Rust 侧的三个选项：

| 方案 | 代价 | 评价 |
|---|---|---|
| A. 手工维护消息类型 + schemars 导出 | 每个方法写两份 | 可靠但工作量随方法数线性增长 |
| B. 用 `#[derive]` 宏生成描述符 | 一次性投入大 | 接近上游能力，但宏开发本身是硬活 |
| C. 放弃类型投影，只保留 schema | 失去编译期保证 | 最快，但前端要手工对 |

**当前实现是 C**（`girder-rpc` 的 schema 导出）。M5 开始前必须在这三个里选一个。

## 执行规则

每个里程碑按固定五步：

1. **接口先行**：写 seam 的 `ServiceKey` 与 trait，确认没有写不出来的接口
2. **测试先行**：验收条件先写成失败的测试
3. **实现**
4. **验证**：`cargo test` + 真实端到端（不只是单测）
5. **记录偏差**：实际与计划的差异，写进本文档

**每个里程碑结束必须能端到端跑一次**。不能积累「写完但没接过真实路径」的模块——
阶段 3-7 的经验是，接真实路径时暴露的 bug 比单测多。

## 已知的不可迁移项（不隐瞒）

| 项 | 原因 | 处理 |
|---|---|---|
| 运行时插件安装 | Rust 无运行时模块导入 | 编译期 `inventory` 清单 |
| `ctx.xxx` 动态属性 | 无 Proxy | 显式 `ctx.tools()` |
| `!!js` 配置表达式 | 无解释器 | `when:` 结构（已实现） |
| `declare module` 类型合并 | 无声明合并 | 每事件一个 trait |
| HMR | 依赖干净拆除 | 不做 |
| `AsyncLocalStorage` | 不跨 spawn（已实测） | 显式传参 |

## 当前进度

- [x] M0 部分（ServiceKey/Scope/Fiber/EventBus 骨架）
- [ ] M0 剩余（隔离/拦截/作用域过滤/schema 校验）
- [ ] M1–M6

**下一个动作**：M0 剩余部分（配置 schema 校验），然后是 M1。

## M0 进度记录

已完成：

- [x] **服务隔离**（`Registry::isolate`）。表键从 `id` 改为 `(id, label)`，`RegistryContext`
      携带一张「服务名 → label」的映射。语义与 cordis 一致：隔离的子上下文看不到父的实现，
      同一 label 的两个子上下文共享实现。**这是 M4 多 agent 的硬前置**——每个 agent 要自己的
      `session`，但要共享一个 `llm`。
- [x] **作用域事件过滤**（`EventBus::on_scoped` / `emit_scoped`）。`ScopeKey` 编码成
      `parent << 8 | index`，祖先判定变成移位比较，不需要父表。准入只向上扩展：
      祖先监听器收后代事件，反之不成立。

两处实现味道值得记：

1. **label 计数器从 1 开始。** 第一版用 `AtomicU64::default()`（从 0 起）配 `.max(1)`，
   结果所有自动 label 都是 1，两个「独立」子树共享了实现。是
   `labels_are_unique_when_not_supplied` 测试抓到的。
2. **`emit` 原本会把事件送给 scoped 监听器。** 因为它用无 scope 的 `snapshot()`。
   无作用域的派发不该被任何子树看到——`an_unscoped_dispatch_reaches_only_global_listeners`
   抓到了。

- [x] **配置 schema 校验**（`ConfigSchema` / `FieldRule`）。这是 `schemastery` 的替代。

      `schemastery` 做两件事：**校验** 和 **从同一份定义派生 TypeScript 类型**。Rust 的类型
      来自 `serde` 派生，所以只剩校验那一半。规则做成**数据**（`FieldRule` 枚举）而不是
      builder，好处是 schema 可以被列举、比对、生成文档。

      覆盖面与上游一致：默认值、数值范围、步长（`.step(1024)`）、枚举取值、必填、未知字段
      拒绝。**一次报告所有问题**，不是一个一个报——三个拼写错误应该一次看到三个。

      校验发生在 **patch 合层之后**，因为一个 mode bundle 有权修正 base bundle 设错的值。
      测试 `composition_before_validation_lets_a_later_layer_fix_a_bad_value` 钉住这条。

## M0 完成

三项全部完成：服务隔离、作用域事件过滤、配置 schema 校验。

## M1 进度记录

上游 86 个 `Branded<'...'>` 类型。M1 先建**机制**和当前代码真正用到的五个，
其余属于 M3-M6 的领域，在那里随领域一起加——提前声明只会产生和最终归属不一致的孤儿类型。

已完成：

- [x] **`Id<B>` + `Brand` trait**。newtype 而非类型别名，构造只能走 `Id::new`，丢失品牌只能走
      `into_string`，两个都是显式的。
- [x] **构造时校验**。空、超长（>512）、非法字符都拒绝。允许的字符集是字母数字加
      `- _ . : / +`，因为上游 id 是复合的（agent id 是 `parent/child`，scope key 带 `:`，
      base64url token 带 `- _`）。
- [x] **序列化不改变线格式**。`SessionId` 在 JSON 里仍是 `"abc"`，这是品牌可以加到已有字段上
      而不需要迁移的前提。反序列化同样校验，畸形 id 不能从 JSON 进来。
- [x] 五个品牌：`SessionId` / `ToolCallId` / `AgentId` / `WorkspaceId` / `MessageId`。

**比 TS 幻影类型强的地方**：TS 的 brand 在运行时被擦除，`brandString()` 是无校验的断言；
Rust 的 newtype 构造器不公开时，非法 id 根本造不出来。

**一个刻意的限制**：`Id` 不实现 `Deref<Target = str>`。实现它会让「传 `&str` 再转回另一个
品牌」变得太容易，而那正是品牌要防的事。`as_str()` 和 `into_string()` 都是显式的。

**一处实现细节**：`base64url` 无填充，所以 `=` 被拒绝。我最初的测试期望里有 `"a+b="`，
被测试自己抓出来——如果允许 `=`，同一个 token 会有两种合法拼写。

**下一个动作**：M1 剩余（`util-values` 的 JSON 无损往返、`util` 的路径与字符串辅助），
然后进 M2 基础设施。
