//! REST client for the mihomo (Clash.Meta) external controller.
//!
//! Only the endpoints a TUI actually needs: version, configs, proxies,
//! connections, delay tests, and the traffic/log streams.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client as HttpClient, Method, Response, Url};
use serde::Deserialize;
use serde_json::json;

pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:9090";
pub const DEFAULT_DELAY_URL: &str = "http://www.gstatic.com/generate_204";
pub const DEFAULT_TIMEOUT_MS: u64 = 5000;
pub const DEFAULT_LOG_LEVEL: &str = "info";
/// The three routing modes mihomo accepts in `PATCH /configs`.
pub const MODES: [&str; 3] = ["rule", "global", "direct"];

#[derive(Clone)]
pub struct Client {
    base: Url,
    secret: Option<String>,
    /// For short-lived calls: a total timeout is what we want.
    http: HttpClient,
    /// For `/traffic` and `/logs`: these never end, so no total timeout.
    stream: HttpClient,
}

impl Client {
    pub fn new(endpoint: &str, secret: Option<String>) -> Result<Self> {
        // Be forgiving about `--api 127.0.0.1:9090` with no scheme.
        let endpoint = if endpoint.contains("://") {
            endpoint.to_string()
        } else {
            format!("http://{endpoint}")
        };
        let base =
            Url::parse(&endpoint).with_context(|| format!("invalid endpoint `{endpoint}`"))?;

        let http = HttpClient::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("building HTTP client")?;
        let stream = HttpClient::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .context("building streaming HTTP client")?;

        Ok(Self {
            base,
            secret: secret.filter(|s| !s.is_empty()),
            http,
            stream,
        })
    }

    pub fn endpoint(&self) -> &str {
        self.base.as_str()
    }

    /// The controller secret, for rendering a runtime config whose
    /// `external-controller` + `secret` match the instance we are talking to.
    pub fn secret(&self) -> Option<&str> {
        self.secret.as_deref()
    }

    /// Build `<base>/<segments...>`, percent-encoding each segment so proxy
    /// names with spaces, slashes or emoji survive the trip.
    fn url(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|()| anyhow!("endpoint URL cannot be used as a base"))?;
            path.pop_if_empty();
            for segment in segments {
                path.push(segment);
            }
        }
        Ok(url)
    }

    fn request(&self, method: Method, url: Url) -> reqwest::RequestBuilder {
        let mut req = self.http.request(method, url);
        if let Some(secret) = &self.secret {
            req = req.bearer_auth(secret);
        }
        req
    }

    fn stream_request(&self, url: Url) -> reqwest::RequestBuilder {
        let mut req = self.stream.get(url);
        if let Some(secret) = &self.secret {
            req = req.bearer_auth(secret);
        }
        req
    }

    async fn send(req: reqwest::RequestBuilder) -> Result<Response> {
        let resp = req.send().await.context("talking to mihomo")?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        let body = body.trim();
        if body.is_empty() {
            bail!("mihomo returned {status}");
        }
        bail!("mihomo returned {status}: {body}")
    }

    pub async fn version(&self) -> Result<Version> {
        let resp = Self::send(self.request(Method::GET, self.url(&["version"])?)).await?;
        resp.json().await.context("decoding /version")
    }

    pub async fn configs(&self) -> Result<Configs> {
        let resp = Self::send(self.request(Method::GET, self.url(&["configs"])?)).await?;
        resp.json().await.context("decoding /configs")
    }

    pub async fn set_mode(&self, mode: &str) -> Result<()> {
        let url = self.url(&["configs"])?;
        Self::send(
            self.request(Method::PATCH, url)
                .json(&json!({ "mode": mode })),
        )
        .await?;
        Ok(())
    }

    /// Hot-reload the controller with a full config, supplied inline.
    ///
    /// `PUT /configs` with `{"payload": <yaml>}` asks a running mihomo to apply
    /// the YAML without a process restart; the external controller (and so this
    /// client's endpoint) survives the reload.
    // Consumed by the profile worker (task 11); until then only tests call it —
    // drop the escape hatch once they do.
    #[allow(dead_code)]
    pub async fn reload_config(&self, yaml_payload: &str) -> Result<()> {
        Self::send(
            self.request(Method::PUT, self.url(&["configs"])?)
                .json(&json!({ "payload": yaml_payload })),
        )
        .await?;
        Ok(())
    }

    /// All selector-ish groups with their members, GLOBAL first.
    pub async fn groups(&self) -> Result<Vec<Group>> {
        let resp = Self::send(self.request(Method::GET, self.url(&["proxies"])?)).await?;
        let envelope: ProxiesEnvelope = resp.json().await.context("decoding /proxies")?;

        let mut groups: Vec<Group> = envelope
            .proxies
            .values()
            .filter(|proxy| proxy.is_group())
            .map(|group| Group {
                name: group.name.clone(),
                kind: group.kind.clone(),
                now: group.now.clone().unwrap_or_default(),
                test_url: group.test_url.clone(),
                members: group
                    .all
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|name| {
                        let proxy = envelope.proxies.get(&name);
                        Member {
                            kind: proxy.map(|p| p.kind.clone()).unwrap_or_default(),
                            delay: proxy.and_then(|p| p.last_delay()),
                            name,
                        }
                    })
                    .collect(),
            })
            .collect();

        // The API hands back an unordered map, so restore a stable, useful order.
        groups.sort_by(|a, b| (a.name != "GLOBAL", &a.name).cmp(&(b.name != "GLOBAL", &b.name)));
        Ok(groups)
    }

    pub async fn select(&self, group: &str, member: &str) -> Result<()> {
        let url = self.url(&["proxies", group])?;
        Self::send(
            self.request(Method::PUT, url)
                .json(&json!({ "name": member })),
        )
        .await?;
        Ok(())
    }

    pub async fn proxy_delay(&self, name: &str, url: &str, timeout_ms: u64) -> Result<u64> {
        let mut endpoint = self.url(&["proxies", name, "delay"])?;
        endpoint
            .query_pairs_mut()
            .append_pair("timeout", &timeout_ms.to_string())
            .append_pair("url", url);

        // The http client's 10s total timeout would truncate long delay tests,
        // so give this one request its own ceiling: the probe itself is capped
        // at `timeout_ms`, plus a grace period for the round trip.
        let resp = Self::send(
            self.request(Method::GET, endpoint)
                .timeout(Duration::from_millis(timeout_ms + 1500)),
        )
        .await?;
        let body: DelayResponse = resp.json().await.context("decoding delay result")?;
        if body.delay == 0 {
            let reason = if body.message.is_empty() {
                "timeout".to_string()
            } else {
                body.message
            };
            bail!("{reason}");
        }
        Ok(body.delay)
    }

    pub async fn connections(&self) -> Result<Connections> {
        let resp = Self::send(self.request(Method::GET, self.url(&["connections"])?)).await?;
        resp.json().await.context("decoding /connections")
    }

    pub async fn close_connection(&self, id: &str) -> Result<()> {
        let url = self.url(&["connections", id])?;
        Self::send(self.request(Method::DELETE, url)).await?;
        Ok(())
    }

    pub async fn close_all_connections(&self) -> Result<()> {
        Self::send(self.request(Method::DELETE, self.url(&["connections"])?)).await?;
        Ok(())
    }

    pub async fn traffic_stream(&self) -> Result<Response> {
        Self::send(self.stream_request(self.url(&["traffic"])?)).await
    }

    pub async fn log_stream(&self, level: &str) -> Result<Response> {
        let mut url = self.url(&["logs"])?;
        url.query_pairs_mut().append_pair("level", level);
        Self::send(self.stream_request(url)).await
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Version {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub meta: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Configs {
    #[serde(default)]
    pub port: u16,
    #[serde(default, rename = "socks-port")]
    pub socks_port: u16,
    #[serde(default, rename = "mixed-port")]
    pub mixed_port: u16,
    #[serde(default, rename = "redir-port")]
    pub redir_port: u16,
    #[serde(default, rename = "tproxy-port")]
    pub tproxy_port: u16,
    #[serde(default)]
    pub mode: String,
    #[serde(default, rename = "log-level")]
    pub log_level: String,
    #[serde(default, rename = "allow-lan")]
    pub allow_lan: bool,
    #[serde(default)]
    pub ipv6: bool,
    #[serde(default)]
    pub tun: Tun,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Tun {
    #[serde(default)]
    pub enable: bool,
}

impl Configs {
    pub fn ports(&self) -> String {
        let mut parts = Vec::new();
        for (label, port) in [
            ("mixed", self.mixed_port),
            ("http", self.port),
            ("socks", self.socks_port),
            ("redir", self.redir_port),
            ("tproxy", self.tproxy_port),
        ] {
            if port > 0 {
                parts.push(format!("{label} {port}"));
            }
        }
        if parts.is_empty() {
            "no inbound".to_string()
        } else {
            parts.join("  ")
        }
    }
}

#[derive(Debug, Deserialize)]
struct ProxiesEnvelope {
    proxies: HashMap<String, Proxy>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Proxy {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub now: Option<String>,
    #[serde(default)]
    pub all: Option<Vec<String>>,
    #[serde(default)]
    pub history: Vec<History>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub test_url: Option<String>,
}

impl Proxy {
    /// `all` is only ever present on groups, and hidden groups are noise here.
    pub fn is_group(&self) -> bool {
        self.all.is_some() && !self.hidden
    }

    /// The most recent measured delay, or `None` if it has never worked.
    pub fn last_delay(&self) -> Option<u64> {
        self.history.last().map(|h| h.delay).filter(|d| *d > 0)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct History {
    #[serde(default)]
    pub delay: u64,
}

/// A group (Selector, URLTest, ...) resolved into the members we render.
#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
    pub kind: String,
    pub now: String,
    pub test_url: Option<String>,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub kind: String,
    pub delay: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DelayResponse {
    #[serde(default)]
    delay: u64,
    #[serde(default)]
    message: String,
}

/// Treat an explicit JSON `null` exactly like a missing field.
///
/// `#[serde(default)]` only covers an absent key, but mihomo sends
/// `"connections": null` whenever nothing is open. Without this the whole
/// `/connections` decode fails and the traffic totals are silently dropped
/// along with the (empty) list.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Connections {
    #[serde(default, rename = "downloadTotal")]
    pub download_total: u64,
    #[serde(default, rename = "uploadTotal")]
    pub upload_total: u64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub connections: Vec<Connection>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Connection {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub metadata: Metadata,
    #[serde(default)]
    pub upload: u64,
    #[serde(default)]
    pub download: u64,
    #[serde(default)]
    pub chains: Vec<String>,
    #[serde(default)]
    pub rule: String,
    #[serde(default, rename = "rulePayload")]
    pub rule_payload: String,
}

impl Connection {
    /// What to put in the "destination" column: host when known, IP otherwise.
    pub fn destination(&self) -> String {
        let host = if self.metadata.host.is_empty() {
            self.metadata.destination_ip.clone()
        } else {
            self.metadata.host.clone()
        };
        if host.is_empty() {
            return "-".to_string();
        }
        if self.metadata.destination_port.is_empty() || self.metadata.destination_port == "0" {
            host
        } else {
            format!("{host}:{}", self.metadata.destination_port)
        }
    }

    /// The proxy chain, most specific first.
    pub fn chain(&self) -> String {
        if self.chains.is_empty() {
            "DIRECT".to_string()
        } else {
            self.chains.join(" → ")
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub network: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default, rename = "sourceIP")]
    pub source_ip: String,
    #[serde(default, rename = "destinationIP")]
    pub destination_ip: String,
    #[serde(default, rename = "sourcePort")]
    pub source_port: String,
    #[serde(default, rename = "destinationPort")]
    pub destination_port: String,
    #[serde(default)]
    pub host: String,
    #[serde(default, rename = "processPath")]
    pub process_path: String,
    #[serde(default)]
    pub process: String,
}

impl Metadata {
    /// Process name, falling back to the full path when that is all we have.
    pub fn process_name(&self) -> String {
        if !self.process.is_empty() {
            return self.process.clone();
        }
        if self.process_path.is_empty() {
            return String::new();
        }
        self.process_path
            .rsplit('/')
            .next()
            .unwrap_or(&self.process_path)
            .to_string()
    }
}

/// One line of the `/traffic` stream, in bytes per second.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Traffic {
    #[serde(default)]
    pub up: u64,
    #[serde(default)]
    pub down: u64,
}

/// One line of the `/logs` stream.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LogLine {
    #[serde(default, rename = "type")]
    pub level: String,
    #[serde(default)]
    pub payload: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn proxy_delay_timeout_is_per_request() {
        let server = MockServer::start().await;

        // A controller handed `timeout=100` may still be slow to give up; this
        // one answers only after 2s, so the client must cut the call at its
        // request-level ceiling (100 + 1500 = 1600ms) instead of riding the
        // 10s total timeout to a success that arrives too late to matter.
        Mock::given(method("GET"))
            .and(path("/proxies/n1/delay"))
            .and(query_param("timeout", "100"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"delay":123}"#)
                    .set_delay(Duration::from_millis(2000)),
            )
            .mount(&server)
            .await;
        // A generous budget must still accommodate a slow but real success.
        Mock::given(method("GET"))
            .and(path("/proxies/n1/delay"))
            .and(query_param("timeout", "5000"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"delay":123}"#)
                    .set_delay(Duration::from_millis(800)),
            )
            .mount(&server)
            .await;

        let client = Client::new(&server.uri(), None).unwrap();

        let started = Instant::now();
        let short = client
            .proxy_delay("n1", "http://example.com/204", 100)
            .await;
        let short_elapsed = started.elapsed();
        assert!(
            short.is_err(),
            "a probe past its per-request ceiling fails, got {short:?}"
        );
        assert!(
            short_elapsed >= Duration::from_millis(1500) && short_elapsed <= Duration::from_secs(9),
            "aborts at the request-level ceiling, not the 10s total: {short_elapsed:?}"
        );

        let started = Instant::now();
        let delay = client
            .proxy_delay("n1", "http://example.com/204", 5000)
            .await
            .unwrap();
        let long_elapsed = started.elapsed();
        assert_eq!(delay, 123);
        assert!(
            long_elapsed <= Duration::from_secs(9),
            "a slow probe succeeds within its larger ceiling: {long_elapsed:?}"
        );
    }

    #[tokio::test]
    async fn reload_config_puts_payload() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/configs"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri(), None).unwrap();
        client
            .reload_config("mixed-port: 7890\nlog-level: info\n")
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "PUT");
        assert_eq!(requests[0].url.path(), "/configs");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let payload = body["payload"].as_str().expect("payload is a string");
        assert!(
            payload.contains("mixed-port"),
            "the yaml rides along as `payload`: {payload}"
        );
    }

    #[tokio::test]
    async fn reload_failure_carries_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/configs"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("yaml: line 3: could not find expected ':'"),
            )
            .mount(&server)
            .await;

        let client = Client::new(&server.uri(), None).unwrap();
        let err = client.reload_config("bad: [").await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("400"), "status is reported: {message}");
        assert!(
            message.contains("could not find expected ':'"),
            "the controller's explanation is reported: {message}"
        );
    }

    #[test]
    fn secret_accessor_exposes_the_controller_secret() {
        let with_secret = Client::new("127.0.0.1:9090", Some("s3cret".into())).unwrap();
        assert_eq!(with_secret.secret(), Some("s3cret"));

        let without_secret = Client::new("127.0.0.1:9090", None).unwrap();
        assert_eq!(without_secret.secret(), None);

        // The constructor drops empty secrets; the accessor agrees.
        let empty = Client::new("127.0.0.1:9090", Some(String::new())).unwrap();
        assert_eq!(empty.secret(), None);
    }

    #[test]
    fn parses_groups_and_hides_hidden_ones() {
        let payload = r#"{
            "proxies": {
                "DIRECT": { "name": "DIRECT", "type": "Direct", "udp": true, "history": [] },
                "node-a": {
                    "name": "node-a", "type": "Shadowsocks", "udp": true, "alive": true,
                    "history": [{ "time": "2024-01-01T00:00:00Z", "delay": 123 }]
                },
                "node-b": { "name": "node-b", "type": "Vmess", "history": [] },
                "auto": {
                    "name": "auto", "type": "URLTest", "now": "node-a",
                    "all": ["node-a", "node-b"],
                    "history": [{ "time": "2024-01-01T00:00:00Z", "delay": 123 }]
                },
                "GLOBAL": {
                    "name": "GLOBAL", "type": "Selector", "now": "DIRECT",
                    "all": ["DIRECT", "node-a", "auto"], "history": []
                },
                "secret-group": {
                    "name": "secret-group", "type": "Selector", "hidden": true,
                    "all": ["node-a"], "history": []
                }
            }
        }"#;

        let envelope: ProxiesEnvelope = serde_json::from_str(payload).unwrap();
        let mut groups: Vec<Group> = envelope
            .proxies
            .values()
            .filter(|p| p.is_group())
            .map(|g| Group {
                name: g.name.clone(),
                kind: g.kind.clone(),
                now: g.now.clone().unwrap_or_default(),
                test_url: g.test_url.clone(),
                members: g
                    .all
                    .clone()
                    .unwrap()
                    .into_iter()
                    .map(|name| Member {
                        delay: envelope.proxies.get(&name).and_then(|p| p.last_delay()),
                        kind: envelope
                            .proxies
                            .get(&name)
                            .map(|p| p.kind.clone())
                            .unwrap_or_default(),
                        name,
                    })
                    .collect(),
            })
            .collect();
        groups.sort_by(|a, b| (a.name != "GLOBAL", &a.name).cmp(&(b.name != "GLOBAL", &b.name)));

        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["GLOBAL", "auto"],
            "hidden groups and plain proxies are excluded"
        );

        let auto = &groups[1];
        assert_eq!(auto.now, "node-a");
        assert_eq!(auto.members.len(), 2);
        assert_eq!(
            auto.members[0].delay,
            Some(123),
            "delay comes from the last history entry"
        );
        assert_eq!(auto.members[1].delay, None, "an empty history has no delay");
    }

    #[test]
    fn zero_delay_is_reported_as_unknown() {
        let proxy: Proxy = serde_json::from_str(
            r#"{ "name": "x", "type": "Direct", "history": [{ "time": "t", "delay": 0 }] }"#,
        )
        .unwrap();
        assert_eq!(proxy.last_delay(), None);
    }

    #[test]
    fn parses_traffic_and_log_lines() {
        let traffic: Traffic = serde_json::from_str(r#"{"up":1024,"down":2048}"#).unwrap();
        assert_eq!((traffic.up, traffic.down), (1024, 2048));

        let log: LogLine =
            serde_json::from_str(r#"{"type":"warning","payload":"[TCP] dial failed"}"#).unwrap();
        assert_eq!(log.level, "warning");
        assert_eq!(log.payload, "[TCP] dial failed");
    }

    #[test]
    fn connection_destination_prefers_host_and_falls_back_to_ip() {
        let with_host: Connection = serde_json::from_str(
            r#"{"id":"1","metadata":{"host":"example.com","destinationIP":"1.2.3.4","destinationPort":"443"}}"#,
        )
        .unwrap();
        assert_eq!(with_host.destination(), "example.com:443");

        let ip_only: Connection = serde_json::from_str(
            r#"{"id":"2","metadata":{"destinationIP":"1.2.3.4","destinationPort":"443"}}"#,
        )
        .unwrap();
        assert_eq!(ip_only.destination(), "1.2.3.4:443");

        let no_port: Connection =
            serde_json::from_str(r#"{"id":"3","metadata":{"host":"example.com"}}"#).unwrap();
        assert_eq!(no_port.destination(), "example.com");

        let empty: Connection = serde_json::from_str(r#"{"id":"4","metadata":{}}"#).unwrap();
        assert_eq!(empty.destination(), "-");
    }

    #[test]
    fn connection_chain_reads_as_a_route() {
        let chained: Connection =
            serde_json::from_str(r#"{"id":"1","chains":["node-a","auto","GLOBAL"]}"#).unwrap();
        assert_eq!(chained.chain(), "node-a → auto → GLOBAL");

        let direct: Connection = serde_json::from_str(r#"{"id":"2"}"#).unwrap();
        assert_eq!(direct.chain(), "DIRECT");
    }

    #[test]
    fn percent_encodes_proxy_names_in_urls() {
        let client = Client::new("127.0.0.1:9090", None).unwrap();
        assert_eq!(client.endpoint(), "http://127.0.0.1:9090/");

        let url = client.url(&["proxies", "🇭🇰 HK 01", "delay"]).unwrap();
        assert_eq!(
            url.path(),
            "/proxies/%F0%9F%87%AD%F0%9F%87%B0%20HK%2001/delay"
        );
    }

    #[test]
    fn a_null_connection_list_decodes_as_empty() {
        // mihomo sends `"connections": null` when nothing is open. Getting this
        // wrong fails the whole decode, which also throws away the totals.
        let idle: Connections = serde_json::from_str(
            r#"{"downloadTotal":21326196,"uploadTotal":1109,"connections":null,"memory":0}"#,
        )
        .expect("explicit null must decode, not fail");

        assert!(idle.connections.is_empty());
        assert_eq!(
            idle.download_total, 21_326_196,
            "totals survive an idle controller"
        );
        assert_eq!(idle.upload_total, 1109);
    }

    #[test]
    fn base_path_is_preserved() {
        let client = Client::new("http://localhost:9090/api/", None).unwrap();
        let url = client.url(&["version"]).unwrap();
        assert_eq!(url.path(), "/api/version");
    }

    #[test]
    fn configs_report_their_inbound_ports() {
        let configs: Configs = serde_json::from_str(
            r#"{"mixed-port":7890,"mode":"rule","log-level":"info","tun":{"enable":true}}"#,
        )
        .unwrap();
        assert_eq!(configs.mode, "rule");
        assert!(configs.tun.enable);
        assert_eq!(configs.ports(), "mixed 7890");

        let empty = Configs::default();
        assert_eq!(empty.ports(), "no inbound");
    }
}
