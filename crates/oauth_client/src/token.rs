//! The token endpoint: a code's exchange, a refresh, and a refresh token's revocation (RFC 6749
//! §4.1.3 and §6, RFC 7636, RFC 8707, RFC 7009).
//!
//! Studio reads an access token's claims without checking its signature: the server checks
//! that. Studio's check only keeps it from sending a token issued for another resource, issuer
//! or client, or one that lives longer than the server accepts (spec 016, Checking the token).

use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http_client::HttpClient;
use http_client::http::StatusCode;
use serde_json::{Map, Value};

use crate::discovery::AuthorizationServer;
use crate::fetch::{self, REVOKE_TIMEOUT, TOKEN_TIMEOUT};
use crate::{MAX_ACCESS_SECONDS, shown};

/// The longest access token Studio keeps, in bytes: the server refuses a longer one.
const MAX_TOKEN_BYTES: usize = 16 * 1024;

/// Whom a token was issued to: its subject, which is the account, and its organization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub subject: String,
    pub organization: Option<String>,
}

/// What a token answer gave, checked.
#[derive(Clone)]
pub struct Tokens {
    pub access_token: String,
    /// A new refresh token, which replaces the one held; `None` when the answer gave none, and
    /// the one held stands (RFC 6749 §6).
    pub refresh_token: Option<String>,
    /// When the access token stops being accepted: `expires_in` after it was given. Its `exp` is
    /// the service's clock, not this machine's, so it does not move this.
    pub expires_at: SystemTime,
    pub account: Account,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("expires_at", &self.expires_at)
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

/// What a token must say to be used.
#[derive(Debug, Clone, Copy)]
pub struct Expected<'a> {
    pub issuer: &'a str,
    pub resource: &'a str,
    pub client_id: &'a str,
    /// The organization the target names, which `org_id` must equal; `None` when it names none,
    /// and the server checks it alone.
    pub organization: Option<&'a str>,
}

#[derive(Clone, PartialEq, Eq)]
pub enum TokenError {
    /// `invalid_grant`: the code or refresh token is not good any more. A refresh that ends so
    /// signs the person out.
    InvalidGrant(String),
    /// The sign-in service or Studio refused the answer, for a reason another try does not
    /// change.
    Refused(String),
    /// The request did not complete, or the service failed. A refresh token may have been
    /// rotated anyway, so a refresh is tried once more before the person is asked to sign in.
    Failed(String),
    /// The service gave tokens Studio may not use, for a reason another try does not change:
    /// why, and the refresh token they came with, which nothing will hold, so the caller
    /// revokes it.
    Unusable {
        why: String,
        refresh_token: Option<String>,
    },
}

/// Says why, never the refresh token an unusable answer carries.
impl std::fmt::Debug for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::InvalidGrant(why) => f.debug_tuple("InvalidGrant").field(why).finish(),
            TokenError::Refused(why) => f.debug_tuple("Refused").field(why).finish(),
            TokenError::Failed(why) => f.debug_tuple("Failed").field(why).finish(),
            TokenError::Unusable { why, .. } => f
                .debug_struct("Unusable")
                .field("why", why)
                .finish_non_exhaustive(),
        }
    }
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::InvalidGrant(why)
            | TokenError::Refused(why)
            | TokenError::Failed(why)
            | TokenError::Unusable { why, .. } => f.write_str(why),
        }
    }
}

/// Exchanges a sign-in's code for tokens.
pub async fn exchange(
    http: &dyn HttpClient,
    server: &AuthorizationServer,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    expected: Expected<'_>,
) -> Result<Tokens, TokenError> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", expected.client_id),
        ("code_verifier", verifier),
        ("resource", expected.resource),
    ];
    request(http, server, &form, expected).await
}

/// Refreshes the access token. The resource is sent every time: without it a service may issue
/// a token for a default audience.
pub async fn refresh(
    http: &dyn HttpClient,
    server: &AuthorizationServer,
    refresh_token: &str,
    expected: Expected<'_>,
) -> Result<Tokens, TokenError> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", expected.client_id),
        ("resource", expected.resource),
    ];
    request(http, server, &form, expected).await
}

/// Revokes a refresh token at the server's revocation endpoint, once, within 10 seconds;
/// nothing when it has none.
pub async fn revoke(
    http: &dyn HttpClient,
    server: &AuthorizationServer,
    client_id: &str,
    refresh_token: &str,
) -> Result<(), String> {
    let Some(endpoint) = &server.revocation_endpoint else {
        return Ok(());
    };
    let form = [
        ("token", refresh_token),
        ("token_type_hint", "refresh_token"),
        ("client_id", client_id),
    ];
    let answer = fetch::post_form(http, endpoint, &form, REVOKE_TIMEOUT).await?;
    if answer.status.is_success() {
        Ok(())
    } else {
        Err(format!("the answer was {}", answer.status.as_u16()))
    }
}

async fn request(
    http: &dyn HttpClient,
    server: &AuthorizationServer,
    form: &[(&str, &str)],
    expected: Expected<'_>,
) -> Result<Tokens, TokenError> {
    let answer = fetch::post_form(http, &server.token_endpoint, form, TOKEN_TIMEOUT)
        .await
        .map_err(|why| {
            TokenError::Failed(format!("The sign-in service could not be reached: {why}."))
        })?;
    let Some(body) = answer.object() else {
        return Err(TokenError::Failed(if answer.body.is_none() {
            "The sign-in service answered with more than 64 KiB.".into()
        } else {
            format!(
                "The sign-in service answered {} with something Studio cannot read.",
                answer.status.as_u16()
            )
        }));
    };
    if !answer.status.is_success() {
        let error = body.get("error").and_then(Value::as_str).unwrap_or("");
        let description = body
            .get("error_description")
            .and_then(Value::as_str)
            .map(shown);
        let said = |default: &str| description.clone().unwrap_or_else(|| default.to_owned());
        return Err(match error {
            "invalid_grant" => TokenError::InvalidGrant(said("The sign-in is no longer valid.")),
            "invalid_client" | "unauthorized_client" => TokenError::Refused(format!(
                "The sign-in service does not accept Omnigraph Studio: {}",
                said("it refused Studio's client ID.")
            )),
            "invalid_target" => TokenError::Refused(format!(
                "The sign-in service does not issue tokens for this server: {}",
                said("it refused the resource.")
            )),
            "" if answer.status.is_server_error() => TokenError::Failed(format!(
                "The sign-in service failed ({}).",
                answer.status.as_u16()
            )),
            _ if answer.status.is_server_error() => {
                TokenError::Failed(said("The sign-in service failed."))
            }
            // Busy, or asking to be asked later: it may pass, so the sign-in stands.
            "temporarily_unavailable" | "slow_down" => {
                TokenError::Failed(said("The sign-in service is busy."))
            }
            _ if answer.status == StatusCode::TOO_MANY_REQUESTS => {
                TokenError::Failed(said("The sign-in service is busy."))
            }
            _ => TokenError::Refused(said(&format!(
                "The sign-in service refused ({}).",
                if error.is_empty() {
                    answer.status.as_str().to_owned()
                } else {
                    shown(error)
                }
            ))),
        });
    }
    checked(&body, expected, SystemTime::now()).map_err(|why| TokenError::Unusable {
        why,
        refresh_token: refresh_token(&body),
    })
}

/// The refresh token an answer gives, if any.
fn refresh_token(body: &Map<String, Value>) -> Option<String> {
    body.get("refresh_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

/// The answer's tokens, if they may be used: a `Bearer` access token that lives at most 300
/// seconds, issued by the expected issuer for exactly the resource, to Studio's client ID, and
/// in the target's organization when it names one.
fn checked(
    body: &Map<String, Value>,
    expected: Expected<'_>,
    now: SystemTime,
) -> Result<Tokens, String> {
    let bearer = body
        .get("token_type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("bearer"));
    if !bearer {
        return Err("The sign-in service gave a token that is not a bearer token.".into());
    }
    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty() && token.len() <= MAX_TOKEN_BYTES && token.is_ascii())
        .ok_or("The sign-in service gave no access token Studio can send.")?;
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_u64)
        .ok_or("The sign-in service did not say when its token expires.")?;
    if expires_in == 0 || expires_in > MAX_ACCESS_SECONDS {
        return Err(format!(
            "The sign-in service gave a token that lasts {expires_in} seconds; the server takes \
             at most {MAX_ACCESS_SECONDS}."
        ));
    }
    let claims =
        claims(access_token).ok_or("The sign-in service gave a token Studio cannot read.")?;
    match claims.get("aud") {
        Some(Value::String(audience)) if audience == expected.resource => {}
        Some(Value::Array(_)) => {
            return Err(
                "The sign-in service gave a token for several audiences; the server takes one."
                    .into(),
            );
        }
        _ => return Err("The sign-in service gave a token issued for another resource.".into()),
    }
    if claims.get("iss").and_then(Value::as_str) != Some(expected.issuer) {
        return Err("The sign-in service gave a token another service issued.".into());
    }
    if claims.get("client_id").and_then(Value::as_str) != Some(expected.client_id) {
        return Err("The sign-in service gave a token issued to another client.".into());
    }
    let subject = claims
        .get("sub")
        .and_then(Value::as_str)
        .filter(|subject| !subject.is_empty())
        .ok_or("The sign-in service gave a token that names no account.")?;
    let organization = claims
        .get("org_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(wanted) = expected.organization
        && organization.as_deref() != Some(wanted)
    {
        return Err(format!(
            "The sign-in was for {}, and this target is in {wanted}.",
            organization
                .as_deref()
                .map_or("no organization".to_owned(), shown)
        ));
    }
    // The server checks `exp` against its own clock. Measured against this machine's, a clock
    // ahead would shorten every token, renewing it in a loop, or refuse the sign-in outright.
    if claims.get("exp").and_then(Value::as_u64).is_none() {
        return Err("The sign-in service gave a token that does not say when it expires.".into());
    }
    let lives = Duration::from_secs(expires_in);
    Ok(Tokens {
        access_token: access_token.to_owned(),
        refresh_token: refresh_token(body),
        expires_at: now + lives,
        account: Account {
            subject: subject.to_owned(),
            organization,
        },
    })
}

/// A JWT's claims, read without checking its signature.
fn claims(token: &str) -> Option<Map<String, Value>> {
    let mut parts = token.split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    match serde_json::from_slice(&bytes).ok()? {
        Value::Object(claims) => Some(claims),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use super::*;
    use serde_json::json;

    const ISSUER: &str = "https://id.example";
    const RESOURCE: &str = "https://graphs.example/mcp";
    const CLIENT: &str = "https://studio.example/client.json";

    fn jwt(claims: Value) -> String {
        format!(
            "{}.{}.c2ln",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    fn expected(organization: Option<&'static str>) -> Expected<'static> {
        Expected {
            issuer: ISSUER,
            resource: RESOURCE,
            client_id: CLIENT,
            organization,
        }
    }

    fn answer(claims: Value, expires_in: u64) -> Map<String, Value> {
        let Value::Object(body) = json!({
            "token_type": "Bearer",
            "access_token": jwt(claims),
            "expires_in": expires_in,
            "refresh_token": "r2",
        }) else {
            unreachable!()
        };
        body
    }

    fn good(now: u64) -> Value {
        json!({"iss": ISSUER, "aud": RESOURCE, "client_id": CLIENT, "sub": "user_1", "org_id": "org_1", "iat": now, "exp": now + 300})
    }

    #[test]
    fn a_token_is_used_only_when_it_was_issued_for_this_resource_and_client() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let secs = 1_000_000;
        let tokens = checked(&answer(good(secs), 300), expected(Some("org_1")), now).unwrap();
        assert_eq!(tokens.account.subject, "user_1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("r2"));
        assert_eq!(tokens.expires_at, now + Duration::from_secs(300));

        // This machine's clock ten minutes ahead of the service's: the token still lives its
        // `expires_in`, and is not refused as expired.
        let mut behind = good(secs);
        behind["exp"] = json!(secs - 600);
        let tokens = checked(&answer(behind, 300), expected(None), now).unwrap();
        assert_eq!(tokens.expires_at, now + Duration::from_secs(300));

        let refused = |claims: Value, expires_in: u64, organization| {
            checked(&answer(claims, expires_in), expected(organization), now).unwrap_err()
        };
        let mut other = good(secs);
        other["aud"] = json!("https://graphs.example/");
        assert!(refused(other, 300, None).contains("another resource"));
        let mut several = good(secs);
        several["aud"] = json!([RESOURCE]);
        assert!(refused(several, 300, None).contains("several audiences"));
        let mut issuer = good(secs);
        issuer["iss"] = json!("https://evil.example");
        assert!(refused(issuer, 300, None).contains("another service"));
        let mut client = good(secs);
        client["client_id"] = json!("cli");
        assert!(refused(client, 300, None).contains("another client"));
        assert!(refused(good(secs), 301, None).contains("at most 300"));
        assert!(
            refused(good(secs), 300, Some("org_2")).contains("org_1, and this target is in org_2")
        );
        let mut no_expiry = good(secs);
        no_expiry.as_object_mut().unwrap().remove("exp");
        assert!(refused(no_expiry, 300, None).contains("does not say when it expires"));
    }
}
