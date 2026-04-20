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

The policy YAML is the single source of truth for sandbox secret configuration. There are three places that bind Keycard secrets to sandbox state, and each maps to a different delivery mechanism:

| Policy block | Keycard URN scheme | Delivered as | Typical use |
|--------------|--------------------|--------------|-------------|
| `secrets.env` | `urn:secret:` | Environment variable | API keys (e.g., `ANTHROPIC_API_KEY`) |
| `secret_mounts` | `urn:secret-b64:` | File on disk (owned by sandbox user) | SSH private keys |
| `gpg_agent` | `urn:secret-b64:` + `urn:secret:` | Running `gpg-agent` daemon; only the Unix socket is exposed | GPG commit signing |

Use `gpg_agent` (not `secret_mounts`) for GPG signing keys. Policy validation rejects `secret_mounts` entries that target a `.gnupg` path when `gpg_agent` is also declared.

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

gpg_agent:
  private_key_urn: "urn:secret-b64:gpg:signing-key"
  passphrase_urn: "urn:secret:gpg-passphrase"
  signing_key_id: "9C90ACA4BA4A104B18B6603CA1E2477E098400D3"

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

The gateway extracts the Keycard provider, env var secrets, file secret mounts, and the `gpg_agent` block from the policy at sandbox creation time.

### secrets block reference

| Field | Type | Description |
|-------|------|-------------|
| `provider` | string | Name of the Keycard provider on the gateway. Required when `env`, `secret_mounts`, or `gpg_agent` is non-empty. |
| `env` | map | Env var name to Keycard resource URN. Each entry becomes an environment variable in the sandbox. |

### secret_mounts reference

| Field | Type | Description |
|-------|------|-------------|
| `source_urn` | string | Keycard resource URN (e.g., `urn:secret-b64:ssh:private-key`). Required. |
| `target_path` | string | Absolute path inside the sandbox where the secret is written. Required. Must not be under `.gnupg` when `gpg_agent` is also declared. |
| `mode` | string | Unix file mode (e.g., `"0600"`). Defaults to `"0600"` when empty. |

The `secrets`, `secret_mounts`, and `gpg_agent` blocks are all **static fields**: they cannot be changed via `openshell policy set` on a running sandbox. They are resolved once at sandbox startup.

#### URN format

| URN scheme | Where it's valid | Keycard storage format |
|------------|------------------|------------------------|
| `urn:secret:` | `secrets.env` values, `gpg_agent.passphrase_urn` | Plain text, returned as-is |
| `urn:secret-b64:` | `secret_mounts.source_urn`, `gpg_agent.private_key_urn` | Base64-encoded bytes; the gateway decodes before delivery |

The part after the scheme prefix (e.g., `gpg:signing-key`, `ssh:private-key`, `claude-api`) is the arbitrary Keycard resource name. Plain `urn:secret:` URNs are rejected for file secret mounts, and `urn:secret-b64:` URNs are rejected for environment variable secrets.

### File Secrets (SSH Keys)

File secrets are binary or multi-line secrets that must be written to a specific filesystem path inside the sandbox. File secrets use the `urn:secret-b64:` URN prefix — the secret must be stored in Keycard as a base64-encoded blob. At sandbox startup, OpenShell performs the standard token exchange, base64-decodes the result, and writes the raw content to the target path with secure permissions (`0600`).

#### Storing an SSH private key in Keycard

```bash
base64 < ~/.ssh/id_ed25519 | keycard secret create ssh:private-key --stdin
```

#### Example policy with an SSH key

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
```

```bash
openshell sandbox create --policy policy.yaml -- claude
```

At sandbox startup, the gateway resolves each file secret, base64-decodes the content, and the supervisor writes it to the target path with `0600` permissions and `sandbox:sandbox` ownership. Landlock enforcement adds the paths to the read-only set, and `GIT_SSH_COMMAND` is injected automatically for `.ssh` paths.

> **GPG keys:** Do not mount GPG private keys with `secret_mounts`. Use the dedicated [`gpg_agent` block](#gpg-commit-signing-via-gpg-agent) instead, which keeps the key material out of any sandbox-readable path.

## GPG Commit Signing via gpg-agent

OpenShell ships a first-party `gpg_agent` policy block for enabling `git commit -S`, `git tag -s`, and `gpg --sign` inside sandboxes **without ever exposing the private key or passphrase to the sandbox user**.

At sandbox startup the supervisor:

1. Resolves the two Keycard URNs (private key, passphrase) via token exchange.
2. Imports the key into a GnuPG keyring at `/sandbox/.gnupg/`.
3. Starts a `gpg-agent` daemon and pre-seeds the passphrase for every keygrip.
4. Locks `/sandbox/.gnupg/private-keys-v1.d/` to `root:root 0700` (the sandbox user cannot read the encrypted `.key` files).
5. Installs a headless pinentry that reads the passphrase from a root-only file, so signing keeps working if the cache is invalidated.
6. Scrubs the passphrase from the sandbox environment.
7. Optionally writes `/sandbox/.gitconfig` with `user.signingkey` and `commit.gpgsign = true`.

The sandbox user only ever sees the `gpg-agent` Unix socket at `/sandbox/.gnupg/S.gpg-agent` and can request signatures through it — the private key never leaves the agent.

### Required Keycard resources

Two Keycard resources must exist before a sandbox can start with `gpg_agent`:

| Keycard resource | URN scheme | Content |
|------------------|------------|---------|
| GPG signing key | `urn:secret-b64:` | Base64 of the ASCII-armored, passphrase-protected private key |
| GPG passphrase | `urn:secret:` | Plain text passphrase that unlocks the private key |

#### Storing the GPG signing key in Keycard

Export your GPG key in armored format and base64-encode it before storing:

```bash
gpg --export-secret-keys --armor <your-key-id> | base64 | \
  keycard secret create gpg:signing-key --stdin
```

Verify the encoding round-trips locally before you rely on it:

```bash
keycard secret get gpg:signing-key | base64 -d | head -1
# Should print: -----BEGIN PGP PRIVATE KEY BLOCK-----
```

#### Storing the GPG passphrase in Keycard

```bash
printf '%s' "<your-passphrase>" | keycard secret create gpg-passphrase --stdin
```

Unlike the key, the passphrase is stored as plain text (no base64) because it is delivered to the gateway as an environment-style secret.

### Policy block

Add a top-level `gpg_agent` block to the policy YAML:

```yaml
gpg_agent:
  private_key_urn: "urn:secret-b64:gpg:signing-key"
  passphrase_urn: "urn:secret:gpg-passphrase"
  signing_key_id: "9C90ACA4BA4A104B18B6603CA1E2477E098400D3"
```

| Field | Required | URN scheme | Description |
|-------|----------|------------|-------------|
| `private_key_urn` | Yes | `urn:secret-b64:` | Keycard URN for the ASCII-armored, base64-encoded GPG private key. |
| `passphrase_urn` | Yes | `urn:secret:` | Keycard URN for the passphrase that unlocks the private key. Delivered as an env-style secret and scrubbed before the sandbox process starts. |
| `signing_key_id` | No | — | GPG key ID (long form, full 40-char fingerprint recommended). When set, the supervisor writes `user.signingkey = <id>` and `commit.gpgsign = true` to `/sandbox/.gitconfig` so `git commit -S` works with no additional user configuration. If omitted, GnuPG picks the first available key. |

### Validation rules

Policy validation enforces the following at load time:

- Both `private_key_urn` and `passphrase_urn` must be non-empty.
- `private_key_urn` must use the `urn:secret-b64:` scheme (the key is delivered as a base64-encoded file secret).
- `passphrase_urn` must use the `urn:secret:` scheme (the passphrase is delivered as a plain env-style secret).
- Declaring `gpg_agent` together with a `secret_mounts` entry whose `target_path` contains `.gnupg` is a hard error — the two mechanisms would fight over ownership of `GNUPGHOME`.
- The `gpg_agent` block uses strict YAML parsing (unknown fields are rejected).

### Migration from `secret_mounts`-based GPG delivery

Earlier versions of OpenShell required users to mount GPG keys as regular file secrets:

```yaml
# OLD — rejected by policy validation when gpg_agent is also present.
secret_mounts:
  - source_urn: "urn:secret-b64:gpg:signing-key"
    target_path: "/sandbox/.gnupg/private-key.asc"
    mode: "0600"
```

This is deprecated. The `gpg_agent` block replaces it, keeps the key material out of any sandbox-readable path, pre-seeds the passphrase, and configures git signing automatically. Remove the `/sandbox/.gnupg/...` entry from `secret_mounts` and add a `gpg_agent` block referencing the same Keycard resource.

### What the sandbox user sees

| Concern | Behavior |
|---------|----------|
| `GNUPGHOME` | Points to `/sandbox/.gnupg`, injected into the sandbox environment automatically. |
| `gpg --list-secret-keys` | Shows the imported key (via the agent). |
| `gpg --sign`, `git commit -S`, `git tag -s` | Work transparently using the pre-seeded passphrase cache. |
| Private key file | Present on disk at `/sandbox/.gnupg/private-keys-v1.d/<keygrip>.key` but owned `root:root 0700` and passphrase-protected — the sandbox user cannot read it. |
| Passphrase | Never written to the sandbox user's environment or any sandbox-readable path. |
| Container image requirements | Must include `gpg`, `gpg-agent`, and `gpg-preset-passphrase` (typically from `gnupg` + `gnupg-utils`). |

### Static field enforcement

`gpg_agent` is a **static field**. Like `secrets` and `secret_mounts`, it is resolved once at sandbox creation and cannot be changed via `openshell policy set` on a running sandbox. Attempting to change it is rejected by the server with a `static field changed` error. Policy hashing includes the `gpg_agent` block, but OPA evaluation data excludes it so URNs and the signing key ID never leak into decision logs.

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

The sandbox has secrets (from `secrets`, `secret_mounts`, or `gpg_agent` in the policy) but no Keycard provider. Fix by adding `secrets.provider` to your policy or attaching a provider with `--provider <name>`.

### "keycard provider missing required config keys"

One or more of the four required config keys (`base_url`, `zone_id`, `client_id`, `client_secret`) was not provided when the provider was created. Delete and recreate the provider with all four values.

### "file secret base64 decode failed"

The value stored in Keycard for a `urn:secret-b64:` resource is not valid base64. Re-encode and update the secret:

```bash
gpg --export-secret-keys --armor <your-key-id> | base64 | \
  keycard secret update gpg:signing-key --stdin
```

Verify the encoding is correct locally before storing:

```bash
keycard secret get gpg:signing-key | base64 -d | head -1
# Should print: -----BEGIN PGP PRIVATE KEY BLOCK-----
```

### "invalid gpg_agent: private_key_urn must use the urn:secret-b64: scheme"

The `private_key_urn` must be a base64-encoded file secret. Update the policy to use the `urn:secret-b64:` prefix (not `urn:secret:`).

### "invalid gpg_agent: passphrase_urn must use the urn:secret: scheme"

The `passphrase_urn` must be a plain env-style secret. Update the policy to use the `urn:secret:` prefix (not `urn:secret-b64:`) and store the passphrase in Keycard as plain text (no base64).

### "invalid gpg_agent: gpg_agent conflicts with secret_mount targeting '/sandbox/.gnupg/...'"

You have both a `gpg_agent` block and a `secret_mounts` entry targeting a `.gnupg` path. Remove the `.gnupg` entry from `secret_mounts` — the `gpg_agent` block handles the full GnuPG setup.

### "required binary 'gpg-preset-passphrase' not found"

The sandbox container image does not include `gpg-preset-passphrase`. Install the `gnupg-utils` package (or distribution equivalent) in the image — the binary lives under the GnuPG `libexecdir` reported by `gpgconf --list-dirs libexecdir`.

### `git commit -S` still prompts for a passphrase

Check that the sandbox image includes `gpg`, `gpg-agent`, and `gpg-preset-passphrase`, and that no user-level `gpg.conf` overrides `pinentry-program`. The supervisor writes `gpg-agent.conf` with `allow-preset-passphrase`, `allow-loopback-pinentry`, and a headless pinentry, but a conflicting user config can still break the flow.

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
