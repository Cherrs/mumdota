# MumDota

A Mumble-to-WebRTC proxy server written in Rust. Browser clients connect to a Mumble voice server via WebRTC — the proxy handles the Mumble TCP/TLS connection and UDP voice channel on behalf of each browser user, bridging Opus audio bidirectionally.

## Building & Running

```bash
# Build
cargo build --release

# Run (reads config.toml in the working directory by default)
cargo run

# Run with a custom config path
cargo run -- /path/to/config.toml

# Run with debug logging
RUST_LOG=mumdota=debug cargo run
```

## Configuration

Configuration is read from `config.toml`. All string and numeric values support `${ENV_VAR}` and `${ENV_VAR:-default}` placeholders which are expanded from environment variables before the file is parsed.

Any field can also be overridden at runtime via `MUMDOTA_*` environment variables (see table below) — these take the highest priority and do not require editing the config file.

### config.toml reference

```toml
[server]
# Address to listen on
listen_addr = "0.0.0.0"

# TCP port to listen on
listen_port = 8080

# Maximum number of concurrent WebSocket connections
max_connections = 100

[mumble]
# Mumble server hostname or IP
host = "mumble.example.com"

# Mumble server port
port = 64738

# Allow self-signed / invalid TLS certificates on the Mumble server
accept_invalid_certs = true

[webrtc]
# STUN servers used for ICE negotiation (comma-separated URIs)
stun_servers = ["stun:stun.l.google.com:19302"]
```

### Environment variable overrides

| Environment variable | Config field | Type | Example |
|---|---|---|---|
| `MUMDOTA_SERVER_LISTEN_ADDR` | `server.listen_addr` | string | `127.0.0.1` |
| `MUMDOTA_SERVER_LISTEN_PORT` | `server.listen_port` | integer | `9090` |
| `MUMDOTA_SERVER_MAX_CONNECTIONS` | `server.max_connections` | integer | `200` |
| `MUMDOTA_MUMBLE_HOST` | `mumble.host` | string | `mumble.example.com` |
| `MUMDOTA_MUMBLE_PORT` | `mumble.port` | integer | `64738` |
| `MUMDOTA_MUMBLE_ACCEPT_INVALID_CERTS` | `mumble.accept_invalid_certs` | bool (`1`/`true`/`yes`) | `true` |
| `MUMDOTA_WEBRTC_STUN_SERVERS` | `webrtc.stun_servers` | comma-separated strings | `stun:stun.l.google.com:19302` |

### Inline placeholder syntax

Values in `config.toml` can reference environment variables directly:

```toml
[mumble]
# Required — error if MUMBLE_HOST is not set
host = "${MUMBLE_HOST}"

# Optional — falls back to "mumble.example.com" if MUMBLE_HOST is unset
host = "${MUMBLE_HOST:-mumble.example.com}"
```

### Priority order (highest → lowest)

1. `MUMDOTA_*` environment variable overrides
2. Inline `${VAR}` / `${VAR:-default}` placeholders in `config.toml`
3. Literal values in `config.toml`

## Deployment Guide

### Option 1: Docker (Recommended)

GitHub Actions CI is configured to automatically build and publish Docker images to GitHub Container Registry (GHCR) on pushes to the `master` branch or `v*` tags.

#### Using pre-built images

```bash
# Pull the latest image (replace <owner> with your GitHub username/org)
docker pull ghcr.io/<owner>/mumdota:master

# Run the container
docker run -d \
  --name mumdota \
  -p 8080:8080 \
  -e MUMDOTA_MUMBLE_HOST=mumble.example.com \
  -e MUMDOTA_MUMBLE_PORT=64738 \
  ghcr.io/<owner>/mumdota:master
```

#### Building locally

```bash
docker build -t mumdota .

docker run -d \
  --name mumdota \
  -p 8080:8080 \
  -e MUMDOTA_MUMBLE_HOST=mumble.example.com \
  mumdota
```

#### Using a custom config file

```bash
docker run -d \
  --name mumdota \
  -p 8080:8080 \
  -v /path/to/your/config.toml:/app/config.toml:ro \
  mumdota
```

### Option 2: Build from Source

#### Prerequisites

- Rust 1.94.0 or later
- CA certificates installed (for TLS connections to the Mumble server)

#### Steps

```bash
# Clone the repository
git clone https://github.com/<owner>/mumdota.git
cd mumdota

# Build in release mode
cargo build --release

# Copy the binary to the target location
cp target/release/mumdota /usr/local/bin/

# Prepare the config file
cp config.toml /etc/mumdota/config.toml
# Edit the config with your actual Mumble server address, etc.
vi /etc/mumdota/config.toml

# Run
mumdota /etc/mumdota/config.toml
```

### Option 3: Systemd Service (Linux)

Create a systemd unit file at `/etc/systemd/system/mumdota.service`:

```ini
[Unit]
Description=MumDota - Mumble to WebRTC Proxy
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/mumdota /etc/mumdota/config.toml
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=mumdota=info

[Install]
WantedBy=multi-user.target
```

Enable and start the service:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now mumdota
sudo systemctl status mumdota

# View logs
journalctl -u mumdota -f
```

### Network Requirements

| Port / Protocol | Direction | Purpose |
|-----------------|-----------|---------|
| TCP 8080 (configurable) | Inbound | WebSocket connections (browser → proxy) |
| TCP 64738 (configurable) | Outbound | Mumble control channel (proxy → Mumble server) |
| UDP 64738 (configurable) | Outbound | Mumble voice channel (proxy → Mumble server) |
| UDP high ports | Inbound/Outbound | WebRTC ICE media streams |

> **Note:** If deploying behind NAT or a firewall, make sure to configure the correct STUN servers (`webrtc.stun_servers`) and open the UDP port range required by WebRTC.

### Health Check

The service exposes a `/health` endpoint:

```bash
curl http://localhost:8080/health
```

This can be used for Docker health checks, load balancer probes, or monitoring systems.
