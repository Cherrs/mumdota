# Mumble-to-WebRTC Proxy Design

## Problem

Build a Rust backend proxy that connects browser-based WebRTC clients to a Mumble voice server, providing voice communication, text chat, and channel management. The proxy is a standalone service; the frontend is a separate project that connects directly to the proxy.

## Approach

Pure Rust stack: `webrtc-rs` for WebRTC, `mumble-protocol` for Mumble, `axum` for WebSocket signaling, `tokio` async runtime. Each WebRTC user maps to an independent Mumble client connection. Opus audio frames pass through without transcoding (both protocols support Opus natively).

## Architecture

```
Browser (WebRTC + WebSocket)
    │
    ├── WebSocket ──→ [Axum HTTP/WS Server] ──→ Signaling + Control
    │                        │
    └── WebRTC ←──→ [WebRTC Engine] ←──→ [Audio Bridge]
                                             │
                            [Mumble Client Connection Pool]
                                             │
                            TCP+TLS / UDP ──→ Mumble Server
```

### Core Components

1. **WebSocket Signaling Service** (axum) — WebRTC SDP exchange, ICE candidates, chat, channel ops
2. **WebRTC Engine** (webrtc-rs) — Per-user PeerConnection, RTP audio send/receive
3. **Audio Bridge** — Extract Opus frames from Mumble VoicePacket, wrap into RTP; reverse for upload
4. **Mumble Client Pool** — One Mumble TCP+TLS/UDP connection per WebRTC user

### Deployment Model

- Single pre-configured Mumble server (address in config)
- Proxy auto-generates self-signed TLS certificates for Mumble connections
- WebRTC users provide only a nickname to connect
- Frontend connects directly to proxy (WebSocket + WebRTC), no intermediate website required

## WebSocket Protocol

All control messages are JSON over WebSocket: `{ "type": "...", "data": { ... } }`

### Client → Server

| Type | Purpose | Key Fields |
|------|---------|------------|
| `connect` | Connect to Mumble | `username` |
| `disconnect` | Disconnect | — |
| `offer` | WebRTC SDP Offer | `sdp` |
| `ice_candidate` | ICE candidate | `candidate` |
| `chat_send` | Send text message | `channel_id`, `message` |
| `channel_join` | Switch channel | `channel_id` |
| `mute` | Toggle mute | `muted: bool` |
| `deafen` | Toggle deafen | `deafened: bool` |

### Server → Client

| Type | Purpose | Key Fields |
|------|---------|------------|
| `answer` | WebRTC SDP Answer | `sdp` |
| `ice_candidate` | ICE candidate | `candidate` |
| `connected` | Connection success | `session_id`, `channels`, `users` |
| `user_joined` | User online | `user` |
| `user_left` | User offline | `user_id` |
| `user_state` | User state change | `user_id`, `channel_id`, `mute`, `deaf` |
| `chat_received` | Chat message received | `sender`, `channel_id`, `message`, `timestamp` |
| `channel_updated` | Channel info update | `channels` |
| `error` | Error notification | `code`, `message` |

## Audio Data Flow

```
[Browser Mic] → WebRTC RTP(Opus) → Proxy → Extract Opus frames → Wrap as Mumble VoicePacket → Mumble Server
[Mumble Server] → Mumble VoicePacket(Opus) → Proxy → Extract Opus frames → Wrap as RTP → WebRTC → Browser Speaker
```

- No transcoding: Opus frames are relayed directly between formats
- Sequence number and timestamp mapping handled by the bridge layer
- Mumble uses custom binary format (session_id, seq, Opus payload)
- WebRTC uses RTP format (SSRC, seq, timestamp, Opus payload)

### Connection Lifecycle (per user)

1. User sends `connect` + username via WebSocket
2. Proxy creates TCP+TLS connection to Mumble server
3. Sends Version + Authenticate protobuf messages (auto-generated cert)
4. Establishes UDP channel for voice
5. Creates WebRTC PeerConnection, returns SDP Answer to frontend
6. Audio bridge begins bidirectional Opus frame relay

## Project Structure

```
mumdota/
├── Cargo.toml
├── config.toml                # Runtime configuration
├── docs/
│   └── frontend-api.md        # Frontend integration docs
└── src/
    ├── main.rs                 # Entry point, start services
    ├── config.rs               # Config parsing
    ├── server.rs               # Axum HTTP/WS server
    ├── session.rs              # User session management
    ├── ws/
    │   ├── mod.rs              # WebSocket message routing
    │   └── messages.rs         # JSON message types (serde)
    ├── webrtc/
    │   ├── mod.rs              # WebRTC PeerConnection management
    │   └── audio.rs            # RTP audio processing
    ├── mumble/
    │   ├── mod.rs              # Mumble client connection management
    │   ├── client.rs           # Single Mumble connection (TCP+UDP)
    │   ├── voice.rs            # Voice packet processing
    │   └── proto.rs            # Protobuf message handling
    └── bridge.rs               # Audio bridge (Mumble ↔ WebRTC Opus relay)
```

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `axum` | HTTP/WebSocket server |
| `webrtc` | WebRTC engine (webrtc-rs) |
| `mumble-protocol` | Mumble protocol implementation |
| `tokio-rustls` | TLS for Mumble connections |
| `serde` + `serde_json` | JSON serialization |
| `toml` | Config file parsing |
| `tracing` | Logging and diagnostics |

## Configuration

```toml
[server]
listen_addr = "0.0.0.0"
listen_port = 8080
max_connections = 100

[mumble]
host = "mumble.example.com"
port = 64738
accept_invalid_certs = true

[webrtc]
stun_servers = ["stun:your-stun-server:3478"]
```

## Error Handling

- Mumble disconnect → send `error` via WebSocket, attempt auto-reconnect (configurable)
- WebRTC failure → send `error`, frontend can re-offer
- WebSocket disconnect → clean up Mumble connection + WebRTC PeerConnection
- Mumble unreachable → return `error` type `mumble_unreachable`

## Resource Cleanup

- Auto-cleanup all resources on user disconnect (Mumble TCP/UDP, WebRTC PeerConnection)
- Heartbeat detection (WebSocket ping/pong + Mumble Ping)
- Configurable max concurrent user limit
