# Local Development Setup

This guide explains how to build all OpenShell components from source and run them using only locally built images, with no pulls from the remote registry (`ghcr.io`).

## Prerequisites

- [mise](https://mise.jdx.dev/) installed and activated in your shell
- Rust 1.88+
- Python 3.12+
- Docker running

```bash
# Install mise
curl https://mise.run | sh

# One-time trust for this repo
mise trust
```

## Quick Start (Recommended)

The fastest way to build everything locally and get a running cluster:

```bash
mise run cluster
```

This single command:

1. Starts a local Docker registry at `127.0.0.1:5000`
2. Builds the `openshell/gateway:dev` image from local source
3. Pushes the gateway image to the local registry
4. Builds the `openshell/cluster:dev` image (k3s + supervisor + Helm chart)
5. Sets `OPENSHELL_CLUSTER_IMAGE=openshell/cluster:dev` so the CLI uses the local image
6. Runs `openshell gateway start` against the local image
7. Bootstraps k3s, deploys the Helm chart, and waits for health checks

No images are pulled from `ghcr.io`. Everything is built and served locally.

To also create a development sandbox in one shot:

```bash
mise run sandbox
```

This runs `mise run cluster` internally (no-ops if the cluster is already healthy), then creates or reconnects to a persistent sandbox named `dev`.

## What Gets Built

| Image | Tag | Contents |
|-------|-----|----------|
| `openshell/gateway` | `dev` | The `openshell-server` binary (gateway API server) |
| `openshell/cluster` | `dev` | k3s, supervisor (`openshell-sandbox`), Helm chart, manifests, entrypoint scripts |

Both images are defined in `deploy/docker/Dockerfile.images` as separate build targets.

## Step-by-Step (Manual)

If you want to understand or control each step individually:

### 1. Build the Docker Images

```bash
# Build both gateway and cluster images
mise run docker:build

# Or build them individually
mise run docker:build:gateway   # -> openshell/gateway:dev
mise run docker:build:cluster   # -> openshell/cluster:dev
```

### 2. Start the Gateway with the Local Cluster Image

The CLI defaults to pulling `ghcr.io/nvidia/openshell/cluster:dev`. Override this with the `OPENSHELL_CLUSTER_IMAGE` environment variable:

```bash
OPENSHELL_CLUSTER_IMAGE=openshell/cluster:dev openshell gateway start
```

The `ensure_image` logic in the CLI checks whether the image exists locally first. If the image reference has no registry prefix (like `openshell/cluster:dev`), it skips the pull entirely and uses the local image. If the image doesn't exist locally, it errors with a message telling you to build it.

### 3. Push Gateway to the Local Registry

For the gateway image to be available inside k3s, it must be in a registry the cluster can reach. The development flow uses a local Docker registry:

```bash
# Start the local registry (if not already running)
docker run -d --restart=always --name openshell-local-registry -p 5000:5000 registry:2

# Tag and push
docker tag openshell/gateway:dev 127.0.0.1:5000/openshell/gateway:dev
docker push 127.0.0.1:5000/openshell/gateway:dev
```

The `mise run cluster` command handles this automatically.

## Custom Ports

Two host ports are used during local development: the **gateway port** and the **local registry port**.

### Gateway Port

The gateway port is the host port mapped to the k3s NodePort (30051) inside the cluster container. This is the port the CLI and sandboxes use to communicate with the gateway.

When using `mise run cluster`, the bootstrap script picks a random free port automatically. To use a specific port instead:

```bash
GATEWAY_PORT=9090 mise run cluster
```

When using the CLI directly:

```bash
OPENSHELL_CLUSTER_IMAGE=openshell/cluster:dev openshell gateway start --port 9090
```

The port is persisted in `.env` at the repo root after the first bootstrap, so subsequent runs of `mise run cluster` reuse the same port. To change it, either edit `.env` or delete it and re-run with the new `GATEWAY_PORT` value.

### Local Registry Port

The local Docker registry defaults to port `5000`. This port is used by the bootstrap scripts to push the gateway image so the k3s cluster can pull it. The registry port is currently hardcoded in the bootstrap scripts and cannot be changed via environment variable.

If port 5000 is already in use on your machine, stop the conflicting service before running `mise run cluster`:

```bash
# Check what's using port 5000
lsof -nP -iTCP:5000 -sTCP:LISTEN

# If it's an old registry container, remove it
docker rm -f openshell-local-registry
```

### Port Conflict with Existing Gateways

If you already have a gateway running on the same port, destroy it first or use a different cluster name:

```bash
# Option 1: Destroy the existing gateway
openshell gateway destroy

# Option 2: Run a second cluster with a different name and port
CLUSTER_NAME=my-feature GATEWAY_PORT=9090 mise run cluster
```

## Incremental Development

After the initial bootstrap, use `mise run cluster` for incremental deploys. It fingerprints your working tree and only rebuilds what changed:

| Change | What Gets Rebuilt |
|--------|-------------------|
| `crates/openshell-server/` | Gateway image -> push to local registry -> helm upgrade -> rollout restart |
| `crates/openshell-sandbox/` | Supervisor binary -> `docker cp` into cluster container |
| `deploy/helm/openshell/` | Helm chart -> copied into cluster -> `helm upgrade` |
| No changes | No-op |

You can also target specific components:

```bash
mise run cluster:deploy:supervisor  # Fast-deploy only the supervisor binary
mise run cluster:deploy:all         # Rebuild everything (gateway + supervisor + helm)
```

## Why `openshell gateway start` Pulls from GHCR

The default image reference is compiled into the CLI binary:

```
ghcr.io/nvidia/openshell/cluster:{DEFAULT_IMAGE_TAG}
```

Where `DEFAULT_IMAGE_TAG` is `dev` unless overridden at compile time via `OPENSHELL_IMAGE_TAG`.

This means running `openshell gateway start` without `mise run cluster` will attempt to pull from GHCR. The development scripts set `OPENSHELL_CLUSTER_IMAGE` to bypass this.

## Environment Variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `OPENSHELL_CLUSTER_IMAGE` | Override the cluster image reference entirely | `ghcr.io/nvidia/openshell/cluster:dev` |
| `IMAGE_TAG` | Tag for locally built images | `dev` |
| `IMAGE_REPO_BASE` | Base path for image repository | `127.0.0.1:5000/openshell` (dev) |
| `SKIP_IMAGE_PUSH` | Skip pushing images during bootstrap (set to `1`) | `0` |
| `SKIP_CLUSTER_IMAGE_BUILD` | Skip building the cluster image (set to `1`) | `0` |
| `CLUSTER_NAME` | Name for the cluster container/volume | Current directory name |
| `GATEWAY_PORT` | Host port for the gateway | Random free port (dev) |
| `CLUSTER_GPU` | Enable GPU passthrough (set to `1`) | `0` |

## Verifying No Remote Pulls

To confirm nothing is pulled from a remote registry, check the Docker images before and after:

```bash
# List images before
docker images | grep openshell

# Run the cluster
mise run cluster

# List images after — should only show openshell/* and 127.0.0.1:5000/openshell/*
docker images | grep openshell
```

The `openshell/cluster:dev` and `openshell/gateway:dev` images should be locally built. The `127.0.0.1:5000/openshell/gateway:dev` tag is the same image pushed to the local registry.

## Troubleshooting

### "Image not found locally"

If you see an error like:

> Image 'openshell/cluster:dev' not found locally. This looks like a locally-built image (no registry prefix). Build it first with `mise run docker:build:gateway`.

Build the images first:

```bash
mise run docker:build
```

### Gateway pulls from GHCR despite local build

You ran `openshell gateway start` directly instead of `mise run cluster`. Either:

- Use `mise run cluster` (recommended), or
- Set the environment variable: `OPENSHELL_CLUSTER_IMAGE=openshell/cluster:dev openshell gateway start`

### Local registry not reachable

The local registry at `127.0.0.1:5000` must be running. Check:

```bash
docker ps | grep openshell-local-registry
curl http://127.0.0.1:5000/v2/
```

If it's not running:

```bash
docker run -d --restart=always --name openshell-local-registry -p 5000:5000 registry:2
```

### Clean rebuild

To start fresh:

```bash
# Destroy the cluster
openshell gateway destroy

# Clean Docker artifacts
mise run docker:cleanup

# Rebuild everything
mise run cluster
```
