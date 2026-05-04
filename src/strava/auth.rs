// strava::auth — OAuth2 plumbing for the Strava API.
//
// Strava implements bog-standard OAuth2 with the authorization code
// grant. We use the `oauth2` crate (v5, typestate API) to construct
// the URLs and exchange tokens; the actual HTTP work is delegated to
// the same `reqwest::Client` we use for the data API. Redirects are
// disabled to keep the SSRF surface minimal (per the oauth2 docs).
//
// Prescriptive failure: every error message names which env var or
// CLI step the operator should reach for next.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet,
    RedirectUrl, Scope, TokenUrl,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration as StdDuration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const STRAVA_AUTHORIZE_URL: &str = "https://www.strava.com/oauth/authorize";
const STRAVA_TOKEN_URL: &str = "https://www.strava.com/oauth/token";
const STRAVA_SCOPE: &str = "activity:read_all";

/// Concrete typestate for our configured client: auth + token URLs are
/// set, the other endpoint slots are intentionally empty.
type StravaOAuthClient = BasicClient<
    EndpointSet,    // HasAuthUrl
    EndpointNotSet, // HasDeviceAuthUrl
    EndpointNotSet, // HasIntrospectionUrl
    EndpointNotSet, // HasRevocationUrl
    EndpointSet,    // HasTokenUrl
>;

/// Wraps an `oauth2` client preconfigured for Strava.
///
/// We keep the raw `client_id` / `client_secret` strings alongside the
/// typed `oauth2` client because `exchange_code` issues a direct POST
/// (to capture the inline `athlete` object) and `oauth2` v5 does not
/// expose a public `client_secret()` accessor.
pub struct AuthClient {
    client: StravaOAuthClient,
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
}

/// A fresh access token plus the refresh token to keep using.
///
/// Strava sometimes rotates the refresh token on a refresh; callers
/// should always persist the value here even if it appears unchanged.
///
/// `athlete_id` is populated from Strava's refresh response (which
/// always includes an `athlete.id`), and from the auth-code exchange
/// when available. If Strava ever omits it, callers should fall back
/// to a separate `/athlete` request.
#[derive(Debug, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    pub athlete_id: Option<i64>,
}

impl AuthClient {
    /// Build a Strava OAuth client. Fails only if the hard-coded
    /// authorize/token URLs become invalid (i.e. never, in practice).
    pub fn new(client_id: ClientId, client_secret: ClientSecret) -> Result<Self> {
        let auth_url = AuthUrl::new(STRAVA_AUTHORIZE_URL.to_string())
            .context("invalid Strava authorize URL (this is a patinate bug)")?;
        let token_url = TokenUrl::new(STRAVA_TOKEN_URL.to_string())
            .context("invalid Strava token URL (this is a patinate bug)")?;

        let id_str = client_id.as_str().to_string();
        let secret_str = client_secret.secret().to_string();

        let client = BasicClient::new(client_id)
            .set_client_secret(client_secret)
            .set_auth_uri(auth_url)
            .set_token_uri(token_url);

        // SSRF mitigation: do not follow redirects in OAuth flows.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context(
                "could not build reqwest client for OAuth. \
                 Re-run; if it persists, check your TLS root store.",
            )?;

        Ok(Self {
            client,
            http,
            client_id: id_str,
            client_secret: secret_str,
        })
    }

    /// Build the URL the user should open in a browser to authorize
    /// patinate against their Strava account. Returns the URL plus
    /// the CSRF token the caller must compare against the `state`
    /// query parameter when the browser redirects back.
    pub fn authorize_url(&self, redirect_url: &str) -> Result<(url::Url, CsrfToken)> {
        let redirect = RedirectUrl::new(redirect_url.to_string()).with_context(|| {
            format!(
                "invalid redirect URL {redirect_url:?}. \
                 Use something Strava accepts, e.g. http://localhost:7878/callback."
            )
        })?;
        let (url, csrf) = self
            .client
            .clone()
            .set_redirect_uri(redirect)
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new(STRAVA_SCOPE.to_string()))
            .url();
        Ok((url, csrf))
    }

    /// Exchange the `code` query parameter Strava returned to the
    /// redirect URL for a full set of tokens.
    ///
    /// Implemented as a direct POST (rather than going through the
    /// typed `oauth2` pipeline) for the same reason as `refresh()`:
    /// Strava's auth-code response includes the `athlete` object
    /// inline. Capturing `athlete.id` here saves callers a follow-up
    /// `/athlete` call. Callers that already do that fallback (see
    /// `cli.rs`) will simply skip it when `athlete_id` is `Some`.
    pub async fn exchange_code(
        &self,
        code: AuthorizationCode,
        redirect_url: &str,
    ) -> Result<Tokens> {
        // Validate + canonicalize the redirect URL up front so a typo
        // surfaces before we hit the network. RFC 6749 4.1.3 requires
        // `redirect_uri` in the token request whenever it was present
        // at /authorize (always, in our case). Strava is lenient today
        // but a future tightening would break the auth flow without it.
        let redirect = RedirectUrl::new(redirect_url.to_string())
            .with_context(|| format!("invalid redirect URL {redirect_url:?}"))?;
        let redirect_uri = redirect.url().as_str();

        let resp = self
            .http
            .post(STRAVA_TOKEN_URL)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("code", code.secret().as_str()),
                ("grant_type", "authorization_code"),
                ("redirect_uri", redirect_uri),
            ])
            .send()
            .await
            .context(
                "Strava rejected the authorization code. \
                 The code may have expired (they're single-use, ~10min); \
                 re-run `patinate auth` to start over.",
            )?;

        let status = resp.status();
        if !status.is_success() {
            // Don't echo the response body. Strava error payloads can
            // include request echoes and token fragments. Status alone
            // tells the operator what to do next.
            let _ = resp.text().await;
            anyhow::bail!(
                "Strava OAuth code exchange returned HTTP {status}. \
                 The code may have expired (they're single-use, ~10min); \
                 re-run `patinate auth` to start over."
            );
        }

        let wire: WireRefreshResponse = resp.json().await.context(
            "could not decode Strava auth-code response. \
             Strava may have changed its OAuth payload; rerun `patinate auth` \
             and if it persists open an issue against patinate.",
        )?;

        Ok(Tokens {
            access_token: wire.access_token,
            refresh_token: wire.refresh_token,
            expires_at: tokens_expires_at(wire.expires_in),
            athlete_id: wire.athlete.map(|a| a.id),
        })
    }

    /// Trade a stored refresh token for a fresh access token.
    ///
    /// Implemented as a direct POST rather than going through the
    /// `oauth2` crate so we can pull `athlete.id` out of the JSON
    /// payload. Strava's refresh response includes it on every call;
    /// the typed pipeline in `oauth2` would otherwise require defining
    /// a custom `Client` specialization just for this one extra field.
    pub async fn refresh(
        &self,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
    ) -> Result<Tokens> {
        let resp = self
            .http
            .post(STRAVA_TOKEN_URL)
            .form(&[
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await
            .context(
                "Strava OAuth refresh failed; the refresh token may be revoked. \
                 Re-authorize at https://www.strava.com/settings/api and update \
                 STRAVA_PATINATE_REFRESH_TOKEN.",
            )?;

        let status = resp.status();
        if !status.is_success() {
            // Don't echo the response body. Strava returns request echoes
            // and occasional token fragments in error payloads; logging it
            // verbatim leaks secrets into logs and CI output. Status alone
            // is enough for the operator to know what to do next.
            let _ = resp.text().await;
            anyhow::bail!(
                "Strava OAuth refresh returned HTTP {status}. \
                 The refresh token may be revoked: re-authorize at \
                 https://www.strava.com/settings/api and update \
                 STRAVA_PATINATE_REFRESH_TOKEN."
            );
        }

        let wire: WireRefreshResponse = resp.json().await.context(
            "could not decode Strava refresh response. \
             Strava may have changed its OAuth payload; rerun `patinate sync` \
             and if it persists open an issue against patinate.",
        )?;

        Ok(Tokens {
            access_token: wire.access_token,
            refresh_token: wire.refresh_token,
            expires_at: tokens_expires_at(wire.expires_in),
            athlete_id: wire.athlete.map(|a| a.id),
        })
    }
}

/// Translate Strava's `expires_in` (seconds) into an absolute
/// `expires_at`. Floors at 1 hour from now when the value is missing
/// or non-positive: a nonsense `expires_in` should not trigger an
/// immediate refresh loop on the next call.
fn tokens_expires_at(expires_in: i64) -> DateTime<Utc> {
    if expires_in <= 0 {
        tracing::warn!(
            expires_in,
            "Strava token response had a missing or non-positive `expires_in`; \
             defaulting to 1h. If refreshes persist, re-authorize at \
             https://www.strava.com/settings/api."
        );
        return Utc::now() + Duration::hours(1);
    }
    Utc::now() + Duration::seconds(expires_in)
}

/// Wire format for Strava's OAuth refresh response. Includes the
/// standard RFC 6749 fields plus the `athlete` object so we can pin
/// the cached row to a specific user without a follow-up `/athlete`
/// request.
#[derive(Debug, Deserialize)]
struct WireRefreshResponse {
    access_token: String,
    refresh_token: String,
    /// Seconds from now until the access token expires. Strava emits
    /// this as a positive integer; we clamp to 0 just in case.
    expires_in: i64,
    #[serde(default)]
    athlete: Option<WireRefreshAthlete>,
}

#[derive(Debug, Deserialize)]
struct WireRefreshAthlete {
    id: i64,
}

/// What we recovered from Strava's redirect back to localhost.
///
/// `code` is the single-use authorization code to swap for tokens
/// (empty when Strava reports an error).
/// `state` is whatever Strava echoed back; the caller must compare it
/// to the `CsrfToken` it got from `authorize_url()` BEFORE acting on
/// `error`. Acting on a forged `error=` value before validating
/// `state` would let an attacker control the operator's UX.
/// `error` carries Strava's `error=` query parameter when present.
#[derive(Debug)]
pub struct CallbackParams {
    pub code: String,
    pub state: String,
    pub error: Option<String>,
}

/// Build the localhost redirect URL for a given port. Centralized so
/// the server bind, the authorize-URL build, and the code-exchange
/// step all agree on the exact string.
pub fn local_redirect_url(port: u16) -> String {
    format!("http://localhost:{port}/callback")
}

/// Constant-time CSRF state comparison. Returns `true` only when both
/// inputs are exactly equal byte-for-byte. The realistic attack on
/// loopback is near-zero, but timing-safe equality costs almost
/// nothing and removes the question from any future audit.
pub fn csrf_states_match(got: &str, want: &str) -> bool {
    use subtle::ConstantTimeEq;
    let got = got.as_bytes();
    let want = want.as_bytes();
    got.len() == want.len() && got.ct_eq(want).into()
}

/// Bind a one-shot HTTP listener on `127.0.0.1:port` and wait up to
/// `timeout` for Strava to redirect the user back with a `code` query
/// parameter. Returns the parsed `code` + `state`.
///
/// This is deliberately a hand-rolled HTTP/1.1 reader rather than
/// pulling in `hyper` directly: the request shape is a single GET to
/// `/callback?code=...&state=...&scope=...`, and the only response we
/// emit is a small thank-you page. Anything more would be cargo-cult.
///
/// Prescriptive failures: bind conflicts, malformed requests, and
/// timeouts each name the next operator action explicitly.
pub async fn await_callback(port: u16, timeout: StdDuration) -> Result<CallbackParams> {
    let bind_addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&bind_addr).await.with_context(|| {
        format!(
            "could not bind {bind_addr}. \
             Another process may already be using port {port}; \
             rerun with `patinate auth --port <free-port>` and ensure \
             your Strava app's authorization callback domain still \
             accepts localhost (it does by default)."
        )
    })?;

    let accept_fut = async {
        loop {
            let (mut stream, _peer) = listener.accept().await.context(
                "lost the localhost listener while waiting for the Strava \
                 redirect. Re-run `patinate auth`.",
            )?;

            // Read until we see the end-of-headers marker `\r\n\r\n` or
            // hit the 8 KiB cap. A single `read()` could truncate when
            // headers split across packets, silently dropping the
            // `code` query parameter on the floor.
            let head = match read_request_head(&mut stream).await? {
                Some(h) => h,
                None => continue, // probe / idle timeout: keep listening
            };
            // Request line: "GET /callback?code=...&state=... HTTP/1.1"
            let request_line = head.lines().next().unwrap_or("");
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("");
            let target = parts.next().unwrap_or("");

            if !method.eq_ignore_ascii_case("GET") {
                // 405/404 write errors are deliberately swallowed: the
                // browser tab is incidental. The real signal is whether
                // we eventually receive a /callback hit.
                write_response(
                    &mut stream,
                    "405 Method Not Allowed",
                    "patinate auth expects a GET on /callback.",
                )
                .await
                .ok();
                continue;
            }

            // Skip favicon probes and other noise; only /callback
            // counts as the real redirect.
            if !target.starts_with("/callback") {
                // See note above on swallowed write errors.
                write_response(&mut stream, "404 Not Found", "Not the callback path.")
                    .await
                    .ok();
                continue;
            }

            let params = parse_query(target);
            let error = params.get("error").cloned();
            let state = params.get("state").cloned().unwrap_or_default();
            let code = params.get("code").cloned().unwrap_or_default();

            // `code` is legitimately absent when Strava reports an
            // error (user denied, scope rejected, etc.). Surface both
            // upward so the caller can validate `state` against the
            // CSRF token first, then decide what to do with `error`.
            // Acting on `error` here would let a forged redirect
            // bypass the CSRF check entirely.
            if code.is_empty() && error.is_none() {
                // See note above on swallowed write errors.
                write_response(
                    &mut stream,
                    "400 Bad Request",
                    "Missing `code` parameter; re-run `patinate auth`.",
                )
                .await
                .ok();
                bail!(
                    "Strava redirect to localhost has neither `code` nor `error`. \
                     Re-run `patinate auth` and click Authorize."
                );
            }

            let body = "<!doctype html><html><head><meta charset=\"utf-8\">\
                        <title>patinate authorized</title></head>\
                        <body style=\"font-family:system-ui;max-width:32rem;margin:4rem auto;\">\
                        <h1>OK.</h1><p>patinate has the authorization code. \
                        You can close this tab and return to the terminal.</p>\
                        </body></html>";
            write_html(&mut stream, "200 OK", body).await.ok();

            return Ok(CallbackParams { code, state, error });
        }
    };

    match tokio::time::timeout(timeout, accept_fut).await {
        Ok(res) => res,
        Err(_) => bail!(
            "Auth timed out. Re-run `patinate auth` and approve in the \
             browser within {} seconds.",
            timeout.as_secs()
        ),
    }
}

/// Read an HTTP request head off `stream` until we observe the
/// end-of-headers marker `\r\n\r\n` or hit the 8 KiB cap. Each
/// individual `read()` is bounded by a 2-second idle timeout so a
/// half-open connection cannot stall the auth loop.
///
/// Returns:
/// - `Ok(Some(head))` on a well-formed request head.
/// - `Ok(None)` on idle timeout, EOF before any bytes, or an empty
///   probe. Callers should `continue` and accept the next connection.
/// - `Err(_)` on irrecoverable IO errors or cap-exceeded with no
///   terminator seen, with prescriptive context for the operator.
async fn read_request_head(stream: &mut tokio::net::TcpStream) -> Result<Option<String>> {
    const HEAD_CAP: usize = 8 * 1024;
    const IDLE_TIMEOUT: StdDuration = StdDuration::from_secs(2);

    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    loop {
        let read_res = tokio::time::timeout(IDLE_TIMEOUT, stream.read(&mut tmp)).await;
        match read_res {
            Ok(Ok(0)) => {
                // Peer closed before sending the full head.
                if buf.is_empty() {
                    return Ok(None);
                }
                bail!(
                    "callback connection closed before headers completed. \
                     Re-run `patinate auth`."
                );
            }
            Ok(Ok(n)) => {
                buf.extend_from_slice(&tmp[..n]);
                if find_double_crlf(&buf).is_some() {
                    return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
                }
                if buf.len() >= HEAD_CAP {
                    // Cap exceeded with no terminator: emit a 408 and
                    // bail. A pathological client should not be able
                    // to stall the auth loop or eat unbounded memory.
                    write_response(
                        stream,
                        "408 Request Timeout",
                        "Request headers exceeded 8 KiB without terminator.",
                    )
                    .await
                    .ok();
                    bail!(
                        "callback request headers exceeded 8 KiB with no `\\r\\n\\r\\n`. \
                         Re-run `patinate auth`; if this persists, ensure no proxy is \
                         injecting unexpected headers."
                    );
                }
            }
            Ok(Err(e)) => {
                return Err(anyhow::Error::new(e).context(
                    "could not read the localhost callback request. \
                     Re-run `patinate auth`.",
                ));
            }
            Err(_) => {
                // Per-read idle timeout. Notify the peer and let the
                // accept loop pick up the next connection.
                write_response(
                    stream,
                    "408 Request Timeout",
                    "patinate auth read timed out.",
                )
                .await
                .ok();
                return Ok(None);
            }
        }
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Tiny URL-decoder + query splitter. Handles `+` -> space and `%XX`
/// escapes, which is everything Strava emits in callback params.
fn parse_query(target: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let q = match target.split_once('?') {
        Some((_, q)) => q,
        None => return out,
    };
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(url_decode(k), url_decode(v));
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// `body.len()` is byte-length, which only matches HTTP Content-Length
// for pure-ASCII bodies. Every callsite passes a fixed English-only
// string, so this is correct today; if a non-ASCII body ever lands
// here, switch to `body.as_bytes().len()` (same value for ASCII, right
// answer for UTF-8 multibyte).
async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.shutdown().await
}

async fn write_html(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_query_extracts_code_and_state() {
        let q = parse_query("/callback?code=abc123&state=xyz&scope=activity:read_all");
        assert_eq!(q.get("code").map(String::as_str), Some("abc123"));
        assert_eq!(q.get("state").map(String::as_str), Some("xyz"));
        assert_eq!(
            q.get("scope").map(String::as_str),
            Some("activity:read_all")
        );
    }

    #[test]
    fn url_decode_handles_percent_and_plus() {
        // `%3A` = ':' and `+` = ' ' — Strava round-trips both.
        assert_eq!(url_decode("activity%3Aread_all"), "activity:read_all");
        assert_eq!(url_decode("hello+world"), "hello world");
    }

    #[test]
    fn csrf_states_match_accepts_equal_strings() {
        assert!(csrf_states_match("abc123", "abc123"));
        assert!(csrf_states_match("", ""));
    }

    #[test]
    fn csrf_states_match_rejects_mismatch() {
        // One char different.
        assert!(!csrf_states_match("abc123", "abc124"));
        // Different lengths (the load-bearing case where naive `!=`
        // also rejects, but timing-safe equality must short-circuit
        // BEFORE the byte loop).
        assert!(!csrf_states_match("abc123", "abc1234"));
        assert!(!csrf_states_match("abc1234", "abc123"));
        // Empty vs non-empty.
        assert!(!csrf_states_match("", "x"));
        assert!(!csrf_states_match("x", ""));
    }

    #[test]
    fn local_redirect_url_matches_strava_expectation() {
        // Strava treats any localhost port as valid as long as the
        // app's authorization callback domain includes `localhost`.
        assert_eq!(local_redirect_url(8765), "http://localhost:8765/callback");
    }

    #[test]
    fn find_double_crlf_locates_header_terminator() {
        let head = b"GET / HTTP/1.1\r\nHost: x\r\n\r\nbody";
        // The terminator starts right after `Host: x` (15 + 1 + 7 = 23).
        assert_eq!(find_double_crlf(head), Some(23));
        assert_eq!(find_double_crlf(b"GET / HTTP/1.1\r\nHost: x"), None);
    }

    #[test]
    fn tokens_expires_at_floors_missing_to_one_hour() {
        let before = Utc::now();
        let when = tokens_expires_at(0);
        let after = Utc::now();
        // Floor pins expires_at to ~1h ahead. Accept a small jitter.
        let lo = before + Duration::hours(1) - Duration::seconds(1);
        let hi = after + Duration::hours(1) + Duration::seconds(1);
        assert!(
            when >= lo && when <= hi,
            "expected ~1h from now, got {when}"
        );
    }

    #[test]
    fn tokens_expires_at_uses_provided_seconds() {
        let before = Utc::now();
        let when = tokens_expires_at(7200);
        let lo = before + Duration::seconds(7199);
        assert!(when >= lo);
    }
}
