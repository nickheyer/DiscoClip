//! Who is on the other end of a request. On the wire the peer is whoever opened the TCP
//! connection; behind a reverse proxy that is the proxy. When the peer is one of the
//! configured trusted proxies, the headers it adds name the browser's address, the scheme
//! it spoke and the host it asked for, and those are what sessions, rate limits, the audit
//! log, the origin check and login callbacks see. A peer that is not trusted keeps its own
//! address whatever headers it sends.

use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;
use ipnet::IpNet;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::AppState;
use super::error::ApiError;

/// An address or a network in CIDR form, as `web.trusted_proxies` lists them: `10.0.0.1`,
/// `10.0.0.0/8`, `::1` or `fd00::/8`. A bare address is that one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Network(pub IpNet);

impl Network {
    pub fn contains(&self, ip: IpAddr) -> bool {
        self.0.contains(&ip)
    }
}

impl FromStr for Network {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if let Ok(net) = text.parse::<IpNet>() {
            return Ok(Network(net));
        }
        text.parse::<IpAddr>()
            .map(|ip| Network(IpNet::from(ip)))
            .map_err(|_| format!("{text:?} is not an address or a network in CIDR form"))
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.prefix_len() == self.0.max_prefix_len() {
            self.0.addr().fmt(f)
        } else {
            self.0.fmt(f)
        }
    }
}

impl Serialize for Network {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Network {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

/// Placed in a connection's request extensions by the TLS listener, so requests know they
/// arrived encrypted even without a proxy saying so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tls;

/// The browser behind a request, once proxies are accounted for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    /// The address requests are attributed to.
    pub ip: IpAddr,
    /// The TCP peer: the browser, or the proxy in front of it.
    pub peer: SocketAddr,
    pub scheme: Scheme,
    /// The host the browser asked for, as the `Host` header or a trusted proxy names it.
    pub host: Option<String>,
    /// Whether a trusted proxy's headers were read.
    pub forwarded: bool,
}

impl ClientInfo {
    /// `scheme://host`, the origin browsers reached the app at.
    pub fn origin(&self) -> Option<String> {
        self.host
            .as_deref()
            .map(|host| format!("{}://{host}", self.scheme.as_str()))
    }
}

/// The trusted proxies, and how their headers are read.
#[derive(Debug, Clone, Default)]
pub struct Proxies {
    trusted: Vec<Network>,
}

impl Proxies {
    pub fn new(trusted: Vec<Network>) -> Self {
        Self { trusted }
    }

    pub fn is_trusted(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        self.trusted.iter().any(|net| net.contains(ip))
    }

    /// What a request from `peer` with `headers` is attributed to.
    pub fn resolve(&self, peer: SocketAddr, headers: &HeaderMap, tls: bool) -> ClientInfo {
        let mut info = ClientInfo {
            ip: canonical(peer.ip()),
            peer,
            scheme: if tls { Scheme::Https } else { Scheme::Http },
            host: header_text(headers, header::HOST),
            forwarded: false,
        };
        if !self.is_trusted(peer.ip()) {
            return info;
        }
        let forwarded = parse_forwarded(headers);
        let hops: Vec<IpAddr> = if forwarded.is_empty() {
            x_forwarded_for(headers)
        } else {
            forwarded.iter().filter_map(|element| element.by).collect()
        };
        if let Some(ip) = self.client_of(&hops) {
            info.ip = ip;
            info.forwarded = true;
        } else if let Some(ip) = header_text(headers, "x-real-ip").and_then(|v| parse_ip(&v)) {
            info.ip = canonical(ip);
            info.forwarded = true;
        }
        let proto = forwarded
            .first()
            .and_then(|element| element.proto.clone())
            .or_else(|| header_text(headers, "x-forwarded-proto"))
            .and_then(|value| value.split(',').next().map(|s| s.trim().to_ascii_lowercase()));
        match proto.as_deref() {
            Some("https") => {
                info.scheme = Scheme::Https;
                info.forwarded = true;
            }
            Some("http") => {
                info.scheme = Scheme::Http;
                info.forwarded = true;
            }
            _ => {}
        }
        let host = forwarded
            .first()
            .and_then(|element| element.host.clone())
            .or_else(|| header_text(headers, "x-forwarded-host"))
            .and_then(|value| value.split(',').next().map(|s| s.trim().to_string()))
            .filter(|host| !host.is_empty());
        if let Some(host) = host {
            info.host = Some(host);
            info.forwarded = true;
        }
        info
    }

    /// The address the request began at: walking the hops from the nearest proxy back,
    /// the first that is not a trusted proxy. When every hop is a proxy the outermost one
    /// is what there is.
    fn client_of(&self, hops: &[IpAddr]) -> Option<IpAddr> {
        hops.iter()
            .rev()
            .map(|ip| canonical(*ip))
            .find(|ip| !self.is_trusted(*ip))
            .or_else(|| hops.first().map(|ip| canonical(*ip)))
    }
}

/// IPv4 addresses mapped into IPv6, as dual-stack sockets report them, as IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

fn header_text<K: header::AsHeaderName>(headers: &HeaderMap, name: K) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// An address as proxies write them: bare, bracketed IPv6, or with a port.
fn parse_ip(text: &str) -> Option<IpAddr> {
    let text = text.trim().trim_matches('"');
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Some(ip);
    }
    if let Some(inner) = text.strip_prefix('[') {
        let end = inner.find(']')?;
        return inner[..end].parse().ok();
    }
    text.rsplit_once(':')
        .and_then(|(host, port)| port.parse::<u16>().ok().and(host.parse().ok()))
}

fn x_forwarded_for(headers: &HeaderMap) -> Vec<IpAddr> {
    headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(parse_ip)
        .collect()
}

/// One element of an RFC 7239 `Forwarded` header, as one proxy wrote it.
#[derive(Debug, Default, PartialEq, Eq)]
struct ForwardedElement {
    by: Option<IpAddr>,
    proto: Option<String>,
    host: Option<String>,
}

fn parse_forwarded(headers: &HeaderMap) -> Vec<ForwardedElement> {
    headers
        .get_all(header::FORWARDED)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter(|element| !element.trim().is_empty())
        .map(|element| {
            let mut parsed = ForwardedElement::default();
            for pair in element.split(';') {
                let Some((key, value)) = pair.split_once('=') else {
                    continue;
                };
                let value = value.trim().trim_matches('"');
                match key.trim().to_ascii_lowercase().as_str() {
                    "for" => parsed.by = parse_ip(value),
                    "proto" => parsed.proto = Some(value.to_ascii_lowercase()),
                    "host" => parsed.host = (!value.is_empty()).then(|| value.to_string()),
                    _ => {}
                }
            }
            parsed
        })
        .collect()
}

/// Works out who the request is from and records it for every handler; answers over
/// HTTPS carry `Strict-Transport-Security`.
pub async fn resolve(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    let tls = request.extensions().get::<Tls>().is_some();
    let info = state.proxies.resolve(peer, request.headers(), tls);
    let secure = info.scheme == Scheme::Https;
    request.extensions_mut().insert(info);
    let mut response = next.run(request).await;
    if secure {
        response.headers_mut().insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    response
}

/// The client behind the request.
pub struct Client(pub ClientInfo);

impl<S: Send + Sync> FromRequestParts<S> for Client {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        parts
            .extensions
            .get::<ClientInfo>()
            .cloned()
            .map(Client)
            .ok_or_else(|| ApiError::Internal("client address unavailable".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn proxies(nets: &[&str]) -> Proxies {
        Proxies::new(nets.iter().map(|n| n.parse().unwrap()).collect())
    }

    fn peer(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn untrusted_peers_keep_their_own_address() {
        let proxies = proxies(&["10.0.0.0/8"]);
        let info = proxies.resolve(
            peer("203.0.113.9:4000"),
            &headers(&[
                ("host", "clips.example.com"),
                ("x-forwarded-for", "198.51.100.1"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-host", "evil.example"),
            ]),
            false,
        );
        assert_eq!(info.ip, "203.0.113.9".parse::<IpAddr>().unwrap());
        assert_eq!(info.scheme, Scheme::Http);
        assert_eq!(info.host.as_deref(), Some("clips.example.com"));
        assert!(!info.forwarded);
        assert_eq!(info.origin().as_deref(), Some("http://clips.example.com"));
    }

    #[test]
    fn trusted_proxies_are_read_for_the_browser() {
        let proxies = proxies(&["10.0.0.0/8", "127.0.0.1"]);
        let info = proxies.resolve(
            peer("10.1.2.3:4000"),
            &headers(&[
                ("host", "backend:8080"),
                ("x-forwarded-for", "198.51.100.1, 10.9.9.9"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-host", "clips.example.com"),
            ]),
            false,
        );
        assert_eq!(info.ip, "198.51.100.1".parse::<IpAddr>().unwrap());
        assert_eq!(info.scheme, Scheme::Https);
        assert_eq!(info.host.as_deref(), Some("clips.example.com"));
        assert!(info.forwarded);

        // A spoofed leading hop is skipped only as far as the trusted chain reaches.
        let info = proxies.resolve(
            peer("127.0.0.1:1"),
            &headers(&[("x-forwarded-for", "1.1.1.1, 203.0.113.5, 10.0.0.2")]),
            true,
        );
        assert_eq!(info.ip, "203.0.113.5".parse::<IpAddr>().unwrap());
        assert_eq!(info.scheme, Scheme::Https);

        // Only proxies listed: the outermost is used.
        let info = proxies.resolve(
            peer("127.0.0.1:1"),
            &headers(&[("x-forwarded-for", "10.0.0.7, 10.0.0.2")]),
            false,
        );
        assert_eq!(info.ip, "10.0.0.7".parse::<IpAddr>().unwrap());

        // Without X-Forwarded-For, X-Real-IP is what there is.
        let info = proxies.resolve(
            peer("127.0.0.1:1"),
            &headers(&[("x-real-ip", "[2001:db8::1]:5000")]),
            false,
        );
        assert_eq!(info.ip, "2001:db8::1".parse::<IpAddr>().unwrap());
        assert!(info.forwarded);
    }

    #[test]
    fn the_forwarded_header_wins_over_the_x_headers() {
        let proxies = proxies(&["192.0.2.0/24"]);
        let info = proxies.resolve(
            peer("192.0.2.10:9"),
            &headers(&[
                ("host", "internal"),
                (
                    "forwarded",
                    "for=\"[2001:db8::42]:1234\";proto=https;host=clips.example.com, for=192.0.2.11",
                ),
                ("x-forwarded-for", "198.51.100.1"),
            ]),
            false,
        );
        assert_eq!(info.ip, "2001:db8::42".parse::<IpAddr>().unwrap());
        assert_eq!(info.scheme, Scheme::Https);
        assert_eq!(info.host.as_deref(), Some("clips.example.com"));
    }

    #[test]
    fn tls_on_the_wire_is_https_and_mapped_addresses_are_ipv4() {
        let proxies = Proxies::default();
        let info = proxies.resolve(peer("[::ffff:203.0.113.9]:4000"), &headers(&[]), true);
        assert_eq!(info.ip, "203.0.113.9".parse::<IpAddr>().unwrap());
        assert_eq!(info.scheme, Scheme::Https);
        assert!(info.host.is_none());
        assert!(!proxies.is_trusted("10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn networks_are_addresses_or_cidr_blocks() {
        let one: Network = "127.0.0.1".parse().unwrap();
        assert!(one.contains("127.0.0.1".parse().unwrap()));
        assert!(!one.contains("127.0.0.2".parse().unwrap()));
        assert_eq!(one.to_string(), "127.0.0.1");
        let block: Network = " 10.0.0.0/8 ".parse().unwrap();
        assert!(block.contains("10.200.1.1".parse().unwrap()));
        assert_eq!(block.to_string(), "10.0.0.0/8");
        let six: Network = "fd00::/8".parse().unwrap();
        assert!(six.contains("fd12::1".parse().unwrap()));
        assert!("10.0.0.0/33".parse::<Network>().is_err());
        assert!("proxy".parse::<Network>().is_err());
        let json = serde_json::to_string(&vec![one, block]).unwrap();
        assert_eq!(json, "[\"127.0.0.1\",\"10.0.0.0/8\"]");
        let back: Vec<Network> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, vec![one, block]);
    }

    #[test]
    fn addresses_in_every_spelling() {
        assert_eq!(parse_ip("203.0.113.9"), Some("203.0.113.9".parse().unwrap()));
        assert_eq!(parse_ip("203.0.113.9:80"), Some("203.0.113.9".parse().unwrap()));
        assert_eq!(parse_ip("\"[::1]\""), Some("::1".parse().unwrap()));
        assert_eq!(parse_ip("[::1]:443"), Some("::1".parse().unwrap()));
        assert_eq!(parse_ip("_hidden"), None);
        assert_eq!(parse_ip("unknown"), None);
    }
}
