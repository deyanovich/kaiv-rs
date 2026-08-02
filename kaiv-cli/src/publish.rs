//! `kaiv publish` — write artifacts to the kaiv registries
//! through the synchronous gate (spec REGISTRY-GATE.md). One
//! verb: a publish is an authenticated `deposit` that either
//! lands validated or returns the reference validator's own
//! denial, verbatim. Artifacts are write-once eternalinks; there
//! is no delete and no overwrite (an identical-bytes republish
//! is an idempotent success, different bytes under a taken name
//! are a 409).
//!
//! Addressing is `{namespace}/{name}.{ext}`. Library artifacts
//! (`.taiv`/`.saiv`/`.csaiv`/`.faiv`) carry their own identity
//! in the format declaration (`.!taiv acme/net`), which the
//! gate enforces — so the publish address is derived from the
//! file, never guessed from its path. Data artifacts
//! (`.kaiv`/`.raiv`) publish by basename under `--namespace`;
//! `.daiv` is content-addressed (the name is the BLAKE3 hex of
//! the bytes). `.maiv` addresses are derived from their
//! endpoints and direction marker, so they take an explicit
//! `--as`.
//!
//! The staging `{t,s,f,d}.kaiv.io` hosts are the default gate
//! bases (the same single-switch-point rationale as the Layer 4
//! read hosts in kaiv's `resolve.rs`); the production
//! `k*aiv.com` domains are refused unless `--production` is
//! passed.

use crate::account;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const USAGE: &str = "\
kaiv publish — publish artifacts to the kaiv registries

USAGE:
    kaiv publish <file> [OPTIONS]
    kaiv publish <files-or-dirs...> --batch [OPTIONS]

The registry is selected by extension:
    .taiv               ktaiv-class  (type libraries;  t.kaiv.io)
    .saiv .csaiv .maiv  ksaiv-class  (schemas;         s.kaiv.io)
    .faiv               kfaiv-class  (unit libraries;  f.kaiv.io)
    .kaiv .raiv .daiv   kdaiv-class  (documents;       d.kaiv.io)

The published address is {namespace}/{name}.{ext}:
  - .taiv/.saiv/.csaiv/.faiv derive namespace and name from the
    file's own library identity (a file declaring
    `.!taiv acme/net` publishes acme/net.taiv); --namespace
    must agree when given.
  - .kaiv/.raiv publish by file basename under --namespace
    (required).
  - .daiv is content-addressed: the name is the BLAKE3 hex of
    the bytes (--namespace required).
  - .maiv addresses are derived from endpoints and direction
    (SPEC.md § Registry Addressing); pass the address with --as,
    e.g. --as acme/server-config/mapto/hub/server-endpoint/v1

OPTIONS:
    --namespace <ns>  target namespace (required for
                      .kaiv/.raiv/.daiv; must agree with the
                      library identity otherwise)
    --registry <t|s|f|d|auto>
                      confirm the target registry (default auto =
                      by extension; a mismatch is an error, never
                      a redirect)
    --as <ns/path>    publish a single file under an explicit
                      address (no extension; required for .maiv)
    --batch           accept several files and directories
                      (directories are searched for registry
                      artifacts): one SRCN pack per cella, sent in
                      registry order t, f, s, d; denials from
                      not-yet-published intra-batch dependencies
                      are retried in fixed-point rounds
    --token <token>   bearer token (else KAIV_TOKEN, else the
                      stored `kaiv login` session)
    --gate <url>      gate base URL for every registry (else
                      KAIV_GATE_URL, else the staging hosts
                      above)
    --dry-run         resolve everything and print the plan;
                      nothing is sent
    --production      allow publishing to the production
                      k*aiv.com registries (refused otherwise)

The gate validates every deposit with the reference validator
and its denial is surfaced verbatim. Published artifacts are
write-once: an identical-bytes republish succeeds idempotently,
different bytes under a taken name are refused (409).";

const MAX_BATCH_RECORDS: usize = 8192;
const MAX_BATCH_BYTES: u64 = 64 * 1024 * 1024;
/// Client-side fixed-point bound. The gate already retries up to
/// 32 rounds *within* a batch; client rounds only reorder across
/// cellae, so real dependency depth here is tiny.
const MAX_ROUNDS: usize = 8;

// ── The four registries ─────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Registry {
    Taiv,
    Faiv,
    Saiv,
    Daiv,
}

impl Registry {
    /// Cella-id prefix (`t.acme`, `s.acme`, …).
    fn prefix(self) -> &'static str {
        match self {
            Registry::Taiv => "t",
            Registry::Saiv => "s",
            Registry::Faiv => "f",
            Registry::Daiv => "d",
        }
    }

    fn from_flag(s: &str) -> Option<Registry> {
        Some(match s {
            "t" => Registry::Taiv,
            "s" => Registry::Saiv,
            "f" => Registry::Faiv,
            "d" => Registry::Daiv,
            _ => return None,
        })
    }

    fn for_ext(ext: &str) -> Option<Registry> {
        Some(match ext {
            "taiv" => Registry::Taiv,
            "saiv" | "csaiv" | "maiv" => Registry::Saiv,
            "faiv" => Registry::Faiv,
            "kaiv" | "raiv" | "daiv" => Registry::Daiv,
            _ => return None,
        })
    }

    fn exts(self) -> &'static str {
        match self {
            Registry::Taiv => ".taiv",
            Registry::Saiv => ".saiv/.csaiv/.maiv",
            Registry::Faiv => ".faiv",
            Registry::Daiv => ".kaiv/.raiv/.daiv",
        }
    }

    /// Default gate base per registry: the staging kaiv.io hosts,
    /// like the Layer 4 read defaults in kaiv's resolve.rs (and
    /// with the same single switch point when the k*aiv.com
    /// zones go live).
    fn default_base(self) -> &'static str {
        match self {
            Registry::Taiv => "https://t.kaiv.io",
            Registry::Saiv => "https://s.kaiv.io",
            Registry::Faiv => "https://f.kaiv.io",
            Registry::Daiv => "https://d.kaiv.io",
        }
    }

    /// Batch send order (REGISTRY-GATE.md §3a): t, f, s, d
    /// resolves every cross-registry dependency by construction.
    fn order() -> [Registry; 4] {
        [
            Registry::Taiv,
            Registry::Faiv,
            Registry::Saiv,
            Registry::Daiv,
        ]
    }
}

// ── Argument parsing ────────────────────────────────────────────

#[derive(Default)]
struct Args {
    paths: Vec<String>,
    namespace: Option<String>,
    registry: Option<Registry>,
    as_path: Option<String>,
    batch: bool,
    dry_run: bool,
    production: bool,
    token: Option<String>,
    gate: Option<String>,
}

fn parse_args(rest: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = rest.iter();
    while let Some(t) = it.next() {
        match t.as_str() {
            "--namespace" | "-n" => {
                a.namespace = Some(it.next().ok_or("--namespace needs a value")?.clone())
            }
            "--registry" => {
                let v = it.next().ok_or("--registry needs t|s|f|d|auto")?;
                if v != "auto" {
                    a.registry = Some(
                        Registry::from_flag(v)
                            .ok_or_else(|| format!("--registry must be t|s|f|d|auto, got {v}"))?,
                    );
                }
            }
            "--as" => a.as_path = Some(it.next().ok_or("--as needs a <ns/path>")?.clone()),
            "--batch" => a.batch = true,
            "--dry-run" => a.dry_run = true,
            "--production" => a.production = true,
            "--token" => a.token = Some(it.next().ok_or("--token needs a value")?.clone()),
            "--gate" => a.gate = Some(it.next().ok_or("--gate needs a URL")?.clone()),
            f if f.starts_with('-') => return Err(format!("unknown option: {f}")),
            p => a.paths.push(p.to_string()),
        }
    }
    if a.paths.is_empty() {
        return Err("publish needs a file (see `kaiv publish --help`)".into());
    }
    if !a.batch {
        if a.paths.len() > 1 {
            return Err("several inputs need --batch".into());
        }
        if Path::new(&a.paths[0]).is_dir() {
            return Err("a directory input needs --batch".into());
        }
    }
    if a.batch && a.as_path.is_some() {
        return Err("--as addresses a single file; it cannot apply to a batch".into());
    }
    Ok(a)
}

// ── Environment (read once, injected for testability) ───────────

#[derive(Default)]
struct EnvOpts {
    token: Option<String>,
    gate: Option<String>,
    offline: bool,
}

impl EnvOpts {
    fn from_env() -> EnvOpts {
        let non_empty = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        EnvOpts {
            token: non_empty("KAIV_TOKEN"),
            gate: non_empty("KAIV_GATE_URL"),
            offline: std::env::var_os("KAIV_OFFLINE").is_some_and(|v| !v.is_empty() && v != "0"),
        }
    }
}

// ── Entry point ─────────────────────────────────────────────────

pub fn cmd(rest: &[String]) -> Result<String, String> {
    if rest.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(format!("{USAGE}\n"));
    }
    run(&parse_args(rest)?, &EnvOpts::from_env(), account::load)
}

/// The whole verb, with the environment and the credential store
/// injected so tests never touch process globals.
fn run(
    args: &Args,
    env: &EnvOpts,
    stored: impl Fn() -> Result<Option<account::Credentials>, String>,
) -> Result<String, String> {
    let artifacts = plan(args)?;
    let auth = resolve_auth(args, env, stored);
    let gate_override = args.gate.as_deref().or(env.gate.as_deref());
    // A dry run sends nothing, so it must not require credentials
    // — inspecting the plan is exactly what someone does BEFORE
    // signing in. Report the auth that would be used, or why there
    // is none, and let the plan print either way.
    if args.dry_run {
        let described = match &auth {
            Ok(a) => a.describe(),
            Err(e) => format!("none ({e})"),
        };
        return Ok(render_plan(
            &artifacts,
            gate_override,
            &described,
            args.production,
        ));
    }
    let auth = auth?;
    if env.offline {
        return Err(
            "publish writes to the network; unset KAIV_OFFLINE / drop --offline \
             (or use --dry-run to inspect the plan)"
                .into(),
        );
    }
    guard_production(&artifacts, gate_override, args.production)?;
    let token = auth.token()?;
    if args.batch {
        publish_batch(&artifacts, gate_override, &token)
    } else {
        publish_single(&artifacts[0], gate_override, &token)
    }
}

// ── Planning: file -> (registry, namespace, name, bytes) ────────

struct Artifact {
    source: PathBuf,
    registry: Registry,
    namespace: String,
    /// Deposit name within the cella, extension included
    /// (`net.taiv`, `util/net.taiv`, `<b3hex>.daiv`).
    name: String,
    bytes: Vec<u8>,
}

impl Artifact {
    fn cella(&self) -> String {
        format!("{}.{}", self.registry.prefix(), self.namespace)
    }
}

fn plan(args: &Args) -> Result<Vec<Artifact>, String> {
    let mut files: Vec<PathBuf> = Vec::new();
    for p in &args.paths {
        let path = PathBuf::from(p);
        if path.is_dir() {
            collect_dir(&path, &mut files)?;
        } else {
            files.push(path);
        }
    }
    if files.is_empty() {
        return Err("no registry artifacts found in the given paths".into());
    }
    files.iter().map(|f| plan_one(f, args)).collect()
}

/// Registry artifacts under `dir`, recursively, in sorted order.
/// `kaiv.kaiv` files are Layer 2 build configuration, never a
/// publishable artifact — skipped.
fn collect_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_dir(&path, out)?;
        } else if path.file_name().is_some_and(|n| n == "kaiv.kaiv") {
            continue;
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| Registry::for_ext(&e.to_ascii_lowercase()).is_some())
        {
            out.push(path);
        }
    }
    Ok(())
}

fn plan_one(path: &Path, args: &Args) -> Result<Artifact, String> {
    let display = path.display();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .ok_or_else(|| format!("{display}: no extension — cannot select a registry"))?;
    let registry = Registry::for_ext(&ext)
        .ok_or_else(|| format!("{display}: .{ext} is not a registry artifact extension"))?;
    if let Some(forced) = args.registry {
        if forced != registry {
            return Err(format!(
                "{display}: --registry {} cannot host .{ext} artifacts \
                 (the {} registry serves {}; .{ext} belongs to {})",
                forced.prefix(),
                forced.prefix(),
                forced.exts(),
                registry.prefix(),
            ));
        }
    }
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {display}: {e}"))?;

    let (namespace, name) = if let Some(addr) = &args.as_path {
        let (ns, stem) = addr
            .split_once('/')
            .ok_or_else(|| format!("--as must be namespace-qualified (<ns>/<path>), got {addr}"))?;
        if ext == "daiv" {
            return Err(format!(
                "{display}: .daiv names are content-addressed (the BLAKE3 of the \
                 bytes); --as cannot rename one — use --namespace"
            ));
        }
        (ns.to_string(), format!("{stem}.{ext}"))
    } else {
        match ext.as_str() {
            "taiv" | "saiv" | "csaiv" | "faiv" => {
                let text = String::from_utf8_lossy(&bytes);
                let lib = header_identity(&text, &ext).map_err(|e| format!("{display}: {e}"))?;
                let (ns, stem) = lib.split_once('/').ok_or_else(|| {
                    format!(
                        "{display}: library identity {lib} is not namespace-qualified \
                         (registry artifacts publish as <namespace>/<name>)"
                    )
                })?;
                (ns.to_string(), format!("{stem}.{ext}"))
            }
            "maiv" => {
                return Err(format!(
                    "{display}: .maiv publish addresses are derived from the \
                     mapping's endpoints and direction (SPEC.md § Registry \
                     Addressing); pass the address explicitly, e.g. --as \
                     acme/server-config/mapto/hub/server-endpoint/v1"
                ))
            }
            "daiv" => {
                let ns = args
                    .namespace
                    .clone()
                    .ok_or_else(|| format!("{display}: .daiv artifacts need --namespace"))?;
                (ns, format!("{}.daiv", blake3::hash(&bytes).to_hex()))
            }
            // .kaiv / .raiv: name-addressed data documents.
            _ => {
                let ns = args
                    .namespace
                    .clone()
                    .ok_or_else(|| format!("{display}: .{ext} artifacts need --namespace"))?;
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .ok_or_else(|| format!("{display}: unusable file name"))?;
                (ns, format!("{stem}.{ext}"))
            }
        }
    };

    if let Some(flag_ns) = &args.namespace {
        if *flag_ns != namespace {
            return Err(format!(
                "{display}: --namespace {flag_ns} conflicts with the artifact's \
                 own identity ({namespace}/…) — the gate would refuse the \
                 mismatch"
            ));
        }
    }
    check_namespace(&namespace).map_err(|e| format!("{display}: {e}"))?;
    check_name(&name).map_err(|e| format!("{display}: {e}"))?;
    Ok(Artifact {
        source: path.to_path_buf(),
        registry,
        namespace,
        name,
        bytes,
    })
}

/// The library identity from the format declaration
/// (`.!taiv acme/net`), mirroring the gate's own header check.
///
/// The version slot is legacy: it no longer figures in a header,
/// and its absence means version 1. It is still accepted where it
/// appears, told apart by shape — an identity is alpha-first by
/// grammar, so only a digit-first first token is a version
/// (SPEC.md § Format Declaration, the rule `lexer::skip_decl_version`
/// applies library-side).
fn header_identity(text: &str, kind: &str) -> Result<String, String> {
    let prefix = format!(".!{kind}");
    for line in text.lines() {
        let s = line.trim_start_matches([' ', '\t']);
        if let Some(rest) = s.strip_prefix(&prefix) {
            if !rest.starts_with([' ', '\t']) {
                continue;
            }
            let mut toks = rest.split_ascii_whitespace().peekable();
            if toks
                .peek()
                .is_some_and(|t| t.starts_with(|c: char| c.is_ascii_digit()))
            {
                toks.next();
            }
            return toks
                .next()
                .map(str::to_string)
                .ok_or_else(|| format!("{prefix} declaration lacks a library identity"));
        }
    }
    Err(format!(
        "missing {prefix} declaration — the publish address derives from the \
         file's own library identity"
    ))
}

/// Namespace grammar `^[a-z0-9][a-z0-9-]{0,63}$` (the gate's
/// contract).
/// Namespace grammar: a letter, then lowercase alphanumeric runs
/// joined by single hyphens — no leading, trailing, or doubled
/// hyphen, and no leading digit.
///
/// This is deliberately the same shape authentes enforces on
/// handles, since a namespace is normally a handle: the two
/// diverging is how a claimable identity ends up unable to publish.
/// Letter-first also keeps a namespace inside the spec's
/// `lib-seg0`, so an artifact's own identity stays a valid library
/// path.
fn check_namespace(ns: &str) -> Result<(), String> {
    let ok = ns.len() <= 64
        && !ns.is_empty()
        && ns.starts_with(|c: char| c.is_ascii_lowercase())
        && ns.split('-').all(|run| {
            !run.is_empty()
                && run
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "malformed namespace {ns} (a letter, then lowercase letters and \
             digits, with single hyphens between runs)"
        ))
    }
}

/// Deposit-name grammar: `/`-separated segments of
/// `[A-Za-z0-9._-]+`, no `.`/`..` segments.
fn check_name(name: &str) -> Result<(), String> {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    for seg in stem.split('/') {
        let ok = !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !ok {
            return Err(format!("malformed artifact name: {name}"));
        }
    }
    Ok(())
}

// ── Auth ────────────────────────────────────────────────────────

enum Auth {
    /// A token given directly (--token / KAIV_TOKEN).
    Token { token: String, source: &'static str },
    /// The stored `kaiv login` session; an access token is
    /// minted from the rotating refresh token at send time.
    Stored(account::Credentials),
}

impl Auth {
    fn describe(&self) -> String {
        match self {
            Auth::Token { source, .. } => format!("bearer token from {source}"),
            Auth::Stored(c) => format!("stored session for {} ({})", c.email, c.issuer),
        }
    }

    fn token(&self) -> Result<String, String> {
        match self {
            Auth::Token { token, .. } => Ok(token.clone()),
            Auth::Stored(credentials) => {
                let mut credentials = account::Credentials {
                    issuer: credentials.issuer.clone(),
                    email: credentials.email.clone(),
                    refresh_token: credentials.refresh_token.clone(),
                };
                account::access_token(&mut credentials)
            }
        }
    }
}

fn resolve_auth(
    args: &Args,
    env: &EnvOpts,
    stored: impl Fn() -> Result<Option<account::Credentials>, String>,
) -> Result<Auth, String> {
    if let Some(t) = &args.token {
        return Ok(Auth::Token {
            token: t.clone(),
            source: "--token",
        });
    }
    if let Some(t) = &env.token {
        return Ok(Auth::Token {
            token: t.clone(),
            source: "KAIV_TOKEN",
        });
    }
    match stored()? {
        Some(credentials) => Ok(Auth::Stored(credentials)),
        None => Err("not signed in — run `kaiv login`, pass --token, or set KAIV_TOKEN".into()),
    }
}

// ── Production guard ────────────────────────────────────────────

/// The four production registry domains (REGISTRY-GATE.md).
const PRODUCTION_DOMAINS: [&str; 4] = ["ktaiv.com", "ksaiv.com", "kfaiv.com", "kdaiv.com"];

fn host_of(url: &str) -> &str {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    host.rsplit_once(':')
        .filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit()))
        .map(|(h, _)| h)
        .unwrap_or(host)
}

fn is_production(url: &str) -> bool {
    let host = host_of(url);
    PRODUCTION_DOMAINS
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

fn base_for(registry: Registry, gate_override: Option<&str>) -> String {
    gate_override
        .map(|g| g.trim_end_matches('/').to_string())
        .unwrap_or_else(|| registry.default_base().to_string())
}

fn guard_production(
    artifacts: &[Artifact],
    gate_override: Option<&str>,
    allowed: bool,
) -> Result<(), String> {
    if allowed {
        return Ok(());
    }
    for a in artifacts {
        let base = base_for(a.registry, gate_override);
        if is_production(&base) {
            return Err(format!(
                "{base} is a production registry; pass --production to publish \
                 there (production deposits are write-once and irreversible)"
            ));
        }
    }
    Ok(())
}

// ── Dry run ─────────────────────────────────────────────────────

fn render_plan(
    artifacts: &[Artifact],
    gate_override: Option<&str>,
    auth: &str,
    production: bool,
) -> String {
    let mut out = String::from("publish plan (dry run — nothing sent):\n");
    for a in artifacts {
        let base = base_for(a.registry, gate_override);
        let guard = if is_production(&base) && !production {
            "  [refused without --production]"
        } else {
            ""
        };
        out.push_str(&format!(
            "  {}/{}  <-  {}  ({} B)\n      POST {base}/v1/c/{}/deposit/{}{guard}\n",
            a.namespace,
            a.name,
            a.source.display(),
            a.bytes.len(),
            a.cella(),
            a.name,
        ));
    }
    out.push_str(&format!("auth: {auth}\n"));
    out
}

// ── HTTP ────────────────────────────────────────────────────────

const USER_AGENT: &str = concat!("kaiv/", env!("CARGO_PKG_VERSION"));

/// POST raw bytes with a bearer token; non-2xx statuses are data
/// (the gate's denial bodies are the point).
fn post_bytes(url: &str, token: &str, bytes: &[u8]) -> Result<(u16, String), String> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    let mut response = agent
        .post(url)
        .header("User-Agent", USER_AGENT)
        .header("Authorization", &format!("Bearer {token}"))
        .header("Content-Type", "application/octet-stream")
        .send(bytes)
        .map_err(|e| format!("cannot reach {url}: {e}"))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("read response from {url}: {e}"))?;
    Ok((status, body))
}

/// A refusal, with the gate's response body verbatim — the gate
/// runs the reference validator and its error names/messages are
/// the authoritative diagnosis.
fn gate_refusal(what: &str, status: u16, body: &str) -> String {
    let hint = account::json_str_field(body, "message")
        .map(|m| format!(": {m}"))
        .unwrap_or_default();
    format!("{what} refused (HTTP {status}){hint}\ngate response: {body}")
}

// ── Single publish ──────────────────────────────────────────────

fn publish_single(
    a: &Artifact,
    gate_override: Option<&str>,
    token: &str,
) -> Result<String, String> {
    let base = base_for(a.registry, gate_override);
    let url = format!("{base}/v1/c/{}/deposit/{}", a.cella(), a.name);
    let (status, body) = post_bytes(&url, token, &a.bytes)?;
    if status != 200 {
        return Err(gate_refusal(
            &format!("publish {}/{}", a.namespace, a.name),
            status,
            &body,
        ));
    }
    let sha = account::json_str_field(&body, "sha256")
        .map(|h| format!(" (sha256 {h})"))
        .unwrap_or_default();
    Ok(format!("published {}/{}{sha}\n", a.namespace, a.name))
}

// ── Batch publish (SRCN packs, fixed-point rounds) ──────────────

/// Encode records as a SRCN v1 pack (pyloros SARCINA.md §2):
/// magic `SRCN`, version 1, flags 0, u32be count, then per
/// record `D` + u16be(name len) + name + u32be(body len) + body.
fn srcn_encode(records: &[&Artifact]) -> Result<Vec<u8>, String> {
    if records.len() > MAX_BATCH_RECORDS {
        return Err(format!("at most {MAX_BATCH_RECORDS} records per batch"));
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"SRCN");
    out.push(1);
    out.push(0);
    out.extend_from_slice(&(records.len() as u32).to_be_bytes());
    for r in records {
        if r.name.len() > u16::MAX as usize {
            return Err(format!("artifact name too long: {}", r.name));
        }
        out.push(b'D');
        out.extend_from_slice(&(r.name.len() as u16).to_be_bytes());
        out.extend_from_slice(r.name.as_bytes());
        out.extend_from_slice(&(r.bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(&r.bytes);
    }
    if out.len() as u64 > MAX_BATCH_BYTES {
        return Err(format!(
            "batch exceeds the {MAX_BATCH_BYTES}-byte pack cap; split it"
        ));
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq)]
enum Outcome {
    Ok,
    /// Retriable in a later round (the gate's `denied`): a
    /// dependency published by a *later* cella batch may unblock
    /// it.
    Denied {
        hook: String,
        message: String,
    },
    /// Terminal: the name exists with different bytes.
    Collision,
}

/// Publish every artifact, one SRCN pack per cella, cellae in
/// registry order t, f, s, d (namespaces sorted within each) —
/// the order REGISTRY-GATE.md §3a pins, which resolves
/// cross-registry dependencies by construction. The gate itself
/// runs fixed-point rounds *within* each pack (e2e scenario 16:
/// a pack may carry its own dependency closure in any order);
/// the client mirrors the same protocol *across* packs: records
/// denied in a round are resubmitted while any round still
/// accepted something, until fixed point.
fn publish_batch(
    artifacts: &[Artifact],
    gate_override: Option<&str>,
    token: &str,
) -> Result<String, String> {
    let mut outcomes: Vec<Option<Outcome>> = vec![None; artifacts.len()];
    let mut pending: Vec<usize> = (0..artifacts.len()).collect();
    let mut rounds = 0usize;

    while !pending.is_empty() && rounds < MAX_ROUNDS {
        rounds += 1;
        // Group the pending records by cella, in send order.
        let mut cellae: BTreeMap<(usize, String), Vec<usize>> = BTreeMap::new();
        for &i in &pending {
            let a = &artifacts[i];
            let reg_rank = Registry::order()
                .iter()
                .position(|r| *r == a.registry)
                .expect("registry in order");
            cellae
                .entry((reg_rank, a.namespace.clone()))
                .or_default()
                .push(i);
        }

        let mut accepted_this_round = 0usize;
        let mut next_pending: Vec<usize> = Vec::new();
        for ((_, _), indices) in cellae {
            let a0 = &artifacts[indices[0]];
            let base = base_for(a0.registry, gate_override);
            let url = format!("{base}/v1/c/{}/deposita", a0.cella());
            let records: Vec<&Artifact> = indices.iter().map(|&i| &artifacts[i]).collect();
            let pack = srcn_encode(&records)?;
            let (status, body) = post_bytes(&url, token, &pack)?;
            if status != 200 {
                // Not a per-record verdict but a cella/transport
                // level refusal (auth, quota, malformed pack):
                // abort the whole publish with the body verbatim.
                return Err(gate_refusal(
                    &format!("batch publish to {}", a0.cella()),
                    status,
                    &body,
                ));
            }
            let failed = parse_failures(&body)
                .map_err(|e| format!("unparseable gate response from {url}: {e}\n{body}"))?;
            for &i in &indices {
                match failed.get(&artifacts[i].name) {
                    None => {
                        outcomes[i] = Some(Outcome::Ok);
                        accepted_this_round += 1;
                    }
                    Some(o @ Outcome::Denied { .. }) => {
                        outcomes[i] = Some(o.clone());
                        next_pending.push(i);
                    }
                    Some(o) => outcomes[i] = Some(o.clone()),
                }
            }
        }
        // Fixed point: stop when nothing is left to retry, or a
        // whole round accepted nothing (no later round can).
        if next_pending.is_empty() || accepted_this_round == 0 {
            break;
        }
        pending = next_pending;
    }

    // Per-file report, in input order.
    let mut report = String::new();
    let mut failures = 0usize;
    for (a, o) in artifacts.iter().zip(&outcomes) {
        match o {
            Some(Outcome::Ok) => {
                report.push_str(&format!("  ok         {}/{}\n", a.namespace, a.name))
            }
            Some(Outcome::Denied { hook, message }) => {
                failures += 1;
                report.push_str(&format!(
                    "  denied     {}/{} — {message} (hook: {hook})\n",
                    a.namespace, a.name
                ));
            }
            Some(Outcome::Collision) => {
                failures += 1;
                report.push_str(&format!(
                    "  collision  {}/{} — name present with different digest\n",
                    a.namespace, a.name
                ));
            }
            None => {
                failures += 1;
                report.push_str(&format!(
                    "  skipped    {}/{} — not submitted (round limit)\n",
                    a.namespace, a.name
                ));
            }
        }
    }
    let summary = format!(
        "published {}/{} artifact(s) in {rounds} round(s)\n{report}",
        artifacts.len() - failures,
        artifacts.len(),
    );
    if failures > 0 {
        Err(summary)
    } else {
        Ok(summary)
    }
}

/// The failed records of a deposita response
/// (`{"ok": N, "failures": [{name, outcome, hook?, message?}]}`),
/// keyed by record name. A name absent from the map was
/// accepted.
fn parse_failures(body: &str) -> Result<BTreeMap<String, Outcome>, String> {
    let value = json::parse(body)?;
    value
        .get("ok")
        .and_then(json::Value::as_u64)
        .ok_or("response lacks an \"ok\" count")?;
    let failures = value
        .get("failures")
        .and_then(json::Value::as_array)
        .ok_or("response lacks a \"failures\" array")?;
    let mut map = BTreeMap::new();
    for f in failures {
        let name = f
            .get("name")
            .and_then(json::Value::as_str)
            .ok_or("failure entry lacks a name")?
            .to_string();
        let outcome = match f.get("outcome").and_then(json::Value::as_str) {
            Some("denied") => Outcome::Denied {
                hook: f
                    .get("hook")
                    .and_then(json::Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
                message: f
                    .get("message")
                    .and_then(json::Value::as_str)
                    .unwrap_or("denied")
                    .to_string(),
            },
            Some("collision") => Outcome::Collision,
            other => {
                return Err(format!(
                    "failure entry for {name} has unknown outcome {other:?}"
                ))
            }
        };
        map.insert(name, outcome);
    }
    Ok(map)
}

// ── Minimal JSON (the deposita response carries arrays, which
//    account.rs's flat field scan cannot address) ────────────────

mod json {
    #[derive(Debug, PartialEq)]
    pub enum Value {
        Null,
        Bool(bool),
        Num(f64),
        Str(String),
        Arr(Vec<Value>),
        Obj(Vec<(String, Value)>),
    }

    impl Value {
        pub fn get(&self, key: &str) -> Option<&Value> {
            match self {
                Value::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }
        pub fn as_str(&self) -> Option<&str> {
            match self {
                Value::Str(s) => Some(s),
                _ => None,
            }
        }
        pub fn as_u64(&self) -> Option<u64> {
            match self {
                Value::Num(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as u64),
                _ => None,
            }
        }
        pub fn as_array(&self) -> Option<&[Value]> {
            match self {
                Value::Arr(items) => Some(items),
                _ => None,
            }
        }
    }

    pub fn parse(text: &str) -> Result<Value, String> {
        let bytes = text.as_bytes();
        let mut pos = 0;
        let value = parse_value(bytes, &mut pos)?;
        skip_ws(bytes, &mut pos);
        if pos != bytes.len() {
            return Err("trailing content after JSON value".into());
        }
        Ok(value)
    }

    fn skip_ws(b: &[u8], pos: &mut usize) {
        while *pos < b.len() && matches!(b[*pos], b' ' | b'\t' | b'\n' | b'\r') {
            *pos += 1;
        }
    }

    fn parse_value(b: &[u8], pos: &mut usize) -> Result<Value, String> {
        skip_ws(b, pos);
        match b.get(*pos) {
            Some(b'{') => parse_obj(b, pos),
            Some(b'[') => parse_arr(b, pos),
            Some(b'"') => Ok(Value::Str(parse_str(b, pos)?)),
            Some(b't') => lit(b, pos, "true", Value::Bool(true)),
            Some(b'f') => lit(b, pos, "false", Value::Bool(false)),
            Some(b'n') => lit(b, pos, "null", Value::Null),
            Some(_) => parse_num(b, pos),
            None => Err("unexpected end of JSON".into()),
        }
    }

    fn lit(b: &[u8], pos: &mut usize, word: &str, v: Value) -> Result<Value, String> {
        if b[*pos..].starts_with(word.as_bytes()) {
            *pos += word.len();
            Ok(v)
        } else {
            Err(format!("bad literal at byte {pos}"))
        }
    }

    fn parse_num(b: &[u8], pos: &mut usize) -> Result<Value, String> {
        let start = *pos;
        while *pos < b.len() && matches!(b[*pos], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
            *pos += 1;
        }
        std::str::from_utf8(&b[start..*pos])
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Value::Num)
            .ok_or_else(|| format!("bad number at byte {start}"))
    }

    fn parse_str(b: &[u8], pos: &mut usize) -> Result<String, String> {
        *pos += 1; // opening quote
        let mut out = String::new();
        loop {
            match b.get(*pos) {
                None => return Err("unterminated string".into()),
                Some(b'"') => {
                    *pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    *pos += 1;
                    match b.get(*pos) {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'b') => out.push('\u{0008}'),
                        Some(b'f') => out.push('\u{000C}'),
                        Some(b'n') => out.push('\n'),
                        Some(b'r') => out.push('\r'),
                        Some(b't') => out.push('\t'),
                        Some(b'u') => {
                            let hex = b
                                .get(*pos + 1..*pos + 5)
                                .and_then(|h| std::str::from_utf8(h).ok())
                                .ok_or("truncated \\u escape")?;
                            let code =
                                u32::from_str_radix(hex, 16).map_err(|_| "bad \\u escape")?;
                            // Denial messages are plain text; a
                            // surrogate pair simply fails.
                            out.push(char::from_u32(code).ok_or("surrogate in \\u escape")?);
                            *pos += 4;
                        }
                        _ => return Err("bad escape".into()),
                    }
                    *pos += 1;
                }
                Some(_) => {
                    // Advance one UTF-8 scalar.
                    let s =
                        std::str::from_utf8(&b[*pos..]).map_err(|_| "invalid UTF-8 in string")?;
                    let c = s.chars().next().unwrap();
                    out.push(c);
                    *pos += c.len_utf8();
                }
            }
        }
    }

    fn parse_arr(b: &[u8], pos: &mut usize) -> Result<Value, String> {
        *pos += 1; // [
        let mut items = Vec::new();
        skip_ws(b, pos);
        if b.get(*pos) == Some(&b']') {
            *pos += 1;
            return Ok(Value::Arr(items));
        }
        loop {
            items.push(parse_value(b, pos)?);
            skip_ws(b, pos);
            match b.get(*pos) {
                Some(b',') => *pos += 1,
                Some(b']') => {
                    *pos += 1;
                    return Ok(Value::Arr(items));
                }
                _ => return Err("expected , or ] in array".into()),
            }
        }
    }

    fn parse_obj(b: &[u8], pos: &mut usize) -> Result<Value, String> {
        *pos += 1; // {
        let mut fields = Vec::new();
        skip_ws(b, pos);
        if b.get(*pos) == Some(&b'}') {
            *pos += 1;
            return Ok(Value::Obj(fields));
        }
        loop {
            skip_ws(b, pos);
            if b.get(*pos) != Some(&b'"') {
                return Err("expected object key".into());
            }
            let key = parse_str(b, pos)?;
            skip_ws(b, pos);
            if b.get(*pos) != Some(&b':') {
                return Err("expected : after object key".into());
            }
            *pos += 1;
            fields.push((key, parse_value(b, pos)?));
            skip_ws(b, pos);
            match b.get(*pos) {
                Some(b',') => *pos += 1,
                Some(b'}') => {
                    *pos += 1;
                    return Ok(Value::Obj(fields));
                }
                _ => return Err("expected , or } in object".into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// The version slot no longer figures in a header — its absence
    /// means version 1 — but headers that still carry one must keep
    /// resolving. The two are told apart by shape, since an identity
    /// is alpha-first by grammar. Reading the version slot
    /// unconditionally made `.!taiv acme/net`, the current form and
    /// the one the gate's own e2e scenarios post, fail to publish.
    #[test]
    fn header_identity_reads_both_declaration_forms() {
        for kind in ["taiv", "saiv", "faiv"] {
            let bare = format!(".!{kind} acme/net\n");
            let versioned = format!(".!{kind} 1 acme/net\n");
            assert_eq!(
                header_identity(&bare, kind).unwrap(),
                "acme/net",
                "{bare:?}"
            );
            assert_eq!(
                header_identity(&versioned, kind).unwrap(),
                "acme/net",
                "{versioned:?}"
            );
            // Multi-component legacy versions too.
            let v3 = format!(".!{kind} 1.0.0 acme/net\n");
            assert_eq!(header_identity(&v3, kind).unwrap(), "acme/net");
        }
        // Trailing modifiers stay out of the identity.
        assert_eq!(
            header_identity(".!saiv acme/config strict\n", "saiv").unwrap(),
            "acme/config"
        );
        // A declaration with no identity at all is still an error.
        assert!(header_identity(".!taiv\n", "taiv").is_err());
        assert!(header_identity(".!taiv 1\n", "taiv").is_err());
        // A missing declaration is a different error.
        assert!(header_identity("!int'::x=1\n", "taiv").is_err());
    }

    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    // ── A scripted one-shot HTTP server (the net.rs test
    //    pattern, extended to capture requests and answer each
    //    path from a consumable script) ──────────────────────────

    struct Captured {
        method: String,
        path: String,
        auth: Option<String>,
        body: Vec<u8>,
    }

    /// Serve `script` — `(path, status, body)` entries, each
    /// answering one request to its path — capturing every
    /// request. The thread exits once the script is consumed.
    fn serve(script: Vec<(&str, u16, &str)>) -> (String, Arc<Mutex<Vec<Captured>>>) {
        let script: Vec<(String, u16, String)> = script
            .into_iter()
            .map(|(p, s, b)| (p.to_string(), s, b.to_string()))
            .collect();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_in = captured.clone();
        std::thread::spawn(move || {
            let mut remaining = script;
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                // Read the head, then the Content-Length body.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(i + 4);
                    }
                };
                let Some(head_end) = head_end else { continue };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.lines();
                let request = lines.next().unwrap_or("");
                let method = request.split(' ').next().unwrap_or("").to_string();
                let path = request.split(' ').nth(1).unwrap_or("/").to_string();
                let mut auth = None;
                let mut content_length = 0usize;
                for l in lines {
                    let lower = l.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("authorization:") {
                        auth = Some(l.split_once(':').unwrap().1.trim().to_string());
                    }
                }
                let mut body = buf[head_end..].to_vec();
                while body.len() < content_length {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                captured_in.lock().unwrap().push(Captured {
                    method,
                    path: path.clone(),
                    auth,
                    body,
                });
                let (status, response_body) =
                    match remaining.iter().position(|(p, _, _)| *p == path) {
                        Some(i) => {
                            let (_, status, body) = remaining.remove(i);
                            (status, body)
                        }
                        None => (404, String::new()),
                    };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                if remaining.is_empty() {
                    break;
                }
            }
        });
        (base, captured)
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("kaiv-publish-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_file(dir: &Path, name: &str, content: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p.display().to_string()
    }

    /// Run the verb with a bare environment and no credential
    /// store — every test authenticates via --token, so no
    /// process globals are read or written.
    fn rt(args: &[&str]) -> Result<String, String> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        run(&parse_args(&args)?, &EnvOpts::default(), || Ok(None))
    }

    // ── Single publish ──────────────────────────────────────────

    #[test]
    fn single_publish_happy_path() {
        let dir = tmp_dir("single");
        let file = write_file(
            &dir,
            "net.taiv",
            ".!taiv acme/net\n\n{tcp,udp}\n&proto=tcp\n",
        );
        let (base, captured) = serve(vec![(
            "/v1/c/t.acme/deposit/net.taiv",
            200,
            r#"{"outcome":"ok","sha256":"cafe"}"#,
        )]);
        let out = rt(&["--token", "tok", "--gate", &base, &file]).unwrap();
        assert_eq!(out, "published acme/net.taiv (sha256 cafe)\n");
        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].auth.as_deref(), Some("Bearer tok"));
        assert_eq!(reqs[0].body, std::fs::read(dir.join("net.taiv")).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn denial_surfaces_the_gate_body_verbatim() {
        let dir = tmp_dir("denied");
        let file = write_file(&dir, "bad.taiv", ".!taiv acme/bad\n???garbage\n");
        let denial = r#"{"error":"denied","message":"line 2: SigilError: unrecognized line form","hook":"kaiv"}"#;
        let (base, _captured) = serve(vec![("/v1/c/t.acme/deposit/bad.taiv", 403, denial)]);
        let err = rt(&["--token", "tok", "--gate", &base, &file]).unwrap_err();
        // The validator's own error name, the status, and the raw
        // body all surface.
        assert!(err.contains("SigilError"), "{err}");
        assert!(err.contains("HTTP 403"), "{err}");
        assert!(err.contains(denial), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identity_derivation_and_conflicts() {
        let dir = tmp_dir("identity");
        let file = write_file(
            &dir,
            "whatever.saiv",
            ".!saiv acme/server-config strict\nhost=\n",
        );
        // Identity comes from the declaration, not the filename;
        // --namespace must agree when given.
        let plan = rt(&["--token", "t", "--dry-run", &file]).unwrap();
        assert!(plan.contains("acme/server-config.saiv"), "{plan}");
        assert!(
            plan.contains("/v1/c/s.acme/deposit/server-config.saiv"),
            "{plan}"
        );
        let err = rt(&["--token", "t", "--namespace", "other", &file]).unwrap_err();
        assert!(err.contains("conflicts"), "{err}");
        // A missing declaration is an error, not a guess.
        let bare = write_file(&dir, "bare.taiv", "{a,b}\n&x=\n");
        let err = rt(&["--token", "t", &bare]).unwrap_err();
        assert!(err.contains("missing .!taiv"), "{err}");
        // Data documents need --namespace; .daiv is content-addressed.
        let kaiv = write_file(&dir, "cfg.kaiv", "::a=x\n");
        let err = rt(&["--token", "t", &kaiv]).unwrap_err();
        assert!(err.contains("--namespace"), "{err}");
        let daiv = write_file(&dir, "doc.daiv", ".!daiv\n!str'::a=x\n");
        let plan = rt(&["--token", "t", "--namespace", "acme", "--dry-run", &daiv]).unwrap();
        let hash = blake3::hash(std::fs::read(dir.join("doc.daiv")).unwrap().as_slice())
            .to_hex()
            .to_string();
        assert!(plan.contains(&format!("acme/{hash}.daiv")), "{plan}");
        // .maiv addresses cannot be derived — --as is required and
        // honored.
        let maiv = write_file(
            &dir,
            "edge.maiv",
            ".!maiv 1\n.!source acme/a\n.!target hub/b\n",
        );
        let err = rt(&["--token", "t", &maiv]).unwrap_err();
        assert!(err.contains("--as"), "{err}");
        let plan = rt(&[
            "--token",
            "t",
            "--dry-run",
            "--as",
            "acme/a/mapto/hub/b/v1",
            &maiv,
        ])
        .unwrap();
        assert!(
            plan.contains("/v1/c/s.acme/deposit/a/mapto/hub/b/v1.maiv"),
            "{plan}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_mismatch_is_an_error_not_a_redirect() {
        let dir = tmp_dir("mismatch");
        let file = write_file(&dir, "net.taiv", ".!taiv acme/net\n");
        let err = rt(&["--token", "t", "--registry", "s", &file]).unwrap_err();
        assert!(err.contains("cannot host .taiv"), "{err}");
        let ok = rt(&["--token", "t", "--registry", "t", "--dry-run", &file]).unwrap();
        assert!(ok.contains("acme/net.taiv"), "{ok}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn not_signed_in_is_a_clear_error() {
        let dir = tmp_dir("noauth");
        let file = write_file(&dir, "net.taiv", ".!taiv acme/net\n");
        let args: Vec<String> = vec![file];
        let err = run(&parse_args(&args).unwrap(), &EnvOpts::default(), || {
            Ok(None)
        })
        .unwrap_err();
        assert!(err.contains("kaiv login"), "{err}");
        assert!(err.contains("KAIV_TOKEN"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Batch publish ───────────────────────────────────────────

    #[test]
    fn batch_reaches_fixed_point_over_out_of_order_dependencies() {
        let dir = tmp_dir("batch");
        // aaa/dep imports bbb/base; the aaa cella is submitted
        // first (namespace order), so round one denies it and the
        // retry lands after bbb/base materialized.
        let f_dep = write_file(&dir, "dep.taiv", ".!taiv aaa/dep\n.!types bbb/base\n");
        let f_base = write_file(&dir, "base.taiv", ".!taiv bbb/base\n");
        let (base, captured) = serve(vec![
            (
                "/v1/c/t.aaa/deposita",
                200,
                r#"{"ok":0,"failures":[{"name":"dep.taiv","outcome":"denied","hook":"kaiv","message":"publish dependencies first: bbb/base.taiv"}],"seq":1}"#,
            ),
            (
                "/v1/c/t.bbb/deposita",
                200,
                r#"{"ok":1,"failures":[],"seq":1}"#,
            ),
            (
                "/v1/c/t.aaa/deposita",
                200,
                r#"{"ok":1,"failures":[],"seq":2}"#,
            ),
        ]);
        let out = rt(&[
            "--token", "tok", "--gate", &base, "--batch", &f_dep, &f_base,
        ])
        .unwrap();
        assert!(
            out.contains("published 2/2 artifact(s) in 2 round(s)"),
            "{out}"
        );
        assert!(out.contains("ok         aaa/dep.taiv"), "{out}");
        assert!(out.contains("ok         bbb/base.taiv"), "{out}");
        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[0].path, "/v1/c/t.aaa/deposita");
        assert_eq!(reqs[1].path, "/v1/c/t.bbb/deposita");
        assert_eq!(reqs[2].path, "/v1/c/t.aaa/deposita");
        // The retry pack carries only the denied record.
        let retry = &reqs[2].body;
        assert_eq!(&retry[..4], b"SRCN");
        assert_eq!(u32::from_be_bytes(retry[6..10].try_into().unwrap()), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn batch_stalls_report_the_denial_and_fail() {
        let dir = tmp_dir("stall");
        let f = write_file(&dir, "dep.taiv", ".!taiv aaa/dep\n.!types nowhere/base\n");
        // Nothing else lands, so the round makes no progress and
        // the denial is final after one submission.
        let (base, captured) = serve(vec![(
            "/v1/c/t.aaa/deposita",
            200,
            r#"{"ok":0,"failures":[{"name":"dep.taiv","outcome":"denied","hook":"kaiv","message":"publish dependencies first: nowhere/base.taiv"}]}"#,
        )]);
        let err = rt(&["--token", "tok", "--gate", &base, "--batch", &f]).unwrap_err();
        assert!(err.contains("published 0/1"), "{err}");
        assert!(err.contains("publish dependencies first"), "{err}");
        assert!(err.contains("hook: kaiv"), "{err}");
        assert_eq!(captured.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn batch_collects_directories_and_splits_cellae_by_registry() {
        let dir = tmp_dir("collect");
        std::fs::create_dir_all(dir.join("types")).unwrap();
        write_file(&dir.join("types"), "net.taiv", ".!taiv acme/net\n");
        write_file(&dir, "config.saiv", ".!saiv acme/config\nhost=\n");
        // Layer 2 build configuration is never a publishable
        // artifact.
        write_file(&dir, "kaiv.kaiv", "/registries::default=./types\n");
        let plan = rt(&[
            "--token",
            "t",
            "--batch",
            "--dry-run",
            dir.to_str().unwrap(),
        ])
        .unwrap();
        assert!(plan.contains("/v1/c/t.acme/deposit/net.taiv"), "{plan}");
        assert!(plan.contains("/v1/c/s.acme/deposit/config.saiv"), "{plan}");
        assert!(!plan.contains("kaiv.kaiv"), "{plan}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Safety ──────────────────────────────────────────────────

    /// A dry run sends nothing, so it must not demand credentials
    /// — inspecting the plan is what someone does before signing
    /// in, and requiring a token there contradicted the documented
    /// contract.
    #[test]
    fn dry_run_needs_no_credentials() {
        let dir = tmp_dir("dryrun-noauth");
        let file = write_file(&dir, "net.taiv", ".!taiv acme/net\n");
        // No --token, no KAIV_TOKEN, no stored session.
        let out = rt(&["--dry-run", &file]).expect("dry run without auth");
        assert!(out.contains("acme/net.taiv"), "{out}");
        assert!(out.contains("nothing sent"), "{out}");
        assert!(out.contains("auth: none"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_sends_nothing() {
        let dir = tmp_dir("dry");
        let file = write_file(&dir, "net.taiv", ".!taiv acme/net\n");
        // An unroutable gate: any attempted request would fail
        // loudly, so a clean plan proves zero network writes.
        let out = rt(&[
            "--token",
            "t",
            "--gate",
            "http://127.0.0.1:1",
            "--dry-run",
            &file,
        ])
        .unwrap();
        assert!(out.contains("nothing sent"), "{out}");
        assert!(
            out.contains("POST http://127.0.0.1:1/v1/c/t.acme/deposit/net.taiv"),
            "{out}"
        );
        assert!(out.contains("auth: bearer token from --token"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn production_hosts_require_the_flag() {
        let dir = tmp_dir("prod");
        let file = write_file(&dir, "net.taiv", ".!taiv acme/net\n");
        let err = rt(&["--token", "t", "--gate", "https://ktaiv.com", &file]).unwrap_err();
        assert!(err.contains("--production"), "{err}");
        // Subdomains of the production zones are guarded too; the
        // staging kaiv.io hosts are not.
        assert!(is_production("https://ktaiv.com"));
        assert!(is_production("https://api.kdaiv.com/x"));
        assert!(is_production("http://ksaiv.com:8080"));
        assert!(!is_production("https://t.kaiv.io"));
        assert!(!is_production("http://localhost:8792"));
        assert!(!is_production("https://notktaiv.com"));
        // The dry-run plan flags the guard instead of refusing.
        let plan = rt(&[
            "--token",
            "t",
            "--gate",
            "https://ktaiv.com",
            "--dry-run",
            &file,
        ])
        .unwrap();
        assert!(plan.contains("[refused without --production]"), "{plan}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offline_mode_refuses_to_publish() {
        let dir = tmp_dir("offline");
        let file = write_file(&dir, "net.taiv", ".!taiv acme/net\n");
        let args: Vec<String> = vec!["--token".into(), "t".into(), file];
        let env = EnvOpts {
            offline: true,
            ..EnvOpts::default()
        };
        let err = run(&parse_args(&args).unwrap(), &env, || Ok(None)).unwrap_err();
        assert!(err.contains("KAIV_OFFLINE"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Wire format and parsing ─────────────────────────────────

    #[test]
    fn srcn_encoding_matches_the_pack_format() {
        let a = Artifact {
            source: PathBuf::from("x"),
            registry: Registry::Saiv,
            namespace: "acme".into(),
            name: "base.saiv".into(),
            bytes: b"hi".to_vec(),
        };
        let pack = srcn_encode(&[&a]).unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(b"SRCN");
        expected.extend_from_slice(&[1, 0, 0, 0, 0, 1]); // ver, flags, count
        expected.push(b'D');
        expected.extend_from_slice(&9u16.to_be_bytes());
        expected.extend_from_slice(b"base.saiv");
        expected.extend_from_slice(&2u32.to_be_bytes());
        expected.extend_from_slice(b"hi");
        assert_eq!(pack, expected);
    }

    #[test]
    fn deposita_responses_parse() {
        let map = parse_failures(
            r#"{"ok":2,"failures":[
                {"name":"a.taiv","outcome":"denied","hook":"kaiv","message":"no \"x\""},
                {"name":"b.taiv","outcome":"collision","message":"name present with different digest"}
            ],"seq":7}"#,
        )
        .unwrap();
        assert_eq!(
            map.get("a.taiv"),
            Some(&Outcome::Denied {
                hook: "kaiv".into(),
                message: "no \"x\"".into()
            })
        );
        assert_eq!(map.get("b.taiv"), Some(&Outcome::Collision));
        assert_eq!(map.get("c.taiv"), None);
        assert!(parse_failures(r#"{"failures":[]}"#).is_err());
        assert!(parse_failures("not json").is_err());
    }

    #[test]
    fn json_values_parse() {
        let v = json::parse(r#"{"a":[1,{"b":"cé"}],"d":true,"e":null}"#).unwrap();
        let arr = v.get("a").and_then(json::Value::as_array).unwrap();
        assert_eq!(arr[0].as_u64(), Some(1));
        assert_eq!(arr[1].get("b").and_then(json::Value::as_str), Some("cé"));
        assert_eq!(v.get("d"), Some(&json::Value::Bool(true)));
        assert_eq!(v.get("e"), Some(&json::Value::Null));
        assert!(json::parse("{").is_err());
        assert!(json::parse("[1,]").is_err());
        assert!(json::parse("{} extra").is_err());
    }

    #[test]
    fn names_and_namespaces_are_checked_client_side() {
        assert!(check_namespace("acme").is_ok());
        assert!(check_namespace("a-1").is_ok());
        assert!(check_namespace("test-ci").is_ok());
        assert!(check_namespace("a").is_ok());
        assert!(check_namespace("Acme").is_err());
        assert!(check_namespace("").is_err());
        // The tightened rules, each of which the looser grammar
        // used to admit. A namespace becomes a path segment and is
        // normally a handle, so it must satisfy both authentes's
        // handle shape and the spec's alpha-first `lib-seg0`.
        assert!(check_namespace("9foo").is_err(), "leading digit");
        assert!(check_namespace("-x").is_err(), "leading hyphen");
        assert!(check_namespace("x-").is_err(), "trailing hyphen");
        assert!(check_namespace("a--b").is_err(), "doubled hyphen");
        assert!(check_namespace("a_b").is_err(), "underscore");
        assert!(check_namespace(&"a".repeat(65)).is_err(), "too long");
        assert!(check_name("net.taiv").is_ok());
        assert!(check_name("util/net.taiv").is_ok());
        assert!(check_name("../evil.taiv").is_err());
        assert!(check_name("a b.taiv").is_err());
    }

    // ── Manual-only staging round trip ──────────────────────────

    /// Publishes a tiny content-addressed `.daiv` to the staging
    /// gate. Manual-only (network + credentials): run with
    ///
    ///   KAIV_TOKEN=... KAIV_PUBLISH_E2E_NS=<your-namespace> \
    ///     cargo test -p kaiv-cli -- --ignored staging_round_trip
    ///
    /// Content addressing makes it idempotent: every run deposits
    /// the identical bytes at the identical name.
    #[test]
    #[ignore = "manual-only: writes to the staging registries"]
    fn staging_round_trip() {
        let ns = std::env::var("KAIV_PUBLISH_E2E_NS")
            .expect("set KAIV_PUBLISH_E2E_NS to a namespace you own on staging");
        let r = kaiv::Resolver::offline();
        let raiv = kaiv::compile_with(b"::probe=kaiv-publish-e2e\n", &r).unwrap();
        let daiv = kaiv::denorm::denormalize_with(&raiv, &r).unwrap();
        let dir = tmp_dir("staging");
        let file = write_file(&dir, "probe.daiv", &daiv);
        let args: Vec<String> = vec![file, "--namespace".into(), ns, "--batch".into()];
        // KAIV_TOKEN comes from the real environment here.
        let out = run(
            &parse_args(&args).unwrap(),
            &EnvOpts::from_env(),
            account::load,
        )
        .unwrap();
        assert!(out.contains("published 1/1"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
