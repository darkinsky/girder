# Port map: upstream Remote endpoints vs this build

Generated from upstream: every class extending `TypertRemoteService` in the in-scope packages
(`client`, `experimental`, `extensions` and `test-support` are out of scope by the plan), its
`super(ctx, key, { namespace })` line, and every `@Remote` marker. This build's side is read from
a running `girder serve`, so it is what a client can actually call.

Upstream in scope: **126** endpoints across **25** namespaces.
Present here: **74** (58%).

The **Tree** column counts uses of the Cordis entry tree (`ctx.loader.entries`, `composeEntries`,
`readProfilePatches`, `readPluginInventory`). A namespace with a non-zero count cannot be ported
onto this build's `ServiceKey` substrate without inventing the tree, which makes it a boundary
rather than backlog.

| Namespace | Upstream | Here | Missing | Tree |
| --- | --- | --- | --- | --- |
| `pluginManager` ✗ | 15 | 0 | 15 | 22 |
| `account` ✗ | 11 | 0 | 11 |  |
| `agentPresets` ✗ | 3 | 0 | 3 |  |
| `messageFeedback` ✗ | 3 | 0 | 3 |  |
| `schedule` | 9 | 6 | 3 |  |
| `mcpManager` ✗ | 2 | 0 | 2 |  |
| `officeKit` ✗ | 2 | 0 | 2 |  |
| `settings` | 5 | 3 | 2 |  |
| `subagents` ✗ | 2 | 0 | 2 |  |
| `terminal` | 8 | 6 | 2 |  |
| `agentTeams` ✗ | 1 | 0 | 1 |  |
| `permissionPresets` ✗ | 1 | 0 | 1 |  |
| `pluginInventory` ✗ | 1 | 0 | 1 | 4 |
| `sessionFeedback` ✗ | 1 | 0 | 1 |  |
| `sessionReferenceResolver` ✗ | 1 | 0 | 1 |  |
| `web` ✗ | 1 | 0 | 1 |  |
| `workspace` | 10 | 9 | 1 |  |
| `commands` ✓ | 2 | 2 | 0 |  |
| `goals` ✓ | 7 | 7 | 0 |  |
| `job` ✓ | 3 | 3 | 0 |  |
| `llm` ✓ | 3 | 3 | 0 |  |
| `pluginMarket` ✓ | 4 | 4 | 0 |  |
| `session` ✓ | 24 | 24 | 0 |  |
| `userQuestions` ✓ | 1 | 1 | 0 |  |
| `workspaceFiles` ✓ | 6 | 6 | 0 |  |

`✓` fully aligned (8): `commands`, `goals`, `job`, `llm`, `pluginMarket`, `session`, `userQuestions`, `workspaceFiles`

`✗` no implementation (13): `account`, `agentPresets`, `agentTeams`, `mcpManager`, `messageFeedback`, `officeKit`, `permissionPresets`, `pluginInventory`, `pluginManager`, `sessionFeedback`, `sessionReferenceResolver`, `subagents`, `web`

## Boundary: blocked by the Cordis substitution

- **`pluginInventory`** — 1 endpoint(s), 4 entry-tree uses
- **`pluginManager`** — 15 endpoint(s), 22 entry-tree uses

These are not backlog. A port would have to build the entry tree this design replaced, and

## Missing endpoints

### `account` (11)

- `account.cancelSignIn`
- `account.getBalance`
- `account.getNotificationContent`
- `account.getNotifications`
- `account.getProfile`
- `account.getState`
- `account.hasRunningAccountTasks`
- `account.reportNotificationEvent`
- `account.signOut`
- `account.startSignIn`
- `account.watchState`

### `agentPresets` (3)

- `agentPresets.list`
- `agentPresets.read`
- `agentPresets.select`

### `agentTeams` (1)

- `agentTeams.setEnabled`

### `mcpManager` (2)

- `mcpManager.probe`
- `mcpManager.restart`

### `messageFeedback` (3)

- `messageFeedback.delete`
- `messageFeedback.list`
- `messageFeedback.put`

### `officeKit` (2)

- `officeKit.generation`
- `officeKit.render`

### `permissionPresets` (1)

- `permissionPresets.catalog`

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

### `schedule` (3)

- `schedule.edit`
- `schedule.history`
- `schedule.run`

### `sessionFeedback` (1)

- `sessionFeedback.record`

### `sessionReferenceResolver` (1)

- `sessionReferenceResolver.candidates`

### `settings` (2)

- `settings.describe`
- `settings.openSettingsDocument`

### `subagents` (2)

- `subagents.interruptByUser`
- `subagents.prompt`

### `terminal` (2)

- `terminal.rename`
- `terminal.retain`

### `web` (1)

- `web.listProviders`

### `workspace` (1)

- `workspace.initializeDefault`

## Endpoints upstream does not serve

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

