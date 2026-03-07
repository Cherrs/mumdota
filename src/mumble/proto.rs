use mumble_protocol::control::msgs;

pub fn build_version_message() -> msgs::Version {
    let mut msg = msgs::Version::new();
    msg.set_version(0x00010400); // 1.4.0
    msg.set_release("mumdota 0.1.0".to_string());
    msg.set_os("Linux".to_string());
    msg.set_os_version("Rust".to_string());
    msg
}

pub fn build_authenticate_message(username: &str) -> msgs::Authenticate {
    let mut msg = msgs::Authenticate::new();
    msg.set_username(username.to_string());
    msg.set_opus(true);
    msg
}

pub fn build_text_message(channel_id: u32, message: &str) -> msgs::TextMessage {
    let mut msg = msgs::TextMessage::new();
    msg.mut_channel_id().push(channel_id);
    msg.set_message(message.to_string());
    msg
}

pub fn build_user_state_channel(channel_id: u32) -> msgs::UserState {
    let mut msg = msgs::UserState::new();
    msg.set_channel_id(channel_id);
    msg
}

pub fn build_user_state_mute(self_mute: bool) -> msgs::UserState {
    let mut msg = msgs::UserState::new();
    msg.set_self_mute(self_mute);
    msg
}

pub fn build_user_state_deaf(self_deaf: bool) -> msgs::UserState {
    let mut msg = msgs::UserState::new();
    msg.set_self_deaf(self_deaf);
    if self_deaf {
        msg.set_self_mute(true);
    }
    msg
}

pub fn build_ping() -> msgs::Ping {
    let mut msg = msgs::Ping::new();
    msg.set_timestamp(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    );
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_text_message_targets_requested_channel() {
        let msg = build_text_message(7, "hello team");

        assert_eq!(msg.get_channel_id(), &[7]);
        assert_eq!(msg.get_message(), "hello team");
    }

    #[test]
    fn build_user_state_channel_sets_destination_channel() {
        let msg = build_user_state_channel(9);

        assert!(msg.has_channel_id());
        assert_eq!(msg.get_channel_id(), 9);
    }

    #[test]
    fn build_user_state_deaf_also_self_mutes_when_enabled() {
        let msg = build_user_state_deaf(true);

        assert!(msg.has_self_deaf());
        assert!(msg.get_self_deaf());
        assert!(msg.has_self_mute());
        assert!(msg.get_self_mute());
    }
}
