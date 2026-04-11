## Problem Statement

Creating a sandbox with Keycard-managed secrets currently requires manually specifying `--secret ENV_VAR=urn:secret:resource --provider provider-name` on the CLI, even though the policy file already declares which network endpoints the sandbox can reach. This is redundant — if the policy says "this sandbox talks to `api.anthropic.com`," it should also be able to declare "and it needs `ANTHROPIC_API_KEY` from Keycard to do so." The policy language should be extended to embed secret bindings, reducing sandbox creation to a single `--policy` flag.

## Technical Context

The policy and secrets systems are entirely independent today. The policy flows through `SandboxPolicy` (proto) and controls network access via OPA evaluation in the sandbox supervisor. Secrets flow through `SandboxSpec.secrets` and are resolved at runtime via Keycard token exchange in the gateway. They converge only at the sandbox level, where the proxy replaces placeholder tokens in outbound HTTP headers with real credentials. Because the policy already defines the network context (which hosts, which protocols, which enforcement level), it's the natural place to also declare which credentials are needed for those hosts.

## Affected Components

| Component | Key Files | Role |
|-----------|-----------|------|
| Policy engine | `crates/openshell-policy/src/lib.rs` | YAML parsing, proto conversion, validation |
| Proto definitions | `proto/sandbox.proto`, `proto/datamodel.proto` | `NetworkPolicyRule`, `SandboxPolicy`, `SandboxSpec` |
| CLI | `crates/openshell-cli/src/run.rs` | `--secret` and `--provider` flag parsing, `CreateSandboxRequest` assembly |
| Gateway server | `crates/openshell-server/src/grpc.rs` | Sandbox creation, secret validation, Keycard provisioning, provider environment resolution |
| Keycard client | `crates/openshell-server/src/keycard.rs` | HTTP client for Keycard token exchange |
| Sandbox supervisor | `crates/openshell-sandbox/src/opa.rs` | OPA data loading from policy proto |

## Technical Investigation

### Architecture Overview

The sandbox creation pipeline has two parallel data flows:

**Policy flow:** CLI loads YAML → `serde_yml` deserializes to `PolicyFile` (with `#[serde(deny_unknown_fields)]`) → `to_proto()` converts to `SandboxPolicy` proto → embedded in `SandboxSpec` → sent via gRPC `CreateSandboxRequest` → persisted in gateway DB → loaded by sandbox supervisor → converted to OPA JSON for Rego evaluation.

**Secrets flow:** CLI parses `--secret KEY=URN` flags into `HashMap<String, String>` → embedded in `SandboxSpec.secrets` → sent via gRPC → gateway validates secrets require a Keycard provider → provisions Keycard APPLICATION → persists spec → sandbox calls `GetSandboxProviderEnvironment` → gateway exchanges ephemeral creds for real tokens via Keycard API → sandbox injects tokens as env vars and placeholder replacements.

The `resolve_provider_environment()` function already accepts `sandbox_secrets` as a parameter — it doesn't care whether secrets came from CLI flags or a policy. This is the clean integration point.

### Code References

| Location | Description |
|----------|-------------|
| `crates/openshell-policy/src/lib.rs:27-39` | `PolicyFile` struct — top-level YAML serde type with `deny_unknown_fields` |
| `crates/openshell-policy/src/lib.rs:68-77` | `NetworkPolicyRuleDef` — YAML representation of a network policy rule (unchanged — secrets are top-level, not per-rule) |
| `crates/openshell-policy/src/lib.rs:156-249` | `to_proto()` — YAML → proto conversion, must map new fields |
| `crates/openshell-policy/src/lib.rs:255-356` | `from_proto()` — proto → YAML round-trip conversion |
| `crates/openshell-policy/src/lib.rs:542-616` | `validate_sandbox_policy()` — policy safety validation |
| `proto/sandbox.proto:47-54` | `NetworkPolicyRule` proto message (unchanged — secrets are top-level) |
| `proto/sandbox.proto:9-20` | `SandboxPolicy` proto message — would get a new `PolicySecrets` field |
| `proto/datamodel.proto:26-39` | `SandboxSpec` — contains both `policy` (field 7) and `secrets` (field 10) as separate fields today |
| `crates/openshell-cli/src/run.rs:2022-2028` | CLI secret parsing — `--secret KEY=URN` → HashMap |
| `crates/openshell-cli/src/run.rs:2057-2067` | `CreateSandboxRequest` assembly with separate `secrets` and `policy` |
| `crates/openshell-server/src/grpc.rs:179-300` | `create_sandbox()` — validates secrets require Keycard provider |
| `crates/openshell-server/src/grpc.rs:3790-3876` | `resolve_provider_environment()` — Keycard token exchange, already parameterized on `sandbox_secrets` |
| `crates/openshell-server/src/grpc.rs:3291-3305` | `validate_sandbox_spec()` — validates secret key/value format |

### Current Behavior

1. User passes `--secret ANTHROPIC_API_KEY=urn:secret:claude-api --provider keyvengers` on the CLI
2. CLI parses these into `SandboxSpec { secrets: {"ANTHROPIC_API_KEY": "urn:secret:claude-api"}, providers: ["keyvengers"], policy: Some(...) }`
3. Gateway's `create_sandbox()` validates: if secrets are non-empty, a Keycard provider must be attached
4. Gateway provisions a Keycard APPLICATION, stores ephemeral credentials
5. When the sandbox starts, it calls `GetSandboxProviderEnvironment` — the gateway exchanges ephemeral creds for real tokens via `resolve_provider_environment()`
6. Real tokens are injected as env vars and used for placeholder replacement in outbound HTTP headers

The policy and secrets are assembled and transmitted independently. The gateway never looks at the policy to determine what secrets are needed.

### What Would Need to Change

**Proto layer:** Add a new `PolicySecrets` message to `SandboxPolicy` as a top-level field, decoupled from network policies. This is an additive proto change (new field number), so it's wire-compatible. Secrets are intentionally kept separate from `NetworkPolicyRule` — they may serve purposes beyond network access in the future (e.g., filesystem mounts, process environment, inference credentials).

**Policy parsing:** Add a new `SecretsDef` struct and an `Option<SecretsDef>` field at the `PolicyFile` level. Update `to_proto()` and `from_proto()` for round-trip fidelity. Because `PolicyFile` uses `deny_unknown_fields`, the new field must be explicitly added — but as an `Option` with serde defaults, existing policies without secrets continue to parse fine.

**Server-side extraction:** A new function to extract secrets from the parsed policy and merge them with any CLI-provided secrets. Feed the merged set to `resolve_provider_environment()`. Update `create_sandbox()` validation to check for Keycard provider when policy-embedded secrets exist (not just `spec.secrets`).

**CLI:** Make `--secret` and `--provider` optional when the policy contains secret bindings. Potentially auto-infer the provider from the policy or from a gateway default.

**OPA data loading:** `proto_to_opa_data_json()` must strip `secrets` from the proto before feeding to the OPA engine. Secrets are not a network evaluation concern and must not leak into Rego data.

### Alternative Approaches Considered

**Rejected: Secrets per network policy rule —** Coupling secrets to `NetworkPolicyRule` would scope them to network access only. Secrets may be needed for other purposes in the future (filesystem mounts, process environment, inference credentials). This would require either duplicating secrets across policies or adding dedup logic. Rejected in favor of a top-level block that keeps secrets as a first-class, independent policy concern.

**Rejected: Companion secrets file —** A separate `--secrets-file` CLI flag would preserve separation of concerns but doesn't solve the core pain — the user still needs extra flags. The policy file is already the single source of truth for sandbox configuration; secrets belong there.

### Patterns to Follow

- All policy serde types use `#[serde(deny_unknown_fields)]` — new fields must be explicitly added
- All policy fields use `#[serde(default, skip_serializing_if = ...)]` for optional fields — follow this for secrets
- Round-trip tests (`round_trip_preserves_*`) ensure YAML → proto → YAML fidelity — add one for secrets
- Policy validation uses a `violations: Vec<String>` pattern — add URN format validation there
- Proto changes follow additive-only field numbering — use the next available field number

## Proposed Approach

Add a top-level `secrets` block to the policy YAML, decoupled from network policies:

```yaml
version: 1
secrets:
  provider: keyvengers
  bindings:
    ANTHROPIC_API_KEY: urn:secret:claude-api
network_policies:
  claude_code:
    ...
```

During sandbox creation, the gateway extracts secrets from the parsed policy, merges them with any CLI-provided secrets, and feeds the combined set to the existing `resolve_provider_environment()` flow. The `--secret` and `--provider` CLI flags remain supported for backward compatibility but become optional when the policy declares secrets. OPA data loading strips secrets before feeding to the Rego engine. Keeping secrets at the top level ensures they can serve future use cases beyond network access without schema redesign.

## Scope Assessment

- **Complexity:** Medium
- **Confidence:** High — clear path forward, clean integration point at `resolve_provider_environment()`
- **Estimated files to change:** ~8 (proto, policy crate, CLI, server grpc, OPA data loading, tests, architecture docs, policy example)
- **Issue type:** `feat`

## Risks & Open Questions

- **Decided: Top-level secrets block.** Secrets are decoupled from network policies to keep them extensible for future use cases beyond network access.
- **Provider binding strategy:** The policy needs to know which Keycard provider to use. Options: explicit `provider` field in the `secrets` block, implicit (use the only attached provider), or gateway-level default. If explicit, the policy becomes coupled to a specific gateway's provider configuration.
- **Live policy update semantics:** If a policy update adds new secret bindings, the gateway must perform a Keycard token exchange. If the exchange fails, the policy update fails. This is a new failure mode — policy updates today never trigger external API calls.
- **Draft policy system guardrails:** If agents can modify policies via the draft approval system, they could add secret bindings to inject credentials they shouldn't have. The draft system must block `secrets` fields (or require elevated approval).
- **Backward compatibility:** `#[serde(deny_unknown_fields)]` means a policy with `secrets` sent to an older gateway will fail to parse. Consider bumping `version: 1` → `version: 2`, or make the new fields optional with serde defaults (which they would be as `Option` types).
- **Secret URN leak surface:** Secret URNs (not actual secrets) in the policy are persisted in the DB and visible via `policy get --full`. This is acceptable (URNs are identifiers, not credentials) but should be documented.
- **OPA data isolation (Critical):** `proto_to_opa_data_json()` must strip secrets before loading into OPA. If secrets leak into OPA JSON, they become queryable via Rego.

## Test Considerations

- **Policy parsing:** Unit tests for YAML with secrets (valid, invalid URN format, missing fields). Follow existing inline YAML test pattern in `crates/openshell-policy/src/lib.rs`.
- **Round-trip fidelity:** `round_trip_preserves_secrets` test — parse with secrets → to_proto → from_proto → compare.
- **`deny_unknown_fields` still works:** Ensure policies with unknown fields (not secrets) still reject.
- **Server-side merging:** Test policy-derived secrets merged with CLI secrets, including conflict resolution (CLI wins? error?).
- **Validation:** URN format validation, provider requirement validation when policy has secrets.
- **OPA data isolation:** Assert that `proto_to_opa_data_json()` output does NOT contain any secrets fields.
- **Live update:** Test policy update that adds/removes secrets (if live update is in scope).
- **Existing patterns:** 30+ policy parsing tests, 30+ secret resolver tests, 10+ Keycard wiremock tests, 6+ `resolve_provider_environment` tests with in-memory SQLite. Follow these patterns.

---
*Created by spike investigation. Use `build-from-issue` to plan and implement.*
