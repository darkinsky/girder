# Port map: upstream Remote endpoints vs this build

Generated from upstream: every class extending `TypertRemoteService` in the in-scope packages
(`client`, `experimental`, `extensions` and `test-support` are out of scope by the plan), its
`super(ctx, key, { namespace })` line, and every `@Remote` marker. This build's side is read from
a running `girder serve`, so it is what a client can actually call.

Upstream in scope: **126** endpoints across **25** namespaces.
Present here: **121** (96%).

Methods registered with a structured refusal (`no_platform`, `no_installer`, `no_renderer`) count
as present: a client discovers the answer from the method rather than from a 404 it would read as
a typo. The **Tree** column counts uses of the Cordis entry tree; the two namespaces that had them
are served anyway, over an explicit registry, so the column is a record rather than a blocker.

| Namespace | Upstream | Here | Missing | Tree |
| --- | --- | --- | --- | --- |
| `mcpManager` ✗ | 2 | 0 | 2 |  |
| `subagents` ✗ | 2 | 0 | 2 |  |
| `schedule` | 9 | 8 | 1 |  |
| `account` ✓ | 11 | 11 | 0 |  |
| `agentPresets` ✓ | 3 | 3 | 0 |  |
| `agentTeams` ✓ | 1 | 1 | 0 |  |
| `commands` ✓ | 2 | 2 | 0 |  |
| `goals` ✓ | 7 | 7 | 0 |  |
| `job` ✓ | 3 | 3 | 0 |  |
| `llm` ✓ | 3 | 3 | 0 |  |
| `messageFeedback` ✓ | 3 | 3 | 0 |  |
| `officeKit` ✓ | 2 | 2 | 0 |  |
| `permissionPresets` ✓ | 1 | 1 | 0 |  |
| `pluginInventory` ✓ | 1 | 1 | 0 | 4 |
| `pluginManager` ✓ | 15 | 15 | 0 | 22 |
| `pluginMarket` ✓ | 4 | 4 | 0 |  |
| `session` ✓ | 24 | 24 | 0 |  |
| `sessionFeedback` ✓ | 1 | 1 | 0 |  |
| `sessionReferenceResolver` ✓ | 1 | 1 | 0 |  |
| `settings` ✓ | 5 | 5 | 0 |  |
| `terminal` ✓ | 8 | 8 | 0 |  |
| `userQuestions` ✓ | 1 | 1 | 0 |  |
| `web` ✓ | 1 | 1 | 0 |  |
| `workspace` ✓ | 10 | 10 | 0 |  |
| `workspaceFiles` ✓ | 6 | 6 | 0 |  |

`✓` fully aligned (22): `account`, `agentPresets`, `agentTeams`, `commands`, `goals`, `job`, `llm`, `messageFeedback`, `officeKit`, `permissionPresets`, `pluginInventory`, `pluginManager`, `pluginMarket`, `session`, `sessionFeedback`, `sessionReferenceResolver`, `settings`, `terminal`, `userQuestions`, `web`, `workspace`, `workspaceFiles`

`✗` no implementation (2): `mcpManager`, `subagents`

## Missing endpoints

### `mcpManager` (2)

- `mcpManager.probe`
- `mcpManager.restart`

### `schedule` (1)

- `schedule.run`

### `subagents` (2)

- `subagents.interruptByUser`
- `subagents.prompt`

## Endpoints upstream does not serve

### `account`

- `account.watch`

### `credentials`

- `credentials.describe`
- `credentials.set`
- `credentials.unset`

### `extension`

- `extension.activate`
- `extension.approve`
- `extension.deactivate`
- `extension.list`
- `extension.status`

### `goals`

- `goals.list`

### `host`

- `host.openPath`
- `host.revealInFileManager`

### `remote`

- `remote.mux`

### `session`

- `session.archive`
- `session.delete`
- `session.duplicate`
- `session.export`
- `session.resume`
- `session.stream`
- `session.version`
- `session.workspaceDesktop`

### `settings`

- `settings.delete`
- `settings.get`
- `settings.json`
- `settings.list`
- `settings.set`

### `team`

- `team.checkTaskGraph`

### `terminal`

- `terminal.read`
- `terminal.send`

### `userQuestions`

- `userQuestions.cancel`
- `userQuestions.list`

### `workflow`

- `workflow.cancel`
- `workflow.get`
- `workflow.list`
- `workflow.start`

### `workspace`

- `workspace.list`

