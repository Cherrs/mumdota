# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

**MumDota** is a Mumble-to-WebRTC proxy server written in Rust. It lets browser clients connect to a Mumble voice server using WebRTC — the proxy handles the Mumble TCP/TLS connection and UDP voice channel on behalf of each browser user, bridging Opus audio bidirectionally.

## Commands

```bash
# Build
cargo build

# Run (config.toml is the default config path; pass a path as arg to override)
cargo run
cargo run -- /path/to/config.toml

# Run tests
cargo test

# Run a specific test
cargo test test_name

# Logging (module-level filtering)
RUST_LOG=mumdota=debug cargo run
```

## Architecture

Each browser connection goes through this lifecycle:

1. **WebSocket** (`/ws`) — browser connects, sends `Connect` with a username
2. **Mumble client** (`src/mumble/`) — proxy authenticates to Mumble server via TCP+TLS
3. **WebRTC session** (`src/webrtc/`) — proxy creates a `PeerConnection`; browser sends SDP offer → proxy sends SDP answer + ICE candidates
4. **Voice (UDP)** (`src/mumble/voice.rs`) — after Mumble sends `CryptState`, a separate UDP voice channel starts
5. **Audio bridge** (`src/bridge.rs`) — `AudioBridge` forwards Opus packets between Mumble UDP voice and the WebRTC RTP track

### Key modules

| Path | Role |
|------|------|
| `src/server.rs` | Axum HTTP server; routes `/ws` and `/health` |
| `src/session.rs` | `SessionManager` — owns all `UserSession`s; dispatches WS commands and events |
| `src/bridge.rs` | `AudioBridge` — two async tasks forwarding Opus in each direction |
| `src/ws/handler.rs` | Per-connection WS loop; parses `ClientMessage`, calls `SessionManager` |
| `src/ws/messages.rs` | All WS message types (serde tagged enums) |
| `src/mumble/client.rs` | Mumble TCP+TLS client; emits `MumbleEvent` on a channel |
| `src/mumble/voice.rs` | Mumble UDP voice (OCB2-AES encrypted); emits/accepts Opus frames |
| `src/webrtc/mod.rs` | WebRTC `PeerConnection` wrapper; emits `WebrtcEvent` |
| `src/config.rs` | Loads `config.toml` |

### Session concurrency model

`SessionManager` holds a `RwLock<HashMap<conn_id, Arc<Mutex<UserSession>>>>`. When a user connects, a background task (`process_events`) is spawned that drives the event loops for both Mumble and WebRTC using `tokio::select!`. A separate `voice_setup_task` waits for the `CryptState` oneshot before starting the UDP voice channel and `AudioBridge`.

### WebSocket protocol

Messages use serde's adjacently tagged format: `{"type": "snake_case_variant", "data": {...}}`. The canonical message types are defined in `src/ws/messages.rs` — the `docs/frontend-api.md` documents these from the browser's perspective but may be slightly out of sync with the actual Rust types.

## Configuration

`config.toml` (read from working directory or first CLI arg):

```toml
[server]
listen_addr = "0.0.0.0"
listen_port = 8080
max_connections = 100

[mumble]
host = "mumble.example.com"
port = 64738
accept_invalid_certs = true   # for self-signed Mumble server certs

[webrtc]
stun_servers = ["stun:stun.l.google.com:19302"]
```
