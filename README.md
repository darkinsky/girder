# Girder

A Rust rewrite of [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) (`dsh`), the
open-source agent harness from DeepSeek AI.

A girder is the main beam that carries a structure: it holds the parts up without being any of
them. That is the job of a harness — accept contributions from plugins, resolve them for
consumers, and stay out of the way.

Upstream's browser UI (`packages/client`, `apps/web`, and the desktop shell) is not part of this
repository. `packages/web` in upstream is the model's ability to search and fetch pages, and that
capability does belong here.

## Layout

- `girder-core` — registry, sessions, the agent loop, tools, providers, and Remote methods.
- `girder-rpc` — the gateway wire protocol and the WebSocket server.
- `girder-cli` — one prompt, or `serve`.
- `girder-macros` — the derive macros the core crate uses.

```sh
cargo test --all-features
cargo run -p girder-cli -- "list files" --tool bash --verbose
cargo run -p girder-cli -- serve --bind 127.0.0.1:8787 --watch tools/pre-execute
cargo run -p girder-cli -- serve --bind 127.0.0.1:8787 --dist-index /path/to/web/dist/index.html
cargo run -p girder-cli -- schema
```

`girder "…"` talks to a DeepSeek-format endpoint when `DEEPSEEK_API_KEY` (or `ANTHROPIC_API_KEY`)
is set, and falls back to a scripted provider otherwise. The live path is tested against a mock
server. No key is stored in this repository.

## Limits that are still true

- **The sandbox is not kernel-enforced.** `PathPolicy` checks declared writes before a process
  starts. A command that writes somewhere it did not declare is not stopped. Do not treat
  `--tool bash` as isolation from a misbehaving model.
- **The standard Remote stream mux is now the default.** `/api/remote.mux` carries upstream
  `open` / `item` / `end` / `cancel` client frames and `item` / `error` / `end` server frames,
  with heartbeat and bounded per-stream uplink accounting. The older `ready` / `emit` /
  `waterfall` interaction protocol remains only at `/api/remote.mux?legacy=1` for compatibility.
- **Connection RPC envelopes are supported at `/api/<namespace>/<method>`.** They use the
  upstream `client-request` / `server-response` envelope, including the Gateway-owned
  `$events/result` endpoint.
- **Config does not embed JavaScript.** A `!!js` tag is a load-time error. The replacement is a
  `when:` condition, which is data rather than code.
- **Legacy RPC mode still accepts the peer name from the request body.** Set
  `GIRDER_REQUIRE_PEER=1` to enable the admitted-peer path; it refuses callers without a configured
  credential instead of treating a body field as proof. When `--dist-index` is used, the CLI prints
  a one-time browser URL and protects the index with an authority-bound signed cookie; assets and
  the `/api` routes remain on the same origin.

The [event inventory](docs/event-inventory.md) records the events upstream actually dispatches:
97 events across 44 domains.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
