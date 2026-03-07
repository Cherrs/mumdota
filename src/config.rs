use anyhow::Context;
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

            anyhow::ensure!(!var_name.is_empty(), "empty variable name in config placeholder");

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
    override_int!("server", "max_connections", "MUMDOTA_SERVER_MAX_CONNECTIONS");
    override_str!("mumble", "host", "MUMDOTA_MUMBLE_HOST");
    override_int!("mumble", "port", "MUMDOTA_MUMBLE_PORT");
    override_bool!("mumble", "accept_invalid_certs", "MUMDOTA_MUMBLE_ACCEPT_INVALID_CERTS");

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

        let config: Config =
            table.try_into().context("failed to deserialize config")?;
        Ok(config)
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
