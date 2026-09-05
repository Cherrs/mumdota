use anyhow::Context;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub mumble: MumbleConfig,
    pub webrtc: WebrtcConfig,
    #[serde(default)]
    pub turn: TurnConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub listen_addr: String,
    pub listen_port: u16,
    pub max_connections: usize,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MumbleConfig {
    pub host: String,
    pub port: u16,
    pub accept_invalid_certs: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebrtcConfig {
    #[serde(default)]
    pub stun_servers: Vec<String>,
    #[serde(default = "default_media_port")]
    pub udp_port: u16,
    #[serde(default)]
    pub public_ip: Option<std::net::Ipv4Addr>,
}

fn default_media_port() -> u16 {
    50000
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TurnConfig {
    pub enabled: bool,
    pub listen_addr: std::net::Ipv4Addr,
    pub port: u16,
    pub public_ip: Option<std::net::Ipv4Addr>,
    pub public_host: String,
    pub realm: String,
    pub relay_min_port: u16,
    pub relay_max_port: u16,
    pub credential_ttl_secs: u64,
    pub tls_port: u16,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_addr: std::net::Ipv4Addr::UNSPECIFIED,
            port: 3478,
            public_ip: None,
            public_host: String::new(),
            realm: "mumdota".into(),
            relay_min_port: 49160,
            relay_max_port: 49999,
            credential_ttl_secs: 3600,
            tls_port: 5349,
            tls_cert: None,
            tls_key: None,
        }
    }
}

/// Expand `${VAR}` and `${VAR:-default}` placeholders in a TOML string using
/// environment variables.  Returns an error if a placeholder references an
/// unset variable with no default value.
fn expand_env_vars(input: &str) -> anyhow::Result<String> {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut inner = String::new();
            let mut closed = false;
            for c in chars.by_ref() {
                if c == '}' {
                    closed = true;
                    break;
                }
                inner.push(c);
            }
            anyhow::ensure!(closed, "unclosed '${{' in config");

            // Split on `:-` to support `${VAR:-default}`
            let (var_name, default_val) = if let Some(idx) = inner.find(":-") {
                (&inner[..idx], Some(&inner[idx + 2..]))
            } else {
                (inner.as_str(), None)
            };

            anyhow::ensure!(
                !var_name.is_empty(),
                "empty variable name in config placeholder"
            );

            match std::env::var(var_name) {
                Ok(val) => output.push_str(&val),
                Err(_) => match default_val {
                    Some(def) => output.push_str(def),
                    None => anyhow::bail!(
                        "environment variable '{}' is not set and has no default value",
                        var_name
                    ),
                },
            }
        } else {
            output.push(ch);
        }
    }

    Ok(output)
}

/// Apply `MUMDOTA_*` environment variable overrides onto a parsed TOML table.
///
/// Mapping:
///   `MUMDOTA_SERVER_LISTEN_ADDR`   → `server.listen_addr`
///   `MUMDOTA_SERVER_LISTEN_PORT`   → `server.listen_port`
///   `MUMDOTA_SERVER_MAX_CONNECTIONS` → `server.max_connections`
///   `MUMDOTA_MUMBLE_HOST`          → `mumble.host`
///   `MUMDOTA_MUMBLE_PORT`          → `mumble.port`
///   `MUMDOTA_MUMBLE_ACCEPT_INVALID_CERTS` → `mumble.accept_invalid_certs`
///   `MUMDOTA_WEBRTC_STUN_SERVERS`  → `webrtc.stun_servers` (comma-separated)
fn apply_env_overrides(table: &mut toml::Table) {
    use toml::Value;

    macro_rules! override_str {
        ($section:literal, $key:literal, $env:literal) => {
            if let Ok(val) = std::env::var($env) {
                table
                    .entry($section)
                    .or_insert_with(|| Value::Table(toml::Table::new()))
                    .as_table_mut()
                    .unwrap()
                    .insert($key.to_string(), Value::String(val));
            }
        };
    }
    macro_rules! override_int {
        ($section:literal, $key:literal, $env:literal) => {
            if let Ok(val) = std::env::var($env) {
                if let Ok(n) = val.parse::<i64>() {
                    table
                        .entry($section)
                        .or_insert_with(|| Value::Table(toml::Table::new()))
                        .as_table_mut()
                        .unwrap()
                        .insert($key.to_string(), Value::Integer(n));
                }
            }
        };
    }
    macro_rules! override_bool {
        ($section:literal, $key:literal, $env:literal) => {
            if let Ok(val) = std::env::var($env) {
                let b = matches!(val.to_lowercase().as_str(), "1" | "true" | "yes");
                table
                    .entry($section)
                    .or_insert_with(|| Value::Table(toml::Table::new()))
                    .as_table_mut()
                    .unwrap()
                    .insert($key.to_string(), Value::Boolean(b));
            }
        };
    }

    override_str!("server", "listen_addr", "MUMDOTA_SERVER_LISTEN_ADDR");
    override_int!("server", "listen_port", "MUMDOTA_SERVER_LISTEN_PORT");
    override_int!(
        "server",
        "max_connections",
        "MUMDOTA_SERVER_MAX_CONNECTIONS"
    );
    override_str!("mumble", "host", "MUMDOTA_MUMBLE_HOST");
    override_int!("mumble", "port", "MUMDOTA_MUMBLE_PORT");
    override_bool!(
        "mumble",
        "accept_invalid_certs",
        "MUMDOTA_MUMBLE_ACCEPT_INVALID_CERTS"
    );

    if let Ok(val) = std::env::var("MUMDOTA_WEBRTC_STUN_SERVERS") {
        let servers: Vec<Value> = val
            .split(',')
            .map(|s| Value::String(s.trim().to_string()))
            .collect();
        table
            .entry("webrtc")
            .or_insert_with(|| Value::Table(toml::Table::new()))
            .as_table_mut()
            .unwrap()
            .insert("stun_servers".to_string(), Value::Array(servers));
    }

    override_int!("webrtc", "udp_port", "MUMDOTA_WEBRTC_UDP_PORT");
    override_str!("webrtc", "public_ip", "MUMDOTA_WEBRTC_PUBLIC_IP");
    override_bool!("turn", "enabled", "MUMDOTA_TURN_ENABLED");
    override_str!("turn", "listen_addr", "MUMDOTA_TURN_LISTEN_ADDR");
    override_str!("turn", "public_ip", "MUMDOTA_TURN_PUBLIC_IP");
    override_str!("turn", "public_host", "MUMDOTA_TURN_PUBLIC_HOST");
    override_str!("turn", "realm", "MUMDOTA_TURN_REALM");
    override_str!("turn", "tls_cert", "MUMDOTA_TURN_TLS_CERT");
    override_str!("turn", "tls_key", "MUMDOTA_TURN_TLS_KEY");
    override_int!("turn", "port", "MUMDOTA_TURN_PORT");
    override_int!("turn", "tls_port", "MUMDOTA_TURN_TLS_PORT");
    override_int!("turn", "relay_min_port", "MUMDOTA_TURN_RELAY_MIN_PORT");
    override_int!("turn", "relay_max_port", "MUMDOTA_TURN_RELAY_MAX_PORT");
    override_int!(
        "turn",
        "credential_ttl_secs",
        "MUMDOTA_TURN_CREDENTIAL_TTL_SECS"
    );
    if let Ok(origins) = std::env::var("MUMDOTA_SERVER_ALLOWED_ORIGINS") {
        table
            .entry("server")
            .or_insert_with(|| Value::Table(toml::Table::new()))
            .as_table_mut()
            .unwrap()
            .insert(
                "allowed_origins".into(),
                Value::Array(
                    origins
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| Value::String(s.into()))
                        .collect(),
                ),
            );
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config file: {}", path.as_ref().display()))?;

        // Phase 1: expand ${VAR} / ${VAR:-default} placeholders
        let expanded = expand_env_vars(&raw)
            .context("failed to expand environment variable placeholders in config")?;

        // Phase 2: parse TOML, then apply MUMDOTA_* overrides
        let mut table: toml::Table =
            toml::from_str(&expanded).context("failed to parse config TOML")?;
        apply_env_overrides(&mut table);

        let config: Config = table.try_into().context("failed to deserialize config")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.server.max_connections > 0,
            "max_connections must be positive"
        );
        if self.turn.enabled {
            let ip = self
                .turn
                .public_ip
                .context("TURN requires turn.public_ip")?;
            anyhow::ensure!(
                !ip.is_unspecified() && !ip.is_multicast(),
                "invalid TURN public IP"
            );
            anyhow::ensure!(
                self.webrtc.public_ip == Some(ip),
                "TURN and WebRTC public_ip must match"
            );
            anyhow::ensure!(
                self.webrtc.udp_port != 0,
                "TURN requires a fixed WebRTC udp_port"
            );
            anyhow::ensure!(
                self.turn.port != 0
                    && self.turn.port != self.webrtc.udp_port
                    && self.turn.port != self.server.listen_port,
                "TURN/media ports must be distinct and nonzero"
            );
            anyhow::ensure!(!self.turn.realm.is_empty(), "TURN realm is required");
            anyhow::ensure!(
                !self.turn.public_host.is_empty()
                    && self
                        .turn
                        .public_host
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'),
                "TURN public_host must be an IPv4 address or hostname"
            );
            anyhow::ensure!(
                (300..=86400).contains(&self.turn.credential_ttl_secs),
                "TURN credential TTL must be 300..86400 seconds"
            );
            let ports = self.turn.relay_min_port..=self.turn.relay_max_port;
            anyhow::ensure!(
                self.turn.relay_min_port > 0
                    && self.turn.relay_min_port <= self.turn.relay_max_port
                    && !ports.contains(&self.webrtc.udp_port)
                    && !ports.contains(&self.turn.port),
                "invalid or overlapping TURN relay port range"
            );
            anyhow::ensure!(
                self.turn.tls_cert.is_some() == self.turn.tls_key.is_some(),
                "TURN TLS requires both certificate and key"
            );
            if self.turn.tls_cert.is_some() {
                anyhow::ensure!(
                    self.turn.tls_port != 0
                        && self.turn.tls_port != self.turn.port
                        && self.turn.tls_port != self.server.listen_port,
                    "TURN TLS port conflicts with another TCP listener"
                );
            }
        }
        Ok(())
    }

    pub fn mumble_addr(&self) -> String {
        format!("{}:{}", self.mumble.host, self.mumble.port)
    }

    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.server.listen_addr, self.server.listen_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_default_config_loads_without_example_variables() {
        // Use the exact file copied into the Docker image, including its comments.
        let config = Config::load(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml"))
            .expect("the shipped config must load without setting example variables");
        assert!(!config.mumble.host.is_empty());
    }

    #[test]
    fn test_expand_env_vars_plain() {
        std::env::set_var("TEST_HOST", "mumble.example.com");
        let result = expand_env_vars("host = \"${TEST_HOST}\"").unwrap();
        assert_eq!(result, "host = \"mumble.example.com\"");
    }

    #[test]
    fn test_expand_env_vars_default() {
        std::env::remove_var("TEST_UNSET_VAR");
        let result = expand_env_vars("port = \"${TEST_UNSET_VAR:-64738}\"").unwrap();
        assert_eq!(result, "port = \"64738\"");
    }

    #[test]
    fn test_expand_env_vars_missing_no_default() {
        std::env::remove_var("TEST_MISSING_VAR");
        let result = expand_env_vars("host = \"${TEST_MISSING_VAR}\"");
        assert!(result.is_err());
    }
}
