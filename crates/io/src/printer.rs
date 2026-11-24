//! Direct printer LAN/local upload client, saved printer registry, and RFC 2617/7616 Digest auth.
//!
//! Data structures, URL/path sanitization, HTTP Digest auth, and multipart form assembly compile
//! across all targets (including `wasm32-unknown-unknown`). Native blocking HTTP transport (`ureq`)
//! and local credentials storage (`printers.json` with `0600` Unix permissions) are gated to
//! non-`wasm32` targets.

use std::fmt::Write as _;

use md5::Digest as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;

/// Supported local/LAN printer API protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrinterProtocol {
    #[default]
    PrusaLink,
    PrusaConnectLocal,
    #[serde(alias = "octoprint")]
    OctoPrint,
    Moonraker,
}

impl PrinterProtocol {
    /// All supported printer upload protocols.
    pub const ALL: [Self; 4] = [
        Self::PrusaLink,
        Self::PrusaConnectLocal,
        Self::OctoPrint,
        Self::Moonraker,
    ];

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::PrusaLink => "PrusaLink",
            Self::PrusaConnectLocal => "Prusa Connect Local",
            Self::OctoPrint => "OctoPrint",
            Self::Moonraker => "Moonraker / Klipper",
        }
    }

    /// Default remote storage identifier for this protocol.
    pub fn default_storage(self) -> &'static str {
        match self {
            Self::PrusaLink => "usb",
            Self::PrusaConnectLocal | Self::OctoPrint => "local",
            Self::Moonraker => "gcodes",
        }
    }

    /// Whether this protocol supports `.bgcode` natively.
    pub fn supports_bgcode(self) -> bool {
        matches!(self, Self::PrusaLink | Self::PrusaConnectLocal)
    }
}

/// Authentication method and credentials for a printer connection.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrinterAuth {
    #[default]
    None,
    ApiKey {
        api_key: String,
    },
    Digest {
        username: String,
        password: String,
    },
    Bearer {
        token: String,
    },
}

impl PrinterAuth {
    /// Short human-readable kind name.
    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::None => "None",
            Self::ApiKey { .. } => "API Key",
            Self::Digest { .. } => "HTTP Digest",
            Self::Bearer { .. } => "Bearer Token",
        }
    }

    /// Return `true` when non-empty credentials are configured.
    pub fn has_credentials(&self) -> bool {
        match self {
            Self::None => false,
            Self::ApiKey { api_key } => !api_key.is_empty(),
            Self::Digest { username, password } => !username.is_empty() || !password.is_empty(),
            Self::Bearer { token } => !token.is_empty(),
        }
    }
}

/// Saved printer connection profile (kept local to disk; never embedded in `.3mf` files).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct PrinterProfile {
    pub id: String,
    pub name: String,
    pub protocol: PrinterProtocol,
    pub base_url: String,
    pub auth: PrinterAuth,
    pub storage: Option<String>,
    pub device_id: Option<String>,
    pub prefer_bgcode: bool,
    pub accept_invalid_certs: bool,
    pub timeout_secs: u64,
}

impl Default for PrinterProfile {
    fn default() -> Self {
        Self {
            id: "printer-1".to_owned(),
            name: "Prusa CORE One+".to_owned(),
            protocol: PrinterProtocol::PrusaLink,
            base_url: String::new(),
            auth: PrinterAuth::None,
            storage: None,
            device_id: None,
            prefer_bgcode: true,
            accept_invalid_certs: false,
            timeout_secs: 30,
        }
    }
}

impl PrinterProfile {
    /// Return a redacted summary safe for API responses and logs (no secrets exposed).
    pub fn summary(&self) -> PrinterProfileSummary {
        let username = match &self.auth {
            PrinterAuth::Digest { username, .. } if !username.is_empty() => Some(username.clone()),
            _ => None,
        };
        PrinterProfileSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            protocol: self.protocol,
            base_url: self.base_url.clone(),
            auth_kind: self.auth.kind_label().to_owned(),
            has_credentials: self.auth.has_credentials(),
            username,
            storage: self.storage.clone(),
            device_id: self.device_id.clone(),
            prefer_bgcode: self.prefer_bgcode,
            accept_invalid_certs: self.accept_invalid_certs,
            timeout_secs: self.timeout_secs,
        }
    }

    /// Resolve effective storage root using the profile override or protocol default.
    pub fn effective_storage<'a>(&'a self, override_storage: Option<&'a str>) -> &'a str {
        override_storage
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.storage
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| self.protocol.default_storage())
    }
}

/// Redacted view of a [`PrinterProfile`] containing zero secret credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrinterProfileSummary {
    pub id: String,
    pub name: String,
    pub protocol: PrinterProtocol,
    pub base_url: String,
    pub auth_kind: String,
    pub has_credentials: bool,
    pub username: Option<String>,
    pub storage: Option<String>,
    pub device_id: Option<String>,
    pub prefer_bgcode: bool,
    pub accept_invalid_certs: bool,
    pub timeout_secs: u64,
}

/// Available storage target reported by a printer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrinterStorageInfo {
    pub name: String,
    pub path: Option<String>,
    pub read_only: bool,
    pub ready: bool,
}

/// Live telemetry & info probe result returned by `probe_printer`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PrinterStatusReport {
    pub printer_id: String,
    pub printer_name: String,
    pub protocol: PrinterProtocol,
    pub base_url: String,
    pub reachable: bool,
    pub firmware: Option<String>,
    pub model: Option<String>,
    pub hostname: Option<String>,
    pub serial: Option<String>,
    pub state: Option<String>,
    pub nozzle_temp_c: Option<f64>,
    pub target_nozzle_temp_c: Option<f64>,
    pub bed_temp_c: Option<f64>,
    pub target_bed_temp_c: Option<f64>,
    pub storages: Vec<PrinterStorageInfo>,
    pub warnings: Vec<String>,
}

/// Parameters controlling a direct upload to a printer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct UploadOptions {
    pub remote_path: String,
    pub storage: Option<String>,
    pub start_print: bool,
    pub overwrite: bool,
}

impl Default for UploadOptions {
    fn default() -> Self {
        Self {
            remote_path: "layercraft_out.bgcode".to_owned(),
            storage: None,
            start_print: false,
            overwrite: true,
        }
    }
}

/// Outcome returned after a successful direct printer upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UploadResult {
    pub printer_id: String,
    pub printer_name: String,
    pub protocol: PrinterProtocol,
    pub base_url: String,
    pub storage: String,
    pub remote_path: String,
    pub bytes_uploaded: usize,
    pub start_print_requested: bool,
    pub http_status: u16,
    pub message: String,
}

/// Cooperative byte-progress and cancellation handle for direct printer uploads.
#[derive(Debug, Default)]
pub struct UploadProgress {
    sent_bytes: std::sync::atomic::AtomicUsize,
    total_bytes: std::sync::atomic::AtomicUsize,
    cancelled: std::sync::atomic::AtomicBool,
}

impl UploadProgress {
    /// Create a new upload progress tracker initialized to `0 / 0`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cooperative cancellation of an active upload.
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Return `true` if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Initialize total byte count and reset sent bytes to zero.
    pub fn set_total_bytes(&self, total: usize) {
        self.sent_bytes
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.total_bytes
            .store(total, std::sync::atomic::Ordering::Relaxed);
    }

    /// Reset sent byte counter to zero (for example before a Digest auth retry).
    pub fn reset_sent(&self) {
        self.sent_bytes
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record `delta` additional uploaded bytes.
    pub fn add_sent_bytes(&self, delta: usize) {
        self.sent_bytes
            .fetch_add(delta, std::sync::atomic::Ordering::Relaxed);
    }

    /// Snapshot `(sent_bytes, total_bytes)`.
    pub fn snapshot(&self) -> (usize, usize) {
        (
            self.sent_bytes.load(std::sync::atomic::Ordering::Relaxed),
            self.total_bytes.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

/// Errors produced during printer URL/path validation, persistence, auth, or HTTP upload.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PrinterError {
    #[error("invalid printer URL `{url}`: {reason}")]
    InvalidUrl { url: String, reason: &'static str },
    #[error("invalid remote file path `{path}`: {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error("invalid upload configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("printer authentication failed or rejected credentials ({0})")]
    AuthFailed(String),
    #[error("printer returned HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("HTTP transport failed: {0}")]
    Transport(String),
    #[error("invalid printer JSON/profile data: {0}")]
    InvalidResponse(String),
    #[error("failed reading or writing printer store: {0}")]
    StoreIo(String),
    #[error("printer upload cancelled by user")]
    Cancelled,
}

/// Validate and normalize a printer base URL (`http://...` or `https://...`, no trailing `/`).
pub fn normalize_base_url(raw: &str) -> Result<String, PrinterError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(PrinterError::InvalidUrl {
            url: raw.to_owned(),
            reason: "URL is empty",
        });
    }
    if trimmed
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '?' || c == '#')
    {
        return Err(PrinterError::InvalidUrl {
            url: raw.to_owned(),
            reason: "URL must not contain whitespace, control characters, query, or fragment",
        });
    }

    let normalized = if let Some((scheme, rest)) = trimmed.split_once("://") {
        let lower = scheme.to_ascii_lowercase();
        if lower != "http" && lower != "https" {
            return Err(PrinterError::InvalidUrl {
                url: raw.to_owned(),
                reason: "only http:// and https:// schemes are supported",
            });
        }
        format!("{lower}://{rest}")
    } else {
        if trimmed.contains("://") {
            return Err(PrinterError::InvalidUrl {
                url: raw.to_owned(),
                reason: "malformed scheme",
            });
        }
        format!("http://{trimmed}")
    };

    let without_trailing = normalized.trim_end_matches('/').to_owned();
    let after_scheme = without_trailing
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or("");
    let authority = after_scheme.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.starts_with(':') {
        return Err(PrinterError::InvalidUrl {
            url: raw.to_owned(),
            reason: "missing hostname",
        });
    }

    Ok(without_trailing)
}

/// Sanitize a remote file path and return `(clean_relative_path, url_encoded_path, basename)`.
pub fn sanitize_remote_path(raw: &str) -> Result<(String, String, String), PrinterError> {
    if raw.chars().any(|c| c == '\0' || c == '\r' || c == '\n') {
        return Err(PrinterError::InvalidPath {
            path: raw.to_owned(),
            reason: "control characters are not allowed",
        });
    }
    let normalized = raw.trim().replace('\\', "/");
    let stripped = normalized.trim_matches('/');
    if stripped.is_empty() {
        return Err(PrinterError::InvalidPath {
            path: raw.to_owned(),
            reason: "remote filename is empty",
        });
    }

    let mut clean_segments = Vec::new();
    let mut encoded_segments = Vec::new();
    for seg in stripped.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(PrinterError::InvalidPath {
                path: raw.to_owned(),
                reason: "path must not contain empty, `.`, or `..` segments",
            });
        }
        clean_segments.push(seg.to_owned());
        encoded_segments.push(percent_encode_component(seg));
    }

    let clean = clean_segments.join("/");
    let encoded = encoded_segments.join("/");
    let basename = clean_segments
        .last()
        .cloned()
        .unwrap_or_else(|| clean.clone());
    Ok((clean, encoded, basename))
}

fn percent_encode_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Parsed HTTP Digest authentication challenge from a `WWW-Authenticate` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestChallenge {
    pub realm: String,
    pub nonce: String,
    pub opaque: Option<String>,
    pub algorithm: Option<String>,
    pub qop: Option<String>,
    pub stale: bool,
}

/// Parse an HTTP `WWW-Authenticate: Digest ...` header value.
pub fn parse_digest_challenge(header: &str) -> Option<DigestChallenge> {
    let trimmed = header.trim();
    let lower = trimmed.to_ascii_lowercase();
    let idx = lower.find("digest ")?;
    let rest = trimmed.get(idx + "digest ".len()..)?;

    let mut realm: Option<String> = None;
    let mut nonce: Option<String> = None;
    let mut opaque: Option<String> = None;
    let mut algorithm: Option<String> = None;
    let mut qop: Option<String> = None;
    let mut stale = false;

    for (raw_k, val) in split_auth_params(rest) {
        let k = raw_k.to_ascii_lowercase();
        match k.as_str() {
            "realm" => realm = Some(val),
            "nonce" => nonce = Some(val),
            "opaque" => opaque = Some(val),
            "algorithm" => algorithm = Some(val),
            "qop" => qop = Some(val),
            "stale" => stale = val.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }

    Some(DigestChallenge {
        realm: realm?,
        nonce: nonce?,
        opaque,
        algorithm,
        qop,
        stale,
    })
}

fn split_auth_params(input: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        while i < bytes.len()
            && bytes
                .get(i)
                .is_some_and(|&b| b == b',' || b.is_ascii_whitespace())
        {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key_start = i;
        while i < bytes.len() && bytes.get(i).is_some_and(|&b| b != b'=') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key = input.get(key_start..i).unwrap_or("").trim().to_owned();
        i += 1; // skip '='
        while i < bytes.len() && bytes.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
            i += 1;
        }
        let val = if bytes.get(i) == Some(&b'"') {
            i += 1;
            let mut s = String::new();
            while i < bytes.len() {
                let Some(&b) = bytes.get(i) else {
                    break;
                };
                if b == b'\\' && i + 1 < bytes.len() {
                    if let Some(&esc) = bytes.get(i + 1) {
                        s.push(esc as char);
                    }
                    i += 2;
                    continue;
                }
                if b == b'"' {
                    i += 1;
                    break;
                }
                if let Some(ch_slice) = input.get(i..)
                    && let Some(ch) = ch_slice.chars().next()
                {
                    s.push(ch);
                    i += ch.len_utf8();
                } else {
                    i += 1;
                }
            }
            s
        } else {
            let val_start = i;
            while i < bytes.len() && bytes.get(i).is_some_and(|&b| b != b',') {
                i += 1;
            }
            input.get(val_start..i).unwrap_or("").trim().to_owned()
        };
        if !key.is_empty() {
            out.push((key, val));
        }
    }

    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DigestAlgo {
    Md5,
    Md5Sess,
    Sha256,
    Sha256Sess,
}

impl DigestAlgo {
    fn parse(raw: Option<&str>) -> Option<Self> {
        match raw.unwrap_or("MD5").trim().to_ascii_uppercase().as_str() {
            "MD5" => Some(Self::Md5),
            "MD5-SESS" => Some(Self::Md5Sess),
            "SHA-256" | "SHA256" => Some(Self::Sha256),
            "SHA-256-SESS" | "SHA256-SESS" => Some(Self::Sha256Sess),
            _ => None,
        }
    }

    fn is_sess(self) -> bool {
        matches!(self, Self::Md5Sess | Self::Sha256Sess)
    }

    fn header_name(self) -> &'static str {
        match self {
            Self::Md5 => "MD5",
            Self::Md5Sess => "MD5-sess",
            Self::Sha256 => "SHA-256",
            Self::Sha256Sess => "SHA-256-sess",
        }
    }

    fn hash_hex(self, input: &str) -> String {
        match self {
            Self::Md5 | Self::Md5Sess => {
                let digest = md5::Md5::digest(input.as_bytes());
                bytes_to_hex(&digest)
            }
            Self::Sha256 | Self::Sha256Sess => {
                let digest = Sha256::digest(input.as_bytes());
                bytes_to_hex(&digest)
            }
        }
    }
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn escape_quoted(val: &str) -> String {
    let mut out = String::with_capacity(val.len());
    for ch in val.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Construct an RFC 2617 / RFC 7616 `Authorization: Digest ...` header value.
pub fn build_digest_authorization(
    method: &str,
    uri: &str,
    username: &str,
    password: &str,
    challenge: &DigestChallenge,
    nc_count: u32,
    cnonce: &str,
) -> Result<String, PrinterError> {
    let algo = DigestAlgo::parse(challenge.algorithm.as_deref()).ok_or_else(|| {
        PrinterError::AuthFailed(format!(
            "unsupported Digest algorithm `{}`",
            challenge.algorithm.as_deref().unwrap_or("")
        ))
    })?;

    let use_qop_auth = challenge.qop.as_deref().is_some_and(|q| {
        q.split(',')
            .any(|tok| tok.trim().eq_ignore_ascii_case("auth"))
    });
    if challenge.qop.is_some() && !use_qop_auth {
        return Err(PrinterError::AuthFailed(format!(
            "unsupported Digest qop `{}`",
            challenge.qop.as_deref().unwrap_or("")
        )));
    }

    let mut ha1 = algo.hash_hex(&format!("{username}:{}:{password}", challenge.realm));
    if algo.is_sess() {
        ha1 = algo.hash_hex(&format!("{ha1}:{}:{cnonce}", challenge.nonce));
    }
    let ha2 = algo.hash_hex(&format!("{method}:{uri}"));
    let nc_hex = format!("{nc_count:08x}");

    let response = if use_qop_auth {
        algo.hash_hex(&format!(
            "{ha1}:{}:{nc_hex}:{cnonce}:auth:{ha2}",
            challenge.nonce
        ))
    } else {
        algo.hash_hex(&format!("{ha1}:{}:{ha2}", challenge.nonce))
    };

    let mut header = format!(
        "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\"",
        escape_quoted(username),
        escape_quoted(&challenge.realm),
        escape_quoted(&challenge.nonce),
        escape_quoted(uri),
        response
    );
    if challenge.algorithm.is_some() {
        let _ = write!(header, ", algorithm={}", algo.header_name());
    }
    if use_qop_auth {
        let _ = write!(
            header,
            ", qop=auth, nc={nc_hex}, cnonce=\"{}\"",
            escape_quoted(cnonce)
        );
    }
    if let Some(op) = &challenge.opaque {
        let _ = write!(header, ", opaque=\"{}\"", escape_quoted(op));
    }

    Ok(header)
}

/// Build an RFC 7578 `multipart/form-data` payload containing form fields and one binary file.
pub fn build_multipart_upload(
    boundary: &str,
    file_field: &str,
    filename: &str,
    content_type: &str,
    payload: &[u8],
    extra_fields: &[(&str, &str)],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 512);
    for &(name, val) in extra_fields {
        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b"\"\r\n\r\n");
        out.extend_from_slice(val.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    let safe_filename = filename.replace(['"', '\r', '\n'], "_");
    out.extend_from_slice(b"--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
    out.extend_from_slice(file_field.as_bytes());
    out.extend_from_slice(b"\"; filename=\"");
    out.extend_from_slice(safe_filename.as_bytes());
    out.extend_from_slice(b"\"\r\nContent-Type: ");
    out.extend_from_slice(content_type.as_bytes());
    out.extend_from_slice(b"\r\n\r\n");
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\r\n--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"--\r\n");
    out
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    #[cfg(any(
        all(not(test), any(target_os = "linux", target_os = "macos")),
        target_os = "windows"
    ))]
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::time::Duration;
    use ureq::Agent;
    use ureq::SendBody;
    use ureq::http::Response;
    use ureq::tls::{TlsConfig, TlsProvider};

    static DIGEST_NC: AtomicU32 = AtomicU32::new(1);
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);
    const KEYCHAIN_SENTINEL_PREFIX: &str = "keychain://layercraft/";
    #[cfg(all(not(test), any(target_os = "linux", target_os = "macos")))]
    const KEYCHAIN_SERVICE: &str = "layercraft-printer";

    pub(super) trait KeychainBackend {
        fn store(&self, account: &str, secret: &str) -> Result<(), String>;
        fn load(&self, account: &str) -> Result<Option<String>, String>;
    }

    struct OsKeychainBackend;

    impl KeychainBackend for OsKeychainBackend {
        fn store(&self, account: &str, secret: &str) -> Result<(), String> {
            #[cfg(test)]
            {
                let _ = (account, secret);
                Err("OS keychain subprocesses are disabled in unit tests".to_owned())
            }
            #[cfg(all(not(test), target_os = "linux"))]
            {
                let mut child = Command::new("secret-tool")
                    .args([
                        "store",
                        "--label=LayerCraft Printer Credential",
                        "service",
                        KEYCHAIN_SERVICE,
                        "account",
                        account,
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("spawn secret-tool: {e}"))?;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin
                        .write_all(secret.as_bytes())
                        .map_err(|e| format!("write secret-tool stdin: {e}"))?;
                }
                let output = child
                    .wait_with_output()
                    .map_err(|e| format!("wait secret-tool: {e}"))?;
                if output.status.success() {
                    Ok(())
                } else {
                    Err(format!(
                        "secret-tool exited {}: {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ))
                }
            }
            #[cfg(all(not(test), target_os = "macos"))]
            {
                let escaped_account = escape_security_arg(account);
                let escaped_secret = escape_security_arg(secret);
                let script = format!(
                    "add-generic-password -U -s {KEYCHAIN_SERVICE} -a {escaped_account} -w {escaped_secret}\n"
                );
                let mut child = Command::new("/usr/bin/security")
                    .arg("-i")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("spawn security: {e}"))?;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin
                        .write_all(script.as_bytes())
                        .map_err(|e| format!("write security stdin: {e}"))?;
                }
                let output = child
                    .wait_with_output()
                    .map_err(|e| format!("wait security: {e}"))?;
                if output.status.success() {
                    Ok(())
                } else {
                    Err(format!(
                        "security exited {}: {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ))
                }
            }
            #[cfg(all(not(test), not(any(target_os = "linux", target_os = "macos"))))]
            {
                let _ = (account, secret);
                Err("OS keyring CLI unavailable on this platform".to_owned())
            }
        }

        fn load(&self, account: &str) -> Result<Option<String>, String> {
            #[cfg(test)]
            {
                let _ = account;
                Ok(None)
            }
            #[cfg(all(not(test), target_os = "linux"))]
            {
                let output = Command::new("secret-tool")
                    .args(["lookup", "service", KEYCHAIN_SERVICE, "account", account])
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| format!("spawn secret-tool lookup: {e}"))?;
                if output.status.success() {
                    let s = String::from_utf8_lossy(&output.stdout)
                        .trim_end_matches(['\r', '\n'])
                        .to_owned();
                    Ok((!s.is_empty()).then_some(s))
                } else {
                    Ok(None)
                }
            }
            #[cfg(all(not(test), target_os = "macos"))]
            {
                let output = Command::new("/usr/bin/security")
                    .args([
                        "find-generic-password",
                        "-s",
                        KEYCHAIN_SERVICE,
                        "-a",
                        account,
                        "-w",
                    ])
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| format!("spawn security lookup: {e}"))?;
                if output.status.success() {
                    let s = String::from_utf8_lossy(&output.stdout)
                        .trim_end_matches(['\r', '\n'])
                        .to_owned();
                    Ok((!s.is_empty()).then_some(s))
                } else {
                    Ok(None)
                }
            }
            #[cfg(all(not(test), not(any(target_os = "linux", target_os = "macos"))))]
            {
                let _ = account;
                Ok(None)
            }
        }
    }

    #[cfg(all(not(test), target_os = "macos"))]
    fn escape_security_arg(raw: &str) -> String {
        let mut out = String::with_capacity(raw.len() + 2);
        out.push('"');
        for ch in raw.chars() {
            if matches!(ch, '"' | '\\') {
                out.push('\\');
            }
            if !ch.is_control() {
                out.push(ch);
            }
        }
        out.push('"');
        out
    }

    #[cfg(all(not(test), target_os = "linux"))]
    fn find_executable_in_path(name: &str) -> bool {
        let Some(path_var) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path_var).any(|dir| dir.join(name).is_file())
    }

    /// Return a short human-readable summary of how printer credentials are protected on this OS.
    pub fn printer_credential_storage_mode() -> &'static str {
        #[cfg(target_os = "macos")]
        {
            if Path::new("/usr/bin/security").is_file() {
                return "macOS Keychain + 0600 printers.json";
            }
        }
        #[cfg(all(not(test), target_os = "linux"))]
        {
            if find_executable_in_path("secret-tool") {
                return "System keyring (secret-tool) + 0600 printers.json";
            }
        }
        #[cfg(target_os = "windows")]
        {
            return "Restricted Windows ACL (current user) printers.json";
        }
        #[cfg(not(target_os = "windows"))]
        {
            "Restricted file permissions (0600 printers.json)"
        }
    }

    fn keychain_account_key(profile_id: &str, field: &str) -> String {
        let id = if profile_id.trim().is_empty() {
            "default"
        } else {
            profile_id.trim()
        };
        format!("{id}:{field}")
    }

    fn redact_secret(
        profile_id: &str,
        field_name: &str,
        secret: &mut String,
        backend: &dyn KeychainBackend,
    ) {
        if secret.is_empty() || secret.starts_with(KEYCHAIN_SENTINEL_PREFIX) {
            return;
        }
        let acct = keychain_account_key(profile_id, field_name);
        if backend.store(&acct, secret).is_ok() {
            *secret = format!("{KEYCHAIN_SENTINEL_PREFIX}{acct}");
        }
    }

    fn hydrate_secret(secret: &mut String, backend: &dyn KeychainBackend) {
        if let Some(acct) = secret.strip_prefix(KEYCHAIN_SENTINEL_PREFIX)
            && let Ok(Some(loaded)) = backend.load(acct)
        {
            *secret = loaded;
        }
    }

    pub(super) fn load_printer_profiles_with_backend(
        path: &Path,
        backend: &dyn KeychainBackend,
    ) -> Result<Vec<PrinterProfile>, PrinterError> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| PrinterError::StoreIo(format!("read {}: {e}", path.display())))?;
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut profiles: Vec<PrinterProfile> = serde_json::from_str(&text)
            .map_err(|e| PrinterError::StoreIo(format!("parse {}: {e}", path.display())))?;
        for profile in &mut profiles {
            match &mut profile.auth {
                PrinterAuth::None => {}
                PrinterAuth::ApiKey { api_key } => hydrate_secret(api_key, backend),
                PrinterAuth::Bearer { token } => hydrate_secret(token, backend),
                PrinterAuth::Digest { password, .. } => hydrate_secret(password, backend),
            }
        }
        Ok(profiles)
    }

    fn write_restricted_file(path: &Path, bytes: &[u8]) -> Result<(), PrinterError> {
        let Some(parent) = path.parent() else {
            return Err(PrinterError::StoreIo(format!(
                "missing parent directory for {}",
                path.display()
            )));
        };
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| PrinterError::StoreIo(format!("mkdir {}: {e}", parent.display())))?;
        }
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("printers.json");
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_path = parent.join(format!(".{file_name}.{}.{seq}.tmp", std::process::id()));

        let write_res = (|| -> Result<(), PrinterError> {
            #[cfg(unix)]
            let mut file = {
                use std::os::unix::fs::OpenOptionsExt as _;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&tmp_path)
                    .map_err(|e| {
                        PrinterError::StoreIo(format!("open {}: {e}", tmp_path.display()))
                    })?
            };
            #[cfg(not(unix))]
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)
                .map_err(|e| PrinterError::StoreIo(format!("open {}: {e}", tmp_path.display())))?;
            file.write_all(bytes)
                .map_err(|e| PrinterError::StoreIo(format!("write {}: {e}", tmp_path.display())))?;
            file.flush()
                .map_err(|e| PrinterError::StoreIo(format!("flush {}: {e}", tmp_path.display())))?;
            drop(file);
            std::fs::rename(&tmp_path, path)
                .map_err(|e| PrinterError::StoreIo(format!("rename {}: {e}", path.display())))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let perms = std::fs::Permissions::from_mode(0o600);
                std::fs::set_permissions(path, perms)
                    .map_err(|e| PrinterError::StoreIo(format!("chmod {}: {e}", path.display())))?;
            }
            #[cfg(target_os = "windows")]
            {
                if let Some(user) = std::env::var_os("USERNAME").and_then(|u| u.into_string().ok())
                {
                    let grant = format!("{user}:F");
                    let _ = Command::new("icacls")
                        .arg(path)
                        .args(["/inheritance:r", "/grant:r", &grant])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            }
            Ok(())
        })();

        if write_res.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        write_res
    }

    pub(super) fn save_printer_profiles_with_backend(
        path: &Path,
        profiles: &[PrinterProfile],
        backend: &dyn KeychainBackend,
    ) -> Result<(), PrinterError> {
        let mut sanitized = profiles.to_vec();
        for profile in &mut sanitized {
            let pid = profile.id.clone();
            match &mut profile.auth {
                PrinterAuth::None => {}
                PrinterAuth::ApiKey { api_key } => {
                    redact_secret(&pid, "api_key", api_key, backend);
                }
                PrinterAuth::Bearer { token } => {
                    redact_secret(&pid, "token", token, backend);
                }
                PrinterAuth::Digest { password, .. } => {
                    redact_secret(&pid, "digest_password", password, backend);
                }
            }
        }
        let json = serde_json::to_string_pretty(&sanitized)
            .map_err(|e| PrinterError::StoreIo(format!("serialize printers: {e}")))?;
        write_restricted_file(path, json.as_bytes())
    }

    /// Return the default disk path storing LayerCraft saved printer profiles (`printers.json`).
    pub fn default_printers_config_path() -> Option<PathBuf> {
        if let Some(custom) = std::env::var_os("LAYERCRAFT_PRINTERS_PATH")
            && !custom.is_empty()
        {
            return Some(PathBuf::from(custom));
        }
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            return Some(PathBuf::from(xdg).join("layercraft").join("printers.json"));
        }
        std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| {
            PathBuf::from(h)
                .join(".config")
                .join("layercraft")
                .join("printers.json")
        })
    }

    /// Load saved printer profiles from disk, hydrating any OS-keychain credentials.
    pub fn load_printer_profiles(path: &Path) -> Result<Vec<PrinterProfile>, PrinterError> {
        load_printer_profiles_with_backend(path, &OsKeychainBackend)
    }

    /// Persist saved printer profiles to disk (`0o600`/ACL protected, offloading secrets to OS keychain when available).
    pub fn save_printer_profiles(
        path: &Path,
        profiles: &[PrinterProfile],
    ) -> Result<(), PrinterError> {
        save_printer_profiles_with_backend(path, profiles, &OsKeychainBackend)
    }

    fn build_agent(profile: &PrinterProfile) -> Agent {
        let timeout = Duration::from_secs(profile.timeout_secs.clamp(5, 600));
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .unversioned_rustls_crypto_provider(Arc::new(rustls_rustcrypto::provider()))
            .disable_verification(profile.accept_invalid_certs)
            .build();
        Agent::config_builder()
            .tls_config(tls)
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .build()
            .new_agent()
    }

    fn next_cnonce() -> String {
        let seq = DIGEST_NC.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("{nanos:08x}{seq:08x}")
    }

    fn next_boundary() -> String {
        let seq = DIGEST_NC.fetch_add(1, Ordering::Relaxed);
        format!("----LayerCraftBoundary{seq:08x}")
    }

    fn read_resp_body(
        mut resp: Response<ureq::Body>,
    ) -> Result<(u16, Option<String>, String), PrinterError> {
        let status = resp.status().as_u16();
        let www_auth = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| PrinterError::Transport(e.to_string()))?;
        Ok((status, www_auth, body))
    }

    fn truncate_body(body: &str) -> String {
        let trimmed = body.trim();
        let max_chars = 240;
        let collected: String = trimmed.chars().take(max_chars).collect();
        if trimmed.chars().count() > max_chars {
            format!("{collected}...")
        } else {
            collected
        }
    }

    fn extract_uri_path_and_query(full_url: &str) -> String {
        let Some((_, after_scheme)) = full_url.split_once("://") else {
            return "/".to_owned();
        };
        if let Some(idx) = after_scheme.find('/') {
            after_scheme.get(idx..).unwrap_or("/").to_owned()
        } else {
            "/".to_owned()
        }
    }

    fn get_with_auth(
        agent: &Agent,
        profile: &PrinterProfile,
        url: &str,
        cached_digest: &mut Option<DigestChallenge>,
    ) -> Result<(u16, String), PrinterError> {
        let uri = extract_uri_path_and_query(url);
        let mut req = agent.get(url);
        match &profile.auth {
            PrinterAuth::None => {}
            PrinterAuth::ApiKey { api_key } => {
                if !api_key.is_empty() {
                    req = req.header("X-Api-Key", api_key.as_str());
                }
            }
            PrinterAuth::Bearer { token } => {
                if !token.is_empty() {
                    req = req.header("Authorization", &format!("Bearer {token}"));
                }
            }
            PrinterAuth::Digest { username, password } => {
                if let Some(ch) = cached_digest.as_ref() {
                    let nc = DIGEST_NC.fetch_add(1, Ordering::Relaxed);
                    let auth_hdr = build_digest_authorization(
                        "GET",
                        &uri,
                        username,
                        password,
                        ch,
                        nc,
                        &next_cnonce(),
                    )?;
                    req = req.header("Authorization", &auth_hdr);
                }
            }
        }

        let resp = req
            .call()
            .map_err(|e| PrinterError::Transport(e.to_string()))?;
        let (status, www_auth, body) = read_resp_body(resp)?;
        if status == 401
            && let PrinterAuth::Digest { username, password } = &profile.auth
            && let Some(hdr) = www_auth.as_deref()
            && let Some(ch) = parse_digest_challenge(hdr)
        {
            let nc = DIGEST_NC.fetch_add(1, Ordering::Relaxed);
            let auth_hdr = build_digest_authorization(
                "GET",
                &uri,
                username,
                password,
                &ch,
                nc,
                &next_cnonce(),
            )?;
            *cached_digest = Some(ch);
            let retry = agent
                .get(url)
                .header("Authorization", &auth_hdr)
                .call()
                .map_err(|e| PrinterError::Transport(e.to_string()))?;
            let (st2, _, body2) = read_resp_body(retry)?;
            if st2 == 401 {
                return Err(PrinterError::AuthFailed(format!(
                    "HTTP 401: {}",
                    truncate_body(&body2)
                )));
            }
            return Ok((st2, body2));
        }

        if status == 401 {
            return Err(PrinterError::AuthFailed(format!(
                "HTTP 401: {}",
                truncate_body(&body)
            )));
        }

        Ok((status, body))
    }

    struct ProgressReader<'a> {
        data: &'a [u8],
        offset: usize,
        progress: Option<&'a UploadProgress>,
        count_bytes: bool,
    }

    impl Read for ProgressReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.progress.is_some_and(UploadProgress::is_cancelled) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "printer upload cancelled",
                ));
            }
            if self.offset >= self.data.len() {
                return Ok(0);
            }
            let remaining = self.data.get(self.offset..).unwrap_or(&[]);
            let n = remaining.len().min(buf.len()).min(64 * 1024);
            if let (Some(dst), Some(src)) = (buf.get_mut(..n), remaining.get(..n)) {
                dst.copy_from_slice(src);
                self.offset += n;
                if self.count_bytes
                    && let Some(p) = self.progress
                {
                    p.add_sent_bytes(n);
                }
                Ok(n)
            } else {
                Ok(0)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn send_with_body(
        agent: &Agent,
        profile: &PrinterProfile,
        is_put: bool,
        url: &str,
        extra_headers: &[(&str, &str)],
        payload: &[u8],
        cached_digest: &mut Option<DigestChallenge>,
        progress: Option<&UploadProgress>,
        count_progress: bool,
    ) -> Result<(u16, String), PrinterError> {
        let method_str = if is_put { "PUT" } else { "POST" };
        let uri = extract_uri_path_and_query(url);
        if count_progress && let Some(p) = progress {
            p.set_total_bytes(payload.len());
        }

        for attempt in 0..2 {
            if progress.is_some_and(UploadProgress::is_cancelled) {
                return Err(PrinterError::Cancelled);
            }
            if count_progress
                && attempt > 0
                && let Some(p) = progress
            {
                p.reset_sent();
            }
            let mut req = if is_put {
                agent.put(url)
            } else {
                agent.post(url)
            };
            req = req.header("Content-Length", &payload.len().to_string());
            for &(k, v) in extra_headers {
                req = req.header(k, v);
            }
            match &profile.auth {
                PrinterAuth::None => {}
                PrinterAuth::ApiKey { api_key } => {
                    if !api_key.is_empty() {
                        req = req.header("X-Api-Key", api_key.as_str());
                    }
                }
                PrinterAuth::Bearer { token } => {
                    if !token.is_empty() {
                        req = req.header("Authorization", &format!("Bearer {token}"));
                    }
                }
                PrinterAuth::Digest { username, password } => {
                    if let Some(ch) = cached_digest.as_ref() {
                        let nc = DIGEST_NC.fetch_add(1, Ordering::Relaxed);
                        let auth_hdr = build_digest_authorization(
                            method_str,
                            &uri,
                            username,
                            password,
                            ch,
                            nc,
                            &next_cnonce(),
                        )?;
                        req = req.header("Authorization", &auth_hdr);
                    }
                }
            }

            let mut reader = ProgressReader {
                data: payload,
                offset: 0,
                progress,
                count_bytes: count_progress,
            };
            let resp = req.send(SendBody::from_reader(&mut reader)).map_err(|e| {
                if progress.is_some_and(UploadProgress::is_cancelled) {
                    PrinterError::Cancelled
                } else {
                    PrinterError::Transport(e.to_string())
                }
            })?;
            let (status, www_auth, body) = read_resp_body(resp)?;
            if status == 401
                && attempt == 0
                && let PrinterAuth::Digest { .. } = &profile.auth
                && let Some(hdr) = www_auth.as_deref()
                && let Some(ch) = parse_digest_challenge(hdr)
            {
                *cached_digest = Some(ch);
                continue;
            }
            if status == 401 || status == 403 {
                return Err(PrinterError::AuthFailed(format!(
                    "HTTP {status}: {}",
                    truncate_body(&body)
                )));
            }
            return Ok((status, body));
        }

        Err(PrinterError::AuthFailed(
            "HTTP Digest authentication did not succeed".to_owned(),
        ))
    }

    /// Query printer info, state, temperatures, and storage volumes.
    pub fn probe_printer(profile: &PrinterProfile) -> Result<PrinterStatusReport, PrinterError> {
        let base_url = normalize_base_url(&profile.base_url)?;
        let agent = build_agent(profile);
        let mut digest_cache: Option<DigestChallenge> = None;

        let mut report = PrinterStatusReport {
            printer_id: profile.id.clone(),
            printer_name: profile.name.clone(),
            protocol: profile.protocol,
            base_url: base_url.clone(),
            reachable: false,
            firmware: None,
            model: None,
            hostname: None,
            serial: None,
            state: None,
            nozzle_temp_c: None,
            target_nozzle_temp_c: None,
            bed_temp_c: None,
            target_bed_temp_c: None,
            storages: Vec::new(),
            warnings: Vec::new(),
        };

        match profile.protocol {
            PrinterProtocol::PrusaLink => {
                let (st_info, body_info) = get_with_auth(
                    &agent,
                    profile,
                    &format!("{base_url}/api/v1/info"),
                    &mut digest_cache,
                )?;
                if (200..300).contains(&st_info) {
                    report.reachable = true;
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body_info) {
                        report.firmware = val
                            .get("firmware")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned);
                        report.model = val
                            .get("type")
                            .and_then(|v| {
                                v.as_str()
                                    .map(str::to_owned)
                                    .or_else(|| v.as_i64().map(|n| n.to_string()))
                            })
                            .or_else(|| {
                                val.get("name").and_then(|v| v.as_str()).map(str::to_owned)
                            });
                        report.hostname = val
                            .get("hostname")
                            .or_else(|| val.get("name"))
                            .and_then(|v| v.as_str())
                            .map(str::to_owned);
                        report.serial = val
                            .get("serial")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned);
                    }
                } else {
                    return Err(PrinterError::HttpStatus {
                        status: st_info,
                        body: truncate_body(&body_info),
                    });
                }

                match get_with_auth(
                    &agent,
                    profile,
                    &format!("{base_url}/api/v1/status"),
                    &mut digest_cache,
                ) {
                    Ok((st, body)) if (200..300).contains(&st) => {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body)
                            && let Some(p) = val.get("printer")
                        {
                            report.state =
                                p.get("state").and_then(|v| v.as_str()).map(str::to_owned);
                            report.nozzle_temp_c = p.get("temp_nozzle").and_then(|v| v.as_f64());
                            report.target_nozzle_temp_c =
                                p.get("target_nozzle").and_then(|v| v.as_f64());
                            report.bed_temp_c = p.get("temp_bed").and_then(|v| v.as_f64());
                            report.target_bed_temp_c = p.get("target_bed").and_then(|v| v.as_f64());
                        }
                    }
                    Ok((st, _)) => report
                        .warnings
                        .push(format!("/api/v1/status returned HTTP {st}")),
                    Err(e) => report.warnings.push(format!("status probe failed: {e}")),
                }

                match get_with_auth(
                    &agent,
                    profile,
                    &format!("{base_url}/api/v1/storage"),
                    &mut digest_cache,
                ) {
                    Ok((st, body)) if (200..300).contains(&st) => {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body)
                            && let Some(arr) = val.get("storage_list").and_then(|v| v.as_array())
                        {
                            for item in arr {
                                let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
                                    continue;
                                };
                                report.storages.push(PrinterStorageInfo {
                                    name: name.to_owned(),
                                    path: item
                                        .get("path")
                                        .and_then(|v| v.as_str())
                                        .map(str::to_owned),
                                    read_only: item
                                        .get("read_only")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false),
                                    ready: item
                                        .get("ready")
                                        .or_else(|| item.get("available"))
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(true),
                                });
                            }
                        }
                    }
                    Ok((st, _)) => report
                        .warnings
                        .push(format!("/api/v1/storage returned HTTP {st}")),
                    Err(e) => report.warnings.push(format!("storage probe failed: {e}")),
                }
            }
            PrinterProtocol::PrusaConnectLocal => {
                let (st, body) = get_with_auth(
                    &agent,
                    profile,
                    &format!("{base_url}/api/v1/devices"),
                    &mut digest_cache,
                )?;
                if !(200..300).contains(&st) {
                    return Err(PrinterError::HttpStatus {
                        status: st,
                        body: truncate_body(&body),
                    });
                }
                report.reachable = true;
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
                    let first = val.as_array().and_then(|a| a.first()).unwrap_or(&val);
                    report.model = first
                        .get("printer_type")
                        .or_else(|| first.get("name"))
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    report.state = first
                        .get("state")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                }
            }
            PrinterProtocol::OctoPrint => {
                let (st, body) = get_with_auth(
                    &agent,
                    profile,
                    &format!("{base_url}/api/version"),
                    &mut digest_cache,
                )?;
                if !(200..300).contains(&st) {
                    return Err(PrinterError::HttpStatus {
                        status: st,
                        body: truncate_body(&body),
                    });
                }
                report.reachable = true;
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
                    report.firmware = val
                        .get("server")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                }
                if let Ok((st_p, body_p)) = get_with_auth(
                    &agent,
                    profile,
                    &format!("{base_url}/api/printer"),
                    &mut digest_cache,
                ) && (200..300).contains(&st_p)
                    && let Ok(val) = serde_json::from_str::<serde_json::Value>(&body_p)
                {
                    report.state = val
                        .get("state")
                        .and_then(|s| s.get("text"))
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    report.nozzle_temp_c = val
                        .get("temperature")
                        .and_then(|t| t.get("tool0"))
                        .and_then(|t| t.get("actual"))
                        .and_then(|v| v.as_f64());
                    report.bed_temp_c = val
                        .get("temperature")
                        .and_then(|t| t.get("bed"))
                        .and_then(|t| t.get("actual"))
                        .and_then(|v| v.as_f64());
                }
            }
            PrinterProtocol::Moonraker => {
                let (st, body) = get_with_auth(
                    &agent,
                    profile,
                    &format!("{base_url}/server/info"),
                    &mut digest_cache,
                )?;
                if !(200..300).contains(&st) {
                    return Err(PrinterError::HttpStatus {
                        status: st,
                        body: truncate_body(&body),
                    });
                }
                report.reachable = true;
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body)
                    && let Some(res) = val.get("result")
                {
                    report.firmware = res
                        .get("klippy_state")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                }
                if let Ok((st_p, body_p)) = get_with_auth(
                    &agent,
                    profile,
                    &format!("{base_url}/printer/info"),
                    &mut digest_cache,
                ) && (200..300).contains(&st_p)
                    && let Ok(val) = serde_json::from_str::<serde_json::Value>(&body_p)
                    && let Some(res) = val.get("result")
                {
                    report.state = res.get("state").and_then(|v| v.as_str()).map(str::to_owned);
                    report.hostname = res
                        .get("hostname")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    report.firmware = res
                        .get("software_version")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                        .or(report.firmware);
                }
            }
        }

        Ok(report)
    }

    /// Upload a `.gcode` or `.bgcode` payload directly to a printer over LAN/local HTTP(S).
    pub fn upload_to_printer(
        profile: &PrinterProfile,
        payload: &[u8],
        opts: &UploadOptions,
    ) -> Result<UploadResult, PrinterError> {
        upload_to_printer_with_progress(profile, payload, opts, None)
    }

    /// Upload a `.gcode` or `.bgcode` payload with cooperative byte-progress and cancellation.
    pub fn upload_to_printer_with_progress(
        profile: &PrinterProfile,
        payload: &[u8],
        opts: &UploadOptions,
        progress: Option<&UploadProgress>,
    ) -> Result<UploadResult, PrinterError> {
        if progress.is_some_and(UploadProgress::is_cancelled) {
            return Err(PrinterError::Cancelled);
        }
        if payload.is_empty() {
            return Err(PrinterError::InvalidConfig("upload payload is empty"));
        }
        let base_url = normalize_base_url(&profile.base_url)?;
        let (clean_rel, encoded_rel, basename) = sanitize_remote_path(&opts.remote_path)?;
        let storage_raw = profile
            .effective_storage(opts.storage.as_deref())
            .trim_matches('/');
        if storage_raw.is_empty() {
            return Err(PrinterError::InvalidConfig("storage target is empty"));
        }
        let storage = storage_raw.to_owned();
        let storage_enc = percent_encode_component(&storage);

        if profile.protocol == PrinterProtocol::PrusaConnectLocal
            && opts.start_print
            && profile
                .device_id
                .as_deref()
                .map(str::trim)
                .is_none_or(str::is_empty)
        {
            return Err(PrinterError::InvalidConfig(
                "device_id is required when start_print is enabled for Prusa Connect Local",
            ));
        }

        let agent = build_agent(profile);
        let mut digest_cache: Option<DigestChallenge> = None;

        match profile.protocol {
            PrinterProtocol::PrusaLink => {
                // Pre-fetch Digest challenge via lightweight GET so large payloads aren't sent unauthenticated.
                if matches!(profile.auth, PrinterAuth::Digest { .. })
                    && let Ok(resp) = agent.get(&format!("{base_url}/api/v1/info")).call()
                    && resp.status().as_u16() == 401
                    && let Some(hdr) = resp
                        .headers()
                        .get("www-authenticate")
                        .and_then(|v| v.to_str().ok())
                {
                    digest_cache = parse_digest_challenge(hdr);
                }

                let print_flag = if opts.start_print { "?1" } else { "?0" };
                let overwrite_flag = if opts.overwrite { "?1" } else { "?0" };
                let put_url = format!("{base_url}/api/v1/files/{storage_enc}/{encoded_rel}");
                let (status, body) = send_with_body(
                    &agent,
                    profile,
                    true,
                    &put_url,
                    &[
                        ("Content-Type", "application/octet-stream"),
                        ("Print-After-Upload", print_flag),
                        ("Overwrite", overwrite_flag),
                    ],
                    payload,
                    &mut digest_cache,
                    progress,
                    true,
                )?;
                if !(200..300).contains(&status) {
                    return Err(PrinterError::HttpStatus {
                        status,
                        body: truncate_body(&body),
                    });
                }
                Ok(UploadResult {
                    printer_id: profile.id.clone(),
                    printer_name: profile.name.clone(),
                    protocol: profile.protocol,
                    base_url,
                    storage,
                    remote_path: clean_rel,
                    bytes_uploaded: payload.len(),
                    start_print_requested: opts.start_print,
                    http_status: status,
                    message: if opts.start_print {
                        "Uploaded via PrusaLink and started print.".to_owned()
                    } else {
                        "Uploaded via PrusaLink.".to_owned()
                    },
                })
            }
            PrinterProtocol::PrusaConnectLocal => {
                let put_url =
                    format!("{base_url}/api/v1/storages/{storage_enc}/content?path={encoded_rel}");
                let (status, body) = send_with_body(
                    &agent,
                    profile,
                    true,
                    &put_url,
                    &[("Content-Type", "application/octet-stream")],
                    payload,
                    &mut digest_cache,
                    progress,
                    true,
                )?;
                if !(200..300).contains(&status) {
                    return Err(PrinterError::HttpStatus {
                        status,
                        body: truncate_body(&body),
                    });
                }
                if opts.start_print {
                    let dev_id = profile.device_id.as_deref().map(str::trim).unwrap_or("");
                    let dev_enc = percent_encode_component(dev_id);
                    let print_url = format!("{base_url}/api/v1/devices/{dev_enc}/print");
                    let print_json = serde_json::json!({
                        "storage": storage,
                        "source": clean_rel,
                        "path": format!("usb/{basename}"),
                        "replace": opts.overwrite,
                    });
                    let json_bytes = serde_json::to_vec(&print_json).map_err(|e| {
                        PrinterError::InvalidConfig("failed encoding print JSON").into_err_or(e)
                    })?;
                    let (st_p, body_p) = send_with_body(
                        &agent,
                        profile,
                        false,
                        &print_url,
                        &[("Content-Type", "application/json")],
                        &json_bytes,
                        &mut digest_cache,
                        progress,
                        false,
                    )?;
                    if !(200..300).contains(&st_p) {
                        return Err(PrinterError::HttpStatus {
                            status: st_p,
                            body: truncate_body(&body_p),
                        });
                    }
                }
                Ok(UploadResult {
                    printer_id: profile.id.clone(),
                    printer_name: profile.name.clone(),
                    protocol: profile.protocol,
                    base_url,
                    storage,
                    remote_path: clean_rel,
                    bytes_uploaded: payload.len(),
                    start_print_requested: opts.start_print,
                    http_status: status,
                    message: if opts.start_print {
                        "Uploaded to Prusa Connect Local storage and triggered device print."
                            .to_owned()
                    } else {
                        "Uploaded to Prusa Connect Local storage.".to_owned()
                    },
                })
            }
            PrinterProtocol::OctoPrint => {
                let boundary = next_boundary();
                let print_str = if opts.start_print { "true" } else { "false" };
                let subdir = clean_rel.rsplit_once('/').map(|(dir, _)| dir);
                let mut fields: Vec<(&str, &str)> = vec![("select", "true"), ("print", print_str)];
                if let Some(dir) = subdir {
                    fields.push(("path", dir));
                }
                let mp = build_multipart_upload(
                    &boundary,
                    "file",
                    &basename,
                    "application/octet-stream",
                    payload,
                    &fields,
                );
                let ct = format!("multipart/form-data; boundary={boundary}");
                let url = format!("{base_url}/api/files/{storage_enc}");
                let (status, body) = send_with_body(
                    &agent,
                    profile,
                    false,
                    &url,
                    &[("Content-Type", &ct)],
                    &mp,
                    &mut digest_cache,
                    progress,
                    true,
                )?;
                if !(200..300).contains(&status) {
                    return Err(PrinterError::HttpStatus {
                        status,
                        body: truncate_body(&body),
                    });
                }
                Ok(UploadResult {
                    printer_id: profile.id.clone(),
                    printer_name: profile.name.clone(),
                    protocol: profile.protocol,
                    base_url,
                    storage,
                    remote_path: clean_rel,
                    bytes_uploaded: payload.len(),
                    start_print_requested: opts.start_print,
                    http_status: status,
                    message: if opts.start_print {
                        "Uploaded via OctoPrint and started print.".to_owned()
                    } else {
                        "Uploaded via OctoPrint.".to_owned()
                    },
                })
            }
            PrinterProtocol::Moonraker => {
                let boundary = next_boundary();
                let print_str = if opts.start_print { "true" } else { "false" };
                let subdir = clean_rel.rsplit_once('/').map(|(dir, _)| dir);
                let mut fields: Vec<(&str, &str)> =
                    vec![("root", storage.as_str()), ("print", print_str)];
                if let Some(dir) = subdir {
                    fields.push(("path", dir));
                }
                let mp = build_multipart_upload(
                    &boundary,
                    "file",
                    &basename,
                    "application/octet-stream",
                    payload,
                    &fields,
                );
                let ct = format!("multipart/form-data; boundary={boundary}");
                let url = format!("{base_url}/server/files/upload");
                let (status, body) = send_with_body(
                    &agent,
                    profile,
                    false,
                    &url,
                    &[("Content-Type", &ct)],
                    &mp,
                    &mut digest_cache,
                    progress,
                    true,
                )?;
                if !(200..300).contains(&status) {
                    return Err(PrinterError::HttpStatus {
                        status,
                        body: truncate_body(&body),
                    });
                }
                Ok(UploadResult {
                    printer_id: profile.id.clone(),
                    printer_name: profile.name.clone(),
                    protocol: profile.protocol,
                    base_url,
                    storage,
                    remote_path: clean_rel,
                    bytes_uploaded: payload.len(),
                    start_print_requested: opts.start_print,
                    http_status: status,
                    message: if opts.start_print {
                        "Uploaded via Moonraker and started print.".to_owned()
                    } else {
                        "Uploaded via Moonraker.".to_owned()
                    },
                })
            }
        }
    }

    trait IntoErrHelper {
        fn into_err_or(self, err: serde_json::Error) -> PrinterError;
    }
    impl IntoErrHelper for PrinterError {
        fn into_err_or(self, _: serde_json::Error) -> PrinterError {
            self
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::{
    default_printers_config_path, load_printer_profiles, printer_credential_storage_mode,
    probe_printer, save_printer_profiles, upload_to_printer, upload_to_printer_with_progress,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_and_path_sanitization_work_and_block_hostile_inputs() {
        assert_eq!(
            normalize_base_url(" 192.168.1.42/ ").unwrap(),
            "http://192.168.1.42"
        );
        assert_eq!(
            normalize_base_url("HTTPS://coreone.local:8443/").unwrap(),
            "https://coreone.local:8443"
        );
        assert!(normalize_base_url("").is_err());
        assert!(normalize_base_url("ftp://printer.local").is_err());
        assert!(normalize_base_url("http://bad host").is_err());

        let (clean, enc, base) = sanitize_remote_path("/folder name/my gear#1.bgcode").unwrap();
        assert_eq!(clean, "folder name/my gear#1.bgcode");
        assert_eq!(enc, "folder%20name/my%20gear%231.bgcode");
        assert_eq!(base, "my gear#1.bgcode");

        assert!(sanitize_remote_path("").is_err());
        assert!(sanitize_remote_path("../secret.gcode").is_err());
        assert!(sanitize_remote_path("a/../b.gcode").is_err());
        assert!(sanitize_remote_path("bad\nfile.gcode").is_err());
    }

    #[test]
    fn rfc2617_md5_and_sha256_digest_challenges_match_exact_vectors() {
        // RFC 2617 Section 3.5 exact vector
        let hdr = r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#;
        let ch = parse_digest_challenge(hdr).unwrap();
        assert_eq!(ch.realm, "testrealm@host.com");
        assert_eq!(ch.nonce, "dcd98b7102dd2f0e8b11d0f600bfb0c093");

        let auth = build_digest_authorization(
            "GET",
            "/dir/index.html",
            "Mufasa",
            "Circle Of Life",
            &ch,
            1,
            "0a4f113b",
        )
        .unwrap();
        assert!(
            auth.contains(r#"response="6629fae49393a05397450978507c4ef1""#),
            "{auth}"
        );
        assert!(auth.contains("nc=00000001"));
        assert!(auth.contains("qop=auth"));

        // SHA-256 Digest test
        let sha_ch = parse_digest_challenge(
            r#"Digest realm="PrusaLink", algorithm=SHA-256, qop="auth", nonce="abc123""#,
        )
        .unwrap();
        let sha_auth = build_digest_authorization(
            "PUT",
            "/api/v1/files/usb/part.bgcode",
            "maker",
            "secret",
            &sha_ch,
            2,
            "cafebabe",
        )
        .unwrap();
        assert!(sha_auth.contains("algorithm=SHA-256"));
        assert!(sha_auth.contains("nc=00000002"));
    }

    #[test]
    fn summary_redacts_all_secret_credentials() {
        let p = PrinterProfile {
            id: "c1".to_owned(),
            name: "CORE One".to_owned(),
            protocol: PrinterProtocol::PrusaLink,
            base_url: "http://192.168.1.50".to_owned(),
            auth: PrinterAuth::Digest {
                username: "maker".to_owned(),
                password: "super-secret-password".to_owned(),
            },
            ..PrinterProfile::default()
        };
        let s = p.summary();
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("super-secret-password"));
        assert!(s.has_credentials);
        assert_eq!(s.username.as_deref(), Some("maker"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    mod native_tests {
        use super::*;
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};
        use std::thread;

        #[derive(Debug, Default)]
        struct RecordedHttpRequest {
            method: String,
            path: String,
            headers: Vec<(String, String)>,
            body: Vec<u8>,
        }

        fn read_http_request(stream: &mut std::net::TcpStream) -> Option<RecordedHttpRequest> {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let mut header_end = None;
            while header_end.is_none() {
                let n = stream.read(&mut tmp).ok()?;
                if n == 0 {
                    return None;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = Some(pos + 4);
                }
            }
            let h_end = header_end?;
            let head_str = std::str::from_utf8(&buf[..h_end]).ok()?;
            let mut lines = head_str.split("\r\n");
            let req_line = lines.next()?;
            let mut parts = req_line.split_whitespace();
            let method = parts.next()?.to_owned();
            let path = parts.next()?.to_owned();
            let mut headers = Vec::new();
            let mut content_len = 0usize;
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    let kl = k.trim().to_ascii_lowercase();
                    let vt = v.trim().to_owned();
                    if kl == "content-length" {
                        content_len = vt.parse().unwrap_or(0);
                    }
                    headers.push((kl, vt));
                }
            }
            while buf.len() < h_end + content_len {
                let n = stream.read(&mut tmp).ok()?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let body = buf[h_end..h_end + content_len].to_vec();
            Some(RecordedHttpRequest {
                method,
                path,
                headers,
                body,
            })
        }

        fn spawn_mock_server<F>(
            expected_requests: usize,
            handler: F,
        ) -> (
            String,
            Arc<Mutex<Vec<RecordedHttpRequest>>>,
            thread::JoinHandle<()>,
        )
        where
            F: Fn(usize, &RecordedHttpRequest) -> (u16, Vec<(String, String)>, String)
                + Send
                + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let recorded = Arc::new(Mutex::new(Vec::new()));
            let rec_clone = Arc::clone(&recorded);
            let handle = thread::spawn(move || {
                for idx in 0..expected_requests {
                    let (mut stream, _) = listener.accept().unwrap();
                    let req = read_http_request(&mut stream).unwrap();
                    let (status, extra_hdrs, resp_body) = handler(idx, &req);
                    rec_clone.lock().unwrap().push(req);
                    let reason = match status {
                        200 => "OK",
                        201 => "Created",
                        401 => "Unauthorized",
                        409 => "Conflict",
                        _ => "Error",
                    };
                    let mut raw = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n",
                        resp_body.len()
                    );
                    for (k, v) in extra_hdrs {
                        let _ = write!(raw, "{k}: {v}\r\n");
                    }
                    raw.push_str("\r\n");
                    raw.push_str(&resp_body);
                    stream.write_all(raw.as_bytes()).unwrap();
                    let _ = stream.flush();
                }
            });
            (format!("http://{addr}"), recorded, handle)
        }

        #[test]
        fn prusalink_digest_probe_and_upload_work_over_loopback() {
            let (url, recorded, handle) = spawn_mock_server(4, |_idx, req| {
                let auth = req
                    .headers
                    .iter()
                    .find(|(k, _)| k == "authorization")
                    .map(|(_, v)| v.as_str());
                if !auth.is_some_and(|a| a.starts_with("Digest ") && a.contains("response=")) {
                    return (
                        401,
                        vec![(
                            "WWW-Authenticate".to_owned(),
                            r#"Digest realm="PrusaLink", qop="auth", nonce="n123456""#.to_owned(),
                        )],
                        "{}".to_owned(),
                    );
                }
                if req.path == "/api/v1/info" {
                    (
                        200,
                        vec![],
                        r#"{"firmware":"6.2.0","type":"COREONE","hostname":"coreone-lab","serial":"SN123"}"#.to_owned(),
                    )
                } else if req.path == "/api/v1/status" {
                    (
                        200,
                        vec![],
                        r#"{"printer":{"state":"IDLE","temp_nozzle":24.5,"target_nozzle":0,"temp_bed":23.0,"target_bed":0}}"#.to_owned(),
                    )
                } else if req.path == "/api/v1/storage" {
                    (
                        200,
                        vec![],
                        r#"{"storage_list":[{"name":"usb","path":"/usb","read_only":false,"ready":true}]}"#.to_owned(),
                    )
                } else if req.method == "PUT" {
                    (201, vec![], r#"{"ok":true}"#.to_owned())
                } else {
                    (404, vec![], "not found".to_owned())
                }
            });

            let profile = PrinterProfile {
                id: "core1".to_owned(),
                name: "CORE One Lab".to_owned(),
                protocol: PrinterProtocol::PrusaLink,
                base_url: url,
                auth: PrinterAuth::Digest {
                    username: "maker".to_owned(),
                    password: "pw".to_owned(),
                },
                ..PrinterProfile::default()
            };

            let status = probe_printer(&profile).unwrap();
            assert!(status.reachable);
            assert_eq!(status.firmware.as_deref(), Some("6.2.0"));
            assert_eq!(status.state.as_deref(), Some("IDLE"));
            assert_eq!(status.storages.len(), 1);
            assert_eq!(status.storages[0].name, "usb");
            handle.join().unwrap();
            let reqs = recorded.lock().unwrap();
            assert_eq!(reqs.len(), 4);

            // Now test direct PUT upload (1 preflight GET /api/v1/info + 1 PUT /api/v1/files/usb/...)
            let (url2, rec2, h2) = spawn_mock_server(2, |idx, req| {
                if idx == 0 {
                    (
                        401,
                        vec![(
                            "WWW-Authenticate".to_owned(),
                            r#"Digest realm="PrusaLink", qop="auth", nonce="putnonce""#.to_owned(),
                        )],
                        "{}".to_owned(),
                    )
                } else {
                    assert_eq!(req.method, "PUT");
                    assert_eq!(req.path, "/api/v1/files/usb/gears.bgcode");
                    (201, vec![], r#"{"ok":true}"#.to_owned())
                }
            });
            let p2 = PrinterProfile {
                base_url: url2,
                ..profile
            };
            let payload = b"GCDE\x01\x00\x00\x00\x01\x00test-payload";
            let res = upload_to_printer(
                &p2,
                payload,
                &UploadOptions {
                    remote_path: "gears.bgcode".to_owned(),
                    storage: None,
                    start_print: true,
                    overwrite: true,
                },
            )
            .unwrap();
            assert_eq!(res.bytes_uploaded, payload.len());
            assert_eq!(res.http_status, 201);
            h2.join().unwrap();
            let reqs2 = rec2.lock().unwrap();
            let put_req = &reqs2[1];
            assert_eq!(put_req.body, payload);
            assert!(
                put_req
                    .headers
                    .iter()
                    .any(|(k, v)| k == "print-after-upload" && v == "?1")
            );
            assert!(
                put_req
                    .headers
                    .iter()
                    .any(|(k, v)| k == "overwrite" && v == "?1")
            );
        }

        #[test]
        fn connect_local_octoprint_and_moonraker_uploads_and_errors_work() {
            // 1. Prusa Connect Local upload + print trigger
            let (url_cl, rec_cl, h_cl) = spawn_mock_server(2, |idx, req| {
                assert!(
                    req.headers
                        .iter()
                        .any(|(k, v)| k == "authorization" && v == "Bearer tok123")
                );
                if idx == 0 {
                    assert_eq!(req.method, "PUT");
                    assert_eq!(req.path, "/api/v1/storages/local/content?path=part.bgcode");
                    (200, vec![], "{}".to_owned())
                } else {
                    assert_eq!(req.method, "POST");
                    assert_eq!(req.path, "/api/v1/devices/dev-42/print");
                    (200, vec![], "{}".to_owned())
                }
            });
            let prof_cl = PrinterProfile {
                protocol: PrinterProtocol::PrusaConnectLocal,
                base_url: url_cl,
                auth: PrinterAuth::Bearer {
                    token: "tok123".to_owned(),
                },
                device_id: Some("dev-42".to_owned()),
                ..PrinterProfile::default()
            };
            let res_cl = upload_to_printer(
                &prof_cl,
                b"GCDEdata",
                &UploadOptions {
                    remote_path: "part.bgcode".to_owned(),
                    storage: None,
                    start_print: true,
                    overwrite: true,
                },
            )
            .unwrap();
            assert!(res_cl.start_print_requested);
            h_cl.join().unwrap();
            assert_eq!(rec_cl.lock().unwrap().len(), 2);

            // 2. OctoPrint multipart upload
            let (url_op, rec_op, h_op) = spawn_mock_server(1, |_idx, req| {
                assert_eq!(req.method, "POST");
                assert_eq!(req.path, "/api/files/local");
                assert!(
                    req.headers
                        .iter()
                        .any(|(k, v)| k == "x-api-key" && v == "opkey")
                );
                (201, vec![], r#"{"done":true}"#.to_owned())
            });
            let prof_op = PrinterProfile {
                protocol: PrinterProtocol::OctoPrint,
                base_url: url_op,
                auth: PrinterAuth::ApiKey {
                    api_key: "opkey".to_owned(),
                },
                ..PrinterProfile::default()
            };
            upload_to_printer(
                &prof_op,
                b"G28\n",
                &UploadOptions {
                    remote_path: "test.gcode".to_owned(),
                    storage: None,
                    start_print: false,
                    overwrite: true,
                },
            )
            .unwrap();
            h_op.join().unwrap();
            let op_body = String::from_utf8_lossy(&rec_op.lock().unwrap()[0].body).into_owned();
            assert!(op_body.contains("filename=\"test.gcode\""));
            assert!(op_body.contains("G28\n"));

            // 3. Moonraker 409 error propagation
            let (url_mr, _rec_mr, h_mr) =
                spawn_mock_server(1, |_idx, _req| (409, vec![], "file locked".to_owned()));
            let prof_mr = PrinterProfile {
                protocol: PrinterProtocol::Moonraker,
                base_url: url_mr,
                ..PrinterProfile::default()
            };
            let err = upload_to_printer(&prof_mr, b"G28\n", &UploadOptions::default()).unwrap_err();
            h_mr.join().unwrap();
            assert!(matches!(err, PrinterError::HttpStatus { status: 409, .. }));
        }

        #[test]
        fn printer_store_roundtrips_and_sets_restrictive_permissions() {
            let dir = std::env::temp_dir()
                .join(format!("layercraft_printer_store_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let path = dir.join("printers.json");

            let empty = load_printer_profiles(&path).unwrap();
            assert!(empty.is_empty());

            let prof = PrinterProfile {
                id: "core1".to_owned(),
                name: "CORE One".to_owned(),
                base_url: "http://192.168.1.10".to_owned(),
                auth: PrinterAuth::ApiKey {
                    api_key: "secret".to_owned(),
                },
                ..PrinterProfile::default()
            };
            save_printer_profiles(&path, std::slice::from_ref(&prof)).unwrap();
            let loaded = load_printer_profiles(&path).unwrap();
            assert_eq!(loaded, vec![prof]);

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600);
            }
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn upload_progress_tracks_bytes_and_honors_cancellation() {
            let payload = vec![b'G'; 140 * 1024];
            let (url, rec, h) = spawn_mock_server(1, |_idx, req| {
                assert_eq!(req.method, "PUT");
                (201, vec![], String::new())
            });
            let prof = PrinterProfile {
                protocol: PrinterProtocol::PrusaLink,
                base_url: url,
                ..PrinterProfile::default()
            };
            let progress = UploadProgress::new();
            let res = upload_to_printer_with_progress(
                &prof,
                &payload,
                &UploadOptions {
                    remote_path: "big.bgcode".to_owned(),
                    storage: None,
                    start_print: false,
                    overwrite: true,
                },
                Some(&progress),
            )
            .unwrap();
            assert_eq!(res.bytes_uploaded, payload.len());
            assert_eq!(progress.snapshot(), (payload.len(), payload.len()));
            h.join().unwrap();
            assert_eq!(rec.lock().unwrap()[0].body.len(), payload.len());

            // Pre-cancelled upload returns Cancelled before touching transport.
            let cancelled = UploadProgress::new();
            cancelled.cancel();
            let err = upload_to_printer_with_progress(
                &prof,
                &payload,
                &UploadOptions::default(),
                Some(&cancelled),
            )
            .unwrap_err();
            assert_eq!(err, PrinterError::Cancelled);
        }

        #[test]
        fn keychain_backend_redacts_secrets_on_disk_and_hydrates_on_load() {
            use std::collections::HashMap;
            use std::sync::Mutex;

            #[derive(Default)]
            struct MockKeychain(Mutex<HashMap<String, String>>);
            impl native::KeychainBackend for MockKeychain {
                fn store(&self, account: &str, secret: &str) -> Result<(), String> {
                    self.0
                        .lock()
                        .unwrap()
                        .insert(account.to_owned(), secret.to_owned());
                    Ok(())
                }
                fn load(&self, account: &str) -> Result<Option<String>, String> {
                    Ok(self.0.lock().unwrap().get(account).cloned())
                }
            }

            let dir = std::env::temp_dir()
                .join(format!("layercraft_keychain_test_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let path = dir.join("printers.json");
            let backend = MockKeychain::default();

            let profiles = vec![
                PrinterProfile {
                    id: "p1".to_owned(),
                    name: "CoreOne".to_owned(),
                    base_url: "http://192.168.1.50".to_owned(),
                    auth: PrinterAuth::ApiKey {
                        api_key: "super-secret-api-key".to_owned(),
                    },
                    ..PrinterProfile::default()
                },
                PrinterProfile {
                    id: "p2".to_owned(),
                    name: "CoreOneDigest".to_owned(),
                    base_url: "http://192.168.1.51".to_owned(),
                    auth: PrinterAuth::Digest {
                        username: "maker".to_owned(),
                        password: "digest-password".to_owned(),
                    },
                    ..PrinterProfile::default()
                },
            ];

            native::save_printer_profiles_with_backend(&path, &profiles, &backend).unwrap();
            let on_disk = std::fs::read_to_string(&path).unwrap();
            assert!(!on_disk.contains("super-secret-api-key"));
            assert!(!on_disk.contains("digest-password"));
            assert!(on_disk.contains("keychain://layercraft/p1:api_key"));
            assert!(on_disk.contains("keychain://layercraft/p2:digest_password"));

            let hydrated = native::load_printer_profiles_with_backend(&path, &backend).unwrap();
            assert_eq!(hydrated, profiles);

            // Idempotent re-save of already-redacted sentinel values preserves secrets.
            let raw_disk_profiles: Vec<PrinterProfile> = serde_json::from_str(&on_disk).unwrap();
            native::save_printer_profiles_with_backend(&path, &raw_disk_profiles, &backend)
                .unwrap();
            let hydrated_again =
                native::load_printer_profiles_with_backend(&path, &backend).unwrap();
            assert_eq!(hydrated_again, profiles);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
