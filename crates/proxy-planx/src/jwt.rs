//! Minimal JWT claim extraction (payload only, **no signature verification**).
//!
//! Signature verification is deliberately out of scope: these tokens are issued
//! by the upstream to the account owner, and we only read them to discover
//! `plan_type` / workspace id. The upstream remains the authority — it rejects
//! anything it considers invalid.

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Claims we care about, regardless of which token they came from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenClaims {
    pub email: Option<String>,
    /// Workspace UUID (`chatgpt_account_id`).
    pub account_id: Option<String>,
    pub user_id: Option<String>,
    /// Raw upstream plan value, e.g. `plus` / `prolite` / `team`.
    pub plan_type: Option<String>,
    pub subscription_expires_at: Option<DateTime<Utc>>,
    /// Standard `exp` claim: when the token itself stops being valid.
    pub expires_at: Option<DateTime<Utc>>,
}

impl TokenClaims {
    pub fn is_empty(&self) -> bool {
        self.email.is_none()
            && self.account_id.is_none()
            && self.plan_type.is_none()
            && self.subscription_expires_at.is_none()
            && self.expires_at.is_none()
    }
}

/// Decode a JWT payload without verifying the signature.
pub fn decode_payload(token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let bytes = decode_base64url(payload)?;
    serde_json::from_slice(&bytes).ok()
}

fn decode_base64url(input: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(input)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(input))
        .ok()
}

/// Parse an id_token. Plan/workspace live under `https://api.openai.com/auth`,
/// the e-mail at the root.
pub fn parse_id_token(token: &str) -> Option<TokenClaims> {
    let value = decode_payload(token)?;
    let auth = value.get("https://api.openai.com/auth");
    Some(TokenClaims {
        email: string_at(&value, "email"),
        account_id: auth.and_then(|a| string_at(a, "chatgpt_account_id")),
        user_id: auth
            .and_then(|a| string_at(a, "user_id").or_else(|| string_at(a, "chatgpt_user_id"))),
        plan_type: auth.and_then(|a| string_at(a, "chatgpt_plan_type")),
        subscription_expires_at: auth
            .and_then(|a| string_at(a, "chatgpt_subscription_active_until"))
            .and_then(|raw| parse_rfc3339(&raw)),
        expires_at: expires_at(&value),
    })
}

/// Parse an access_token. The e-mail lives under `https://api.openai.com/profile`.
pub fn parse_access_token(token: &str) -> Option<TokenClaims> {
    let value = decode_payload(token)?;
    let auth = value.get("https://api.openai.com/auth");
    let profile = value.get("https://api.openai.com/profile");
    Some(TokenClaims {
        email: profile
            .and_then(|p| string_at(p, "email"))
            .or_else(|| string_at(&value, "email")),
        account_id: auth.and_then(|a| string_at(a, "chatgpt_account_id")),
        user_id: auth
            .and_then(|a| string_at(a, "user_id").or_else(|| string_at(a, "chatgpt_user_id"))),
        plan_type: auth.and_then(|a| string_at(a, "chatgpt_plan_type")),
        subscription_expires_at: auth
            .and_then(|a| string_at(a, "chatgpt_subscription_active_until"))
            .and_then(|raw| parse_rfc3339(&raw)),
        expires_at: expires_at(&value),
    })
}

fn string_at(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Standard `exp` claim, in unix seconds (int or float).
fn expires_at(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    let raw = value.get("exp")?;
    let seconds = raw
        .as_i64()
        .or_else(|| raw.as_f64().map(|f| f as i64))
        .or_else(|| raw.as_str().and_then(|s| s.trim().parse().ok()))?;
    DateTime::from_timestamp(seconds, 0)
}

fn parse_rfc3339(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jws(payload: serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"none","typ":"JWT"}"#);
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("{header}.{body}.signature")
    }

    #[test]
    fn id_token_carries_plan_and_workspace() {
        let token = jws(serde_json::json!({
            "email": "u@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "11111111-2222-3333-4444-555555555555",
                "chatgpt_plan_type": "plus",
                "chatgpt_subscription_active_until": "2030-01-02T03:04:05Z"
            }
        }));
        let claims = parse_id_token(&token).unwrap();
        assert_eq!(claims.plan_type.as_deref(), Some("plus"));
        assert_eq!(claims.email.as_deref(), Some("u@example.com"));
        assert_eq!(
            claims.account_id.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert!(claims.subscription_expires_at.is_some());
        assert!(!claims.is_empty());
    }

    #[test]
    fn access_token_email_comes_from_profile() {
        let token = jws(serde_json::json!({
            "https://api.openai.com/profile": { "email": "p@x.y" },
            "https://api.openai.com/auth": { "chatgpt_plan_type": "prolite" }
        }));
        let claims = parse_access_token(&token).unwrap();
        assert_eq!(claims.email.as_deref(), Some("p@x.y"));
        assert_eq!(claims.plan_type.as_deref(), Some("prolite"));
    }

    #[test]
    fn malformed_tokens_yield_none() {
        assert!(parse_id_token("not-a-jwt").is_none());
        assert!(parse_id_token("a.b").is_none());
        assert!(parse_id_token("").is_none());
    }

    #[test]
    fn token_without_openai_auth_block_is_empty() {
        let token = jws(serde_json::json!({ "sub": "x" }));
        let claims = parse_id_token(&token).unwrap();
        assert!(claims.is_empty());
    }

    #[test]
    fn access_token_expiry_is_read_from_exp() {
        // Pre-emptive refresh needs the access token's own expiry; `auth.json`
        // does not carry one.
        let token = jws(serde_json::json!({ "exp": 1_893_456_000_i64 }));
        let claims = parse_access_token(&token).unwrap();
        assert_eq!(
            claims.expires_at.map(|dt| dt.timestamp()),
            Some(1_893_456_000)
        );
    }

    #[test]
    fn float_and_string_exp_are_tolerated() {
        let float = jws(serde_json::json!({ "exp": 1_893_456_000.5_f64 }));
        assert!(parse_access_token(&float).unwrap().expires_at.is_some());
        let text = jws(serde_json::json!({ "exp": "1893456000" }));
        assert!(parse_access_token(&text).unwrap().expires_at.is_some());
        let junk = jws(serde_json::json!({ "exp": "later" }));
        assert!(parse_access_token(&junk).unwrap().expires_at.is_none());
    }
}
