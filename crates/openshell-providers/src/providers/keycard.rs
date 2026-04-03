// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{DiscoveredProvider, ProviderError, ProviderPlugin};

/// Keycard provider for per-sandbox SPIFFE-based service-to-service authentication.
///
/// Unlike other providers, Keycard admin credentials live in the provider `config`
/// map (base_url, zone_id, client_id, client_secret) and are used server-side only.
/// Per-sandbox credentials (KEYCARD_CLIENT_ID, KEYCARD_CLIENT_SECRET) are created
/// dynamically via the Keycard API during sandbox provisioning and stored in an
/// ephemeral credential store scoped to the sandbox lifetime.
pub struct KeycardProvider;

pub const PROVIDER_TYPE: &str = "keycard";

pub const CONFIG_BASE_URL: &str = "base_url";
pub const CONFIG_ZONE_ID: &str = "zone_id";
pub const CONFIG_CLIENT_ID: &str = "client_id";
pub const CONFIG_CLIENT_SECRET: &str = "client_secret";

pub const REQUIRED_CONFIG_KEYS: &[&str] = &[
    CONFIG_BASE_URL,
    CONFIG_ZONE_ID,
    CONFIG_CLIENT_ID,
    CONFIG_CLIENT_SECRET,
];

impl ProviderPlugin for KeycardProvider {
    fn id(&self) -> &'static str {
        PROVIDER_TYPE
    }

    fn discover_existing(&self) -> Result<Option<DiscoveredProvider>, ProviderError> {
        // Keycard providers are configured explicitly via admin credentials —
        // no local discovery is applicable.
        Ok(None)
    }

    fn credential_env_vars(&self) -> &'static [&'static str] {
        // Keycard providers don't define fixed credential env vars. The actual
        // env vars are declared per-sandbox via the `secrets` map on SandboxSpec
        // and resolved server-side via token exchange.
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::KeycardProvider;
    use crate::ProviderPlugin;

    #[test]
    fn keycard_provider_id() {
        let provider = KeycardProvider;
        assert_eq!(provider.id(), "keycard");
    }

    #[test]
    fn keycard_provider_discovery_returns_none() {
        let provider = KeycardProvider;
        let discovered = provider.discover_existing().expect("discovery");
        assert!(discovered.is_none());
    }

    #[test]
    fn keycard_provider_credential_env_vars_is_empty() {
        let provider = KeycardProvider;
        let vars = provider.credential_env_vars();
        assert!(vars.is_empty());
    }
}
