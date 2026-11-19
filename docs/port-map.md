# Port map: upstream Remote endpoints vs this build

Generated from upstream: every class extending `TypertRemoteService` in the in-scope packages
(`client`, `experimental`, `extensions` and `test-support` are out of scope by the plan), its
`super(ctx, key, { namespace })` line, and every `@Remote` marker. This build's side is read from
a running `girder serve`, so it is what a client can actually call.

Upstream in scope: **126** endpoints across **25** namespaces.
Present here: **102** (80%).

The **Tree** column counts uses of the Cordis entry tree. A non-zero count means the namespace
cannot be ported onto this build's `ServiceKey` substrate without inventing the tree -- a boundary
rather than backlog. Methods registered with a structured refusal (`no_platform`,
`store_unavailable`) count as present: a client discovers the answer from the method rather than
from a 404 it would read as a typo.

| Namespace | Upstream | Here | Missing | Tree |
| --- | --- | --- | --- | --- |
| `pluginManager` ✗ | 15 | 0 | 15 | 22 |
| `mcpManager` ✗ | 2 | 0 | 2 |  |
| `officeKit` ✗ | 2 | 0 | 2 |  |
| `schedule` | 9 | 7 | 2 |  |
| `subagents` ✗ | 2 | 0 | 2 |  |
| `pluginInventory` ✗ | 1 | 0 | 1 | 4 |
| `account` ✓ | 11 | 11 | 0 |  |
| `agentPresets` ✓ | 3 | 3 | 0 |  |
| `agentTeams` ✓ | 1 | 1 | 0 |  |
| `commands` ✓ | 2 | 2 | 0 |  |
| `goals` ✓ | 7 | 7 | 0 |  |
| `job` ✓ | 3 | 3 | 0 |  |
| `llm` ✓ | 3 | 3 | 0 |  |
| `messageFeedback` ✓ | 3 | 3 | 0 |  |
| `permissionPresets` ✓ | 1 | 1 | 0 |  |
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

`✓` fully aligned (19): `account`, `agentPresets`, `agentTeams`, `commands`, `goals`, `job`, `llm`, `messageFeedback`, `permissionPresets`, `pluginMarket`, `session`, `sessionFeedback`, `sessionReferenceResolver`, `settings`, `terminal`, `userQuestions`, `web`, `workspace`, `workspaceFiles`

`✗` no implementation (5): `mcpManager`, `officeKit`, `pluginInventory`, `pluginManager`, `subagents`

## Boundary: blocked by the Cordis substitution

- **`pluginInventory`** — 1 endpoint(s), 4 entry-tree uses
- **`pluginManager`** — 15 endpoint(s), 22 entry-tree uses

These are not backlog. A port would have to build the entry tree this design replaced.

## Missing endpoints

### `mcpManager` (2)

- `mcpManager.probe`
- `mcpManager.restart`

### `officeKit` (2)

- `officeKit.generation`
- `officeKit.render`

### `pluginInventory` (1)

- `pluginInventory.list`

### `pluginManager` (15)

- `pluginManager.cancelInstall`
- `pluginManager.inspect`
- `pluginManager.installBundle`
- `pluginManager.listBundles`
- `pluginManager.listPlugins`
- `pluginManager.listVersionExemptions`
- `pluginManager.listVersions`
- `pluginManager.readBundleDetail`
- `pluginManager.registries`
- `pluginManager.removeBundle`
- `pluginManager.setBundleEnabled`
- `pluginManager.setPluginEnabled`
- `pluginManager.setVersionExemption`
- `pluginManager.updateBundle`
- `pluginManager.waitForInstall`

### `schedule` (2)

- `schedule.history`
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

