//! `sessionReferenceResolver.candidates`: the sessions a `@`-mention may point at.
//!
//! # What upstream has
//!
//! `packages/context/session-reference/src/index.ts` marks one method. A person typing `@` in a
//! prompt needs a short list of sessions they might mean, ranked so the ones in the same directory
//! come first -- a mention usually means "that other conversation about this repository".
//!
//! Upstream's filter matches four fields: the id, the working directory, the projected label, and
//! the display title. The ranking is `candidateRank(cwd, targetCwd)` and then the record's index,
//! which keeps the catalogue's own order among equally-good matches.
//!
//! # What this port decides differently
//!
//! Upstream's `label` is a projected value a session may have recorded ("this conversation is about
//! the importer"). This build has session titles but no separate label projection, so `label` and
//! `displayTitle` come from the same source. Reported rather than faked: a caller that shows both
//! columns will see the same string twice, and that is the truth about what this build knows.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::remote::{RemoteCall, RemoteError, RemoteRegistry, SessionCatalog};

/// The namespace this method registers under, as upstream spells it.
pub const NAMESPACE: &str = "sessionReferenceResolver";

/// The one endpoint.
pub const CANDIDATES_METHOD: &str = "candidates";

/// How many candidates a caller gets when it names no limit.
///
/// Upstream's `candidateLimit`. A mention picker shows a handful; a list longer than this is one
/// nobody scans.
pub const DEFAULT_CANDIDATE_LIMIT: usize = 10;

/// The largest limit a caller may ask for.
///
/// Bounded because each candidate costs a catalogue read, and a client asking for a thousand would
/// turn one keystroke into a directory walk.
pub const MAX_CANDIDATE_LIMIT: usize = 100;

/// One session a mention could point at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The session id, which is what a mention resolves to.
    pub session_id: String,
    /// A short label.
    pub label: String,
    /// A display title.
    pub display_title: String,
    /// The session's working directory, when it recorded one.
    pub cwd: Option<String>,
    /// Whether that directory is the caller's.
    pub same_workspace: bool,
}

/// Ranks a candidate: same workspace first, then catalogue order.
///
/// Upstream's `candidateRank`. Expressed as a `u8` so the sort is total: `0` is the caller's own
/// directory, `1` is any other, and a session with no directory is `2` -- it cannot be the one the
/// caller means, but it is still a candidate.
fn rank(candidate: &Candidate) -> u8 {
    if candidate.same_workspace {
        0
    } else if candidate.cwd.is_some() {
        1
    } else {
        2
    }
}

/// Whether a candidate matches a lowered needle.
///
/// Upstream matches four fields, and each exists for a reason: a person may remember the id from a
/// log, the directory they were in, what the session was called, or what they wrote about it.
fn matches(candidate: &Candidate, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    candidate.session_id.to_lowercase().contains(needle)
        || candidate
            .cwd
            .as_deref()
            .is_some_and(|cwd| cwd.to_lowercase().contains(needle))
        || candidate.label.to_lowercase().contains(needle)
        || candidate.display_title.to_lowercase().contains(needle)
}

/// Renders one candidate for the wire.
fn to_wire(candidate: &Candidate) -> Value {
    let mut value = json!({
        "sessionId": candidate.session_id,
        "label": candidate.label,
        "displayTitle": candidate.display_title,
        "sameWorkspace": candidate.same_workspace,
    });
    if let Some(cwd) = &candidate.cwd {
        value["cwd"] = json!(cwd);
    }
    value
}

/// The candidates for one query, ordered as upstream orders them.
///
/// # Errors
///
/// The invalid limit, which upstream reports as `SESSION_REFERENCE_INVALID_REFERENCE` rather than
/// clamping: a caller that asked for zero asked for something impossible, and silently answering
/// with ten would hide the mistake.
pub fn candidates(
    records: &[Value],
    query: &str,
    target_cwd: Option<&str>,
    exclude: Option<&str>,
    limit: i64,
) -> Result<Vec<Candidate>, String> {
    if limit <= 0 {
        return Err("candidate limit must be a positive integer".to_owned());
    }
    let limit = usize::try_from(limit).unwrap_or(MAX_CANDIDATE_LIMIT);
    if limit > MAX_CANDIDATE_LIMIT {
        return Err(format!(
            "candidate limit must be at most {MAX_CANDIDATE_LIMIT}"
        ));
    }
    let needle = query.to_lowercase();
    let mut found: Vec<(u8, usize, Candidate)> = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let session_id = record
            .get("id")
            .or_else(|| record.get("sessionId"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if session_id.is_empty() {
            continue;
        }
        // The caller's own session is never a candidate: a mention of yourself resolves to
        // nothing. The exclusion is by id, which the caller passes separately from its directory --
        // comparing the id to a path was a mistake in an earlier draft of this function.
        if exclude == Some(session_id.as_str()) {
            continue;
        }
        let title = record
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(&session_id)
            .to_owned();
        let cwd = record.get("cwd").and_then(Value::as_str).map(str::to_owned);
        let same_workspace = match (&cwd, target_cwd) {
            (Some(cwd), Some(target)) => cwd == target,
            _ => false,
        };
        let candidate = Candidate {
            session_id,
            // This build has no separate label projection, so the title serves as both.
            label: title.clone(),
            display_title: title,
            cwd,
            same_workspace,
        };
        if !matches(&candidate, &needle) {
            continue;
        }
        found.push((rank(&candidate), index, candidate));
    }
    // Same workspace first, then the catalogue's own order. Sorting on the index rather than the id
    // keeps a caller's most-recent-first listing intact among equal matches.
    found.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    Ok(found.into_iter().map(|(_, _, c)| c).take(limit).collect())
}

/// Registers `sessionReferenceResolver.candidates`.
pub fn register_session_reference_methods(
    registry: &mut RemoteRegistry,
    catalog: &std::sync::Arc<dyn SessionCatalog>,
) {
    let listing = std::sync::Arc::clone(catalog);
    registry.register(NAMESPACE, CANDIDATES_METHOD, move |call: RemoteCall| {
        let catalog = std::sync::Arc::clone(&listing);
        Box::pin(async move {
            let query = call.args.get("query").and_then(Value::as_str).unwrap_or("");
            let target_cwd = call.args.get("cwd").and_then(Value::as_str);
            // Omitted means "the default", and zero means zero: a caller that sent `0` asked for
            // something impossible, and `candidates` reports it rather than clamping.
            let limit = call
                .args
                .get("limit")
                .and_then(Value::as_i64)
                .unwrap_or(i64::try_from(DEFAULT_CANDIDATE_LIMIT).unwrap_or(i64::MAX));
            let listed = catalog.list().await;
            // The title lives in the log, not on the record: the newest `session/title` event is
            // what a picker shows. A session that never had one falls back to its id, which is what
            // the candidate shape does upstream too.
            let records: Vec<Value> = listed
                .iter()
                .map(|record| {
                    let title = record
                        .events
                        .iter()
                        .rev()
                        .find_map(|event| match event {
                            crate::session::SessionEvent::SessionTitle { title, .. } => {
                                Some(title.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| record.id.clone());
                    json!({ "id": record.id, "title": title })
                })
                .collect();
            let found =
                candidates(&records, query, target_cwd, None, limit).map_err(|message| {
                    RemoteError::InvalidArguments {
                        namespace: NAMESPACE.to_owned(),
                        method: CANDIDATES_METHOD.to_owned(),
                        message,
                    }
                })?;
            Ok(json!({
                "candidates": found.iter().map(to_wire).collect::<Vec<_>>(),
            }))
        })
    });
    registry.declare(
        NAMESPACE,
        CANDIDATES_METHOD,
        crate::access::Declaration::read("session"),
    );
}

/// A lookup from session id to the label a caller would recognise.
///
/// Exposed because a resolver that reused this and a picker that did not would disagree about what a
/// mention is called.
#[must_use]
pub fn labels(candidates: &[Candidate]) -> BTreeMap<String, String> {
    candidates
        .iter()
        .map(|candidate| (candidate.session_id.clone(), candidate.label.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, title: &str, cwd: Option<&str>) -> Value {
        match cwd {
            Some(cwd) => json!({ "id": id, "title": title, "cwd": cwd }),
            None => json!({ "id": id, "title": title }),
        }
    }

    #[test]
    fn the_same_workspace_ranks_first() {
        let records = vec![
            record("other", "Other", Some("/elsewhere")),
            record("mine", "Mine", Some("/repo")),
        ];
        let found = candidates(&records, "", Some("/repo"), None, 10).expect("ok");
        assert_eq!(found[0].session_id, "mine");
        assert!(found[0].same_workspace);
        assert!(!found[1].same_workspace);
    }

    #[test]
    fn a_session_with_no_directory_ranks_last() {
        // It cannot be the one the caller means, having recorded no place to be in -- but it is
        // still a candidate rather than filtered out.
        let records = vec![
            record("nowhere", "Nowhere", None),
            record("other", "Other", Some("/elsewhere")),
        ];
        let found = candidates(&records, "", Some("/repo"), None, 10).expect("ok");
        assert_eq!(found[0].session_id, "other");
        assert_eq!(found[1].session_id, "nowhere");
    }

    #[test]
    fn equal_ranks_keep_the_catalogue_order() {
        // A caller listing most-recent-first expects that order to survive among equal matches.
        let records = vec![
            record("b", "B", Some("/elsewhere")),
            record("a", "A", Some("/elsewhere")),
        ];
        let found = candidates(&records, "", Some("/repo"), None, 10).expect("ok");
        let ids: Vec<&str> = found.iter().map(|c| c.session_id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a"]);
    }

    #[test]
    fn every_field_a_person_might_remember_matches() {
        let records = vec![record("session-42", "The importer", Some("/repo/sub"))];
        for needle in ["session-42", "importer", "repo"] {
            let found = candidates(&records, needle, None, None, 10).expect("ok");
            assert_eq!(found.len(), 1, "`{needle}` should match");
        }
    }

    #[test]
    fn the_callers_own_session_is_never_a_candidate() {
        // A mention of yourself resolves to nothing, so it must not appear in the picker.
        let records = vec![
            record("self", "Me", Some("/repo")),
            record("other", "Other", Some("/repo")),
        ];
        let found = candidates(&records, "", Some("/repo"), Some("self"), 10).expect("ok");
        let ids: Vec<&str> = found.iter().map(|c| c.session_id.as_str()).collect();
        assert_eq!(ids, vec!["other"]);
    }

    #[test]
    fn a_zero_limit_is_refused_rather_than_clamped() {
        // Answering ten would hide the mistake.
        let error = candidates(&[], "", None, None, 0).expect_err("refused");
        assert!(error.contains("positive"), "{error}");
    }

    #[test]
    fn a_limit_past_the_cap_is_refused() {
        let error = candidates(&[], "", None, None, 1_000).expect_err("refused");
        assert!(error.contains("at most"), "{error}");
    }

    #[test]
    fn the_limit_caps_the_answer() {
        let records: Vec<Value> = (0..20)
            .map(|index| record(&format!("s{index}"), "t", None))
            .collect();
        let found = candidates(&records, "", None, None, 3).expect("ok");
        assert_eq!(found.len(), 3);
    }
}
