# Port map: upstream Remote endpoints vs this build

Generated from `packages/**/src/index.ts` — every class extending `TypertRemoteService`,
its `super(ctx, key, { namespace })` line, and every `@Remote` marker.

Upstream: **187** endpoints across **37** namespaces.
Present here: **37**.

| Namespace | Upstream | Here | Missing |
| --- | --- | --- | --- |
| `account` | 11 | 0 | 11 |
| `actionCards` | 1 | 0 | 1 |
| `agentPresets` | 3 | 0 | 3 |
| `agentTeams` | 1 | 0 | 1 |
| `claudeCodeMods` | 2 | 0 | 2 |
| `commands` | 2 | 0 | 2 |
| `cotTranslation` | 2 | 0 | 2 |
| `dynamicCordisRunner` | 12 | 0 | 12 |
| `fileUploads` | 1 | 0 | 1 |
| `forgePanel` | 30 | 0 | 30 |
| `goals` | 7 | 0 | 7 |
| `job` | 3 | 3 | 0 |
| `llm` | 3 | 0 | 3 |
| `mcpManager` | 2 | 0 | 2 |
| `messageFeedback` | 3 | 0 | 3 |
| `officeKit` | 2 | 0 | 2 |
| `permissionPresets` | 1 | 0 | 1 |
| `pluginInventory` | 1 | 0 | 1 |
| `pluginManager` | 15 | 0 | 15 |
| `pluginMarket` | 4 | 0 | 4 |
| `pluginRecommendations` | 1 | 0 | 1 |
| `pluginRegistryProbe` | 1 | 0 | 1 |
| `productAnalytics` | 2 | 0 | 2 |
| `remoteAccess` | 1 | 0 | 1 |
| `schedule` | 9 | 3 | 6 |
| `session` | 25 | 19 | 6 |
| `sessionFeedback` | 1 | 0 | 1 |
| `sessionReferenceResolver` | 1 | 0 | 1 |
| `settings` | 5 | 3 | 2 |
| `speech` | 5 | 0 | 5 |
| `subagents` | 2 | 0 | 2 |
| `terminal` | 8 | 6 | 2 |
| `toolResults` | 1 | 0 | 1 |
| `userQuestions` | 2 | 1 | 1 |
| `web` | 1 | 0 | 1 |
| `workspace` | 10 | 2 | 8 |
| `workspaceFiles` | 6 | 0 | 6 |

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

### `commands` (2)

- `commands.execute`
- `commands.list`

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

### `goals` (7)

- `goals.clear`
- `goals.complete`
- `goals.create`
- `goals.edit`
- `goals.get`
- `goals.pause`
- `goals.resume`

### `llm` (3)

- `llm.discoverModels`
- `llm.listConfigurableProviders`
- `llm.listProviders`

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

### `remoteAccess` (1)

- `remoteAccess.if`

### `schedule` (6)

- `schedule.catalog`
- `schedule.deleteInactive`
- `schedule.deliveryLimits`
- `schedule.edit`
- `schedule.history`
- `schedule.run`

### `session` (6)

- `session.initializeDefaultModel`
- `session.observeEvent`
- `session.observeProjections`
- `session.observeState`
- `session.switch`
- `session.wait`

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

### `toolResults` (1)

- `toolResults.if`

### `userQuestions` (1)

- `userQuestions.if`

### `web` (1)

- `web.listProviders`

### `workspace` (8)

- `workspace.archiveSession`
- `workspace.initializeDefault`
- `workspace.insertBefore`
- `workspace.insertSessionBefore`
- `workspace.pinSession`
- `workspace.rename`
- `workspace.unarchiveSession`
- `workspace.unpinSession`

### `workspaceFiles` (6)

- `workspaceFiles.changes`
- `workspaceFiles.list`
- `workspaceFiles.read`
- `workspaceFiles.readBytes`
- `workspaceFiles.search`
- `workspaceFiles.stat`

