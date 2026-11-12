//! The `extension.activate`, `extension.deactivate`, and `extension.status` Remote methods: starting an
//! approved extension, stopping it again, and saying where it is.
//!
//! [`crate::extensions`] owns the whole lifecycle — approval, the service check, status, and the stopper a
//! running extension hands back — and [`crate::extension_runtime`] owns execution. What neither owns is a
//! way in from a client: [`crate::remote::register_extension_methods`] exposes `list` and `approve` only,
//! so a definition could be added and signed for and then never started through the wire. This module is
//! that entrance, and it is deliberately the thinnest one in the crate.
//!
//! # Why this module re-implements no rule
//!
//! Every decision belongs to [`ExtensionRegistry`]. `activate` checks approval first and services second,
//! and the order is load-bearing: an unapproved definition never reaches the runtime at all, and a missing
//! service costs a status change rather than a half-started process. Re-checking either condition here
//! would give the rule two implementations, and the two would eventually disagree about an edge case —
//! most likely the one where a definition is approved between the RPC's check and the registry's.
//!
//! So the handlers do three things and no more: read `id` out of the call, call the registry, and render
//! the result. Whatever the registry refuses is what the caller is told.
//!
//! # Services, and why the plain registration supplies none
//!
//! `Registry::activate` takes an `available` list: the services this process actually holds, checked
//! against the definition's `requires`. This module cannot discover that list. The crate's service
//! registry ([`crate::registry`]) is a separate component that a host mounts, and nothing in the extension
//! modules holds a reference to it — inventing a list from the extension registry's own contents would be
//! answering a different question, and a wrong answer here is the one that lets a definition run without
//! the service it declared.
//!
//! Two registrations exist for that reason:
//!
//! | function | `available` | consequence |
//! |---|---|---|
//! | [`register_extension_run_methods`] | `[]` | **every definition that declares any `requires` is refused** with `MissingServices`, at activation, after approval |
//! | [`register_extension_run_methods_with_services`] | the host's list | the check has something to check against |
//!
//! The empty default is the honest one: this process has been told about no services, so none can be
//! promised to an extension. It is not a silent failure — the activation returns an error naming every
//! service that was missing, and the extension's status becomes `failed` with the same list in the reason.
//! A deployment that wants extensions with dependencies to run must call the second function with the
//! services it can vouch for.
//!
//! Note the asymmetry with [`crate::remote::register_extension_methods`], which needs no such list: `list`
//! and `approve` never start anything, so they never consult `requires`.
//!
//! # Why `activate` and `deactivate` declare a write, and `status` a read
//!
//! The declarations are what a mounted access policy judges. Activation starts code with the harness's
//! authority and deactivation ends a process, so both change the deployment's state and are `write`.
//! Neither is `destructive`: an extension that is stopped can be started again, and the definition is
//! untouched — declaring them destructive would deny a peer something that is recoverable in one call.
//! `status` inspects and is `read`, which is what lets a read-only mirror report what is running.
//!
//! A read-only peer therefore reads `extension.status` successfully and receives `Refused` for
//! `activate` and `deactivate`, with the policy's own reason attached.
//!
//! Note that a policy cannot stop a *running* extension by refusing `deactivate`; refusing a stop is not a
//! safety property, it is a way to keep one alive after the peer that started it lost its rights. That is
//! the policy author's decision to make, and the declaration leaves it visible rather than hiding it.
//!
//! # Error mapping, and what is not invented here
//!
//! [`ExtensionError`] has richer variants than [`RemoteError`] can carry, and this module must not widen
//! [`RemoteError`] (it is shared with every other namespace). The mapping is therefore two-way only:
//!
//! - `NotFound` becomes [`RemoteError::InvalidArguments`], naming the id. An id no definition holds is a
//!   caller mistake, which is the same judgement `extension.approve` already makes.
//! - everything else becomes [`RemoteError::Failed`] with the registry's own message, unchanged. A client
//!   can show it and a reader can act on it; `NotApproved` arrives as "extension `x` has not been
//!   approved", `MissingServices` as the exact list, and neither is softened into a success.
//!
//! The consequence worth stating plainly: `NOT_APPROVED` and `MISSING_SERVICES` have no stable codes of
//! their own here, because adding them means editing [`crate::remote`], which this module does not do.
//! A client that needs to branch on them must read the message, or the status it can fetch separately.
//!
//! # Wire shapes
//!
//! ```text
//! extension.activate    {id}  -> {id, status}
//! extension.deactivate  {id}  -> {id, status}
//! extension.status      {id}  -> {id, status, approvedBy, reason?}
//! ```
//!
//! `status` is one of `stopped`, `running`, or `failed`. `approvedBy` is `null` for a definition nobody has
//! approved yet, matching `extension.list` rather than being omitted: a client that checks the field for
//! truthiness behaves the same either way, and a field that disappears is a field a typed client rejects.
//! `reason` is present only for `failed`, and holds the registry's own explanation — a status of `failed`
//! without it would report that something broke while withholding the only text that says what.
//!
//! Activating an extension that is already running is an error (`AlreadyRunning`), not a success: the
//! registry refuses it and the refusal is passed through. Deactivating one that is not running *is* a
//! success, because the caller asked for a state and the state now holds.

use std::sync::Arc;

use serde_json::{Value, json};

use girder_core::{
    Declaration, ExtensionDefinition, ExtensionError, ExtensionRegistry, ExtensionRuntime,
    ExtensionStatus, RemoteCall, RemoteError, RemoteRegistry,
};

/// The namespace every method here is served under.
pub const NAMESPACE: &str = "extension";

/// Starts an approved, service-satisfied extension.
pub const ACTIVATE_METHOD: &str = "activate";

/// Stops a running extension.
pub const DEACTIVATE_METHOD: &str = "deactivate";

/// Reports one extension's status and approver.
pub const STATUS_METHOD: &str = "status";

/// The resource kind these methods touch, as reported to an access policy.
const RESOURCE_KIND: &str = "extension";

/// Registers `activate`, `deactivate`, and `status` with no services available.
///
/// This is the registration for a deployment that has not told the extension surface about any service it
/// holds. **Every definition that declares a `requires` will be refused at activation** with
/// `MissingServices`, naming what it needed; a definition with an empty `requires` is unaffected. Use
/// [`register_extension_run_methods_with_services`] to supply the list when one exists.
///
/// # Arguments
///
/// - `registry` — the Remote method registry to add to; other namespaces are untouched.
/// - `extensions` — the extension registry that owns the lifecycle and the statuses.
/// - `runtime` — how an approved definition is started. Held as a trait object so a host chooses between
///   shell execution, a no-op recorder, or its own loader without this module changing.
pub fn register_extension_run_methods(
    registry: &mut RemoteRegistry,
    extensions: &Arc<ExtensionRegistry>,
    runtime: &Arc<dyn ExtensionRuntime>,
) {
    register_extension_run_methods_with_services(registry, extensions, runtime, &[]);
}

/// Registers `activate`, `deactivate`, and `status` against an explicit service list.
///
/// `available` is the set of services this host can vouch for, checked verbatim against each definition's
/// `requires` by the registry. It is copied once at registration rather than consulted per call: the
/// methods are registered on a registry that outlives any one request, and a list read from a live
/// component on every call would make activation's answer depend on when it was asked.
///
/// # Arguments
///
/// - `registry` — the Remote method registry to add to; other namespaces are untouched.
/// - `extensions` — the extension registry that owns the lifecycle and the statuses.
/// - `runtime` — how an approved definition is started.
/// - `available` — the services the host holds, by name. Empty means none, which is not the same as
///   "no check": the check still runs and refuses every non-empty `requires`.
pub fn register_extension_run_methods_with_services(
    registry: &mut RemoteRegistry,
    extensions: &Arc<ExtensionRegistry>,
    runtime: &Arc<dyn ExtensionRuntime>,
    available: &[String],
) {
    let available = Arc::new(available.to_vec());

    let activate_extensions = Arc::clone(extensions);
    let activate_runtime = Arc::clone(runtime);
    let activate_available = Arc::clone(&available);
    registry.register(NAMESPACE, ACTIVATE_METHOD, move |call: RemoteCall| {
        let extensions = Arc::clone(&activate_extensions);
        let runtime = Arc::clone(&activate_runtime);
        let available = Arc::clone(&activate_available);
        Box::pin(async move {
            let id = id_argument(&call, ACTIVATE_METHOD)?;
            // No approval check and no service check here: `activate` performs both, and in its own order.
            // The status it returns is the registry's, so a client cannot be told "running" for a start
            // the registry did not record.
            let status = extensions
                .activate(&id, available.as_slice(), runtime.as_ref())
                .await
                .map_err(|error| extension_failure(ACTIVATE_METHOD, &error))?;
            Ok(json!({ "id": id, "status": status.as_str() }))
        })
    });
    registry.declare(
        NAMESPACE,
        ACTIVATE_METHOD,
        Declaration::write(RESOURCE_KIND),
    );

    let deactivate_extensions = Arc::clone(extensions);
    registry.register(NAMESPACE, DEACTIVATE_METHOD, move |call: RemoteCall| {
        let extensions = Arc::clone(&deactivate_extensions);
        Box::pin(async move {
            let id = id_argument(&call, DEACTIVATE_METHOD)?;
            // Idempotent on the registry's side: stopping something already stopped succeeds, because the
            // caller asked for a state and the state now holds.
            let status = extensions
                .deactivate(&id)
                .map_err(|error| extension_failure(DEACTIVATE_METHOD, &error))?;
            Ok(json!({ "id": id, "status": status.as_str() }))
        })
    });
    registry.declare(
        NAMESPACE,
        DEACTIVATE_METHOD,
        Declaration::write(RESOURCE_KIND),
    );

    let status_extensions = Arc::clone(extensions);
    registry.register(NAMESPACE, STATUS_METHOD, move |call: RemoteCall| {
        let extensions = Arc::clone(&status_extensions);
        Box::pin(async move {
            let id = id_argument(&call, STATUS_METHOD)?;
            // The definition and the status are read under two separate locks, so they are not one atomic
            // snapshot: a concurrent activation can land between them. That is acceptable because neither
            // half is a lie — `approvedBy` only grows, and a status read a moment late is a status that
            // was true a moment ago. Faking a transaction would need a registry method this module must
            // not add.
            let definition = extensions.definition(&id).ok_or_else(|| {
                extension_failure(STATUS_METHOD, &ExtensionError::NotFound { id: id.clone() })
            })?;
            let status = extensions.status(&id).ok_or_else(|| {
                extension_failure(STATUS_METHOD, &ExtensionError::NotFound { id: id.clone() })
            })?;
            Ok(status_view(&definition, &status))
        })
    });
    registry.declare(NAMESPACE, STATUS_METHOD, Declaration::read(RESOURCE_KIND));
}

/// Reads the `id` a call names, or reports the method's argument error.
///
/// A blank id is refused here rather than passed to the registry: `ExtensionId::new` already treats the
/// empty string as unrepresentable, so forwarding it would only produce a `NotFound` for an id that could
/// never have existed, which reads as "somebody removed it" instead of "you sent nothing".
fn id_argument(call: &RemoteCall, method: &str) -> Result<String, RemoteError> {
    call.args
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            message: "`id` is required and must be a non-empty string".to_owned(),
        })
}

/// Maps a registry failure to the wire, preserving the registry's own message.
///
/// Only `NotFound` is reinterpreted, and only because an id no definition holds is a caller mistake of the
/// same kind `extension.approve` already reports that way. Everything else — not approved, missing
/// services, already running, a failed start — keeps the registry's exact text and is reported as a
/// failure, never as a shrug.
fn extension_failure(method: &str, error: &ExtensionError) -> RemoteError {
    match error {
        ExtensionError::NotFound { .. } => RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            message: error.to_string(),
        },
        _ => RemoteError::Failed(error.to_string()),
    }
}

/// Renders one extension's status as the wire object `status` returns.
///
/// `reason` is added only for `failed`: the registry keeps it there precisely so a failure can be
/// explained, and dropping it would leave a client with a status it can display and cannot understand.
fn status_view(definition: &ExtensionDefinition, status: &ExtensionStatus) -> Value {
    let mut view = json!({
        "id": definition.id.as_str(),
        "status": status.as_str(),
        // `null` rather than omitted, matching `extension.list`: a field that disappears when it has no
        // value is a field a typed client rejects.
        "approvedBy": definition.approved_by,
    });
    if let ExtensionStatus::Failed { reason } = status {
        view["reason"] = json!(reason);
    }
    view
}
