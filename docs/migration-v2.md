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

- [x] **JSON 无损（`json.rs`）**。原计划是照搬上游的 `isRemoteJsonValue`，实测后发现
      **前提不一样**：`serde_json::Number` 在构造时就拒绝 NaN（`from_f64` 返回 `None`），
      `Value::from(f64::NAN)` 直接变成 `Null`。所以「NaN 变 null」这个 JS 风险在 Rust 里
      **不可表达**——检查器根本看不到它。

      剩下唯一真实的风险是 **整数超过 2^53**：JS 全是 f64 所以没问题，而 Rust 的 64 位计数器、
      字节数、时间戳会超过它。`9007199254740993` 在 Rust 里往返无损，但 JS 客户端解析后得到
      `...992`。检查放在边界上，两个构造函数（`number_from_f64` / `number_from_u64`）负责在
      值产生前拒绝。

- [x] **路径根（`paths.rs`）**。`~` 展开、环境变量覆盖、显示缩写三条语义。
      两个上游踩过的坑都保留了：**空白环境变量视为未设置**（否则会把根解析到工作目录），
      **未知 home 报错而不回退**（否则状态散落到启动目录）。

      额外加了 `join_within`：工具从模型拿路径时，`../../etc/passwd` 必须被拒。这里有个 Rust
      特有的陷阱——`root.join("/etc/passwd")` 会**静默返回绝对路径**，绕过根目录，所以检查必须
      在规范化之后而不是靠字符串前缀。

## M1 完成

三项：品牌标识符、JSON 无损、路径根。

## M2 进度记录

### fs seam（已完成）

`fs.rs`：`FileSystem` trait + `LocalFileSystem`。

**核心设计是 `FsVersion`**。上游 7.3k 行的抽象层里，真正决定正确性的是乐观并发：
写入必须携带从 `stat` 拿到的版本，不匹配就拒绝。这样「读-改-写」不会静默覆盖别人的修改，
而是告诉模型「文件变了，重读」。

测试 `a_write_with_a_stale_version_is_refused` 断言**另一个写入者的改动存活**——
这是这个设计存在的唯一理由。

`FsTarget` 分成 `key`（稳定身份）和 `display_path`（给人看）两个字段。本地后端两者相同，
但远程后端的 display 是 URI 而 key 不是。接口不假设它们一样。

**一处实现陷阱**：第一版用 `std::sync::Mutex` 守护写入临界区，编译报
`future cannot be sent between threads safely`——`std` 的 guard 不是 `Send`，跨 `await`
持有会让 future 无法 spawn。改成 `tokio::sync::Mutex`（`async_path_lock`）。

这已经是同一类错误的**第三次**（`fiber` 的 `start`、`events` 的 `observe_services`、
现在的 `fs`）。规则很清楚：**任何跨 `await` 的临界区必须用 tokio 的锁**。

### subprocess seam（已完成）

`subprocess.rs`：`SpawnOptions` + `run`，`exec.rs` 改为委托。

拆分的理由不是审美：`run_shell` 和 `run_bash` 原本各自复制了管道并发读取、进程组杀死、
超时竞态三块逻辑，改任何一处都要改两遍。上一轮在 `exec.rs` 内部合并过一次，但进程机制和
路径策略还混在一个文件里。

现在职责清楚：

| 文件 | 职责 |
|---|---|
| `subprocess.rs` | 进程机制：spawn、进程组、管道、超时、回收 |
| `exec.rs` | 路径策略、`Sandbox` trait、bash 工具 |
| `fs.rs` | 文件读写与版本守卫 |

**三种静默失败模式各有测试**：

1. `a_timeout_kills_the_whole_process_group` —— 断言**孙进程已死**，不是只断言 shell 退出
2. `a_large_output_does_not_deadlock` —— 20,000 行输出，管道必须并发排空
3. `a_killed_command_leaves_no_zombie` —— 读 `/proc` 统计本进程的 `Z` 状态子进程

### storage seam（已完成）

`storage.rs`：`KvBackend` trait + `FileBackend`（单文档布局）。

**最要紧的是版本门禁**。一个 unit 声明格式版本；后端读到声明之外的版本必须拒绝，而不是读。
理由：旧文档在新 schema 下读出来会**静默产生错误数据**，那比失败更糟。

测试 `a_document_from_a_newer_build_is_refused` 钉住这条：用 v2 写，用 v1 打开，必须报
`VersionMismatch`。

`compatible_with()` 是逃生口：owner 可以担保旧记录在当前 schema 下仍可读。

**一个刻意留下的缺口**：`PerRecord` 布局在词汇里声明了，但**没有实现**。测试
`the_per_record_layout_is_declared_but_not_implemented` 明确记录这件事——现在后端接受它
但行为等同 `Single`。如果假装支持，调用方会以为大记录不再重写整个 unit。

**未完成**：`sandbox` 内核实现（本机无 Landlock）、`PerRecord` 布局。

## 真实 API 验证（M3 闭环）

拿到 key 后做的第一件事是**用真实端点验证 provider**，结果抓到两个只有真实 API 才会暴露的 bug。

### Bug 1：tool 结果没序列化成 `tool_result`

真实 API 直接拒绝：

```
messages.2: `tool_use` ids were found without `tool_result` blocks immediately after
```

Messages 协议要求每个 `tool_use` 由**紧邻的下一条消息**里的 `tool_result` 应答。我的 provider 把
tool 消息平铺成 `text`，本地 mock 服务器不校验，所以**全套测试通过而真实调用必然失败**。

修法：`llm::Message` 增加 `tool_call_id`（协议必需信息，不是可选装饰），provider 序列化时生成
`tool_result` 块。

### Bug 2：空 `block-end` 覆盖了累积文本

真实响应里模型回了 `OK`，我的 `AssistantMessage` 却是空字符串。

原因：DeepSeek 在 `content_block_stop` 发的是**空块占位**（内容靠 delta 累积），而我的 assembler
把占位写进了 slot，覆盖了 delta 累积的文本。

修法：占位块**永不落盘**——它不携带内容，落到 slot 只会遮蔽现有内容。

### 这两个 bug 的共性

**mock 服务器只验证我让它验证的东西。** 它返回我构造的 SSE，所以我的解析器遇到的输入恰好是
我预期的那种。真实 API 会：
- 校验协议（`tool_result` 必须存在）
- 发出我没预料到的形状（空占位块）

现在有一个 `tests/live_provider.rs`，**无 key 时全部跳过**，有 key 时跑 4 个真实场景
（纯文本、工具请求、tool_result 回传、取消）。这是唯一能覆盖这类 bug 的测试。

### 压缩摘要器（已完成，真实模型验证）

`compaction.rs` 新增 `summary_request` / `extract_summary` / `checkpoint_message` / `is_checkpoint`。

**指令作为最后一条 user 消息，而不是单独的 system prompt。** 这不是排版偏好：保留对话自己的
system prompt、tools 和前导消息，让这次辅助调用成为最后一次路由请求的**真正前缀**，provider 的
prefix cache 因此被复用而不是作废。换成单独 system prompt 会改变前缀，每次压缩都付全价。

**八个 section 全部必需，空的写 `(none)`。** 摘要漏掉「试过什么失败了」，下一个模型就会重蹈覆辙。
要求每节都存在，让「空」是**可见的空**，而不是悄悄缺失。

**checkpoint 带 preamble 和标签。** 没有 preamble，模型会把摘要当成对它说的话去回答；没有标签，
第二次压缩分不清 checkpoint 和用户写的内容。

**真实模型验证**（`tests/live_provider.rs`，无 key 时跳过）：

- `a_real_summary_holds_every_required_section` —— 断言八个 section 都在，且主题没丢
- `a_summary_becomes_a_detectable_checkpoint` —— 摘要 → 包装 → `is_checkpoint` 认出

实测输出保留了 `crates/core/src/retry.rs`、`RetryPolicy`、`run_with_retry`、
`jitter_never_produces_zero` 这些**精确标识符**，以及「jitter 结果永不为零」的设计决策。

### 多 provider 注册与模型目录（已完成）

`providers.rs`：`RouteRegistry` / `Route` / `ModelInfo` / `Providers` / `MountedProvider`。

**路由是寻址单位，不是 provider。** 一个 provider 可以服务多个模型，一个部署也可以把同一个
provider 挂载两次用不同凭据（个人 key 和组织 key）。路由名是调用方选的，所以两次挂载互不遮蔽；
名字也在适配器上而不是 provider 上。

**模型目录存在的原因**：超过窗口的请求要等一次往返才被拒绝，而拒绝信息是笼统的
"context length exceeded"，**没有数字**。提前知道窗口才能让压缩在发送前决定。

**未知窗口不能当作 0。** `fits()` 在窗口未知时返回 `None` 而不是 `false`——「装不下」是一个
需要知道窗口才能下的断言，对未知窗口回答 `false` 会让所有发往该模型的请求都被拒绝。

**重复路由名被拒绝。** 静默替换会让配置重载后，已记录的路由名指向另一个 provider。

**真实 API 验证**：`a_registered_route_serves_a_real_request` 走完整链路
catalogue → resolve → 真实请求，并断言路由声明了窗口（没有窗口就无法驱动压缩）。

## M4 进度记录

### 消息收件箱（已完成）

`inbox.rs`：`Inbox` / `Splice` / `replay`。

**两个队列，并且必须分开。** 用户在模型思考时发的消息属于**下一个 turn**，在工具执行时发的属于
**下一个 step**。前者是模型下一轮才该看到的补充，后者是这一轮里就该看到的纠正——混在一起会让
模型在错误的时机看到它。

**日志记录 splice 而不是 add/remove 对。** 收件箱是可变的：输入在被消费前可以编辑或撤回。
记「加了一条」「删了一条」会让折叠依赖顺序，且难以校验。splice 把整次编辑记成一个操作：
在 `start` 处删 `removed` 条、插 `inserted` 条。重放是确定的，非法的那条在它出现处就能被发现。

**不变量：跨两个队列不能有重复的消息 id。** 重复会被消费两次，模型会把一条指令看成两条消息。

**拒绝的 splice 不留痕迹。** 范围检查和重复检查都在改动之前，所以被拒的 splice 让收件箱保持原样，
而不是半应用。

## M3 进度记录

### 重试策略（已完成）

`retry.rs`：`RetryPolicy` / `Backoff` / `run_with_retry`。

上游 `llm` 15k 行里，重试策略是**独立可验证**的一块，所以先做。语义与上游一致：
指数退避、对称抖动、按稳定错误码决定是否重试。

三个设计点：

**按错误码而非消息决定重试。** 消息是给人看的，码是给策略看的。按散文匹配的结果是
provider 改一次措辞，重试循环就悄悄不再重试了。

**抖动由调用方提供随机数。** `jittered(base, draw)` 接受 `[0,1)` 的抽样，而不是内部调
`rand`。这样调度可测，库也不需要引入随机数依赖。

**单次延迟有上限，总时长可计算。** `worst_case_total()` 让调用方在请求前就能判断
「这个策略我能不能接受」。没有这个数，调参只能靠试。

测试里两条值得说：

- `jitter_never_produces_zero` —— 零延迟是重试风暴，正是抖动要防的东西
- `a_large_attempt_number_does_not_overflow` —— `2u32.pow(31)` 在 debug 下会 panic，位移是饱和的

### 消息模型（已完成）

`message.rs`：`Message` / `Role` / `MessageSource` / `ContextForm`。

**核心是把「角色」和「来源」拆成两个轴。**

`role` 回答「这是什么位置的消息」，`source` 回答「谁产生的」。当前实现三种角色
（user/assistant/tool），补到五种：`system` 和 `developer`。

`developer` 单独成角色是因为**工具增删骑在一轮对话上，但既不是模型说的也不是人说的**。
归到任何一边都会让重放有歧义。

来源是判别联合：`user` / `model` / `tool` / `system-prompt` / `relay`。同一个 `role: user`
可以是本人、可以是别的 agent 中继、可以是插件注入的上下文——角色相同，来源不同。

**一致性检查** `check_history`：`assistant` 必须来自 model，`tool` 必须带 call id。
这个检查存在是因为日志可以从磁盘解码，**手改的文件会说谎**，重放要在行动前拒绝而不是照做。

**`ContextForm` 是语义的，不是视觉的**：`instructions` / `catalog` / `snapshot` / `notice` /
`relay` / `recall`。颜色、图标、折叠默认值属于消费方，不能进这个枚举。

**一处换算细节**：`bound_context_summary` 按**字符**而非字节截断。CJK 摘要是 N 个字符但 3N 字节，
按字节限制会砍到三分之一。

### session 事件词汇（对话核心已完成）

`session.rs`：10 个事件类型，覆盖对话核心。

上游 `known-event-types.ts` 有 **54 个事件类型、30 个领域**。这轮做的是对话核心
（turn / step / message / tool / retry），其他领域（compaction、team、subagent、approval…）
的事件挂在它们上面，属于后续批次。

**加了 `step` 层。** 这是我之前的设计缺口：一个 turn 不是一次模型调用，而是一串 step，
每个 step 是一次模型调用加上它请求的工具执行。只记 turn 会丢失「哪个工具调用属于哪一步」，
重放时无法重建模型在 turn 中途看到的东西。

**`TurnEndReason` 与 `FinishReason` 分开。** turn 可以因为模型从不报告的原因结束——取消、
被拦、工具激活失败。复用模型的 stop reason 会让「模型说完了」和「用户按了 Ctrl-C」
在日志里是同一个值。

**`user/message` 改成一个变体承载三种来源。** 本人输入、合成注入、中继消息都是 user 角色，
`message.source` 区分它们。拆成三个事件会让每个消费者为读一轮对话而匹配三种情况。

**`llm/retry` 事件。** 没有它，一次重试留下的两个 assistant 消息看起来像协议违规。

### 工作区指令（context 的一部分，已完成）

`instructions.rs`：`discover` / `render` / `digest` / `InstructionState`。

**发现向上走。** 指令对它下面的一切生效，所以 `AGENTS.md` 要沿路径从根走到工作目录，全部收集；
外层在前，让文本读起来是「通用规则，然后是本地细则」。

**三种结果必须区分**：

| 结果 | 含义 |
|---|---|
| 渲染 | 进入文本 |
| 截断 | 进入文本，在字节预算处切断，并记录切了多少 |
| 省略 | 不进文本，**但文件被点名** |

第三种是要小心处理的：静默丢掉用户写的文件，会让模型在没有本该遵守的规则时行动。
**点名和不点名的差别，就是「模型知道有一个它看不到的文件」和「模型不知道」。**

**一个被测试逼出来的阈值。** 第一版在剩余预算很小时选择截断，测试发现预算 10 字节时
文件被截成 10 字节——那段文本不表达任何规则，但呈现为内容会让模型以为自己被指示过。
加了 `MIN_USEFUL_SLICE = 256`（约一段），低于它改为省略。

阈值只作用于**剩余预算**，不作用于单文件上限：运维设了小的单文件上限是要求短摘录，那是刻意的选择。

**UTF-8 边界。** 按字节截断会切开多字节字符，构造 `str` 时会被有损替换而不是拒绝。截断点退到字符边界。

**digest 按 trim 后的内容算。** 指令文件在每次文件系统工具调用后都会重新检查，
空白改动导致整份重新注入会把上下文塞满副本。

### 上下文压缩（已完成范围选择）

`compaction.rs`：`select` / `balances` / `pairing_balanced_at` / `transcript_for_summary`。

**一条不变量主导全部正确性**：切割不能把 tool call 和它的 result 分开。

一条 `tool/call` 活着而 `tool/result` 被压掉的history，不只是不完整——**每个 provider 都会拒绝
下一次请求**，因为无应答的 tool call 是协议违规。

所以合法的切割点只有「跨切割点的未应答调用数为零」的位置。

**为什么压缩是一个区间而不是过滤器**：单独过滤消息看起来更简单，但是错的。从一轮对话中间丢掉
一条消息，会让存活的 tool call 和 result 引用一个空洞。区间让存活的前缀和后缀各自内部一致，
摘要在中间顶上。

**起点也要平衡**：只检查结束点不够。范围若从一对 tool call 中间开始，存活的前缀会剩下一个
result 而它的 call 被摘要掉了。

**一个被测试纠正的断言**：我最初断言「压缩范围必须在 call 之前结束」，实际实现让它包含整个配对
（`end=12`，即 result 之后）。两者都合法——真正的不变量是**两侧各自平衡**，而不是范围边界落在哪里。
测试改成断言前缀和后缀独立一致。

### 附件准入（已完成）

`attachment.rs`：`admit_image` / `admit_batch` / `detect_media_type` / `decode_base64_canonical`。

**两条属性承载正确性。**

**1. 媒体类型必须从字节嗅探，不能信声明。** 浏览器按扩展名报告类型，所以一个合法的
`.jpg` 上传可能实际是 PNG。信任声明会让存储层存下一个改名的 ZIP，而后续渲染它的东西
可能执行而不是显示。

**2. base64 必须是 canonical 形式。** 同一份字节有多种拼写（padding 变化、尾部垃圾），
两种拼写就是两个存储 blob——既让去重失效，也让长度检查可被绕过。
校验方式是**解码后重新编码再比较**。

**批量上限是聚合的，不只是逐项。** 只有单图上限时，二十张各自合法的图仍可一起耗尽内存。
计数和总字节在任何东西被存之前检查，所以被拒的批次不留痕迹。

**存储 id 是路径段。** 只允许 `[A-Za-z0-9_-]`，不安全就拒绝而不是转义——同一个 id 的两种拼写
会是两个 blob。文件名（`name`）只是显示字符串，从不被当作路径解释，测试覆盖了这一点。

**未完成**：`llm` 的多 provider、`session` 的其余 44 个事件类型、
`context` 的其余子包、**压缩的摘要器**（需要 provider）。
