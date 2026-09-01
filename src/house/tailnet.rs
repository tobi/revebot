//! Detect a running host Tailscale and expose its listen address.
//!
//! The house does not join a tailnet of its own and does not touch host
//! networking. If `tailscaled` is already up, serve binds the same HTTP/WS
//! surface (bearer token still required) on the node's Tailscale IPv4 so
//! other tailnet members can reach it. Being on the tailnet is transport,
//! not authorisation.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// A running tailnet node we can bind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    addr: SocketAddr,
    dns_name: String,
}

impl Endpoint {
    /// `100.x.y.z:port` on the tailnet.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://hostname.tailnet.ts.net:port/` (or the IP when `MagicDNS` is unnamed).
    pub fn origin(&self) -> String {
        let host = if self.dns_name.is_empty() {
            self.addr.ip().to_string()
        } else {
            self.dns_name.clone()
        };
        format_origin(&host, self.addr.port())
    }

    /// Bind an extra listener unless `--bind` already covers this address.
    pub fn needs_extra_listener(&self, local: SocketAddr) -> bool {
        !covers(local, self.addr.ip())
    }
}

/// Parse `--bind`, falling back to the house default.
#[must_use]
pub fn parse_bind(bind: &str) -> SocketAddr {
    bind.parse()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 7420)))
}

/// Probe `tailscaled`'s `LocalAPI`. `None` if Tailscale is absent or not running.
pub async fn detect(port: u16) -> Option<Endpoint> {
    for path in socket_paths() {
        if !path.exists() {
            continue;
        }
        if let Ok(Ok(endpoint)) =
            tokio::time::timeout(Duration::from_secs(2), probe(&path, port)).await
        {
            return endpoint;
        }
    }
    None
}

pub(crate) async fn probe(path: &Path, port: u16) -> anyhow::Result<Option<Endpoint>> {
    let json = local_api_status(path).await?;
    Ok(parse_status(&json, port))
}

fn socket_paths() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("/var/run/tailscale/tailscaled.sock"),
        PathBuf::from("/run/tailscale/tailscaled.sock"),
        PathBuf::from("/var/run/tailscaled.sock"),
    ];
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR")
        && !runtime.is_empty()
    {
        paths.push(PathBuf::from(runtime).join("tailscale/tailscaled.sock"));
    }
    paths
}

const MAX_STATUS_BYTES: u64 = 1024 * 1024;

async fn local_api_status(path: &Path) -> anyhow::Result<Value> {
    let mut stream = UnixStream::connect(path).await?;
    stream
        .write_all(
            b"GET /localapi/v0/status?peers=false HTTP/1.1\r\n\
Host: local-tailscaled.sock\r\n\
User-Agent: revebot\r\n\
Accept: application/json\r\n\
Connection: close\r\n\
\r\n",
        )
        .await?;
    stream.shutdown().await?;
    let mut buf = Vec::new();
    (&mut stream)
        .take(MAX_STATUS_BYTES)
        .read_to_end(&mut buf)
        .await?;
    let body = http_body(&buf).ok_or_else(|| anyhow::anyhow!("invalid localapi response"))?;
    Ok(serde_json::from_slice(body)?)
}

fn http_body(bytes: &[u8]) -> Option<&[u8]> {
    const SEP: &[u8] = b"\r\n\r\n";
    let pos = bytes.windows(SEP.len()).position(|window| window == SEP)?;
    let (head, rest) = bytes.split_at(pos);
    let body = rest.get(SEP.len()..)?;
    if head.starts_with(b"HTTP/1.1 200") || head.starts_with(b"HTTP/1.0 200") {
        Some(body)
    } else {
        None
    }
}

fn parse_status(json: &Value, port: u16) -> Option<Endpoint> {
    let state = json.get("BackendState")?.as_str()?;
    if state != "Running" {
        return None;
    }
    let ips = json
        .get("TailscaleIPs")
        .and_then(Value::as_array)
        .or_else(|| json.pointer("/Self/TailscaleIPs").and_then(Value::as_array))?;
    let ip = ips
        .iter()
        .filter_map(|value| value.as_str()?.parse::<IpAddr>().ok())
        .find(IpAddr::is_ipv4)?;
    let dns_name = json
        .pointer("/Self/DNSName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end_matches('.')
        .to_string();
    Some(Endpoint {
        addr: SocketAddr::new(ip, port),
        dns_name,
    })
}

fn covers(local: SocketAddr, ip: IpAddr) -> bool {
    if local.ip() == ip {
        return true;
    }
    match local.ip() {
        IpAddr::V4(v4) if v4.is_unspecified() && ip.is_ipv4() => true,
        IpAddr::V6(v6) if v6.is_unspecified() => true,
        _ => false,
    }
}

fn format_origin(host: &str, port: u16) -> String {
    match port {
        80 => format!("http://{host}/"),
        port => format!("http://{host}:{port}/"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    fn running() -> Value {
        json!({
            "BackendState": "Running",
            "TailscaleIPs": ["100.1.2.3", "fd7a:115c:a1e0::1"],
            "Self": { "DNSName": "box.tail123.ts.net." }
        })
    }

    #[test]
    fn running_status_yields_ipv4_and_magicdns() {
        let endpoint = parse_status(&running(), 7420).expect("running");
        assert_eq!(endpoint.addr(), "100.1.2.3:7420".parse().unwrap());
        assert_eq!(endpoint.origin(), "http://box.tail123.ts.net:7420/");
    }

    #[test]
    fn origin_omits_default_http_port() {
        let endpoint = parse_status(&running(), 80).expect("running");
        assert_eq!(endpoint.origin(), "http://box.tail123.ts.net/");
    }

    #[test]
    fn missing_magicdns_falls_back_to_the_ip() {
        let json = json!({
            "BackendState": "Running",
            "TailscaleIPs": ["100.9.8.7"],
        });
        let endpoint = parse_status(&json, 7420).expect("running");
        assert_eq!(endpoint.origin(), "http://100.9.8.7:7420/");
    }

    #[test]
    fn self_ips_are_used_when_the_top_level_list_is_absent() {
        let json = json!({
            "BackendState": "Running",
            "Self": {
                "DNSName": "box.tail123.ts.net.",
                "TailscaleIPs": ["100.4.5.6"]
            }
        });
        let endpoint = parse_status(&json, 7420).expect("running");
        assert_eq!(endpoint.addr().ip(), "100.4.5.6".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn not_running_is_ignored() {
        for state in ["Stopped", "NeedsLogin", "Starting", "NoState"] {
            let json = json!({
                "BackendState": state,
                "TailscaleIPs": ["100.1.2.3"]
            });
            assert_eq!(parse_status(&json, 7420), None, "{state}");
        }
    }

    #[test]
    fn ipv6_only_is_ignored() {
        let json = json!({
            "BackendState": "Running",
            "TailscaleIPs": ["fd7a:115c:a1e0::1"]
        });
        assert_eq!(parse_status(&json, 7420), None);
    }

    #[test]
    fn loopback_bind_needs_an_extra_listener() {
        let endpoint = parse_status(&running(), 7420).expect("running");
        let local: SocketAddr = "127.0.0.1:7420".parse().unwrap();
        assert!(endpoint.needs_extra_listener(local));
    }

    #[test]
    fn unspecified_bind_already_covers_the_tailnet() {
        let endpoint = parse_status(&running(), 7420).expect("running");
        let all: SocketAddr = "0.0.0.0:7420".parse().unwrap();
        let v6: SocketAddr = "[::]:7420".parse().unwrap();
        let same: SocketAddr = "100.1.2.3:7420".parse().unwrap();
        assert!(!endpoint.needs_extra_listener(all));
        assert!(!endpoint.needs_extra_listener(v6));
        assert!(!endpoint.needs_extra_listener(same));
    }

    #[test]
    fn parse_bind_falls_back_to_the_house_default() {
        assert_eq!(
            parse_bind("127.0.0.1:9000"),
            "127.0.0.1:9000".parse().unwrap()
        );
        assert_eq!(
            parse_bind("not-an-addr"),
            SocketAddr::from(([127, 0, 0, 1], 7420))
        );
    }

    #[test]
    fn http_body_requires_200() {
        let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(http_body(ok), Some(&b"{}"[..]));
        let deny = b"HTTP/1.1 401 Unauthorized\r\n\r\nno";
        assert_eq!(http_body(deny), None);
    }

    #[tokio::test]
    async fn probe_reads_a_running_status_from_the_local_api() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("tailscaled.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let body = running().to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 1024];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        let endpoint = probe(&sock, 7420)
            .await
            .expect("localapi")
            .expect("running");
        assert_eq!(endpoint.origin(), "http://box.tail123.ts.net:7420/");
    }

    #[tokio::test]
    async fn probe_treats_a_stopped_daemon_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("tailscaled.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let body = json!({"BackendState":"Stopped","TailscaleIPs":["100.1.2.3"]}).to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 1024];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        let endpoint = probe(&sock, 7420).await.expect("localapi");
        assert_eq!(endpoint, None);
    }
}
