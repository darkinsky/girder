//! Tests for the host's serving half: bind-address classification, compression eligibility, the route
//! table, index injection rendering, and static dist serving.
//!
//! The properties are transcribed from the upstream suites that pin them:
//! `packages/host/webserver/tests/webserver.spec.ts` for the address predicates, `normalizeBindAddress`,
//! `bindUrlHost`, `getOrigin`, and `renderIndexInjections`, and
//! `packages/host/frontend-static/tests/frontend-static.spec.ts` for MIME, ETag/304, the traversal
//! fence, empty 404s, and 405.
//!
//! What is deliberately *not* asserted is anything that needs a socket: this module decides and
//! renders, and the transport is out of scope. Every property below is therefore a fact about a pure
//! function except the static-server cases, which read a real directory the test creates.
//!
//! # Why the module is compiled by path
//!
//! `host_gap` is not exported from the crate root yet; wiring belongs to a separate change to
//! `lib.rs`. Compiling the exact source file here keeps these tests runnable before that wiring, and
//! it is why the module reaches nothing through `crate::` — under `#[path]` inclusion, `crate::` names
//! this test binary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/host/webserver.rs"]
pub mod host_gap;

use std::fs;
use std::path::PathBuf;

use host_gap::{
    BindAddress, BindAddressError, Compression, DEFAULT_COMPRESSION_LEVEL,
    DEFAULT_COMPRESSION_THRESHOLD_BYTES, FallbackError, IndexInjection, OCTET_STREAM_MIME,
    OriginError, Placement, READY_MARKUP, RESOURCE_METHODS, ResourceAnswer, ResourceResponse,
    Responder, RouteKind, StaticConfig, StaticRequest, TlsConfig, TlsError, WebRouter,
    WebServerConfig, accepts_gzip, apply_index_taps, base64url, bind_url_host,
    escape_html_attribute, gzip_eligible, is_compressible, is_loopback_host, is_resource_method,
    is_wildcard_host, load_tls_material, mime_for_path, normalize_bind_address, parse_bind_address,
    pem_labels, render_index_injections, serve_static, sha1, validate_bind_host, weak_etag,
};

// ---- helpers ------------------------------------------------------------------------------------

/// Creates a fresh temp directory this test owns.
///
/// Hand-rolled rather than dev-dependency-driven: the pid and a counter keep two concurrent test
/// binaries from colliding, and the directory is only ever removed by the caller's own path.
fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-host-gap-{tag}-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A `Resource` responder carrying a label, so a match can be named in an assertion.
fn resource(label: &'static str) -> Responder<&'static str> {
    Responder::Resource(label)
}

// ---- bind addresses -----------------------------------------------------------------------------

#[test]
fn classifies_loopback_from_the_parsed_value_not_the_spelling() {
    // Upstream webserver.spec.ts: "classifies loopback bind addresses from the parsed value,
    // including mapped and zone forms".
    for host in [
        "127.0.0.1",
        "127.8.9.10",
        "127.5.5.5",
        "::1",
        "0:0:0:0:0:0:0:1",
        "::ffff:127.0.0.1",
        "::ffff:7f00:1",
        "::1%lo",
        "::0.0.0.1",
        "::0.0.0.1%lo",
    ] {
        assert!(is_loopback_host(host), "{host} should read as loopback");
    }
    // A dotted-quad tail is the address's own low bits, never an IPv4 tail: `::127.0.0.1` is the
    // unrelated IPv6 address `::7f00:1`, not IPv4 loopback.
    for host in [
        "10.1.2.3",
        "::ffff:10.1.2.3",
        "fd00::1",
        "fe80::1%lo",
        "::2",
        "localhost",
        "::127.0.0.1",
        "::127.0.0.1%lo",
        "::0.0.0.2",
    ] {
        assert!(!is_loopback_host(host), "{host} must not read as loopback");
    }
}

#[test]
fn reads_a_dotted_quad_ipv6_tail_as_the_address_it_names() {
    // Upstream: "reads a dotted-quad IPv6 tail as the address it names".
    let rows: [(&str, &str); 8] = [
        ("::0.0.0.1", "::1"),
        ("::0.0.0.1%lo", "::1"),
        ("::127.0.0.1", "::7f00:1"),
        ("::1", "::1"),
        ("10.1.2.3", "10.1.2.3"),
        ("::ffff:127.0.0.1", "127.0.0.1"),
        ("::ffff:7f00:1", "127.0.0.1"),
        ("::ffff:10.1.2.3", "10.1.2.3"),
    ];
    for (host, text) in rows {
        assert_eq!(
            normalize_bind_address(host).as_deref(),
            Ok(text),
            "normalize {host}"
        );
    }
    assert_eq!(
        normalize_bind_address("localhost"),
        Err(BindAddressError::NotLiteral("localhost".to_owned()))
    );
}

#[test]
fn writes_bind_addresses_in_their_url_host_form() {
    // Upstream: "writes bind addresses in their URL host form".
    let rows: [(&str, Option<&str>); 8] = [
        ("127.0.0.1", Some("127.0.0.1")),
        ("::ffff:127.0.0.1%lo", Some("127.0.0.1")),
        ("::1", Some("[::1]")),
        ("::0.0.0.1%lo", Some("[::1]")),
        ("10.1.2.3", Some("10.1.2.3")),
        ("fd00::1", Some("[fd00::1]")),
        // A zone selects an interface and no URL can carry it; only a loopback zone is redundant.
        ("fe80::1%eth0", None),
        ("::127.0.0.1%lo", None),
    ];
    for (host, text) in rows {
        match text {
            Some(text) => assert_eq!(
                bind_url_host(host).as_deref(),
                Ok(text),
                "bindUrlHost {host}"
            ),
            None => assert_eq!(
                bind_url_host(host),
                Err(BindAddressError::ZoneNotExpressible(host.to_owned())),
                "bindUrlHost {host}"
            ),
        }
    }
}

#[test]
fn refuses_non_literals_and_unpadded_or_malformed_forms() {
    for host in [
        "localhost",
        "",
        "127.0.0",
        "127.0.0.1.5",
        "256.0.0.1",
        "010.0.0.1",
        "1:2:3:4:5:6:7",
        "1:2:3:4:5:6:7:8:9",
        "1::2::3",
        "::g",
        "[::1]",
        "999.1.1.1",
    ] {
        assert_eq!(
            parse_bind_address(host),
            None,
            "{host} is not a concrete literal"
        );
        assert!(!is_loopback_host(host));
        assert!(!is_wildcard_host(host));
    }
    // Full eight-group form, no `::`, still parses.
    assert_eq!(
        parse_bind_address("0:0:0:0:0:0:0:1"),
        Some(BindAddress::V6([0, 0, 0, 0, 0, 0, 0, 1]))
    );
    // A single zero group is not abbreviated: `0:0:0:0:0:0:0:2`, never `::2` written oddly.
    assert_eq!(
        normalize_bind_address("0:0:0:0:0:0:0:2").as_deref(),
        Ok("::2")
    );
    assert_eq!(
        normalize_bind_address("1:0:0:2:0:0:3:4").as_deref(),
        Ok("1::2:0:0:3:4")
    );
    // A tie between two equal zero runs abbreviates the first, which is what ipaddr.js writes.
    assert_eq!(
        normalize_bind_address("1::2:0:0:3:4").as_deref(),
        Ok("1::2:0:0:3:4")
    );
}

#[test]
fn rejects_the_wildcard_address_the_config_transform_rejects() {
    for host in [
        "0.0.0.0",
        "::",
        "::ffff:0.0.0.0",
        "0:0:0:0:0:0:0:0",
        "::0.0.0.0",
    ] {
        assert!(is_wildcard_host(host), "{host} binds every interface");
        assert_eq!(
            validate_bind_host(host),
            Err(BindAddressError::Wildcard(host.to_owned()))
        );
    }
    // 0/8 is "unspecified" to ipaddr.js, but only the all-zero address binds every interface.
    for host in ["0.0.0.1", "127.0.0.1", "::1", "10.0.0.1"] {
        assert!(!is_wildcard_host(host), "{host} names one interface");
        assert_eq!(validate_bind_host(host), Ok(()));
    }
    assert_eq!(
        validate_bind_host("localhost"),
        Err(BindAddressError::NotLiteral("localhost".to_owned()))
    );
}

// ---- configuration and origin --------------------------------------------------------------------

#[test]
fn applies_the_documented_defaults_and_reads_the_protocol_from_configuration() {
    let config = WebServerConfig::new("127.0.0.1", 0);
    assert_eq!(config.compression, Compression::None);
    assert_eq!(config.compression_level, DEFAULT_COMPRESSION_LEVEL);
    assert_eq!(
        config.compression_threshold_bytes,
        DEFAULT_COMPRESSION_THRESHOLD_BYTES
    );
    assert_eq!(config.protocol(), "http:");
    assert_eq!(config.validate(), Ok(()));
    // The protocol follows configuration, never a request: the TLS variant reports https before any
    // listener exists.
    let https = config.clone().with_tls(TlsConfig {
        cert_file: PathBuf::from("/x/cert.pem"),
        key_file: PathBuf::from("/x/key.pem"),
    });
    assert_eq!(https.protocol(), "https:");
    assert_eq!(
        https.origin(Some(3080)).as_deref(),
        Ok("https://127.0.0.1:3080")
    );
    assert_eq!(
        config.origin(Some(3080)).as_deref(),
        Ok("http://127.0.0.1:3080")
    );
    // A plain-HTTP configuration has no material to load, and the threshold is tunable.
    assert_eq!(config.load_tls(), Ok(None));
    let tuned = config.clone().with_compression_threshold(2048);
    assert_eq!(tuned.compression_threshold_bytes, 2048);
    assert_eq!(config.origin(Some(0)).as_deref(), Ok("http://127.0.0.1:0"));
}

#[test]
fn reports_no_origin_before_binding_or_for_an_address_no_url_can_name() {
    // Upstream: "reports no origin before binding or for a bind address that no URL can express".
    let config = WebServerConfig::new("127.0.0.1", 0);
    assert_eq!(config.origin(None), Err(OriginError::NotBound));
    assert_eq!(
        config.origin(None).unwrap_err().to_string(),
        "webserver: origin read before the listener bound a port"
    );
    let zoned = WebServerConfig::new("fe80::1%eth0", 0);
    assert_eq!(
        zoned.origin(Some(1)),
        Err(OriginError::Bind(BindAddressError::ZoneNotExpressible(
            "fe80::1%eth0".to_owned()
        )))
    );
    assert_eq!(
        zoned.origin(Some(1)).unwrap_err().to_string(),
        "webserver: bind address \"fe80::1%eth0\" carries an interface zone id, which no URL can \
         express"
    );
}

#[test]
fn rejects_a_compression_level_outside_the_schema_range() {
    let mut config = WebServerConfig::new("127.0.0.1", 0).with_gzip(10);
    assert_eq!(
        config.validate(),
        Err(host_gap::WebServerConfigError::CompressionLevel(10))
    );
    config.compression_level = 9;
    assert_eq!(config.validate(), Ok(()));
    config.compression_level = 0;
    assert_eq!(config.validate(), Ok(()));
}

#[test]
fn rejects_an_empty_tls_path_before_any_read() {
    let config = WebServerConfig::new("127.0.0.1", 0).with_tls(TlsConfig {
        cert_file: PathBuf::new(),
        key_file: PathBuf::from("/x/key.pem"),
    });
    assert_eq!(
        config.validate(),
        Err(host_gap::WebServerConfigError::MissingTlsPath("certFile"))
    );
}

// ---- TLS material --------------------------------------------------------------------------------

/// One self-signed-shaped PEM pair: real PEM framing with placeholder base64. The module checks
/// framing and shape, not cryptography, so a fixture that would never complete a handshake is the
/// honest input for these tests.
const CERT_PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIBkTCB+w==\n-----END CERTIFICATE-----\n";
const KEY_PEM: &str =
    "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49==\n-----END PRIVATE KEY-----\n";

#[test]
fn reads_a_pem_pair_and_reports_the_failures_upstream_reports() {
    let dir = temp_dir("tls");
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    fs::write(&cert, CERT_PEM).expect("write cert");
    fs::write(&key, KEY_PEM).expect("write key");
    let material = load_tls_material(&cert, &key).expect("a framed pair loads");
    assert_eq!(material.cert_pem, CERT_PEM);
    assert_eq!(material.key_pem, KEY_PEM);

    // A missing file names the field and the path.
    let missing = dir.join("nope.pem");
    let error = load_tls_material(&missing, &key).unwrap_err();
    assert!(
        matches!(&error, TlsError::Read { field, .. } if *field == "certFile"),
        "unexpected: {error}"
    );
    assert!(
        error
            .to_string()
            .starts_with("webserver: cannot read tls.certFile ")
    );

    // Blank content is rejected here because node:tls accepts it silently and then cannot complete a
    // handshake.
    let blank = dir.join("blank.pem");
    fs::write(&blank, "  \n\t\n").expect("write blank");
    assert_eq!(
        load_tls_material(&blank, &key),
        Err(TlsError::Empty {
            field: "certFile",
            path: blank.clone()
        })
    );

    // A file that holds no CERTIFICATE block is not a certificate.
    let wrong = dir.join("wrong.pem");
    fs::write(&wrong, KEY_PEM).expect("write wrong");
    assert!(matches!(
        load_tls_material(&wrong, &key),
        Err(TlsError::Unusable { .. })
    ));
    // And a key file that holds no PRIVATE KEY block is not a key.
    assert!(matches!(
        load_tls_material(&cert, &cert),
        Err(TlsError::Unusable { .. })
    ));
    // The configuration's own loader reports the same failure through its own error.
    let configured = WebServerConfig::new("127.0.0.1", 0).with_tls(TlsConfig {
        cert_file: blank.clone(),
        key_file: key.clone(),
    });
    assert_eq!(
        configured.load_tls(),
        Err(host_gap::WebServerConfigError::Tls(TlsError::Empty {
            field: "certFile",
            path: blank,
        }))
    );
    let good = WebServerConfig::new("127.0.0.1", 0).with_tls(TlsConfig {
        cert_file: cert,
        key_file: key,
    });
    assert_eq!(
        good.load_tls()
            .map(|material| material.map(|it| it.cert_pem)),
        Ok(Some(CERT_PEM.to_owned()))
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn counts_a_pem_block_only_when_its_end_line_matches() {
    assert_eq!(pem_labels(CERT_PEM), vec!["CERTIFICATE".to_owned()]);
    assert_eq!(
        pem_labels("-----BEGIN CERTIFICATE-----\nAAAA\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        pem_labels("-----BEGIN CERTIFICATE-----\nA\n-----END PRIVATE KEY-----\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        pem_labels("-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----\nmore"),
        vec!["CERTIFICATE".to_owned()]
    );
}

// ---- compression ---------------------------------------------------------------------------------

#[test]
fn negotiates_gzip_by_q_value_with_the_wildcard_losing_to_an_explicit_entry() {
    assert!(accepts_gzip("gzip"));
    assert!(accepts_gzip("gzip, deflate, br"));
    assert!(accepts_gzip("gzip;q=0.5"));
    assert!(accepts_gzip("GZIP"));
    // An explicit refusal beats the wildcard.
    assert!(!accepts_gzip("gzip;q=0"));
    assert!(!accepts_gzip("gzip;q=0.000, *"));
    // The wildcard alone admits gzip.
    assert!(accepts_gzip("*"));
    assert!(!accepts_gzip("*;q=0"));
    // No entry at all is a refusal: identity only.
    assert!(!accepts_gzip(""));
    assert!(!accepts_gzip("identity"));
    assert!(!accepts_gzip("br, deflate"));
    // An unparsable quality falls back to the default rather than disabling gzip everywhere.
    assert!(accepts_gzip("gzip;q=abc"));
}

#[test]
fn decides_eligibility_from_the_upstream_filter() {
    let threshold = 1024;
    let ok =
        |length: Option<usize>| gzip_eligible("gzip", Some("text/plain"), None, length, threshold);
    assert!(ok(None), "an unknown length is eligible");
    assert!(ok(Some(threshold)), "at the threshold is eligible");
    assert!(!ok(Some(threshold - 1)), "below the threshold is not");
    // A client that does not accept gzip gets identity.
    assert!(!gzip_eligible(
        "identity",
        Some("text/plain"),
        None,
        None,
        threshold
    ));
    // A partial body's length arithmetic does not survive compression.
    assert!(!gzip_eligible(
        "gzip",
        Some("text/plain"),
        Some("bytes 0-9/100"),
        None,
        threshold
    ));
    // An event stream is incremental and flushing per event defeats compression.
    assert!(!gzip_eligible(
        "gzip",
        Some("text/event-stream; charset=utf-8"),
        None,
        None,
        threshold
    ));
    // Multipart bodies are compressed regardless of threshold; they are large by construction.
    assert!(gzip_eligible(
        "gzip",
        Some("multipart/form-data; boundary=x"),
        None,
        Some(10),
        threshold
    ));
    // An already-compressed type is not compressed again.
    for content_type in ["image/png", "font/woff2", "application/gzip"] {
        assert!(!is_compressible(content_type), "{content_type}");
        assert!(!gzip_eligible(
            "gzip",
            Some(content_type),
            None,
            None,
            threshold
        ));
    }
    for content_type in [
        "text/html; charset=utf-8",
        "application/json",
        "application/javascript",
        "application/manifest+json",
        "image/svg+xml",
        "application/vnd.api+json",
        "application/atom+xml",
    ] {
        assert!(is_compressible(content_type), "{content_type}");
    }
    // A missing content type with a known small length still obeys the threshold.
    assert!(!gzip_eligible("gzip", None, None, Some(3), threshold));
}

// ---- routing -------------------------------------------------------------------------------------

/// The match for one pathname, owned so an assertion can name it.
fn matched(
    router: &WebRouter<&'static str>,
    path: &str,
) -> Option<(String, RouteKind, Responder<&'static str>)> {
    router
        .match_route(path)
        .map(|(registered, kind, responder)| (registered.to_owned(), kind, responder.clone()))
}

#[test]
fn matches_exact_before_prefix_and_the_longest_prefix_within_the_table() {
    let mut router: WebRouter<&'static str> = WebRouter::new();
    router
        .register(RouteKind::Prefix, "/api", resource("api"))
        .expect("register /api");
    router
        .register(RouteKind::Prefix, "/api/deep", resource("deep"))
        .expect("register /api/deep");
    router
        .register(RouteKind::Exact, "/api", resource("api-exact"))
        .expect("register exact /api");
    router
        .register(RouteKind::Prefix, "/apix", resource("apix"))
        .expect("register /apix");

    // Exact wins over prefix at the same path.
    assert_eq!(matched(&router, "/api").unwrap().1, RouteKind::Exact);
    // A prefix covers itself and its children, but never a sibling that merely shares the spelling.
    assert_eq!(matched(&router, "/api/x").unwrap().0, "/api");
    assert_eq!(matched(&router, "/apix/y").unwrap().0, "/apix");
    assert_eq!(matched(&router, "/ap").map(|row| row.0), None);
    // The longest matching prefix wins, whatever the registration order.
    assert_eq!(matched(&router, "/api/deep/x").unwrap().0, "/api/deep");
    // A trailing slash is part of the path, not a second spelling of the same one.
    assert_eq!(matched(&router, "/api/").unwrap().0, "/api");
    assert_eq!(matched(&router, "/").map(|row| row.0), None);
}

#[test]
fn refuses_duplicate_routes_and_duplicate_upgrade_owners() {
    let mut router: WebRouter<&'static str> = WebRouter::new();
    router
        .register(RouteKind::Exact, "/a", Responder::Handler)
        .expect("first registration");
    let error = router
        .register(RouteKind::Exact, "/a", resource("x"))
        .unwrap_err();
    assert_eq!(error.to_string(), "webserver: duplicate exact route \"/a\"");
    // A handler and a resource collide like any other pair: the pattern is the contract.
    // The same path under the other kind is a different route.
    assert!(
        router
            .register(RouteKind::Prefix, "/a", resource("x"))
            .is_ok()
    );

    router
        .register_upgrade("/socket", "one")
        .expect("first owner");
    assert_eq!(
        router
            .register_upgrade("/socket", "two")
            .unwrap_err()
            .to_string(),
        "webserver: duplicate exact route \"/socket\""
    );
    assert_eq!(router.upgrade_route("/socket"), Some(("/socket", &"one")));
    assert_eq!(router.upgrade_route("/socket/child"), None);
}

#[test]
fn gives_the_fallback_seat_to_exactly_one_owner_and_releases_it_on_disposal() {
    let mut router: WebRouter<&'static str> = WebRouter::new();
    router
        .register(RouteKind::Exact, "/named", resource("named"))
        .expect("register");
    assert!(!router.has_fallback());
    // Unmatched paths reach no responder until the seat is claimed.
    assert_eq!(router.responder_for("/other"), None);

    router.claim_fallback(resource("dist")).expect("claim");
    assert!(router.has_fallback());
    assert_eq!(router.responder_for("/other"), Some(&resource("dist")));
    // A named route still wins over the fallback.
    assert_eq!(router.responder_for("/named"), Some(&resource("named")));

    assert_eq!(
        router.claim_fallback(Responder::Handler),
        Err(FallbackError)
    );
    assert_eq!(
        FallbackError.to_string(),
        "webserver: fallback already registered"
    );

    // Disposal releases the seat, and it is claimable again — what a hot reload depends on.
    assert!(router.release_fallback().is_some());
    assert!(!router.has_fallback());
    assert_eq!(router.responder_for("/other"), None);
    assert_eq!(router.claim_fallback(Responder::Handler), Ok(()));
    // A route is removable by its own disposer and no other.
    assert!(router.unregister(RouteKind::Exact, "/named"));
    assert!(!router.unregister(RouteKind::Exact, "/named"));
    assert!(!router.unregister_upgrade("/socket"));
}

#[test]
fn answers_in_process_reads_with_the_method_fence_before_matching() {
    let mut router: WebRouter<&'static str> = WebRouter::new();
    router
        .register(RouteKind::Exact, "/res", resource("res"))
        .expect("register");
    router
        .register(RouteKind::Exact, "/handler", Responder::Handler)
        .expect("register");

    let respond = |label: &&'static str| ResourceResponse::ok("text/plain", label.as_bytes());
    // A non-GET/HEAD method is refused before matching, even where a route exists.
    assert_eq!(
        router.answer("POST", "/res", respond),
        ResourceAnswer::MethodNotAllowed
    );
    assert_eq!(
        router.answer("POST", "/nothing", respond),
        ResourceAnswer::MethodNotAllowed
    );
    // A handler route has no resource to answer, so a read is a 404 rather than a made-up body.
    assert_eq!(
        router.answer("GET", "/handler", respond),
        ResourceAnswer::NotFound
    );
    assert_eq!(
        router.answer("GET", "/missing", respond),
        ResourceAnswer::NotFound
    );
    assert_eq!(
        router.answer("GET", "/res", respond),
        ResourceAnswer::Found(ResourceResponse::ok("text/plain", b"res".to_vec()))
    );
    // HEAD keeps status and headers and drops the body.
    assert_eq!(
        router.answer("HEAD", "/res", respond),
        ResourceAnswer::Found(ResourceResponse::ok("text/plain", Vec::new()))
    );
}

#[test]
fn fences_methods_to_get_and_head() {
    assert!(is_resource_method("GET"));
    assert!(is_resource_method("HEAD"));
    assert!(is_resource_method("get"));
    for method in ["POST", "PUT", "OPTIONS", "DELETE", ""] {
        assert!(!is_resource_method(method), "{method}");
    }
    assert_eq!(RESOURCE_METHODS, "GET, HEAD");
}

// ---- index injections ----------------------------------------------------------------------------

#[test]
fn renders_rows_into_their_placement_in_table_order() {
    let html = "<html><head><meta name=\"probe\"></head><body>shell</body></html>";
    let rows = vec![
        IndexInjection::Script {
            placement: Placement::Head,
            text: "window.__Q__=1".to_owned(),
        },
        IndexInjection::ScriptSrc {
            placement: Placement::Head,
            src: "/plugins/a.js?rev=\"1\"&x=<y>".to_owned(),
        },
        IndexInjection::ScriptPreload {
            src: "/plugins/b.js?rev=\"2\"&x=<z>".to_owned(),
        },
        IndexInjection::Global {
            name: "__DSH_BOOT__".to_owned(),
            value: Some(serde_json::json!({ "rev": "</script><b>" })),
        },
        IndexInjection::Style {
            text: "body{margin:0}".to_owned(),
        },
        IndexInjection::Script {
            placement: Placement::Body,
            text: "window.__P__=\"dark\"".to_owned(),
        },
    ];
    let out = render_index_injections(html, &rows);
    let expected_order = [
        "<head>",
        "<script>window.__Q__=1</script>",
        "<script src=\"/plugins/a.js?rev=&quot;1&quot;&amp;x=&lt;y&gt;\"></script>",
        "<link rel=\"preload\" as=\"script\" href=\"/plugins/b.js?rev=&quot;2&quot;&amp;x=&lt;z&gt;\">",
        "globalThis[\"__DSH_BOOT__\"] = {\"rev\":\"\\u003c/script>\\u003cb>\"}",
        "<style>body{margin:0}</style>",
        "<meta name=\"probe\">",
        "<body>",
        "<script>window.__P__=\"dark\"</script>",
        "shell",
    ]
    .map(|part| (part, out.find(part)));
    let positions: Vec<usize> = expected_order
        .iter()
        .map(|(part, at)| at.unwrap_or_else(|| panic!("missing {part:?} in {out}")))
        .collect();
    let mut sorted = positions.clone();
    sorted.sort_unstable();
    assert_eq!(positions, sorted, "rows render out of order in {out}");
    // The boot-readiness tail settles after every body row so a client entry that awaits it reads a
    // complete table, and before the closing body tag so it stays part of the document.
    let tail = out.find(READY_MARKUP).expect("tail present");
    let body_row = out
        .find("<script>window.__P__=\"dark\"</script>")
        .expect("body row");
    assert!(body_row < tail, "tail precedes a body row in {out}");
    assert!(tail < out.find("</body>").expect("closing body"), "{out}");
}

#[test]
fn prepends_head_rows_and_appends_body_rows_to_a_tagless_fragment() {
    // Upstream: a headless fixture page gets the rows ahead of every document script, and a body-less
    // fragment receives them at the end where the parser has already synthesised a body.
    let out = render_index_injections(
        "<main>x</main>",
        &[
            IndexInjection::Script {
                placement: Placement::Head,
                text: "H".to_owned(),
            },
            IndexInjection::Script {
                placement: Placement::Body,
                text: "B".to_owned(),
            },
        ],
    );
    assert_eq!(
        out,
        format!("<script>H</script><main>x</main><script>B</script>{READY_MARKUP}")
    );
    // No rows still settles the deferred: a page that renders nothing must not hang a client entry.
    assert_eq!(
        render_index_injections("<main>x</main>", &[]),
        format!("<main>x</main>{READY_MARKUP}")
    );
}

#[test]
fn escapes_a_global_name_and_a_missing_value() {
    let rows = vec![
        IndexInjection::Global {
            name: "\"><script>".to_owned(),
            value: None,
        },
        IndexInjection::Global {
            name: "plain".to_owned(),
            value: Some(serde_json::json!(2 + 2)),
        },
    ];
    let out = render_index_injections("<html><head></head><body></body></html>", &rows);
    // The name is embedded as a JSON string, so a quote or a `<` cannot break out of the element.
    assert!(
        out.contains("globalThis[\"\\\">\\u003cscript>\"] = undefined"),
        "{out}"
    );
    assert!(out.contains("globalThis[\"plain\"] = 4"), "{out}");
}

#[test]
fn places_a_raw_html_row_verbatim_in_its_region() {
    let out = render_index_injections(
        "<html><head></head><body></body></html>",
        &[
            IndexInjection::Html {
                placement: Placement::Head,
                html: "<meta name=\"probe\">".to_owned(),
            },
            IndexInjection::Html {
                placement: Placement::Body,
                html: "<div id=\"root\"></div>".to_owned(),
            },
        ],
    );
    // A raw row is markup the caller vouched for: the renderer does not escape it.
    assert!(out.contains("<head><meta name=\"probe\"></head>"), "{out}");
    assert!(out.contains("<body><div id=\"root\"></div>"), "{out}");
    // A bodyless answer is just a status, which is what a 304 or an error carries.
    assert_eq!(
        ResourceResponse::empty(304),
        ResourceResponse {
            status: 304,
            content_type: None,
            body: Vec::new()
        }
    );
}

#[test]
fn escapes_the_four_attribute_characters_with_ampersand_first() {
    assert_eq!(escape_html_attribute("a&b"), "a&amp;b");
    assert_eq!(escape_html_attribute("\"<>"), "&quot;&lt;&gt;");
    // Escaping `&` last would turn the other escapes' ampersands into entities again.
    assert_eq!(escape_html_attribute("&lt;"), "&amp;lt;");
}

#[test]
fn treats_a_header_like_element_as_a_different_tag() {
    let out = render_index_injections(
        "<html><header>x</header><body>y</body></html>",
        &[IndexInjection::Style {
            text: "s".to_owned(),
        }],
    );
    // No head tag exists, so the row is prepended rather than spliced into `<header>`.
    assert!(out.starts_with("<style>s</style><html><header>"), "{out}");
    // A head tag with attributes is still a head tag.
    let with_attrs = render_index_injections(
        "<html><head data-x=\"1\"><body>y</body></html>",
        &[IndexInjection::Style {
            text: "s".to_owned(),
        }],
    );
    assert!(
        with_attrs.starts_with("<html><head data-x=\"1\"><style>s</style>"),
        "{with_attrs}"
    );
}

#[test]
fn runs_raw_taps_in_registration_order_over_the_rendered_rows() {
    let upper = |html: &str| html.to_uppercase();
    let strip = |html: &str| html.replace('X', "");
    let rendered = render_index_injections(
        "<head></head><body>abx</body>",
        &[IndexInjection::Script {
            placement: Placement::Body,
            text: "x".to_owned(),
        }],
    );
    let tapped = apply_index_taps(&rendered, &[&upper, &strip]);
    assert!(!tapped.contains('X'), "{tapped}");
    assert!(tapped.contains("AB"), "{tapped}");
    // A tap sees the document a browser would: the rows are already in it when the second tap runs.
    assert!(tapped.contains("<SCRIPT></SCRIPT>"), "{tapped}");
    // The tail is markup too, so a tap that rewrites text rewrites it as well.
    assert!(tapped.contains("__DSH_BOOT_READY__"), "{tapped}");
}

// ---- static frontend -----------------------------------------------------------------------------

/// A dist fixture: index, script, stylesheet, image, manifest, unknown extension, and a subdirectory.
struct Dist {
    root: PathBuf,
    config: StaticConfig,
}

impl Dist {
    fn new(tag: &str) -> Self {
        let root = temp_dir(tag);
        fs::create_dir_all(root.join("plugins")).expect("mkdir plugins");
        fs::write(root.join("index.html"), "<head></head><body>shell</body>").expect("write index");
        fs::write(root.join("app.js"), "export {}").expect("write app.js");
        fs::write(root.join("style.css"), "body{}").expect("write css");
        fs::write(root.join("icon-192.png"), "PNG").expect("write png");
        fs::write(root.join("manifest.webmanifest"), "{}").expect("write manifest");
        fs::write(root.join("blob.bin"), "BLOB").expect("write bin");
        fs::write(root.join("preview.html"), "<body>preview</body>").expect("write preview");
        fs::write(root.join("plugins/boot.js"), "boot").expect("write boot.js");
        let index = root.join("index.html");
        let config = StaticConfig::from_index(index).expect("absolute index path");
        Self { root, config }
    }
}

impl Drop for Dist {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The identity renderer: no injections, so a test observes the raw document.
fn render_identity(html: &str) -> String {
    html.to_owned()
}

#[test]
fn serves_each_dist_file_as_its_own_mime_type() {
    let dist = Dist::new("mime");
    let rows: [(&str, &str); 6] = [
        ("/app.js", "text/javascript; charset=utf-8"),
        ("/style.css", "text/css; charset=utf-8"),
        ("/icon-192.png", "image/png"),
        ("/manifest.webmanifest", "application/manifest+json"),
        ("/preview.html", "text/html; charset=utf-8"),
        // Unknown extension ships as octet-stream rather than a guess.
        ("/blob.bin", OCTET_STREAM_MIME),
    ];
    for (path, content_type) in rows {
        let response = serve_static(&StaticRequest::get(path), &dist.config, &render_identity)
            .expect("served");
        assert_eq!(response.status, 200, "{path}");
        assert_eq!(response.content_type, Some(content_type), "{path}");
        assert!(!response.body.is_empty(), "{path}");
    }
    assert_eq!(
        mime_for_path(std::path::Path::new("/x/a.JSON")),
        "application/json"
    );
    assert_eq!(
        mime_for_path(std::path::Path::new("/x/a.map")),
        "application/json"
    );
    assert_eq!(
        mime_for_path(std::path::Path::new("/x/vfs.gz")),
        "application/gzip"
    );
    assert_eq!(
        mime_for_path(std::path::Path::new("/x/a.svg")),
        "image/svg+xml"
    );
    assert_eq!(
        mime_for_path(std::path::Path::new("/x/noext")),
        OCTET_STREAM_MIME
    );
}

#[test]
fn renders_the_index_at_the_root_and_the_configured_path_and_nowhere_else() {
    let dist = Dist::new("index");
    let rendered = |html: &str| html.replace("<head>", "<head><script>window.__T__=1</script>");
    for path in ["/", "/index.html", "/?view=test", "/index.html?x=1"] {
        // The query never reaches this function: the caller passes a pathname.
        let path = path.split('?').next().unwrap_or(path);
        let response =
            serve_static(&StaticRequest::get(path), &dist.config, &rendered).expect("served");
        assert_eq!(response.status, 200, "{path}");
        assert_eq!(response.content_type, Some(host_gap::HTML_MIME), "{path}");
        let body = String::from_utf8(response.body).expect("utf8");
        assert!(body.contains("window.__T__=1"), "{path}");
        assert!(body.contains("shell"), "{path}");
        // The index carries no cache headers: a stale shell would keep serving a stale table.
        assert_eq!(response.cache_control, None, "{path}");
        assert_eq!(response.etag, None, "{path}");
    }
    // `preview.html` is an ordinary file: it is not the shell and is not rendered.
    let preview = serve_static(
        &StaticRequest::get("/preview.html"),
        &dist.config,
        &rendered,
    )
    .expect("served");
    let body = String::from_utf8(preview.body).expect("utf8");
    assert_eq!(body, "<body>preview</body>");
    assert_eq!(preview.cache_control, Some("no-cache"));
}

#[test]
fn stamps_a_weak_etag_on_assets_and_answers_a_matching_validator_with_304() {
    let dist = Dist::new("etag");
    let first = serve_static(
        &StaticRequest::get("/app.js"),
        &dist.config,
        &render_identity,
    )
    .expect("served");
    let etag = first.etag.clone().expect("a dist file carries an ETag");
    assert!(etag.starts_with("W/\""), "{etag}");
    assert!(etag.ends_with('"'));
    // base64url: no `+`, `/`, or `=`.
    let inner = etag.trim_start_matches("W/\"").trim_end_matches('"');
    assert!(
        inner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "{etag} is not base64url"
    );
    assert_eq!(first.cache_control, Some("no-cache"));

    let revalidated = serve_static(
        &StaticRequest::get("/app.js").with_if_none_match(&etag),
        &dist.config,
        &render_identity,
    )
    .expect("revalidated");
    assert_eq!(revalidated.status, 304);
    assert_eq!(revalidated.etag.as_deref(), Some(etag.as_str()));
    assert_eq!(revalidated.cache_control, Some("no-cache"));
    assert!(revalidated.body.is_empty());

    // New content under the same name changes the ETag, so a stale copy is replaced.
    fs::write(dist.root.join("app.js"), "export const rebuilt = true").expect("rewrite");
    let rebuilt = serve_static(
        &StaticRequest::get("/app.js").with_if_none_match(&etag),
        &dist.config,
        &render_identity,
    )
    .expect("rebuilt");
    assert_eq!(rebuilt.status, 200);
    assert_ne!(rebuilt.etag.as_deref(), Some(etag.as_str()));
}

#[test]
fn the_etag_is_the_base64url_sha1_upstream_writes() {
    // Known-answer vectors for SHA-1 (FIPS 180-4 appendix A / the usual published set).
    fn hex(bytes: [u8; 20]) -> String {
        use std::fmt::Write;
        bytes.iter().fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
    }
    assert_eq!(hex(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(
        hex(sha1(b"abc")),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    assert_eq!(
        hex(sha1(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
    );
    // A million 'a' characters exercises the multi-block path.
    let million = vec![b'a'; 1_000_000];
    assert_eq!(
        hex(sha1(&million)),
        "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
    );
    // base64url without padding, as Buffer#toString('base64url') writes it.
    assert_eq!(base64url(b""), "");
    assert_eq!(base64url(b"f"), "Zg");
    assert_eq!(base64url(b"fo"), "Zm8");
    assert_eq!(base64url(b"foo"), "Zm9v");
    assert_eq!(base64url(b"foob"), "Zm9vYg");
    assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    // `export {}` is the vector the upstream suite reads back.
    assert_eq!(
        weak_etag(b"export {}"),
        format!("W/\"{}\"", base64url(&sha1(b"export {}")))
    );
}

#[test]
fn refuses_traversal_outside_the_dist_root() {
    let dist = Dist::new("traversal");
    // A sibling whose name shares the root's prefix must not pass as a child of it.
    let sibling = dist.root.with_file_name(format!(
        "{}-backup",
        dist.root.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::create_dir_all(&sibling).expect("mkdir sibling");
    fs::write(sibling.join("secret.txt"), "SECRET").expect("write secret");
    let sibling_name = sibling
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    for path in [
        "/..%2f..%2fetc%2fpasswd",
        "/../../../../etc/passwd",
        "/plugins/../../etc/passwd",
        &format!("/../{sibling_name}/secret.txt"),
    ] {
        let response = serve_static(&StaticRequest::get(path), &dist.config, &render_identity)
            .expect("answered");
        assert_eq!(response.status, 403, "{path}");
        assert!(response.body.is_empty(), "{path}");
        assert_eq!(response.content_type, None, "{path}");
    }
    // A dot segment that stays inside the root is a path, not an escape.
    let inside = serve_static(
        &StaticRequest::get("/plugins/./boot.js"),
        &dist.config,
        &render_identity,
    )
    .expect("served");
    assert_eq!(inside.status, 200);
    let _ = fs::remove_dir_all(&sibling);
}

#[test]
fn answers_misses_with_an_empty_headerless_404_and_head_without_a_body() {
    let dist = Dist::new("miss");
    // A missing file, a directory requested as a file, and a file used as a path component are all
    // misses rather than server errors.
    for path in [
        "/no/such/route",
        "/plugins",
        "/app.js/child",
        "/missing.webmanifest",
    ] {
        let response = serve_static(&StaticRequest::get(path), &dist.config, &render_identity)
            .expect("answered");
        assert_eq!(response.status, 404, "{path}");
        assert!(response.body.is_empty(), "{path}");
        assert_eq!(response.content_type, None, "{path}");
        assert_eq!(response.etag, None, "{path}");
        // HEAD answers exactly what GET did.
        let head = serve_static(
            &StaticRequest {
                method: "HEAD",
                ..StaticRequest::get(path)
            },
            &dist.config,
            &render_identity,
        )
        .expect("answered");
        assert_eq!(head, response, "{path}");
    }
    // A dist build with no index is an empty 404 for both of its entry paths.
    fs::remove_file(dist.root.join("index.html")).expect("remove index");
    for path in ["/", "/index.html"] {
        let response = serve_static(&StaticRequest::get(path), &dist.config, &render_identity)
            .expect("answered");
        assert_eq!(response.status, 404, "{path}");
        assert!(response.body.is_empty(), "{path}");
    }
}

#[test]
fn keeps_the_status_and_headers_and_drops_the_body_for_head() {
    let dist = Dist::new("head");
    let get = serve_static(
        &StaticRequest::get("/app.js"),
        &dist.config,
        &render_identity,
    )
    .expect("served");
    let head = serve_static(
        &StaticRequest {
            method: "HEAD",
            ..StaticRequest::get("/app.js")
        },
        &dist.config,
        &render_identity,
    )
    .expect("served");
    assert_eq!(head.status, get.status);
    assert_eq!(head.content_type, get.content_type);
    assert_eq!(head.etag, get.etag);
    assert_eq!(head.cache_control, get.cache_control);
    assert!(head.body.is_empty());
    // The index too.
    let index_head = serve_static(
        &StaticRequest {
            method: "HEAD",
            ..StaticRequest::get("/")
        },
        &dist.config,
        &render_identity,
    )
    .expect("served");
    assert_eq!(index_head.status, 200);
    assert_eq!(index_head.content_type, Some(host_gap::HTML_MIME));
    assert!(index_head.body.is_empty());
}

#[test]
fn refuses_a_method_other_than_get_or_head_before_touching_the_disk() {
    let dist = Dist::new("method");
    for method in ["POST", "PUT", "DELETE", "OPTIONS"] {
        let response = serve_static(
            &StaticRequest {
                method,
                ..StaticRequest::get("/app.js")
            },
            &dist.config,
            &render_identity,
        )
        .expect("answered");
        assert_eq!(response.status, 405, "{method}");
        assert_eq!(response.allow, Some(RESOURCE_METHODS), "{method}");
        assert!(response.body.is_empty(), "{method}");
    }
}

#[test]
fn rejects_a_malformed_percent_escape_rather_than_reading_a_wrong_file() {
    let dist = Dist::new("escape");
    for path in ["/bad%00path", "/bad%", "/bad%2", "/bad%zz"] {
        let error =
            serve_static(&StaticRequest::get(path), &dist.config, &render_identity).unwrap_err();
        assert!(
            matches!(error, host_gap::StaticError::MalformedPath(_)),
            "{path} produced {error}"
        );
    }
    // A `+` is a literal plus in a path, not a space.
    fs::write(dist.root.join("a+b.js"), "plus").expect("write");
    let response = serve_static(
        &StaticRequest::get("/a+b.js"),
        &dist.config,
        &render_identity,
    )
    .expect("served");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"plus");
    // A percent-encoded name resolves to the real file.
    let encoded = serve_static(
        &StaticRequest::get("/a%2Bb.js"),
        &dist.config,
        &render_identity,
    )
    .expect("served");
    assert_eq!(encoded.status, 200);
    // A percent-encoded separator is a separator, so it cannot smuggle a path component past the
    // traversal fence.
    assert_eq!(
        serve_static(
            &StaticRequest::get("/%2e%2e%2f%2e%2e%2fetc%2fpasswd"),
            &dist.config,
            &render_identity
        )
        .expect("answered")
        .status,
        403
    );
}

#[test]
fn requires_an_absolute_dist_index_so_a_relative_one_cannot_rebase_on_the_cwd() {
    let error = StaticConfig::from_index(std::path::Path::new("dist/index.html")).unwrap_err();
    assert_eq!(
        error.to_string(),
        "webserver: distIndex dist/index.html is not an absolute path"
    );
    let config = StaticConfig::from_index("/srv/app/dist/index.html").expect("absolute");
    assert_eq!(config.dist_root, PathBuf::from("/srv/app/dist"));
    assert_eq!(config.dist_index, PathBuf::from("/srv/app/dist/index.html"));
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod host {
    pub use super::host_gap as webserver;
}
