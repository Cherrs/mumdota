use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use turn::auth::{generate_auth_key, AuthHandler};

#[derive(Debug, Clone, Serialize)]
pub struct IceServer {
    pub urls: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IceConfig {
    pub ice_servers: Vec<IceServer>,
    pub expires_at: Option<u64>,
}

#[derive(Clone)]
struct Credential {
    session: String,
    username: String,
    password: String,
    key: Vec<u8>,
    expires_at: u64,
}

pub struct Credentials {
    realm: String,
    ttl: u64,
    entries: RwLock<HashMap<String, Credential>>,
}

pub fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl Credentials {
    pub fn new(realm: String, ttl: u64) -> Self {
        Self {
            realm,
            ttl,
            entries: RwLock::new(HashMap::new()),
        }
    }

    pub fn issue(&self, session: &str, urls: &[String]) -> IceConfig {
        self.issue_at(session, urls, unix_time())
    }

    fn issue_at(&self, session: &str, urls: &[String], now: u64) -> IceConfig {
        let mut entries = self.entries.write().expect("credential lock poisoned");
        entries.retain(|_, c| c.expires_at > now);
        // Keep the previous credential valid during an ICE restart. Repeated
        // refresh requests reuse the current credential instead of growing state.
        let credential = entries
            .values()
            .find(|c| c.session == session && c.expires_at > now + 90)
            .cloned()
            .unwrap_or_else(|| {
                let username = format!("{}:{}", now + self.ttl, uuid::Uuid::new_v4());
                let password = format!(
                    "{}{}",
                    uuid::Uuid::new_v4().simple(),
                    uuid::Uuid::new_v4().simple()
                );
                let credential = Credential {
                    session: session.into(),
                    key: generate_auth_key(&username, &self.realm, &password),
                    username: username.clone(),
                    password,
                    expires_at: now + self.ttl,
                };
                entries.insert(username, credential.clone());
                credential
            });
        IceConfig {
            ice_servers: urls
                .iter()
                .map(|url| IceServer {
                    urls: url.clone(),
                    username: url.starts_with("turn").then(|| credential.username.clone()),
                    credential: url.starts_with("turn").then(|| credential.password.clone()),
                })
                .collect(),
            expires_at: Some(credential.expires_at),
        }
    }

    pub fn revoke(&self, session: &str) -> Vec<String> {
        let mut entries = self.entries.write().expect("credential lock poisoned");
        let usernames = entries
            .values()
            .filter(|c| c.session == session)
            .map(|c| c.username.clone())
            .collect();
        entries.retain(|_, c| c.session != session);
        usernames
    }

    fn authenticate_at(
        &self,
        username: &str,
        realm: &str,
        now: u64,
    ) -> Result<Vec<u8>, turn::Error> {
        if realm != self.realm {
            return Err(turn::Error::Other("invalid realm".into()));
        }
        self.entries
            .read()
            .expect("credential lock poisoned")
            .get(username)
            .filter(|c| c.expires_at > now)
            .map(|c| c.key.clone())
            .ok_or_else(|| turn::Error::Other("expired or unknown session credential".into()))
    }
}

impl AuthHandler for Credentials {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        _src_addr: SocketAddr,
    ) -> Result<Vec<u8>, turn::Error> {
        self.authenticate_at(username, realm, unix_time())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_per_session_expire_and_are_revoked() {
        let auth = Credentials::new("test".into(), 300);
        let urls = vec!["stun:localhost:3478".into(), "turn:localhost:3478".into()];
        let a = auth.issue_at("a", &urls, 1000);
        let b = auth.issue_at("b", &urls, 1000);
        assert!(a.ice_servers[0].credential.is_none());
        let user = a.ice_servers[1].username.as_ref().unwrap();
        assert_ne!(Some(user), b.ice_servers[1].username.as_ref());
        assert!(auth.authenticate_at(user, "test", 1299).is_ok());
        assert!(auth.authenticate_at(user, "test", 1300).is_err());
        assert!(auth.authenticate_at(user, "other", 1001).is_err());
        let refreshed = auth.issue_at("a", &urls, 1250);
        assert_ne!(a.ice_servers[1].username, refreshed.ice_servers[1].username);
        assert_eq!(auth.revoke("a").len(), 2);
        assert!(auth.authenticate_at(user, "test", 1251).is_err());
        assert!(auth
            .authenticate_at(b.ice_servers[1].username.as_ref().unwrap(), "test", 1251)
            .is_ok());
    }
}
