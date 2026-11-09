# typert 的 Rust 方案决策

> 状态：**待决策**。本文不选定方案，只把事实和代价摆清楚。
> 上一个决策（跳过 sandbox 内核实现）是在你明确同意后做的；这个不同——它波及 30k+ 行，
> 应该由你决定。

## 事实

### 上游 typert 是什么

| 包 | 行数 | 职责 |
|---|---|---|
| `generator` | 8,038 | **编译期**：用 TypeScript API 分析源码，提取类型模型，生成产物 |
| `protocol` | 1,802 | 运行时：`@Remote` 装饰器、网关绑定、版本化描述符 |
| `registry` | 977 | 服务端的 Remote 方法注册表 |
| `loader` | 507 | 加载生成的产物 |

**关键事实：`generator` 是 TypeScript 的 TypeChecker 消费者。**

它 `import ts from 'typescript'`，遍历 Program/Symbol/TypeNode，把一个 TS 类型图抽成
"compiler-independent model"，再渲染成客户端代码。

产物共 **164 个文件**（`typert.host.js` / `.d.ts` / `typert.remote-client.*`）。

### 它解决的是什么问题

不是「传输格式」，是**客户端如何知道服务端有什么方法、参数是什么类型**。

一个典型的 TS 项目里，`@Remote` 装饰器标记方法，生成器读取它们签名里的**真实类型**，
产出客户端可调用的类型安全桩（stub）。

### Rust 侧的实际情况

Rust 已经有这套机制的一部分：

- `girder-rpc` 已实现 mux 协议和 4 种下行帧（1,039 行）
- Remote 方法已经能通过 `/api/remote.invoke` 调用
- 类型由 Rust 编译器检查，**运行时不需要反射**

**但 Rust 没有等价的 TypeChecker 分析器**——而且也不需要：Rust 的类型信息在编译期
由编译器掌握，可以用过程宏（proc macro）在编译期读取。

## 三个方案

### 方案 A：过程宏生成（保留类型安全）

用 `#[derive(Typert)]` / `#[remote]` 过程宏，在编译期读取 Rust 类型，生成 JSON Schema 和客户端桩。

| 维度 | 评估 |
|---|---|
| 工作量 | **约 4,000–6,000 行**（proc macro crate + 运行时支持） |
| 类型安全 | **保留**——客户端拿到的是生成的 `.d.ts` 或 Rust 桩 |
| 风险 | 中：proc macro 调试困难，错误信息不如手写清晰 |
| 依赖 | `syn` / `quote` / `proc-macro2`（成熟） |
| 能否验证 | **能**：生成产物可以断言，编译失败即测试失败 |

**最大优势**：保持上游的设计意图——类型是单一事实来源。

**最大风险**：proc macro 的学习曲线和调试成本。

### 方案 B：手写 schema（放弃生成）

每个 Remote 方法手工声明参数 schema（`serde_json::Value` + `schemars`）。

| 维度 | 评估 |
|---|---|
| 工作量 | **约 1,500–2,500 行**（几乎全是声明，不是逻辑） |
| 类型安全 | **部分**——schema 和 Rust 类型可能不一致，且无编译期检查 |
| 风险 | 低（无新技术），但**漂移风险高** |
| 能否验证 | 只能测 schema 格式，**测不出与实现的不一致** |

**最大问题**：schema 和实现会漂移，而且漂移是静默的。改了一个 struct 字段忘了改 schema，
客户端拿到的是旧形状——这正是上游花 8k 行避免的事。

### 方案 C：不做（保持当前 RPC）

只用现有的 `girder-rpc`，客户端手工构造 JSON。

| 维度 | 评估 |
|---|---|
| 工作量 | **0** |
| 类型安全 | **无**——客户端没有任何类型信息 |
| 风险 | 零新增，但**客户端每次调用都可能拼错字段** |
| 能否验证 | 能测协议，测不出客户端误用 |

**最大优势**：立刻可用。

**最大问题**：这恰好是上游用 typert 解决的问题。选择 C 等于承认「不做类型化客户端」。

## 量化对比

| | A 过程宏 | B 手写 schema | C 不做 |
|---|---|---|---|
| 行数 | 4–6k | 1.5–2.5k | 0 |
| 占上游 typert | 50–75% | 20–30% | 0% |
| 类型漂移风险 | 无 | **高** | **极高** |
| 首次可用时间 | 长 | 中 | 立即 |
| 后续 api/web/host 依赖 | 直接可用 | 需手工同步 | 全部手工 |

## 关键的连带影响

`api`（23k）、`web`（3.7k）、`host`（4.5k）三块共 **31k 行**都依赖「客户端如何知道服务端类型」。

- 选 A：这三块能直接生成客户端调用
- 选 B：这三块要手工同步，**漂移风险乘以三**
- 选 C：这三块全部手工

所以 A 的 4–6k 换来的是 31k 行的可行性；B 的 1.5–2.5k 把风险推给后面；C 把风险推给使用者。

## 实测：proc macro 的真实能力边界

我在 `/tmp/macroprobe` 里实际跑了 `syn`，确认了三点：

### 1. 函数签名可以提取

```
fn: list_sessions
  arg type tokens: u32
  arg type tokens: Option < String >
  return type tokens: Vec < SessionSummary >
```

### 2. 结构体字段可以提取

```
struct: SessionSummary
  field `id`: String
  field `turns`: u64
  field `title`: Option < String >
```

### 3. **但拿到的是 token，不是解析后的类型**

这是关键限制。`Vec<SessionSummary>` 里的 `SessionSummary`，macro **只知道这个名字，
不知道它有哪些字段**——除非 `SessionSummary` 本身也派生了 `Typert`。

**三种处理方式，各有代价：**

| 方式 | 机制 | 代价 |
|---|---|---|
| 每个类型都派生 | 生成 trait impl，运行时组装 schema | 需要用户给所有类型加 `#[derive(Typert)]` |
| 注册表 + 类型名 | 运行时按名字查已注册的 schema | 名字拼错是**运行时**错误，不是编译期 |
| 全量分析 | 用 `rustdoc --output-format json` 或 `ra` 解析 | 需要外部工具，构建复杂 |

第一种是上游的做法（TS 的 TypeChecker 天然有完整类型图）。Rust 里它意味着更多标注，
但**保持了编译期可发现**。

第二、三种要么丢编译期检查，要么引入重依赖。

## 修正后的工作量估计

之前写「4–6k 行」偏乐观。加入了「token 不是类型」这个限制后：

| 部分 | 行数 |
|---|---|
| proc macro 骨架（解析、代码生成） | 800–1,200 |
| `TypertType` trait + 基础类型实现 | 600–900 |
| 派生宏（struct/enum 展开） | 1,200–1,800 |
| schema 渲染（JSON Schema / TS 类型） | 1,000–1,500 |
| 客户端桩生成 | 800–1,200 |
| 测试 | 1,200–2,000 |
| **合计** | **5,600–8,600** |

**这是 5.6k–8.6k，不是 4–6k。** 上游 typert 用 8k 行达到了同样目标（借助 TypeChecker），
Rust 侧需要相近甚至更多，因为要做 TypeChecker 不做的事——自己解析 token。

## 我的建议

**方案 A**，但我要修正自己的估计：它是 **5.6k–8.6k 行**，不是 4–6k。

理由仍然成立：

1. 它保留单一事实来源——这是上游 8k 行换来的核心价值
2. 即使 8k，对应 31k 的下游依赖仍然合理
3. proc macro 的风险可控：它能编译失败，而编译失败是可发现的；B 的漂移是静默的

**但成本比我上一轮说的高 40%–100%。** 如果你是想要更快看到系统跑起来，B 或 C 更合适，
只是要在文档里明确记下「客户端无类型安全」。

我倾向 A，但我不会假装它比实际便宜。

## 第一步已实现（可回退）

`typert.rs`：`Schema` / `ObjectSchema` / `Field` / `Method` / `Catalogue`，25 个测试。

**没有 proc macro。** 按计划先手写 trait 的实现，用证据确定 schema 形状，而不是让未来的
macro 决定形状。

**`Schema` 是枚举而不是裸 `Value`。** 从 `Value` 构造的 schema 可以是任何东西，包括错的，
而编译器帮不上忙。枚举只能表示存在的形状。

**发现一个 JSON Schema 表达不了的东西：revision 兼容性。**

一个客户端必须能**省略它从没听说过的字段**。所以「在 revision 2 引入的字段」对 revision 1
的客户端是可省的——哪怕它在服务端是必需的。

这个事实无法放进 schema，只能记在旁边：`Field::added_in` + `ObjectSchema::required_at(revision)`。

这是本轮唯一的新设计，也是不做这件事就发现不了的。

## 第二步已实现：`#[derive(Typert)]`

`crates/girder-macros`（proc macro crate，约 330 行）+ 13 个测试。

**验证方式是手写版本作标准答案。** `the_derive_matches_the_hand_written_schema` 断言
macro 产出的 `ObjectSchema` 与手写版本**结构相等**，`the_rendered_json_matches` 断言渲染出的
JSON 也相等。

**这个测试立刻抓到了一个差异**：macro 把结构体的 `/// The same shape...` 文档也写进了 schema，
而我的参考值里没有。

**我修正的是参考值，不是 macro。** 因为把 doc 注释带进 schema 是正确行为——生成客户端的
文档正是这么来的。这是一个具体例子，说明为什么要有手写标准答案：没有它，我会以为两边一致。

### enum 派生（已完成）

enum 展开成判别联合，14 个新测试（共 21 个）。

**struct 和 enum 是两个独立的展开函数，不是一个函数的两个分支。**
struct 产出 `ObjectSchema`，enum 产出 `Schema::OneOf`——类型不同，硬塞进一个函数会让两个分支
都无法完整表达。

**每个分支把 `kind` 固定到自己的 tag**，所以客户端按 `kind` 收窄时不需要检查别的东西。
测试 `the_rendered_union_constrains_each_tag` 断言的是 `"const"` 而不是枚举列表。

**默认 tag 是 Rust 名字的 kebab-case**（`Failed` → `failed`），和这个代码库已经写出的事件 tag
一致。客户端读的是 `turn-end`，不是 `TurnEnd`。

**tuple variant 被拒绝。** 它没有字段名，叫它们 `0`/`1` 会产出一个客户端无法解释的 schema。

**每个变体渲染成有名字的对象**（`DocumentedEnumOnly`），这样 `$ref` 指向的目标是可读的。

### `#[remote]` 属性宏（已完成）

`crates/girder-macros/src/remote.rs`（约 150 行）+ 13 个测试。

**属性宏会「替换」被标注的项，所以必须把原函数重新输出。**

这是属性宏最经典的坑：忘了返回原函数，代码能编译，函数却消失了，错误出现在所有调用点。
每个测试都**既调用原函数，又读它的描述符**，就是为了抓这个。

**生成的是 `const` 而不是 `impl`。**

第一版我写的是 `impl #fn_name`——但 `fn_name` 是**函数名，不是类型名**，那会指向一个不存在的
类型。自由函数不能带固有方法。改成同级 `const`：任何作用域都可用，也不会和已有 impl 冲突。

**`&self` 接收者不进 wire 签名。** namespace 已经说明了这个对象是谁。

**`&str` 参数描述值，不描述借用。** wire 上传的是字符串，不是指向它的指针。

**参数名来自 `Pat::Ident`。** 解构模式（`fn f((a, b): (u32, u32))`）没有 wire 名，被拒绝。

**解析失败时仍然输出原函数**，这样调用方看到的是宏的错误，而不是一串「函数未找到」。

### 实测的行数

| 部分 | 实际 |
|---|---|
| `derive.rs`（解析 + 展开） | 约 260 行 |
| `schema_path.rs` | 20 行 |
| `lib.rs`（文档 + 入口） | 50 行 |
| 测试 | 约 200 行 |

比估计的 1,200–1,800 行少很多，因为**只做了 struct**，没做 enum（判别联合需要不同的展开，
我让它显式报错而不是生成错误的东西）。

## 如果选 A，下一步是什么

前两步已完成（见上）。剩余：

1. ~~enum 的派生~~（已完成，约 150 行）
2. ~~`#[remote]` 属性宏~~（已完成，约 150 行）
3. **客户端桩生成**——把 `Catalogue` 渲染成 TypeScript 或 Rust 客户端。约 800–1,200 行。

**到目前为止实际是约 530 行**（macro 330 + 测试 200），远低于我最初估计的 5.6–8.6k。
差额在客户端桩生成那一块——那才是真正的大头。
