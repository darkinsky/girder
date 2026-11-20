//! The workspace and settings Remote surfaces.
//!
//! Two subsystems that share a file because they share a property: both manage *configuration* rather
//! than conversation. A workspace is a registered directory a session runs in; a setting is a value
//! the deployment reads. Neither touches the model.
//!
//! # Why a workspace is a registration rather than a directory
//!
//! The path is a host filesystem fact. The title and the session order are not: they are user
//! decisions about how work is grouped. Keeping them together means deleting a registration does not
//! delete a directory, which is the difference between closing a tab and losing a project.
//!
//! # Why settings describe rather than expose
//!
//! A client renders a form from a description — namespace, type, current value, whether it is
//! editable — and does not learn the full configuration tree. A client that knew every key would
//! render fields this deployment does not support and offer edits the server would refuse.

use crate::typert::{Catalogue, Method, ObjectSchema, Parameter, Schema};

// ---- workspace --------------------------------------------------------------------------------

/// One registered workspace, as a client renders it.
#[derive(Debug, Clone, crate::Typert)]
pub struct WorkspaceView {
    /// Its id.
    pub workspace_id: String,
    /// The host directory it registers.
    pub path: String,
    /// The user-visible title.
    pub title: String,
    /// The sessions accounted to it, in manual order.
    ///
    /// Manual rather than by creation time: a user who drags one session above another expects it to
    /// stay there.
    pub session_ids: Vec<String>,
    /// When it was registered, in milliseconds since the epoch.
    pub created_at: u64,
    /// When it was last mutated.
    pub updated_at: u64,
}

/// The workspace list.
#[derive(Debug, Clone, crate::Typert)]
pub struct WorkspaceListValue {
    /// Every registration, in manual order.
    pub workspaces: Vec<WorkspaceView>,
}

/// Asks to register a directory.
#[derive(Debug, Clone, crate::Typert)]
pub struct WorkspaceCreateRequest {
    /// The host directory.
    pub path: String,
    /// A title; omitted derives one from the directory name.
    pub title: Option<String>,
}

/// The result of a mutation that yields one workspace.
#[derive(Debug, Clone, crate::Typert)]
pub struct WorkspaceValue {
    /// The workspace after the change.
    pub workspace: WorkspaceView,
}

/// Asks to rename a workspace.
#[derive(Debug, Clone, crate::Typert)]
pub struct WorkspaceRenameRequest {
    /// Which one.
    pub workspace_id: String,
    /// The new title.
    pub title: String,
}

/// Asks to delete a registration.
#[derive(Debug, Clone, crate::Typert)]
pub struct WorkspaceDeleteRequest {
    /// Which one.
    pub workspace_id: String,
}

/// The receipt after a deletion.
#[derive(Debug, Clone, crate::Typert)]
pub struct WorkspaceDeleteValue {
    /// Always true: a refusal is an error, and a boolean that could be false would make every client
    /// check a value that never changes shape.
    ///
    /// The directory itself is untouched.
    pub deleted: bool,
}

/// Asks to archive a session inside its workspace.
#[derive(Debug, Clone, crate::Typert)]
pub struct WorkspaceArchiveSessionRequest {
    /// The workspace.
    pub workspace_id: String,
    /// The session.
    pub session_id: String,
}

/// The receipt after an archive or unarchive.
#[derive(Debug, Clone, crate::Typert)]
pub struct WorkspaceArchiveValue {
    /// The workspace after the change.
    pub workspace: WorkspaceView,
}

// ---- settings ---------------------------------------------------------------------------------

/// One setting, described for a form.
#[derive(Debug, Clone, crate::Typert)]
pub struct SettingView {
    /// The dotted key, such as `provider.model`.
    pub key: String,
    /// The value type: `string`, `number`, `boolean`, or `json`.
    ///
    /// A type name rather than a schema: the client renders an input, and a full schema would be a
    /// second implementation of the same information.
    pub value_type: String,
    /// The current value, already serialized.
    pub value: serde_json::Value,
    /// Whether a client may change it.
    ///
    /// False for a value the deployment pins, such as an endpoint derived from the environment.
    pub editable: bool,
    /// What it means, for the form's label.
    pub docs: Option<String>,
}

/// The settings a client may render.
#[derive(Debug, Clone, crate::Typert)]
pub struct SettingsDescribeValue {
    /// Every setting, in a stable order.
    pub settings: Vec<SettingView>,
}

/// Asks to change one setting.
#[derive(Debug, Clone, crate::Typert)]
pub struct SettingsUpdateRequest {
    /// The key.
    pub key: String,
    /// The new value.
    pub value: serde_json::Value,
}

/// The result of a settings change.
#[derive(Debug, Clone, crate::Typert)]
pub struct SettingsUpdateValue {
    /// The setting after the change, re-described so a client re-renders from one source.
    pub setting: SettingView,
}

// ---- methods ----------------------------------------------------------------------------------

/// The workspace subsystem's methods.
#[must_use]
pub fn workspace_methods() -> Vec<Method> {
    vec![
        Method::new("workspace", "list")
            .returning(Schema::Ref("WorkspaceListValue".to_owned()))
            .with_docs("Lists registered workspaces in manual order."),
        Method::new("workspace", "create")
            .with_parameter(Parameter::required("path", Schema::String))
            .with_parameter(Parameter::optional(
                "title",
                Schema::Optional(Box::new(Schema::String)),
            ))
            .returning(Schema::Ref("WorkspaceValue".to_owned()))
            .with_docs("Registers a directory as a workspace."),
        Method::new("workspace", "rename")
            .with_parameter(Parameter::required("workspaceId", Schema::String))
            .with_parameter(Parameter::required("title", Schema::String))
            .returning(Schema::Ref("WorkspaceValue".to_owned()))
            .with_docs("Renames a workspace."),
        Method::new("workspace", "delete")
            .with_parameter(Parameter::required("workspaceId", Schema::String))
            .returning(Schema::Ref("WorkspaceDeleteValue".to_owned()))
            .with_docs("Deletes a registration, not the directory."),
        Method::new("workspace", "archiveSession")
            .with_parameter(Parameter::required("workspaceId", Schema::String))
            .with_parameter(Parameter::required("sessionId", Schema::String))
            .returning(Schema::Ref("WorkspaceArchiveValue".to_owned()))
            .with_docs("Archives a session inside its workspace."),
    ]
}

/// The settings subsystem's methods.
#[must_use]
pub fn settings_methods() -> Vec<Method> {
    vec![
        Method::new("settings", "describe")
            .returning(Schema::Ref("SettingsDescribeValue".to_owned()))
            .with_docs("Describes every setting a client may render."),
        Method::new("settings", "update")
            .with_parameter(Parameter::required("key", Schema::String))
            .with_parameter(Parameter::required("value", Schema::Any))
            .returning(Schema::Ref("SettingsUpdateValue".to_owned()))
            .with_docs("Changes one setting."),
    ]
}

/// The workspace and settings catalogue.
#[must_use]
pub fn config_catalogue() -> Catalogue {
    let mut catalogue = Catalogue::new(1);

    for schema in [
        name(WorkspaceView::typert_schema(), "WorkspaceView"),
        name(WorkspaceListValue::typert_schema(), "WorkspaceListValue"),
        name(
            WorkspaceCreateRequest::typert_schema(),
            "WorkspaceCreateRequest",
        ),
        name(WorkspaceValue::typert_schema(), "WorkspaceValue"),
        name(
            WorkspaceRenameRequest::typert_schema(),
            "WorkspaceRenameRequest",
        ),
        name(
            WorkspaceDeleteRequest::typert_schema(),
            "WorkspaceDeleteRequest",
        ),
        name(
            WorkspaceDeleteValue::typert_schema(),
            "WorkspaceDeleteValue",
        ),
        name(
            WorkspaceArchiveSessionRequest::typert_schema(),
            "WorkspaceArchiveSessionRequest",
        ),
        name(
            WorkspaceArchiveValue::typert_schema(),
            "WorkspaceArchiveValue",
        ),
        name(SettingView::typert_schema(), "SettingView"),
        name(
            SettingsDescribeValue::typert_schema(),
            "SettingsDescribeValue",
        ),
        name(
            SettingsUpdateRequest::typert_schema(),
            "SettingsUpdateRequest",
        ),
        name(SettingsUpdateValue::typert_schema(), "SettingsUpdateValue"),
    ] {
        catalogue.define(schema);
    }

    for method in workspace_methods().into_iter().chain(settings_methods()) {
        catalogue.add(method);
    }
    catalogue
}

/// Files a schema under the name its references use.
fn name(mut schema: ObjectSchema, expected: &str) -> ObjectSchema {
    assert_eq!(
        schema.name, expected,
        "derive and reference names must agree"
    );
    schema.name.clear();
    schema.name.push_str(expected);
    schema
}

/// A workspace registration, as the server holds it.
///
/// Kept separate from [`WorkspaceView`] because the view is a wire shape: adding a field to the
/// server type must not silently change what a client receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Its id.
    pub id: String,
    /// The host directory.
    pub path: std::path::PathBuf,
    /// Its title.
    pub title: String,
    /// Its sessions, in manual order.
    pub session_ids: Vec<String>,
    /// When it was registered.
    pub created_at: u64,
    /// When it was last mutated.
    pub updated_at: u64,
}

impl Workspace {
    /// Projects into the wire view.
    #[must_use]
    pub fn view(&self, now: u64) -> WorkspaceView {
        let _ = now;
        WorkspaceView {
            workspace_id: self.id.clone(),
            path: self.path.display().to_string(),
            title: self.title.clone(),
            session_ids: self.session_ids.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// The workspace registry.
///
/// A `Mutex` rather than a lock-free structure: every operation here is a small map edit or a list
/// reorder, and the interesting invariant — the manual order — spans the whole list.
#[derive(Debug, Default)]
pub struct WorkspaceRegistry {
    workspaces: std::sync::Mutex<Vec<Workspace>>,
}

impl WorkspaceRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a directory.
    ///
    /// # Errors
    ///
    /// Returns the existing id when the path is already registered. Registering twice would give one
    /// directory two entries, and a client would show the same project twice.
    pub fn register(
        &self,
        id: impl Into<String>,
        path: impl Into<std::path::PathBuf>,
        title: Option<String>,
        now: u64,
    ) -> Result<Workspace, String> {
        let path = path.into();
        let mut workspaces = self.lock();
        if let Some(existing) = workspaces.iter().find(|workspace| workspace.path == path) {
            return Err(existing.id.clone());
        }
        let title = title.unwrap_or_else(|| {
            path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            )
        });
        let workspace = Workspace {
            id: id.into(),
            path,
            title,
            session_ids: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        workspaces.push(workspace.clone());
        Ok(workspace)
    }

    /// Every registration, in manual order.
    #[must_use]
    pub fn list(&self) -> Vec<Workspace> {
        self.lock().clone()
    }

    /// Finds one.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Workspace> {
        self.lock()
            .iter()
            .find(|workspace| workspace.id == id)
            .cloned()
    }

    /// Renames one.
    ///
    /// # Errors
    ///
    /// Returns `None` when the id is unknown.
    pub fn rename(&self, id: &str, title: &str, now: u64) -> Option<Workspace> {
        let mut workspaces = self.lock();
        let workspace = workspaces.iter_mut().find(|workspace| workspace.id == id)?;
        workspace.title.clear();
        workspace.title.push_str(title);
        workspace.updated_at = now;
        Some(workspace.clone())
    }

    /// Deletes a registration.
    ///
    /// The directory is untouched: this closes a tab, it does not delete a project.
    pub fn delete(&self, id: &str) -> bool {
        let mut workspaces = self.lock();
        let before = workspaces.len();
        workspaces.retain(|workspace| workspace.id != id);
        workspaces.len() != before
    }

    /// Moves a workspace earlier in the manual order.
    ///
    /// # Errors
    ///
    /// Returns `None` when either id is unknown.
    pub fn insert_before(&self, id: &str, before: &str, now: u64) -> Option<Vec<Workspace>> {
        let mut workspaces = self.lock();
        let from = workspaces.iter().position(|workspace| workspace.id == id)?;
        let to = workspaces
            .iter()
            .position(|workspace| workspace.id == before)?;
        if from == to {
            return Some(workspaces.clone());
        }
        let workspace = workspaces.remove(from);
        // Removing an earlier element shifts the target left by one.
        let to = if from < to { to - 1 } else { to };
        let mut workspace = workspace;
        workspace.updated_at = now;
        workspaces.insert(to, workspace);
        Some(workspaces.clone())
    }

    /// Attaches a session to a workspace.
    ///
    /// Idempotent: attaching the same session twice leaves it listed once. A duplicate would render
    /// twice in a sidebar and make a drag reorder ambiguous.
    ///
    /// # Errors
    ///
    /// Returns `None` when the workspace is unknown.
    pub fn attach_session(&self, workspace: &str, session: &str, now: u64) -> Option<Workspace> {
        let mut workspaces = self.lock();
        let found = workspaces.iter_mut().find(|w| w.id == workspace)?;
        if !found.session_ids.iter().any(|id| id == session) {
            found.session_ids.push(session.to_owned());
        }
        found.updated_at = now;
        Some(found.clone())
    }

    /// Archives a session inside its workspace.
    ///
    /// # Errors
    ///
    /// Returns `None` when either id is unknown.
    pub fn archive_session(&self, workspace: &str, session: &str, now: u64) -> Option<Workspace> {
        let mut workspaces = self.lock();
        let found = workspaces.iter_mut().find(|w| w.id == workspace)?;
        if !found.session_ids.iter().any(|id| id == session) {
            return None;
        }
        found.session_ids.retain(|id| id != session);
        found.updated_at = now;
        Some(found.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Workspace>> {
        self.workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
