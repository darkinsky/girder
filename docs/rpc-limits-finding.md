# 传输层缺少背压：一个已确认的缺口

## 测量结果

`crates/girder-rpc/src/server.rs:371` 用的是 `mpsc::unbounded_channel`：

```rust
let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<DownlinkFrame>();
```

**无界**。一个客户端不断发请求、又不读响应，出站队列会一直增长，直到进程内存耗尽。这不是理论问题：
WebSocket 的对端可以合法地只写不读，而服务端没有任何办法知道它读了多少。

全文件唯一的限制是 `DEFAULT_SUBSCRIPTION_LIMIT = 8`（`stream.rs:381`），限制的是**订阅数**，不是请求数，
也不是队列长度。

## 已有的缓解措施

`rpc_limits.rs` 提供了 per-connection 的令牌桶和在途上限，但它**还没有接进传输层**。接进去需要：

1. 每个连接一个 `Limiter`
2. 在读循环里 `admit`，被拒时回 `429` 风格的错误帧而不是静默丢弃
3. 请求完成时 `release`（`Permit` 故意不实现 `Drop`，因为它需要跨 `await` 存活）

## 未做的部分及原因

- **出站队列的有界化**：把 `unbounded_channel` 换成 `channel(capacity)` 会让 `send` 变成异步的，
  而发送点在若干同步上下文里。这是一次真实的改动，不是一行替换，需要单独设计。
- **`max_message_size`**：`upgrade`/`invoke` 没有设置传输层的帧上限。WebSocket 消息是整条送达后本代码
  才看到的，所以应用层的尺寸检查拦的是 dispatch，不是分配。这个上限只能靠传输层设。

## 结论

`rpc_limits` 把**策略**实现了并测过了 20 个用例；把它接进 `server.rs` 是**独立的一步**，
涉及出站队列的语义变化，不适合和策略实现混在一次改动里。

这条记录存在的理由是：一个已知的、测量过的缺口，写下来比记在脑子里可靠。

---

# 缓存已验证：3 次请求只调用 1 次 provider

`CachingProvider` 的 40 个单元测试只证明了**正确性**——重复请求返回相同答案。但**一个每次都调用 provider 的缓存也是正确的**。

实测（计数 provider + 3 次相同请求）：

```
provider calls: 1
cache entries: 1
stats: Stats { hits: 2, misses: 1 }
```

这确认了缓存真的在避免网络调用，而不只是返回了正确的字节。

同一个教训在本会话出现过三次：`--tool read` 不调用 read 工具、`tools-mcp` 少报 8 个工具、
以及这里的缓存。**单元测试证明单元正确；只有把东西跑起来才证明接缝正确。**

---

# 全部 26 个 session 方法已端到端验证

对真实运行的服务逐个调用（空参数 + 真实参数两轮）：

```
list            ok          create          ok
page            ok          events          ok
projections     ok          prompt          ok（空参数下 INVALID_ARGUMENTS，正确）
cancel          ok          rename          ok
delete          ok          fork            ok
search          ok          follow          ok
resume          ok          export          ok
archive         ok          duplicate       ok
state           ok          updateQueue     ok
attachment      ok          modelCatalog    ok
selectModel     ok          openWorkspacePath ok
canOpenWorkspacePath ok     workspaceDesktop  ok
workspacePathApplications ok  stream        ok
```

**零个 `NOT_IMPLEMENTED`，零个 `UNKNOWN_METHOD`。**

## 错误消息的质量

空参数调 `session.page` 得到：

```
invalid arguments for `session.page`: missing field `address`
```

传 `address.session` 而不是 `address.sessionId` 得到：

```
invalid arguments for `session.page`: `address.sessionId` is required
```

**两个错误都指出了下一步该传什么**，而不是「参数错误」，这是端到端可用的实际标志。
