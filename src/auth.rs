use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use chrono::Utc;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

// ---------------------------------------------------------------------------
// JWT
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    /// Lowercase wallet address
    pub address: String,
    /// admin or user
    pub role: String,
    /// Expiration (unix timestamp)
    pub exp: usize,
    /// Issued at
    pub iat: usize,
    /// Unique token ID
    pub jti: String,
}

/// Generate a JWT token for an authenticated user.
pub fn create_jwt(address: &str, role: &str, secret: &str, ttl_hours: u64) -> AppResult<String> {
    let now = Utc::now().timestamp() as usize;
    let claims = Claims {
        address: address.to_string(),
        role: role.to_string(),
        exp: now + (ttl_hours as usize) * 3600,
        iat: now,
        jti: Uuid::new_v4().to_string(),
    };

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| AppError::AuthError(format!("JWT encode error: {}", e)))?;

    Ok(token)
}

/// Decode and validate a JWT token.
pub fn verify_jwt(token: &str, secret: &str) -> AppResult<Claims> {
    let token_data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|e| AppError::AuthError(format!("JWT decode error: {}", e)))?;

    Ok(token_data.claims)
}

// ---------------------------------------------------------------------------
// Axum Auth Extractor
// ---------------------------------------------------------------------------

/// Extracted user info from JWT for use in route handlers.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub address: String,
    pub role: String,
}

impl AuthUser {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}

// Implement FromRequestParts so AuthUser can be used directly in handlers.
// Reads JWT from Authorization header and validates against AppState's secret.
impl FromRequestParts<crate::models::AppState> for AuthUser
{
    type Rejection = (StatusCode, axum::Json<serde_json::Value>);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &crate::models::AppState,
    ) -> Result<Self, Self::Rejection> {
        // Extract the Authorization header
        let header_value = parts.headers.get("Authorization").cloned();

        let bearer_token = header_value
            .and_then(|v| {
                let s = v.to_str().ok()?;
                s.strip_prefix("Bearer ").map(|s| s.to_string())
            })
            .ok_or_else(|| {
                (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({
                        "error": "Missing or invalid Authorization header",
                        "code": 401
                    })),
                )
            })?;

        let claims = verify_jwt(&bearer_token, &state.jwt_secret).map_err(|e| {
            (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({
                    "error": e.to_string(),
                    "code": 401
                })),
            )
        })?;

        Ok(AuthUser {
            address: claims.address,
            role: claims.role,
        })
    }
}

// ---------------------------------------------------------------------------
// Admin-only guard
// ---------------------------------------------------------------------------

/// Verify that an AuthUser has admin privileges.
pub fn require_admin(user: &AuthUser) -> AppResult<()> {
    if !user.is_admin() {
        return Err(AppError::Forbidden("Admin privileges required".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SIWE verification helpers
// ---------------------------------------------------------------------------

/// Verify a SIWE (Sign-In with Ethereum) message and signature.
pub async fn verify_siwe(
    message: &str,
    signature: &str,
    expected_nonce: &str,
) -> AppResult<String> {
    let siwe_msg: siwe::Message = message.parse().map_err(|e| {
        AppError::AuthError(format!("Invalid SIWE message: {}", e))
    })?;

    // Validate nonce matches
    if siwe_msg.nonce != expected_nonce {
        return Err(AppError::AuthError("SIWE nonce mismatch".into()));
    }

    // Validate the SIWE message hasn't expired (siwe::TimeStamp derefs to time::OffsetDateTime)
    if let Some(ref exp) = siwe_msg.expiration_time {
        let exp_ts: i64 = exp.as_ref().unix_timestamp();
        if chrono::Utc::now().timestamp() > exp_ts {
            return Err(AppError::AuthError("SIWE message expired".into()));
        }
    }

    let signature_bytes = hex::decode(signature.trim_start_matches("0x")).map_err(|e| {
        AppError::AuthError(format!("Invalid signature hex: {}", e))
    })?;

    siwe_msg
        .verify(&signature_bytes, &siwe::VerificationOpts::default())
        .await
        .map_err(|e| AppError::AuthError(format!("SIWE verification failed: {}", e)))?;

    let address = format!("0x{}", hex::encode(siwe_msg.address)).to_lowercase();
    Ok(address)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jwt_roundtrip() {
        let secret = "test-secret-key-12345";
        let token = create_jwt("0xabc123", "user", secret, 24).unwrap();
        let claims = verify_jwt(&token, secret).unwrap();
        assert_eq!(claims.address, "0xabc123");
        assert_eq!(claims.role, "user");
    }

    #[test]
    fn test_jwt_invalid_token() {
        let result = verify_jwt("not.a.valid.token", "secret");
        assert!(result.is_err());
    }

    #[test]
    fn test_jwt_wrong_secret() {
        let token = create_jwt("0xabc", "user", "secret-a", 24).unwrap();
        let result = verify_jwt(&token, "secret-b");
        assert!(result.is_err());
    }

    #[test]
    fn test_jwt_expiration() {
        let secret = "test-secret";
        // Create a token that expired 1 hour ago
        let now = chrono::Utc::now().timestamp() as usize;
        let claims = Claims {
            address: "0xabc".into(),
            role: "user".into(),
            exp: now.saturating_sub(3600), // expired 1 hour ago
            iat: now.saturating_sub(7200),
            jti: uuid::Uuid::new_v4().to_string(),
        };
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();
        let result = verify_jwt(&token, secret);
        assert!(result.is_err(), "Expected expired token to fail validation");
    }

    #[test]
    fn test_auth_user_is_admin() {
        let user = AuthUser {
            address: "0xadmin".into(),
            role: "admin".into(),
        };
        assert!(user.is_admin());

        let user2 = AuthUser {
            address: "0xuser".into(),
            role: "user".into(),
        };
        assert!(!user2.is_admin());
    }

    #[test]
    fn test_require_admin() {
        let admin = AuthUser { address: "0xadmin".into(), role: "admin".into() };
        assert!(require_admin(&admin).is_ok());

        let user = AuthUser { address: "0xuser".into(), role: "user".into() };
        assert!(require_admin(&user).is_err());
    }
}
