# 阶段 7 决策：按需迁移清单与实测限制

迁移计划 §2.10 把阶段 7 定为「按需迁移」，列了七个候选。本文是本机的实测结论与取舍。

## 先说的实测发现

### Landlock 在这台机器上不可用

```
$ grep -i landlock /boot/config-$(uname -r)
# CONFIG_SECURITY_LANDLOCK is not set

$ python3 ... (syscall 444, LANDLOCK_CREATE_RULESET_VERSION)
landlock unavailable, errno=Function not implemented (os error 38)
```

内核 6.6.47，但**编译时没开 `CONFIG_SECURITY_LANDLOCK`**，`/sys/kernel/security/lsm` 里也没有
`landlock`。所以在当前环境里**无法验证内核级文件系统隔离**。

这个事实决定了沙箱那一项的处理方式：可以写代码，但**不能声称它被验证过**。写一个无法测试的
安全机制，比不写更危险——它会让人以为受到了保护。

### 各候选的真实规模

| 候选 | 上游源码 | 本机可验证 |
|---|---|---|
| `subagent` | 9,575 行（11 个包，含 ACP / claude-code / codex 三种外部 driver） | 部分 |
| `fs` / `sandbox` 完整策略 | 646 行（仅 sandbox-local） | ❌ Landlock 不可用 |
| `context` / `compaction` | 需查 | ✅ |
| `mcp` | 需查 | ✅ |
| `skill` | 需查 | ✅ |
| `team` / `goal` / `schedule` | 低优先级 | — |
| `experimental` | 227k 行，计划建议部分放弃 | — |

## 本阶段的选择

**做 `subagent`（进程内驱动 + 深度限制 + 工具），不做内核沙箱。**

理由：

1. **`subagent` 是计划标记的最高优先级**（「高」），且不依赖 Landlock。
2. **它是唯一能在这台机器上完整验证的高优先级项**。9,575 行里，进程内驱动部分可以完整实现
   并测试；ACP / claude-code / codex 三种外部 driver 需要对应的 CLI，本机没有。
3. **沙箱内核级隔离推迟是诚实的**：与其写一个测不了的 landlock 封装，不如把 `Sandbox` trait
   的接口定好，等有可用内核的环境再实现。阶段 5 已经留了 `PathPolicy` 作为配置侧，接口不会返工。

## `subagent` 的范围

上游 11 个包做的是：fork 出一个子 agent，在**隔离的会话**里跑，有自己的深度配额、工具集、
以及对外部 CLI（claude-code / codex / acp）的适配。

本阶段做**进程内驱动**（`subagent-in-process-driver` + `subagent-fork-in-process` 的核心）：

1. **深度限制**：子 agent 不能再无限制地派生子 agent。上游用 `depth.ts` 管这个。
2. **子 agent 的循环**：复用阶段 4 的 `run`，但会话是独立的，工具集可裁剪。
3. **`subagent` 工具**：模型可以调用它派生一个子任务，等待结果。
4. **结果回流**：子 agent 的最终回答作为工具结果返回给父 agent。

不做：外部 CLI driver（需要那些 CLI 存在）、控制类工具（`tool-subagent-control` 的暂停/恢复）、
结构化输出约束（`structured.ts`）。

## `Sandbox` 接口（只定接口，不实现内核）

阶段 5 留的缺口需要一个形状。本阶段定义：

```rust
pub trait Sandbox: Send + Sync {
    /// Runs `command` confined by this sandbox.
    fn run(&self, command: Command, policy: &PathPolicy) -> BoxFut<Result<ExecOutput, ExecError>>;
    /// What this sandbox actually enforces, for honest reporting.
    fn enforcement(&self) -> Enforcement;
}

pub enum Enforcement {
    /// Only declared paths are checked before spawn; a command can still write anywhere.
    Advisory,
    /// The kernel refuses out-of-scope access. `mechanism` names it (`landlock`, `seatbelt`).
    Kernel { mechanism: &'static str },
}
```

`enforcement()` 存在的理由：调用方需要知道自己在哪一档保护下。一个返回 `Advisory` 的沙箱
和返回 `Kernel` 的沙箱在接口上完全一样，唯一区别就是这个值——所以它必须是接口的一部分，
而不是文档里的一句话。

## 预期留下的缺口

1. **内核级沙箱未实现**（Landlock 不可用，本机无法验证）。
2. **外部 CLI driver 未实现**（需要 claude-code / codex CLI）。
3. **子 agent 的取消语义简化**：上游有完整的暂停/恢复控制协议，本阶段只做「派生 → 等待 → 取结果」。
