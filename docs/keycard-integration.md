# Keycard Provider Integration

This guide explains how to integrate OpenShell with [Keycard](https://keycard.ai) for SPIFFE-based, per-sandbox credential management.

## Overview

The Keycard provider enables automatic, ephemeral credential provisioning for each sandbox. Instead of injecting static API keys, OpenShell uses your Keycard admin credentials to:

1. Create a unique Keycard APPLICATION identity per sandbox (with a SPIFFE ID of `spiffe://{zone_id}/sandbox/{sandbox_id}`)
2. Generate ephemeral password credentials scoped to that sandbox's lifetime
3. Exchange those credentials for short-lived access tokens at runtime (e.g., actual API keys for Anthropic, OpenAI, etc.)
4. Automatically delete the Keycard APPLICATION when the sandbox is destroyed

Admin credentials never leave the gateway server. Only per-sandbox credentials are injected into the sandbox environment.

## Prerequisites

You need the following from your Keycard account:

| Value | Description |
|-------|-------------|
| `base_url` | Keycard API base URL (e.g., `https://api.keycard.ai`) |
| `zone_id` | Your Keycard zone identifier |
| `client_id` | Admin client ID for API authentication |
| `client_secret` | Admin client secret for API authentication |

## Create the Provider

Register a Keycard provider with your admin credentials:

```bash
openshell provider create \
  --type keycard \
  --name keyvengers \
  --config base_url=https://api.keycard.ai \
  --config zone_id=<your-zone-id> \
  --config client_id=<your-client-id> \
  --config client_secret=<your-client-secret>
```

All four `--config` values are required. The provider name (`keyvengers` above) is arbitrary and is how you reference this provider when creating sandboxes.

Verify the provider was created:

```bash
openshell provider list
openshell provider get keyvengers
```

## Create a Sandbox with the Provider

Attach the Keycard provider to a sandbox using `--provider`:

```bash
openshell sandbox create \
  --name my-sandbox \
  --provider keyvengers \
  -- claude
```

When the sandbox starts, OpenShell automatically provisions a Keycard APPLICATION and injects the per-sandbox `KEYCARD_CLIENT_ID` and `KEYCARD_CLIENT_SECRET` into the sandbox environment. These credentials are unique to this sandbox and are deleted when the sandbox is destroyed.

### With Secrets

If the sandbox needs access to specific resources through Keycard token exchange, pass secrets with `--secret`:

```bash
openshell sandbox create \
  --name my-sandbox \
  --provider keyvengers \
  --secret ANTHROPIC_API_KEY=urn:resource:anthropic-api-key \
  -- claude
```

Secrets require at least one Keycard provider attached to the sandbox. The gateway resolves secrets at runtime by exchanging the per-sandbox credentials for access tokens scoped to the requested resource URN.

## Provider Configuration Reference

| Config Key | Required | Description |
|-----------|----------|-------------|
| `base_url` | Yes | Keycard API base URL |
| `zone_id` | Yes | Keycard zone identifier |
| `client_id` | Yes | Admin client ID (used server-side only for API calls) |
| `client_secret` | Yes | Admin client secret (used server-side only for API calls) |

## Lifecycle

| Event | What Happens |
|-------|-------------|
| Sandbox created | Keycard APPLICATION created with SPIFFE ID `spiffe://{zone_id}/sandbox/{sandbox_id}`, ephemeral credentials generated |
| Sandbox running | Per-sandbox credentials available for token exchange against resource URNs |
| Sandbox deleted | Keycard APPLICATION and credentials are deleted via the Keycard API |
| Credential failure during creation | APPLICATION is cleaned up automatically, sandbox creation aborted |

## Troubleshooting

### "sandbox has secrets but no keycard provider attached"

You passed `--secret` flags but no `--provider` pointing to a Keycard provider. Add `--provider <name>` where `<name>` is a Keycard-type provider.

### "keycard provider missing required config keys"

One or more of the four required config keys (`base_url`, `zone_id`, `client_id`, `client_secret`) was not provided when the provider was created. Delete and recreate the provider with all four values.

### "keycard provisioning failed"

The gateway could not reach the Keycard API or the admin credentials were rejected. Check that:

- The `base_url` is correct and reachable from the gateway container
- The `client_id` and `client_secret` are valid
- The `zone_id` matches your Keycard account

If the gateway runs inside Docker, the Keycard API URL must be reachable from inside the container. Use a network policy that allows egress to the Keycard API host, or run with a permissive policy during initial setup.

### Updating Provider Credentials

To rotate your admin credentials, delete and recreate the provider:

```bash
openshell provider delete keyvengers
openshell provider create \
  --type keycard \
  --name keyvengers \
  --config base_url=https://api.keycard.ai \
  --config zone_id=<your-zone-id> \
  --config client_id=<new-client-id> \
  --config client_secret=<new-client-secret>
```

Existing sandboxes that were provisioned with the old credentials continue to work until they are deleted. New sandboxes will use the updated credentials.
