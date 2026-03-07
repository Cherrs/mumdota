use serde::{Deserialize, Serialize};

// ── Client → Server messages ──

#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "data")]
#[serde(rename_all = "snake_case")]
pub enum ClientMessage {
    Connect(ConnectData),
    Disconnect,
    Offer(OfferData),
    IceCandidate(IceCandidateData),
    ChatSend(ChatSendData),
    ChannelJoin(ChannelJoinData),
    Mute(MuteData),
    Deafen(DeafenData),
}

#[derive(Debug, Deserialize)]
pub struct ConnectData {
    pub username: String,
}

#[derive(Debug, Deserialize)]
pub struct OfferData {
    pub sdp: String,
}

#[derive(Debug, Deserialize)]
pub struct IceCandidateData {
    pub candidate: String,
    #[serde(default)]
    pub sdp_mid: Option<String>,
    #[serde(default)]
    pub sdp_mline_index: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct ChatSendData {
    pub channel_id: u32,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct ChannelJoinData {
    pub channel_id: u32,
}

#[derive(Debug, Deserialize)]
pub struct MuteData {
    pub muted: bool,
}

#[derive(Debug, Deserialize)]
pub struct DeafenData {
    pub deafened: bool,
}

// ── Server → Client messages ──

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
#[serde(rename_all = "snake_case")]
pub enum ServerMessage {
    Answer(AnswerData),
    IceCandidate(IceCandidateResponse),
    Connected(ConnectedData),
    UserJoined(UserInfo),
    UserLeft(UserLeftData),
    UserState(UserStateData),
    ChatReceived(ChatReceivedData),
    ChannelUpdated(ChannelUpdatedData),
    Error(ErrorData),
}

#[derive(Debug, Clone, Serialize)]
pub struct AnswerData {
    pub sdp: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IceCandidateResponse {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<u16>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectedData {
    pub session_id: u32,
    pub channels: Vec<ChannelInfo>,
    pub users: Vec<UserInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelInfo {
    pub id: u32,
    pub name: String,
    pub parent_id: u32,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserInfo {
    pub session_id: u32,
    pub name: String,
    pub channel_id: u32,
    pub mute: bool,
    pub deaf: bool,
    pub self_mute: bool,
    pub self_deaf: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserLeftData {
    pub session_id: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserStateData {
    pub session_id: u32,
    pub channel_id: Option<u32>,
    pub name: Option<String>,
    pub mute: Option<bool>,
    pub deaf: Option<bool>,
    pub self_mute: Option<bool>,
    pub self_deaf: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatReceivedData {
    pub sender_session: u32,
    pub sender_name: String,
    pub channel_id: u32,
    pub message: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelUpdatedData {
    pub channels: Vec<ChannelInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorData {
    pub code: String,
    pub message: String,
}

impl ServerMessage {
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        ServerMessage::Error(ErrorData {
            code: code.into(),
            message: message.into(),
        })
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|e| {
            format!(
                r#"{{"type":"error","data":{{"code":"serialize_error","message":"{}"}}}}"#,
                e
            )
        })
    }
}
