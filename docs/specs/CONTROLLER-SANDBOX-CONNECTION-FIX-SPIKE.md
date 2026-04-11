## Problem Statement

The controller sandbox feature — where a sandbox can authenticate to and manage other sandboxes via the OpenShell gRPC API — was partially implemented across 8 commits on `kamil/keycard-integration`. The OIDC JWT verification module, auth interceptor, scope enforcement, and H2 gRPC proxy relay with credential injection are all built. However, the end-to-end flow fails: when a sandbox workload (e.g., the OpenClaw gateway) attempts to reach the OpenShell server through the proxy CONNECT tunnel, the connection dies immediately after TLS handshake with "Broken pipe" / "unexpected eof while reading". No HTTP response is ever received.

The feature is incomplete until: (1) gateway starts with OIDC config, (2) a controller sandbox is created with a Keycard-scoped token, (3) the sandbox can successfully reach the server's `/health` endpoint and gRPC methods, and (4) a child sandbox can be created from within the controller sandbox.

## Technical Context

The sandbox proxy (`openshell-sandbox`) performs MITM TLS termination for outbound HTTPS connections. When a sandbox workload sends `CONNECT openshell.openshell.svc.cluster.local:8080`, the proxy:
1. Accepts the CONNECT and responds `200 Connection Established`
2. Detects TLS ClientHello, terminates client-side TLS using an ephemeral sandbox CA
3. Attempts to connect TLS to the upstream server using Mozilla's public root CAs
4. Relays traffic between the two TLS sessions, performing L7 inspection and credential injection

Step 3 fails silently because the OpenShell server uses a cluster-internal self-signed certificate (signed by the cluster CA in the `openshell-server-tls` K8s secret), which is NOT in Mozilla's root store. The proxy-to-server TLS handshake fails, and the MITM connection closes — the curl client sees "broken pipe".

Additionally, the server requires mTLS by default (client certificate verification). The proxy's upstream TLS config uses `with_no_client_auth()` — it never presents a client certificate. Even if the root store issue were fixed, the server would reject the connection unless `--disable-gateway-auth` is explicitly passed.

## Affected Components

| Component | Key Files | Role |
|-----------|-----------|------|
| Proxy TLS upstream config | `crates/openshell-sandbox/src/l7/tls.rs` | Builds the rustls ClientConfig for proxy-to-server connections |
| Proxy TLS state init | `crates/openshell-sandbox/src/lib.rs:218-230` | Creates `ProxyTlsState` with `build_upstream_client_config()` |
| Proxy CONNECT handler | `crates/openshell-sandbox/src/proxy.rs:597-670` | TLS detection, termination, upstream connection, L7 relay dispatch |
| Server TLS config | `crates/openshell-server/src/tls.rs:37-75` | mTLS vs optional client cert based on `allow_unauthenticated` |
| Server main/CLI | `crates/openshell-server/src/main.rs:107-168` | `--disable-gateway-auth` and `--oidc-*` CLI flags |
| Gateway deploy pipeline | `crates/openshell-cli/src/run.rs:1408-1436` | Plumbs OIDC and disable-gateway-auth into `DeployOptions` |
| Cluster entrypoint | `deploy/docker/cluster-entrypoint.sh:493-530` | `DISABLE_GATEWAY_AUTH` and `OIDC_*` env → HelmChart manifest |
| H2 gRPC relay | `crates/openshell-sandbox/src/l7/grpc.rs:338-380` | Credential injection (works correctly, never reached) |
| Secret resolver | `crates/openshell-sandbox/src/secrets.rs:108-133` | `rewrite_header_value()` for placeholder → JWT replacement |
| OIDC module | `crates/openshell-server/src/oidc.rs` | JWT verification, JWKS fetching, auth interceptor (correctly implemented) |
| Sandbox supervisor gRPC client | `crates/openshell-sandbox/src/grpc_client.rs:29-65` | Has `OPENSHELL_TLS_CA` — the cluster CA is available in the supervisor env |
| E2E test | `e2e-controller-test/test_controller.sh` | Shell-based diagnostic test |

## Technical Investigation

### Architecture Overview

The proxy operates as a MITM for sandbox outbound HTTPS traffic:

```
Sandbox workload (curl)
  → CONNECT proxy (10.200.0.1:3128)
    → Proxy responds 200 Connection Established
    → Workload sends TLS ClientHello
    → Proxy terminates client TLS (ephemeral sandbox CA cert)
      ┌─ Client sees: CN=openshell.openshell.svc.cluster.local, Issuer=OpenShell Sandbox CA
      │  (this is the MITM cert, NOT the real server cert)
      │
      → Proxy connects TLS to upstream server ← FAILS HERE
        └─ Root store: webpki_roots (Mozilla public CAs)
        └─ Server cert: signed by cluster CA (openshell-server-tls)
        └─ Verification fails: cluster CA not in Mozilla roots
        └─ Connection drops → client sees "broken pipe"
```

The supervisor (running outside the network namespace) has the cluster CA at `OPENSHELL_TLS_CA=/etc/openshell-tls/client/ca.crt`. This same cert is used for the supervisor's mTLS connection to the server. The proxy runs in the same process as the supervisor but its upstream TLS config doesn't use this CA.

### Code References

| Location | Description |
|----------|-------------|
| `l7/tls.rs:210-220` | `build_upstream_client_config()` — root store is **only** Mozilla CAs |
| `l7/tls.rs:216` | `with_no_client_auth()` — proxy never presents client cert upstream |
| `l7/tls.rs:195-207` | `tls_connect_upstream()` — where the proxy-to-server TLS handshake happens |
| `lib.rs:228` | `build_upstream_client_config()` called with no extra CAs |
| `lib.rs:230` | `ProxyTlsState::new(cert_cache, upstream_config)` — no cluster CA awareness |
| `grpc_client.rs:41-43` | Supervisor reads `OPENSHELL_TLS_CA` — cluster CA is available |
| `proxy.rs:618-620` | `tls_connect_upstream(upstream, &host_lc, tls.upstream_config())` — uses the config with only Mozilla roots |
| `main.rs:107-110` | `--disable-gateway-auth` flag — NOT auto-set when OIDC is configured |
| `main.rs:112-128` | `--oidc-issuer-url`, `--oidc-audience` — no validation that disable-gateway-auth is also set |

### Current Behavior

1. **Proxy upstream TLS fails silently.** `tls_connect_upstream()` at `proxy.rs:618` returns an error because the server's certificate cannot be verified against the Mozilla-only root store. The error is caught at `proxy.rs:672` and logged as a "TLS relay error" (or "TLS connection closed" if classified as benign). The MITM connection to the client closes, causing the "broken pipe" the user sees.

2. **Token placeholder is never replaced.** The `Authorization: Bearer openshell:resolve:env:OPENSHELL_TOKEN` header is visible in test 7 because the L7 relay (which does credential injection) is never reached — the upstream TLS failure prevents it. The H2 relay's `rewrite_h2_headers()` at `grpc.rs:338` and the REST relay's credential injection would correctly replace the placeholder, but they never execute.

3. **Server would reject even if proxy connected.** The gateway was started without `--disable-gateway-auth`, so `allow_unauthenticated` is `false` in `tls.rs:55`. The `WebPkiClientVerifier` requires a valid client certificate. The proxy's upstream config uses `with_no_client_auth()`, so it never presents one. In TLS 1.3, the CertificateRequest is encrypted — the handshake appears to complete but the server drops the connection post-handshake.

### What Would Need to Change

**1. Proxy upstream TLS root store must include the cluster CA**

`build_upstream_client_config()` in `l7/tls.rs:210-220` needs a variant that accepts additional CA certificates. The cluster CA cert is already available to the sandbox supervisor at `OPENSHELL_TLS_CA`. The init code in `lib.rs:228` should read this CA and pass it to the upstream config builder.

**2. Server must accept connections without client certs when OIDC is enabled**

Either:
- Auto-set `allow_unauthenticated = true` when OIDC is configured (the whole point of OIDC is to authenticate without mTLS)
- Or validate at startup that `--disable-gateway-auth` is set when `--oidc-issuer-url` is provided, and emit a clear error

The cleanest approach: when `--oidc-issuer-url` is provided, automatically enable `allow_unauthenticated` and log a message. This avoids requiring users to remember a second flag that is always required with OIDC.

**3. (Optional) Proxy upstream mTLS for infrastructure connections**

Currently the proxy uses `with_no_client_auth()` for all upstream connections. For the specific case of connecting to the OpenShell server, the proxy could present the supervisor's client certificate to authenticate at both the TLS and application layers. However, this would bypass the OIDC scope model — the server would see a valid mTLS client cert and grant full access (the auth interceptor passes through mTLS callers). For Phase 1, `--disable-gateway-auth` + OIDC JWT verification is the correct approach.

### Alternative Approaches Considered

**A. Skip MITM for the server endpoint (tls: skip in policy)**

Set `tls: skip` on the `openshell.openshell.svc.cluster.local:8080` endpoint in the OPA policy. This would create a raw TCP tunnel — no TLS termination, no MITM, no credential injection. The sandbox workload would connect directly to the server's TLS. Problem: this breaks credential injection entirely — the `openshell:resolve:env:OPENSHELL_TOKEN` placeholder would be sent as-is and never replaced with the real JWT.

**B. Proxy presents supervisor's mTLS cert upstream**

Load `OPENSHELL_TLS_CERT` and `OPENSHELL_TLS_KEY` into the proxy's upstream ClientConfig. The proxy would authenticate as the supervisor to the server, which grants full mTLS access. Then the L7 relay adds the JWT for scope enforcement. This is architecturally complex and creates a confused deputy problem — the proxy authenticates with infrastructure creds while carrying user-scoped JWT claims.

**C. Server listens on a second plaintext port for intra-cluster traffic (rejected)**

Add a plaintext gRPC listener for sandbox-to-server communication. Simpler TLS setup, but removes encryption on the wire and creates a second attack surface. Does not align with the defense-in-depth model.

### Patterns to Follow

The supervisor's gRPC client (`grpc_client.rs:40-63`) reads `OPENSHELL_TLS_CA` and builds a TLS config with the cluster CA. The proxy's upstream config should follow the same pattern — read the CA from the environment and add it to the root store alongside Mozilla roots.

The `write_ca_files()` function in `l7/tls.rs:229-246` already creates a combined CA bundle (system CAs + sandbox CA). This pattern should be extended to also include the cluster CA in the upstream root store.

## Proposed Approach

Fix the two blocking issues in the proxy-to-server TLS path:

1. Extend `build_upstream_client_config()` to accept optional extra CA certificates (the cluster CA). Read `OPENSHELL_TLS_CA` in the sandbox supervisor's `lib.rs` init, parse the PEM certs using the existing `parse_pem_certs()` helper, and pass them to the upstream config builder. This allows the proxy to verify the server's self-signed certificate.

2. Auto-enable `allow_unauthenticated` on the server when `--oidc-issuer-url` is configured. The OIDC flow requires connections without client certs — requiring a separate `--disable-gateway-auth` flag is an error-prone UX. Add a startup check in `main.rs` that sets `allow_unauthenticated = true` when OIDC is present and logs it.

With these two fixes, the proxy can complete the TLS handshake to the server, negotiate H2 via ALPN, enter the gRPC relay, inject the real JWT (replacing the `openshell:resolve:env:OPENSHELL_TOKEN` placeholder), and the server's auth interceptor can verify the JWT.

## Scope Assessment

- **Complexity:** Low — two targeted changes in well-understood code paths
- **Confidence:** High — root causes are definitively identified from code reading and test log analysis
- **Estimated files to change:** 4-5 (`l7/tls.rs`, `lib.rs` in openshell-sandbox, `main.rs` in openshell-server, possibly `proxy.rs` for error handling improvement)
- **Issue type:** `fix`

## Risks & Open Questions

- **Cluster CA availability.** The fix assumes `OPENSHELL_TLS_CA` is always available to the sandbox supervisor when TLS is enabled. This is true for the standard deployment (the CA is mounted from `openshell-server-client-ca` K8s secret), but should be validated for edge cases (e.g., `--disable-tls`).
- **OIDC + mTLS interaction.** Auto-enabling `allow_unauthenticated` when OIDC is set means the server accepts connections without client certs. The auth interceptor must then enforce that callers without a client cert present a valid JWT. This is already implemented in the OIDC module, but should be explicitly tested for the case where no JWT and no client cert are provided.
- **Token exchange success validation.** The Keycard token exchange (`exchange_token()`) may fail silently if the provider is misconfigured. The `SecretResolver` would then have no JWT to inject. The test should validate that the resolved token is a real JWT, not the raw placeholder. Currently there is no server-side validation that the token exchange succeeded before the sandbox starts.
- **Test script vs Python E2E.** The current `test_controller.sh` uses curl for HTTP/1.1 diagnostics. The real E2E validation needs the Python `test_controller.py` which uses gRPC. The shell script is useful for debugging but the completion criteria require gRPC (H2) success, not just HTTP health checks.

## End-to-End Validation

The fix is complete only when the following end-to-end flow succeeds without errors.

### Prerequisites

Create the Keycard provider (one-time, or after a fresh cluster):

```bash
./target/debug/openshell provider create \
  --type keycard \
  --name default-zone \
  --config base_url=https://api.keycard.ai \
  --config zone_id=zkaqxyadzg901lcpadwi2odzhp \
  --config client_id=KvFg0AKbw4kHp7tjoZtZk \
  --config client_secret=J8zgOMgWQCtiTNl28WQ5C8rkCJhdOhF21vFFsACYgrV6QCaoL6Zhb1Qwju_Dqvu_
```

### Step 1: Start the gateway with OIDC config

```bash
./target/debug/openshell gateway start \
  --oidc-issuer-url https://o36mbsre94s2vlt8x5jq6nbxs0.keycard.cloud \
  --oidc-audience spiffe://openshell/control-plane
```

**Expected:** Gateway starts successfully. Server logs confirm OIDC verifier is initialized and JWKS keys are fetched.

### Step 2: Create the controller sandbox

```bash
./target/debug/openshell sandbox create \
  --name controller-test \
  --provider keyvengers \
  --secret OPENSHELL_TOKEN=spiffe://openshell/control-plane \
  --policy ./controller-policy.yaml \
  --from ./e2e-controller-test/ \
  -- /app/test_controller.sh
```

The `controller-policy.yaml` must allow the server endpoint:

```yaml
network_policies:
  openshell_control_plane:
    name: openshell-control-plane
    endpoints:
      - host: openshell.openshell.svc.cluster.local
        port: 8080
        protocol: grpc
        enforcement: enforce
        access: full
        allowed_ips:
          - "10.43.0.0/16"
    binaries:
      - path: "/**"
```

**Expected:** Sandbox is created. The Keycard token exchange succeeds (server provisions a JWT for the `spiffe://openshell/control-plane` audience). The `OPENSHELL_TOKEN` env var inside the sandbox contains the `openshell:resolve:env:OPENSHELL_TOKEN` placeholder.

### Step 3: Validate connectivity from inside the sandbox

Connect to the sandbox and verify:

```bash
./target/debug/openshell sandbox connect controller-test
```

#### 3a. HTTP health check succeeds

```bash
curl -k --proxy http://10.200.0.1:3128 \
  https://openshell.openshell.svc.cluster.local:8080/health
```

**Expected:** Returns HTTP 200 with a health response body. This confirms:
- Proxy CONNECT tunnel works
- Proxy MITM TLS termination works
- Proxy upstream TLS to server completes (cluster CA is trusted)
- Server accepts connections without client certs

#### 3b. HTTPS with Bearer token succeeds

```bash
curl -k --proxy http://10.200.0.1:3128 \
  -H "Authorization: Bearer ${OPENSHELL_TOKEN}" \
  https://openshell.openshell.svc.cluster.local:8080/health
```

**Expected:** Returns HTTP 200. The proxy replaces the `openshell:resolve:env:OPENSHELL_TOKEN` placeholder with the real JWT in the `Authorization` header before forwarding to the server.

#### 3c. gRPC calls succeed via Python test

Run the Python E2E test (`test_controller.py`) which uses the `grpc` library over the proxy to:
1. Call `Health` — verifies gRPC connectivity with Bearer token auth
2. Call `ListSandboxes` — verifies scope `openshell:sandbox:read` is granted
3. Call `CreateSandbox` — verifies scope `openshell:sandbox:create` is granted, a child sandbox is created
4. Call `GetSandbox` — verifies the child sandbox exists
5. Call `DeleteSandbox` — verifies scope `openshell:sandbox:delete` is granted

**Expected:** All gRPC calls return success. The proxy negotiates H2 via ALPN, the H2 relay performs credential injection, and the server's OIDC auth interceptor verifies the JWT and checks scopes.

### Failure modes that must NOT occur

- `curl: (56) Send failure: Broken pipe` — proxy-to-server TLS failed (root cause #1)
- `curl: (56) OpenSSL SSL_read: unexpected eof while reading` — server rejected no-client-cert connection (root cause #2)
- `curl: (1) Received HTTP/0.9 when not allowed` — plain HTTP to a TLS-only port
- `Authorization: Bearer openshell:resolve:env:OPENSHELL_TOKEN` visible in server logs — credential injection not happening
- gRPC `UNAUTHENTICATED` — JWT not injected or not verified
- gRPC `PERMISSION_DENIED` — scopes not correctly granted by Keycard token exchange

## Unit Test Considerations

- **Proxy upstream TLS with cluster CA.** Test that `build_upstream_client_config` with extra CAs can verify a certificate signed by those CAs. Use `rcgen` to generate a test CA and leaf cert.
- **Auto-enable allow_unauthenticated with OIDC.** Verify that when `oidc_issuer_url` is set, the server config has `allow_unauthenticated = true` regardless of `--disable-gateway-auth`.
- **OIDC server rejects no-cert no-JWT.** With OIDC configured (and mTLS optional), verify that a connection with neither client cert nor JWT is rejected with `Unauthenticated`.
- **Existing test patterns.** Follow `wiremock` patterns from `keycard.rs` tests. Use `rcgen` for test TLS materials.

---
*Created by spike investigation. Use `build-from-issue` to plan and implement.*
