//! `kaiv login` — identity against the kaiv registries (idaiv).
//!
//! Passwordless, CLI-first, on the literium model: the email-link
//! device authorization grant (authentes; the RFC 8628 shape with
//! an emailed one-time link as the approval channel). Creating an
//! account and signing in are the same act — the first verified
//! sign-in provisions the account — so there is no separate
//! `signup` verb.
//!
//! The stored credential is the **rotating refresh token**: access
//! tokens live minutes and are minted on demand by
//! [`access_token`], which MUST persist the newly returned refresh
//! token immediately — authentes revokes the whole family if a
//! rotated-out token is ever reused.
//!
//! Credentials live at `$XDG_CONFIG_HOME/kaiv/credentials`
//! (default `~/.config/kaiv/credentials`), mode 0600, one
//! `key = "value"` per line. The identity host defaults to the
//! alpha idaiv (`id.kaiv.io`, matching the toolchain's alpha
//! registry hosts); `KAIV_ID_URL` overrides it (the production
//! `idaiv.com`, or a local wrangler dev authentes).

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The alpha identity host; `idaiv.com` activates at beta with the
/// rest of the `k*aiv.com` surface.
pub const DEFAULT_ISSUER: &str = "https://id.kaiv.io";
const CLIENT_ID: &str = "kaiv-cli";
const SCOPE: &str = "pyloros";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const USER_AGENT: &str = concat!("kaiv/", env!("CARGO_PKG_VERSION"));

pub fn issuer() -> String {
    std::env::var("KAIV_ID_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ISSUER.to_string())
        .trim_end_matches('/')
        .to_string()
}

// ── The credentials file ────────────────────────────────────────

pub struct Credentials {
    pub issuer: String,
    pub email: String,
    pub refresh_token: String,
}

pub fn credentials_path() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or("cannot locate a config directory (no HOME)")?;
    Ok(base.join("kaiv").join("credentials"))
}

pub fn load() -> Result<Option<Credentials>, String> {
    let path = credentials_path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let mut fields = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(
                key.trim().to_string(),
                value.trim().trim_matches('"').to_string(),
            );
        }
    }
    let take = |k: &str| -> Result<String, String> {
        fields
            .get(k)
            .cloned()
            .ok_or_else(|| format!("credentials file lacks {k}: {}", path.display()))
    };
    Ok(Some(Credentials {
        issuer: take("issuer")?,
        email: take("email")?,
        refresh_token: take("refresh_token")?,
    }))
}

pub fn save(credentials: &Credentials) -> Result<(), String> {
    let path = credentials_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let body = format!(
        "issuer = \"{}\"\nemail = \"{}\"\nrefresh_token = \"{}\"\n",
        credentials.issuer, credentials.email, credentials.refresh_token
    );
    write_private(&path, &body)
}

#[cfg(unix)]
fn write_private(path: &PathBuf, body: &str) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    file.write_all(body.as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn write_private(path: &PathBuf, body: &str) -> Result<(), String> {
    std::fs::write(path, body).map_err(|e| format!("write {}: {e}", path.display()))
}

pub fn erase() -> Result<bool, String> {
    let path = credentials_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

// ── HTTP plumbing (ureq 3; non-2xx statuses are data here — the
//    device poll reads its state from 400 bodies) ────────────────

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    )
}

fn post_form(url: &str, form: &[(&str, &str)]) -> Result<(u16, String), String> {
    let mut response = agent()
        .post(url)
        .header("User-Agent", USER_AGENT)
        .send_form(form.iter().copied())
        .map_err(|e| format!("cannot reach {url}: {e}"))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("read response from {url}: {e}"))?;
    Ok((status, body))
}

fn get_bearer(url: &str, token: &str) -> Result<(u16, String), String> {
    let mut response = agent()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| format!("cannot reach {url}: {e}"))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("read response from {url}: {e}"))?;
    Ok((status, body))
}

// ── Minimal JSON field extraction ───────────────────────────────
//
// The authentes responses are small JSON objects whose keys are
// unique per body, so a scan for `"key"` followed by a `:` and a
// literal is sufficient — no document model needed. Strings decode
// the standard escapes (tokens and emails never carry them, error
// descriptions may).

fn json_str_field(body: &str, key: &str) -> Option<String> {
    let value_at = find_value(body, key)?;
    let rest = &body[value_at..];
    if !rest.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut chars = rest[1..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'b' => out.push('\u{0008}'),
                'f' => out.push('\u{000C}'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    // Surrogates cannot appear in these bodies; a
                    // lone one simply fails the extraction.
                    out.push(char::from_u32(code)?);
                }
                _ => return None,
            },
            c => out.push(c),
        }
    }
    None
}

fn json_u64_field(body: &str, key: &str) -> Option<u64> {
    let value_at = find_value(body, key)?;
    let digits: String = body[value_at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Byte offset of the value that follows `"key" :`, or None.
fn find_value(body: &str, key: &str) -> Option<usize> {
    let pat = format!("\"{key}\"");
    let mut search = 0;
    while let Some(p) = body[search..].find(&pat) {
        let mut i = search + p + pat.len();
        let bytes = body.as_bytes();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b':' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            return Some(i);
        }
        search += p + pat.len();
    }
    None
}

fn oauth_error(body: &str) -> String {
    json_str_field(body, "error_description")
        .or_else(|| json_str_field(body, "error"))
        .unwrap_or_else(|| "unexpected response".to_string())
}

// ── The device grant ────────────────────────────────────────────

pub struct DeviceGrant {
    pub user_code: String,
    device_code: String,
    interval: Duration,
    expires_in: Duration,
    issuer: String,
    email: String,
}

/// Step 1: initiate. authentes emails the one-time link and we get
/// the polling credential plus the user code the reader must
/// compare against the mail (RFC 8628 §5.4).
pub fn begin_login(email: &str) -> Result<DeviceGrant, String> {
    let issuer = issuer();
    let (status, body) = post_form(
        &format!("{issuer}/device/authorize"),
        &[("client_id", CLIENT_ID), ("scope", SCOPE), ("email", email)],
    )?;
    if status == 501 {
        return Err("this identity host has no email sender configured".into());
    }
    if status != 200 {
        return Err(format!("sign-in refused: {}", oauth_error(&body)));
    }
    let device_code = json_str_field(&body, "device_code")
        .ok_or("initiation response lacks device_code")?;
    let user_code =
        json_str_field(&body, "user_code").ok_or("initiation response lacks user_code")?;
    let interval = json_u64_field(&body, "interval").unwrap_or(5);
    let expires_in = json_u64_field(&body, "expires_in").unwrap_or(600);
    Ok(DeviceGrant {
        user_code,
        device_code,
        interval: Duration::from_secs(interval),
        expires_in: Duration::from_secs(expires_in),
        issuer,
        email: email.to_string(),
    })
}

/// Step 2: poll `/token` until the emailed link is redeemed.
/// Blocks (sleeping the advertised interval; `slow_down` backs off
/// per RFC 8628 §3.5) and returns the stored credentials.
pub fn wait_for_approval(grant: &DeviceGrant) -> Result<Credentials, String> {
    let deadline = Instant::now() + grant.expires_in;
    let mut interval = grant.interval;
    loop {
        std::thread::sleep(interval);
        if Instant::now() > deadline {
            return Err("the sign-in link expired — try again".into());
        }
        let (status, body) = post_form(
            &format!("{}/token", grant.issuer),
            &[
                ("grant_type", DEVICE_GRANT),
                ("device_code", &grant.device_code),
                ("client_id", CLIENT_ID),
            ],
        )?;
        if status == 200 {
            let refresh_token = json_str_field(&body, "refresh_token")
                .ok_or("token response lacks refresh_token")?;
            let credentials = Credentials {
                issuer: grant.issuer.clone(),
                email: grant.email.clone(),
                refresh_token,
            };
            save(&credentials)?;
            return Ok(credentials);
        }
        match json_str_field(&body, "error").unwrap_or_default().as_str() {
            "authorization_pending" => {}
            // RFC 8628 §3.5: back the interval off by 5 s.
            "slow_down" => interval += Duration::from_secs(5),
            "expired_token" => return Err("the sign-in link expired — try again".into()),
            _ => return Err(format!("sign-in refused: {}", oauth_error(&body))),
        }
    }
}

/// Mint a fresh access token from the stored refresh token,
/// persisting the rotated refresh token before returning — the
/// non-negotiable rotation discipline.
pub fn access_token(credentials: &mut Credentials) -> Result<String, String> {
    let (status, body) = post_form(
        &format!("{}/token", credentials.issuer),
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &credentials.refresh_token),
            ("client_id", CLIENT_ID),
        ],
    )?;
    if status != 200 {
        return Err(format!(
            "session expired or revoked ({}) — run `kaiv login`",
            oauth_error(&body)
        ));
    }
    if let Some(rotated) = json_str_field(&body, "refresh_token") {
        credentials.refresh_token = rotated;
        save(credentials)?;
    }
    json_str_field(&body, "access_token").ok_or_else(|| "token response lacks access_token".into())
}

/// The account as idaiv sees it (`GET /account`):
/// `(id, email, handle)`.
pub fn whoami(
    credentials: &mut Credentials,
) -> Result<(String, String, Option<String>), String> {
    let token = access_token(credentials)?;
    let (status, body) = get_bearer(&format!("{}/account", credentials.issuer), &token)?;
    if status != 200 {
        return Err(format!("account lookup: {}", oauth_error(&body)));
    }
    let id = json_str_field(&body, "id").ok_or("account response lacks id")?;
    let email = json_str_field(&body, "email").ok_or("account response lacks email")?;
    Ok((id, email, json_str_field(&body, "handle")))
}

/// Best-effort server-side revocation (RFC 7009); the caller
/// erases the file regardless.
pub fn revoke(credentials: &Credentials) {
    let _ = post_form(
        &format!("{}/revoke", credentials.issuer),
        &[
            ("token", &credentials.refresh_token),
            ("client_id", CLIENT_ID),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Environment variables are process-global; every env-touching
    /// assertion lives in this one test so parallel test threads
    /// never race on them.
    #[test]
    fn credentials_roundtrip_and_issuer_override() {
        let dir = std::env::temp_dir().join(format!("kaiv-account-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        assert!(load().unwrap().is_none());
        save(&Credentials {
            issuer: "https://id.example".into(),
            email: "w@example.com".into(),
            refresh_token: "abc.def".into(),
        })
        .unwrap();
        let back = load().unwrap().unwrap();
        assert_eq!(back.issuer, "https://id.example");
        assert_eq!(back.email, "w@example.com");
        assert_eq!(back.refresh_token, "abc.def");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(credentials_path().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        assert!(erase().unwrap());
        assert!(!erase().unwrap());
        assert!(load().unwrap().is_none());

        std::env::remove_var("KAIV_ID_URL");
        assert_eq!(issuer(), DEFAULT_ISSUER);
        std::env::set_var("KAIV_ID_URL", "http://localhost:8787/");
        assert_eq!(issuer(), "http://localhost:8787");
        std::env::remove_var("KAIV_ID_URL");

        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_field_extraction() {
        let body = r#"{"user":{"id":"u1","email":"a@b.c","handle":null},"interval":5}"#;
        assert_eq!(json_str_field(body, "id").as_deref(), Some("u1"));
        assert_eq!(json_str_field(body, "email").as_deref(), Some("a@b.c"));
        assert_eq!(json_str_field(body, "handle"), None);
        assert_eq!(json_u64_field(body, "interval"), Some(5));
        assert_eq!(json_str_field(body, "missing"), None);

        // Escapes decode; a key inside a VALUE is not a key.
        let tricky = r#"{"note":"say \"hi\" é","error":"x"}"#;
        assert_eq!(
            json_str_field(tricky, "note").as_deref(),
            Some("say \"hi\" é")
        );
        let value_not_key = r#"{"a":"error","error":"real"}"#;
        assert_eq!(json_str_field(value_not_key, "error").as_deref(), Some("real"));
    }

    #[test]
    fn oauth_error_extraction() {
        assert_eq!(
            oauth_error(r#"{"error":"expired_token","error_description":"gone"}"#),
            "gone"
        );
        assert_eq!(oauth_error(r#"{"error":"slow_down"}"#), "slow_down");
        assert_eq!(oauth_error("not json"), "unexpected response");
    }
}
