//! Tests for session backend selection.
//!
//! The rules under test are the ones whose failure is silent. A wrong backend does not throw: a
//! process that picks memory when it meant to pick a directory runs to completion, reports success,
//! and only afterwards is the data missing. So every test here asserts on the answer of
//! [`choose`] — and, for the capability tests, on what that answer *claims* a caller may rely on —
//! rather than on text alone.
//!
//! The module is compiled into this test binary through `#[path]` because exporting it from `lib.rs`
//! is the wiring change, which belongs to whoever connects the backend to the session RPC surface.
//! When it is exported, this can become `use girder_core::session::backend::...;`.

#[path = "../src/session/backend.rs"]
pub mod session_backend;

use session_backend::{Backend, Capabilities, Choice, Config, choose};

// ---- the default is the directory ---------------------------------------------------------------

#[test]
fn default_config_chooses_directory() {
    let choice = choose(&Config::default());

    assert_eq!(
        choice.backend,
        Backend::Directory,
        "an unconfigured harness must persist sessions, not hold them in memory"
    );
    assert!(
        choice.capabilities().durable(),
        "the default backend must survive a restart: {}",
        choice.reason
    );
}

#[test]
fn no_session_dir_and_not_ephemeral_is_still_directory() {
    // The tempting inference is "no directory configured, so use memory". It is wrong: a missing
    // configuration is the common mistake, and turning it into memory turns it into data loss.
    let config = Config {
        session_dir: None,
        ephemeral: false,
    };

    assert_eq!(choose(&config).backend, Backend::Directory);
}

#[test]
fn default_does_not_report_search_as_missing() {
    // The default backend is a directory, so search must be available on it. A default that hid the
    // search affordance would look like a client bug rather than a backend choice.
    assert!(choose(&Config::default()).capabilities().search);
}

// ---- ephemeral selects memory -------------------------------------------------------------------

#[test]
fn ephemeral_chooses_memory() {
    let config = Config {
        session_dir: None,
        ephemeral: true,
    };

    let choice = choose(&config);

    assert_eq!(
        choice.backend,
        Backend::Memory,
        "ephemeral is the only way to ask for memory, and it must take effect: {}",
        choice.reason
    );
    assert!(!choice.capabilities().durable());
}

#[test]
fn ephemeral_wins_over_a_configured_directory() {
    // Ephemeral is checked first on purpose. Honouring a directory here would let a test write into
    // a real sessions directory and overwrite logs a user still needs.
    let config = Config {
        session_dir: Some(std::path::PathBuf::from("/tmp/girder-sessions")),
        ephemeral: true,
    };

    let choice = choose(&config);

    assert_eq!(choice.backend, Backend::Memory);
    assert!(
        choice.reason.contains("ignored"),
        "the reason must say the configured directory did not take effect: {}",
        choice.reason
    );
    assert!(
        choice.reason.contains("/tmp/girder-sessions"),
        "the reason must name the directory that was ignored: {}",
        choice.reason
    );
}

// ---- an explicit directory takes precedence -----------------------------------------------------

#[test]
fn explicit_session_dir_chooses_directory_at_that_path() {
    let config = Config {
        session_dir: Some(std::path::PathBuf::from("/var/lib/girder/sessions")),
        ephemeral: false,
    };

    let choice = choose(&config);

    assert_eq!(choice.backend, Backend::Directory);
    assert!(
        choice.reason.contains("/var/lib/girder/sessions"),
        "an operator must be able to see which directory was selected: {}",
        choice.reason
    );
}

// ---- memory does not persist --------------------------------------------------------------------

#[test]
fn memory_capabilities_disclaim_persistence() {
    // The core of the ephemeral decision: if a caller cannot tell that memory loses data, nothing
    // stops it from choosing memory for a deployment. Capabilities are how it tells.
    let capabilities = Backend::Memory.capabilities();

    assert!(!capabilities.durable());
    assert_eq!(capabilities.medium, "memory");
    assert!(
        !capabilities.shared,
        "a process-local map is not shared with another process"
    );
}

#[test]
fn directory_capabilities_claim_persistence() {
    let capabilities = Backend::Directory.capabilities();

    assert!(capabilities.durable());
    assert_eq!(capabilities.medium, "directory");
    assert!(
        capabilities.shared,
        "a directory of logs is writable by more than one process"
    );
}

#[test]
fn only_the_directory_backend_is_durable() {
    // Guards the derivation in `Capabilities::durable`: a new backend that forgets to describe
    // itself would otherwise be reported as durable by an unchanged medium name.
    assert!(Backend::Directory.capabilities().durable());
    assert!(!Backend::Memory.capabilities().durable());
}

#[test]
fn memory_backend_cannot_search() {
    // Memory has no text on disk and no index, so a search built on it would answer "no matches"
    // for sessions that exist. Saying so up front is the point of `Capabilities`.
    assert!(!Backend::Memory.capabilities().search);
    assert!(Backend::Memory.capabilities().list);
    assert!(Backend::Memory.capabilities().page);
}

// ---- the choice explains itself -----------------------------------------------------------------

#[test]
fn reason_is_never_empty() {
    let cases = [
        Config::default(),
        Config {
            session_dir: Some(std::path::PathBuf::from("/tmp/sessions")),
            ephemeral: false,
        },
        Config {
            session_dir: None,
            ephemeral: true,
        },
        Config {
            session_dir: Some(std::path::PathBuf::from("/tmp/sessions")),
            ephemeral: true,
        },
    ];

    for config in cases {
        let choice = choose(&config);
        assert!(
            !choice.reason.trim().is_empty(),
            "every choice needs a reason an operator can log: {config:?}"
        );
    }
}

#[test]
fn reason_names_the_selected_backend() {
    // The reason is read next to the backend in a log line, and the two must not disagree.
    for config in [
        Config::default(),
        Config {
            session_dir: None,
            ephemeral: true,
        },
    ] {
        let choice = choose(&config);
        let expected = match choice.backend {
            Backend::Directory => "directory backend",
            Backend::Memory => "memory backend",
        };
        assert!(
            choice.reason.contains(expected),
            "reason {reason} does not name the {expected} it accompanies",
            reason = choice.reason
        );
    }
}

// ---- the wrapper types --------------------------------------------------------------------------

#[test]
fn choice_reports_the_capabilities_of_its_backend() {
    let directory = Choice::new(Backend::Directory, "test");
    assert_eq!(directory.capabilities(), Backend::Directory.capabilities());

    let memory = Choice::new(Backend::Memory, "test");
    assert_eq!(memory.capabilities(), Backend::Memory.capabilities());
}

#[test]
fn choose_agrees_with_the_backend_capability_table() {
    // `choose` must not carry its own idea of what a backend can do: the selection and the table
    // have to come from one place, or a caller can be told a backend is durable while the table
    // says otherwise.
    let choices = [
        choose(&Config::default()),
        choose(&Config {
            session_dir: Some(std::path::PathBuf::from("/tmp/elsewhere")),
            ephemeral: false,
        }),
        choose(&Config {
            session_dir: None,
            ephemeral: true,
        }),
    ];

    for choice in choices {
        let expected: Capabilities = choice.backend.capabilities();
        assert_eq!(choice.capabilities(), expected);
    }
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod session {
    pub use super::session_backend as backend;
}
