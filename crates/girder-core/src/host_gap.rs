//! The host's *serving* half: the browser carrier, its route table, and the static frontend that
//! occupies its fallback seat.
//!
//! # What upstream `packages/host` actually contains
//!
//! The package tree is not one thing; it is seven capability families, measured by the production
//! sources under `packages/host/*/src` (4,492 lines, 25 files):
//!
//! | Family | Upstream files | Lines | What it decides |
//! | --- | --- | --- | --- |
//! | Browser carrier (`webserver`) | `webserver/src/index.ts`, `injections.ts` | 862 | Bind address, TLS, gzip, the route table, the fallback seat, index injection |
//! | Static frontend (`frontend-static`) | `frontend-static/src/index.ts` | 167 | Dist serving, MIME, ETag/304, traversal fence, index render |
//! | Open in app (`open-in-app`) | `index.ts`, `resolver.ts`, `catalog.ts`, `icons.ts`, `shared.ts`, `internals.ts` | 1,670 | Application catalog resolution, launch, icons |
//! | Directory picking (`directory-picker`, `-browse`, `-auto`, `-native`) | 9 files | 813 | One-level listing, child creation, native chooser, backend resolution |
//! | Plugin inventory (`plugin-inventory`) | `index.ts`, `types.ts` | 198 | A read-only projection of loaded plugin entries |
//! | Product telemetry (`product-telemetry-otel`) | `index.ts` | 121 | OTLP export of product events |
//! | (support) `frontend-static` + `webserver` overlap | — | — | Counted above |
//!
//! # What this build already covers
//!
//! `host_rpc.rs` covers the *opening* half of the host: `host.openPath` and
//! `host.revealInFileManager` with a workspace-confined [`LaunchPlan`]. `workspace_desktop_rpc.rs`
//! covers the Linux desktop probe (`session.workspaceDesktop`, `workspacePathApplications`, XDG
//! `.desktop` parsing, `gio` handler discovery). `girder-rpc/src/web.rs` serves one embedded page with
//! no routing, no TLS, no compression, and no dist directory. Nothing in this crate parses a bind
//! address, matches a route, renders an index injection, or answers a static file.
//!
//! # The three largest gaps, in order
//!
//! 1. **The browser carrier** (862 lines, and the base every other host feature is reached through).
//!    There is no bind-address classification, no TLS material loading, no route/upgrade/fallback
//!    table, no index injection renderer, and no compression negotiation anywhere in this crate.
//! 2. **Static frontend serving** (167 lines, but it is the only way a browser build reaches a
//!    user). No MIME table, no `ETag`, no 304, no traversal fence.
//! 3. **Open in app** (1,670 lines). `workspace_desktop_rpc.rs` covers the Linux *listing* half, so
//!    what remains is the catalog, launch adapters, and icons — real, but the largest part of it is
//!    per-platform launcher resolution that this build can already reach through
//!    `workspace_desktop_rpc` plus `host_rpc`.
//!
//! So this module implements families 1 and 2. They are one subject — the HTTP surface a browser
//! talks to — split across two upstream packages only because of how Cordis bundles plugins into
//! units. Implementing the carrier without the static server would leave the carrier with nothing to
//! put on its fallback seat and the static server with no route table to answer through.
//!
//! # What is deliberately not here
//!
//! - **A socket.** This module decides and renders; binding a `TcpListener`, running the accept loop,
//!   and speaking HTTP belong to the transport, and a half-finished async listener would be worse
//!   than none. Every decision that a listener would otherwise hide — which address is legal, what
//!   the origin is, which route answers a path, what bytes a static request produces — is here and
//!   is testable without a port.
//! - **Certificate/key pairing.** Node's `tls` module verifies that the key matches the certificate.
//!   Doing that here would mean an X.509 parser and a key-type-aware signature check; instead
//!   [`load_tls_material`] checks the things a reader can check without one — both files exist, both
//!   are non-blank, and both hold the PEM block their field promises — and reports the rest as the
//!   caller's to verify. That is a real reduction from upstream and it is stated rather than hidden.
//! - **The open-in-app catalog** (gap 3) and the plugin inventory. They are measured above and left
//!   for a later change rather than half-built here.
//!
//! # Why `girder_core::` inside the module
//!
//! The integration test includes this file by path, because wiring it into `lib.rs` belongs to a
//! different change. Under `#[path]` inclusion `crate::` names the test binary, so every path to
//! another crate item would fail to resolve. The module therefore names nothing from the crate root
//! at all: what it needs from the rest of the crate (`serde_json`) is a dependency, not a sibling,
//! and that keeps one copy of the file working from both sides.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

// -------------------------------------------------------------------------------------------------
// Bind addresses
// -------------------------------------------------------------------------------------------------

/// The `Allow` value a resource answers with, and the methods a resource ever sees.
///
/// Upstream `RESOURCE_METHODS` in `packages/host/webserver/src/index.ts`.
pub const RESOURCE_METHODS: &str = "GET, HEAD";

/// The `content-type` of every rendered HTML document.
///
/// Upstream `HTML_MIME` in `packages/host/frontend-static/src/index.ts`.
pub const HTML_MIME: &str = "text/html; charset=utf-8";

/// The MIME type a file with no known extension is served as.
///
/// Upstream's `MIME[extname(target)] ?? 'application/octet-stream'` fallback in
/// `packages/host/frontend-static/src/index.ts`.
pub const OCTET_STREAM_MIME: &str = "application/octet-stream";

/// Why a bind address was refused, or could not be turned into a URL.
///
/// Mirrors the three throw sites in `packages/host/webserver/src/index.ts`: `notLiteralError`,
/// the `Config.host` transform's wildcard rejection, and `getOrigin`'s zone refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindAddressError {
    /// The value is not a concrete IPv4 or IPv6 literal.
    ///
    /// Upstream `notLiteralError`.
    NotLiteral(String),
    /// The value is an unspecified (wildcard) address, which would bind every interface.
    ///
    /// Upstream's `Config.host` transform.
    Wildcard(String),
    /// The value carries an interface zone id, which no URL can express.
    ///
    /// Upstream `getOrigin`.
    ZoneNotExpressible(String),
}

impl fmt::Display for BindAddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLiteral(host) => write!(
                f,
                "webserver: host {host:?} is not a concrete IPv4 or IPv6 address literal"
            ),
            Self::Wildcard(host) => write!(
                f,
                "webserver: host {host:?} is an unspecified (wildcard) address, which is not \
                 supported: binding every interface would expose remote code execution to the \
                 network; bind one concrete IPv4 or IPv6 address of a local interface instead"
            ),
            Self::ZoneNotExpressible(host) => write!(
                f,
                "webserver: bind address {host:?} carries an interface zone id, which no URL can \
                 express"
            ),
        }
    }
}

impl std::error::Error for BindAddressError {}

/// A parsed bind address: the value the socket binds, not the text that named it.
///
/// Upstream keeps this as `ipaddr.js`'s `IPv4 | IPv6` union from `parseIpLiteral`
/// (`packages/host/webserver/src/index.ts`). The distinction that matters is carried here too:
/// an IPv4-mapped IPv6 literal stays IPv4, because a browser treats `::ffff:127.0.0.1` as the
/// loopback origin `127.0.0.1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindAddress {
    /// A dotted-quad address; the IPv4-mapped form of one is parsed into this variant.
    V4([u8; 4]),
    /// An IPv6 address as eight groups.
    V6([u16; 8]),
}

impl BindAddress {
    /// The canonical text of the address, exactly as `ipaddr.js`'s `toString` writes it.
    ///
    /// IPv4 is dotted quad; IPv6 is the compressed form with lowercase hex, no leading zeros per
    /// group, `::` at the longest run of zero groups, and an IPv4 tail for a mapped address. This is
    /// the text the address is reachable at, so `::0.0.0.1` correctly reads as `::1`.
    #[must_use]
    pub fn text(self) -> String {
        match self {
            Self::V4(octets) => ipv4_text(octets),
            Self::V6(groups) => ipv6_text(groups),
        }
    }

    /// Whether this names the loopback authority: any address in 127/8, or `::1`.
    ///
    /// Upstream `isLoopbackHost` (`packages/host/webserver/src/index.ts`). A mapped address was
    /// already folded into [`Self::V4`] at parse time, which is what makes `::ffff:127.0.0.1` count.
    #[must_use]
    pub fn is_loopback(self) -> bool {
        match self {
            Self::V4([127, ..]) => true,
            Self::V4(_) => false,
            Self::V6(groups) => groups[..7].iter().all(|group| *group == 0) && groups[7] == 1,
        }
    }

    /// Whether this is the unspecified address, which binds every interface.
    ///
    /// Upstream's private `isWildcardAddress`: the all-zero address only. `ipaddr.js` classifies all
    /// of 0/8 as `unspecified`, but only `0.0.0.0` binds every interface, so 0.0.0.1 is not wildcard.
    #[must_use]
    pub fn is_wildcard(self) -> bool {
        match self {
            Self::V4(octets) => octets == [0, 0, 0, 0],
            Self::V6(groups) => groups == [0; 8],
        }
    }
}

/// Dotted-quad text.
fn ipv4_text(octets: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], octets[3])
}

/// Compressed IPv6 text, with a dotted-quad tail only for an IPv4-mapped address.
///
/// The `::` run is the *longest* run of zero groups, and the first such run on a tie, because two
/// spellings of one address would make the text comparison in [`normalize_bind_address`] unreliable.
fn ipv6_text(groups: [u16; 8]) -> String {
    // An IPv4-mapped address is written as `::ffff:a.b.c.d` rather than `::ffff:7f00:1`.
    if groups[..5].iter().all(|group| *group == 0) && groups[5] == 0xffff {
        let high = groups[6];
        let low = groups[7];
        return format!(
            "::ffff:{}.{}.{}.{}",
            (high >> 8) & 0xff,
            high & 0xff,
            (low >> 8) & 0xff,
            low & 0xff
        );
    }
    let mut best_at = None;
    let mut best_len = 0;
    let mut at = 0;
    while at < 8 {
        if groups[at] != 0 {
            at += 1;
            continue;
        }
        let start = at;
        while at < 8 && groups[at] == 0 {
            at += 1;
        }
        if at - start > best_len {
            best_len = at - start;
            best_at = Some(start);
        }
    }
    // A single zero group is not abbreviated: `::` for one group would be longer, not shorter.
    let Some(start) = best_at.filter(|_| best_len > 1) else {
        return (0..8)
            .map(|at| format!("{:x}", groups[at]))
            .collect::<Vec<_>>()
            .join(":");
    };
    let head = (0..start)
        .map(|at| format!("{:x}", groups[at]))
        .collect::<Vec<_>>()
        .join(":");
    let tail = (start + best_len..8)
        .map(|at| format!("{:x}", groups[at]))
        .collect::<Vec<_>>()
        .join(":");
    format!("{head}::{tail}")
}

/// Split a literal into its address text and optional interface zone.
///
/// A `%zone` selects a local interface and is never part of the address, which is why it comes off
/// before parsing but is kept for the caller: `listen` needs it, a URL cannot carry it.
fn split_zone(host: &str) -> (&str, Option<&str>) {
    match host.find('%') {
        Some(at) => (&host[..at], Some(&host[at + 1..])),
        None => (host, None),
    }
}

/// Parse one IPv4 dotted quad with no leading-zero tolerance beyond what a socket accepts.
fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let mut octets = [0_u8; 4];
    let mut count = 0;
    for part in text.split('.') {
        if count == 4 || part.is_empty() || part.len() > 3 {
            return None;
        }
        if !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        // Three digits could still overflow a u8 (999); parse and check rather than trusting length.
        let value: u16 = part.parse().ok()?;
        if value > 255 {
            return None;
        }
        // `010` is a valid octal-ish spelling to some resolvers and a decimal 10 to others; Node
        // rejects leading zeros in an IPv4 literal, and accepting both would make the parsed value
        // differ from the value the socket binds.
        if part.len() > 1 && part.starts_with('0') {
            return None;
        }
        octets[count] = u8::try_from(value).ok()?;
        count += 1;
    }
    (count == 4).then_some(octets)
}

/// Rewrite a dotted-quad tail as the two hex groups holding the same bits.
///
/// Upstream `withHexTail` (`packages/host/webserver/src/index.ts`). A quad tail is the address's own
/// low 32 bits — `::0.0.0.1` is `::1` — but a reader that recognised it as an IPv4 tail would parse
/// it as the mapped form `::ffff:0:1`, a different address than the socket binds. Rewriting the quad
/// before the group split keeps exactly one interpretation.
fn with_hex_tail(text: &str) -> Option<String> {
    if !text.contains('.') {
        return Some(text.to_owned());
    }
    let colon = text.rfind(':')?;
    let quad = parse_ipv4(&text[colon + 1..])?;
    let high = u16::from(quad[0]) << 8 | u16::from(quad[1]);
    let low = u16::from(quad[2]) << 8 | u16::from(quad[3]);
    Some(format!("{}{high:x}:{low:x}", &text[..=colon]))
}

/// Parse an IPv6 literal, including an embedded dotted-quad tail.
fn parse_ipv6(text: &str) -> Option<[u16; 8]> {
    let normalized = with_hex_tail(text)?;
    let text = normalized.as_str();
    // At most one `::` abbreviates a run of zero groups; two would be ambiguous.
    let gap = text.find("::")?;
    let head_text = &text[..gap];
    let tail_text = &text[gap + 2..];
    if tail_text.contains("::") {
        return None;
    }
    let mut head: Vec<u16> = Vec::with_capacity(8);
    if !head_text.is_empty() {
        for part in head_text.split(':') {
            head.push(parse_ipv6_group(part)?);
        }
    }
    let mut tail: Vec<u16> = Vec::with_capacity(8);
    if !tail_text.is_empty() {
        for part in tail_text.split(':') {
            tail.push(parse_ipv6_group(part)?);
        }
    }
    let written = head.len() + tail.len();
    // The gap must stand for at least one group, and the written groups must fit around it.
    if written >= 8 {
        return None;
    }
    let mut groups = [0_u16; 8];
    groups[..head.len()].copy_from_slice(&head);
    groups[8 - tail.len()..].copy_from_slice(&tail);
    Some(groups)
}

/// One 1-to-4 digit hexadecimal group.
fn parse_ipv6_group(part: &str) -> Option<u16> {
    if part.is_empty() || part.len() > 4 || !part.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(part, 16).ok()
}

/// Parse one IPv6 literal that contains no `::`, which is the only other legal spelling.
fn parse_full_ipv6(text: &str) -> Option<[u16; 8]> {
    let normalized = with_hex_tail(text)?;
    if !normalized.contains("::") {
        let mut groups = [0_u16; 8];
        let mut count = 0;
        for part in normalized.split(':') {
            if count == 8 {
                return None;
            }
            groups[count] = parse_ipv6_group(part)?;
            count += 1;
        }
        return (count == 8).then_some(groups);
    }
    parse_ipv6(&normalized)
}

/// Parse a bind-address literal: a concrete IPv4 or IPv6 address with an optional `%zone`.
///
/// Upstream `parseIpLiteral` (`packages/host/webserver/src/index.ts`), including its two
/// normalizations: the zone is stripped before parsing, and an IPv4-mapped IPv6 address is folded
/// into IPv4 so that later classification sees the address a browser sees.
#[must_use]
pub fn parse_bind_address(host: &str) -> Option<BindAddress> {
    let (bare, _zone) = split_zone(host);
    if bare.is_empty() {
        return None;
    }
    if let Some(octets) = parse_ipv4(bare) {
        return Some(BindAddress::V4(octets));
    }
    let groups = parse_full_ipv6(bare)?;
    let mapped = groups[..5].iter().all(|group| *group == 0) && groups[5] == 0xffff;
    if mapped {
        let high = groups[6];
        let low = groups[7];
        // Each half of a u16 is its own octet; a `as u8` cast would be a truncating conversion, which
        // this crate denies, and the big-endian bytes are the octets without one.
        return Some(BindAddress::V4([
            (high >> 8).to_be_bytes()[1],
            (high & 0xff).to_be_bytes()[1],
            (low >> 8).to_be_bytes()[1],
            (low & 0xff).to_be_bytes()[1],
        ]));
    }
    Some(BindAddress::V6(groups))
}

/// Whether a bind address requests every interface at once.
///
/// Upstream `isWildcardHost` (`packages/host/webserver/src/index.ts`).
#[must_use]
pub fn is_wildcard_host(host: &str) -> bool {
    parse_bind_address(host).is_some_and(BindAddress::is_wildcard)
}

/// Whether a bind address names the local loopback authority.
///
/// Upstream `isLoopbackHost` (`packages/host/webserver/src/index.ts`).
#[must_use]
pub fn is_loopback_host(host: &str) -> bool {
    parse_bind_address(host).is_some_and(BindAddress::is_loopback)
}

/// The canonical text of the address a bind literal names.
///
/// Upstream `normalizeBindAddress` (`packages/host/webserver/src/index.ts`): IPv4 and IPv4-mapped
/// literals read as dotted quad, other literals as compressed IPv6, and the zone never appears.
///
/// # Errors
///
/// [`BindAddressError::NotLiteral`] when the value is not a concrete address literal.
pub fn normalize_bind_address(host: &str) -> Result<String, BindAddressError> {
    match parse_bind_address(host) {
        Some(address) => Ok(address.text()),
        None => Err(BindAddressError::NotLiteral(host.to_owned())),
    }
}

/// The URL host form of a bind address.
///
/// Upstream `bindUrlHost` (`packages/host/webserver/src/index.ts`). A loopback address uses its
/// canonical text so a browser treats it as a trustworthy local origin — which is what makes
/// `::ffff:127.0.0.1` and `::0.0.0.1` reachable as `127.0.0.1` and `[::1]`. A redundant loopback
/// zone is dropped; IPv6 text is bracketed.
///
/// # Errors
///
/// [`BindAddressError::NotLiteral`] for a non-literal, and [`BindAddressError::ZoneNotExpressible`]
/// when a non-loopback address carries a zone id, because no URL can name an interface.
pub fn bind_url_host(host: &str) -> Result<String, BindAddressError> {
    let (bare, zone) = split_zone(host);
    let address =
        parse_bind_address(bare).ok_or_else(|| BindAddressError::NotLiteral(host.to_owned()))?;
    if zone.is_some() && !address.is_loopback() {
        return Err(BindAddressError::ZoneNotExpressible(host.to_owned()));
    }
    let text = address.text();
    // IPv4 text has dots and no colons; only IPv6 needs brackets in an authority.
    if text.contains(':') {
        Ok(format!("[{text}]"))
    } else {
        Ok(text)
    }
}

/// Validate one configured bind address the way the upstream config transform does.
///
/// Upstream turns `Config.host` into a rejection at load: a non-literal or a wildcard address
/// fails plugin initialization rather than binding something surprising.
///
/// # Errors
///
/// [`BindAddressError::NotLiteral`] or [`BindAddressError::Wildcard`].
pub fn validate_bind_host(host: &str) -> Result<(), BindAddressError> {
    let address =
        parse_bind_address(host).ok_or_else(|| BindAddressError::NotLiteral(host.to_owned()))?;
    if address.is_wildcard() {
        return Err(BindAddressError::Wildcard(host.to_owned()));
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------------
// Compression and TLS configuration
// -------------------------------------------------------------------------------------------------

/// Response compression for socket-backed HTTP requests.
///
/// Upstream `Config.compression` in `packages/host/webserver/src/index.ts`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compression {
    /// No compression: the default.
    #[default]
    None,
    /// gzip with [`WebServerConfig::compression_level`].
    Gzip,
}

/// The default gzip DEFLATE level.
///
/// Upstream `DEFAULT_COMPRESSION_LEVEL`.
pub const DEFAULT_COMPRESSION_LEVEL: u8 = 1;

/// The default minimum known response length eligible for gzip.
///
/// Upstream `DEFAULT_COMPRESSION_THRESHOLD_BYTES`.
pub const DEFAULT_COMPRESSION_THRESHOLD_BYTES: usize = 1024;

/// TLS material for the HTTPS listener: one certificate chain file and the key it pairs with.
///
/// Upstream `TlsConfig` (`packages/host/webserver/src/index.ts`). Both paths resolve against the
/// process working directory upstream; here they are absolute by the time they reach
/// [`load_tls_material`], and a relative path is the caller's to resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsConfig {
    /// Certificate chain file, leaf certificate first, PEM, no passphrase.
    pub cert_file: PathBuf,
    /// Private key for the chain's leaf certificate; unencrypted PEM.
    pub key_file: PathBuf,
}

/// Why TLS material could not be turned into a serving listener.
///
/// Mirrors the three failure sites in upstream `readTlsFile` and the `createSecureServer` catch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TlsError {
    /// The file could not be read; the field name is `certFile` or `keyFile`.
    Read {
        /// The config field being read.
        field: &'static str,
        /// The resolved path.
        path: PathBuf,
        /// The underlying reason, already rendered.
        message: String,
    },
    /// The file is empty or blank. Node's `tls` accepts this silently and then cannot complete a
    /// handshake, which is why upstream rejects it here.
    Empty {
        /// The config field being read.
        field: &'static str,
        /// The resolved path.
        path: PathBuf,
    },
    /// The two files are not a usable certificate and key pair by the checks available here.
    Unusable {
        /// The certificate file.
        cert_file: PathBuf,
        /// The key file.
        key_file: PathBuf,
        /// What failed, in the operator's terms.
        message: String,
    },
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read {
                field,
                path,
                message,
            } => write!(
                f,
                "webserver: cannot read tls.{field} {}: {message}",
                path.display()
            ),
            Self::Empty { field, path } => {
                write!(f, "webserver: tls.{field} {} is empty", path.display())
            }
            Self::Unusable {
                cert_file,
                key_file,
                message,
            } => write!(
                f,
                "webserver: tls.certFile {} and tls.keyFile {} are not a usable certificate and \
                 key pair: {message}",
                cert_file.display(),
                key_file.display()
            ),
        }
    }
}

impl std::error::Error for TlsError {}

/// TLS material held in memory: the PEM texts the listener is constructed from.
///
/// Upstream reads both files once, before the listener binds, and never re-reads them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsMaterial {
    /// The certificate chain PEM.
    pub cert_pem: String,
    /// The private key PEM.
    pub key_pem: String,
}

/// Read and check the configured TLS material before a listener binds.
///
/// Upstream `readTlsFile` plus the `createSecureServer` validation in
/// `packages/host/webserver/src/index.ts`. The check here is a PEM shape check, not a cryptographic
/// pairing check — see the module note on what is deliberately absent.
///
/// # Errors
///
/// [`TlsError::Read`] for an unreadable file, [`TlsError::Empty`] for blank content, and
/// [`TlsError::Unusable`] when a file holds no PEM block of the expected kind.
pub fn load_tls_material(cert_file: &Path, key_file: &Path) -> Result<TlsMaterial, TlsError> {
    let cert_pem = read_tls_file("certFile", cert_file)?;
    let key_pem = read_tls_file("keyFile", key_file)?;
    let cert_ok = pem_labels(&cert_pem)
        .iter()
        .any(|label| label == "CERTIFICATE" || label.ends_with("CERTIFICATE"));
    if !cert_ok {
        return Err(TlsError::Unusable {
            cert_file: cert_file.to_path_buf(),
            key_file: key_file.to_path_buf(),
            message: "the certificate file holds no PEM CERTIFICATE block".to_owned(),
        });
    }
    let key_ok = pem_labels(&key_pem)
        .iter()
        .any(|label| label.ends_with("PRIVATE KEY"));
    if !key_ok {
        return Err(TlsError::Unusable {
            cert_file: cert_file.to_path_buf(),
            key_file: key_file.to_path_buf(),
            message: "the key file holds no PEM PRIVATE KEY block".to_owned(),
        });
    }
    Ok(TlsMaterial { cert_pem, key_pem })
}

/// Read one TLS file for activation, rejecting blank content.
fn read_tls_file(field: &'static str, path: &Path) -> Result<String, TlsError> {
    let content = std::fs::read_to_string(path).map_err(|error| TlsError::Read {
        field,
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    if content.trim().is_empty() {
        return Err(TlsError::Empty {
            field,
            path: path.to_path_buf(),
        });
    }
    Ok(content)
}

/// The labels of the PEM blocks a text contains, in file order.
///
/// A block is only counted when its `BEGIN <label>-----` line is matched by an
/// `END <label>-----` line, so a truncated file is not mistaken for material a listener could use.
#[must_use]
pub fn pem_labels(text: &str) -> Vec<String> {
    let mut labels = Vec::new();
    let mut open: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("-----BEGIN ") {
            if let Some(label) = rest.strip_suffix("-----") {
                open = Some(label.to_owned());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("-----END ") {
            if let Some(label) = rest.strip_suffix("-----") {
                if open.as_deref() == Some(label) {
                    labels.push(label.to_owned());
                    open = None;
                }
            }
        }
    }
    labels
}

/// Why a configured web server was refused.
///
/// The union of upstream's schemastery rejections: a `Config.host` transform failure, a
/// `compressionLevel` outside 0..=9, and a `tls` object missing one of its required strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebServerConfigError {
    /// The bind address is not usable.
    Host(BindAddressError),
    /// The gzip level is outside 0 through 9.
    CompressionLevel(u8),
    /// One of the TLS paths is empty.
    MissingTlsPath(&'static str),
    /// The TLS file pair is empty or not a PEM pair.
    Tls(TlsError),
}

impl fmt::Display for WebServerConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host(error) => error.fmt(f),
            Self::CompressionLevel(level) => write!(
                f,
                "webserver: compressionLevel {level} is outside the supported 0 through 9"
            ),
            Self::MissingTlsPath(field) => {
                write!(
                    f,
                    "webserver: tls.{field} is required when tls is configured"
                )
            }
            Self::Tls(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for WebServerConfigError {}

/// Why an origin could not be reported.
///
/// Upstream `getOrigin`'s two throw sites (`packages/host/webserver/src/index.ts`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OriginError {
    /// The listener has not bound a port yet, so there is no origin to report.
    NotBound,
    /// The bind address has no URL form.
    Bind(BindAddressError),
}

impl fmt::Display for OriginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotBound => write!(f, "webserver: origin read before the listener bound a port"),
            Self::Bind(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for OriginError {}

/// Listen, TLS, and compression configuration for the browser carrier.
///
/// Upstream `Config` / `ResolvedConfig` in `packages/host/webserver/src/index.ts`, with the schema
/// defaults already applied by [`Self::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebServerConfig {
    /// The bind address: one concrete IPv4 or IPv6 literal of a local interface.
    pub host: String,
    /// The listen port; zero requests an OS-assigned port.
    pub port: u16,
    /// Serve HTTPS with this material instead of plain HTTP.
    pub tls: Option<TlsConfig>,
    /// Response compression mode.
    pub compression: Compression,
    /// gzip DEFLATE level, 0 through 9.
    pub compression_level: u8,
    /// Minimum known response length eligible for gzip.
    pub compression_threshold_bytes: usize,
}

impl WebServerConfig {
    /// A plain-HTTP configuration with upstream's documented defaults.
    #[must_use]
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            tls: None,
            compression: Compression::None,
            compression_level: DEFAULT_COMPRESSION_LEVEL,
            compression_threshold_bytes: DEFAULT_COMPRESSION_THRESHOLD_BYTES,
        }
    }

    /// The same configuration serving HTTPS from this material.
    #[must_use]
    pub fn with_tls(mut self, tls: TlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    /// The same configuration compressing responses at this level.
    #[must_use]
    pub fn with_gzip(mut self, level: u8) -> Self {
        self.compression = Compression::Gzip;
        self.compression_level = level;
        self
    }

    /// The same configuration with a different gzip eligibility threshold.
    #[must_use]
    pub fn with_compression_threshold(mut self, bytes: usize) -> Self {
        self.compression_threshold_bytes = bytes;
        self
    }

    /// The protocol the listener speaks, from configuration alone.
    ///
    /// Upstream's `protocol` getter: it follows configuration, never a socket or a request header,
    /// so a consumer can read it before any request and no proxy can change it.
    #[must_use]
    pub fn protocol(&self) -> &'static str {
        if self.tls.is_none() {
            "http:"
        } else {
            "https:"
        }
    }

    /// Reject a configuration the way the upstream schema does at load.
    ///
    /// # Errors
    ///
    /// [`WebServerConfigError::Host`] for a non-literal or wildcard bind address,
    /// [`WebServerConfigError::CompressionLevel`] outside 0..=9, and
    /// [`WebServerConfigError::MissingTlsPath`] for an empty TLS path.
    pub fn validate(&self) -> Result<(), WebServerConfigError> {
        validate_bind_host(&self.host).map_err(WebServerConfigError::Host)?;
        if self.compression_level > 9 {
            return Err(WebServerConfigError::CompressionLevel(
                self.compression_level,
            ));
        }
        if let Some(tls) = &self.tls {
            if tls.cert_file.as_os_str().is_empty() {
                return Err(WebServerConfigError::MissingTlsPath("certFile"));
            }
            if tls.key_file.as_os_str().is_empty() {
                return Err(WebServerConfigError::MissingTlsPath("keyFile"));
            }
        }
        Ok(())
    }

    /// The root URL local clients use to reach the bound listener.
    ///
    /// Upstream `getOrigin`. `bound_port` is the port the listener actually bound, which differs from
    /// [`Self::port`] when zero requested an OS-assigned one; `None` means nothing has bound yet.
    ///
    /// # Errors
    ///
    /// [`OriginError::NotBound`] before a bind, and [`OriginError::Bind`] when the bind address has no
    /// URL form.
    pub fn origin(&self, bound_port: Option<u16>) -> Result<String, OriginError> {
        let host = bind_url_host(&self.host).map_err(OriginError::Bind)?;
        let port = bound_port.ok_or(OriginError::NotBound)?;
        Ok(format!("{}//{host}:{port}", self.protocol()))
    }

    /// Read the configured TLS material, or `None` when this configuration serves plain HTTP.
    ///
    /// Upstream reads both files once, before the listener binds, so a bad pair rejects
    /// initialization instead of producing a listener that can never complete a handshake. This is
    /// that read, folded into the configuration so the two cannot disagree about which files are
    /// being served: a caller that validated [`Self::validate`] and then loaded material through this
    /// method has read exactly the pair the configuration names.
    ///
    /// # Errors
    ///
    /// [`WebServerConfigError::Tls`] when the material is unreadable, blank, or not a PEM pair.
    pub fn load_tls(&self) -> Result<Option<TlsMaterial>, WebServerConfigError> {
        match &self.tls {
            None => Ok(None),
            Some(tls) => load_tls_material(&tls.cert_file, &tls.key_file)
                .map(Some)
                .map_err(WebServerConfigError::Tls),
        }
    }
}

// -------------------------------------------------------------------------------------------------
// gzip eligibility
// -------------------------------------------------------------------------------------------------

/// Parse one `Accept-Encoding` value and answer whether gzip is acceptable.
///
/// Upstream negotiates with `Negotiator#encoding(['gzip', 'identity'])`. The rule that matters for
/// correctness is the q-value: `gzip;q=0` is an explicit refusal, and `*` stands for any coding not
/// named, with an explicit `gzip` entry always beating the wildcard.
#[must_use]
pub fn accepts_gzip(accept_encoding: &str) -> bool {
    let mut explicit: Option<f32> = None;
    let mut wildcard: Option<f32> = None;
    for part in accept_encoding.split(',') {
        let mut pieces = part.split(';');
        let name = pieces.next().unwrap_or("").trim().to_ascii_lowercase();
        let mut quality = 1.0_f32;
        for parameter in pieces {
            let parameter = parameter.trim();
            if let Some(value) = parameter.strip_prefix("q=") {
                // An unparsable quality is a malformed header; treating it as the default rather
                // than as a refusal keeps one bad token from disabling compression everywhere.
                quality = value.trim().parse().unwrap_or(1.0);
            }
        }
        match name.as_str() {
            "gzip" => explicit = Some(quality),
            "*" => wildcard = Some(quality),
            _ => {}
        }
    }
    match explicit.or(wildcard) {
        Some(quality) => quality > 0.0,
        None => false,
    }
}

/// Whether a content type is worth compressing at all.
///
/// Upstream delegates this to the `compression` package's `compressible` check. This is the subset a
/// harness actually serves: text documents, JSON, JavaScript, CSS, SVG, and XML. An image, a font, or
/// an archive is already compressed, and compressing it again costs CPU for a larger body.
#[must_use]
pub fn is_compressible(content_type: &str) -> bool {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if base.is_empty() {
        return false;
    }
    if base.starts_with("text/") {
        return true;
    }
    matches!(
        base.as_str(),
        "application/json"
            | "application/javascript"
            | "application/manifest+json"
            | "application/xml"
            | "application/xhtml+xml"
            | "image/svg+xml"
            | "application/wasm"
    ) || base.ends_with("+json")
        || base.ends_with("+xml")
}

/// Whether one response may be gzipped.
///
/// Upstream's `createGzipMiddleware` filter and its `compression` defaults, in one decision:
/// a client that does not accept gzip gets identity; a `content-range` response is a partial body
/// whose length arithmetic compression would invalidate; an event stream is incremental and
/// flushing per event defeats compression; multipart bodies are compressed regardless of threshold
/// because they are large by construction; anything else needs a known length at or above the
/// threshold and a compressible type. An unknown length is eligible — a stream cannot be sized
/// before it is read, and refusing every stream would disable compression where it matters most.
#[must_use]
pub fn gzip_eligible(
    accept_encoding: &str,
    content_type: Option<&str>,
    content_range: Option<&str>,
    content_length: Option<usize>,
    threshold: usize,
) -> bool {
    if !accepts_gzip(accept_encoding) {
        return false;
    }
    if content_range.is_some() {
        return false;
    }
    if let Some(content_type) = content_type {
        let base = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if base == "text/event-stream" {
            return false;
        }
        if base == "multipart/form-data" {
            return true;
        }
        if !is_compressible(content_type) {
            return false;
        }
    }
    match content_length {
        Some(length) => length >= threshold,
        None => true,
    }
}

// -------------------------------------------------------------------------------------------------
// The route table
// -------------------------------------------------------------------------------------------------

/// Route match kind: `Exact` matches the pathname verbatim; `Prefix` matches it and every path below
/// it, so `/api` covers `/api` and `/api/x` but not `/apix`.
///
/// Upstream `WebRouteKind` in `packages/host/webserver/src/index.ts`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RouteKind {
    /// The pathname must equal the registered path.
    Exact,
    /// The pathname must equal the path or start with `path/`.
    Prefix,
}

impl RouteKind {
    /// The wire spelling, as it appears in a duplicate-route diagnostic.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Prefix => "prefix",
        }
    }
}

/// A route registration that cannot be honoured.
///
/// Upstream throws for a duplicate `(kind, path)` pair, because route patterns are a
/// composition-level contract and a collision is a misconfiguration rather than a preference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteError {
    /// The kind of the colliding route.
    pub kind: RouteKind,
    /// Its path.
    pub path: String,
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "webserver: duplicate {} route {:?}",
            self.kind.as_str(),
            self.path
        )
    }
}

impl std::error::Error for RouteError {}

/// What answers a request: a handler that owns the response lifecycle, or a resource that maps a
/// request to a response.
///
/// Upstream models this as two optional fields on `WebRoute | WebResourceRoute`
/// (`packages/host/webserver/src/index.ts`). An enum makes the two exclusive in the type, which is
/// what they already are: a route with both would leave the answer ambiguous.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Responder<R> {
    /// Owns the full response lifecycle, including holding the socket open (SSE).
    ///
    /// A resource read against a handler route has nothing to answer, which is why it is a 404.
    Handler,
    /// Answers GET and HEAD, over HTTP and in-process alike.
    Resource(R),
}

/// A response a resource produced.
///
/// The fields a resource read observes: status, the headers that reach a caller, and the body a GET
/// returns. A HEAD answer is the same response with an empty body, produced by [`Self::head`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceResponse {
    /// The HTTP status.
    pub status: u16,
    /// The `content-type`, when the resource set one.
    pub content_type: Option<String>,
    /// The body bytes; empty for a HEAD answer.
    pub body: Vec<u8>,
}

impl ResourceResponse {
    /// A `200` answer with a content type and body.
    #[must_use]
    pub fn ok(content_type: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: Some(content_type.into()),
            body: body.into(),
        }
    }

    /// A bodyless answer with only a status.
    #[must_use]
    pub fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: None,
            body: Vec::new(),
        }
    }

    /// The same status and headers with no body: what a HEAD exchange writes.
    #[must_use]
    pub fn head(&self) -> Self {
        Self {
            status: self.status,
            content_type: self.content_type.clone(),
            body: Vec::new(),
        }
    }
}

/// What a resource read answers.
///
/// Upstream `readResource`'s three outcomes, in the order it checks them: the method fence before
/// matching, then 404 for a handler route or an unmatched path, otherwise the resource's response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceAnswer {
    /// The method is not GET or HEAD; the caller sends 405 with this `Allow` value.
    MethodNotAllowed,
    /// The path reaches a handler route, a fallback handler, or no responder at all.
    NotFound,
    /// The resource's response, already stripped of its body for a HEAD read.
    Found(ResourceResponse),
}

/// The named route table plus the upgrade table and the single fallback seat.
///
/// Upstream `WebServer`'s routing state in `packages/host/webserver/src/index.ts`, with the socket and
/// the Cordis service removed. Registration order does not affect requests: exact routes are matched
/// before prefixes, and among prefixes the longest one wins. A request no named route matches is
/// answered by the fallback seat once it is claimed, and 404 until then.
#[derive(Clone, Debug)]
pub struct WebRouter<R> {
    exact: BTreeMap<String, Responder<R>>,
    prefixes: BTreeMap<String, Responder<R>>,
    upgrades: BTreeMap<String, R>,
    fallback: Option<Responder<R>>,
}

impl<R> Default for WebRouter<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> WebRouter<R> {
    /// An empty router: no routes, no upgrade owner, and an unclaimed fallback seat.
    #[must_use]
    pub fn new() -> Self {
        Self {
            exact: BTreeMap::new(),
            prefixes: BTreeMap::new(),
            upgrades: BTreeMap::new(),
            fallback: None,
        }
    }

    /// Register a named route.
    ///
    /// Upstream `WebServer#register`. The returned disposer is modelled as [`Self::unregister`],
    /// because a Rust caller that drops the router does not need a closure to release one entry and a
    /// closure would have to outlive a borrow of the router.
    ///
    /// # Errors
    ///
    /// [`RouteError`] when the same `(kind, path)` pair is already registered.
    pub fn register(
        &mut self,
        kind: RouteKind,
        path: impl Into<String>,
        responder: Responder<R>,
    ) -> Result<(), RouteError> {
        let path = path.into();
        let table = match kind {
            RouteKind::Exact => &mut self.exact,
            RouteKind::Prefix => &mut self.prefixes,
        };
        if table.contains_key(&path) {
            return Err(RouteError { kind, path });
        }
        table.insert(path, responder);
        Ok(())
    }

    /// Remove one named route, reporting whether it was there.
    ///
    /// Upstream's registration disposer.
    pub fn unregister(&mut self, kind: RouteKind, path: &str) -> bool {
        let table = match kind {
            RouteKind::Exact => &mut self.exact,
            RouteKind::Prefix => &mut self.prefixes,
        };
        table.remove(path).is_some()
    }

    /// Register an exact-path HTTP upgrade route.
    ///
    /// Upstream `registerUpgrade`: exact only, because one socket can have only one protocol owner.
    ///
    /// # Errors
    ///
    /// [`RouteError`] when the path already has an upgrade owner.
    pub fn register_upgrade(
        &mut self,
        path: impl Into<String>,
        value: R,
    ) -> Result<(), RouteError> {
        let path = path.into();
        if self.upgrades.contains_key(&path) {
            return Err(RouteError {
                kind: RouteKind::Exact,
                path,
            });
        }
        self.upgrades.insert(path, value);
        Ok(())
    }

    /// Remove one upgrade route.
    pub fn unregister_upgrade(&mut self, path: &str) -> bool {
        self.upgrades.remove(path).is_some()
    }

    /// The upgrade owner for a pathname, if any.
    #[must_use]
    pub fn upgrade_route(&self, pathname: &str) -> Option<(&str, &R)> {
        self.upgrades
            .get_key_value(pathname)
            .map(|(path, value)| (path.as_str(), value))
    }

    /// Claim the fallback seat.
    ///
    /// Upstream `registerFallback`: one owner only, because two fallbacks cannot compose.
    ///
    /// # Errors
    ///
    /// [`FallbackError`] when the seat is already claimed.
    pub fn claim_fallback(&mut self, responder: Responder<R>) -> Result<(), FallbackError> {
        if self.fallback.is_some() {
            return Err(FallbackError);
        }
        self.fallback = Some(responder);
        Ok(())
    }

    /// Release the fallback seat, returning the responder that held it.
    ///
    /// Upstream's fallback disposer; disposal is what makes the seat claimable again, which a hot
    /// reload depends on.
    pub fn release_fallback(&mut self) -> Option<Responder<R>> {
        self.fallback.take()
    }

    /// Whether the fallback seat is claimed.
    #[must_use]
    pub fn has_fallback(&self) -> bool {
        self.fallback.is_some()
    }

    /// The named route for a pathname: an exact match first, then the longest matching prefix.
    ///
    /// Upstream `WebServer#match`.
    #[must_use]
    pub fn match_route(&self, pathname: &str) -> Option<(&str, RouteKind, &Responder<R>)> {
        if let Some((path, responder)) = self.exact.get_key_value(pathname) {
            return Some((path.as_str(), RouteKind::Exact, responder));
        }
        // Longest prefix wins. BTreeMap order is lexicographic, which says nothing about length, so
        // every candidate is compared; the table holds routes, not request paths.
        let mut best: Option<(&str, &Responder<R>)> = None;
        for (prefix, responder) in &self.prefixes {
            // A prefix may or may not carry its trailing separator, and a caller registering `/api/` means the same
            // directory as one registering `/api`. Joining unconditionally produced `/api//`, which nothing starts with,
            // so a prefix route with a trailing slash could never match anything.
            let boundary = if prefix.ends_with('/') {
                prefix.clone()
            } else {
                format!("{prefix}/")
            };
            if pathname != prefix && !pathname.starts_with(&boundary) {
                continue;
            }
            if best.is_none_or(|(current, _)| prefix.len() > current.len()) {
                best = Some((prefix.as_str(), responder));
            }
        }
        best.map(|(path, responder)| (path, RouteKind::Prefix, responder))
    }

    /// The responder for one pathname: the matched named route, otherwise the fallback seat.
    ///
    /// Upstream `responderFor`.
    #[must_use]
    pub fn responder_for(&self, pathname: &str) -> Option<&Responder<R>> {
        self.match_route(pathname)
            .map(|(_, _, responder)| responder)
            .or(self.fallback.as_ref())
    }

    /// Answer one in-process resource read.
    ///
    /// Upstream `readResource`: the method fence runs before matching, a handler route or an
    /// unmatched path is a 404, and a HEAD read returns the GET status and headers with no body.
    /// `respond` is only called for a resource route, so a read never materialises a body the caller
    /// cannot use.
    pub fn answer(
        &self,
        method: &str,
        pathname: &str,
        respond: impl FnOnce(&R) -> ResourceResponse,
    ) -> ResourceAnswer {
        if !is_resource_method(method) {
            return ResourceAnswer::MethodNotAllowed;
        }
        match self.responder_for(pathname) {
            Some(Responder::Resource(resource)) => {
                let response = respond(resource);
                if method.eq_ignore_ascii_case("HEAD") {
                    ResourceAnswer::Found(response.head())
                } else {
                    ResourceAnswer::Found(response)
                }
            }
            Some(Responder::Handler) | None => ResourceAnswer::NotFound,
        }
    }
}

/// The fallback seat is already claimed.
///
/// Upstream's `webserver: fallback already registered`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FallbackError;

impl fmt::Display for FallbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("webserver: fallback already registered")
    }
}

impl std::error::Error for FallbackError {}

/// Whether a method is one a resource answers.
///
/// Upstream `RESOURCE_METHODS`: GET and HEAD. The check accepts any case because HTTP methods are
/// case-sensitive in the specification but every real client sends the upper-case spelling, and
/// rejecting `head` for that reason would be a defect rather than a convention.
#[must_use]
pub fn is_resource_method(method: &str) -> bool {
    method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD")
}

// -------------------------------------------------------------------------------------------------
// Index injections
// -------------------------------------------------------------------------------------------------

/// Document region a rendered injection row lands in: after the opening `<head>` or `<body>` tag.
///
/// Upstream `IndexInjectionPlacement` in `packages/host/webserver/src/injections.ts`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Immediately after the opening head tag.
    Head,
    /// Immediately after the opening body tag.
    Body,
}

/// One structured index injection row.
///
/// Upstream `IndexInjection` in `packages/host/webserver/src/injections.ts`: pure data, because one
/// table feeds two renderers — the served form here, and a worker deployment that ships the same rows
/// over a boot payload.
///
/// The two text-carrying variants document a constraint upstream states and does not enforce: `text`
/// must not contain `</script` (or, for a style row, `</style`), which would close the element early.
/// A caller that inserted untrusted text here would be injecting markup, so the check belongs at the
/// point that decides what is untrusted, not in the renderer.
#[derive(Clone, Debug, PartialEq)]
pub enum IndexInjection {
    /// Assign a JSON-serialisable value to a `globalThis` property, ahead of later script rows.
    /// `None` renders as JavaScript `undefined`, which is what an omitted value means upstream.
    Global {
        /// The `globalThis` property name.
        name: String,
        /// The value; `None` renders `undefined`.
        value: Option<Value>,
    },
    /// Inline classic script.
    Script {
        /// The document region the element lands in.
        placement: Placement,
        /// The script text.
        text: String,
    },
    /// External classic script, executed in table order.
    ScriptSrc {
        /// The document region the element lands in.
        placement: Placement,
        /// The script URL.
        src: String,
    },
    /// Advisory preload for an external classic script; static workers may ignore it.
    ScriptPreload {
        /// The script URL.
        src: String,
    },
    /// A `<style>` element in the head.
    Style {
        /// The stylesheet text.
        text: String,
    },
    /// Raw markup fragment.
    Html {
        /// The document region the fragment lands in.
        placement: Placement,
        /// The markup.
        html: String,
    },
}

/// The tail script settling the boot-readiness deferred before any client entry reads injected state.
///
/// Upstream `READY_MARKUP` in `packages/host/webserver/src/injections.ts`. Whichever side runs first
/// creates the deferred (`??=`), so a bootstrap that applies the table asynchronously still settles
/// it after the last row; the served form creates and resolves it in one statement because every row
/// is already in the document text.
pub const READY_MARKUP: &str =
    "<script>(globalThis.__DSH_BOOT_READY__ ??= Promise.withResolvers()).resolve()</script>";

/// Escape one value for a double-quoted HTML attribute.
///
/// Upstream `escapeHtmlAttribute`: `&`, `"`, `<`, and `>` — `&` first, because escaping it last would
/// turn the ampersands of the other escapes into entities again.
#[must_use]
pub fn escape_html_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render one row to markup and its placement.
///
/// Upstream `renderRow`. A `global` row's JSON has every `<` rewritten as `\u003c` so a row-controlled
/// string cannot close the script element, which is the one escape a JSON encoder does not do.
#[must_use]
pub fn render_index_row(row: &IndexInjection) -> (Placement, String) {
    match row {
        IndexInjection::Global { name, value } => {
            let name = json_script_safe(&Value::String(name.clone()));
            let value = match value {
                Some(value) => json_script_safe(value),
                None => "undefined".to_owned(),
            };
            (
                Placement::Head,
                format!("<script>globalThis[{name}] = {value}</script>"),
            )
        }
        IndexInjection::Script { placement, text } => {
            (*placement, format!("<script>{text}</script>"))
        }
        IndexInjection::ScriptSrc { placement, src } => (
            *placement,
            format!("<script src=\"{}\"></script>", escape_html_attribute(src)),
        ),
        IndexInjection::ScriptPreload { src } => (
            Placement::Head,
            format!(
                "<link rel=\"preload\" as=\"script\" href=\"{}\">",
                escape_html_attribute(src)
            ),
        ),
        IndexInjection::Style { text } => (Placement::Head, format!("<style>{text}</style>")),
        IndexInjection::Html { placement, html } => (*placement, html.clone()),
    }
}

/// Serialise a value for a script context, with every `<` escaped.
///
/// `serde_json` cannot be told to escape `<` the way `JSON.stringify(...).replaceAll('<', '\\u003c')`
/// can, so the escape is a textual pass over the serialised text. A `Value` that fails to serialise
/// is not representable (`serde_json::Value` has no failure mode here), and `null` is the safe stand-in
/// rather than a panic.
fn json_script_safe(value: &Value) -> String {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
    text.replace('<', "\\u003c")
}

/// Find the end of an opening tag, case-insensitively, returning the index just past its `>`.
///
/// Upstream matches `/<head(?:\s[^>]*)?>/i`, which stops at the first `>` even inside a quoted
/// attribute. That limitation is mirrored deliberately: a more capable parser here would render rows
/// into a position the other renderer (the worker form) does not agree on.
fn find_open_tag(html: &str, tag: &str) -> Option<usize> {
    let bytes = html.as_bytes();
    let needle = format!("<{tag}");
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find(&needle) {
        let start = from + at;
        let after = start + needle.len();
        let boundary = bytes.get(after).copied();
        // `<head>` and `<head ...>`; `<header>` must not match.
        if matches!(boundary, Some(b'>' | b' ' | b'\t' | b'\n' | b'\r')) {
            let close = bytes[after..].iter().position(|byte| *byte == b'>')?;
            return Some(after + close + 1);
        }
        from = start + needle.len();
    }
    None
}

/// Render the structured injection table into an index.html body.
///
/// Upstream `renderIndexInjections` (`packages/host/webserver/src/injections.ts`): head rows land
/// immediately after the opening head tag, body rows immediately after the opening body tag, each
/// group in table order, and the boot-readiness tail after the last body row. Tag-less fragments —
/// which is what a fixture page is — receive head rows prepended and body rows appended, because the
/// HTML parser synthesises a body around them either way.
#[must_use]
pub fn render_index_injections(html: &str, rows: &[IndexInjection]) -> String {
    let mut head = String::new();
    let mut body = String::new();
    for row in rows {
        let (placement, markup) = render_index_row(row);
        match placement {
            Placement::Head => head.push_str(&markup),
            Placement::Body => body.push_str(&markup),
        }
    }
    body.push_str(READY_MARKUP);
    let mut out = html.to_owned();
    if !head.is_empty() {
        out = match find_open_tag(&out, "head") {
            Some(at) => splice(&out, at, &head),
            None => format!("{head}{out}"),
        };
    }
    if !body.is_empty() {
        out = match find_open_tag(&out, "body") {
            Some(at) => splice(&out, at, &body),
            None => format!("{out}{body}"),
        };
    }
    out
}

/// Insert `markup` into `html` at byte offset `at`.
fn splice(html: &str, at: usize, markup: &str) -> String {
    let mut out = String::with_capacity(html.len() + markup.len());
    out.push_str(&html[..at]);
    out.push_str(markup);
    out.push_str(&html[at..]);
    out
}

/// Apply raw html-to-html transforms in registration order.
///
/// Upstream `WebServer#applyIndexTaps`: the escape hatch for markup no structured row expresses, run
/// after the rows are rendered so a tap sees the document a browser would.
#[must_use]
pub fn apply_index_taps(html: &str, taps: &[&dyn Fn(&str) -> String]) -> String {
    let mut out = html.to_owned();
    for tap in taps {
        out = tap(&out);
    }
    out
}

// -------------------------------------------------------------------------------------------------
// Static frontend serving
// -------------------------------------------------------------------------------------------------

/// The MIME type one dist file is served as, from its extension.
///
/// Upstream's `MIME` table plus its `application/octet-stream` fallback
/// (`packages/host/frontend-static/src/index.ts`). The table is short because it only has to cover
/// what a built browser bundle ships; an unknown extension is bytes, not a guess.
#[must_use]
pub fn mime_for_path(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("html") => HTML_MIME,
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        // Launchers require an image Content-Type for the install icons.
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("json" | "map") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        // The packed VFS image is served as its own bytes, never as a Content-Encoding: the worker
        // inflates it itself, and a transport-level encoding would leave it inflating a decoded body.
        Some("gz") => "application/gzip",
        _ => OCTET_STREAM_MIME,
    }
}

/// An absolute dist anchor: the directory files are served from and the index that renders.
///
/// Upstream `Config.distIndex`, whose directory is the dist root (`dirname(distIndex)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaticConfig {
    /// The absolute dist root: no response may name a path outside it.
    pub dist_root: PathBuf,
    /// The absolute path of index.html inside the dist root.
    pub dist_index: PathBuf,
}

impl StaticConfig {
    /// Build the pair from the configured index path.
    ///
    /// # Errors
    ///
    /// [`StaticConfigError::NotAbsolute`] when the index path is relative. Upstream does not check
    /// this, because the composing application supplies a workspace path expression; a relative one
    /// here would resolve against the serving process's working directory, which is exactly the kind
    /// of hidden rebase the traversal fence exists to prevent.
    pub fn from_index(dist_index: impl Into<PathBuf>) -> Result<Self, StaticConfigError> {
        let dist_index = dist_index.into();
        if !dist_index.is_absolute() {
            return Err(StaticConfigError::NotAbsolute(dist_index));
        }
        let dist_root = dist_index
            .parent()
            .map_or_else(|| dist_index.clone(), Path::to_path_buf);
        Ok(Self {
            dist_root,
            dist_index,
        })
    }
}

/// Why a dist anchor was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StaticConfigError {
    /// The configured index path is not absolute.
    NotAbsolute(PathBuf),
}

impl fmt::Display for StaticConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAbsolute(path) => write!(
                f,
                "webserver: distIndex {} is not an absolute path",
                path.display()
            ),
        }
    }
}

impl std::error::Error for StaticConfigError {}

/// One dist request: the method, the pathname, and the validation header that matters.
///
/// Upstream `serveStatic` reads `request.method`, the URL pathname, and `if-none-match`; those are
/// the whole request surface it uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StaticRequest<'a> {
    /// The HTTP method.
    pub method: &'a str,
    /// The request pathname, still percent-encoded.
    pub path: &'a str,
    /// The `If-None-Match` value, when the client sent one.
    pub if_none_match: Option<&'a str>,
}

impl<'a> StaticRequest<'a> {
    /// A GET request for a pathname.
    #[must_use]
    pub const fn get(path: &'a str) -> Self {
        Self {
            method: "GET",
            path,
            if_none_match: None,
        }
    }

    /// The same request with no revalidation header removed from consideration.
    #[must_use]
    pub const fn with_if_none_match(mut self, value: &'a str) -> Self {
        self.if_none_match = Some(value);
        self
    }
}

/// One dist response: exactly the fields a caller sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaticResponse {
    /// The status code.
    pub status: u16,
    /// The `content-type`; absent for an error or 304 answer.
    pub content_type: Option<&'static str>,
    /// The `cache-control`; absent for the index, which must not be cached.
    pub cache_control: Option<&'static str>,
    /// The weak `ETag` of the file's bytes; absent for the index.
    pub etag: Option<String>,
    /// The `allow` header for a 405 answer.
    pub allow: Option<&'static str>,
    /// The body; empty for HEAD, for a 304, and for an error.
    pub body: Vec<u8>,
}

/// Why a dist request could not be answered at all.
///
/// Upstream separates two classes: a target that cannot be named at the wire throws before any read
/// (which the webserver answers 400), while an unreadable file other than a missing or non-file one
/// reaches the caller. Both are errors here; a 404 is not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StaticError {
    /// The pathname holds a malformed percent escape or is not valid UTF-8 once decoded.
    MalformedPath(String),
    /// The filesystem refused a read for a reason that is not "missing".
    Io(String),
}

impl fmt::Display for StaticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedPath(path) => write!(f, "webserver: malformed request path {path:?}"),
            Self::Io(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for StaticError {}

/// Decode the percent escapes in one request pathname.
///
/// Upstream `decodeURIComponent`. `+` stays a plus: this is a path, not a form body, and rewriting it
/// to a space would make a file whose name contains `+` unreachable.
fn percent_decode(path: &str) -> Result<String, StaticError> {
    let bytes = path.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'%' {
            out.push(bytes[at]);
            at += 1;
            continue;
        }
        let Some(high) = bytes.get(at + 1).and_then(|byte| hex_value(*byte)) else {
            return Err(StaticError::MalformedPath(path.to_owned()));
        };
        let Some(low) = bytes.get(at + 2).and_then(|byte| hex_value(*byte)) else {
            return Err(StaticError::MalformedPath(path.to_owned()));
        };
        out.push(high << 4 | low);
        at += 3;
    }
    // A NUL cannot appear in a path the filesystem will accept, and passing one through would surface
    // as an `InvalidInput` I/O error rather than the malformed-path answer the wire deserves.
    if out.contains(&0) {
        return Err(StaticError::MalformedPath(path.to_owned()));
    }
    String::from_utf8(out).map_err(|_| StaticError::MalformedPath(path.to_owned()))
}

/// One hexadecimal digit's value.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The filesystem target of one request, or the reason it has none.
///
/// Upstream `resolve(normalize(join(distRoot, decodeURIComponent(pathname))))`. The join is
/// component-wise and clamps `..` at the filesystem root, so `/../../etc/passwd` resolves to
/// `<root>/../../etc/passwd`'s real target — outside the root, and therefore refused — rather than
/// silently collapsing to the root and serving the index.
fn dist_target(config: &StaticConfig, pathname: &str) -> Result<PathBuf, StaticError> {
    let decoded = percent_decode(pathname)?;
    let mut target = config.dist_root.clone();
    for part in decoded.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                target.pop();
            }
            name => target.push(name),
        }
    }
    Ok(target)
}

/// Answer one dist request.
///
/// Upstream `serveStatic` (`packages/host/frontend-static/src/index.ts`):
///
/// - traversal outside the dist root is 403, and the root itself and the configured index path render
///   the index;
/// - an index render goes through `render_index`, so a caller composes its injections exactly as the
///   webserver does;
/// - every other file carries a weak `ETag` of its bytes with `cache-control: no-cache`, and a match
///   `If-None-Match` is answered 304 with the same validators but no body;
/// - the index carries neither, because a stale shell would keep serving a stale injection table;
/// - a missing or non-file target is an empty 404 with no content type, which is what keeps a miss
///   from being mistaken for the HTML shell;
/// - HEAD keeps the status and headers and drops the body;
/// - any method other than GET or HEAD is 405 with `allow: GET, HEAD`, which upstream answers before
///   the resource runs.
///
/// # Errors
///
/// [`StaticError::MalformedPath`] for a bad percent escape and [`StaticError::Io`] for a filesystem
/// failure other than a missing or non-file target.
pub fn serve_static(
    request: &StaticRequest<'_>,
    config: &StaticConfig,
    render_index: &dyn Fn(&str) -> String,
) -> Result<StaticResponse, StaticError> {
    if !is_resource_method(request.method) {
        return Ok(StaticResponse {
            status: 405,
            content_type: None,
            cache_control: None,
            etag: None,
            allow: Some(RESOURCE_METHODS),
            body: Vec::new(),
        });
    }
    let target = dist_target(config, request.path)?;
    // The target must be the root itself or stay under it. Comparing components rather than a string
    // prefix is what stops `<root>-backup/x` from passing as a child of `<root>`.
    if target != config.dist_root && !target.starts_with(&config.dist_root) {
        return Ok(StaticResponse {
            status: 403,
            content_type: None,
            cache_control: None,
            etag: None,
            allow: None,
            body: Vec::new(),
        });
    }
    let mut response = read_dist_target(request, config, &target, render_index)?;
    if request.method.eq_ignore_ascii_case("HEAD") {
        response.body.clear();
    }
    Ok(response)
}

/// The response for one already-confined target.
fn read_dist_target(
    request: &StaticRequest<'_>,
    config: &StaticConfig,
    target: &Path,
    render_index: &dyn Fn(&str) -> String,
) -> Result<StaticResponse, StaticError> {
    if target == config.dist_root || target == config.dist_index {
        // The index is read as text and rendered; a dist build with no index is an empty 404 for both
        // of its entry paths rather than a 500.
        let raw = match std::fs::read(config.dist_index.clone()) {
            Ok(raw) => raw,
            Err(error) if is_miss(&error) => return Ok(not_found()),
            Err(error) => return Err(StaticError::Io(error.to_string())),
        };
        let Ok(raw) = String::from_utf8(raw) else {
            return Err(StaticError::Io(format!(
                "webserver: dist index {} is not valid UTF-8",
                config.dist_index.display()
            )));
        };
        return Ok(StaticResponse {
            status: 200,
            content_type: Some(HTML_MIME),
            cache_control: None,
            etag: None,
            allow: None,
            body: render_index(&raw).into_bytes(),
        });
    }
    let body = match std::fs::read(target) {
        Ok(body) => body,
        Err(error) if is_miss(&error) => return Ok(not_found()),
        Err(error) => return Err(StaticError::Io(error.to_string())),
    };
    // Weak, because a transport-level gzip changes the transferred bytes without changing the entity
    // the client ends up with.
    let etag = weak_etag(&body);
    let cache_control = Some("no-cache");
    if request.if_none_match == Some(etag.as_str()) {
        return Ok(StaticResponse {
            status: 304,
            content_type: None,
            cache_control,
            etag: Some(etag),
            allow: None,
            body: Vec::new(),
        });
    }
    Ok(StaticResponse {
        status: 200,
        content_type: Some(mime_for_path(target)),
        cache_control,
        etag: Some(etag),
        allow: None,
        body,
    })
}

/// An empty, header-free 404: the answer a dist miss must give.
fn not_found() -> StaticResponse {
    StaticResponse {
        status: 404,
        content_type: None,
        cache_control: None,
        etag: None,
        allow: None,
        body: Vec::new(),
    }
}

/// Whether a filesystem failure means "no such file here" rather than "the disk is broken".
///
/// Upstream `STATIC_MISS_CODES`: `ENOENT`, `EISDIR`, `ENOTDIR`. A directory requested as a file, or a
/// file used as a path component, is a miss — not a server error, and not a directory listing.
#[must_use]
pub fn is_miss(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::IsADirectory | io::ErrorKind::NotADirectory
    )
}

/// The weak `ETag` of one body: `W/"` plus the base64url of its SHA-1, without padding.
///
/// Upstream `W/"${createHash('sha1').update(body).digest('base64url')}"`
/// (`packages/host/frontend-static/src/index.ts`). The algorithm is reproduced rather than depended
/// on: this crate carries no digest or base64 crate, and adding one for a cache validator would be a
/// dependency decision made by a leaf module. SHA-1 is used here for the same reason upstream uses it
/// — change detection, not authentication — and a validator is only ever compared for equality.
#[must_use]
pub fn weak_etag(body: &[u8]) -> String {
    format!("W/\"{}\"", base64url(&sha1(body)))
}

/// SHA-1 of `body` (FIPS 180-4), reproduced so an `ETag` needs no dependency.
#[must_use]
#[allow(
    clippy::many_single_char_names,
    reason = "the round variable names are the ones FIPS 180-4 uses; renaming them would hide a transcription error"
)]
pub fn sha1(body: &[u8]) -> [u8; 20] {
    let mut state: [u32; 5] = [
        0x6745_2301,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];
    let bit_len = (body.len() as u64).wrapping_mul(8);
    let mut message = body.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in message.chunks_exact(64) {
        let mut words = [0_u32; 80];
        for (at, word) in chunk.chunks_exact(4).enumerate() {
            words[at] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for at in 16..80 {
            words[at] =
                (words[at - 3] ^ words[at - 8] ^ words[at - 14] ^ words[at - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (at, word) in words.iter().enumerate() {
            let (function, constant) = match at {
                0..=19 => ((b & c) | (!b & d), 0x5a82_7999_u32),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(function)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }
    let mut out = [0_u8; 20];
    for (at, word) in state.iter().enumerate() {
        out[at * 4..at * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// base64url without padding, as `Buffer#toString('base64url')` writes it.
#[must_use]
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let triple = (u32::from(first) << 16) | (u32::from(second) << 8) | u32::from(third);
        // Six bits per character; the final partial group contributes only the characters whose bits
        // exist, which is what makes the encoding unpadded.
        for (at, shift) in [18_u32, 12, 6, 0].into_iter().enumerate() {
            if at > chunk.len() {
                break;
            }
            let index = usize::try_from((triple >> shift) & 0x3f).unwrap_or(0);
            out.push(char::from(ALPHABET[index]));
        }
    }
    out
}
