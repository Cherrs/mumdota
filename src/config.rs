use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub mumble: MumbleConfig,
    pub webrtc: WebrtcConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub listen_addr: String,
    pub listen_port: u16,
    pub max_connections: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MumbleConfig {
    pub host: String,
    pub port: u16,
    pub accept_invalid_certs: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebrtcConfig {
    pub stun_servers: Vec<String>,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    pub fn mumble_addr(&self) -> String {
        format!("{}:{}", self.mumble.host, self.mumble.port)
    }

    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.server.listen_addr, self.server.listen_port)
    }
}
