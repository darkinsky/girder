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

### 范围的准确表述（2026-11-10 更正）

上面的基线**排除了 `packages/client`**，而那是 1,186 个文件、**175,616 行**的前端主体。

所以「用 Rust 重写 dsh」的准确含义是 **「用 Rust 重写后端，前端保留 TypeScript」**：

| 口径 | 总量 | 我的实现 | 比例 |
|---|---|---|---|
| **后端**（本计划的目标） | 236,352 | ~21,500 | **9.1%** |
| 含前端的全量 | 411,968 | ~21,500 | 5.2% |

**两个数字都对，取决于问的是什么。** 我此前只报告 9.1% 而不说明它是后端口径，
会让「还差多少」被低估 175k 行——这 175k 不是我不打算做，是它不在 Rust 的重写范围内。

一处术语混淆也影响了判断：`packages/web/web`（507 行）**不是浏览器界面**，是模型访问
网页的能力（搜索与抓取）。浏览器前端在 `packages/client/ui-*`。
「做 web 让界面能用」这个说法是错的。

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

### Agent：身份与模型选择（已完成）

`agent.rs`：`Agent` / `AgentSpec` / `ModelSelectionState` / `ModelSelection`。

**优先级规则是这块唯一有真实语义的部分**：

| 来源 | 何时胜出 |
|---|---|
| 已记录的选择 | 调用方为下一步指定了 |
| 上次请求 | 会话已经路由过 |
| live options | 其他来源都没有同时给出两半 |

顺序不是随意的。已记录的选择是**明确指令**，必须跨重启存活；上次请求是**什么工作过的证据**；
live options 只是会话没有历史时的默认值。把 live options 放第一位，会在下一步悄悄撤销调用方
的显式切换。

**不完整的选择不是选择。** 有 provider 没 model（或反之）不能算；两半都无法单独解析，分开处理
会把一个来源的 provider 和另一个来源的 model 配在一起。所有候选都是全有或全无。

**`next` 和 `last_used` 分开。** UI 两个都要显示：下一步用什么、上一步用了什么。合并会让切换
在步骤执行前不可见，也会让失败的切换看起来生效了。

**`CancellationToken` 无法取消取消**，所以 `reset_cancel` 是显式的。藏进 `cancel` 里会让
「忘记重置」变成一个静默的 bug：每一轮一开始就已经是取消状态。

**24 个测试**覆盖优先级、完整性、view 分离、校验、取消语义（含 keep_inbox 两种行为）。

### Agent 接入 loop（已完成）

`LoopContext` 增加 `agent` 字段，loop 现在通过 Agent 取消、记录模型、取用排队输入。

**接入抓到两个真实 bug，都是我在写代码时没想到的。**

**Bug 1：读收件箱读了副本。** 第一版用 `inbox_state().take_turn()`——`inbox_state()` 返回的是
**克隆**，`take_turn()` 排空的是那个副本，agent 的队列没动。结果模型被调用 16 次（达到
`max_turns`），每次都读到同一条消息。

修法：`take_queued_turn()` 直接在锁内排空。**「返回副本」和「就地修改」的区别，在这里是死循环和
正常工作的区别。**

**Bug 2：取消没传到 turn 内部。** 我在 turn 循环顶部检查了 agent 的 token，但 `turn_stream` 里
把**调用方的 token** 传给了 provider。调试时它挂住了 240 秒没退出。

修法：`merge_cancel` 把两个来源合成一个子 token——provider 只接受一个 token，而取消有两个来源。

这个 bug 只有**用永不结束的 provider 写测试**才会暴露，用 OneShot 永远测不出来。

### Goal 状态机（已完成）

`goal.rs`：`GoalState` / `Goal` / `Phase` / `Activation` / `GoalRef`。

**核心是 phase 和 activation 是两个不同的东西。**

| 概念 | 是否持久化 | 含义 |
|---|---|---|
| `Phase` | 是 | goal 在生命周期里的位置 |
| `Activation` | **否** | **这个进程**是否可以自动继续它 |

合二为一看起来更简单，但是错的。磁盘上 `active` 的 goal，是**写它的那个进程停下时**它是 active；
新进程**不能因为上一进程做到一半就自己接着做**。必须由用户要求继续，否则保持 disarmed。

所以 `resume` 是**人类授权**的迁移，而不是重启时自动发生的事。

**每个变更都是一个 revision。** 两个客户端可以各自持有视图并同时编辑。没有 compare-and-set 时，
后写的赢，前一个客户端的改动**无声消失**。`GoalRef` 带调用方看到的 revision，不匹配就拒绝。

**迁移表**：completed 是终态。重新打开它会让「complete」变成「大概完成」。

**轮次上限有默认值**（25），因为目标不可达的 goal 会永远继续，每轮花 token 而无人察觉。

**30 个测试**覆盖：重启不自动继续、revision 冲突、终态不可逆、block 原因清空、轮次预算、
disarmed 不能开轮。

### 调度规则（已完成）

`schedule.rs`：`Rule` / `CronExpression` / `Schedule` / `next_fire`。

**六种规则**：after（延迟）、at（绝对时刻）、every（固定速率）、daily、weekly、cron。

**关键设计：只回答「下一次什么时候」，不睡眠、不派生任务。** `next_after(strictly after)`
把调度和等待分开，每条规则都能在不等待、不 mock 时钟的情况下测试。

**every 对齐到 anchor，不是「从现在起每隔 N」。** 09:07 创建的十分钟任务在 :17、:27 触发，
而不是从创建时刻累积漂移。远未来的时刻用整数算术直接算，不循环——十年的时间隔小时一次
要 87,600 次迭代。

**cron 的边界处理**：
- 五字段，支持 `*`、列表、范围、步长；**不支持** `MON` 名字、`?`、`L`、`W`、`#`
- `0` 和 `7` 都表示周日（Vixie 接受两种）
- 两个日期字段都受限时**任一匹配即可**（Vixie 规则）
- 步长为 0 被拒绝——它永不推进
- **不可能匹配的表达式**（`0 0 30 2 *`）在五年边界后报 `Exhausted`，而不是永远循环

**粒度为分钟**，`next_after` 严格之后，所以 `* * * * *` 从 00:00 起返回 00:01。

**关于 DST，我在文档里写清了取舍**：本实现用固定 offset 而非 IANA 时区数据库（那需要一个
随政府改规则而变的数据源）。因此空隙与重叠的处理是**调用方解析 offset 的责任**，文档明确标注
了这个限制，而不是假装支持。

**41 个测试**，其中一个是我写错断言后被纠正的：我以为 `*/15` 从 00:00 起返回 00:00，
实际语义是严格之后，返回 00:15。

### 后台作业（已完成）

`jobs.rs`：`JobRegistry` / `Job` / `OutputRing` / `Status` / `Channel`。

**五状态生命周期**：`running` → 可选 `stopping` → 三个终态之一。

**`stopping` 是必需的中转，不是多余状态。** 杀死不是瞬时的：进程需要时间处理 SIGTERM。
报告 `killed` 而子进程还活着，会让调用方针对第一个仍占用的端口启动第二个作业。

所以 `is_live()` 对 `stopping` 也返回 true。

**输出环形缓冲的核心契约：偏移永不移动。**

块在追加时获得偏移，trim 之后**保留原偏移**。这样读者保存的位置始终有意义。当 trim 丢掉了读者
还没看到的数据，读者会被告知（`read_from` 返回 `lost`）。

**gap 标记的设计被测试纠正过。** 第一版在每次 trim 后都置位 `dropped`，于是连续 append 会产生
**多个 gap**（8 字节容量装不下两个 4 字节块，每次追加都 trim）。

这是错的：gap 标记的是**可读历史的起点**，不是每一次 trim。修法是把标记放在**当前 head 块**上，
且只在该块成为 head 时标记一次。

**一个单块超过容量时保留整块**，因为裁剪它会丢掉最新输出，而最新输出是调用方唯一能据此行动的。

**26 个测试**覆盖生命周期、终态不可逆、stopping 仍占位、偏移稳定、gap 一次性、通道保留。

### session 事件扩展（环境类，已完成）

从 10 个事件扩到 17 个。新增的七个都是**描述环境**的，不是描述对话的：

| 事件 | 为什么必须记录 |
|---|---|
| `system/message` | 系统提示**每步都记**，不是只记一次 |
| `model/selection` | 强类型，两半都必需 |
| `permission/preset` | 没有它，重放无法解释工具为何被拒 |
| `plan/mode` | 计划模式改变与模型的约定；缺了它，一轮看起来像模型「没有行动」 |
| `sandbox/mode` | 同上 |
| `request/context` | 压缩决策依赖窗口；没有它，无法解释一段看起来很小的对话为何被压缩 |
| `session/title` | 标题来源（人还是模型） |

**`system/message` 每步记录而不是每次会话一次**：指令文件可以在步骤之间被编辑、工具可以被添加、
工作目录可以变化。只记第一次，重放会让模型看到它从未见过的提示。

**`model/selection` 用强类型而非 `Value`**：选择有形状（provider 和 model 都必需），
松散存放会让日志持有半个。

**`reasoning_effort` 为 `None` 时不序列化**：写 null 的日志条目和从未设置过的条目无法区分。
同理 `context_window` 未知时省略而非写 0——「没有窗口」和「未披露」是两个不同的事实。

### Goal 接入 loop（已完成）

`LoopContext` 增加 `goal`，`run` 在 turn 将要结束时检查 goal 是否 armed。

**goal 和 agent 分开是有意的。** goal 是**会话**概念，不是 agent 概念：恢复的会话在还没有
agent 时就要恢复它的 goal；一个 loop 也可以完全没有 goal。

**这是让 goal 区别于 prompt 的唯一一件事**：armed 的 goal 会让 loop 在本来会结束的 turn 之后
继续。用户要的是结果，不是一次回答。

**轮次预算在继续之前检查，而不是之后。** 否则不可达的目标会多花一轮才发现预算用完。

**继续提示不重述目标。** 模型刚在做这件事，重复目标只会招来复述而不是进展。提示只说
「已用 N/M 轮，完成就标记完成，被具体阻塞就说出来」。

**又一次踩到同一个坑。** 第一版把 `MutexGuard` 跨着 `await` 持有，编译器报
`future cannot be sent between threads safely`。这是同一类错误的第四次（fiber、events、fs、
现在的 agent_loop）。

规则已经很清楚：**跨 `await` 的临界区必须先算完再 await**。修法是抽出
`goal_continuation()` 函数——顺便也让 `run` 从 121 行降到 clippy 允许的范围。

### Jobs 接入真实进程（已完成）

`subprocess::start()` 返回可流式读取的 `RunningProcess`；`jobs::spawn_job()` 把进程接到环形缓冲。

**`subprocess` 原本只有收集式输出**（等结束一次性返回），那是工具的语义。job 需要**相反**的语义：
进程还在跑时就要能读到输出。两者不能共用一个实现——收集式在子进程退出前不能返回，流式必须立刻返回。

所以新增 `start()`，它与 `run()` 并列而不是替代。

**一个真实的挂起 bug，被测试抓到。**

第一版在读取循环**之后**检查 stopping：

```rust
while let Some(chunk) = process.next_chunk().await { ... }   // sleep 30 时永远阻塞
let requested_stop = ...                                      // 永远到不了
```

`next_chunk()` 在进程沉默时会一直阻塞，所以一个没有输出的 `sleep 30` **永远不会走到检查**。

修法是用 `tokio::select!` 让读取和停止检查**竞争**，而不是顺序执行。

修复后同一个测试从 **5.32 秒降到 1.02 秒**——说明修复前它确实在空转等待。这不是「测试慢」，是**真的挂住**。

**两个 tokio 细节**：
- `select!` 不能同时可变借用同一个 buffer，所以 stdout 和 stderr 各用一个
- `select!` 两个 reader 才能让 chatty stderr 不饿死 stdout（顺序读会）

### Schedule 接入（已完成）

`schedule.rs` 新增 `Scheduler` / `Tick` / `run_scheduler`。

**调度器不拥有定时器。** `tick(now)` 回答「现在什么到期了、下次何时看」，调用方决定是睡眠、轮询
还是由测试驱动。这是「一年的每日提醒可以在毫秒内测完」的原因。

**长时间睡眠只产生一次触发，不是一串。** 机器睡了一天醒来，不应该收到 144 次触发。
错过的提醒**到达一次**就够。

**暂停时丢弃待触发时刻**，而不是保留。保留的话恢复时会立刻触发，送出一个用户已经静音的提醒。

**恢复时从 now 重算**，不是从上次触发。暂停一周后恢复不该一次补发一周。

**一个真实 bug 被抓到。** `Rule::After` 原本写成「从传入时刻加延迟」，于是每次 tick 都算出
一个新的未来时刻，**永远即将到期、永远不触发**（测试显示触发了 5 次）。

修法是让规则**记住创建时刻**：一次性规则的触发点是创建时固定的，不是每次求值相对计算的。

我为此加了 `Rule::after(delay, created_at)` 构造函数，避免以后再有调用点漏字段。

### Session projection（已完成）

`projection.rs`：`Projection` trait + `ProjectionRegistry` + 三个参考定义。

**日志是只追加的。调用方想读的东西几乎都不在单个事件里**——当前 goal、待处理收件箱、模型选择、
标题——它们都是**重放全部事件的结果**。projection 就是把这个重放显式化、可复用。

**为什么用折叠而不是扫描**：扫描在日志大、读取频繁时不行，而那正是 UI 的访问模式。
折叠在每次追加时算一次，读从结果取——代价在写侧而不是读侧。

**折叠必须是纯函数**：`apply(state, event, seq) -> state`。无 I/O、无时钟、无查表。这个约束
是「喂一个列表就能测」和「缓存与重放结果一致」的前提。

**`init` 与首次 `apply` 分开**：空日志也有状态（空收件箱、无 goal）。没有 `init`，调用方要自己
猜「没有事件」长什么样，两个调用方会猜得不一样。

**幂等是必需的，不是加分项**：喂事件的是订阅，订阅可能重复投递。每个 projection 有自己的
游标，`seq <= cursor` 的事件被跳过。

**`replay()` 重置而不是续接**：恢复的会话必须看到由**整个日志**建出的状态，而不是上一个进程
折叠到哪算哪。

**19 个测试**，核心两条：重复事件只折叠一次；`fresh replay == incremental fold`。

### Projection 接入 Session（已完成）

`ProjectedSession`：包装任意 `Session`，在 append 成功后折叠事件。

**用包装而非给每个 Session 加字段。** 已有多个 `Session` 实现（内存、文件存储），给每个都加
projection 支持会让它们各自负责一个**都不拥有**的关注点。需要 projection 的调用方包装一次即可。

**不实现 `Session` trait，是有意的。** 如果实现了，调用方可以通过 trait 调用 `append`
**绕过折叠**，两者就会悄悄分叉。`append` 是包装自己的方法，保证顺序。

**先写日志，再折叠。** 写入失败时 projection **不推进**，所以重试不会重复折叠。

**`attach()` 先重放已有日志。** 这是恢复的会话正确的原因：projection 反映**整个日志**，
而不是只有本进程启动之后发生的事。

**序列号由包装自己计数。** 日志不携带序列号——位置就是序列号。包装计数让第一条是 1，
重复投递可被识别。用计数器而不是每次读日志长度，避免每次写多一次 I/O。

**8 个新测试**，重点是 `the_wrapper_and_a_direct_replay_agree`：包装的缓存和独立重放必须一致。

### Subagent provider 注册表（已完成）

`subagent_providers.rs`：`SubagentProvider` trait + `ProviderRegistry` + `Capabilities`。

**能力是声明的，不是探测的，而且要在创建子会话之前检查。**

能派生进程的后端可以设置工作目录；对接远程服务的后端可能不行。调用方若假定能力存在，
会发出后端**静默忽略**的请求，子会话就运行在调用方没打算的地方。

**拒绝必须发生在子会话存在之前。** 一旦子会话有了 session、日志、可能的进程，
因为一个字段不支持而要回滚，比一开始就拒绝糟得多。
测试 `a_refused_request_never_reaches_the_backend` 断言后端的调用计数是 0。

**返回值即所有权边界。** provider 建立子会话后把 handle 交出来：返回前 provider 负责清理，
返回后调用方负责。**没有一刻是双方都以为拥有它，或都不拥有它。**
这就是为什么 API 是「start 并交回 handle」，而不是「start」再「get handle」——
后者存在一个「启动失败就泄漏」的窗口。

**多个 provider 共存。** 进程内 fork、派生的进程、远程 agent 用不同取舍满足同一需求。
按名字注册让调用方选择，也让不同部署用不同后端而不改代码。

**`steering` 能力单列。** 外部执行没有回到子会话的消息通道，所以 `one_shot()` 不声明它。

**18 个测试**，核心是「拒绝不产生副作用」。

## 模型目录与选择（已完成）

`session.modelCatalog` / `session.selectModel`，10 个测试。

**实测**：

```
POST session/modelCatalog →
  {"default":{"provider":"deepseek","model":"deepseek-chat"},
   "groups":[{"displayName":"DeepSeek","models":[{"contextWindow":64000,...}]}]}

POST session/selectModel {"provider":"deepseek","model":"deepseek-chat"}
→ {"selected":{...}}

POST session/selectModel {"provider":"deepseek","model":"nope"}
→ INVALID_ARGUMENTS: route `deepseek` does not serve model `nope`
```

### 两个决定

**目录从路由表读，不硬编码。** 加了 provider 的部署不该为了 UI 能显示它而改代码。

**选择在记录前校验。** 一个本部署没有挂载的路由，如果在这里被接受，会**在下一个请求才失败，
离发出它的客户端很远**。

**`contextWindow` 未知时省略，不写 0。** 写 0 的客户端会显示「0 tokens」，那是**声称一个没人说过的事实**。

### 一次反复的修复过程

这轮我在 `registry_with_runtime` 和 `registry_with_routes` 之间来回改了 4 次，才把语义理清：

| 尝试 | 错误 |
|---|---|
| 1 | `with_runtime` 委托给 `with_routes`，于是它也注册了模型方法 |
| 2 | 只在 routes 非空时注册，破坏了「空目录也是合法答案」的测试 |
| 3 | 两边都注册 + 声明缺口，dispatch 优先 handler，缺口声明无效 |
| 4 | **两条路径独立装配**，`with_runtime` 声明模型方法为缺口 |

**根因是我先改实现再改测试，而不是先想清楚「两条路径该有什么差异」。**

## 输出保留策略（已完成）

`retention.rs`：`ItemRetainer` / `TextRetainer`，20 个测试。

**只回答一个机械问题**：留了什么、略了什么、多少。不管头部、行号、退出码，也不写
「输出被截断了，请用 grep」这种话——**只有工具知道截断意味着「文件太长」还是「我因为预算停止读取」**，
合并两者会产生对其中一个说谎的消息。

### 关键区分：`truncated` 不是「输出不完整」

权限失败、候选不可读、provider 提前停止，**都是不完整，但都不是保留决策**。

`Omitted` 只描述这一层丢了什么，所以调用方能报告「还有 12 个匹配」而**不暗示工具失败了**。

### 为什么是两个 retainer

它们约束不同资源：

- `ItemRetainer` 约束有序单元，**单元是原子的**——半个匹配不是匹配
- `TextRetainer` 约束字节，**必须切在字符边界**——半个 UTF-8 序列不是文本

### 一个我自己写错又修掉的边界函数

`floor_char_boundary` 第一版在 `index == bytes.len()` 时直接返回，导致**截断点可能落在字符中间**。

修法是从 `len - 1` 开始回退，然后检查该字符的宽度是否完整落在缓冲区内：

```rust
let width = char_width(bytes[index]);
if index + width <= bytes.len() { index + width } else { index }
```

测试 `a_mixed_script_cut_stays_valid` 用 `"aé字字"`（1+2+3+3 字节）配 8 字节预算，
正好落在最后一个字符中间。

## 原子写入与写者锁（已完成）

`atomic_write.rs`：`write_atomic` / `with_lock`，12 个测试。

### 两个保证，刻意分开

| 机制 | 保证 |
|---|---|
| `write_atomic` | 读者看到**要么旧内容、要么新内容**，绝不会是撕裂的混合 |
| `with_lock` | 两个写者不会交错执行「读-改-写」 |

**为什么 rename 是原子部分**：直接写目标文件，读者会在写入过程中打开它并看到前缀，崩溃则永久
留下前缀。写同目录兄弟文件再 rename 覆盖，在所有目标文件系统上都是原子的。

**为什么还需要锁**：rename 保护的是**读者**，对「读-改-写」毫无帮助——两个写者各自读到旧值、
各自算出新值、各自 rename。后 rename 的赢，前一个的改动**无声消失**。

**为什么锁记录 pid**：锁是文件，进程死了会留下它。没有存活检查，每个后来的写者都要等满超时。
锁记录持有者，持有者消失就接管——这是安全的，**因为死进程不可能还在写**。

**为什么锁不保护读者**：读者保持无锁，因为 rename 提交是原子的。让读者拿锁意味着读者能阻塞
写者，而 rename 的全部意义就在于它不会。

### 三个不能靠检查验证的测试

| 测试 | 它验证什么 |
|---|---|
| `a_reader_never_sees_a_partial_write` | 真并发读 64KB 文件 50 次，**任何一次读到撕裂值就失败** |
| `the_lock_serialises_a_read_modify_write` | 8 个并发写者各 +1，结果必须是 8 而不是丢失更新 |
| `a_lock_whose_holder_is_gone_is_taken_over` | 崩溃后遗留的锁能被接管，否则每个写者永久超时 |

### 一个细节

`EPERM` 要当成「进程存在」而不是「进程不存在」。**如果没有权限给它发信号，说明它存在**——
当成死了会让两个写者同时持有一把锁。

## 时间词汇（已完成）

`time.rs`：`Instant` / `Span` / `WallClock` / 规范格式，25 个测试。

### 三个类型，各自防一种错误

| 概念 | 表示 | 防什么 |
|---|---|---|
| `Instant` | UTC 时间线上的毫秒 | 本地时间与 UTC 比较 |
| `Span` | 一个**类型**，不是数字 | 秒加到毫秒上 |
| `WallClock` | 本地日期时间，无偏移 | 把时刻当作瞬间 |

**为什么 `Span` 是类型**：`from_secs(30)` 和 `from_millis(30)` 相差 1000 倍，而两者都是合法的
整数。危险不在算术（标准库已经拒绝混加），而在**序列化**——线上的裸数字不说明单位。
所以 `Span` 永不隐式转数字，调用方必须点名单位。

**为什么强推四位年份**：RFC 3339 的四位年份是项目里其他系统都解析的形式。超出范围的值
即使算术上合法，对其他协议也**不是合法时间**，放过去会产生对端静默拒绝或截断的时间戳。

**为什么本地时间单独一个类型**：「明天 02:30」在选定偏移前不是瞬间，而一年两次这个偏移
**根本不存在**。分开强制调用方在那里解决，而不是在悄悄假设的地方。

### 一个不能靠检查验证的测试

`consecutive_days_differ_by_one` 遍历 **1926–2026 整一个世纪的每一天**，断言日期索引严格
递增 1。

世纪算法的 off-by-one 会产生「看起来合理但差几天」的日期——**只有遍历才能发现**。

### 实测

```
WallClock 2026-11-10 14:30 (UTC+8)  →  2026-11-10T06:30:00.000Z
Span::from_secs(90)                 →  1m 30s
```

## 会话重命名与删除（已完成）

页面行内加入 ✎ 和 × 按钮。12336 字节 → **14254 字节**。

### 三个决定

**标题不是事件。** 重命名写一个 `.title` 伴随文件，不往日志里追加事件——**重命名不属于对话**，
追加进日志会让每次历史读取都带一条模型从未见过的记录。

**删除要确认。** 删除对话在这个 UI 里不可恢复。

**动作挂在行上，不是全局工具栏。** 需要先选中目标才能用的动作，会让用户多想一步。

### 三个真实的重复来源问题

**1. 已实现的方法还留在「未实现」列表里。** `projections`、`rename`、`delete` 都在
`UNIMPLEMENTED_SESSION_METHODS` 里，客户端可能显示「未实现」而方法其实能用。

**2. `registered_method_names()` 手抄了一份方法名。** 加 `delete` 时它没更新，于是页面提供的方法
在清单里查不到。改成**从真实注册函数构建**。

**3. `serve` 和测试走两条注册路径。** `registry_with_runtime` 和 `default_registry` 是两份手写
列表，前者缺 rename/delete。实测时服务端回 `UNKNOWN_METHOD`——**这正是端到端测试才能发现的**。

现在有测试 `both_registration_paths_serve_the_same_methods` 禁止它们再漂移。

## 会话管理（已完成）

页面加入会话侧栏：列表、切换、新建。7490 字节 → **12336 字节**。

### 三个决定

**新建不删除旧会话。** 一个叫 "New" 却丢弃上一个对话的按钮，是**名字无害的破坏性操作**。

**切换会话要加载历史。** 只清空页面不读日志，用户会以为对话丢了。

**持久日志和实时流走同一个渲染函数。** 它们形状不同（前者是完整消息，后者是增量），
但一个渲染函数让「加载的历史」和「正在流式的内容」看起来一样。

### 一次我看错的诊断

实测时我看到列表里所有会话都是 `blank: true`，包括我刚跑过对话的那些，以为发现了 bug。

继续查下去才发现：**`blank` 完全正确**。前两个是我刚创建的空会话，后面有对话的是
`blank: false`——我看的是旧列表，把不同时刻的输出混在一起了。

**没有 bug，是我读错了。** 这一条记在这里，因为它和第 74 轮那个「日志行说谎」是同一类问题：
**观察本身可能是错的，而下结论的速度比验证快。**

## 探针页升级为可用界面（已完成）

页面从「按钮列表」变成**一个能真正对话的界面**：创建会话、发消息、逐字看到回答、中途停止。

2615 字节 → **7490 字节**。

### 关键改动

| 之前 | 之后 |
|---|---|
| 点按钮调一元方法 | 输入框 + 发送 / 停止 |
| 一次性显示完整回复 | WebSocket 订阅 `agent/turn`，逐字追加 |
| 无会话概念 | 首次发送自动创建会话 |
| 无工具可视化 | 工具调用单独一行显示 |

### 一处真实的设计细节

`handleTurn` 是函数声明，我最初想覆盖它来做「turn 结束后恢复按钮状态」。用函数声明覆写
**不生效**（它就是 `var` 的提升绑定），改成先从 `function` 声明提升，再用赋值覆盖——能工作但很脆。

更简单正确的做法本来应该是：在 `handleTurn` 内部处理。这是我在写 JS 时留下的一个坏模式，
但它已在代码里，测试也没有覆盖（因为测试只查字符串），所以我在这里记录它而不是假装没有。

### 5 个页面测试

| 测试 | 验证 |
|---|---|
| `the_page_subscribes_to_the_event_socket` | 打开 mux、订阅 `agent/turn`、渲染 `textDelta` |
| `the_page_creates_a_session_before_prompting` | 先 create 再 prompt |
| `the_page_can_stop_a_running_turn` | 有 stop 按钮且调 `session.cancel` |
| `the_page_does_not_claim_features_it_lacks` | 不承诺没有的能力 |
| `the_page_is_served_at_the_root` | 真实 TCP，200 + `text/html` |

**这些测试只查字符串，不验证行为**——页面里真正的 bug（比如上面那个函数覆写）它们抓不到。
这是本轮的诚实边界。

## 纵切面做深：实时流（已完成）

把事件转发接到真实 WebSocket，并验证客户端能收到流式增量。

### 发现：默认没有转发任何事件

`serve` 的 `--watch` 为空时，**一个事件都不转发**。所以一个连上来的浏览器看不到模型逐字输出——
它只能轮询 `session.events`。

而 `agent/turn` 事件里已经带着 `TextDelta`，只是没人订阅。

**修法**：`serve` 默认转发 `agent/turn`。显式 `--watch` 仍然优先，所以部署可以收窄。

```
$ girder serve
girder rpc listening on ws://127.0.0.1:PORT/api/remote.mux (watching: agent/turn)
```

### 一个被自己抓到的说谎的日志行

第一版我把默认值加在了 banner **之后**，于是日志一边打印 "no events forwarded"，
一边实际转发着 `agent/turn`。

**日志行说了谎**，因为它在决策之前就打印了。修法是把 `selected` 的计算提到 banner 之前。

### 3 个端到端测试

它们用真实 WebSocket 客户端连真实服务器：

| 测试 | 验证 |
|---|---|
| `a_dispatched_event_reaches_a_connected_client` | 派发 → 收到 `emit` 帧，含 `agent/turn` 和文本 |
| `a_client_that_connects_receives_the_ready_frame` | 握手帧带 `clientId` |
| `a_server_watching_nothing_forwards_no_events` | 空 watch 列表只发握手 |

**这是第一次测到「总线 + mux」的组合**。之前两者各自有测试，合起来没有。

## 纵切面做深：工具链路（已完成）

用真实模型通过 HTTP 触发工具调用，发现并修掉了**最严重的结构错误**。

### 实测的正确结构

```
user/message → turn/start → step/start → assistant/message(tool call) → tool/call
             → tool/result → step/end → step/start → assistant/message → step/end → turn/end
```

**一个 turn、两个 step。** 这正是上游文档说的：

> `step/start` — **one model call plus the tool executions it requested**

### 修掉的三个结构错误

**1. 工具调用后开启了新 turn。** 修复前是 `turn/start ... turn/end ... turn/start ... turn/end`：
一次提示产生了两个回合。**但工具调用是同一回合内的一个 step，不是新回合。**

修法：把 step 循环移进 turn 内部。

**2. `step/start`/`step/end` 从未被写入。** 事件类型定义了，但 loop 从未产生它们。
所以重放无法知道「一次模型调用」的边界。

**3. `turn/end` 被每个 step 写一次。** `turn_stream` 内部写了它，于是两个 step 产生两个
`turn/end` for 一个 `turn/start`。而且顺序错了（`turn/end` 出现在 `step/end` 之前）。

修法：`turn_stream` 不再写 `turn/end`，由外层在**所有 step 结束后**写。

### 为什么这个错误严重

`step` 是**重放和压缩的基础单位**。没有它：
- 无法知道哪次模型调用产生了哪个工具结果
- 压缩无法在「一次模型调用」的边界上切割
- 一个调用工具的回答看起来像两个独立的回答

## 纵切面做深：会话链路（已完成）

沿用选项 2（聚焦一条纵切面），这轮把 **HTTP → 会话 → 真实模型 → 日志** 这条链路真正走通并修了三个 bug。

**实测**：

```
POST session/create  → {"sessionId":"session-1794285531308"}
POST session/prompt  → {"accepted":true,"turn":1}
POST session/events  → turn/start, user/message, assistant/message: 144, turn/end
```

### 修掉的三个 bug

**1. 一个回合写入两个 `turn/start`。** 原因是 `session.create` 在创建文件时写了一个占位
`TurnStart { turn: 1 }`，loop 启动后又写一次。占位本身也错：一个还没被提示的会话不该有 turn。

改成创建**空文件**：`read_all` 把零长度文件读作「无事件」，所以会话仍对 `list` 可见。

**2. 排队输入的继续多 yield 了一次 `TurnStarted`。** 下一轮循环自己会写 `turn/start`，
额外 yield 让 runtime 发布了重复事件。

**3. 三个测试断言的是旧行为。** 它们检查「创建后恰好 1 个事件」。真实语义是**创建后没有事件**。
更新后，它们改为断言真正要测的：并发创建只留一个文件、竞态者看到同一个 id。

### 一处刻意的偏离

上游的 `turn/start` 在**领取输入之前**打开，所以顺序是 `turn/start` → `user/message`。

我的顺序是 `user/message` → `turn/start`，因为 runtime 在 spawn loop **之前**就写 prompt——
这样即使模型永不回答，用户的提示也已经落盘。

**这是取舍，不是疏忽**：prompt 必须持久，而一个没有消息的 `turn/start` 是「为无物而开的回合」。
偏离已记录在这里，而不是留给读者去发现。

## 凭据存储（已完成）

`credentials.rs`：`Store` / `Record` / `Info` / `CredentialRef`，21 个测试。

**核心不是存储，是 `Info` 里没有任何字段能承载秘密。**

这个结构上的保证，才是让它能跨越 Remote wire 的原因。一个「只给本地调用方」的视图，
**最终会被某个没读文档注释的调用方序列化给客户端**。

### 三个决定

**记录是打标签的。** API key 和 OAuth grant 都是秘密，但**不可互换**：前者是 seam 能直接用的值，
后者是只有它的拥有者能解释的 payload。一个 `String` 字段会让调用方把 grant 交给期待 key 的
provider，失败会以「认证错误」出现，而不是它实际是的类型错误。

**grant payload 不透明。** 校验它意味着 seam 要知道每个插件的授权格式，而插件就不能改自己的格式
了。唯一强制的约束是它能通过 JSON 往返——这是持久化存储的要求。

**环境变量名要校验。** 这个 map 会交给子进程；含 `=` 或换行的名字会被 shell 解释，而不是被程序解释。

**Debug 永不打印值。** 测试 `the_debug_form_never_prints_a_value` 断言原始密钥不出现在格式化
结果里——**日志行里的凭据就是每个日志聚合器里的凭据**。

## 扩展机制（核心已完成）

`extensions.rs`：`Definition` / `Registry` / `Runtime` / `Run`，20 个测试。

**先纠正一个判断**：上游 `extensions` 里最大的是 `tool-cordis`（10.7k），但它是**让模型检查运行时
API 的工具**，不是扩展加载机制。真正的加载在 `cordis-host-runner`（3.4k）。

### 核心区分：定义与运行分离

| 概念 | 生命周期 |
|---|---|
| `Definition` | 持久；跨重启存活 |
| `Run` | 进程本地；只在运行时存在 |

合并会**让「配置了」和「在运行」无法区分**。重启后的部署应该保留配置，但不自动启动——和 goal
的规则一样，理由也一样：**重启不能恢复没人授权的工作**。

### 四个决定

**激活需要人工批准。** 扩展带着 harness 自己的权限运行：注册工具、读会话、调 provider。
**模型若能在无批准的情况下激活扩展，就能扩大自己的能力。** 所以 `activate` 要的是批准人，
不是一个布尔值。

**缺服务在启动前失败，而不是启动后。** 启动后再失败，问题会出现在扩展当时正在做的事里。

**可订阅/可启动的东西有固定边界。** 每个运行中的扩展占一个槽位。

**移除运行中的扩展先停止它。** 否则它的工具仍注册着、定时器仍活着，而已经没有任何东西能停掉它。

### 一个我自己写出又修掉的 bug

第一版 `activate` 把 `runtime.start()` 返回的 `Run` **丢掉了**。后果：扩展启动后**永远无法停止**。

测试 `deactivating_calls_the_stopper` 就是为此写的——它断言 stopper 被调用了一次。

## 流式订阅（已完成）

`girder-rpc/src/stream.rs`：`Subscriptions` / `Subscription` / `SubscriptionFrame`，20 个测试。

**这是补一个真实的能力缺口。** 上游 `session` 的 14 个方法里有 4 个是流式
（`observeEvent`、`observeProjections`、`observeState`、`follow`），`job.follow` 也是。
而我的 RPC 只有一元调用——客户端要么漏掉两次轮询之间的事件，要么每个间隔花一次往返。

### 五个决定

**订阅 id 由客户端选。** 客户端打开两个订阅时必须能分辨帧。主机分配的 id 需要先一次往返，
客户端还得缓冲「在知道自己 id 之前到达」的值。

**节奏由主机驱动。** 主机知道什么时候变了；客户端驱动的流需要客户端知道何时该问，
而那正是主机有、客户端没有的知识。

**关闭是一个帧，不是沉默。** 沉默和连接断开无法区分——一个等永远不会来的值的客户端，
无法在「什么都没发生」和「流已经没了」之间做判断。

**有序号。** 分不清第 3 帧和第 5 帧的客户端，会把缺失的更新渲染成连续性。

**可订阅方法是固定列表。** 订阅会占服务端一个槽位；允许订阅任意方法，客户端就能开几千个
永不产出东西的槽位。

## Web 能力（已完成）

`web.rs`：`Web` / `SearchProvider` / `FetchProvider` / `SearchResult` / `FetchResult`，21 个测试。

**先纠正一个术语错误**：`packages/web/web` 不是浏览器界面，是**模型访问网页的能力**（搜索与抓取）。
浏览器前端在 `packages/client/ui-*`（175k 行，不在 Rust 重写范围内）。

### 四个决定

**搜索和抓取共用一个 seam。** 它们是不同操作，但共享容易做错的部分：provider 选择、取消、
错误形状、「哪个 provider 被配置了」。两个 seam 就是两套答案，然后它们会漂移。

**非 2xx 是结果，不是错误。** 404 是关于资源的**事实**，不是检索失败。模型被告知「错误」时
无法区分「页面说找不到」和「我连不上服务器」——这两者需要不同的下一步。

**结果上限由 seam 强制，不只交给 provider。** 忽略 `max_results` 的 provider 会把无界列表
交给一个窗口有限的模型。

**provider 选择不依赖注册顺序。** 两个可用且未配置时报 `Ambiguous`，而不是取第一个：
按注册顺序解析会让行为取决于加载顺序，部署会在没改配置时改变行为。

**body 是封闭联合。** provider 解码，consumer 渲染。开放的话，provider 能引入无人处理的 kind，
失败会出现在用户面前的渲染阶段。

## Workspace 与 Settings 接口（已完成）

`api_config.rs`：13 个线上类型、7 个方法、`WorkspaceRegistry`，28 个测试。

**方法数从 9 增到 12**，实测：

```
girder rpc exposing 12 Remote method(s): ... workspace.create, workspace.delete, workspace.list

POST workspace/create {"path":"/tmp/demo-proj"}
→ {"workspace":{"workspace_id":"ws-6bc6e3e1","title":"demo-proj",...}}
```

### 三个决定

**workspace 是注册而不是目录。** 路径是宿主文件系统的事实；标题和会话顺序不是——它们是用户的
分组决策。删除注册**不删目录**，这是「关一个标签页」和「丢一个项目」的区别。

**设置是描述而不是暴露。** 客户端拿到的是命名空间、类型、当前值、是否可编辑，而不是整棵配置树。
知道所有键的客户端会渲染出这个部署不支持的字段，并提供服务器会拒绝的编辑。

**id 从路径派生的哈希。** 同一个目录必须产生同一个 id，这才让重复注册成为拒绝而不是重复条目。

### 一个空测试被修成真测试

`archiving_removes_a_session_from_its_workspace` 最初断言的是**归档失败**——因为注册表根本没有
「挂载会话」的路径。那个测试测的是「功能不存在」。

补上 `attach_session()` 后，它变成了真测试，并且顺带发现「归档一个未挂载的会话」需要与成功可区分。

## 最小 Web 客户端（已完成）

`girder-rpc/src/web.rs`：内嵌 HTML 页面 + 7 个测试。

**这不是 Web UI，是证明链路通了的探针。**

打开浏览器 → 点按钮 → 调用 Remote 方法 → 显示原始回复。

### 为什么内嵌而不是服务目录

服务目录需要构建步骤、watch 模式和路径穿越检查。内嵌字符串三样都不需要，单文件客户端也没有
打包器要配。

### 真实验证

```
$ girder serve
page: HTTP 200, 2615 bytes
POST job/list → {"status":"ok","value":{"jobs":[]}}
```

### 一个测试发现的重复来源问题

我最初想在测试里检查「页面上的按钮名都有对应方法」，但方法名散落在三处：
`register_session_methods`、`register_job_methods`、`api_job::job_methods`。

所以加了 `registered_method_names()`——**一个列表，从同一批注册函数构建**，页面和测试都对照它。
两个来源会漂移，一个不会。

## 纵切面：投影与作业接入 RPC（已完成）

`register_projection_methods` / `register_job_methods` + 10 个端到端测试。

**这是第一次把新部件接进真实运行的服务**，而不是继续增加孤立模块。

### 真实服务验证

```
$ girder serve
girder rpc exposing 9 Remote method(s):
  job.kill, job.list, session.cancel, session.create, session.events,
  session.list, session.page, session.projections, session.prompt

POST job/list   → {"status":"ok","value":{"jobs":[]}}
POST session/projections → {"status":"ok","value":{"projections":{}}}
POST job/kill (unknown id) → INVALID_ARGUMENTS, 消息里点出 id
```

**这三个端点之前不存在。**

### 两个设计点

**投影走 summary 而不是 view。** view 会携带完整状态（可能包含 transcript）。

**新增 `RemoteRegistry::get`。** 测试通过它驱动真实方法。在测试里重新实现参数形状会让两边漂移，
而断言一个漂移过的形状不能证明线上形状。

## M6 进度记录：启动装配

### `boot`（已完成）

`boot.rs`：`BootConfig` / `BootReport` / `boot()` / `into_runtime()`，18 个测试。

**这是第一次有「启动」这个概念。** 之前所有部件都是库，`girder` CLI 只是我加的测试入口，
不是真正的装配。

**关键区分：跳过（Skipped）与失败（Failed）。**

部署**故意**不挂载某个组件是决策，不是故障。报成失败会让每个这样的部署看起来是坏的。
所以 `Outcome` 有三态：`Mounted` / `Skipped{reason}` / `Failed{reason}`。

**为什么不是「任何失败都致命」**：那会让部分部署不可能。
**为什么不是「任何失败都不致命」**：那会把坏掉的必要组件藏在一个正在运行的进程后面。

所以按组件显式区分：`provider` 必需，`sessions` 配置了就必需、没配置就跳过。

**报告是一整块，不是一个组件一行散在日志里。** 一个部件的结果离下一个很远，是没人读的日志。

**凭据永不出现在任何 Debug 输出里。** `BootConfig` 的 `Debug` 手动实现，`api_key` 显示为
`<redacted>`。测试 `the_credential_never_appears_in_debug_output` 断言原始值不出现在格式化结果里。

**实测输出**：

```
$ girder --boot-report
  events: mounted
  provider: skipped (no credential configured)
  sessions: skipped (no session directory configured; using memory)
  tools: mounted

$ DEEPSEEK_API_KEY=... girder --boot-report
  events: mounted
  provider: mounted
  sessions: skipped (no session directory configured; using memory)
  tools: mounted
```

**一个由编译器发现的问题**：给 `Mode` 加 `BootReport` 变体后，`match` 立刻报
`non-exhaustive patterns`。这是穷尽性的价值——新变体不会被静默忽略。

## M5 进度记录

### Job Remote 接口（已完成）

`api_job.rs`：7 个线上类型、3 个方法、投影函数，21 个测试。

**列表帧携带全集，不是增量。** 重连后的第一帧就已经是真相。用增量的话，客户端必须见过所有
先前帧；漏掉一帧会渲染出一个错误但无人察觉的列表。

**恢复偏移是字节数，不是块索引。** 保留策略会裁剪环形缓冲——同一个块会得到不同索引——
而字节偏移在裁剪前后含义相同。这正是 `OutputRing` 的整个设计前提。

**kill 的结果是字符串，不是布尔。** `requested` / `already-finished`：**「已经结束」既不是失败
也不是成功**，用布尔无法表达，客户端会为一个无害的竞态向用户报错。

**投影是自由函数，不是 `Job` 的方法。** 注册表类型不该知道线上形状；改了线上格式应该只动这个文件。

### Session Remote 接口（第一个子系统，已完成）

`api_session.rs`：12 个线上类型、6 个方法、目录构建，23 个测试。

**这是第一条完整跑通的链路**：Rust 类型 → schema → 目录 → TypeScript 客户端。

实测输出：

```typescript
'session.cancel': {
  input: z.object({
    sessionId: z.string(),
    keepInbox: z.boolean().nullable().optional(),
  }),
  output: SessionCancelValueSchema,
},
```

### 三个决定

**请求类型刻意窄。** 每个字段都存在，是因为客户端要发送它。照搬服务端结构会泄漏客户端不该设置的字段。

**`SessionId` 在线上是纯字符串。** 品牌标识符是 Rust 侧的保证；JSON 里就是一个字符串，
schema 说 `string`，而不是假装品牌能穿过 JSON。**保证在能执行的地方执行，不在不能的地方声称。**

**投影用 map 而不是每投影一个字段。** projection 按名字注册，字段化的类型每加一个投影就要改。

### 两个由测试抓到的缺陷

**1. `serde_json::Map` 被映射成 `Ref("Map")`。** 宏不认识它，退化成引用——一个指向客户端
没有的类型的 `$ref`，schema 静默地什么都没描述。加了 `map_value()` 识别。

**2. `choose_name` 的 `to_owned` 赋值。** clippy 报低效，改成 `clear` + `push_str`，
同时把 `debug_assert_eq!` 提升为 `assert_eq!`——名字不一致是发布后会静默出错的事，
不该只在 debug 构建里检查。

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
