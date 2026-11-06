//! Find AI that's ready to use on this machine: env keys. The scan is bounded.

use crate::{BackendConfig, BackendKind, Detected};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Per-server connect bound.
pub const PROBE: Duration = Duration::from_millis(300);
/// Whole-scan bound.
pub const TOTAL: Duration = Duration::from_millis(1500);

fn cfg(name: &str, kind: BackendKind, model: &str) -> BackendConfig {
    BackendConfig { name: name.into(), kind, model: model.into(), base_url: None, ssh_host: None, enabled: true }
}

fn cfg_at(name: &str, kind: BackendKind, model: &str, base: &str) -> BackendConfig {
    BackendConfig { base_url: Some(base.into()), ..cfg(name, kind, model) }
}

/// Environment keys that are set.
pub fn env_keys(get: impl Fn(&str) -> Option<String>) -> Vec<Detected> {
    let mut out = Vec::new();
    // Hosted APIs we know by name. Offered only when their key is already set.
    for p in crate::providers::PROVIDERS {
        if let Some(var) = p.key_names().find(|var| get(var).is_some_and(|v| !v.trim().is_empty())) {
            out.push(Detected {
                suggestion: cfg_at(p.name, p.kind, p.default_model, p.base_url),
                reason: format!("Found {var} in your environment"),
                needs_key: false,
            });
        }
    }
    out
}

/// Is something listening at this URL's host:port? (bounded TCP connect)
pub fn port_open(base_url: &str, timeout: Duration) -> bool {
    let hostport = base_url.split("://").nth(1).unwrap_or(base_url).split('/').next().unwrap_or_default();
    let addrs: Vec<SocketAddr> = hostport.to_socket_addrs().map(|a| a.collect()).unwrap_or_default();
    addrs.iter().any(|a| TcpStream::connect_timeout(a, timeout).is_ok())
}

/// Everything, bounded by [`TOTAL`].
pub fn detect_all() -> Vec<Detected> {
    env_keys(|v| std::env::var(v).ok())
}
