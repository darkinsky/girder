# Port map: upstream Remote endpoints vs this build

Generated from upstream itself: every class extending `TypertRemoteService`, the
`super(ctx, key, { namespace })` line that names its namespace, and every `@Remote` marker.

Upstream: **183** endpoints across **35** namespaces.
Present here: **67**.

| Namespace | Upstream | Here | Missing |
| --- | --- | --- | --- |
| `forgePanel` | 30 | 0 | 30 |
| `pluginManager` | 15 | 0 | 15 |
| `dynamicCordisRunner` | 12 | 0 | 12 |
| `account` | 11 | 0 | 11 |
| `schedule` | 9 | 3 | 6 |
| `speech` | 5 | 0 | 5 |
| `pluginMarket` | 4 | 0 | 4 |
| `agentPresets` | 3 | 0 | 3 |
| `messageFeedback` | 3 | 0 | 3 |
| `claudeCodeMods` | 2 | 0 | 2 |
| `cotTranslation` | 2 | 0 | 2 |
| `mcpManager` | 2 | 0 | 2 |
| `officeKit` | 2 | 0 | 2 |
| `productAnalytics` | 2 | 0 | 2 |
| `settings` | 5 | 3 | 2 |
| `subagents` | 2 | 0 | 2 |
| `terminal` | 8 | 6 | 2 |
| `actionCards` | 1 | 0 | 1 |
| `agentTeams` | 1 | 0 | 1 |
| `fileUploads` | 1 | 0 | 1 |
| `permissionPresets` | 1 | 0 | 1 |
| `pluginInventory` | 1 | 0 | 1 |
| `pluginRecommendations` | 1 | 0 | 1 |
| `pluginRegistryProbe` | 1 | 0 | 1 |
| `sessionFeedback` | 1 | 0 | 1 |
| `sessionReferenceResolver` | 1 | 0 | 1 |
| `web` | 1 | 0 | 1 |
| `workspace` | 10 | 9 | 1 |
| `commands` ✓ | 2 | 2 | 0 |
| `goals` ✓ | 7 | 7 | 0 |
| `job` ✓ | 3 | 3 | 0 |
| `llm` ✓ | 3 | 3 | 0 |
| `session` ✓ | 24 | 24 | 0 |
| `userQuestions` ✓ | 1 | 1 | 0 |
| `workspaceFiles` ✓ | 6 | 6 | 0 |

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

### `actionCards` (1)

- `actionCards.act`

### `agentPresets` (3)

- `agentPresets.list`
- `agentPresets.read`
- `agentPresets.select`

### `agentTeams` (1)

- `agentTeams.setEnabled`

### `claudeCodeMods` (2)

- `claudeCodeMods.pressBand`
- `claudeCodeMods.watchBand`

### `cotTranslation` (2)

- `cotTranslation.limits`
- `cotTranslation.translate`

### `dynamicCordisRunner` (12)

- `dynamicCordisRunner.getClientCode`
- `dynamicCordisRunner.inventory`
- `dynamicCordisRunner.invoke`
- `dynamicCordisRunner.reportClientGuardFailure`
- `dynamicCordisRunner.reportRenderFailure`
- `dynamicCordisRunner.resolveInspectQuery`
- `dynamicCordisRunner.resolveRequestRun`
- `dynamicCordisRunner.runHostHalf`
- `dynamicCordisRunner.settleUserRun`
- `dynamicCordisRunner.stopFromPanel`
- `dynamicCordisRunner.syncInspectManifest`
- `dynamicCordisRunner.undefineFromPanel`

### `fileUploads` (1)

- `fileUploads.upload`

### `forgePanel` (30)

- `forgePanel.branches`
- `forgePanel.cancelWorktree`
- `forgePanel.changeRequestChecks`
- `forgePanel.changeRequestCommits`
- `forgePanel.changeRequestDiff`
- `forgePanel.changeRequestReview`
- `forgePanel.changeRequestReviewWrite`
- `forgePanel.changeRequestText`
- `forgePanel.changeRequestViewed`
- `forgePanel.continueInProject`
- `forgePanel.diffBases`
- `forgePanel.dismiss`
- `forgePanel.localCommits`
- `forgePanel.localDiff`
- `forgePanel.localText`
- `forgePanel.markReady`
- `forgePanel.merge`
- `forgePanel.refresh`
- `forgePanel.requestReviews`
- `forgePanel.requestWorktree`
- `forgePanel.rerunChecks`
- `forgePanel.restoreWorktree`
- `forgePanel.retryWorktree`
- `forgePanel.reviewerCandidates`
- `forgePanel.setFileViewed`
- `forgePanel.stopCreation`
- `forgePanel.submitReview`
- `forgePanel.track`
- `forgePanel.updateBranch`
- `forgePanel.worktreeCleanup`

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

### `pluginMarket` (4)

- `pluginMarket.listIndexes`
- `pluginMarket.readEntryDetail`
- `pluginMarket.search`
- `pluginMarket.setIndexes`

### `pluginRecommendations` (1)

- `pluginRecommendations.describe`

### `pluginRegistryProbe` (1)

- `pluginRegistryProbe.fastest`

### `productAnalytics` (2)

- `productAnalytics.enabled`
- `productAnalytics.report`

### `schedule` (6)

- `schedule.catalog`
- `schedule.deleteInactive`
- `schedule.deliveryLimits`
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

### `speech` (5)

- `speech.cancelPreparation`
- `speech.catalog`
- `speech.configure`
- `speech.prepare`
- `speech.transcribe`

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

Each is either a rename still to make or a capability this build added. Listed so the
difference stays visible rather than being discovered by a client.

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

### `schedule`

- `schedule.delete`
- `schedule.get`
- `schedule.pause`
- `schedule.resume`

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

