# Cordis 的 Rust 迁移方案

本文是 `girder` 的迁移设计基线。它回答一个问题：Cordis 这套 TypeScript 插件框架，在 Rust 里应该长成什么样，哪些能一比一搬过来，哪些必须换实现，哪些要直接放弃。

上游规模：`vendor/cordis/src` 共 2811 行，其中 `fiber.ts` 820、`reflect.ts` 436、`events.ts` 371、`registry.ts` 337。DSH 对它的使用强度：`ctx.on()` 1714 处、`ctx.provide()` 997 处、`inject:` 947 处、`ctx.effect()` 657 处、`Service` 子类 242 个、`waterfall()` 108 处、`declare module '@deepseek-ai/cordis'` 386 处。

## 一、先划清楚：四层结构，逐层判断可行性

Cordis 不是一个整体，它是四层叠加起来的东西，每层在 Rust 里的迁移难度差一个数量级。

| 层 | 内容 | Rust 可行性 | 结论 |
|---|---|---|---|
| L1 注册表 | `Service`、`provide`、`inject`、`Fiber` 生命周期 | 高 | 一比一迁移，只换类型表达 |
| L2 事件总线 | 5 种 dispatch 模式、按 fiber 过滤、disposer 回收 | 高 | 迁移，但泛型约束要重新设计 |
| L3 反射代理 | `ctx.xxx` 动态属性、`Proxy` trap、mixin、accessor | 低 | **放弃**，改显式访问器 |
| L4 动态配置 | `!!js` 表达式、`StandardSchemaV1` 运行时校验、HMR | 中 | 降级迁移，用 `serde` + 编译期配置 |

**最大的判断：L3 必须放弃。** 不是"暂时做不到"，是"不该做"。`ctx.xxx` 动态属性在 Rust 里只能靠 `Deref` 到 `dyn Any` 再 downcast，这会让每个服务访问都变成运行时失败，而且彻底摧毁编译器能给的帮助。DSH 里 997 处 `ctx.provide()` 全都依赖这个机制，迁移时这些点会变成显式的 `ctx.tools()` / `ctx.sessions()` 方法调用——**这是好事**，因为类型错误从运行时前移到编译期。

## 二、L1：服务注册表

### 现状

`reflect.ts` 的 `provide(name, value)` 把值塞进 `store[symbol]`，读取时通过 Proxy 的 `get` trap 拦截。隔离（isolate）靠 `symbol` 做 key 的间接层。

### Rust 方案：类型化 ServiceKey + 类型擦除表

关键设计是**服务表按 `&'static str` 存储擦除后的 `Arc<dyn Any>`，但对外暴露强类型访问器**。我实测编译通过：

```rust
pub trait ServiceKey {
    type Service: ?Sized + Send + Sync + 'static;
    const ID: &'static str;
}

pub struct Tools;
impl ServiceKey for Tools {
    type Service = dyn ToolService;
    const ID: &'static str = "tools";
}

// 存储：擦除
type Table = Mutex<HashMap<&'static str, Arc<dyn Any + Send + Sync>>>;

// 写入：强类型
pub fn provide<K: ServiceKey>(&self, value: Arc<K::Service>);
// 读取：强类型，只有 ID 拼错才可能失败，而 ID 是常量
pub fn get<K: ServiceKey>(&self) -> Option<Arc<K::Service>>;
```

这个设计解决了 Cordis 里 `ctx.tools` 的三个问题：ID 打错在编译期不可能发生；`downcast` 失败只可能来自 trait 实现错误，不是配置错误；`Service` 天然 `Send + Sync`，不需要 DSH 里那些手写的线程安全约束。

### 关键点：`Service` 要拆成两部分

Cordis 的 `Service` 基类同时干了三件事：声明名字、注册自己、提供 mixin。Rust 里要拆开：

```rust
// 1. 能力接口：消费者依赖这个
pub trait ToolService: Send + Sync {
    fn list(&self) -> Vec<ToolSchema>;
    fn execute(&self, call: ToolCall) -> BoxFut<ToolResult>;
}

// 2. 注册锚点：只用来定位，不承载行为
impl ServiceKey for Tools { type Service = dyn ToolService; const ID: &'static str = "tools"; }
```

消费者签名从 `ctx.tools.execute(...)` 变成 `ctx.tools().execute(...)`，其中 `tools()` 返回 `Arc<dyn ToolService>`。**这比上游更安全**：上游 `ctx.tools` 在服务未注册时返回 `undefined`，错误要等到调用点才炸；Rust 版本在 `get()` 处就返回 `Option`，调用方必须处理。

### 必须处理的差异：`check` 谓词

Cordis 的 `provide(name, value, check)` 支持"服务存在但暂不可用"，`_checkImpl` 会用它决定 dependent 能否加载。Rust 里用：

```rust
pub struct Binding {
    value: Arc<dyn Any + Send + Sync>,
    ready: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}
```

`ready` 为 `Some` 且返回 `false` 时，依赖它的 fiber 停在该状态而非报错。这是上游 `FiberState::PENDING` 在 Rust 里的对应物。

### 隔离（isolate）

上游用 `Object.create(this[symbols.isolate])` 做原型链继承，11 处使用。Rust 里没有原型链，用**显式 scope 链**：

```rust
pub struct Scope { parent: Option<Arc<Scope>>, overrides: HashMap<&'static str, Arc<dyn Any + Send + Sync>> }
```

查找时沿链向上。`dsh-scope` 包已经是这个模型的显式版本（"child scopes inherit ancestor contributions, nearest definition wins"），说明 DSH 自己也在往这个方向收敛——Rust 版本直接实现 DSH 已经想要的东西，而不是复刻 Cordis 的实现细节。

## 三、L2：事件总线

### 现状

5 种模式：`emit`（不等待）、`parallel`（等全部）、`serial`（顺序等，遇 bail 停）、`bail`（同步顺序，遇 bail 停）、`waterfall`（环绕中间件）。DSH 里 `waterfall` 用了 108 处，是核心机制。

### Rust 方案：泛型 struct，不是 trait object

**我实测踩到一个坑，值得写进方案**：泛型方法不 object-safe，所以 `Events` 必须是具体 struct，不能是 `dyn Events` 服务。实测报 `E0038`。正确形态：

```rust
pub struct Events { /* 按事件名分组的 hook 表 */ }

impl Events {
    pub fn emit<E: EmitEvent>(&self, args: E::Args) { ... }
    pub fn waterfall<E: WaterfallEvent>(&self, input: E::In) -> BoxFut<E::Out> { ... }
}
```

### 关键点：用 trait 承载事件类型，而不是字符串

Cordis 靠 `declare module` 给 `Events` 接口补索引签名，386 处。Rust 里等价物是**每个事件一个 trait impl**：

```rust
pub trait EmitEvent: Send + Sync + 'static {
    const NAME: &'static str;
    type Args: Clone + Send + 'static;
}

pub trait WaterfallEvent: Send + Sync + 'static {
    const NAME: &'static str;
    type In: Send + 'static;
    type Out: Send + 'static;
}

pub struct ToolsPreExecute;
impl WaterfallEvent for ToolsPreExecute {
    const NAME: &'static str = "tools/pre-execute";
    type In = (String, u32);
    type Out = String;
}
```

这个设计的价值：事件名和参数类型**绑死在一个地方**，写错事件名是编译错误；`waterfall` 的输入输出类型由事件本身决定，监听器实现 `Waterfall<T>` 时必须匹配。上游 `declare module` 的合并是"声明在 A、实现在 B、类型检查靠全局收集"，Rust 版本是"类型就在事件类型旁边"。

代价：**386 处 `declare module` 不会自动消失**，每个都要手写成 trait impl。这是迁移的主要工作量之一。

### Waterfall 的 Rust 实现

这个我重点验证了，因为它是最容易写错的机制。Cordis 的 `waterfall` 是运行时递归 + `args.pop()` 取 `next`。Rust 里不能这么干，我用**索引游标 + `Arc<[...]>`**：

```rust
pub struct Chain<T: Send + 'static> {
    layers: Arc<[Arc<dyn Waterfall<T>>]>,
    index: usize,
    terminal: Arc<dyn Terminal<T>>,
}

impl<T: Send + 'static> Chain<T> {
    pub fn run(self, req: T) -> BoxFut<T> {
        match self.layers.get(self.index) {
            None => self.terminal.run(req),
            Some(layer) => {
                let layer = Arc::clone(layer);
                let next = Chain { layers: Arc::clone(&self.layers), index: self.index + 1, terminal: Arc::clone(&self.terminal) };
                layer.run(req, next)
            }
        }
    }
}
```

三个设计决策值得说明：

1. **`Arc<[...]>` 而不是 `Vec`**：每层推进只增加引用计数，不克隆整个中间件列表。
2. **索引游标 + 自有型 `Chain`**：如果 `Chain` 借用切片（`&'a [Arc<dyn Waterfall>]`），`BoxFut` 就必须带生命周期参数，而 `type Base<T> = dyn for<'a> Fn(T) -> BoxFut<'a, T>` 会直接触发 `E0582`（associated type 引用了不在 trait 输入里的 lifetime）——实测确认。用 `Arc<[...]>` + `usize` 让 `Chain` 完全自有，`BoxFut<T>` 保持单参数，绕开整个问题。
3. **`terminal` 单独抽象**：上游把内层行为作为 waterfall 的最后一个 "listener"，Rust 里显式区分"中间件"和"终端行为"更清晰，也避免了空列表时的特殊分支。

实测输出（三行代码验证）：
```
waterfall("a") = <a!>
waterfall("") = vetoed      // 第一层直接 return，短路成功
```

**"调用 `next()` 就委托，不调用就短路"这个契约，在 Rust 里变成"调用 `next.run()` 还是直接 return"**，语义完全等价，而且编译器会检查返回值类型。上游 AGENTS.md 里那条"Waterfall listeners MUST call `next()`"的纪律，Rust 版本靠类型表达一半、靠 code review 表达另一半——因为 Rust 无法强制"必须调用"。

### 五个模式的 Rust 签名

| 模式 | Cordis | Rust |
|---|---|---|
| `emit` | 同步，不等返回值 | `fn emit<E: EmitEvent>(&self, args: E::Args)` |
| `parallel` | 全部并发等 | `async fn parallel<E: EmitEvent>(&self, args: E::Args)` |
| `serial` | 顺序等，遇 bail 停 | `async fn serial<E: BailEvent>(&self, args: E::Args) -> Option<E::Out>` |
| `bail` | 同步顺序，遇 bail 停 | `fn bail<E: BailEvent>(&self, args: E::Args) -> Option<E::Out>` |
| `waterfall` | 环绕中间件 | `fn waterfall<E: WaterfallEvent>(&self, input: E::In) -> BoxFut<E::Out>` |

`serial` 和 `bail` 的返回值我用 `Option<E::Out>` 而非上游的"`null`/`false`/`undefined` 三者都算未 bail"。Rust 里没有三态，`Option` 是唯一诚实的表达。这条会改变少量监听器的写法，但消除了上游 `isBailed()` 那个隐晦的三值判断。

## 四、L1 的核心难点：Fiber 生命周期

这是整个迁移里**最难、也最值得先做**的部分。`fiber.ts` 820 行，处理的是：依赖未满足时挂起、依赖变化时重载、卸载时等待 dependent 先撤、effect 按逆序回收、重入安全。

### Rust 方案：显式状态机 + 确定性拆除

```rust
pub enum FiberState { Pending, Loading, Active, Failed, Unloading, Disposed }
```

上游的状态机可以直接搬。真正要重新设计的是**"卸载时等待 dependent"**这部分。上游靠：

```ts
while ([...this.provisions].some(impl => impl.dependents.size)) {
  await new Promise<void>((resolve) => this._release = resolve);
}
```

即：provider 卸载前，自旋等待所有 dependent 清空。Rust 里自旋 + `await` 是可行的，但更合适的做法是用 `tokio::sync::Notify` 或直接建依赖图做拓扑排序。

我建议**第一版用 `tokio::sync::watch` + 显式依赖计数**，理由：自旋在 tokio 下会占用 executor slot，而 `watch` 能在 dependent 清零时精确唤醒。这是"换实现而非照搬"的典型点。

### effect 的逆序回收

这个我实测验证了：

```
unloading 3 tool registrations (expect write, read, bash):
  disposing write
    unregister write
  disposing read
    unregister read
  disposing bash
    unregister bash
```

Rust 方案：

```rust
pub type BoxDisposer = Box<dyn FnOnce() -> BoxFut<()> + Send>;

pub struct Scope { disposers: Mutex<Vec<(&'static str, BoxDisposer)>> }

impl Scope {
    pub async fn dispose(&self) {
        let all: Vec<_> = std::mem::take(&mut *self.disposers.lock().unwrap());
        for (_label, d) in all.into_iter().rev() { d().await; }
    }
}
```

关键决策：**`FnOnce` 而不是 `Fn`**。上游 disposer 是幂等的（调用两次是 no-op），Rust 里用 `FnOnce` + 消费语义表达"只能拆一次"，把上游靠 `disposing` 标志位做的防护变成类型保证。这消除了整类"重复 dispose"的 bug。

### 和 `Drop` 的关系

**不要用 `Drop` 承担异步拆除。** `Drop` 不能 `await`，而 DSH 里 657 处 `ctx.effect()` 中有大量异步清理。方案是显式的 `async fn dispose(&self)`，由 registry 的 `shutdown()` 统一调用。`Drop` 只作为兜底：检测到未拆除时 `log::error!`，不执行实际清理。

Cordis 里 `DisposableList` + `_disposables.clear()` 的模式在 Rust 里对应 `Scope`，不要试图让每个 fiber 自己管一个 `Vec<BoxDisposer>` 然后靠 `Drop` 回收——那会在异步场景下静默丢清理。

## 五、L4：配置与校验

### 现状

`resolveConfig` 用 `StandardSchemaV1` 做运行时校验，`Plugin.Config` 是 schema。配置来源是 YAML + `!!js` 表达式。

### Rust 方案

- **`serde` + `schemars`**：`#[derive(Deserialize, JsonSchema)]` 覆盖上游 schema 的校验能力，且能生成 JSON Schema 给前端。
- **`!!js` 表达式放弃**。上游用它做"配置里嵌 JS 表达式"，Rust 里无对应物。用固定的插值语法替代（如 `${env.HOME}`），并在加载期报错而非运行期。
- **类型化配置优于运行时校验**：上游 `Config` schema 的价值在于"配置错误在加载期报错"。Rust 里 `serde` 反序列化天然做到这点，`schemars` 额外提供文档和前端提示。

### HMR

**第一版不做。** 上游 HMR 依赖"卸载插件后重新加载"，这要求所有 effect 都能干净回收。等 Fiber 生命周期的拆除顺序稳定后再加，且只做"改配置重启进程"级别，不做热替换。

## 六、迁移顺序：按依赖倒序，不做自底向上

这一点最容易出错。直觉是"先把 Cordis 搬完，再搬 DSH 包"，但那会造出一个**没有人用、也没被验证过的框架**。Cordis 的每个设计决策都是为了服务 DSH 的具体需求，脱离使用者去实现只会实现错。

推荐顺序，每一步都有可运行的验收物：

| 阶段 | 内容 | 验收 |
|---|---|---|
| 1 | `girder-core`：ServiceKey + Registry + effect 逆序回收 | 已有 greeter 示例跑通 |
| 2 | Fiber 状态机 + 依赖等待 + 拆除顺序 | 单测覆盖"provider 卸载等 dependent" |
| 3 | Events：`emit` / `waterfall` | 用 `tools/pre-execute` 的真实签名做测试 |
| 4 | **第一个真实切片**：`llm` 服务 + 一个 provider + 不涉及 agent loop | 能发一次流式请求并打印 |
| 5 | `session`（JSONL 持久化） | 读写一个真实 session 文件 |
| 6 | `tools` + `shell` | 能跑一次工具调用 |
| 7 | agent loop | 端到端一次对话 |
| 8 | 补齐其余 seam（fs、sandbox、subagent…） | 按需 |

**第 4 步是关键判断点。** 做完 llm + session + tools + loop 这条垂直切片（预估 8-12k 行 Rust），才算真正验证了 Cordis 的 Rust 化是否成立。在那之前，任何"框架已经搬完"的判断都是未经验证的。

## 七、必须放弃的东西（明确列出，避免后期返工）

| 上游机制 | 为什么放弃 | 替代 |
|---|---|---|
| `ctx.xxx` 动态属性 | Rust 无 Proxy，只能 downcast，摧毁类型检查 | 显式 `ctx.xxx()` 访问器 |
| `declare module` 类型合并 | Rust 无全局声明合并 | 每事件一个 trait impl |
| `!!js` 配置表达式 | 无对应物，且是安全风险 | 固定插值语法 + 加载期校验 |
| `mixin`（21 处 `ctx.inject`、mixin 调用） | 和动态属性同源 | 显式委托方法 |
| `accessor` / `Proxy` trap | 同上 | 普通方法 |
| `StandardSchemaV1` | 换成 serde/schemars | 编译期 + 加载期双校验 |
| HMR 热替换 | 依赖完全干净的拆除，第一版做不到 | 改配置重启，后期再评估 |
| TS 装饰器 `@Inject` | Rust 无装饰器 | 手写 `inject` 字段 |

## 八、风险清单

按"会不会让方案失效"排序：

1. **`Arc<dyn Trait>` 的 `Send` 传染性**。DSH 里大量服务持有非 `Send` 的运行时句柄（浏览器、PTY、子进程）。Rust 里 `BoxFut` 要求 `Send`，这会迫使所有服务实现 `Send + Sync`。如果某个 seam 本质上是单线程的（比如某些 Web 绑定），会需要 `LocalSet` 或单独线程 + channel。**这个风险在第 4 步就会暴露**，早暴露是好事。

2. **生命周期在 `BoxFut` 里的传播**。实测已踩到 `E0582`：只要 `BoxFut` 带生命周期参数，`dyn for<'a> Fn(T) -> BoxFut<'a, T>` 就编译不过。绕法是消除生命周期（`Arc` 持有中间件列表），但代价是所有跨 `await` 的借用都要变成所有权转移。这会显著增加 clone，需要在第 4 步评估 `Arc` 泛滥的程度。

3. **386 处 `declare module` 的手工迁移量**。这不是技术风险，是工作量风险。每处都要判断"这个事件该是什么类型"，不能机械转换。

4. **`ctx.provide()` 997 处的隐式契约**。上游靠 `check` 谓词和 `PENDING` 状态处理"服务存在但不可用"，这些语义散落在 997 个调用点里。Rust 版本必须先把这套语义收敛成显式 API，否则会逐个踩坑。

5. **框架与产品的边界模糊**。Cordis 是 vendored 的，DSH 对它有本地修改（`vendor/README.md` 有记录）。Rust 版本要不要保持"可替换框架"的形态？我的建议是**不要**：girder 直接实现一个融合了 Cordis 语义和应用装配的单体，因为 Rust 里"框架 + 产品"分层的收益远小于维护成本。这一点需要在第 1 步就定下来，后期改代价很大。

## 九、建议的下一步

在写任何框架代码之前，先做一件事：**把第 4 步的垂直切片画出接口草图**。具体是列出 llm / session / tools / loop 四个 seam 的 ServiceKey 定义和事件 trait，然后检查：

- 有没有哪个 seam 的接口在 Rust 里写不出来（尤其是异步 + 生命周期）
- 事件 trait 的数量是否可控（预期 40-60 个，而非 386 个——因为很多 `declare module` 是给同一个事件加不同视角的 JSDoc）
- `BoxFut` 的 clone 开销是否可接受

这一步产出的接口草图，会直接决定 `girder-core` 的 API 是否需要返工。比现在就把 fiber 状态机写完整更有价值。
