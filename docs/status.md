# Where this port stands

A checkpoint, not a plan. Every number below is measured, and the command that measures it is
named so the next checkpoint can repeat it rather than trust it.

## What runs

```
cargo build --release        ->  20.6 MB binary
./girder "list the files"    ->  executes `bash`, prints the directory
./girder serve               ->  ws://127.0.0.1:PORT/api/remote.mux, 104 Remote methods
```

The binary is not a stub. A prompt runs a real agent loop with real tools; `serve` answers real RPC
calls; `--boot-report` says what it assembled and what is missing.

## Endpoints

Measured by `docs/port-map.md`'s generator, which reads upstream's `@Remote` markers and compares
them against a running `girder serve`:

| | Count |
| --- | --- |
| Upstream endpoints | 183 |
| Served here | 74 (40%) |
| Namespaces fully aligned | 8 of 35 |
| Namespaces with nothing | 23 of 35 |

Fully aligned: `commands`, `goals`, `job`, `llm`, `pluginMarket`, `session`, `userQuestions`,
`workspaceFiles`.

That 40% is the honest number and it is lower than the line count suggests. The reason is in the
next section.

## Why lines and endpoints disagree

230,051 lines of Rust exist here; upstream's in-scope packages are about the same. But **an endpoint
is not proportional to the lines behind it**: one `forgePanel.merge` is a Git operation with
authentication, and one `session.list` is a directory read. 40% of endpoints is not 40% of the work,
in either direction.

What the line count does say is that the *logic* exists. 82 `*_gap` files still carry that name, and
most of them are the decision half of something whose surface half is an endpoint that is not yet
registered. Wiring one is often a few hundred lines of handler over thousands of lines that already
pass their own tests.

## What is blocked, and by what

The largest gaps are not "not yet done". Four namespaces account for 68 of the 109 missing endpoints,
and each needs something this build deliberately does not have:

| Namespace | Missing | Blocked by |
| --- | --- | --- |
| `forgePanel` | 30 | A Git forge: network, authentication, worktrees |
| `pluginManager` | 15 | **The Cordis entry tree** — 12 uses of `composeEntries`, `readProfilePatches`, `ctx.loader.entries()` |
| `dynamicCordisRunner` | 12 | **The Cordis runtime** |
| `account` | 11 | An account backend: sign-in, balance, notifications |

The two Cordis ones are the important entry. This port **replaced** Cordis with `ServiceKey` +
`Fiber` + `EventBus` + `Registry` (~1,900 lines against upstream's 2,811), which was a design
decision made at the start. A plugin manager that reads the entry tree cannot be ported onto a
substrate that has no tree: the port would have to invent one, and then the read-only reasons it
computes (`management-required`, `unaddressable`) would describe a tree nobody has.

That is the same conclusion `boot_loader_gap` reached independently. It is a boundary of the
substitution, not a backlog item — and no amount of further porting moves it.

## The defect class this series kept finding

Every wiring in this series fixed the same shape: **a module with passing tests, no caller, and a
failure that nothing reported.**

- `fs_gap` cut grep results to `limit` lines and said nothing about the rest, so a model read a
  partial answer as complete.
- `content_projection_gap` / `block_to_wire` sent `{"type":"text","text":"[image sha256:abc]"}`
  where the API expects an image block. The model was told an image existed and shown a filename.
  Nothing failed, because a text block is a valid block.
- `deepseek_serialize_gap` was called with `&[]` as the history, so every request serialized to
  `"messages": []` while its system prompt and tools were correct. The test asserted the empty array.
- `gateway_gap`'s admission was reachable and unreached: every deployment served whatever `peer`
  string arrived.
- `schedule` served four endpoints upstream does not have, which is worse than a missing one: a
  client written against upstream cannot call them, and no further porting fixes that.

The common thread is that none of these are crashes. Each is a system answering confidently wrong,
and each was found by asking "what does upstream do here" rather than "does this compile".

## What a completion claim would need

**Not** "X% of lines ported". The defensible forms are:

1. **Endpoint parity**, which `docs/port-map.md` measures: 74 of 183, with 23 namespaces at zero.
2. **Named unportability.** A namespace whose blocker is the Cordis substitution should say so in
   the map rather than sitting in the missing list indistinguishable from backlog.
3. **Behaviour parity** on the paths that do exist. The suite is 10,312 tests, most of them written
   against the ported semantics rather than upstream's fixtures; a real parity claim needs upstream's
   own cases.

## The next three things, in order

1. **Name the Cordis-blocked endpoints in the map**, so "missing" splits into "backlog" and "boundary".
2. **`session.control`**, the last session endpoint: it streams a live-control baseline across every
   session a peer may see, so it needs the access layer to answer "which sessions" first.
3. **`schedule.{history, run, edit}`**, which are small and need execution history rather than a
   new substrate.
