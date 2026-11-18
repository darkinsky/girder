//! Tests for the startup wiring: the `.env` policy the CLI applies before it starts, and the startup
//! audit it runs over the tools it mounted.
//!
//! # Why these are called through the crate rather than compiled by `#[path]`
//!
//! `tests/boot_app_gap.rs` compiles the module's source by path, because when it was written the
//! module was not declared in `lib.rs`. It is declared now, so these tests import
//! `girder_core::boot_app_gap` and exercise the code the binary actually links. That difference
//! matters for exactly the failure this file is about: a module that compiles in a test binary but
//! is unreachable from the crate would pass every `#[path]` test and still be dead at runtime.
//!
//! # What the `.env` tests are protecting
//!
//! The policy's own documentation states the reason plainly: a `.env` travels with a repository, so a
//! clone that could set `LD_PRELOAD` would choose which shared library loads into the parent process.
//! The same rule covers every other name in [`BOOTSTRAP_NAMES`] and every prefix in
//! [`BOOTSTRAP_PREFIXES`]. A test that only checked one name would pass while the list was emptied, so
//! these walk the whole list, and the suite's real subject is that the list is consulted at all.

use girder_core::boot_app_gap::{
    BOOTSTRAP_NAMES, BOOTSTRAP_PREFIXES, EnvErrorKind, EnvSource, InactiveEntry, InactiveOutcome,
    REQUIRED_STARTUP_ENTRY_IDS, audit_startup_entries, check_env_layer, is_bootstrap_only,
    is_home_layer_route_name, layer_env, parse_env,
};

const HOME: &str = "/home/someone/.girder";
const PROJECT: &str = "/work/checkout";

// ============================================================================================
// The bootstrap-only policy
// ============================================================================================

#[test]
fn every_bootstrap_name_is_refused_from_a_project_env() {
    // The whole list, because a single sampled name would keep passing if the list lost the rest.
    for name in BOOTSTRAP_NAMES {
        let document = format!("{name}=something\n");
        let refused = check_env_layer(PROJECT, HOME, &document)
            .expect_err(&format!("{name} must be refused from a project .env"));
        assert_eq!(refused.kind, EnvErrorKind::Refused, "{name}");
        assert_eq!(refused.name, *name);
        assert_eq!(refused.path, format!("{PROJECT}/.env"));
    }
}

#[test]
fn every_bootstrap_prefix_is_refused_from_a_project_env() {
    for prefix in BOOTSTRAP_PREFIXES {
        let name = format!("{prefix}SOMETHING");
        let document = format!("{name}=1\n");
        let refused = check_env_layer(PROJECT, HOME, &document)
            .expect_err(&format!("{name} must be refused from a project .env"));
        assert_eq!(refused.kind, EnvErrorKind::Refused, "{name}");
        assert_eq!(refused.name, name);
    }
}

#[test]
fn ld_preload_is_refused_and_the_remedy_names_exporting_it() {
    // The case the policy exists for: a shared library named by a file that arrived with the checkout
    // would load into the parent process before any of this build's code runs.
    let refused = check_env_layer(PROJECT, HOME, "LD_PRELOAD=./libevil.so\n")
        .expect_err("LD_PRELOAD must be refused");
    assert_eq!(refused.kind, EnvErrorKind::Refused);
    assert_eq!(refused.name, "LD_PRELOAD");
    assert!(
        refused.remedy.contains("export LD_PRELOAD"),
        "the remedy must say what to do instead; got {:?}",
        refused.remedy
    );
    // The rendered sentence names the file, the variable, and why the rule exists -- a person who
    // reads only this line must be able to act on it.
    let rendered = refused.to_string();
    assert!(rendered.contains(&format!("{PROJECT}/.env")), "{rendered}");
    assert!(rendered.contains("LD_PRELOAD"), "{rendered}");
    assert!(
        rendered.contains("only the launching environment may set"),
        "{rendered}"
    );
}

#[test]
fn case_does_not_smuggle_a_bootstrap_name_past_the_policy() {
    // Windows folds the case of an environment name, so a file that spelled `Path` would otherwise
    // slip through a check that compared exact strings.
    for spelling in ["Path", "path", "pAtH", "ld_preload", "node_extra_ca_certs"] {
        let document = format!("{spelling}=x\n");
        let refused = check_env_layer(PROJECT, HOME, &document)
            .expect_err(&format!("{spelling} must be refused"));
        assert_eq!(refused.kind, EnvErrorKind::Refused, "{spelling}");
    }
}

#[test]
fn an_ordinary_name_is_accepted_and_the_layer_carries_its_path() {
    let layer = check_env_layer(
        PROJECT,
        HOME,
        "GIRDER_MODEL=deepseek-chat\nLOG_LEVEL=debug\n",
    )
    .expect("ordinary names are what a .env is for");
    assert_eq!(layer.path, format!("{PROJECT}/.env"));
    assert_eq!(
        layer.values.get("GIRDER_MODEL").map(String::as_str),
        Some("deepseek-chat")
    );
    assert_eq!(
        layer.values.get("LOG_LEVEL").map(String::as_str),
        Some("debug")
    );
}

#[test]
fn the_home_layer_may_set_a_route_name_and_a_project_layer_may_not() {
    // The asymmetry the policy is built around: a proxy or an endpoint is a route the user owns, and
    // the harness-home file does not travel with a repository. The same name in the project layer is
    // refused, which is why `is_home_layer_route_name` exists as a separate predicate.
    for name in ["HTTP_PROXY", "https_proxy", "ALL_PROXY", "NO_PROXY"] {
        assert!(is_home_layer_route_name(name), "{name} is a route name");
        assert!(is_bootstrap_only(name), "{name} is bootstrap-only");
        let document = format!("{name}=http://proxy.example:3128\n");
        // The home layer may set it.
        check_env_layer(HOME, HOME, &document).expect("the harness home may set a proxy name");
        // The project layer may not.
        let refused = check_env_layer(PROJECT, HOME, &document)
            .expect_err("the project layer may not set a proxy name");
        assert_eq!(refused.kind, EnvErrorKind::Refused);
        // The remedy points at the file that *is* allowed, rather than at an export that would also work.
        assert!(
            refused.remedy.contains(HOME),
            "the remedy must name the home file it belongs in; got {:?}",
            refused.remedy
        );
    }
}

#[test]
fn an_endpoint_counts_only_in_its_exact_spelling() {
    // `DEEPSEEK_BASE_URL` is looked up by exact name on POSIX, so another spelling would load without
    // effect. Refusing it with a remedy that names the exact spelling is the difference between a
    // puzzling no-op and a fix.
    let document = "DeepSeek_BASE_URL=http://localhost\n";
    let refused = check_env_layer(HOME, HOME, document)
        .expect_err("a mis-spelled endpoint must still be refused");
    assert!(
        refused.remedy.contains("DEEPSEEK_BASE_URL"),
        "the remedy must name the exact spelling; got {:?}",
        refused.remedy
    );
}

#[test]
fn the_first_forbidden_name_in_declaration_order_is_the_one_reported() {
    // Upstream refuses the *first* offender, so a diagnostic names the variable the author wrote first
    // rather than whichever the map happened to yield. A `BTreeMap` would report `LD_PRELOAD` first
    // here, which is why the ordered parse exists.
    let document = "AAA=1\nLD_PRELOAD=./evil.so\nSSH_ASKPASS=./steal.sh\n";
    let refused = check_env_layer(PROJECT, HOME, document).expect_err("refused");
    // `AAA` is ordinary; `LD_PRELOAD` is the first refusal.
    assert_eq!(refused.name, "LD_PRELOAD");
    // Reversed document order reports the other one, which proves the order is the file's and not
    // alphabetical.
    let reversed = "SSH_ASKPASS=./steal.sh\nLD_PRELOAD=./evil.so\n";
    assert_eq!(
        check_env_layer(PROJECT, HOME, reversed)
            .expect_err("refused")
            .name,
        "SSH_ASKPASS"
    );
}

#[test]
fn a_refused_layer_applies_nothing_at_all() {
    // Validation runs before application, so the diagnostic cannot describe a half-applied file.
    let document = "FINE=1\nLD_PRELOAD=./evil.so\nALSO_FINE=2\n";
    assert!(check_env_layer(PROJECT, HOME, document).is_err());
}

#[test]
fn quoting_and_comments_do_not_hide_a_bootstrap_name() {
    // The parser is Node's `util.parseEnv`, which accepts `export`, quotes, and trailing comments. A
    // policy that ran on the raw lines would miss every one of these spellings.
    for document in [
        "export LD_PRELOAD=./evil.so\n",
        "LD_PRELOAD=\"./evil.so\"\n",
        "LD_PRELOAD='./evil.so'\n",
        "  LD_PRELOAD=./evil.so  \n",
        "LD_PRELOAD=./evil.so # a comment\n",
        "OTHER=1\r\nLD_PRELOAD=./evil.so\r\n",
    ] {
        let refused = check_env_layer(PROJECT, HOME, document)
            .unwrap_err_or_else(|| panic!("must be refused: {document:?}"));
        assert_eq!(refused.name, "LD_PRELOAD", "document was {document:?}");
    }
}

/// Disambiguates the `Result` inside the loop above without importing `expect_err`'s formatting.
trait UnwrapErrOrElse<T, E> {
    fn unwrap_err_or_else(self, otherwise: impl FnOnce() -> E) -> E;
}

impl<T, E> UnwrapErrOrElse<T, E> for Result<T, E> {
    fn unwrap_err_or_else(self, otherwise: impl FnOnce() -> E) -> E {
        match self {
            Ok(_) => otherwise(),
            Err(error) => error,
        }
    }
}

// ============================================================================================
// Layer order
// ============================================================================================

#[test]
fn the_process_outranks_the_project_which_outranks_the_home() {
    // The order is the whole point of `layer_env`: a container passes a secret through the
    // environment, and a checked-out file must not be able to override it.
    let mut inherited = std::collections::BTreeMap::new();
    inherited.insert("SHARED".to_owned(), "from-process".to_owned());
    let project =
        check_env_layer(PROJECT, HOME, "SHARED=from-project\nPROJECT_ONLY=1\n").expect("accepted");
    let home = check_env_layer(HOME, HOME, "SHARED=from-home\nHOME_ONLY=1\n").expect("accepted");

    let snapshot = layer_env(&inherited, Some(&project), Some(&home));

    let shared = snapshot.get("SHARED").expect("present");
    assert_eq!(shared.value, "from-process");
    assert_eq!(shared.source, EnvSource::Process);
    assert_eq!(shared.path, None);

    // A name the process does not set comes from the project before the home.
    let project_only = snapshot.get("PROJECT_ONLY").expect("present");
    assert_eq!(project_only.source, EnvSource::ProjectEnv);
    assert_eq!(project_only.path.as_deref(), Some("/work/checkout/.env"));

    let home_only = snapshot.get("HOME_ONLY").expect("present");
    assert_eq!(home_only.source, EnvSource::UserEnv);
    assert_eq!(
        home_only.path.as_deref(),
        Some("/home/someone/.girder/.env")
    );
}

#[test]
fn get_from_answers_whether_a_file_set_it_rather_than_what_won() {
    // The distinction a caller needs when the value in effect is not the one a file wrote: the CLI
    // reports a file-only value because only that value is inert in this build.
    let mut inherited = std::collections::BTreeMap::new();
    inherited.insert("BOTH".to_owned(), "from-process".to_owned());
    let project = check_env_layer(PROJECT, HOME, "BOTH=from-project\nFILE_ONLY=1\n").expect("ok");
    let snapshot = layer_env(&inherited, Some(&project), None);

    assert_eq!(
        snapshot.get("BOTH").map(|value| value.value.as_str()),
        Some("from-process")
    );
    assert!(
        snapshot
            .get_from("BOTH", &[EnvSource::ProjectEnv])
            .is_some(),
        "the file did set it, even though the process won"
    );
    assert!(
        snapshot
            .get_from("FILE_ONLY", &[EnvSource::Process])
            .is_none(),
        "the process did not set it"
    );
}

// ============================================================================================
// The startup audit
// ============================================================================================

/// A failed required entry.
fn failed(id: &str, module: &str, required: bool) -> InactiveEntry {
    InactiveEntry {
        id: id.to_owned(),
        module: module.to_owned(),
        required,
        outcome: InactiveOutcome::Failed {
            phase: None,
            detail: "boom".to_owned(),
        },
    }
}

/// An entry parked on services nobody registered.
fn pending(id: &str, module: &str, missing: &[&str]) -> InactiveEntry {
    InactiveEntry {
        id: id.to_owned(),
        module: module.to_owned(),
        // A pending entry is never the required failure in these tests: a required one is caught by
        // `failed` above, which is what the audit's fatal branch keys on.
        required: false,
        outcome: InactiveOutcome::Pending {
            missing: missing.iter().map(|name| (*name).to_owned()).collect(),
        },
    }
}

#[test]
fn a_healthy_tool_set_says_nothing() {
    // Every required id this build claims is satisfied, so the audit must produce no message at all:
    // an audit that warns on a working deployment is one nobody reads.
    let verdict = audit_startup_entries("girder", &[]);
    assert_eq!(verdict.error, None);
    assert_eq!(verdict.warning, None);
}

#[test]
fn a_missing_registered_capability_is_fatal_and_names_what_is_missing() {
    // The state the audit exists to catch: a build whose native tools silently did not register would
    // otherwise start and answer "no tool named" one call at a time.
    let verdict = audit_startup_entries(
        "girder",
        &[failed(
            "tool-executor-native",
            "the native file and shell tools",
            true,
        )],
    );
    let message = verdict.error.expect("a required entry failed");
    assert_eq!(
        verdict.warning, None,
        "a fatal verdict is not also a warning"
    );
    assert!(
        message.contains("1 required plugin did not activate"),
        "{message}"
    );
    assert!(
        message.contains("tool-executor-native (required)"),
        "{message}"
    );
    assert!(
        message.contains("Package: the native file and shell tools"),
        "{message}"
    );
    assert!(message.contains("boom"), "{message}");
}

#[test]
fn a_waiting_capability_only_warns_and_names_its_service() {
    // A collaborator that does not exist yet is not a defect: the tools are absent because nothing
    // provides what they need, and the warning says which service that is.
    let verdict = audit_startup_entries(
        "girder",
        &[pending(
            "web-runtime",
            "the web tools",
            &["a web provider (set GIRDER_FETCH_ENDPOINT)"],
        )],
    );
    assert_eq!(verdict.error, None);
    let warning = verdict.warning.expect("optional entries warn");
    assert!(warning.contains("web-runtime (the web tools)"), "{warning}");
    assert!(
        warning.contains("pending (waiting for service: a web provider"),
        "{warning}"
    );
    // The warning is one line of count plus one line per entry, which is the shape upstream pins.
    assert!(warning.starts_with("girder: warning: 1 entry did not activate\n"));
}

#[test]
fn only_the_required_flag_decides_which_severity_a_failure_gets() {
    // The audit's whole rule, at its smallest: the same outcome is fatal or a warning depending on
    // whether the entry was required. A test that only checked one side would pass while the flag was
    // ignored.
    let required = audit_startup_entries("girder", &[failed("x", "m", true)]);
    let optional = audit_startup_entries("girder", &[failed("x", "m", false)]);
    assert!(required.error.is_some());
    assert_eq!(required.warning, None);
    assert_eq!(optional.error, None);
    assert!(optional.warning.is_some());
}

#[test]
fn the_required_ids_are_the_ones_the_audit_reasons_about() {
    // The list is what makes an entry required *by name* rather than by a caller's flag, so its
    // membership is worth pinning: a shrinking list would quietly turn a fatal audit into a warning.
    for id in [
        "agent-loop",
        "tool-executor-native",
        "tool-executor-ptc",
        "webserver",
        "web-runtime",
    ] {
        assert!(
            REQUIRED_STARTUP_ENTRY_IDS.contains(&id),
            "`{id}` must be a required startup id"
        );
    }
}

// ============================================================================================
// The parse the policy reads
// ============================================================================================

#[test]
fn the_policy_reads_the_parsed_names_not_the_raw_lines() {
    // A policy that scanned raw lines would miss a quoted or `export`-prefixed spelling; the parse is
    // the step that normalises them, so this pins that the two halves agree.
    let parsed = parse_env("export a=1\nb=\"two\"\nc='three'\n# comment\nd=4 # trailing\n");
    assert_eq!(parsed.get("a").map(String::as_str), Some("1"));
    assert_eq!(parsed.get("b").map(String::as_str), Some("two"));
    assert_eq!(parsed.get("c").map(String::as_str), Some("three"));
    assert_eq!(parsed.get("d").map(String::as_str), Some("4"));
}
