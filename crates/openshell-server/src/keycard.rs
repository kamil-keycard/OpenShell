// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Keycard API client and ephemeral per-sandbox credential store.
//!
//! The Keycard integration creates a unique APPLICATION identity per sandbox via
//! the Keycard API, then generates ephemeral password credentials scoped to that
//! sandbox's lifetime. Admin credentials (used to call the Keycard API) never
//! leave the server — only the per-sandbox credentials are injected into the
//! sandbox environment.

use openshell_providers::providers::keycard as keycard_provider;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Keycard API types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct CreateApplicationRequest {
    identifier: String,
    name: String,
}

/// Matches `iam_Application` — we only need `id` and `identifier`.
#[derive(Debug, Deserialize)]
struct ApplicationResponse {
    id: String,
    identifier: String,
}

/// Matches `iam_ApplicationCredentialCreatePassword`.
#[derive(Debug, Serialize)]
struct CreateCredentialRequest {
    application_id: String,
    #[serde(rename = "type")]
    credential_type: String,
}

/// Matches `iam_ApplicationCredentialPassword`.
/// `password` is only returned on creation.
#[derive(Debug, Deserialize)]
struct CredentialResponse {
    identifier: String,
    password: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
}

// ---------------------------------------------------------------------------
// Keycard API client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct KeycardConfig {
    pub base_url: String,
    pub zone_id: String,
    pub admin_client_id: String,
    pub admin_client_secret: String,
}

impl KeycardConfig {
    /// Extract Keycard configuration from a provider's config map.
    ///
    /// Returns `None` if any required config key is missing.
    pub fn from_provider_config(config: &HashMap<String, String>) -> Option<Self> {
        Some(Self {
            base_url: config.get(keycard_provider::CONFIG_BASE_URL)?.clone(),
            zone_id: config.get(keycard_provider::CONFIG_ZONE_ID)?.clone(),
            admin_client_id: config.get(keycard_provider::CONFIG_CLIENT_ID)?.clone(),
            admin_client_secret: config.get(keycard_provider::CONFIG_CLIENT_SECRET)?.clone(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct KeycardClient {
    http: reqwest::Client,
    config: KeycardConfig,
}

/// Result of provisioning a Keycard application for a sandbox.
#[derive(Debug, Clone)]
pub struct ProvisionedApplication {
    /// Keycard-internal application ID (used for deletion).
    pub application_id: String,
    /// Application identifier (the SPIFFE ID we set on creation).
    pub identifier: String,
    /// Per-sandbox client ID for authentication.
    pub client_id: String,
    /// Per-sandbox client secret for authentication.
    pub client_secret: String,
}

#[derive(Debug, thiserror::Error)]
pub enum KeycardError {
    #[error("keycard API request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("keycard API returned {status}: {body}")]
    Api { status: u16, body: String },

    #[error("missing keycard config key: {0}")]
    MissingConfig(String),

    #[error("keycard credential response missing password")]
    MissingPassword,
}

impl KeycardClient {
    pub fn new(config: KeycardConfig) -> Result<Self, KeycardError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self { http, config })
    }

    /// Build the SPIFFE ID for a sandbox application.
    ///
    /// Format: `spiffe://{zone_id}/sandbox/{sandbox_id}`
    fn spiffe_id(&self, sandbox_id: &str) -> String {
        format!("spiffe://{}/sandbox/{}", self.config.zone_id, sandbox_id)
    }

    /// Exchange admin credentials for a short-lived Bearer token via the
    /// OAuth2 client_credentials grant.
    async fn authenticate(&self) -> Result<String, KeycardError> {
        let url = format!("{}/service-account-token", self.config.base_url);

        let response = self
            .http
            .post(&url)
            .basic_auth(
                &self.config.admin_client_id,
                Some(&self.config.admin_client_secret),
            )
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body("grant_type=client_credentials")
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(KeycardError::Api {
                status: status.as_u16(),
                body,
            });
        }

        let token: TokenResponse = response.json().await?;
        Ok(token.access_token)
    }

    /// Create a Keycard APPLICATION and generate password credentials for a sandbox.
    ///
    /// Returns the provisioned application details including the ephemeral credentials.
    pub async fn provision_sandbox(
        &self,
        sandbox_id: &str,
        sandbox_name: &str,
    ) -> Result<ProvisionedApplication, KeycardError> {
        let spiffe_id = self.spiffe_id(sandbox_id);
        let token = self.authenticate().await?;

        debug!(
            sandbox_id = %sandbox_id,
            spiffe_id = %spiffe_id,
            sandbox_name = %sandbox_name,
            "Creating Keycard application"
        );

        let app = self
            .create_application(&token, &spiffe_id, sandbox_id, sandbox_name)
            .await?;

        info!(
            sandbox_id = %sandbox_id,
            application_id = %app.id,
            identifier = %app.identifier,
            "Keycard application created"
        );

        let cred = match self.create_credential(&token, &app.id).await {
            Ok(cred) => cred,
            Err(e) => {
                warn!(
                    sandbox_id = %sandbox_id,
                    application_id = %app.id,
                    error = %e,
                    "Failed to create Keycard credential, attempting application cleanup"
                );
                if let Err(cleanup_err) = self.delete_application(&app.id).await {
                    warn!(
                        application_id = %app.id,
                        error = %cleanup_err,
                        "Failed to clean up Keycard application after credential failure"
                    );
                }
                return Err(e);
            }
        };

        let client_secret = cred.password.ok_or(KeycardError::MissingPassword)?;

        info!(
            sandbox_id = %sandbox_id,
            application_id = %app.id,
            "Keycard credential created"
        );

        Ok(ProvisionedApplication {
            application_id: app.id,
            identifier: app.identifier,
            client_id: cred.identifier,
            client_secret,
        })
    }

    /// Delete a Keycard APPLICATION by its internal ID.
    pub async fn delete_application(&self, application_id: &str) -> Result<(), KeycardError> {
        let token = self.authenticate().await?;
        let url = format!(
            "{}/zones/{}/applications/{}",
            self.config.base_url, self.config.zone_id, application_id
        );

        let response = self
            .http
            .delete(&url)
            .bearer_auth(&token)
            .header(ACCEPT, "application/json")
            .send()
            .await?;

        let status = response.status();
        if status.is_success() || status.as_u16() == 404 {
            Ok(())
        } else {
            let body = response.text().await.unwrap_or_default();
            Err(KeycardError::Api {
                status: status.as_u16(),
                body,
            })
        }
    }

    async fn create_application(
        &self,
        token: &str,
        spiffe_id: &str,
        _sandbox_id: &str,
        sandbox_name: &str,
    ) -> Result<ApplicationResponse, KeycardError> {
        let url = format!(
            "{}/zones/{}/applications",
            self.config.base_url, self.config.zone_id
        );

        let body = CreateApplicationRequest {
            identifier: spiffe_id.to_string(),
            name: sandbox_name.to_string(),
        };

        let response = self
            .http
            .post(&url)
            .bearer_auth(token)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(KeycardError::Api {
                status: status.as_u16(),
                body,
            });
        }

        Ok(response.json().await?)
    }

    async fn create_credential(
        &self,
        token: &str,
        application_id: &str,
    ) -> Result<CredentialResponse, KeycardError> {
        let url = format!(
            "{}/zones/{}/application-credentials",
            self.config.base_url, self.config.zone_id
        );

        let body = CreateCredentialRequest {
            application_id: application_id.to_string(),
            credential_type: "password".to_string(),
        };

        let response = self
            .http
            .post(&url)
            .bearer_auth(token)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(KeycardError::Api {
                status: status.as_u16(),
                body,
            });
        }

        Ok(response.json().await?)
    }
}

// ---------------------------------------------------------------------------
// Ephemeral per-sandbox credential store
// ---------------------------------------------------------------------------

/// Per-sandbox Keycard credentials scoped to the sandbox lifetime.
///
/// These are generated dynamically during sandbox provisioning and removed
/// when the sandbox is deleted. They MUST NOT cross sandbox boundaries.
#[derive(Debug, Clone)]
pub struct SandboxKeycardCredentials {
    /// Keycard-internal application ID (for cleanup on sandbox deletion).
    pub application_id: String,
    /// Provider name this credential belongs to.
    pub provider_name: String,
    /// Per-sandbox client ID injected as KEYCARD_CLIENT_ID.
    pub client_id: String,
    /// Per-sandbox client secret injected as KEYCARD_CLIENT_SECRET.
    pub client_secret: String,
}

/// Thread-safe ephemeral store for per-sandbox Keycard credentials.
///
/// Credentials live only in memory and are scoped to the sandbox lifetime.
/// The store is keyed by sandbox ID — each sandbox gets exactly one set of
/// Keycard credentials. Credentials are inserted during sandbox provisioning
/// and removed during sandbox deletion.
#[derive(Debug, Clone, Default)]
pub struct KeycardCredentialStore {
    inner: Arc<RwLock<HashMap<String, SandboxKeycardCredentials>>>,
}

impl KeycardCredentialStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Store credentials for a sandbox. Overwrites any existing entry.
    pub async fn insert(&self, sandbox_id: String, credentials: SandboxKeycardCredentials) {
        self.inner.write().await.insert(sandbox_id, credentials);
    }

    /// Retrieve credentials for a sandbox.
    pub async fn get(&self, sandbox_id: &str) -> Option<SandboxKeycardCredentials> {
        self.inner.read().await.get(sandbox_id).cloned()
    }

    /// Remove and return credentials for a sandbox.
    pub async fn remove(&self, sandbox_id: &str) -> Option<SandboxKeycardCredentials> {
        self.inner.write().await.remove(sandbox_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycard_config_from_provider_config() {
        let mut config = HashMap::new();
        config.insert(
            "base_url".to_string(),
            "https://keycard.example.com".to_string(),
        );
        config.insert("zone_id".to_string(), "zone-001".to_string());
        config.insert("client_id".to_string(), "admin-id".to_string());
        config.insert("client_secret".to_string(), "admin-secret".to_string());

        let kc = KeycardConfig::from_provider_config(&config).unwrap();
        assert_eq!(kc.base_url, "https://keycard.example.com");
        assert_eq!(kc.zone_id, "zone-001");
        assert_eq!(kc.admin_client_id, "admin-id");
        assert_eq!(kc.admin_client_secret, "admin-secret");
    }

    #[test]
    fn keycard_config_returns_none_on_missing_key() {
        let mut config = HashMap::new();
        config.insert(
            "base_url".to_string(),
            "https://keycard.example.com".to_string(),
        );
        // Missing zone_id, client_id, client_secret
        assert!(KeycardConfig::from_provider_config(&config).is_none());
    }

    #[test]
    fn spiffe_id_format() {
        let config = KeycardConfig {
            base_url: "https://keycard.example.com".to_string(),
            zone_id: "zone-abc".to_string(),
            admin_client_id: "admin".to_string(),
            admin_client_secret: "secret".to_string(),
        };
        let client = KeycardClient::new(config).unwrap();
        assert_eq!(
            client.spiffe_id("sandbox-123"),
            "spiffe://zone-abc/sandbox/sandbox-123"
        );
    }

    #[tokio::test]
    async fn credential_store_insert_get_remove() {
        let store = KeycardCredentialStore::new();

        let creds = SandboxKeycardCredentials {
            application_id: "app-1".to_string(),
            provider_name: "my-keycard".to_string(),
            client_id: "client-1".to_string(),
            client_secret: "secret-1".to_string(),
        };

        store.insert("sandbox-1".to_string(), creds.clone()).await;

        let fetched = store.get("sandbox-1").await.unwrap();
        assert_eq!(fetched.client_id, "client-1");
        assert_eq!(fetched.client_secret, "secret-1");
        assert_eq!(fetched.application_id, "app-1");

        let removed = store.remove("sandbox-1").await.unwrap();
        assert_eq!(removed.client_id, "client-1");

        assert!(store.get("sandbox-1").await.is_none());
    }

    #[tokio::test]
    async fn credential_store_get_nonexistent_returns_none() {
        let store = KeycardCredentialStore::new();
        assert!(store.get("no-such-sandbox").await.is_none());
    }

    #[tokio::test]
    async fn credential_store_does_not_cross_sandboxes() {
        let store = KeycardCredentialStore::new();

        store
            .insert(
                "sandbox-a".to_string(),
                SandboxKeycardCredentials {
                    application_id: "app-a".to_string(),
                    provider_name: "kc".to_string(),
                    client_id: "id-a".to_string(),
                    client_secret: "secret-a".to_string(),
                },
            )
            .await;
        store
            .insert(
                "sandbox-b".to_string(),
                SandboxKeycardCredentials {
                    application_id: "app-b".to_string(),
                    provider_name: "kc".to_string(),
                    client_id: "id-b".to_string(),
                    client_secret: "secret-b".to_string(),
                },
            )
            .await;

        let a = store.get("sandbox-a").await.unwrap();
        let b = store.get("sandbox-b").await.unwrap();
        assert_eq!(a.client_id, "id-a");
        assert_eq!(b.client_id, "id-b");
        assert_ne!(a.client_id, b.client_id);
        assert_ne!(a.client_secret, b.client_secret);
    }

    // ---- Wiremock-based Keycard HTTP client tests ----

    mod wiremock_tests {
        use super::*;
        use wiremock::matchers::{body_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const TEST_TOKEN: &str = "test-bearer-token";

        fn test_config(base_url: &str) -> KeycardConfig {
            KeycardConfig {
                base_url: base_url.to_string(),
                zone_id: "zone-test".to_string(),
                admin_client_id: "admin-id".to_string(),
                admin_client_secret: "admin-secret".to_string(),
            }
        }

        async fn mock_token_endpoint(mock_server: &MockServer) {
            Mock::given(method("POST"))
                .and(path("/service-account-token"))
                .and(header("content-type", "application/x-www-form-urlencoded"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": TEST_TOKEN,
                    "token_type": "Bearer",
                    "expires_in": 3600
                })))
                .mount(mock_server)
                .await;
        }

        #[tokio::test]
        async fn authenticate_success() {
            let mock_server = MockServer::start().await;
            mock_token_endpoint(&mock_server).await;

            let config = test_config(&mock_server.uri());
            let client = KeycardClient::new(config).unwrap();
            let token = client.authenticate().await.unwrap();
            assert_eq!(token, TEST_TOKEN);
        }

        #[tokio::test]
        async fn authenticate_failure() {
            let mock_server = MockServer::start().await;

            Mock::given(method("POST"))
                .and(path("/service-account-token"))
                .respond_with(ResponseTemplate::new(401).set_body_string("invalid credentials"))
                .expect(1)
                .mount(&mock_server)
                .await;

            let config = test_config(&mock_server.uri());
            let client = KeycardClient::new(config).unwrap();
            let err = client.authenticate().await.unwrap_err();

            match err {
                KeycardError::Api { status, body } => {
                    assert_eq!(status, 401);
                    assert!(body.contains("invalid credentials"));
                }
                other => panic!("expected Api error, got: {other}"),
            }
        }

        #[tokio::test]
        async fn provision_sandbox_success() {
            let mock_server = MockServer::start().await;
            mock_token_endpoint(&mock_server).await;

            Mock::given(method("POST"))
                .and(path("/zones/zone-test/applications"))
                .and(header("authorization", format!("Bearer {TEST_TOKEN}")))
                .and(header("content-type", "application/json"))
                .and(body_json(serde_json::json!({
                    "identifier": "spiffe://zone-test/sandbox/sandbox-001",
                    "name": "fuzzy-kitten"
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "internal-app-id",
                    "organization_id": "org-1",
                    "zone_id": "zone-test",
                    "slug": "sandbox-001",
                    "identifier": "spiffe://zone-test/sandbox/sandbox-001",
                    "name": "fuzzy-kitten",
                    "dependencies_count": 0,
                    "owner_type": "customer",
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:00Z"
                })))
                .expect(1)
                .mount(&mock_server)
                .await;

            Mock::given(method("POST"))
                .and(path("/zones/zone-test/application-credentials"))
                .and(header("authorization", format!("Bearer {TEST_TOKEN}")))
                .and(header("content-type", "application/json"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "cred-1",
                    "organization_id": "org-1",
                    "zone_id": "zone-test",
                    "slug": "cred-sandbox-001",
                    "application_id": "internal-app-id",
                    "type": "password",
                    "identifier": "sandbox-client-id",
                    "password": "sandbox-client-secret",
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:00Z"
                })))
                .expect(1)
                .mount(&mock_server)
                .await;

            let config = test_config(&mock_server.uri());
            let client = KeycardClient::new(config).unwrap();
            let result = client
                .provision_sandbox("sandbox-001", "fuzzy-kitten")
                .await
                .unwrap();

            assert_eq!(result.application_id, "internal-app-id");
            assert_eq!(result.identifier, "spiffe://zone-test/sandbox/sandbox-001");
            assert_eq!(result.client_id, "sandbox-client-id");
            assert_eq!(result.client_secret, "sandbox-client-secret");
        }

        #[tokio::test]
        async fn provision_sandbox_auth_failure() {
            let mock_server = MockServer::start().await;

            Mock::given(method("POST"))
                .and(path("/service-account-token"))
                .respond_with(ResponseTemplate::new(401).set_body_string("bad credentials"))
                .expect(1)
                .mount(&mock_server)
                .await;

            let config = test_config(&mock_server.uri());
            let client = KeycardClient::new(config).unwrap();
            let err = client
                .provision_sandbox("sandbox-auth-fail", "auth-fail-name")
                .await
                .unwrap_err();

            match err {
                KeycardError::Api { status, .. } => assert_eq!(status, 401),
                other => panic!("expected Api error, got: {other}"),
            }
        }

        #[tokio::test]
        async fn provision_sandbox_app_creation_failure() {
            let mock_server = MockServer::start().await;
            mock_token_endpoint(&mock_server).await;

            Mock::given(method("POST"))
                .and(path("/zones/zone-test/applications"))
                .and(header("authorization", format!("Bearer {TEST_TOKEN}")))
                .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
                .expect(1)
                .mount(&mock_server)
                .await;

            let config = test_config(&mock_server.uri());
            let client = KeycardClient::new(config).unwrap();
            let err = client
                .provision_sandbox("sandbox-fail", "fail-name")
                .await
                .unwrap_err();

            match err {
                KeycardError::Api { status, body } => {
                    assert_eq!(status, 500);
                    assert!(body.contains("internal error"));
                }
                other => panic!("expected Api error, got: {other}"),
            }
        }

        #[tokio::test]
        async fn provision_sandbox_credential_failure_cleans_up_app() {
            let mock_server = MockServer::start().await;
            mock_token_endpoint(&mock_server).await;

            Mock::given(method("POST"))
                .and(path("/zones/zone-test/applications"))
                .and(header("authorization", format!("Bearer {TEST_TOKEN}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "app-to-cleanup",
                    "organization_id": "org-1",
                    "zone_id": "zone-test",
                    "slug": "sandbox-cred-fail",
                    "identifier": "spiffe://zone-test/sandbox/sandbox-cred-fail",
                    "name": "sandbox-cred-fail",
                    "dependencies_count": 0,
                    "owner_type": "customer",
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:00Z"
                })))
                .expect(1)
                .mount(&mock_server)
                .await;

            Mock::given(method("POST"))
                .and(path("/zones/zone-test/application-credentials"))
                .and(header("authorization", format!("Bearer {TEST_TOKEN}")))
                .respond_with(ResponseTemplate::new(500).set_body_string("cred error"))
                .expect(1)
                .mount(&mock_server)
                .await;

            Mock::given(method("DELETE"))
                .and(path("/zones/zone-test/applications/app-to-cleanup"))
                .and(header("authorization", format!("Bearer {TEST_TOKEN}")))
                .respond_with(ResponseTemplate::new(204))
                .expect(1)
                .mount(&mock_server)
                .await;

            let config = test_config(&mock_server.uri());
            let client = KeycardClient::new(config).unwrap();
            let err = client
                .provision_sandbox("sandbox-cred-fail", "cred-fail-name")
                .await
                .unwrap_err();

            match err {
                KeycardError::Api { status, .. } => assert_eq!(status, 500),
                other => panic!("expected Api error, got: {other}"),
            }
        }

        #[tokio::test]
        async fn delete_application_success() {
            let mock_server = MockServer::start().await;
            mock_token_endpoint(&mock_server).await;

            Mock::given(method("DELETE"))
                .and(path("/zones/zone-test/applications/app-123"))
                .and(header("authorization", format!("Bearer {TEST_TOKEN}")))
                .respond_with(ResponseTemplate::new(204))
                .expect(1)
                .mount(&mock_server)
                .await;

            let config = test_config(&mock_server.uri());
            let client = KeycardClient::new(config).unwrap();
            client.delete_application("app-123").await.unwrap();
        }

        #[tokio::test]
        async fn delete_application_not_found_is_ok() {
            let mock_server = MockServer::start().await;
            mock_token_endpoint(&mock_server).await;

            Mock::given(method("DELETE"))
                .and(path("/zones/zone-test/applications/gone"))
                .and(header("authorization", format!("Bearer {TEST_TOKEN}")))
                .respond_with(ResponseTemplate::new(404))
                .expect(1)
                .mount(&mock_server)
                .await;

            let config = test_config(&mock_server.uri());
            let client = KeycardClient::new(config).unwrap();
            client.delete_application("gone").await.unwrap();
        }

        #[tokio::test]
        async fn delete_application_server_error() {
            let mock_server = MockServer::start().await;
            mock_token_endpoint(&mock_server).await;

            Mock::given(method("DELETE"))
                .and(path("/zones/zone-test/applications/app-err"))
                .and(header("authorization", format!("Bearer {TEST_TOKEN}")))
                .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
                .expect(1)
                .mount(&mock_server)
                .await;

            let config = test_config(&mock_server.uri());
            let client = KeycardClient::new(config).unwrap();
            let err = client.delete_application("app-err").await.unwrap_err();

            match err {
                KeycardError::Api { status, .. } => assert_eq!(status, 503),
                other => panic!("expected Api error, got: {other}"),
            }
        }
    }
}
