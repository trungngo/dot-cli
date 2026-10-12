//! HTTP GET for directives that download things (`github_release`). Directives only see the
//! [`HttpClient`] trait, so tests hand them an in-memory client or point [`UreqClient`] at a
//! local fixture server; nothing in the test suite reaches the network.
//!
//! The real client is `ureq`: rustls with bundled roots on Unix, the system TLS stack on Windows
//! (see `Cargo.toml`). Proxies come from the usual environment variables. Redirects are followed
//! here, not by ureq (which would follow `https` to plain `http`): a redirect from `https` to any
//! other scheme is refused, so a request that starts on https stays on https, and
//! `Authorization` is never forwarded to another origin (GitHub release downloads redirect to a
//! CDN).

use std::fmt;
use std::io::Read;
use std::time::Duration;

use anyhow::{Context as _, Result};

/// A GET request. Header values may hold a token: `Debug` masks `Authorization` (and GitLab's
/// `PRIVATE-TOKEN`).
#[derive(Clone, Default)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

impl Request {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: Vec::new(),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The value of header `name` (case-insensitive).
    pub fn get_header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                let v = if k.eq_ignore_ascii_case("authorization")
                    || k.eq_ignore_ascii_case("private-token")
                {
                    dot_core::redact::MASK
                } else {
                    v.as_str()
                };
                (k.as_str(), v)
            })
            .collect();
        f.debug_struct("Request")
            .field("url", &self.url)
            .field("headers", &headers)
            .finish()
    }
}

/// A response of any status; the caller decides what a non-2xx status means.
pub struct Response {
    pub status: u16,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub content_length: Option<u64>,
    pub body: Box<dyn Read + Send>,
}

impl Response {
    /// The value of header `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as text, at most `limit` bytes (more is an error).
    pub fn text(self, limit: u64) -> Result<String> {
        let mut buf = Vec::new();
        self.body
            .take(limit + 1)
            .read_to_end(&mut buf)
            .context("reading response")?;
        if buf.len() as u64 > limit {
            anyhow::bail!("response is larger than {limit} bytes");
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("content_length", &self.content_length)
            .finish_non_exhaustive()
    }
}

/// Performs GET requests. `Send + Sync`: one client serves the whole run.
pub trait HttpClient: Send + Sync {
    /// Sends `req`, following redirects (never from `https` to another scheme). `Err` only for
    /// transport failures (DNS, TLS, connection) and refused redirects; any HTTP status comes
    /// back as a [`Response`].
    fn get(&self, req: &Request) -> Result<Response>;
}

/// The environment variable that points URLs elsewhere for every request `dot` makes (test
/// fixtures, mirrors): comma-separated `from=to` prefix pairs, e.g.
/// `https://go.dev/=http://127.0.0.1:8765/go/,https://github.com/=http://127.0.0.1:8765/gh/`.
/// The first matching prefix is replaced. Plans, records and messages keep the configured URL.
pub const URL_REWRITE_ENV: &str = "DOT_URL_REWRITE";

/// Parses [`URL_REWRITE_ENV`]'s value; malformed pairs are left out.
pub fn parse_rewrites(v: &str) -> Vec<(String, String)> {
    v.split(',')
        .filter_map(|pair| {
            let (from, to) = pair.trim().split_once('=')?;
            (from.contains("://") && to.contains("://"))
                .then(|| (from.trim().to_string(), to.trim().to_string()))
        })
        .collect()
}

/// `url` with the first matching `from` prefix of `rewrites` replaced by its `to`.
pub fn rewrite(rewrites: &[(String, String)], url: &str) -> String {
    for (from, to) in rewrites {
        if let Some(rest) = url.strip_prefix(from.as_str()) {
            return format!("{to}{rest}");
        }
    }
    url.to_string()
}

/// The real client.
pub struct UreqClient {
    agent: ureq::Agent,
    /// [`URL_REWRITE_ENV`]: `(from prefix, to prefix)`.
    rewrites: Vec<(String, String)>,
}

impl fmt::Debug for UreqClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UreqClient")
    }
}

impl Default for UreqClient {
    fn default() -> Self {
        Self::new()
    }
}

impl UreqClient {
    /// A client using the proxy from the environment (`HTTPS_PROXY`, `ALL_PROXY`, ...).
    pub fn new() -> Self {
        Self::build(ureq::Proxy::try_from_env())
    }

    /// A client that never uses a proxy (tests against a local server).
    pub fn direct() -> Self {
        Self::build(None)
    }

    fn build(proxy: Option<ureq::Proxy>) -> Self {
        // ureq defaults to rustls, which Windows does not compile in (see `Cargo.toml`): the
        // provider has to be named, or every https request panics.
        let provider = if cfg!(windows) {
            ureq::tls::TlsProvider::NativeTls
        } else {
            ureq::tls::TlsProvider::Rustls
        };
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .tls_config(ureq::tls::TlsConfig::builder().provider(provider).build())
            .proxy(proxy)
            .user_agent(concat!("dot/", env!("CARGO_PKG_VERSION")))
            // Followed by `get`, which refuses a downgrade from https.
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            rewrites: std::env::var(URL_REWRITE_ENV)
                .map(|v| parse_rewrites(&v))
                .unwrap_or_default(),
        }
    }

    /// The same client sending requests for `from` prefixes to `to` (see [`URL_REWRITE_ENV`]).
    pub fn with_rewrites(mut self, rewrites: Vec<(String, String)>) -> Self {
        self.rewrites = rewrites;
        self
    }
}

/// Redirects followed before giving up.
const MAX_REDIRECTS: usize = 10;

/// Where a redirect from `from` to `location` goes, as an absolute URL; `Err` when it would
/// leave https (`from` is https and the target is not).
pub fn redirect_target(from: &str, location: &str) -> Result<String> {
    let (scheme, rest) = from
        .split_once("://")
        .with_context(|| format!("{from} is not an absolute URL"))?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let origin = format!("{scheme}://{}", &rest[..authority_end]);
    let location = location.trim();
    let target = if location.contains("://") {
        location.to_string()
    } else if let Some(r) = location.strip_prefix("//") {
        format!("{scheme}://{r}")
    } else if location.starts_with('/') {
        format!("{origin}{location}")
    } else {
        // Relative to the current path's directory (without its query).
        let path = rest[authority_end..].split(['?', '#']).next().unwrap_or("");
        let dir = match path.rfind('/') {
            Some(i) => &path[..=i],
            None => "/",
        };
        format!("{origin}{dir}{location}")
    };
    let to_scheme = target
        .split_once("://")
        .map(|(s, _)| s.to_ascii_lowercase())
        .unwrap_or_default();
    if scheme.eq_ignore_ascii_case("https") && to_scheme != "https" {
        anyhow::bail!(
            "GET {from}: refusing the redirect to {target}: it leaves https, and a download in \
             the clear can be swapped in transit"
        );
    }
    Ok(target)
}

/// `scheme://host[:port]`, lower-cased.
fn origin_of(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    format!("{scheme}://{}", &rest[..end]).to_ascii_lowercase()
}

impl HttpClient for UreqClient {
    fn get(&self, req: &Request) -> Result<Response> {
        let mut url = rewrite(&self.rewrites, &req.url);
        let mut headers = req.headers.clone();
        for _ in 0..=MAX_REDIRECTS {
            let mut r = self.agent.get(&url);
            for (k, v) in &headers {
                r = r.header(k, v);
            }
            let resp = r.call().with_context(|| format!("GET {url}"))?;
            let status = resp.status().as_u16();
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            match location {
                Some(loc) if matches!(status, 301 | 302 | 303 | 307 | 308) => {
                    let next = redirect_target(&url, &loc)?;
                    if origin_of(&next) != origin_of(&url) {
                        headers.retain(|(k, _)| {
                            !k.eq_ignore_ascii_case("authorization")
                                && !k.eq_ignore_ascii_case("private-token")
                        });
                    }
                    url = rewrite(&self.rewrites, &next);
                }
                _ => return Ok(response(resp)),
            }
        }
        anyhow::bail!("GET {}: more than {MAX_REDIRECTS} redirects", req.url)
    }
}

fn response(resp: ureq::http::Response<ureq::Body>) -> Response {
    {
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_ascii_lowercase(), v.to_str().ok()?.into())))
            .collect();
        let body = resp.into_body();
        let content_length = body.content_length();
        Response {
            status,
            headers,
            content_length,
            body: Box::new(body.into_reader()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A one-thread HTTP/1.1 server on 127.0.0.1 answering from `route(path)` with
    /// `(status, extra headers, body)`, and recording each request's head.
    struct Fixture {
        base: String,
        seen: Arc<Mutex<Vec<String>>>,
    }

    type Route = fn(&str) -> (u16, Vec<(&'static str, String)>, Vec<u8>);

    impl Fixture {
        fn start(requests: usize, route: Route) -> Fixture {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let log = seen.clone();
            let base2 = base.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().take(requests) {
                    let mut stream = stream.unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut head = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                            break;
                        }
                        head.push_str(&line);
                    }
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    log.lock().unwrap().push(head);
                    let (status, headers, body) = route(&path);
                    let mut out = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (k, v) in headers {
                        // `OTHER`: the same server under another host name.
                        let other = base2.replace("127.0.0.1", "localhost");
                        let v = v.replace("OTHER", &other).replace("BASE", &base2);
                        out.push_str(&format!("{k}: {v}\r\n"));
                    }
                    out.push_str("\r\n");
                    stream.write_all(out.as_bytes()).unwrap();
                    stream.write_all(&body).unwrap();
                }
            });
            Fixture { base, seen }
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[test]
    fn gets_status_headers_and_body() {
        let f = Fixture::start(2, |path| match path {
            "/ok" => (200, vec![("X-Thing", "yes".into())], b"hello".to_vec()),
            _ => (404, vec![], b"{\"message\":\"Not Found\"}".to_vec()),
        });
        let c = UreqClient::direct();
        let r = c
            .get(
                &Request::new(format!("{}/ok", f.base))
                    .header("Authorization", "Bearer s3cret")
                    .header("Accept", "application/json"),
            )
            .unwrap();
        assert_eq!(r.status, 200);
        assert!(r.is_success());
        assert_eq!(r.header("x-thing"), Some("yes"));
        assert_eq!(r.content_length, Some(5));
        assert_eq!(r.text(100).unwrap(), "hello");
        let head = &f.seen()[0];
        assert!(head.contains("GET /ok HTTP/1.1"), "{head}");
        assert!(head.to_lowercase().contains("authorization: bearer s3cret"));
        assert!(head.to_lowercase().contains("user-agent: dot/"), "{head}");

        let r = c.get(&Request::new(format!("{}/missing", f.base))).unwrap();
        assert_eq!(r.status, 404);
        assert!(!r.is_success());
        assert!(r.text(100).unwrap().contains("Not Found"));
    }

    #[test]
    fn rewrites_send_a_configured_url_to_a_fixture() {
        let f = Fixture::start(1, |path| match path {
            "/go/dl/go1.99.linux-amd64.tar.gz" => (200, vec![], b"tarball".to_vec()),
            _ => (404, vec![], Vec::new()),
        });
        let rules = parse_rewrites(&format!(
            "https://go.dev/={}/go/, junk, https://github.com/=http://nowhere.invalid/",
            f.base
        ));
        assert_eq!(rules.len(), 2);
        let c = UreqClient::direct().with_rewrites(rules);
        let r = c
            .get(&Request::new("https://go.dev/dl/go1.99.linux-amd64.tar.gz"))
            .unwrap();
        assert_eq!(r.text(100).unwrap(), "tarball");
        assert_eq!(rewrite(&[], "https://x/"), "https://x/");
    }

    #[test]
    fn follows_redirects_without_the_token_across_hosts() {
        // 127.0.0.1 and localhost are different hosts to the client.
        let f = Fixture::start(3, |path| match path {
            "/asset" => (302, vec![("Location", "/moved".into())], Vec::new()),
            "/moved" => (307, vec![("Location", "OTHER/cdn".into())], Vec::new()),
            _ => (200, vec![], b"payload".to_vec()),
        });
        let r = UreqClient::direct()
            .get(&Request::new(format!("{}/asset", f.base)).header("Authorization", "Bearer t0k"))
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.text(100).unwrap(), "payload");
        let seen = f.seen();
        assert_eq!(seen.len(), 3);
        // Same origin: the token goes along; another host: never.
        assert!(seen[1].contains("GET /moved"), "{seen:?}");
        assert!(seen[1].to_lowercase().contains("t0k"), "{seen:?}");
        assert!(seen[2].contains("GET /cdn"), "{seen:?}");
        assert!(
            seen[2].to_lowercase().contains("host: localhost"),
            "{seen:?}"
        );
        assert!(!seen[2].to_lowercase().contains("t0k"), "{seen:?}");
    }

    #[test]
    fn redirects_never_leave_https() {
        let t = |from: &str, loc: &str| redirect_target(from, loc).map_err(|e| e.to_string());
        assert_eq!(
            t(
                "https://github.com/o/r/releases/x",
                "https://cdn.example.com/f?sig=1"
            )
            .unwrap(),
            "https://cdn.example.com/f?sig=1"
        );
        assert_eq!(
            t("https://h.test/a/b?q=1", "/c").unwrap(),
            "https://h.test/c"
        );
        assert_eq!(
            t("https://h.test/a/b?q=1", "c").unwrap(),
            "https://h.test/a/c"
        );
        assert_eq!(
            t("https://h.test/a", "//cdn.test/x").unwrap(),
            "https://cdn.test/x"
        );
        for bad in ["http://h.test/payload", "HTTP://h.test/x", "ftp://h.test/x"] {
            let e = t("https://h.test/a", bad).unwrap_err();
            assert!(e.contains("leaves https"), "{e}");
        }
        // A request that starts in the clear (a local fixture) may stay there.
        assert_eq!(
            t("http://127.0.0.1:1/a", "http://localhost:1/b").unwrap(),
            "http://localhost:1/b"
        );
        assert_eq!(origin_of("HTTPS://H.test:8/x"), "https://h.test:8");
    }

    #[test]
    fn text_is_limited() {
        let f = Fixture::start(1, |_| (200, vec![], vec![b'x'; 64]));
        let r = UreqClient::direct()
            .get(&Request::new(format!("{}/", f.base)))
            .unwrap();
        assert!(r.text(10).unwrap_err().to_string().contains("larger than"));
    }

    #[test]
    fn transport_errors_name_the_url_and_debug_masks_tokens() {
        // A port nothing listens on.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}/x");
        let e = UreqClient::direct().get(&Request::new(&url)).unwrap_err();
        assert!(format!("{e:#}").contains(&url), "{e:#}");
        let req = Request::new("https://api.github.com").header("Authorization", "Bearer s3cret");
        assert!(!format!("{req:?}").contains("s3cret"));
        assert_eq!(req.get_header("authorization"), Some("Bearer s3cret"));
    }
}
