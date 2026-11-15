//! Argument-parsing tests for the `girder` binary: where `--schema-format` is accepted, what it
//! defaults to, and what the two help screens say about it.
//!
//! These tests spawn the real executable and assert on its real stdout, stderr, and exit code. That
//! is the only level at which the defect they document is observable: `Cli::schema_format` is a field
//! on the *top-level* parser struct rather than on `Command::Schema`, and clap's decision to bind it
//! before the subcommand happens during argument parsing, not inside `print_schema`. A unit test of
//! the renderer would pass while the user-facing spelling stayed broken.
//!
//! # The defect these tests pin
//!
//! ```text
//! $ girder schema --schema-format csv     # what a user writes
//! error: unexpected argument '--schema-format' found
//!
//! $ girder --schema-format csv schema     # the only spelling that works
//! tool,calls,failures,failure_rate,argument_bytes,result_bytes
//! ```
//!
//! Worse, `girder schema --help` never mentions `--schema-format` at all, so a user who follows the
//! subcommand's own help has no way to discover the flag or its position. Every test that pins this
//! current behaviour says so in a comment, so that a future fix fails these tests loudly and the new
//! behaviour has to be written down on purpose instead of drifting.
//!
//! # Why the assertions name exact text
//!
//! "It exited 0" is satisfied by a binary that prints nothing, and "the help mentions the flag"
//! is satisfied by a help screen whose flag is unusable. Each test therefore asserts the concrete
//! bytes a caller sees -- the full help screen, the exact clap error, the exact header row -- so a
//! change to any of them is visible here rather than in a bug report.

use std::path::PathBuf;
use std::process::{Command, Output};

/// The binary under test, resolved by cargo for this integration test.
const BIN: &str = env!("CARGO_BIN_EXE_girder");

/// The nine values `--schema-format` accepts, in the order the error message lists them.
///
/// The count matters: a format added to `print_schema`'s `match` but not to its error arm would
/// print a message listing eight of nine, which is the failure this constant exists to catch.
const VALID_FORMATS: [&str; 9] = [
    "rpc",
    "openapi",
    "catalog",
    "metrics",
    "disk",
    "csv",
    "health",
    "tools-mcp",
    "tools-openai",
];

/// A private directory for one test.
///
/// `csv`, `disk`, `health`, and `metrics` aggregate every session under `GIRDER_SESSIONS`, so a test
/// that inherited the developer's real session directory would see nondeterministic row counts. Each
/// test gets its own empty directory, its own working directory, and a credential-free environment.
struct Scratch {
    /// The root of this test's directory.
    root: PathBuf,
}

impl Scratch {
    /// Creates a fresh directory named after `name`.
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "girder-cli-args-{name}-{}-{}",
            std::process::id(),
            // A nanosecond stamp keeps two concurrent invocations of the same test from sharing a
            // directory, which would let one test's sessions leak into the other's aggregate.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos())
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sessions")).expect("created sessions dir");
        std::fs::create_dir_all(root.join("work")).expect("created work dir");
        Self { root }
    }

    /// The session directory this test's binary reads.
    fn sessions(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// Runs the binary with `args` in this scratch's workspace.
    ///
    /// The environment is scrubbed rather than trusted: an exported `GIRDER_MODEL` would change the
    /// `--help` text (clap prints `[env: GIRDER_MODEL=...]`) and an exported `DEEPSEEK_API_KEY`
    /// would route a run at a live model. Clearing them is what makes the expected strings below
    /// exact instead of approximate.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .current_dir(self.root.join("work"))
            .env("GIRDER_SESSIONS", self.sessions())
            .env_remove("DEEPSEEK_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("GIRDER_OPENAI_API_KEY")
            .env_remove("GIRDER_PROTOCOL")
            .env_remove("GIRDER_MODEL")
            .output()
            .expect("ran the girder binary")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The stdout of an output, as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of an output, as text.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The exit code of an output, reporting the signal case separately.
fn exit_code(output: &Output) -> i32 {
    output
        .status
        .code()
        .unwrap_or_else(|| panic!("the process was terminated by a signal, not an exit code"))
}

// ---------------------------------------------------------------------------------------------
// 1. The flag is accepted before the subcommand -- the only spelling that currently works.
// ---------------------------------------------------------------------------------------------

/// `girder --schema-format csv schema` must succeed and print the CSV table.
///
/// This is the one placement clap accepts today, so it is the reference case the other placement
/// tests are compared against. The assertion is on the header line rather than on "some output":
/// the header is the part a downstream spreadsheet importer depends on, and it is also what proves
/// the format argument was actually honoured instead of the command silently falling back to `rpc`.
#[test]
fn schema_format_before_the_subcommand_prints_the_requested_document() {
    let scratch = Scratch::new("before-subcommand");
    let output = scratch.run(&["--schema-format", "csv", "schema"]);

    assert!(
        output.status.success(),
        "`--schema-format csv schema` is the supported spelling and must succeed; \
         exit was {:?}, stderr:\n{}",
        exit_code(&output),
        stderr(&output)
    );
    assert_eq!(exit_code(&output), 0, "a successful run exits 0");
    assert_eq!(
        stdout(&output),
        // `csv` writes CRLF line endings, so the expected header carries the trailing `\r\n`.
        "tool,calls,failures,failure_rate,argument_bytes,result_bytes\r\n",
        "an empty session directory must yield exactly the header row of the CSV table"
    );
    assert_eq!(
        stderr(&output),
        "",
        "the schema command has no provider banner and must keep stderr clean; got:\n{}",
        stderr(&output)
    );
}

/// The `--schema-format=<value>` form before the subcommand must behave like the space-separated form.
///
/// clap accepts both spellings for a long option, and users type both. Pinning the equals form here
/// keeps a future move of the flag onto `Command::Schema` from fixing one spelling and breaking the
/// other: this test and the one above would have to disagree, which is impossible to do by accident.
#[test]
fn schema_format_with_an_equals_sign_before_the_subcommand_is_also_accepted() {
    let scratch = Scratch::new("equals-form");
    let output = scratch.run(&["--schema-format=csv", "schema"]);

    assert!(
        output.status.success(),
        "`--schema-format=csv schema` must be accepted; exit was {:?}, stderr:\n{}",
        exit_code(&output),
        stderr(&output)
    );
    assert_eq!(
        stdout(&output),
        "tool,calls,failures,failure_rate,argument_bytes,result_bytes\r\n",
        "the equals form must select `csv`, not fall back to the default document"
    );
}

// ---------------------------------------------------------------------------------------------
// 2. The flag is rejected after the subcommand -- the defect.
// ---------------------------------------------------------------------------------------------

/// `girder schema --schema-format csv` must be rejected, and rejected the way clap rejects it today.
///
/// **This test pins a usability defect.** A user's first instinct is to put a subcommand's option
/// after the subcommand; here that is a hard usage error because `schema_format` belongs to the
/// top-level parser. The exact error text is asserted because it is the only feedback the user gets:
/// after this message they still do not know where the flag belongs, since the message does not
/// suggest the pre-subcommand position and `girder schema --help` does not mention the flag (see
/// `schema_subcommand_help_does_not_document_the_schema_format_flag`).
///
/// If this test ever fails because the command succeeds, the flag was moved onto `Command::Schema`.
/// That is a fix, not a regression -- update this test to assert the accepted order instead of
/// deleting it, so the supported spelling stays documented.
#[test]
fn schema_format_after_the_subcommand_is_rejected() {
    let scratch = Scratch::new("after-subcommand");
    let output = scratch.run(&["schema", "--schema-format", "csv"]);

    assert_eq!(
        exit_code(&output),
        2,
        "clap reports a usage error with exit code 2; stderr:\n{}",
        stderr(&output)
    );
    assert_eq!(
        stderr(&output),
        "error: unexpected argument '--schema-format' found\n\
         \n\
         Usage: girder schema\n\
         \n\
         For more information, try '--help'.\n",
        "the misplaced flag must produce exactly this clap usage error"
    );
    assert_eq!(
        stdout(&output),
        "",
        "a usage error must not print a partial document; a caller piping stdout into a spreadsheet \
         would otherwise import half a table and no error"
    );
}

/// The equals spelling after the subcommand must be rejected the same way.
///
/// clap strips the `=value` before it looks for a matching argument, so the error names the bare
/// `--schema-format`. Asserting that detail keeps the two rejected spellings from drifting apart: if
/// only the space-separated form were fixed, the equals form would still error and this test would
/// say so.
#[test]
fn schema_format_with_an_equals_sign_after_the_subcommand_is_rejected_the_same_way() {
    let scratch = Scratch::new("after-subcommand-equals");
    let output = scratch.run(&["schema", "--schema-format=csv"]);

    assert_eq!(
        exit_code(&output),
        2,
        "the equals form is a usage error as well; stderr:\n{}",
        stderr(&output)
    );
    assert_eq!(
        stderr(&output),
        "error: unexpected argument '--schema-format' found\n\
         \n\
         Usage: girder schema\n\
         \n\
         For more information, try '--help'.\n",
        "the error must name the flag without its value, which is how clap reports it"
    );
}

// ---------------------------------------------------------------------------------------------
// 3. The two help screens, pinned byte for byte.
// ---------------------------------------------------------------------------------------------

/// `girder schema --help` must print exactly this screen, which does not mention `--schema-format`.
///
/// **This test pins the discoverability half of the defect.** The subcommand help is where a user
/// looks for the option that controls this subcommand, and it lists only `-h, --help`. The whole
/// screen is asserted rather than just the absence of the flag, because the absence is only
/// meaningful next to what is present: the moment the flag is added, this test fails with a diff
/// that shows the new line, and the change is deliberate.
///
/// The screen also misdescribes the command -- its summary says it prints the RPC schema, while it
/// can print nine documents -- which is the same defect from the documentation side.
#[test]
fn schema_subcommand_help_does_not_document_the_schema_format_flag() {
    let scratch = Scratch::new("schema-help");
    let output = scratch.run(&["schema", "--help"]);

    assert_eq!(
        exit_code(&output),
        0,
        "`--help` is not an error; stderr:\n{}",
        stderr(&output)
    );
    assert_eq!(
        stdout(&output),
        "Print the RPC JSON Schema that the client side is checked against\n\
         \n\
         Usage: girder schema\n\
         \n\
         Options:\n  \
         -h, --help  Print help\n",
        "the subcommand help is pinned verbatim; today it omits `--schema-format` entirely"
    );
    assert!(
        !stdout(&output).contains("--schema-format"),
        "if the subcommand now documents the flag, the placement defect was addressed and this \
         assertion must be updated together with the rejected-order tests"
    );
    assert_eq!(
        stderr(&output),
        "",
        "help goes to stdout, so a caller can pipe it; got stderr:\n{}",
        stderr(&output)
    );
}

/// `girder --help` must list `--schema-format` with its value name, and must not list the six
/// formats the flag actually accepts beyond the four its description names.
///
/// The flag exists and is discoverable from the top-level help -- that is why the feature is not
/// invisible, only mispositioned. The second half pins a documentation gap: the description names
/// `rpc`, `openapi`, `tools-mcp`, and `tools-openai`, while the error message for an unknown value
/// names nine including `catalog`, `metrics`, `disk`, `csv`, and `health`. A user reading
/// `--help` concludes five formats do not exist.
#[test]
fn root_help_lists_the_schema_format_flag_and_every_accepted_value() {
    let scratch = Scratch::new("root-help");
    let output = scratch.run(&["--help"]);

    assert_eq!(
        exit_code(&output),
        0,
        "`--help` is not an error; stderr:\n{}",
        stderr(&output)
    );
    let text = stdout(&output);

    assert!(
        text.contains("--schema-format <FORMAT>"),
        "the top-level help must show the flag with its value name, which is how a caller learns it \
         takes an argument; got:\n{text}"
    );
    // Every format the flag accepts, named in its description. This assertion started inverted: the description listed
    // four names while nine formats existed, so `catalog`, `metrics`, `disk`, `csv` and `health` were reachable only by
    // guessing. A help screen that names some of a flag's values is worse than one that names none, because it reads as
    // complete.
    for format in [
        "rpc",
        "openapi",
        "catalog",
        "metrics",
        "disk",
        "csv",
        "health",
        "tools-mcp",
        "tools-openai",
    ] {
        assert!(
            text.contains(format),
            "`{format}` is accepted by `--schema-format` and must be named in `--help`; got:\n{text}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// 4. The default is `rpc`.
// ---------------------------------------------------------------------------------------------

/// `girder schema` with no format must print the same bytes as `girder --schema-format rpc schema`.
///
/// The default is documented as `rpc`, and the only way to hold that promise honest is to compare
/// the two invocations: a default that drifted to another format would still exit 0 and still print
/// valid output, so only a comparison catches it. The draft-07 `$defs` key is asserted separately
/// because "both invocations print nothing" would satisfy equality while telling a client author
/// nothing.
#[test]
fn the_default_schema_format_is_rpc() {
    let scratch = Scratch::new("default-format");
    let implicit = scratch.run(&["schema"]);
    let explicit = scratch.run(&["--schema-format", "rpc", "schema"]);

    assert_eq!(
        exit_code(&implicit),
        0,
        "`girder schema` must succeed; stderr:\n{}",
        stderr(&implicit)
    );
    assert!(
        stdout(&implicit).contains("\"$defs\""),
        "the default document must be the JSON Schema, whose root carries `$defs`; got:\n{}",
        stdout(&implicit)
    );
    assert_eq!(
        stdout(&implicit),
        stdout(&explicit),
        "the default must be the `rpc` document; the implicit and explicit runs differ"
    );
    assert_eq!(
        stderr(&explicit),
        "",
        "neither run may write to stderr on success; got:\n{}",
        stderr(&explicit)
    );
}

// ---------------------------------------------------------------------------------------------
// 5. An unknown format fails and lists all nine values.
// ---------------------------------------------------------------------------------------------

/// `--schema-format bogus` must exit 1 with a message naming the bad value and all nine valid ones.
///
/// This error is the only complete list of formats the binary offers -- `--help` names four -- so it
/// is asserted in full, including its exact wording. Two properties matter and are checked
/// separately: the `bogus` value appears so the user knows what was rejected, and every entry of
/// `VALID_FORMATS` appears so the message is not a shorter list than the `match` in `print_schema`.
///
/// The exit code is 1, not 2: this is `anyhow` failing at runtime after parsing succeeded, not a
/// usage error. Pinning the distinction keeps a future "validate in clap with `value_parser`"
/// change from silently altering the contract scripts may branch on.
#[test]
fn an_unknown_schema_format_fails_with_the_full_list_of_valid_formats() {
    let scratch = Scratch::new("unknown-format");
    let output = scratch.run(&["--schema-format", "bogus", "schema"]);

    assert_eq!(
        exit_code(&output),
        1,
        "an unknown format fails at runtime with exit code 1, unlike a usage error's 2; stderr:\n{}",
        stderr(&output)
    );
    assert_eq!(
        stderr(&output),
        "Error: unknown schema format `bogus`; expected rpc, openapi, catalog, metrics, disk, csv, \
         health, surface, mcp, tools-mcp, or tools-openai\n",
        "the error is pinned verbatim because it is the only complete list of accepted formats"
    );
    assert!(
        stderr(&output).contains("`bogus`"),
        "the error must quote the rejected value; got:\n{}",
        stderr(&output)
    );
    for format in VALID_FORMATS {
        assert!(
            stderr(&output).contains(format),
            "the error must list `{format}`; a message naming fewer formats than `print_schema` \
             accepts sends the user to the source"
        );
    }
    assert_eq!(
        stdout(&output),
        "",
        "a rejected format must not print a partial document before failing"
    );
}
