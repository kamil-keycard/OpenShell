## Problem Statement

Sandboxes need access to SSH private keys and GPG keyrings for operations like git signing and SSH-based repository cloning. Today, the Keycard integration supports only environment variable injection (API keys via token exchange), which doesn't fit file-based secrets that require mounting at specific filesystem paths with correct permissions. The policy language has no concept of secret mounts, and the Keycard provider has no mechanism to deliver file content to the sandbox filesystem.

## Technical Context

The current secret pipeline is built entirely around environment variables: the CLI passes `--secret KEY=URN` mappings, the gateway resolves them via Keycard token exchange into `access_token` strings, and the sandbox supervisor injects them as env vars with proxy-time rewriting. SSH private keys (~3KB PEM files requiring `0600` permissions) and GPG keyrings (binary blobs requiring `0700` directories) don't fit this model — they need to be written to specific filesystem paths inside the sandbox before the process starts and before Landlock enforcement locks down the filesystem.

## Affected Components

| Component | Key Files | Role |
|-----------|-----------|------|
| Policy language (serde types) | `crates/openshell-policy/src/lib.rs` (`PolicyFile` struct, line ~27-39) | Needs a new `secret_mounts` field in the YAML schema |
| Proto definitions | `proto/datamodel.proto` (`SandboxSpec`, line ~26-39), `proto/sandbox.proto` (`SandboxPolicy`, line ~9-19) | Needs `file_secrets` map on `SandboxSpec` and/or `SecretMount` message |
| Keycard HTTP client | `crates/openshell-server/src/keycard.rs` (`exchange_token()`, line ~249-280) | No change needed — reused as-is. The `access_token` carries base64-encoded file content for `urn:secret-b64:` URNs. |
| Gateway sandbox creation | `crates/openshell-server/src/grpc.rs` (`resolve_provider_environment()`, line ~3790-3870) | Needs parallel `resolve_file_secrets()` function |
| Sandbox supervisor | `crates/openshell-sandbox/src/lib.rs` (startup, line ~190-205) | Must receive file secrets, write to disk, set permissions before Landlock applies |
| Sandbox policy (Rust types) | `crates/openshell-sandbox/src/policy.rs` (line ~14-20) | Needs `secret_mounts` field |
| Landlock enforcement | `crates/openshell-sandbox/src/sandbox/linux/landlock.rs` | Secret mount paths must be auto-added to `read_only` |
| Child env setup | `crates/openshell-sandbox/src/child_env.rs` | Needs `ssh_env_vars()` / `gpg_env_vars()` for companion env vars (`GNUPGHOME`, etc.) |
| CLI | `crates/openshell-cli/src/main.rs` (line ~1144-1148), `crates/openshell-cli/src/run.rs` (line ~2022-2028) | Needs `--file-secret PATH=URN` flag |
| Policy validation | `crates/openshell-policy/src/lib.rs` (`validate_sandbox_policy()`, line ~542-616) | Needs validation for mount paths (absolute, no traversal, path length limits) |
| Secret resolver | `crates/openshell-sandbox/src/secrets.rs` | No change needed — file secrets bypass the proxy rewriting pipeline |

## Technical Investigation

### Architecture Overview

The current end-to-end secret flow:

```
CLI: --secret ANTHROPIC_API_KEY=urn:resource:anthropic-api-key
  │
  ├── Parsed into SandboxSpec.secrets: HashMap<String, String>
  │   (proto: datamodel.proto:38 — map<string, string> secrets = 10)
  │
  └── CreateSandboxRequest → Gateway
        │
        Gateway: create_sandbox()
        ├── Validates secrets require a keycard provider
        ├── Provisions Keycard APPLICATION + ephemeral credentials
        └── Stores ephemeral creds in KeycardCredentialStore (in-memory)
              │
              Sandbox supervisor: run_sandbox()
              ├── Calls GetSandboxProviderEnvironment gRPC
              │   → Gateway: resolve_provider_environment() (grpc.rs:3790)
              │     ├── For keycard providers: exchange_token() per secret entry
              │     │   → POST https://{zone_id}.keycard.cloud/oauth/2/token
              │     │   → Returns access_token (the actual API key string)
              │     └── Inserts {env_key: access_token} into result map
              │
              ├── SecretResolver::from_provider_env(provider_env)
              │   → Replaces values with placeholders: "openshell:resolve:env:KEY"
              │   → Stores real values in supervisor-only memory
              │
              ├── Spawns child processes with placeholder env vars
              └── Proxy rewrites placeholders → real secrets in outbound HTTP
```

The entire pipeline assumes secrets are short strings suitable for env vars and HTTP header injection. SSH private keys and GPG keyrings fundamentally don't fit this model.

The sandbox startup sequence that constrains timing:

```
prepare_filesystem() → landlock::apply() → seccomp::apply() → exec()
```

File secrets must be written during `prepare_filesystem()` and their paths included in the Landlock `read_only` list before Landlock locks down the filesystem.

### Code References

| Location | Description |
|----------|-------------|
| `crates/openshell-policy/src/lib.rs:27-39` | `PolicyFile` struct — the serde schema for policy YAML. Uses `deny_unknown_fields`, so adding a field requires explicit schema change. |
| `crates/openshell-policy/src/lib.rs:233-248` | `to_proto()` — maps `PolicyFile` to proto `SandboxPolicy`. New field needs mapping here. |
| `crates/openshell-policy/src/lib.rs:255-356` | `from_proto()` — reverse mapping from proto to `PolicyFile`. |
| `crates/openshell-policy/src/lib.rs:542-616` | `validate_sandbox_policy()` — validation for paths (absolute, no traversal). Pattern to extend for mount paths. |
| `proto/datamodel.proto:38` | `map<string, string> secrets = 10` on `SandboxSpec`. Insertion point for `file_secrets`. |
| `proto/sandbox.proto:9-19` | `SandboxPolicy` message. Could host a `SecretMount` sub-message. |
| `crates/openshell-server/src/grpc.rs:3790-3870` | `resolve_provider_environment()` — iterates sandbox secrets, does Keycard token exchange. Template for `resolve_file_secrets()`. |
| `crates/openshell-server/src/keycard.rs:249-280` | `exchange_token()` — returns `access_token` string. May need a variant for file content. |
| `crates/openshell-sandbox/src/lib.rs:190-205` | Sandbox startup: fetches provider env. Insertion point for file secret fetch. |
| `crates/openshell-sandbox/src/sandbox/linux/landlock.rs:15-27` | `apply()` — builds Landlock ruleset from `read_only` and `read_write` paths. Must include secret mount paths. |
| `crates/openshell-sandbox/src/child_env.rs:8-36` | `proxy_env_vars()` / `tls_env_vars()` — pattern for `ssh_env_vars()` / `gpg_env_vars()`. |
| `crates/openshell-cli/src/main.rs:1144-1148` | CLI `--secret` flag definition. Pattern for `--file-secret`. |
| `crates/openshell-cli/src/run.rs:2022-2028` | Secret flag parsing. Pattern for file secret parsing. |

### Current Behavior

**Env var secrets**: The CLI parses `--secret KEY=URN` pairs into `SandboxSpec.secrets` (a `map<string, string>`). On sandbox creation, the gateway validates that a Keycard provider is attached, provisions a Keycard APPLICATION with SPIFFE ID, and stores ephemeral credentials. At sandbox startup, the supervisor calls `resolve_provider_environment()` which does a Keycard token exchange per secret entry, returning the actual API key as an `access_token` string. The `SecretResolver` replaces these with opaque placeholders in the child env, and the supervisor-side proxy rewrites placeholders back to real values in outbound HTTP requests. Secrets never touch disk and are held only in supervisor memory.

**File-based secrets**: No support exists. There is no policy schema for declaring mount paths, no mechanism to fetch file content from Keycard, no filesystem preparation logic for secret files, and no Landlock integration for secret mount paths.

### What Would Need to Change

**Proto/Data Model (Low effort)**
- Add `map<string, string> file_secrets = N` to `SandboxSpec` in `datamodel.proto` (key=target path, value=Keycard resource URN)
- Or add `repeated SecretMount secret_mounts` to `SandboxPolicy` in `sandbox.proto` with fields for `source_urn`, `target_path`, `mode`
- Add a response field (or new RPC) for delivering resolved file content from gateway to sandbox supervisor

**Policy Language (Medium effort)**
- Add `secret_mounts` section to `PolicyFile` serde struct with a `SecretMountDef` struct (source URN, target path, optional mode)
- Add validation rules: paths must be absolute, no `..` traversal, path length limits, target path must be within writable sandbox areas
- Round-trip serialization support (`to_proto()` / `from_proto()`)

**Keycard Client (Low effort)**
- Secrets intended for file mounting must be stored in Keycard as base64-encoded blobs and referenced with a `urn:secret-b64:` prefix (e.g., `urn:secret-b64:ssh-private-key`).
- The existing `exchange_token()` flow is reused as-is — the `access_token` returned by Keycard carries the base64-encoded content.
- The gateway's `resolve_file_secrets()` detects the `urn:secret-b64:` prefix, performs the standard token exchange, then base64-decodes the `access_token` value to recover the raw file bytes.
- This convention eliminates Keycard API changes: the platform already stores and returns arbitrary string values via token exchange. The encoding/decoding contract is between the user who stores the secret and OpenShell which consumes it.

**Gateway (Medium effort)**
- New `resolve_file_secrets()` function parallel to `resolve_provider_environment()`
- Per file secret: strip the `urn:secret-b64:` prefix to get the resource URN, call `exchange_token()`, base64-decode the returned `access_token` to get raw file bytes
- Validate: decoded content is non-empty, within a size limit (e.g., 256KB), and the target path passes validation
- New gRPC response field carrying resolved file content (still base64-encoded for transport) to the sandbox supervisor

**Sandbox Supervisor (High effort)**
- Receive resolved file secrets from gateway (via gRPC response)
- Write secret files to disk during `prepare_filesystem()` with correct ownership (`sandbox:sandbox`) and permissions (`0600` for keys, `0700` for GPG dirs)
- Auto-inject mount paths into Landlock `read_only` list before `landlock::apply()`
- Set companion env vars: `SSH_AUTH_SOCK` or `GIT_SSH_COMMAND` for SSH, `GNUPGHOME` for GPG
- Zeroize file contents on sandbox exit (secret cleanup)
- All file writes must happen BEFORE Landlock enforcement

**CLI (Low effort)**
- Add `--file-secret PATH=URN` flag (e.g., `--file-secret /sandbox/.ssh/id_ed25519=urn:secret-b64:ssh-private-key`)
- Validate that the URN uses the `urn:secret-b64:` prefix (reject plain `urn:resource:` for file secrets)
- Parse into `SandboxSpec.file_secrets`

### Alternative Approaches Considered

**Alternative A: Treat SSH keys as large env vars (zero code changes)**

Use the existing `--secret SSH_KEY=urn:resource:ssh-key` mechanism. The SSH key content lands in an env var. A startup script inside the container writes it to `~/.ssh/id_ed25519` and sets permissions. Works today.

- Pros: No codebase changes. Immediate.
- Cons: Pushes complexity to users. Env vars have size limits. PEM content visible in `/proc/*/environ`. Doesn't handle GPG keyrings (binary). Poor UX.

**Alternative B: Full file secret pipeline (proposed)**

Build the complete pipeline: CLI flag → proto field → gateway resolution → supervisor file write → Landlock integration.

- Pros: First-class UX, secure file permissions, clean policy-level declaration. No Keycard API changes needed thanks to `urn:secret-b64:` convention.
- Cons: Medium effort, new security surface (file permissions, Landlock interaction).

**Alternative C: Bake keys into container images**

Use `openshell image push` to build custom images with keys pre-baked.

- Pros: Simple, no runtime secret resolution.
- Cons: Secrets in images is a security anti-pattern. No per-sandbox isolation. Keys aren't ephemeral.

**Recommendation**: Implement Alternative B with the `urn:secret-b64:` convention — no Keycard API changes required since the base64 encoding is a user-side convention and the standard token exchange carries the content.

### Patterns to Follow

| Pattern | Where | Apply to |
|---------|-------|----------|
| `SandboxSpec.secrets` field + `--secret` CLI flag | `datamodel.proto:38`, `main.rs:1147`, `run.rs:2022` | New `file_secrets` field + `--file-secret` flag |
| `resolve_provider_environment()` with Keycard token exchange | `grpc.rs:3790-3870` | New `resolve_file_secrets()` |
| `prepare_filesystem()` with chown | `lib.rs` | Write secret files with correct ownership/perms |
| `validate_sandbox_policy()` path validation | `policy/lib.rs:542` | Validate mount paths |
| `deny_unknown_fields` on serde types | `policy/lib.rs:28` | Backward compat when adding new fields |
| `proxy_env_vars()` / `tls_env_vars()` | `child_env.rs:8-36` | New `ssh_env_vars()` / `gpg_env_vars()` |
| Keycard wiremock tests | `keycard.rs:560-937` | Mock file content exchange |
| Keycard lifecycle (provision → use → cleanup) | `grpc.rs` sandbox create/delete | Extend to file secret lifecycle |

## Proposed Approach

Add a `file_secrets` map to `SandboxSpec` (parallel to the existing `secrets` field) and a `--file-secret PATH=URN` CLI flag. File secret URNs use a `urn:secret-b64:` prefix convention — the user stores the secret in Keycard as a base64-encoded blob, and OpenShell decodes it after token exchange. This reuses the existing Keycard token exchange pipeline without requiring any Keycard API changes. The gateway's `resolve_file_secrets()` strips the prefix, calls `exchange_token()`, base64-decodes the `access_token`, and delivers the raw content inline in the gRPC response. The sandbox supervisor writes secret files to disk during `prepare_filesystem()` with `0600` permissions and `sandbox:sandbox` ownership, auto-injects the mount paths into the Landlock `read_only` list, and sets companion env vars (`GNUPGHOME`, `GIT_SSH_COMMAND`) so tools discover the keys without user configuration. The policy YAML optionally gains a `secret_mounts` section for declarative auditing, but the actual bindings remain on `SandboxSpec` to keep runtime concerns separate from static policy.

## Scope Assessment

- **Complexity:** Medium
- **Confidence:** High — the `urn:secret-b64:` convention reuses existing Keycard token exchange with no API changes
- **Estimated files to change:** 10-12
- **Issue type:** `feat`

## Risks & Open Questions

- **Base64 encoding contract:** File secrets must be stored in Keycard as base64-encoded blobs and referenced with `urn:secret-b64:` URNs. This is a user-side convention — if the stored value is not valid base64, the gateway will fail at decode time with a clear error. Documentation must explain the encoding requirement and provide examples (e.g., `cat ~/.ssh/id_ed25519 | base64` when storing the secret in Keycard).
- **Where do file secret declarations live?** Option A: `SandboxSpec.file_secrets` (matches existing `secrets` pattern, per-sandbox, not in policy YAML). Option B: `SandboxPolicy.secret_mounts` (auditable in policy YAML, but mixes runtime and static concerns). Recommendation: A for bindings, with optional policy YAML section for auditing.
- **Secret transport size:** SSH keys are ~3KB and fit easily inline in gRPC responses. GPG keyrings can be larger. Is there a practical upper bound? For v1, inline gRPC is sufficient; Kubernetes Secret volumes are a follow-up for large payloads.
- **Landlock interaction risk:** If a secret mount path falls under a `read_write` directory (e.g., `/sandbox/.ssh/`), the sandbox process could overwrite the mounted secret. Mitigation: mount secrets to a dedicated read-only path like `/etc/openshell-secrets/` and set env vars to point there, or ensure the parent directory is in `read_only`.
- **Secret zeroization:** File secrets break the current property that secrets never touch disk. Risk of secret content surviving in the container filesystem. Mitigation: write to `tmpfs` if available, or zeroize file contents on sandbox exit.
- **File permissions model:** SSH keys require `0600`, GPG directories require `0700`. Default `0600` with optional `mode` override, or hardcode by convention based on path pattern?
- **Static vs. dynamic:** File mounts should be static (applied at sandbox creation, immutable). This means `policy set` cannot update file secrets on a running sandbox. Is this acceptable?

## Test Considerations

- **Policy parsing:** Round-trip YAML↔proto tests for the new `secret_mounts` field. Existing pattern at `openshell-policy/src/lib.rs:657-1229`.
- **Policy validation:** Mount path validation (absolute paths, no traversal, length limits). Existing pattern at `openshell-policy/src/lib.rs:542-616`.
- **Keycard client:** Wiremock-based tests for token exchange returning base64-encoded content. Existing pattern at `keycard.rs:560-937`.
- **Base64 decode:** Gateway correctly decodes valid base64 from `access_token`, rejects invalid base64 with a clear error.
- **Gateway resolution:** `resolve_file_secrets()` unit tests, parallel to existing `resolve_provider_environment` tests at `grpc.rs:5068-5564`.
- **Sandbox supervisor:** File writing with correct permissions, Landlock integration, companion env var injection.
- **CLI:** `--file-secret` flag parsing tests.
- **E2E:** Sandbox with mounted SSH key can `git clone` from GitHub via SSH. Sandbox with mounted GPG key can sign commits.
- **Existing test infra is comprehensive** — all affected modules have test patterns to extend rather than create from scratch.

---
*Created by spike investigation. Use `build-from-issue` to plan and implement.*
