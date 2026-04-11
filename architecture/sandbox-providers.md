# Providers

## Overview

OpenShell uses a first-class `Provider` entity to represent external tool credentials and
configuration (for example `claude`, `gitlab`, `github`, `outlook`, `generic`, `nvidia`).

Providers exist as an abstraction layer for configuring tools that rely on third-party
access. Rather than each tool managing its own credentials and service configuration,
providers centralize that concern: a user configures a provider once, and any sandbox that
needs that external service can reference it.

At sandbox creation time, providers are validated and associated with the sandbox. The
sandbox supervisor then fetches credentials at runtime, keeps the real secret values in
supervisor-only memory, and injects placeholder environment variables into every child
process it spawns. When outbound traffic is allowed through the sandbox proxy, the
supervisor rewrites those placeholders back to the real secret values before forwarding.
Access is enforced through the sandbox policy — the policy decides which outbound
requests are allowed or denied based on the providers attached to that sandbox.

Core goals:

- manage providers directly via CLI,
- discover provider data from the local machine automatically,
- require providers during sandbox creation,
- project provider context into sandbox runtime,
- drive sandbox policy to allow or deny outbound access to third-party services.

## Data Model

Provider is defined in `proto/datamodel.proto`:

- `id`: unique entity id
- `name`: user-managed name
- `type`: canonical provider slug (`claude`, `gitlab`, `github`, etc.)
- `credentials`: `map<string, string>` for secret values
- `config`: `map<string, string>` for non-secret settings

The gRPC surface is defined in `proto/openshell.proto`:

- `CreateProvider`
- `GetProvider`
- `ListProviders`
- `UpdateProvider`
- `DeleteProvider`

## Components

- `crates/openshell-providers`
  - canonical provider type normalization and command detection,
  - provider registry and per-provider discovery plugins,
  - shared discovery engine and context abstraction for testability.
- `crates/openshell-cli`
  - `openshell provider ...` command handlers,
  - sandbox provider requirement resolution in `sandbox create`.
- `crates/openshell-server` (gateway)
  - provider CRUD gRPC handlers,
  - `GetSandboxProviderEnvironment` handler resolves credentials at runtime,
  - persistence using `object_type = "provider"`.
- `crates/openshell-sandbox`
  - sandbox supervisor fetches provider credentials via gRPC at startup,
  - injects placeholder env vars into entrypoint and SSH child processes,
  - resolves placeholders back to real secrets in the outbound proxy path.

## Provider Plugins

Each provider has its own module under `crates/openshell-providers/src/providers/`.

### Trait Definition

`ProviderPlugin` (`crates/openshell-providers/src/lib.rs`):

```rust
pub trait ProviderPlugin: Send + Sync {
    fn id(&self) -> &'static str;
    fn discover_existing(&self) -> Result<Option<DiscoveredProvider>, ProviderError>;
    fn apply_to_sandbox(&self, _provider: &Provider) -> Result<(), ProviderError> {
        Ok(())  // default no-op, forward-looking extension point
    }
}
```

`DiscoveredProvider` holds two maps (`credentials` and `config`) returned by discovery.

### Current Modules

| Module | Env Vars Discovered | Config Paths |
|---|---|---|
| `claude.rs` | `ANTHROPIC_API_KEY`, `CLAUDE_API_KEY` | `~/.claude.json`, `~/.claude/credentials.json`, `~/.config/claude/config.json` |
| `codex.rs` | `OPENAI_API_KEY` | `~/.config/codex/config.json`, `~/.codex/config.json`, `~/.config/openai/config.json` |
| `opencode.rs` | `OPENCODE_API_KEY`, `OPENROUTER_API_KEY`, `OPENAI_API_KEY` | `~/.config/opencode/config.json`, `~/.opencode/config.json` |
| `openclaw.rs` | `OPENCLAW_API_KEY`, `OPENAI_API_KEY` | `~/.config/openclaw/config.json`, `~/.openclaw/config.json` |
| `generic.rs` | *(none)* | *(none)* |
| `nvidia.rs` | `NVIDIA_API_KEY` | *(none)* |
| `gitlab.rs` | `GITLAB_TOKEN`, `GLAB_TOKEN`, `CI_JOB_TOKEN` | `~/.config/glab-cli/config.yml` |
| `github.rs` | `GITHUB_TOKEN`, `GH_TOKEN` | `~/.config/gh/hosts.yml` |
| `outlook.rs` | *(none)* | *(none)* |
| `keycard.rs` | *(none — lifecycle provider)* | *(none)* |

`generic`, `outlook`, and `keycard` are stubs for discovery — `discover_existing()` always
returns `None`. The keycard provider is a lifecycle-aware provider (see below) rather than
a passive credential store.

Each plugin defines a `ProviderDiscoverySpec` with its `id`, `credential_env_vars`, and
`config_paths`. The registry is assembled in `ProviderRegistry::new()` by registering
each provider module.

### Normalization

`normalize_provider_type()` maps common aliases to canonical slugs: `"glab"` -> `"gitlab"`,
`"gh"` -> `"github"`, and accepts `"generic"` directly as a first-class type.
`detect_provider_from_command()` extracts the file basename from the first command token
and passes it through normalization.

## Discovery Architecture

Discovery behavior is split into three layers:

1. provider module defines static spec (`ProviderDiscoverySpec`),
2. shared engine (`discover_with_spec`) performs env/file scanning,
3. runtime context (`DiscoveryContext`) supplies filesystem/environment reads.

### Discovery Engine

`discover_with_spec(spec, context)` performs two passes:

1. **Environment variable scan**: for each var in `spec.credential_env_vars`, reads from
   the `DiscoveryContext`. Non-empty values are stored in `discovered.credentials`.

2. **Config file scan**: for each path in `spec.config_paths`:
   - expands `~/` via the context,
   - rejects `~/` expansions that contain path-escape components (for example `..`),
   - checks file existence,
   - **only parses `.json` files** (`.yml`/`.yaml` are checked for existence but not read),
   - recursively collects JSON fields whose keys match credential patterns
     (`api_key`, `apikey`, `token`, `secret`, `password`, `auth` — case-insensitive),
   - collected values go into `discovered.credentials` using dotted path keys
     (for example `"oauth.api_key"`).

Config file values always go into `credentials`, not `config`. The `config` map is only
populated via explicit CLI flags.

### Discovery Context

`DiscoveryContext` trait:

```rust
pub trait DiscoveryContext {
    fn env_var(&self, key: &str) -> Option<String>;
    fn expand_home(&self, path: &str) -> Option<PathBuf>;
    fn path_exists(&self, path: &Path) -> bool;
    fn read_to_string(&self, path: &Path) -> Option<String>;
}
```

Implementations:

- `RealDiscoveryContext` for production runtime (reads from `std::env` and filesystem),
- `MockDiscoveryContext` test helper for deterministic tests.

This keeps provider tests isolated from host environment and filesystem.

## CLI Flows

### Provider CRUD

`openshell provider create --type <type> --name <name> [--from-existing] [--credential k=v]... [--config k=v]...`

- `--credential` supports `KEY=VALUE` and `KEY` forms.
  - `KEY=VALUE` sets an explicit credential value.
  - `KEY` reads from the local environment variable with the same key, and fails when
    the local value is missing or empty.
- `--from-existing` and `--credential` are mutually exclusive.
- `--from-existing` merges discovered laptop data into explicit `--config` args.

Also supported:

- `openshell provider get <name>`
- `openshell provider list`
- `openshell provider update <name> ...`
- `openshell provider delete <name> [<name>...]`

### Sandbox Create

`openshell sandbox create --provider gitlab -- claude`

Resolution logic (CLI side, `crates/openshell-cli/src/run.rs`):

1. `detect_provider_from_command()` infers provider from command token after `--`
   (for example `claude`),
2. union with explicit `--provider <type>` flags (normalized),
3. deduplicate,
4. `ensure_required_providers()` checks each required type exists on the gateway,
5. if interactive and missing, auto-create from existing local state
   (uses `ProviderRegistry::discover_existing()`), trying names like `"claude"`,
   `"claude-1"`, etc. up to 5 retries for name conflicts,
6. non-interactive mode fails with a clear missing-provider error,
7. set resolved provider **names** in `SandboxSpec.providers`.

Gateway-side `create_sandbox()` (`crates/openshell-server/src/grpc.rs`):

1. validates policy (process identity defaults, safety checks),
2. **extracts policy-declared secrets** via `extract_policy_secrets()` — merges
   `secrets.env`, `secrets.provider`, and `secret_mounts` from the policy into the
   `SandboxSpec` (see [Policy-Declared Secrets](#policy-declared-secrets) below),
3. validates all provider names exist by fetching each from the store (fail fast),
4. creates the `Sandbox` object with `spec.providers` set,
5. **does not inject credentials into the pod spec** — credentials are fetched at runtime.

If a requested provider name is not found, sandbox creation fails with a
`FailedPrecondition` error.

> **Note:** Providers can also be configured from within the sandbox itself. This allows
> sandbox users to set up or update provider credentials and configuration at runtime,
> without requiring them to be fully resolved before sandbox creation.

## Policy-Declared Secrets

Secret bindings (env var secrets, file secret mounts, and the Keycard provider) are
declared in the policy YAML. The policy is the single source of truth for sandbox
secret configuration.

### Policy Schema

The policy YAML supports two top-level fields for secret declarations:

```yaml
version: 1

secrets:
  provider: keyvengers
  env:
    ANTHROPIC_API_KEY: "urn:secret:claude-api"
    GITHUB_TOKEN: "urn:secret:gh-token"

secret_mounts:
  - source_urn: "urn:secret-b64:ssh:private-key"
    target_path: "/sandbox/.ssh/id_ed25519"
    mode: "0600"

filesystem_policy:
  # ...
network_policies:
  # ...
```

The `secrets` block is defined by the `SecretsDef` serde struct in
`crates/openshell-policy/src/lib.rs` and maps to the `PolicySecrets` proto message
in `proto/sandbox.proto`:

| Field | Type | Description |
|-------|------|-------------|
| `secrets.provider` | `string` | Name of the Keycard provider on the gateway. Required when `env` is non-empty. |
| `secrets.env` | `map<string, string>` | Env var name → Keycard resource URN. Each entry becomes an environment variable in the sandbox. |

The `secret_mounts` field uses the existing `SecretMount` proto message and
`SecretMountDef` serde struct.

### Extraction Semantics

The gateway's `extract_policy_secrets()` function (`crates/openshell-server/src/grpc.rs`)
runs during `create_sandbox()`, after policy validation but before provider validation.
It reads the parsed `SandboxPolicy` and extracts its secret declarations into `SandboxSpec`:

1. **`secrets.env` → `SandboxSpec.secrets`**: Each `(key, urn)` pair is inserted directly.
   The policy is the sole source of secret configuration.

2. **`secrets.provider` → `SandboxSpec.providers`**: The provider name is appended to the
   providers list if not already present.

3. **`secret_mounts` → `SandboxSpec.file_secrets`**: Each mount's `(target_path, source_urn)`
   pair is inserted directly.

This extraction happens before provider validation, so policy-declared providers are
included in the validation loop that checks provider existence on the gateway.

### Usage

```bash
openshell sandbox create --policy policy.yaml -- claude
```

### Validation

Policy-declared secrets are validated by `validate_sandbox_policy()` in
`crates/openshell-policy/src/lib.rs`:

- `secrets.env` keys must be valid environment variable names
  (`^[A-Za-z_][A-Za-z0-9_]*$`).
- `secrets.env` values must be non-empty.
- `secrets.provider` must be non-empty when `secrets.env` has entries.
- `secret_mounts` paths must be absolute, contain no traversal components, and respect
  length limits.

### Static Field Enforcement

Both `secrets` and `secret_mounts` are static fields — they cannot be changed via
`openshell policy set` on a running sandbox. The `validate_static_fields_unchanged()`
function rejects any update that modifies either field. Both are resolved once at sandbox
startup (secret resolution and file writes happen during provisioning).

### OPA Data Isolation

The `proto_to_opa_data_json()` function in `crates/openshell-sandbox/src/opa.rs` uses an
allowlist approach: it constructs OPA data from `filesystem_policy`, `landlock`, `process`,
and `network_policies` only. The `secrets` and `secret_mounts` fields are never included
in the OPA data document, so secret URNs and provider names do not leak into Rego
evaluation.

### Policy Hashing

The `deterministic_policy_hash()` function in `crates/openshell-server/src/grpc.rs`
includes both `secrets` (provider name + sorted env entries) and `secret_mounts` in the
SHA-256 hash. Adding, removing, or changing secret declarations produces a different hash,
which triggers sandbox reload detection.

## Sandbox Credential Injection

### Runtime Credential Resolution

`SandboxSpec` includes a `providers` field (`repeated string`) containing provider names.
Credentials are **not** embedded in the pod spec. Instead, the sandbox supervisor fetches
them at runtime via the `GetSandboxProviderEnvironment` gRPC call.

### Gateway-side: `resolve_provider_environment()`

`resolve_provider_environment()` (`crates/openshell-server/src/grpc.rs`) builds the
environment map returned by `GetSandboxProviderEnvironment`:

1. for each provider name in `spec.providers`, fetch the provider from the store,
2. iterate over `provider.credentials` only (not `config`),
3. validate each key matches `^[A-Za-z_][A-Za-z0-9_]*$` (valid env var name),
4. insert into result map using `entry().or_insert()` — first provider's value wins
   when duplicate keys appear across providers,
5. invalid keys are skipped with a warning log.

Key behaviors:

- Only `credentials` are injected, not `config`.
- Invalid env var keys (containing `.`, `-`, spaces, etc.) are skipped.
- Credentials are never persisted in the sandbox spec's environment map.

### Sandbox Supervisor: Fetching Credentials

The sandbox pod runs `openshell-sandbox` (`crates/openshell-sandbox/src/main.rs`). On
startup it receives `OPENSHELL_SANDBOX_ID` and `OPENSHELL_ENDPOINT` as environment
variables (injected into the pod spec by the gateway's Kubernetes sandbox creation code).

In `run_sandbox()` (`crates/openshell-sandbox/src/lib.rs`):

1. loads the sandbox policy via gRPC (`GetSandboxSettings`),
2. fetches provider credentials via gRPC (`GetSandboxProviderEnvironment`),
3. if the fetch fails, continues with an empty map (graceful degradation with a warning).

The returned `provider_env` `HashMap<String, String>` is immediately transformed into:

- a child-visible env map with placeholder values such as
  `openshell:resolve:env:ANTHROPIC_API_KEY`, and
- a supervisor-only in-memory registry mapping each placeholder back to its real secret.

The placeholder env map is threaded to the entrypoint process spawner and SSH server.
The registry is threaded to the proxy so it can rewrite outbound headers.

### Child Process Environment Variable Injection

Provider placeholders are injected into child processes in two places, covering all
process spawning paths inside the sandbox:

**1. Entrypoint process** (`crates/openshell-sandbox/src/process.rs`):

```rust
let mut cmd = Command::new(program);
cmd.args(args)
    .env("OPENSHELL_SANDBOX", "1");

// Set provider environment variables (supervisor-managed placeholders).
for (key, value) in provider_env {
    cmd.env(key, value);
}
```

This uses `tokio::process::Command`. The `.env()` call adds each variable to the child's
inherited environment without clearing it. The spawn path also explicitly removes
`OPENSHELL_SSH_HANDSHAKE_SECRET` so the handshake secret does not leak into the agent
entrypoint process.

After provider env vars, proxy env vars (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`,
`NO_PROXY=127.0.0.1,localhost,::1`, lowercase variants, etc.) are also set when
`NetworkMode` is `Proxy`. The child is then launched with namespace
isolation, privilege dropping, seccomp, and Landlock restrictions via `pre_exec`.

**2. SSH shell sessions** (`crates/openshell-sandbox/src/ssh.rs`):

When a user connects via `openshell sandbox connect`, a PTY shell is spawned:

```rust
let mut cmd = Command::new(shell);
cmd.env("OPENSHELL_SANDBOX", "1")
    .env("HOME", "/sandbox")
    .env("USER", "sandbox")
    .env("TERM", term);

// Set provider environment variables (supervisor-managed placeholders).
for (key, value) in provider_env {
    cmd.env(key, value);
}
```

This uses `std::process::Command`. The `SshHandler` holds the `provider_env` map and
passes it to `spawn_pty_shell()` for each new shell or exec request. SSH child processes
start from `env_clear()`, so the handshake secret is not present there.

### Proxy-Time Secret Resolution

When a sandboxed tool uses one of these placeholder env vars in an outbound HTTP request,
the sandbox proxy rewrites the placeholder to the real secret value immediately before the
request is forwarded upstream. Placeholders are resolved in four locations:

- **HTTP header values** — exact match (`x-api-key: openshell:resolve:env:KEY`), prefixed
  match (`Authorization: Bearer openshell:resolve:env:KEY`), and Base64-decoded Basic auth
  tokens (`Authorization: Basic <base64(user:openshell:resolve:env:PASS)>`)
- **URL query parameters** — for APIs that authenticate via query string
  (e.g., `?key=openshell:resolve:env:YOUTUBE_API_KEY`)
- **URL path segments** — for APIs that embed tokens in the URL path
  (e.g., `/bot<placeholder>/sendMessage` for Telegram Bot API)

This applies to forward-proxy HTTP requests, L7-inspected REST requests inside CONNECT
tunnels, and credential-injection-only passthrough relays on TLS-terminated connections.

All rewriting fails closed: if any `openshell:resolve:env:*` placeholder is detected but
cannot be resolved, the proxy rejects the request with HTTP 500 instead of forwarding the
raw placeholder upstream. Resolved secret values are validated for prohibited control
characters (CR, LF, null byte) to prevent header injection (CWE-113). Path segment
credentials are additionally validated to reject traversal sequences, path separators, and
URI delimiters (CWE-22).

The real secret value remains in supervisor memory only; it is not re-injected into the
child process environment. See [Credential injection](sandbox.md#credential-injection) for
the full implementation details, encoding rules, and security properties.

### End-to-End Flow

```
CLI: openshell sandbox create -- claude
  |
  +-- detect_provider_from_command(["claude"]) -> "claude"
  +-- ensure_required_providers() -> discovers local ANTHROPIC_API_KEY
  |     +-- Creates provider record "claude" on gateway with credentials
  +-- Sets SandboxSpec.providers = ["claude"]
  +-- Sends CreateSandboxRequest to gateway
        |
        Gateway: create_sandbox()
          +-- Validates provider "claude" exists in store (fail fast)
          +-- Persists Sandbox with spec.providers = ["claude"]
          +-- Creates K8s Sandbox CRD (no credentials in pod spec)
                |
                K8s: pod starts openshell-sandbox binary
                  +-- OPENSHELL_SANDBOX_ID and OPENSHELL_ENDPOINT set in pod env
                  |
                    Sandbox supervisor: run_sandbox()
                      +-- Fetches policy via gRPC
                      +-- Fetches provider env via gRPC
                      |     +-- Gateway resolves: "claude" -> credentials -> {ANTHROPIC_API_KEY: "sk-..."}
                      +-- Builds placeholder registry
                      |     +-- child env: {ANTHROPIC_API_KEY: "openshell:resolve:env:ANTHROPIC_API_KEY"}
                      |     +-- supervisor registry: {"openshell:resolve:env:ANTHROPIC_API_KEY": "sk-..."}
                      +-- Spawns entrypoint with placeholder env
                      +-- SSH server holds placeholder env
                      |     +-- Each SSH shell: cmd.env("ANTHROPIC_API_KEY", "openshell:resolve:env:ANTHROPIC_API_KEY")
                      +-- Proxy rewrites outbound auth header placeholders -> real secrets
```

## Keycard Provider (Lifecycle-Aware)

The Keycard provider is architecturally distinct from all other providers. While standard
providers are passive credential stores, the Keycard provider is lifecycle-aware: it makes
external API calls during sandbox provisioning and decommissioning to create and destroy
per-sandbox application identities.

### Data Model

A single Keycard provider record holds admin API credentials in the `config` map (not
`credentials`), preventing them from being injected into sandboxes:

| Config Key | Description |
|-----------|-------------|
| `base_url` | Keycard API base URL |
| `zone_id` | Keycard zone ID (also used as the SPIFFE trust domain) |
| `client_id` | Admin client ID for Keycard API authentication |
| `client_secret` | Admin client secret for Keycard API authentication |

The `credentials` map is empty for Keycard providers. Per-sandbox credentials are created
dynamically and stored in an ephemeral in-memory credential store on the gateway server.

#### Per-Sandbox Secrets

`SandboxSpec` has a `secrets` field (`map<string, string>`) that maps environment variable
names to Keycard resource URNs:

```protobuf
map<string, string> secrets = 10;
```

This allows each sandbox to declare which API keys it needs, resolved at runtime via
Keycard token exchange. The secrets map is per-sandbox, not per-provider, because different
sandboxes may need different API keys from the same Keycard provider.

### SPIFFE Identity

Each sandbox gets a SPIFFE-formatted identity:

```
spiffe://{zone_id}/sandbox/{sandbox_id}
```

This identifier is used when creating the Keycard APPLICATION. The zone ID serves as the
SPIFFE trust domain.

### Sandbox Lifecycle

**Provisioning (`create_sandbox()`):**

1. After sandbox ID generation, the server checks if any listed provider has type `keycard`.
2. For each Keycard provider, the server extracts admin config and creates a `KeycardClient`.
3. The client calls `POST /zones/{zoneId}/applications` with the SPIFFE ID as identifier.
4. The client calls `POST /zones/{zoneId}/application-credentials` to generate a password
   credential for the application.
5. The returned `identifier` (client ID) and `password` (client secret) are stored in the
   `KeycardCredentialStore` keyed by sandbox ID.
6. If any Keycard API call fails, sandbox creation is aborted entirely (fail-closed).
7. If K8s resource creation fails after Keycard provisioning, the ephemeral credentials are
   cleaned up from the store.

**Decommissioning (`delete_sandbox()`):**

1. The server removes the sandbox's entry from the `KeycardCredentialStore`.
2. If credentials existed, the server calls `DELETE /zones/{zoneId}/applications/{id}` to
   remove the Keycard APPLICATION.
3. Cleanup failures are logged but do not block sandbox deletion.

### Credential Resolution (Token Exchange)

When `resolve_provider_environment()` encounters a Keycard provider, it skips the
provider's `credentials` and `config` maps entirely. Instead, it performs a server-side
OAuth2 token exchange for each entry in the sandbox's `secrets` map:

1. Reads per-sandbox credentials (`client_id`, `client_secret`, `zone_id`) from the
   `KeycardCredentialStore`.
2. For each `(env_key, resource_urn)` in `sandbox.spec.secrets`:
   - Calls `POST https://{zone_id}.keycard.cloud/oauth/2/token` with the per-sandbox
     `client_id`/`client_secret` via BasicAuth and `resource={resource_urn}`.
   - The returned `access_token` is the actual API key (e.g., an Anthropic key).
   - Injects `{env_key: access_token}` into the resolved environment map.
3. If the secrets map is empty, the Keycard provider is silently skipped.

The sandbox never sees the per-sandbox OAuth credentials or the Keycard admin credentials.
It receives only the actual API keys (e.g., `ANTHROPIC_API_KEY=sk-ant-...`) through the
standard placeholder/proxy resolution mechanism.

Secrets are declared in the policy YAML:

```yaml
secrets:
  provider: my-keycard
  env:
    ANTHROPIC_API_KEY: "urn:resource:anthropic-api-key"
```

```bash
openshell sandbox create --policy policy.yaml
```

The gateway extracts `secrets.provider` and `secrets.env` from the policy during sandbox
creation. See [Policy-Declared Secrets](#policy-declared-secrets) for extraction semantics.

### Ephemeral Credential Store

The `KeycardCredentialStore` (`crates/openshell-server/src/keycard.rs`) is an in-memory
`HashMap<String, SandboxKeycardCredentials>` protected by a `RwLock`. Each entry stores:

- `application_id` — Keycard-internal ID for API calls (deletion)
- `provider_name` — which Keycard provider this credential belongs to
- `client_id` — per-sandbox client ID for token exchange
- `client_secret` — per-sandbox client secret for token exchange
- `zone_id` — zone ID from provider config, used to construct the exchange URL

Credentials exist only in memory, scoped to the sandbox lifetime. They are never persisted
to the database. If the gateway process restarts, credentials are lost — existing sandboxes
would need reprovisioning.

### Components

| File | Role |
|------|------|
| `crates/openshell-providers/src/providers/keycard.rs` | Provider plugin (type registration, discovery stub) |
| `crates/openshell-server/src/keycard.rs` | Keycard HTTP client, ephemeral credential store |
| `crates/openshell-server/src/grpc.rs` | Lifecycle hooks in `create_sandbox()` and `delete_sandbox()` |
| `crates/openshell-server/src/lib.rs` | `KeycardCredentialStore` in `ServerState` |

### End-to-End Flow

Secrets are declared in the policy YAML. The gateway extracts them into the
`SandboxSpec` before proceeding with Keycard provisioning.

```
CLI: openshell sandbox create --policy policy.yaml -- claude
  |
  +-- SandboxSpec.policy.secrets = {provider: "my-keycard", env: {ANTHROPIC_API_KEY: ...}}
  +-- SandboxSpec.providers = [] (populated server-side from policy)
  +-- SandboxSpec.secrets = {} (populated server-side from policy)
  +-- Sends CreateSandboxRequest to gateway
```

**Gateway processing:**

```
Gateway: create_sandbox()
  +-- Validates policy, ensures process identity defaults
  +-- extract_policy_secrets():
  |     +-- Extracts policy.secrets.env into spec.secrets
  |     +-- Extracts policy.secrets.provider into spec.providers (if not already listed)
  |     +-- Extracts policy.secret_mounts into spec.file_secrets
  +-- Validates provider "my-keycard" exists, detects type "keycard"
  +-- Validates secrets requires keycard provider (fail if none)
  +-- Generates sandbox ID
  +-- Calls Keycard API: POST /zones/{zoneId}/applications
  |     +-- identifier: "spiffe://{zoneId}/sandbox/{sandboxId}"
  +-- Calls Keycard API: POST /zones/{zoneId}/application-credentials
  |     +-- Returns: {identifier: "client-id", password: "client-secret"}
  +-- Stores in KeycardCredentialStore: sandboxId -> {client_id, client_secret, zone_id}
  +-- Persists Sandbox with spec.secrets, creates K8s resource
        |
        Sandbox supervisor: run_sandbox()
          +-- Fetches provider env via gRPC
          |     +-- Gateway resolves:
          |     |   "my-keycard" (type: keycard) -> token exchange for each secret
          |     |   POST https://{zoneId}.keycard.cloud/oauth/2/token
          |     |     BasicAuth(client-id, client-secret), resource=urn:resource:anthropic-api-key
          |     |   -> {ANTHROPIC_API_KEY: "sk-ant-actual-key"}
          |     +-- Admin creds and per-sandbox OAuth creds NEVER included
          +-- Builds placeholder registry + child env
          +-- Spawns entrypoint/SSH with ANTHROPIC_API_KEY placeholder

CLI: openshell sandbox delete my-sandbox
  |
  +-- Gateway: delete_sandbox()
        +-- Removes credentials from KeycardCredentialStore
        +-- Calls Keycard API: DELETE /zones/{zoneId}/applications/{appId}
        +-- Deletes K8s resource, cleans up SSH sessions, settings, bus entries
```

## Persistence and Validation

The gateway enforces:

- `provider.type` must be non-empty,
- `provider.credentials` must be non-empty (except for Keycard providers),
- Keycard providers must have all required config keys (`base_url`, `zone_id`,
  `client_id`, `client_secret`) with non-empty values,
- name uniqueness for providers,
- generated `id` on create,
- id preservation on update,
- `SandboxSpec.secrets` keys must be valid environment variable names,
- sandboxes with non-empty `secrets` must have at least one Keycard provider attached,
- policy-declared `secrets.env` keys must be valid env var names with non-empty values,
- policy-declared `secrets.provider` must be non-empty when `secrets.env` has entries,
- `secrets` and `secret_mounts` are static fields — rejected by
  `validate_static_fields_unchanged()` on live policy updates.

Providers are stored with `object_type = "provider"` in the shared object store.

## Security Notes

- Provider credentials are stored in `credentials` map and treated as sensitive.
- CLI output intentionally avoids printing credential values.
- CLI displays only non-sensitive summaries (counts/key names where relevant).
- Credentials are never persisted in the sandbox spec — they exist only in the
  provider store and are fetched at runtime by the sandbox supervisor.
- Child processes never receive the raw provider secret values; they only receive
  placeholders, and the supervisor resolves those placeholders during outbound proxying.
- `OPENSHELL_SSH_HANDSHAKE_SECRET` is required by the supervisor/SSH server path but is
  explicitly kept out of spawned sandbox child-process environments.
- Keycard admin credentials (base_url, zone_id, client_id, client_secret) are stored in
  the provider `config` map and are used server-side only. They are explicitly excluded
  from `resolve_provider_environment()` for Keycard providers.
- Per-sandbox Keycard credentials (client_id, client_secret) exist only in the gateway's
  in-memory `KeycardCredentialStore`, scoped to the sandbox lifetime. They are never
  persisted to the database and are never exposed to the sandbox. They are used
  server-side only for OAuth2 token exchange.
- The sandbox never sees Keycard OAuth credentials. It receives only the actual API keys
  (e.g., `ANTHROPIC_API_KEY`) resolved via server-side token exchange.

## Test Strategy

- Per-provider unit tests in each provider module.
- Shared normalization/command-detection tests in `crates/openshell-providers/src/lib.rs`.
- Mocked discovery context tests cover env and path-based behavior.
- CLI and gateway integration tests validate end-to-end RPC compatibility.
- `resolve_provider_environment` unit tests in `crates/openshell-server/src/grpc.rs`.
- Keycard-specific `resolve_provider_environment` tests verify admin credential filtering,
  secrets-based token exchange, empty-secrets skipping, and mixed provider scenarios.
- Keycard HTTP client tests use `wiremock` to mock all API endpoints (create application,
  create credential, delete application, token exchange) with success, failure, and
  rollback scenarios.
- Secrets validation tests verify env var key format and keycard provider requirement.
- `KeycardCredentialStore` unit tests verify isolation between sandboxes.
- Keycard provider creation tests validate required config key presence.
- sandbox unit tests validate placeholder generation and header rewriting.
- E2E sandbox tests verify placeholders are visible in child env, outbound proxy traffic
  is rewritten with the real secret, and the SSH handshake secret is absent from exec env.
- Policy-declared secrets tests in `crates/openshell-policy/src/lib.rs`:
  - Round-trip fidelity: YAML with `secrets` block → proto → YAML preserves all fields.
  - Validation rejects `secrets.env` without a provider, invalid env keys, and empty values.
  - Validation accepts valid secrets with and without env entries (provider-only).
  - Policies without `secrets` continue to parse (backward compatibility).
- `extract_policy_secrets` unit tests in `crates/openshell-server/src/grpc.rs`:
  - Policy env secrets merge into spec, CLI values take precedence on conflict.
  - Policy provider merges into spec providers list (deduplicated).
  - Policy `secret_mounts` merge into spec file secrets, CLI target paths take precedence.
- Static field enforcement tests verify `validate_static_fields_unchanged()` rejects
  changes to `secrets` and `secret_mounts` on live sandboxes.
- OPA data isolation tests confirm `proto_to_opa_data_json()` excludes both `secrets` and
  `secret_mounts` from OPA data.
- Policy hashing tests confirm `deterministic_policy_hash()` includes `secrets` fields.
