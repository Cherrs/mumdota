# MumDota Frontend Integration Guide

> 浏览器客户端接入 MumDota Mumble-to-WebRTC 代理的完整参考文档。

## 连接地址

```
ws://<host>:8080/ws      WebSocket 信令连接
http://<host>:8080/health  健康检查（返回 "ok"）
```

---

## 消息格式

所有消息均为 JSON，统一采用 **adjacently tagged** 格式：

```json
{"type": "<消息类型>", "data": { ...字段... }}
```

- `type` 字段值全部为 **snake_case**（小写加下划线）
- 有内容的消息带 `"data"` 对象；无数据的消息（如 `disconnect`）只有 `"type"` 字段

---

## 连接流程

```
Browser                        MumDota Proxy                  Mumble Server
  │                                │                               │
  │──── WS Connect ────────────────│                               │
  │──── connect ───────────────────│                               │
  │                                │──── TCP+TLS Connect ──────────│
  │                                │──── Authenticate ─────────────│
  │                                │◄─── ServerSync ───────────────│
  │◄─── connected ─────────────────│                               │
  │                                │                               │
  │──── offer (SDP) ───────────────│                               │
  │◄─── answer (SDP) ──────────────│                               │
  │◄──► ice_candidate ─────────────│                               │
  │                                │                               │
  │════ WebRTC Audio (Opus/RTP) ═══│════ Mumble Voice (Opus/UDP) ══│
  │◄──► chat_send / chat_received ─│◄──► TextMessage ──────────────│
```

---

## 客户端 → 服务器消息

### connect

连接并加入 Mumble 服务器。这是建立 WebSocket 后必须发送的第一条消息。

```json
{"type": "connect", "data": {"username": "PlayerOne"}}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `username` | string | 在 Mumble 中显示的用户名 |

---

### disconnect

断开连接并释放资源。

```json
{"type": "disconnect"}
```

---

### offer

发送 WebRTC SDP Offer，在收到 `connected` 之后发送。

```json
{"type": "offer", "data": {"sdp": "<SDP 字符串>"}}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `sdp` | string | RTCPeerConnection.createOffer() 生成的 SDP |

---

### ice_candidate

发送 ICE 候选项。

```json
{
  "type": "ice_candidate",
  "data": {
    "candidate": "candidate:...",
    "sdp_mid": "0",
    "sdp_mline_index": 0
  }
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `candidate` | string | ICE candidate 字符串 |
| `sdp_mid` | string \| null | SDP mid 值 |
| `sdp_mline_index` | number \| null | SDP m-line 索引 |

---

### chat_send

发送文字消息到指定频道。

```json
{"type": "chat_send", "data": {"channel_id": 0, "message": "Hello!"}}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `channel_id` | number | 目标频道 ID |
| `message` | string | 消息内容 |

---

### channel_join

切换到指定频道。

```json
{"type": "channel_join", "data": {"channel_id": 2}}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `channel_id` | number | 目标频道 ID |

---

### mute

静音或取消静音自己的麦克风。

```json
{"type": "mute", "data": {"muted": true}}
```

---

### deafen

屏蔽或取消屏蔽接收音频。

```json
{"type": "deafen", "data": {"deafened": true}}
```

---

## 服务器 → 客户端消息

### connected

登录成功后返回，包含当前频道列表和在线用户列表。

```json
{
  "type": "connected",
  "data": {
    "session_id": 42,
    "channels": [
      {"id": 0, "name": "Root", "parent_id": 0, "description": ""},
      {"id": 1, "name": "General", "parent_id": 0, "description": "默认频道"}
    ],
    "users": [
      {
        "session_id": 1,
        "name": "Admin",
        "channel_id": 0,
        "mute": false,
        "deaf": false,
        "self_mute": false,
        "self_deaf": false
      }
    ]
  }
}
```

收到后应立即发起 WebRTC `offer`。

---

### answer

WebRTC SDP Answer，响应客户端的 `offer`。

```json
{"type": "answer", "data": {"sdp": "<SDP 字符串>"}}
```

收到后调用 `pc.setRemoteDescription({type: 'answer', sdp: data.sdp})`。

---

### ice_candidate

服务器侧的 ICE 候选项。

```json
{
  "type": "ice_candidate",
  "data": {
    "candidate": "candidate:...",
    "sdp_mid": "0",
    "sdp_mline_index": 0
  }
}
```

---

### user_joined

有新用户加入服务器。

```json
{
  "type": "user_joined",
  "data": {
    "session_id": 5,
    "name": "NewUser",
    "channel_id": 1,
    "mute": false,
    "deaf": false,
    "self_mute": false,
    "self_deaf": false
  }
}
```

---

### user_left

有用户离开服务器。

```json
{"type": "user_left", "data": {"session_id": 5}}
```

---

### user_state

用户状态变更（静音、屏蔽、切换频道、改名）。只有发生变更的字段不为 null。

```json
{
  "type": "user_state",
  "data": {
    "session_id": 5,
    "channel_id": 2,
    "name": null,
    "mute": null,
    "deaf": null,
    "self_mute": true,
    "self_deaf": null
  }
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `session_id` | number | 发生变更的用户 |
| `channel_id` | number \| null | 新频道（null 表示未变） |
| `name` | string \| null | 新用户名（null 表示未变） |
| `mute` | bool \| null | 服务器静音状态 |
| `deaf` | bool \| null | 服务器屏蔽状态 |
| `self_mute` | bool \| null | 用户自己静音 |
| `self_deaf` | bool \| null | 用户自己屏蔽 |

---

### chat_received

收到文字消息。

```json
{
  "type": "chat_received",
  "data": {
    "sender_session": 5,
    "sender_name": "OtherUser",
    "channel_id": 1,
    "message": "Hello!",
    "timestamp": 1741276800
  }
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `sender_session` | number | 发送者的 session ID |
| `sender_name` | string | 发送者用户名 |
| `channel_id` | number | 频道 ID |
| `message` | string | 消息内容 |
| `timestamp` | number | Unix 时间戳（秒） |

---

### channel_updated

频道列表变更（新建/删除频道时触发）。

```json
{
  "type": "channel_updated",
  "data": {
    "channels": [
      {"id": 0, "name": "Root", "parent_id": 0, "description": ""},
      {"id": 1, "name": "General", "parent_id": 0, "description": ""}
    ]
  }
}
```

---

### error

请求处理失败。

```json
{"type": "error", "data": {"code": "connect_failed", "message": "Connection refused"}}
```

| `code` 值 | 触发场景 |
|-----------|---------|
| `invalid_message` | 消息格式解析失败 |
| `connect_failed` | 连接 Mumble 服务器失败 |
| `offer_failed` | WebRTC offer 处理失败 |
| `ice_failed` | ICE candidate 添加失败 |
| `chat_failed` | 发送消息失败 |
| `channel_join_failed` | 切换频道失败 |
| `mute_failed` | 静音操作失败 |
| `deafen_failed` | 屏蔽操作失败 |

---

## 完整示例代码

```html
<!DOCTYPE html>
<html>
<head><title>MumDota Client</title></head>
<body>
  <input id="nick" placeholder="用户名" value="WebUser" />
  <button id="connect">连接</button>
  <button id="disconnect" disabled>断开</button>
  <div id="status">未连接</div>
  <div id="channels"></div>
  <input id="chatInput" placeholder="输入消息..." disabled />
  <div id="chatLog"></div>

  <script>
    let ws, pc, localStream;
    const statusEl = document.getElementById('status');

    document.getElementById('connect').onclick = async () => {
      await startSession(document.getElementById('nick').value);
    };
    document.getElementById('disconnect').onclick = () => {
      ws?.send(JSON.stringify({type: 'disconnect'}));
      cleanup();
    };

    async function startSession(username) {
      ws = new WebSocket(`ws://${location.hostname}:8080/ws`);
      ws.onopen = () => {
        statusEl.textContent = '已连接，正在验证...';
        ws.send(JSON.stringify({type: 'connect', data: {username}}));
      };
      ws.onmessage = (e) => handleMessage(JSON.parse(e.data));
      ws.onclose = () => { statusEl.textContent = '已断开'; cleanup(); };

      localStream = await navigator.mediaDevices.getUserMedia({
        audio: {echoCancellation: true, noiseSuppression: true, sampleRate: 48000, channelCount: 1}
      });
    }

    async function handleMessage({type, data}) {
      switch (type) {
        case 'connected':
          statusEl.textContent = `已连接，session ${data.session_id}`;
          renderChannels(data.channels);
          document.getElementById('chatInput').disabled = false;
          document.getElementById('disconnect').disabled = false;
          document.getElementById('connect').disabled = true;
          await setupWebRTC();
          break;

        case 'answer':
          await pc.setRemoteDescription({type: 'answer', sdp: data.sdp});
          break;

        case 'ice_candidate':
          await pc.addIceCandidate({
            candidate: data.candidate,
            sdpMid: data.sdp_mid,
            sdpMLineIndex: data.sdp_mline_index
          });
          break;

        case 'channel_updated':
          renderChannels(data.channels);
          break;

        case 'user_joined':
          console.log('用户加入:', data.name, 'session', data.session_id);
          break;

        case 'user_left':
          console.log('用户离开: session', data.session_id);
          break;

        case 'user_state':
          console.log('用户状态变更:', data);
          break;

        case 'chat_received':
          appendChat(data.sender_name, data.message);
          break;

        case 'error':
          statusEl.textContent = `错误 [${data.code}]: ${data.message}`;
          break;
      }
    }

    async function setupWebRTC() {
      pc = new RTCPeerConnection({
        iceServers: [{urls: 'stun:stun.l.google.com:19302'}]
      });

      localStream.getTracks().forEach(t => pc.addTrack(t, localStream));

      pc.ontrack = (e) => {
        const audio = document.createElement('audio');
        audio.srcObject = e.streams[0];
        audio.autoplay = true;
        document.body.appendChild(audio);
        audio.play().catch(() => {
          const btn = document.createElement('button');
          btn.textContent = '点击开启音频';
          btn.onclick = () => { audio.play(); btn.remove(); };
          document.body.appendChild(btn);
        });
      };

      pc.onicecandidate = (e) => {
        if (e.candidate) {
          ws.send(JSON.stringify({
            type: 'ice_candidate',
            data: {
              candidate: e.candidate.candidate,
              sdp_mid: e.candidate.sdpMid,
              sdp_mline_index: e.candidate.sdpMLineIndex
            }
          }));
        }
      };

      pc.oniceconnectionstatechange = () => {
        console.log('ICE 状态:', pc.iceConnectionState);
        if (pc.iceConnectionState === 'connected') {
          statusEl.textContent = '语音已连接';
        }
      };

      // 限定使用 Opus 编解码器
      const transceiver = pc.getTransceivers()[0];
      if (transceiver?.setCodecPreferences) {
        const codecs = RTCRtpReceiver.getCapabilities('audio').codecs
          .filter(c => c.mimeType === 'audio/opus');
        if (codecs.length) transceiver.setCodecPreferences(codecs);
      }

      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      ws.send(JSON.stringify({type: 'offer', data: {sdp: offer.sdp}}));
    }

    function cleanup() {
      pc?.close();
      localStream?.getTracks().forEach(t => t.stop());
      ws?.close();
      pc = null; localStream = null; ws = null;
      document.getElementById('chatInput').disabled = true;
      document.getElementById('disconnect').disabled = true;
      document.getElementById('connect').disabled = false;
    }

    function renderChannels(channels) {
      document.getElementById('channels').innerHTML =
        '<h3>频道</h3>' + channels.map(c =>
          `<div><button onclick="joinChannel(${c.id})">${c.name}</button></div>`
        ).join('');
    }

    function appendChat(sender, message) {
      const el = document.getElementById('chatLog');
      el.innerHTML += `<div><b>${sender}:</b> ${message}</div>`;
      el.scrollTop = el.scrollHeight;
    }

    window.joinChannel = (id) => {
      ws.send(JSON.stringify({type: 'channel_join', data: {channel_id: id}}));
    };

    document.getElementById('chatInput').onkeydown = (e) => {
      if (e.key === 'Enter' && e.target.value) {
        ws.send(JSON.stringify({
          type: 'chat_send',
          data: {channel_id: 0, message: e.target.value}
        }));
        e.target.value = '';
      }
    };
  </script>
</body>
</html>
```

---

## 浏览器要求

- Chrome 70+ / Firefox 63+ / Safari 14+ / Edge 79+
- 生产环境需要 HTTPS 才能使用 `getUserMedia()`（localhost 除外）
- 音频播放需要用户手势触发（部分浏览器限制）
