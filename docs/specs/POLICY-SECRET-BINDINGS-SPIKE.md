## Problem Statement

Creating a sandbox with Keycard-managed secrets currently requires manually specifying `--secret`, `--file-secret`, and `--provider` flags on the CLI alongside a `--policy` file. The policy already declares filesystem access, network endpoints, and process configuration — it should also be the single source of truth for secret bindings. Today, a sandbox with full Keycard integration looks like this:

```bash
openshell sandbox create \
  --name ironman \
  --policy docs/policies/claude-policy.yaml \
  --secret ANTHROPIC_API_KEY=urn:secret:claude-api \
  --file-secret /sandbox/.ssh/id_ed25519=urn:secret-b64:ssh:private-key \
  --provider keyvengers \
  -- claude
```

The desired experience is:

```bash
openshell sandbox create \
  --name ironman \
  --policy docs/policies/claude-policy.yaml \
  -- claude
```

With all secret and provider configuration expressed in the policy YAML.

## Technical Context

Three separate CLI flags feed three independent fields on `SandboxSpec`:

| CLI Flag | Proto Field | Purpose |
|----------|------------|---------|
| `--secret KEY=URN` | `SandboxSpec.secrets` (`map<string, string>`) | Env var secrets resolved via Keycard token exchange |
| `--file-secret PATH=URN` | `SandboxSpec.file_secrets` (`map<string, string>`) | File secrets written to sandbox filesystem at startup |
| `--provider NAME` | `SandboxSpec.providers` (`repeated string`) | Keycard provider(s) for credential resolution |

The policy YAML has partial coverage: `secret_mounts` already exists as a top-level field in both the YAML schema and the `SandboxPolicy` proto, but it is documented as a "declarative audit trail" — the gateway does not extract or use it during sandbox creation. Env var secrets and the provider binding have no policy representation at all.

The gateway's `resolve_provider_environment()` already accepts `sandbox_secrets` as a parameter and doesn't care where the secrets came from. Similarly, `resolve_file_secrets()` just needs a map of `(target_path, source_urn)` pairs. Both are clean integration points.

## Affected Components

| Component | Key Files | Role |
|-----------|-----------|------|
| Policy engine | `crates/openshell-policy/src/lib.rs` | YAML parsing (`PolicyFile`), proto conversion, validation |
| Proto definitions | `proto/sandbox.proto` (`SandboxPolicy`, `SecretMount`), `proto/datamodel.proto` (`SandboxSpec`) | Policy and spec proto messages |
| CLI | `crates/openshell-cli/src/run.rs` | `--secret`, `--file-secret`, `--provider` flag parsing, `CreateSandboxRequest` assembly |
| Gateway server | `crates/openshell-server/src/grpc.rs` | `create_sandbox()` validation, `resolve_provider_environment()`, `resolve_file_secrets()` |
| Keycard client | `crates/openshell-server/src/keycard.rs` | HTTP client for Keycard token exchange |
| Sandbox supervisor | `crates/openshell-sandbox/src/opa.rs` | OPA data loading — must strip secrets before Rego evaluation |
| Architecture docs | `architecture/security-policy.md`, `architecture/sandbox-providers.md` | Policy schema reference and provider lifecycle docs |

## Technical Investigation

### Architecture Overview

The sandbox creation pipeline has three parallel data flows that converge at the gateway:

**Policy flow:** CLI loads YAML → `serde_yml` deserializes to `PolicyFile` (with `#[serde(deny_unknown_fields)]`) → `to_proto()` converts to `SandboxPolicy` proto → embedded in `SandboxSpec.policy` → sent via gRPC → persisted → loaded by sandbox supervisor → converted to OPA JSON for Rego evaluation.

**Env var secrets flow:** CLI parses `--secret KEY=URN` flags → `SandboxSpec.secrets` → gateway validates Keycard provider is attached → provisions Keycard APPLICATION → sandbox calls `GetSandboxProviderEnvironment` → gateway does token exchange → sandbox injects tokens as env vars with placeholder/proxy resolution.

**File secrets flow:** CLI parses `--file-secret PATH=URN` flags → `SandboxSpec.file_secrets` → gateway does token exchange + base64 decode → delivers raw content to sandbox supervisor → supervisor writes to disk during `prepare_filesystem()` with `0600` permissions before Landlock applies → auto-adds paths to `read_only`.

**Provider flow:** CLI resolves `--provider NAME` → `SandboxSpec.providers` → gateway validates provider exists → used by `resolve_provider_environment()` for credential injection and Keycard token exchange.

All four flows are assembled independently at the CLI and transmitted as separate fields on `SandboxSpec`. The gateway never looks at the policy to determine secrets, file mounts, or providers.

### Code References

| Location | Description |
|----------|-------------|
| `crates/openshell-policy/src/lib.rs:27-39` | `PolicyFile` struct — top-level serde type with `deny_unknown_fields`. Already has `secret_mounts` field. |
| `crates/openshell-policy/src/lib.rs:156-249` | `to_proto()` — YAML → proto conversion. Already maps `secret_mounts`. |
| `crates/openshell-policy/src/lib.rs:255-356` | `from_proto()` — proto → YAML round-trip. Already maps `secret_mounts`. |
| `crates/openshell-policy/src/lib.rs:542-616` | `validate_sandbox_policy()` — validates `secret_mounts` paths (absolute, no traversal, length). |
| `proto/sandbox.proto:9-20` | `SandboxPolicy` — already has `repeated SecretMount secret_mounts`. Needs new `PolicySecrets` field. |
| `proto/sandbox.proto` | `SecretMount` message — `source_urn`, `target_path`, `mode`. Already defined. |
| `proto/datamodel.proto:26-39` | `SandboxSpec` — has `policy`, `secrets`, `file_secrets`, `providers` as separate fields. |
| `crates/openshell-cli/src/run.rs:2022-2028` | CLI `--secret` parsing → HashMap |
| `crates/openshell-cli/src/run.rs` | CLI `--file-secret` parsing → HashMap |
| `crates/openshell-cli/src/run.rs:2057-2067` | `CreateSandboxRequest` assembly with separate secrets, file_secrets, providers, policy |
| `crates/openshell-server/src/grpc.rs:179-300` | `create_sandbox()` — validates secrets require Keycard provider, provisions APPLICATION |
| `crates/openshell-server/src/grpc.rs:3790-3876` | `resolve_provider_environment()` — Keycard token exchange, parameterized on `sandbox_secrets` |
| `crates/openshell-server/src/grpc.rs:3291-3305` | `validate_sandbox_spec()` — validates secret key/value format |
| `architecture/security-policy.md:640-672` | `secret_mounts` schema docs — describes field as "declarative audit trail" |
| `architecture/sandbox-providers.md:393-461` | Keycard per-sandbox secrets model and token exchange flow |

### Current Behavior

**Env var secrets (`--secret`):**
1. CLI parses `--secret ANTHROPIC_API_KEY=urn:secret:claude-api` into `SandboxSpec.secrets`
2. Gateway validates a Keycard provider is attached
3. Gateway provisions Keycard APPLICATION, stores ephemeral credentials
4. At sandbox startup, `resolve_provider_environment()` exchanges ephemeral creds for real tokens per secret entry
5. Real tokens are injected as placeholder env vars; proxy resolves placeholders in outbound HTTP

**File secrets (`--file-secret`):**
1. CLI parses `--file-secret /sandbox/.ssh/id_ed25519=urn:secret-b64:ssh:private-key` into `SandboxSpec.file_secrets`
2. Gateway exchanges credentials, base64-decodes `access_token` to raw file bytes
3. Sandbox supervisor writes file during `prepare_filesystem()` with `0600` and `sandbox:sandbox` ownership
4. Path auto-added to Landlock `read_only`; companion env vars set (`GIT_SSH_COMMAND`, `GNUPGHOME`)

**`secret_mounts` in policy YAML:**
- Already exists in the schema and proto (`SandboxPolicy.secret_mounts`)
- Already has validation (path must be absolute, no traversal, etc.)
- Currently described as "declarative audit trail" — the gateway does NOT extract these for actual file secret resolution
- Actual file secret bindings come exclusively from `SandboxSpec.file_secrets` (CLI `--file-secret`)

**Provider (`--provider`):**
- No representation in the policy YAML
- Only set via CLI → `SandboxSpec.providers`

### What Would Need to Change

**New `secrets` top-level field in policy YAML:** Add a `secrets` block to `PolicyFile` containing:
- `provider`: name of the Keycard provider to use for resolving all secrets in this policy
- `env`: map of env var name → resource URN (replaces `--secret`)

This is a new `SecretsDef` serde struct, mapped to a new `PolicySecrets` proto message on `SandboxPolicy`.

**Make existing `secret_mounts` functional:** The `secret_mounts` field already exists in the YAML schema, proto, and validation pipeline. The missing piece is gateway-side extraction: `create_sandbox()` needs to read `policy.secret_mounts` and merge them into the file secrets resolution path, rather than relying exclusively on `SandboxSpec.file_secrets`.

**Provider extraction from policy:** When the policy has a `secrets.provider` field, the gateway should use it to resolve the Keycard provider without requiring `--provider` on the CLI. The `--provider` CLI flag remains supported for backward compatibility and for non-Keycard providers.

**Server-side merging in `create_sandbox()`:** A new extraction step that reads the parsed `SandboxPolicy` and:
1. Extracts `secrets.env` → merges into `SandboxSpec.secrets` (CLI takes precedence on conflict)
2. Extracts `secret_mounts` → merges into `SandboxSpec.file_secrets` (CLI takes precedence on conflict)
3. Extracts `secrets.provider` → adds to `SandboxSpec.providers` if not already present

**CLI:** Make `--secret`, `--file-secret`, and `--provider` optional when the policy declares them. No breaking changes — all flags continue to work.

**OPA data loading:** `proto_to_opa_data_json()` must strip the new `secrets` field (and continue to strip `secret_mounts`) before feeding to the OPA engine. Neither field is relevant to network evaluation.

**Static field enforcement:** `secrets` and `secret_mounts` are both static fields — they cannot be changed via live policy updates (`openshell policy set`). Both are applied at sandbox startup (secret resolution and file writes happen once). `validate_static_fields_unchanged()` must include the new `secrets` field.

### Alternative Approaches Considered

**Rejected: Secrets per network policy rule —** Coupling secrets to `NetworkPolicyRule` would scope them to network access only. File secrets serve filesystem purposes (SSH keys, GPG keyrings) that have nothing to do with network policies. Env var secrets may be needed for non-network use cases in the future. Rejected in favor of a top-level block.

**Rejected: Fold `secret_mounts` into the `secrets` block —** Could unify all secret types under one top-level key (`secrets.env`, `secrets.files`). This would be cleaner for new users but requires migrating the existing `secret_mounts` field, which is already in the proto and YAML schema. The migration cost isn't worth it — keeping `secret_mounts` as a separate top-level field is backward compatible and already validated.

**Rejected: Companion secrets file —** A `--secrets-file` CLI flag doesn't solve the core pain — the user still needs extra flags. The policy is the single source of truth.

### Patterns to Follow

- All policy serde types use `#[serde(deny_unknown_fields)]` — the new `secrets` field must be explicitly added
- All optional policy fields use `#[serde(default, skip_serializing_if = ...)]` — follow this for `secrets`
- `secret_mounts` already has full to_proto/from_proto and validation — follow the same pattern for `secrets`
- Round-trip tests (`round_trip_preserves_*`) — add one for the `secrets` block
- Static field validation in `validate_static_fields_unchanged()` — add `secrets`
- Policy hashing in `deterministic_policy_hash()` — include `secrets` field

## Proposed Approach

Add a top-level `secrets` block to the policy YAML for env var secrets and provider binding. Promote the existing `secret_mounts` from audit-only to functional. The combined policy becomes the single source of truth:

```yaml
version: 1

secrets:
  provider: keyvengers
  env:
    ANTHROPIC_API_KEY: urn:secret:claude-api

secret_mounts:
  - source_urn: "urn:secret-b64:ssh:private-key"
    target_path: "/sandbox/.ssh/id_ed25519"
    mode: "0600"

filesystem_policy:
  # ...
network_policies:
  claude_code:
    # ...
```

During sandbox creation, the gateway extracts `secrets.env` and `secret_mounts` from the parsed policy, merges them with any CLI-provided `--secret` and `--file-secret` flags (CLI takes precedence on conflict), resolves the Keycard provider from `secrets.provider` (or CLI `--provider`), and feeds the combined set into the existing `resolve_provider_environment()` and `resolve_file_secrets()` flows. All CLI flags remain supported for backward compatibility. OPA data loading strips both `secrets` and `secret_mounts` before Rego evaluation. Both fields are static — immutable after sandbox creation.

## Scope Assessment

- **Complexity:** Medium
- **Confidence:** High — `secret_mounts` already has schema, proto, and validation; `resolve_provider_environment()` and `resolve_file_secrets()` are already parameterized on input maps
- **Estimated files to change:** ~10 (proto, policy crate serde+validation, CLI, server grpc extraction/merge, OPA data stripping, policy hashing, static field validation, architecture docs, policy example, tests)
- **Issue type:** `feat`

## Risks & Open Questions

- **Decided: Top-level `secrets` block + existing `secret_mounts`.** Env var secrets and provider go in a new `secrets` field. File secrets use the existing `secret_mounts` field, promoted from audit-only to functional. Both are decoupled from network policies.
- **Provider binding strategy:** The proposed approach uses an explicit `provider` field inside the `secrets` block. This couples the policy to a named provider on the gateway. Alternative: implicit resolution (use the only Keycard provider, or a gateway default). Explicit is simpler and more predictable, but policies aren't portable across gateways with different provider names. Needs human decision on whether portability matters.
- **Merge semantics on conflict:** When both the policy and CLI provide the same secret key (e.g., `ANTHROPIC_API_KEY`), which wins? Proposed: CLI takes precedence (explicit override). Same for `secret_mounts` — a CLI `--file-secret` for the same `target_path` overrides the policy's `secret_mount`. Needs confirmation.
- **`secret_mounts` promotion backward compatibility:** Existing policies with `secret_mounts` are audit-only today. After this change, they become functional — the gateway will actually attempt to resolve those URNs via Keycard. Policies that used `secret_mounts` as documentation without a working Keycard provider will start failing. Mitigation: only extract `secret_mounts` when a Keycard provider is available (either from `secrets.provider` or `--provider`). If no provider, treat `secret_mounts` as audit-only (current behavior) with a warning.
- **Provider required for `secret_mounts` too:** Currently `secret_mounts` is audit-only and doesn't require a provider. Once functional, it needs a Keycard provider. Should `secret_mounts` inherit the `secrets.provider`, or have its own? Proposed: inherit from `secrets.provider`. If `secret_mounts` exist without a `secrets` block (or without `secrets.provider`), require `--provider` on the CLI.
- **Live policy update:** Both `secrets` and `secret_mounts` are static fields — immutable after creation. `validate_static_fields_unchanged()` must enforce this.
- **Draft policy guardrails:** The draft policy approval system must block changes to the `secrets` field to prevent agents from injecting unauthorized credentials.
- **OPA data isolation (Critical):** `proto_to_opa_data_json()` must strip both `secrets` and `secret_mounts` before loading into OPA. Secret URNs must not leak into Rego data.
- **Deterministic policy hashing:** `deterministic_policy_hash()` must include the new `secrets` field for idempotent update detection.

## Test Considerations

- **Policy parsing:** Unit tests for YAML with `secrets` block (valid, missing provider, empty env, invalid URN format). Follow existing inline YAML pattern in `crates/openshell-policy/src/lib.rs`.
- **Round-trip fidelity:** `round_trip_preserves_secrets` — parse with `secrets` → to_proto → from_proto → compare. Existing `secret_mounts` round-trip tests should already exist.
- **Validation:** URN format validation for `secrets.env` values. Provider name non-empty validation. Existing `secret_mounts` validation already covers path safety.
- **Server-side extraction and merging:** Test policy-derived secrets merged with CLI secrets (both env and file). Verify CLI takes precedence on conflict. Verify provider is extracted from policy. Verify `secret_mounts` are extracted and fed to `resolve_file_secrets()`.
- **Backward compatibility:** Existing policies without `secrets` continue to parse. Existing `secret_mounts`-only policies without a provider either work with `--provider` on CLI or warn/skip gracefully.
- **OPA data isolation:** Assert `proto_to_opa_data_json()` output contains neither `secrets` nor `secret_mounts`.
- **Static field enforcement:** Test that `UpdateSandboxPolicy` rejects changes to the `secrets` field.
- **Policy hashing:** Test that adding/removing `secrets` changes the deterministic hash.
- **E2E:** Sandbox created with only `--policy` (no `--secret`, `--file-secret`, or `--provider`) gets env var secrets and file mounts from the policy's `secrets` and `secret_mounts` blocks.
- **Existing patterns:** 30+ policy parsing tests, 30+ secret resolver tests, 10+ Keycard wiremock tests, 6+ `resolve_provider_environment` tests, `secret_mounts` validation tests. Follow these patterns.

---
*Created by spike investigation. Use `build-from-issue` to plan and implement.*
