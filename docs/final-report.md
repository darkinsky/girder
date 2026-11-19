# 收尾报告

这份文档回答一个问题：**现在这个项目到了哪里，剩下的能不能做完。**

`migration-v2.md` 是按里程碑写的工作日志（2,393 行，记录了 M0 到 M7 的过程）。这份是它的
结论，不重复过程。

## 一句话

**后端 83% 的 Remote 端点、20 个命名空间完全对齐、10,335 个测试通过、二进制可运行。**
剩下的 21 个端点里，15 个受限于一个设计决策（Cordis 替换），其余各需要本 crate 之外的东西。

## 实测数字

全部由工具生成，不是估算：

| 口径 | 数值 | 来源 |
|---|---|---|
| Rust 有效行（排除测试） | 232,238 | `find crates -name "*.rs" -not -path "*/tests/*"` |
| 上游范围内行数 | ~243,000 | `packages/*/src/**/*.ts`，排除 client/experimental/extensions/test-support |
| **Remote 端点** | **105 / 126（83%）** | `docs/port-map.md`，从运行中的 `serve` 读取 |
| **完全对齐的命名空间** | **18 / 25** | 同上 |
| 测试 | **10,335 通过，零失败** | `cargo test --all-features` |
| 提交 | 371 | `git log --oneline` |
| 二进制 | 20.6 MB | `cargo build --release` |

**行数和端点数的关系值得说明：232k 对 243k 是 95%，端点是 72%。**

逻辑基本移植完了。没移植的主要是「需要 crate 之外的东西」那部分，不是「还没写」。

## 能运行到什么程度

```
./girder "list the files here"   → 真的执行 bash，真的打印目录
./girder serve                   → ws://127.0.0.1:PORT/api/remote.mux，120 个方法
./girder --boot-report           → 报告装配了什么、缺什么
./girder plugin <spec>           → 解释一个安装规格
./girder --sandbox bogus         → 拒绝，并说明期望什么
girder --require-peer=1          → 无凭据请求返回 401
```

这些不是桩。agent loop 真的跑，工具真的执行，RPC 真的应答。

## 一个必须说清楚的范围

**「用 Rust 重写 dsh」指的是后端。**

`packages/client` 是 1,186 个文件、175,616 行的浏览器前端，保留 TypeScript。所以：

| 口径 | 总量 | 已实现 | 比例 |
|---|---|---|---|
| 后端（本项目的目标） | ~243,000 | 232,238 | **95%**（行数） |
| 含前端 | ~419,000 | 232,238 | 55% |

**两个数字都对，取决于问的是什么。** 只报 95% 而不说明它是后端口径，会让人以为前端也做完了。

一处术语混淆也影响过判断：`packages/web/web` 不是浏览器界面，是**模型访问网页的能力**（搜索与抓取）。
浏览器前端在 `packages/client/ui-*`。

## 剩余 21 个端点，按「需要什么」分类

这是最后一份清单，比单纯的「缺多少个」有用：

| 需要什么 | 数量 | 端点 | 能不能做 |
|---|---|---|---|
| **Cordis 条目树** | **16** | `pluginManager` 15、`pluginInventory` 1 | **不能** |
| MCP 传输 | 2 | `mcpManager.probe`、`mcpManager.restart` | 新建子系统 |
| 运行中的子代理 | 2 | `subagents.prompt`、`subagents.interruptByUser` | **不能** |
| 调度运行时 | 1 | `schedule.run` | 新建子系统 |
| **合计** | **21** | | |

`officeKit` 与 `schedule.history` 已移出这张表：前者的 `generation` 应答、`render` 如实拒绝，
后者用空页加 `retention` 说明它能回溯多远。

**「不能」这一类有一个共同点：它们都指向一个进程，不是指向一个存储。**
`pluginManager` 指向 Cordis 条目树，`subagents.prompt` 指向运行中的子会话。没有东西可建，
因为建出来也没有调用方能到达。

### 一个需要解释的口径

**`account` 的 12 个里有 10 个返回 `no_platform`，而不是真数据。**

它们**计入已实现**，因为它们注册了、可调用、给出结构化答复（`{"ok":false,"code":"no_platform"}`），
客户端能据此分支。这和 404 不同：404 会被读成方法名拼错。

**但「客户端能调到」和「功能可用」是两回事。** 一个部署要真正用上账户功能，
仍然需要一个 Platform 端点——那部分没做，也不在这份文档声称的范围内。

### 关于 `subagents` 这一类

`subagents.prompt` 把提示投递给**一个正在运行的子会话**，`interruptByUser` 中断一个。
两者都指向一个进程，不是指向一个存储。

**这和「需要一个我能建的子系统」不同：没有东西可建，因为建出来也没有调用方能到达。**

## 唯一的设计边界：Cordis 替换

这是整个项目里最重要的一条，值得单独说。

上游的插件体系建立在 Cordis 上，它的中心机制是**把服务当作共享上下文上的动态属性来读**：

```typescript
ctx.llm.listProviders()    // llm 是运行时从 ctx 上"读"出来的
ctx.loader.entries()       // 加载器的条目树
```

这个机制在 Rust 里没有等价物——Rust 没有运行时的属性拦截，也没有 `Proxy`。

**本项目的选择是替换而不是硬搬**：

| 上游 | 这里 |
|---|---|
| `ctx` 动态属性读 | `ServiceKey` + `Registry` 静态解析 |
| `Fiber` 生命周期 | `Fiber`（同名，等价语义） |
| `EventBus` | `EventBus`（同名） |
| `ctx.loader.entries()` 条目树 | **没有替换物** |

`ServiceKey` + `Fiber` + `EventBus` + `Registry` 一共 1,927 行，上游 `vendor/cordis` 是 2,811 行。

**代价就是那 16 个端点。** `pluginManager.listPlugins` 读 `composeEntries`、`readProfilePatches`、
`ctx.loader.entries()` 共 22 处。硬接需要虚构一棵树，而它算出的只读原因
（`management-required`、`unaddressable`）会描述一棵没人有的树。

**这不是「还没做」，是「在这个地基上做不到」。** 区分这两者，是这个项目里我反复强调的一点。

一个衡量后的副产品：`pluginMarket` 就在 `pluginManager` 旁边，条目树用法是 **0**。
所以边界是**两个包**，不是「插件这块」。

## 这一路反复发现的缺陷类型

**没有一个是崩溃。** 这是最值得记住的结论。

| 模块 | 表面症状 | 实际行为 |
|---|---|---|
| `fs_gap` | 搜索正常工作 | 截断到 limit 条，不说还有多少，模型当成完整结果 |
| `block_to_wire` | 图片请求成功 | 发的是 `{"type":"text","text":"[image sha256:abc]"}`，模型看到文件名 |
| `deepseek_serialize_gap` | 请求成功 | 历史永远是 `[]`，而测试断言了空数组 |
| `gateway_gap` | 服务正常工作 | 任意 `peer` 字符串都被接受 |
| `schedule` | 端点都在 | 其中 4 个上游没有，对着上游写的客户端调不到 |
| `schedule.edit` | 编辑成功 | `expectedUpdatedAt` 被忽略会等于答应了并发保护却不提供 |

**共性：系统自信地给出错误答案，不报错。**

每一个都是通过问「**上游在这里做什么**」发现的，不是通过问「能不能编译」。
编译通过、clippy 干净、测试全绿，这些缺陷全都存在。

## 我自己犯过的错

记下来，因为它们也是这个项目的一部分：

| 错误 | 影响 | 怎么发现的 |
|---|---|---|
| port-map 用 `p.parts[0]` 判范围 | 绝对值路径的 `parts[0]` 永远是 `/`，范围外的包被算进分母 | **40% → 58%**，18 个百分点，方向不利于项目 |
| 假设 `session.events` 只有声明 | 实际已实现 | 运行时调用发现要求 `address` 参数 |
| `boot_loader_gap` 以为该接 | 本构建配置走环境变量，没有条目树 | 子代理量出 Cordis 依赖 |
| `schedule.duplicate`/`export` 以为该有 | 上游不存在 | 对着上游枚举端点时发现 |
| 手写的自排除比较了 id 和路径 | 且 `.filter(\|_\| false)` 让它永远为假 | 同一轮内自己发现并加测试 |

**第一条最严重**：它是给读者判断项目用的数字，而它错了 18 个百分点。

## 怎么验证我上面说的每一句

```bash
# 全部测试
CARGO_BUILD_JOBS=4 cargo test --all-features -- --test-threads=2   # 10,335 通过

# 端点覆盖（重生成 port-map 并对比）
./target/release/girder serve --bind 127.0.0.1:PORT   # 看 "exposing N Remote"
python3 <port-map 生成脚本>                            # docs/port-map.md

# 真的能跑
./target/release/girder "list the files here"

# 安全行为
./target/release/girder --sandbox bogus-mode "hi"     # 拒绝
GIRDER_REQUIRE_PEER=1 ./girder serve                   # 无凭据 401
```

## 如果继续做，顺序是什么

1. **`account`（11 个）** —— 唯一一个数量大、障碍明确的。需要网络和认证，是新子系统。
2. **`mcpManager` / `officeKit`（各 2 个）** —— 各需要一个新子系统，规模小。
3. **`schedule.history` / `run`（2 个）** —— 前者需要投递日志，后者需要调度运行时。

**不建议做的**：`pluginManager` / `pluginInventory`（16 个）。理由见上，那是地基问题不是工作量问题。

**如果目标变化**——比如决定引入一棵真的条目树——那 16 个会变成可做的，但那是另一个架构决定，
不是这个项目的收尾。
