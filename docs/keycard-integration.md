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

## Declaring Secrets in the Policy File

All secret bindings (env var secrets, file secret mounts, and the Keycard provider) are declared in the policy YAML. The policy is the single source of truth for sandbox secret configuration.

### Policy-only sandbox creation

```yaml
# policy.yaml
version: 1

secrets:
  provider: keyvengers
  env:
    ANTHROPIC_API_KEY: "urn:secret:claude-api"

secret_mounts:
  - source_urn: "urn:secret-b64:ssh:private-key"
    target_path: "/sandbox/.ssh/id_ed25519"
    mode: "0600"

filesystem_policy:
  # ...
network_policies:
  # ...
```

```bash
openshell sandbox create \
  --name my-sandbox \
  --policy policy.yaml \
  -- claude
```

The gateway extracts the Keycard provider, env var secrets, and file secret mounts from the policy at sandbox creation time.

### secrets block reference

| Field | Type | Description |
|-------|------|-------------|
| `provider` | string | Name of the Keycard provider on the gateway. Required when `env` is non-empty. |
| `env` | map | Env var name to Keycard resource URN. Each entry becomes an environment variable in the sandbox. |

### secret_mounts reference

| Field | Type | Description |
|-------|------|-------------|
| `source_urn` | string | Keycard resource URN (e.g., `urn:secret-b64:ssh:private-key`). Required. |
| `target_path` | string | Absolute path inside the sandbox where the secret is written. Required. |
| `mode` | string | Unix file mode (e.g., `"0600"`). Defaults to `"0600"` when empty. |

Both `secrets` and `secret_mounts` are static fields: they cannot be changed via `openshell policy set` on a running sandbox. They are resolved once at sandbox startup.

#### URN format

File secret URNs must use the `urn:secret-b64:` prefix. The part after the prefix is the Keycard resource name:

| URN | Keycard Resource | Description |
|-----|-----------------|-------------|
| `urn:secret-b64:gpg:signing-key` | `gpg:signing-key` | GPG private key (armored, base64-encoded) |
| `urn:secret-b64:ssh:private-key` | `ssh:private-key` | SSH private key (PEM, base64-encoded) |
| `urn:secret-b64:ssh:deploy-key` | `ssh:deploy-key` | Deploy key for CI (PEM, base64-encoded) |

Plain `urn:resource:` URNs are for environment variable secrets only and are rejected for file secret mounts.

### File Secrets (SSH Keys, GPG Keys)

File secrets are binary or multi-line secrets (SSH private keys, GPG keyrings) that need to be written to a specific filesystem path inside the sandbox rather than injected as environment variables.

File secrets use the `urn:secret-b64:` URN prefix. The secret must be stored in Keycard as a base64-encoded blob. At sandbox startup, OpenShell performs the standard token exchange, base64-decodes the result, and writes the raw content to the target path with secure permissions (`0600`).

#### Storing a GPG signing key in Keycard

Export your GPG key in armored format and base64-encode it before storing it in Keycard:

```bash
gpg --export-secret-keys --armor your-key-id@example.com | base64 | \
  keycard secret create gpg:signing-key --stdin
```

#### Storing an SSH private key in Keycard

```bash
base64 < ~/.ssh/id_ed25519 | keycard secret create ssh:private-key --stdin
```

#### Example policy with file secrets

```yaml
# policy.yaml
version: 1

secrets:
  provider: keyvengers
  env:
    ANTHROPIC_API_KEY: "urn:secret:claude-api"

secret_mounts:
  - source_urn: "urn:secret-b64:gpg:signing-key"
    target_path: "/sandbox/.gnupg/private-key.asc"
    mode: "0600"
  - source_urn: "urn:secret-b64:ssh:private-key"
    target_path: "/sandbox/.ssh/id_ed25519"
    mode: "0600"
```

```bash
openshell sandbox create --policy policy.yaml -- claude
```

At sandbox startup, the gateway resolves each file secret, base64-decodes the content, and the supervisor writes it to the target path with `0600` permissions and `sandbox:sandbox` ownership. Landlock enforcement adds the paths to the read-only set, and companion environment variables (`GIT_SSH_COMMAND`, `GNUPGHOME`) are injected automatically for well-known path patterns.

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

The sandbox has secrets (from `secrets`/`secret_mounts` in the policy) but no Keycard provider. Fix by adding `secrets.provider` to your policy or attaching a provider with `--provider <name>`.

### "keycard provider missing required config keys"

One or more of the four required config keys (`base_url`, `zone_id`, `client_id`, `client_secret`) was not provided when the provider was created. Delete and recreate the provider with all four values.

### "file secret base64 decode failed"

The value stored in Keycard for a `urn:secret-b64:` resource is not valid base64. Re-encode and update the secret:

```bash
gpg --export-secret-keys --armor your-key-id@example.com | base64 | \
  keycard secret update gpg:signing-key --stdin
```

Verify the encoding is correct locally before storing:

```bash
gpg --export-secret-keys --armor your-key-id@example.com | base64 | base64 -d | head -1
# Should print: -----BEGIN PGP PRIVATE KEY BLOCK-----
```

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
