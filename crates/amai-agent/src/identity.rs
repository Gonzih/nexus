use std::path::PathBuf;

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

// ─── Stored Identity ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentIdentity {
    pub identity_id: String,
    pub name: String,
    pub kid: String,
    pub trust_score: f64,
    /// PEM-encoded Ed25519 private key
    secret_key_pem: String,
    /// PEM-encoded Ed25519 public key
    public_key_pem: String,
}

impl AgentIdentity {
    /// Sign arbitrary bytes, return base64-encoded signature.
    pub fn sign(&self, data: &[u8]) -> String {
        let secret_bytes = self.secret_key_bytes();
        let signing_key = SigningKey::from_bytes(&secret_bytes);
        let signature = signing_key.sign(data);
        BASE64.encode(signature.to_bytes())
    }

    pub fn public_key_pem(&self) -> &str {
        &self.public_key_pem
    }

    pub fn secret_key_bytes(&self) -> [u8; 32] {
        // PEM format: base64 between -----BEGIN/END PRIVATE KEY-----
        let pem_body: String = self
            .secret_key_pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        let der = BASE64.decode(&pem_body).expect("valid PEM base64");
        // PKCS#8 DER wraps the 32-byte seed at the end
        // For ed25519-dalek PEM keys, the raw 32 bytes are at offset 16
        let mut key = [0u8; 32];
        if der.len() == 32 {
            key.copy_from_slice(&der);
        } else if der.len() >= 48 {
            // PKCS#8: 16-byte header + 32-byte key
            key.copy_from_slice(&der[der.len() - 32..]);
        } else {
            panic!("unexpected PEM key length: {}", der.len());
        }
        key
    }
}

// ─── Key Generation ──────────────────────────────────────────────────────────

fn generate_keypair() -> (SigningKey, VerifyingKey) {
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();
    (signing_key, verifying_key)
}

/// Generate kid from public key PEM (matches id-service: "kid_" + SHA256(pem)[..16])
fn compute_kid(public_key_pem: &str) -> String {
    let hash = Sha256::digest(public_key_pem.as_bytes());
    let hex = hex::encode(hash);
    format!("kid_{}", &hex[..16])
}

/// Generate a 32-byte hex-encoded nonce (64 hex chars, matches id-service)
fn generate_nonce() -> String {
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut OsRng, &mut bytes);
    hex::encode(bytes)
}

fn to_secret_pem(key: &SigningKey) -> String {
    let b64 = BASE64.encode(key.to_bytes());
    format!(
        "-----BEGIN PRIVATE KEY-----\n{b64}\n-----END PRIVATE KEY-----"
    )
}

fn to_public_pem(key: &VerifyingKey) -> String {
    let b64 = BASE64.encode(key.to_bytes());
    format!(
        "-----BEGIN PUBLIC KEY-----\n{b64}\n-----END PUBLIC KEY-----"
    )
}

// ─── Disk Persistence ────────────────────────────────────────────────────────

fn identity_dir(base: &Option<String>) -> PathBuf {
    if let Some(dir) = base {
        PathBuf::from(dir)
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".amai")
            .join("identity")
    }
}

fn identity_path(base: &Option<String>) -> PathBuf {
    identity_dir(base).join("agent_identity.json")
}

pub fn load_identity(key_dir: &Option<String>) -> Option<AgentIdentity> {
    let path = identity_path(key_dir);
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_identity(identity: &AgentIdentity, key_dir: &Option<String>) -> Result<(), String> {
    let dir = identity_dir(key_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create identity dir: {e}"))?;
    let path = dir.join("agent_identity.json");
    let json = serde_json::to_string_pretty(identity)
        .map_err(|e| format!("Failed to serialize identity: {e}"))?;
    std::fs::write(&path, json)
        .map_err(|e| format!("Failed to write identity: {e}"))?;

    // Restrict permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(&path, perms);
    }

    Ok(())
}

// ─── Registration with id-service ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct IdServiceResponse<T> {
    success: bool,
    data: Option<T>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegisterData {
    identity: RegisteredIdentity,
}

#[derive(Debug, Deserialize)]
struct RegisteredIdentity {
    id: String,
    name: String,
    trust_score: f64,
}

/// Load existing identity from disk, or generate a new keypair and register
/// with the id-service. Returns the identity for use in the agent session.
pub async fn load_or_register(
    id_service_url: &str,
    agent_name: &str,
    key_dir: &Option<String>,
) -> Result<AgentIdentity, String> {
    // Try loading from disk first
    if let Some(identity) = load_identity(key_dir) {
        tracing::info!(
            identity_id = %identity.identity_id,
            name = %identity.name,
            kid = %identity.kid,
            "Loaded existing identity"
        );
        return Ok(identity);
    }

    tracing::info!(name = %agent_name, "No identity found — generating keypair and registering");

    // Generate Ed25519 keypair
    let (signing_key, verifying_key) = generate_keypair();
    let secret_pem = to_secret_pem(&signing_key);
    let public_pem = to_public_pem(&verifying_key);

    // Build registration payload
    let timestamp = chrono::Utc::now().to_rfc3339();
    let nonce = generate_nonce();

    // Sign the registration: sign("name|timestamp|nonce") — pipe-separated
    let sign_payload = format!("{agent_name}|{timestamp}|{nonce}");
    let signature = signing_key.sign(sign_payload.as_bytes());
    let sig_b64 = BASE64.encode(signature.to_bytes());

    let body = serde_json::json!({
        "name": agent_name,
        "public_key": public_pem,
        "key_type": "ed25519",
        "signature": sig_b64,
        "timestamp": timestamp,
        "nonce": nonce,
    });

    let client = reqwest::Client::new();
    let url = format!("{}/register", id_service_url.trim_end_matches('/'));

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("id-service request failed: {e}"))?;

    let status = resp.status();
    let resp_body: IdServiceResponse<RegisterData> = resp
        .json()
        .await
        .map_err(|e| format!("id-service response parse error: {e}"))?;

    if !resp_body.success || resp_body.data.is_none() {
        let err_msg = resp_body.error.unwrap_or_else(|| format!("HTTP {status}"));
        return Err(format!("id-service registration failed: {err_msg}"));
    }

    let reg = resp_body.data.unwrap();

    // Derive kid (fingerprint) from public key — matches id-service
    let kid = compute_kid(&public_pem);

    let identity = AgentIdentity {
        identity_id: reg.identity.id,
        name: reg.identity.name,
        kid,
        trust_score: reg.identity.trust_score,
        secret_key_pem: secret_pem,
        public_key_pem: public_pem,
    };

    // Persist to disk
    save_identity(&identity, key_dir)?;
    tracing::info!(
        identity_id = %identity.identity_id,
        name = %identity.name,
        "Identity registered and saved"
    );

    Ok(identity)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypair_generation_and_signing() {
        let (signing_key, verifying_key) = generate_keypair();
        let message = b"test message";
        let signature = signing_key.sign(message);
        assert!(verifying_key.verify_strict(message, &signature).is_ok());
    }

    #[test]
    fn pem_roundtrip() {
        let (signing_key, verifying_key) = generate_keypair();
        let secret_pem = to_secret_pem(&signing_key);
        let public_pem = to_public_pem(&verifying_key);

        assert!(secret_pem.contains("-----BEGIN PRIVATE KEY-----"));
        assert!(public_pem.contains("-----BEGIN PUBLIC KEY-----"));

        // Verify we can reconstruct from PEM
        let identity = AgentIdentity {
            identity_id: "test".into(),
            name: "test".into(),
            kid: "test".into(),
            trust_score: 0.0,
            secret_key_pem: secret_pem,
            public_key_pem: public_pem,
        };

        let recovered_bytes = identity.secret_key_bytes();
        assert_eq!(recovered_bytes, signing_key.to_bytes());
    }

    #[test]
    fn sign_and_verify() {
        let (signing_key, verifying_key) = generate_keypair();
        let identity = AgentIdentity {
            identity_id: "id_123".into(),
            name: "test_agent".into(),
            kid: "kid_123".into(),
            trust_score: 50.0,
            secret_key_pem: to_secret_pem(&signing_key),
            public_key_pem: to_public_pem(&verifying_key),
        };

        let sig_b64 = identity.sign(b"hello world");
        let sig_bytes = BASE64.decode(&sig_b64).unwrap();
        let signature = ed25519_dalek::Signature::from_bytes(
            sig_bytes.as_slice().try_into().unwrap(),
        );
        assert!(verifying_key.verify_strict(b"hello world", &signature).is_ok());
    }

    #[test]
    fn identity_serialization() {
        let (signing_key, verifying_key) = generate_keypair();
        let identity = AgentIdentity {
            identity_id: "id_abc".into(),
            name: "agent_one".into(),
            kid: "kid_abc".into(),
            trust_score: 75.5,
            secret_key_pem: to_secret_pem(&signing_key),
            public_key_pem: to_public_pem(&verifying_key),
        };

        let json = serde_json::to_string(&identity).unwrap();
        let recovered: AgentIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(recovered.identity_id, "id_abc");
        assert_eq!(recovered.name, "agent_one");
        assert_eq!(recovered.trust_score, 75.5);
    }

    #[test]
    fn save_and_load_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_string_lossy().to_string();
        let key_dir = Some(dir);

        let (signing_key, verifying_key) = generate_keypair();
        let identity = AgentIdentity {
            identity_id: "id_test".into(),
            name: "test_agent".into(),
            kid: "kid_test".into(),
            trust_score: 0.0,
            secret_key_pem: to_secret_pem(&signing_key),
            public_key_pem: to_public_pem(&verifying_key),
        };

        save_identity(&identity, &key_dir).unwrap();
        let loaded = load_identity(&key_dir).unwrap();
        assert_eq!(loaded.identity_id, "id_test");
        assert_eq!(loaded.name, "test_agent");
    }

    #[test]
    fn load_nonexistent_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nonexistent").to_string_lossy().to_string();
        let key_dir = Some(dir);
        assert!(load_identity(&key_dir).is_none());
    }
}
