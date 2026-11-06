# Girder

A Rust rewrite of [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) (`dsh`), the
open-source agent harness from DeepSeek AI.

A girder is the main beam that carries a structure: it holds the parts up without being any of
them. That is the job of a harness — accept contributions from plugins, resolve them for
consumers, and stay out of the way.

> **Status: planning.** No implementation yet. The repository currently holds the
> [migration plan](docs/migration-plan.md) and nothing executable.

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

## Where the risk is

The plan's central finding is that feasibility is not the constraint — the upstream is a
developer preview whose API changes continuously. The plan therefore front-loads a vertical slice
(model call, session log, tool call, agent loop) and treats finishing it as the point to decide
whether the rest is worth doing.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
