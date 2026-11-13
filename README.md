# Girder

A Rust rewrite of [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) (`dsh`), the
open-source agent harness from DeepSeek AI.

A girder is the main beam that carries a structure: it holds the parts up without being any of
them. That is the job of a harness — accept contributions from plugins, resolve them for
consumers, and stay out of the way.

> **Status: 66 Remote methods, 31 tools, 3 slash commands, 2,962 tests.** `girder "…"` talks to a
> DeepSeek-format endpoint when `DEEPSEEK_API_KEY` (or `ANTHROPIC_API_KEY`) is set and falls back to a
> scripted provider otherwise, saying which on stderr. The live path is **tested against a mock server,
> not the real API** — no key exists in this environment, so the wire format is exercised rather than the
> service.
>
> Measured against upstream's backend (243,536 lines, excluding `client`, `test-support`, and
> `experimental`): **26%** by line count and **42%** of the Remote methods. The gap is concentrated in
> three places, listed at the end of this file rather than left for a reader to discover.

## Why this repository starts with a plan

The upstream harness is roughly 272,000 lines of host TypeScript across 2,453 files, built on a
plugin framework whose central mechanism — reading services as dynamic properties of a shared
context — has no Rust equivalent. A rewrite can absorb that cost, but only if the plan names
which mechanisms translate, which degrade, and which are abandoned before any code is written.

The [migration plan](docs/migration-plan.md) does that. It is split into three parts:

- **判断** — what Rust implements, what it cannot, and why. Every claim is backed by a compile or
  runtime check, listed in the appendix.
- **计划** — eight phases with acceptance criteria, and a decision point after the fourth.
- **执行** — what to do first, the fixed per-phase workflow, and explicit stop conditions.

## What exists today

Three crates: `girder-core` for the machinery, `girder-rpc` for the wire protocol, `girder-cli` for
the commands.

```sh
cargo test --all-features
cargo run -p girder-cli -- "list files" --tool bash --verbose
cargo run -p girder-cli -- serve --bind 127.0.0.1:8787 --watch tools/pre-execute
cargo run -p girder-cli -- schema
```

- **`ServiceKey` + `Registry`** — a capability is named by a key that carries its type, and
  resolved through a type-erased table. A provider and a consumer compile against the trait alone;
  publishing the same id twice is an error that names both plugins.
- **`Scope`** — every contribution a plugin makes registers a disposer, and disposal runs them in
  reverse registration order. Nothing depends on a plugin cleaning up after itself.
- **`Fiber`** — a plugin waits until the services it injects are published, unloads when one goes
  away, and unloads for good when its owner does. Teardown waits for every dependent to release
  first, and the wait parks on a notification instead of polling.
- **`EventBus`** — five dispatch modes (`emit`, `parallel`, `serial`, `bail`, `waterfall`) over a
  typed catalogue. An event is a type, so a misspelled name or a wrong argument is a compile error
  rather than an `undefined` at runtime. Waterfall layers are a value, not a recursive closure —
  that is what keeps the chain out of E0582 territory.

- **Config loading** — layered YAML patches over an entry list, with `when:` conditions replacing
  upstream's embedded JavaScript. A `!!js` tag in any file is a load-time error naming its line,
  because a config that cannot be evaluated must not silently disable a plugin.
- **The vertical slice** — `llm`, `session`, `tools`, and `loop` seams joined end to end. A turn
  streams chunks, folds them into a message, runs the three-waterfall tool pipeline, and appends
  every step to a zstd-compressed JSONL log.
- **The execution layer** — a shell runs with pipes, a timeout kills the whole process group rather
  than just the direct child, and a path policy refuses out-of-scope writes before spawning.
- **The RPC protocol** — the four downlink frames and one uplink result of upstream's gateway, over
  a WebSocket mux, with JSON Schema export so both sides can be checked against one artifact.
  Bus dispatches reach a watching client as `emit` frames through a push subscription, not a poll —
  a poll cannot carry arguments and cannot report the same event twice.
- **66 Remote methods** over `/api/remote.invoke`, across `session`, `workspace`, `terminal`, `job`,
  `settings`, `schedule`, `extension`, `host`, and `command`. The two that remain answer
  `NOT_IMPLEMENTED` rather than `UNKNOWN_METHOD`, so a client can tell a gap from a typo. `create` is
  idempotent per id: a retry must not append a second turn to the log.
- **Subagent delegation** — a child agent runs in its own session, and at the depth ceiling the
  delegation tool is removed from the tool set rather than refused at call time, so the model cannot
  spend its way into unbounded recursion.
- **A live provider** — the Anthropic Messages wire format (which is what DeepSeek's endpoint
  speaks), with an incremental SSE parser and chunk translation. The request shape and the parser
  are covered by tests against a local server.

Everything else on the plan — the `Remote` method namespaces, kernel-enforced sandboxing — is not
here.

> **The sandbox is not kernel-enforced.** `PathPolicy` checks *declared* writes before a process
> starts. A command that writes somewhere it did not declare is not stopped. Do not treat
> `--tool bash` as isolation from a misbehaving model; see
> [migration plan §2.8](docs/migration-plan.md).

The [event inventory](docs/event-inventory.md) records what upstream actually dispatches: 97 events
across 44 domains, not the 179 `declare module` blocks they superficially resemble.

Upstream's bundle configs embed JavaScript in YAML — 61 occurrences of `!!js` across the bundles.
Those are rejected and replaced with a `when:` condition, which is data rather than code; see
[migration plan §2.6](docs/migration-plan.md) for the four shapes they come in.

The [interface sketch](docs/phase4-interfaces.md) records what was decided before the slice was
written — most importantly that cancellation and agent identity become explicit parameters, because
Rust has no ambient equivalent that survives `tokio::spawn`.

The backend is **63,865 lines of Rust against 243,536** of upstream — 26% by line count, higher by
capability because a rewrite says in one line what a plugin framework spends ten on. The ratio improves
only by finishing whole subsystems rather than whole files.

## What is not done, and why

Three gaps are worth naming because each fails differently:

- **Streaming.** Upstream has stream-mode Remote methods (`session.stream`, `terminal.attach`,
  `job.watch`). `RemoteRegistry` has no stream kind, so these answer `NOT_IMPLEMENTED` rather than
  opening a subscription that never delivers. `session.follow` and `job.follow` are cursor polls that
  resume exactly, which is the honest version of the same capability.
- **Transport backpressure.** `server.rs` uses an unbounded outbound channel, so a client that writes
  without reading grows the queue until memory runs out. [`rpc_limits`](docs/rpc-limits-finding.md)
  implements and tests the per-connection policy; wiring it needs the outbound queue bounded, which
  makes `send` asynchronous at sites that are synchronous today.
- **The Cordis plugin runtime.** Upstream's 11k-line extension runtime has no Rust equivalent, and
  closing it means reimplementing the framework this project deliberately replaced. Extensions here
  are shell scripts with an approval gate, which is a smaller promise kept.

A full accounting is in [the migration plan](docs/migration-v2.md), which records each milestone's
limits alongside its results rather than only its results.

## Where the risk is

The plan's central finding is that feasibility is not the constraint — the upstream is a
developer preview whose API changes continuously. The plan therefore front-loads a vertical slice
(model call, session log, tool call, agent loop) and treats finishing it as the point to decide
whether the rest is worth doing.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
