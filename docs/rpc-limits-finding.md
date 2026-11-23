# 传输层背压：已接到 WebSocket

`serve_full` 的下行队列是 `Outbound`（容量 256，`Reject`）。同步 `send` 保持不变。队列满时通知帧被丢弃并按帧类型计数；`ready` 和 `waterfall` 被拒绝时 `ClientHandle::send` 返回 `false`，调用方按连接失败处理，不再等待一个写不出去的应答。

每个 WebSocket 有自己的 `Limiter`：读循环 `admit`，处理结束 `release`。超过配额的上行被丢弃并记一条 debug，连接保持打开。升级时 `max_message_size` 和 `max_frame_size` 都是 4 MiB，与 HTTP invoke 的 body 上限相同。

订阅数上限 `DEFAULT_SUBSCRIPTION_LIMIT` 仍然只限制订阅个数。上游那套带心跳和按 stream 背压的双向 mux 还不是这条连接在用的帧格式。

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
