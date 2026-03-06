# MumDota Frontend Integration Guide

> Complete reference for integrating browser clients with the MumDota Mumble-to-WebRTC proxy.

## Quick Start

```html
<!DOCTYPE html>
<html>
<head><title>MumDota Client</title></head>
<body>
  <input id="nick" placeholder="Nickname" value="WebUser" />
  <button id="connect">Connect</button>
  <button id="disconnect" disabled>Disconnect</button>
  <div id="status">Disconnected</div>
  <div id="channels"></div>
  <input id="chatInput" placeholder="Type a message..." disabled />
  <div id="chatLog"></div>

  <script>
    let ws, pc, localStream;
    const statusEl = document.getElementById('status');

    document.getElementById('connect').onclick = async () => {
      const nick = document.getElementById('nick').value;
      await startSession(nick);
    };
    document.getElementById('disconnect').onclick = () => {
      ws?.send(JSON.stringify({ type: 'Disconnect' }));
      cleanup();
    };

    async function startSession(nickname) {
      // 1. Open WebSocket
      ws = new WebSocket(`ws://${location.hostname}:3000/ws`);
      ws.onopen = () => {
        statusEl.textContent = 'WebSocket connected, authenticating...';
        ws.send(JSON.stringify({ type: 'Connect', nickname }));
      };
      ws.onmessage = (e) => handleMessage(JSON.parse(e.data));
      ws.onclose = () => { statusEl.textContent = 'Disconnected'; cleanup(); };

      // 2. Get microphone
      localStream = await navigator.mediaDevices.getUserMedia({
        audio: { echoCancellation: true, noiseSuppression: true }
      });
    }

    async function handleMessage(msg) {
      switch (msg.type) {
        case 'Connected':
          statusEl.textContent = `Connected as "${msg.username}" (session ${msg.session_id})`;
          document.getElementById('chatInput').disabled = false;
          document.getElementById('disconnect').disabled = false;
          await setupWebRTC();
          break;

        case 'SdpAnswer':
          await pc.setRemoteDescription({ type: 'answer', sdp: msg.sdp });
          break;

        case 'IceCandidate':
          await pc.addIceCandidate({
            candidate: msg.candidate,
            sdpMid: msg.sdp_mid,
            sdpMLineIndex: msg.sdp_mline_index
          });
          break;

        case 'ChannelList':
          renderChannels(msg.channels);
          break;

        case 'UserList':
          console.log('Users:', msg.users);
          break;

        case 'ChatMessage':
          appendChat(msg.sender, msg.message);
          break;

        case 'Error':
          statusEl.textContent = `Error: ${msg.message}`;
          break;

        case 'Disconnected':
          statusEl.textContent = 'Disconnected from Mumble';
          cleanup();
          break;
      }
    }

    async function setupWebRTC() {
      pc = new RTCPeerConnection({
        iceServers: [{ urls: 'stun:stun.l.google.com:19302' }]
      });

      // Add mic track
      localStream.getTracks().forEach(t => pc.addTrack(t, localStream));

      // Receive remote audio
      pc.ontrack = (e) => {
        const audio = new Audio();
        audio.srcObject = e.streams[0];
        audio.play();
      };

      // Send ICE candidates
      pc.onicecandidate = (e) => {
        if (e.candidate) {
          ws.send(JSON.stringify({
            type: 'IceCandidate',
            candidate: e.candidate.candidate,
            sdp_mid: e.candidate.sdpMid,
            sdp_mline_index: e.candidate.sdpMLineIndex
          }));
        }
      };

      // Create and send offer
      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      ws.send(JSON.stringify({ type: 'SdpOffer', sdp: offer.sdp }));
    }

    function cleanup() {
      pc?.close();
      localStream?.getTracks().forEach(t => t.stop());
      ws?.close();
      pc = null; localStream = null; ws = null;
      document.getElementById('chatInput').disabled = true;
      document.getElementById('disconnect').disabled = true;
    }

    function renderChannels(channels) {
      const el = document.getElementById('channels');
      el.innerHTML = '<h3>Channels</h3>' + channels.map(c =>
        `<div><button onclick="switchChannel(${c.id})">${c.name}</button></div>`
      ).join('');
    }

    function appendChat(sender, message) {
      const el = document.getElementById('chatLog');
      el.innerHTML += `<div><b>${sender}:</b> ${message}</div>`;
      el.scrollTop = el.scrollHeight;
    }

    window.switchChannel = (id) => {
      ws.send(JSON.stringify({ type: 'SwitchChannel', channel_id: id }));
    };

    document.getElementById('chatInput').onkeydown = (e) => {
      if (e.key === 'Enter' && e.target.value) {
        ws.send(JSON.stringify({
          type: 'SendChat',
          channel_id: 0,
          message: e.target.value
        }));
        e.target.value = '';
      }
    };
  </script>
</body>
</html>
```

---

## Connection Flow

```
Browser                          MumDota Proxy                    Mumble Server
  │                                  │                                │
  │──── WS Connect ──────────────────│                                │
  │──── {"type":"Connect"} ──────────│                                │
  │                                  │──── TCP+TLS Connect ───────────│
  │                                  │──── Authenticate ──────────────│
  │                                  │◄─── ServerSync ────────────────│
  │◄─── {"type":"Connected"} ────────│                                │
  │──── {"type":"SdpOffer"} ─────────│                                │
  │◄─── {"type":"SdpAnswer"} ────────│                                │
  │◄──► ICE Candidates ─────────────►│                                │
  │                                  │                                │
  │════ WebRTC Media (Opus/RTP) ═════│════ Mumble Voice (Opus) ═══════│
  │◄──► Chat Messages ──────────────►│◄──► TextMessage ──────────────►│
```

---

## WebSocket Protocol Reference

All messages are JSON over WebSocket at `ws://<host>:3000/ws`.

### Client → Server Messages

#### Connect
Authenticate and join the Mumble server.
```json
{ "type": "Connect", "nickname": "PlayerOne" }
```

#### Disconnect
Leave the Mumble server and clean up resources.
```json
{ "type": "Disconnect" }
```

#### SdpOffer
Send a WebRTC SDP offer to establish media connection.
```json
{ "type": "SdpOffer", "sdp": "<SDP string>" }
```

#### IceCandidate
Send an ICE candidate for WebRTC connectivity.
```json
{
  "type": "IceCandidate",
  "candidate": "candidate:...",
  "sdp_mid": "0",
  "sdp_mline_index": 0
}
```

#### SwitchChannel
Move to a different Mumble channel.
```json
{ "type": "SwitchChannel", "channel_id": 2 }
```

#### SendChat
Send a text message. Use `channel_id: 0` for current channel.
```json
{ "type": "SendChat", "channel_id": 0, "message": "Hello everyone!" }
```

#### SetMute
Mute or unmute your microphone.
```json
{ "type": "SetMute", "muted": true }
```

#### SetDeaf
Deafen or undeafen (stop receiving audio).
```json
{ "type": "SetDeaf", "deafened": true }
```

### Server → Client Messages

#### Connected
Login successful. Returned after `Connect`.
```json
{
  "type": "Connected",
  "session_id": 42,
  "username": "PlayerOne",
  "channel_id": 0
}
```

#### Disconnected
Connection to Mumble ended (server kick, network error, or requested).
```json
{ "type": "Disconnected", "reason": "Server kicked you" }
```

#### SdpAnswer
WebRTC SDP answer in response to `SdpOffer`.
```json
{ "type": "SdpAnswer", "sdp": "<SDP string>" }
```

#### IceCandidate
ICE candidate from the server side.
```json
{
  "type": "IceCandidate",
  "candidate": "candidate:...",
  "sdp_mid": "0",
  "sdp_mline_index": 0
}
```

#### ChannelList
Current channel tree. Sent on login and when channels change.
```json
{
  "type": "ChannelList",
  "channels": [
    { "id": 0, "name": "Root", "parent_id": 0, "description": "" },
    { "id": 1, "name": "General", "parent_id": 0, "description": "Default channel" }
  ]
}
```

#### UserList
Current user list. Sent on login and when users join/leave/move.
```json
{
  "type": "UserList",
  "users": [
    { "session_id": 1, "name": "Admin", "channel_id": 0, "muted": false, "deafened": false }
  ]
}
```

#### ChatMessage
Text message received from another user.
```json
{
  "type": "ChatMessage",
  "sender": "OtherUser",
  "message": "Hello!",
  "channel_id": 0
}
```

#### UserStateChanged
A user's state changed (mute, deaf, channel move).
```json
{
  "type": "UserStateChanged",
  "session_id": 1,
  "channel_id": 2,
  "muted": false,
  "deafened": false
}
```

#### Error
An error occurred processing a request.
```json
{ "type": "Error", "message": "Not connected to Mumble server" }
```

---

## WebRTC Setup Details

### Audio Constraints

```javascript
const constraints = {
  audio: {
    echoCancellation: true,
    noiseSuppression: true,
    autoGainControl: true,
    sampleRate: 48000,
    channelCount: 1
  }
};
const stream = await navigator.mediaDevices.getUserMedia(constraints);
```

### PeerConnection Configuration

```javascript
const pc = new RTCPeerConnection({
  iceServers: [
    // Use your private STUN server for production
    { urls: 'stun:your-stun-server:3478' },
    // Public fallback (not recommended for production)
    { urls: 'stun:stun.l.google.com:19302' }
  ]
});
```

### Codec Preference (Opus)

The proxy only supports Opus. To ensure the browser prefers Opus:

```javascript
const transceiver = pc.getTransceivers()[0];
if (transceiver && transceiver.setCodecPreferences) {
  const codecs = RTCRtpReceiver.getCapabilities('audio').codecs
    .filter(c => c.mimeType === 'audio/opus');
  transceiver.setCodecPreferences(codecs);
}
```

### Handling Remote Audio

```javascript
pc.ontrack = (event) => {
  const audio = document.createElement('audio');
  audio.srcObject = event.streams[0];
  audio.autoplay = true;
  // Audio elements must be triggered by user gesture in some browsers
  document.body.appendChild(audio);
  audio.play().catch(() => {
    // Add a "click to unmute" button if autoplay is blocked
    const btn = document.createElement('button');
    btn.textContent = 'Click to hear audio';
    btn.onclick = () => { audio.play(); btn.remove(); };
    document.body.appendChild(btn);
  });
};
```

---

## Configuration

The proxy reads `config.toml` from the working directory:

```toml
[mumble]
host = "mumble.example.com"
port = 64738

[server]
listen_port = 3000

[webrtc]
stun_servers = ["stun:stun.l.google.com:19302"]
```

| Field | Description | Default |
|-------|-------------|---------|
| `mumble.host` | Mumble server hostname/IP | `"localhost"` |
| `mumble.port` | Mumble server port | `64738` |
| `server.listen_port` | HTTP/WS listen port | `3000` |
| `webrtc.stun_servers` | STUN server URLs for ICE | `["stun:stun.l.google.com:19302"]` |

---

## Error Handling

### WebSocket Reconnection

```javascript
function connectWithRetry(nickname, maxRetries = 5) {
  let retries = 0;

  function connect() {
    const ws = new WebSocket(`ws://${location.hostname}:3000/ws`);

    ws.onopen = () => {
      retries = 0;
      ws.send(JSON.stringify({ type: 'Connect', nickname }));
    };

    ws.onclose = (e) => {
      if (retries < maxRetries) {
        const delay = Math.min(1000 * Math.pow(2, retries), 30000);
        retries++;
        console.log(`Reconnecting in ${delay}ms (attempt ${retries}/${maxRetries})`);
        setTimeout(connect, delay);
      }
    };

    ws.onerror = (e) => console.error('WebSocket error:', e);

    return ws;
  }

  return connect();
}
```

### ICE Connection Monitoring

```javascript
pc.oniceconnectionstatechange = () => {
  console.log('ICE state:', pc.iceConnectionState);
  switch (pc.iceConnectionState) {
    case 'connected':
      statusEl.textContent = 'Voice connected';
      break;
    case 'disconnected':
    case 'failed':
      statusEl.textContent = 'Voice connection lost';
      // Consider restarting ICE or reconnecting
      break;
  }
};
```

---

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/ws` | WebSocket | Main signaling connection |
| `/health` | GET | Health check, returns `"ok"` |

---

## Browser Requirements

- Modern browser with WebRTC support (Chrome 70+, Firefox 63+, Safari 14+, Edge 79+)
- HTTPS required in production for `getUserMedia()` (localhost exempt)
- User gesture required before `getUserMedia()` and audio playback on most browsers
