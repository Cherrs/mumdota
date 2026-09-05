# MumDota 浏览器协议 v2

网页可部署在 Cloudflare Workers。浏览器通过 `wss://voice.example.com/ws` 直达 MumDota；媒体使用 WebRTC 直连，无法直连时使用同一 MumDota 进程内置的 TURN。无需 coturn 或其他 TURN 服务。

v2 改为服务端发起 SDP offer。旧客户端发送 `offer` 会收到 `upgrade_required`，需要刷新并升级客户端。前后端应配套发布。

## 建立连接和语音

1. 打开 WebSocket 后 10 秒内发送 `connect`，用户名不能为空，最多 128 字节。
2. 收到 `connected` 后，用 `data.ice.ice_servers` 创建 `RTCPeerConnection`。只读文字客户端可以跳过语音步骤。
3. 用户允许麦克风后添加本地音轨，发送 `start_voice`。
4. 收到服务端 `offer`，调用 `setRemoteDescription`、`createAnswer`、`setLocalDescription`，发送 `answer`。
5. 双向交换 `ice_candidate`；远端 SDP 设置完成前先缓存候选。对 WebSocket 消息串行处理，避免异步 SDP 与候选处理发生竞争。
6. 新用户加入会增加独立接收音轨，并由 MumDota 再次发送 offer。每位说话人的 stream ID 为 `mumble-stream-<session_id>`，分别播放，在浏览器中混音。

```json
{"type":"connect","data":{"username":"Player"}}
```

`connected` 的数据示例（凭证仅为说明用途）：

```json
{
  "type": "connected",
  "data": {
    "protocol_version": 2,
    "session_id": 1,
    "channels": [{"id":0,"name":"Root","parent_id":0,"description":""}],
    "users": [{"session_id":1,"name":"Player","channel_id":0,"mute":false,"deaf":false,"self_mute":false,"self_deaf":false}],
    "ice": {
      "ice_servers": [
        {"urls":"stun:turn.example.com:3478"},
        {"urls":"turn:turn.example.com:3478?transport=udp","username":"session-user","credential":"temporary-password"},
        {"urls":"turn:turn.example.com:3478?transport=tcp","username":"session-user","credential":"temporary-password"},
        {"urls":"turns:turn.example.com:5349?transport=tcp","username":"session-user","credential":"temporary-password"}
      ],
      "expires_at": 2000000000
    }
  }
}
```

`expires_at` 是 Unix 秒；无到期凭证时为 `null`。TLS 未配置时不会下发 `turns:`。TURN 凭证仅在成功登录 Mumble 后签发，并绑定当前 WebSocket 会话；不要写入日志、Worker 配置或浏览器持久存储。

## 客户端消息

| type | data | 作用 |
| --- | --- | --- |
| `connect` | `{username}` | 登录 Mumble，必须是第一条文本消息 |
| `disconnect` | 无 | 关闭会话和 WebSocket |
| `start_voice` | 无 | 启动语音，等待服务端 offer |
| `answer` | `{sdp}` | 回答服务端 offer |
| `ice_candidate` | `{candidate,sdp_mid,sdp_mline_index}` | ICE 候选，后两项可为 `null` |
| `ice_refresh` | 无 | 获取当前或即将轮换的会话凭证 |
| `ice_restart` | 无 | 请求服务端发起 ICE restart |
| `channel_join` | `{channel_id}` | 切换频道 |
| `chat_send` | `{channel_id,message}` | 发送文字，最多 4096 字节 |
| `mute` | `{muted}` | 设置静音 |
| `deafen` | `{deafened}` | 设置耳聋 |

## 服务端消息

| type | data |
| --- | --- |
| `connected` | 上面的登录结果 |
| `offer` | `{sdp}` |
| `ice_candidate` | `{candidate,sdp_mid,sdp_mline_index}` |
| `ice_config` | `{ice_servers,expires_at}`，与登录结果中的 `ice` 相同结构 |
| `channel_updated` | `{channels:[{id,name,parent_id,description}]}`，更新已知频道 |
| `user_joined` | `{session_id,name,channel_id,mute,deaf,self_mute,self_deaf}` |
| `user_left` | `{session_id}`，移除用户及对应音频元素 |
| `user_state` | `{session_id,channel_id,name,mute,deaf,self_mute,self_deaf}`，可空字段只更新非空值 |
| `chat_received` | `{sender_session,sender_name,message,channel_id,timestamp}`，时间为 Unix 秒 |
| `error` | `{code,message}` |

## 刷新和故障恢复

在到期前约 60 秒发送 `ice_refresh`。收到 `ice_config` 后调用 `pc.setConfiguration({iceServers: data.ice_servers})`，再发送 `ice_restart`。服务端对协商排队，同一连接始终由服务端创建 offer，避免双方同时发起协商。默认凭证 TTL 为一小时，旧凭证保留到其原到期时间，使重启期间可以平滑切换。

ICE `disconnected` 等待约 3 秒，持续失败时刷新凭证并重启 ICE。重复的失败通知不能中断正在进行的重启。15 秒仍未成功时，关闭整个连接并重新登录 Mumble；应用客户端采用有上限的指数退避。

`mumble_disconnected`、`connect_failed` 是终止错误，后端随即关闭 WebSocket；客户端收到时立即清除旧的 connected 状态，无需等待 close 事件。`voice_error` 应重建会话，以便重新初始化 Mumble UDP 和协商。`upgrade_required` 要求刷新网页。

所有 WebSocket 帧/消息限制为 128KiB。服务端每 15 秒发送 Ping；浏览器会自动回应 Pong。静默连接会在 45 秒后关闭。断开时撤销会话 TURN 凭证及其 allocations。

## 部署验收

MumDota 固定媒体端口默认为 UDP 50000；内置 TURN 使用 UDP/TCP 3478 和可选 TLS/TCP 5349。公网 IP 和端口映射需要指向同一实例。TURN 域名使用 DNS-only，证书应匹配该域名；TURN TLS 不能接到普通 HTTP Ingress。

`/health` 为进程存活；`/ready` 检查上游 TCP 可达性，不等价于端到端音频成功。应用中的“连接质量”使用实际选中的 ICE candidate pair 判断直连或 TURN，并展示传输协议、RTT、接收抖动与累计丢包率。

同目录客户端 `client.html` 可作为诊断页。通过 HTTPS 提供该文件，附加 `?relay=1&turnTransport=udp`、`tcp` 或 `tls`，分别强制验证内置 TURN 的三种传输。完整应用实现位于 playdota2win 的 `src/lib/mumble/client.ts`，部署配置参见 MumDota README。
