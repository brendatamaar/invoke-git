//! The one place that sends HTTP, on `reqwest`'s blocking client (every
//! caller is on a background thread).
//!
//! Rules enforced here for every request, so no caller can forget them:
//! - a URL must be https, or http to this machine, before anything is sent
//! - a token goes in an `Authorization` header only, never in a URL
//! - redirects are not followed (a token must not follow one)
//! - answers are capped in size, and errors carry no URL or body

use std::io::Read;
use std::time::Duration;

use invoke_core::forge::Origin;

use super::error::ForgeError;
use super::secrets::Secret;

/// An answer larger than this is refused.
const MAX_BODY: u64 = 8 * 1024 * 1024;

pub struct HttpResponse {
    pub status: u16,
    /// Header names in lower case.
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Shows the status and sizes only: a body can hold a device code, a token or
/// an authorization code, and must not reach a log through `{:?}`.
impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

impl HttpResponse {
    #[cfg(test)]
    pub fn for_test(status: u16, headers: &[(&str, &str)]) -> HttpResponse {
        HttpResponse {
            status,
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub struct Http {
    /// For https: the system proxy settings apply (the proxy only sees an
    /// encrypted tunnel).
    client: reqwest::blocking::Client,
    /// For http, which is only ever this machine: never through a proxy, or
    /// the bearer token would travel to it in clear text.
    local: reqwest::blocking::Client,
}

/// The origin part of `url` (`scheme://authority`), parsed with the same
/// rules as an instance address.
pub fn origin_of_url(url: &str) -> Option<Origin> {
    // No whitespace or control characters anywhere: `Origin::parse` trims, and
    // a padded address must not pass a check that the sender then reads
    // differently.
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Origin::parse(&format!("{scheme}://{}", &rest[..end])).ok()
}

impl Http {
    pub fn new() -> Result<Http, ForgeError> {
        Self::build(None)
    }

    /// `proxy` replaces the system proxy for https (tests aim it at a local
    /// server to prove http never uses one).
    pub(crate) fn build(proxy: Option<&str>) -> Result<Http, ForgeError> {
        let base = || {
            reqwest::blocking::Client::builder()
                .user_agent(concat!("invoke-git/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
        };
        let mut secure = base();
        if let Some(proxy) = proxy {
            secure = secure.proxy(reqwest::Proxy::all(proxy).map_err(|_| ForgeError::Network)?);
        }
        let secure = secure.build().map_err(|_| ForgeError::Network)?;
        let mut local = base();
        if let Some(proxy) = proxy {
            // Only a test sets this: it proves `no_proxy` below wins.
            local = local.proxy(reqwest::Proxy::all(proxy).map_err(|_| ForgeError::Network)?);
        }
        let local = local.no_proxy().build().map_err(|_| ForgeError::Network)?;
        Ok(Http {
            client: secure,
            local,
        })
    }

    /// The client for this address: http (this machine) bypasses proxies.
    fn client_for(&self, url: &str) -> &reqwest::blocking::Client {
        match origin_of_url(url) {
            Some(origin) if origin.is_https() => &self.client,
            // http (this machine) and anything unreadable: never a proxy.
            _ => &self.local,
        }
    }

    pub fn get(
        &self,
        url: &str,
        bearer: Option<&Secret>,
        accept: &str,
        extra: &[(&str, &str)],
    ) -> Result<HttpResponse, ForgeError> {
        check_url(url)?;
        let mut request = self.client_for(url).get(url).header("Accept", accept);
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        if let Some(token) = bearer {
            request = request.header("Authorization", format!("Bearer {}", token.expose()));
        }
        finish(request)
    }

    /// A form POST. `body` is already encoded (see `codec::form_body`).
    pub fn post_form(
        &self,
        url: &str,
        body: String,
        bearer: Option<&Secret>,
    ) -> Result<HttpResponse, ForgeError> {
        check_url(url)?;
        let mut request = self
            .client_for(url)
            .post(url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body);
        if let Some(token) = bearer {
            request = request.header("Authorization", format!("Bearer {}", token.expose()));
        }
        finish(request)
    }
}

fn check_url(url: &str) -> Result<(), ForgeError> {
    match origin_of_url(url) {
        Some(origin) if origin.is_secure() => Ok(()),
        _ => Err(ForgeError::InsecureUrl),
    }
}

fn finish(request: reqwest::blocking::RequestBuilder) -> Result<HttpResponse, ForgeError> {
    let response = request.send().map_err(transport_error)?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            Some((
                name.as_str().to_ascii_lowercase(),
                value.to_str().ok()?.to_owned(),
            ))
        })
        .collect();
    let mut body = Vec::new();
    response
        .take(MAX_BODY + 1)
        .read_to_end(&mut body)
        .map_err(|_| ForgeError::Network)?;
    if body.len() as u64 > MAX_BODY {
        return Err(ForgeError::BadResponse("answer too large"));
    }
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// Classifies a transport failure without keeping its text: reqwest's own
/// message includes the request URL.
fn transport_error(error: reqwest::Error) -> ForgeError {
    if error.is_timeout() {
        ForgeError::TimedOut
    } else {
        ForgeError::Network
    }
}

/// The error an API answer's status means, or `None` for a success.
pub fn status_error(response: &HttpResponse) -> Option<ForgeError> {
    let retry_after = || {
        response
            .header("retry-after")
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    match response.status {
        200..=299 => None,
        401 => Some(ForgeError::Unauthorized),
        403 | 429 => {
            let limited = response.status == 429
                || response.header("x-ratelimit-remaining") == Some("0")
                || response.header("retry-after").is_some();
            Some(if limited {
                ForgeError::RateLimited {
                    retry_after_secs: retry_after(),
                }
            } else {
                ForgeError::Forbidden
            })
        }
        404 => Some(ForgeError::NotFound),
        500..=599 => Some(ForgeError::Server(response.status)),
        other => Some(ForgeError::Status(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, headers: &[(&str, &str)]) -> HttpResponse {
        HttpResponse {
            status,
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
        }
    }

    #[test]
    fn statuses_map_to_the_errors_the_app_reacts_to() {
        assert_eq!(status_error(&response(200, &[])), None);
        assert_eq!(status_error(&response(204, &[])), None);
        assert_eq!(
            status_error(&response(401, &[])),
            Some(ForgeError::Unauthorized)
        );
        assert_eq!(
            status_error(&response(403, &[])),
            Some(ForgeError::Forbidden)
        );
        assert_eq!(
            status_error(&response(403, &[("x-ratelimit-remaining", "0")])),
            Some(ForgeError::RateLimited {
                retry_after_secs: None
            })
        );
        assert_eq!(
            status_error(&response(429, &[("retry-after", "30")])),
            Some(ForgeError::RateLimited {
                retry_after_secs: Some(30)
            })
        );
        assert_eq!(
            status_error(&response(404, &[])),
            Some(ForgeError::NotFound)
        );
        assert_eq!(
            status_error(&response(502, &[])),
            Some(ForgeError::Server(502))
        );
        assert_eq!(
            status_error(&response(302, &[])),
            Some(ForgeError::Status(302))
        );
    }

    /// Answers one request on a local port; the flag says if one came.
    fn one_shot(
        body: &'static str,
    ) -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(AtomicBool::new(false));
        let flag = seen.clone();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                flag.store(true, Ordering::SeqCst);
                let mut buffer = [0u8; 2048];
                let _ = stream.read(&mut buffer);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (addr, seen)
    }

    #[test]
    fn http_to_this_machine_never_goes_through_a_proxy() {
        use std::sync::atomic::Ordering;
        let (proxy, proxy_seen) = one_shot("from the proxy");
        let (target, target_seen) = one_shot("from the target");
        // Every https request would use this proxy; http must not.
        let http = Http::build(Some(&format!("http://{proxy}"))).unwrap();
        let token = Secret::new("tok-for-the-target").unwrap();
        let response = http
            .get(
                &format!("http://{target}/x"),
                Some(&token),
                "application/json",
                &[],
            )
            .unwrap();
        assert_eq!(response.text(), "from the target");
        assert!(target_seen.load(Ordering::SeqCst));
        assert!(
            !proxy_seen.load(Ordering::SeqCst),
            "the bearer token went to the proxy in clear text"
        );
    }

    #[test]
    fn a_response_never_prints_its_body() {
        let mut response = HttpResponse::for_test(200, &[("set-cookie", "s=cookie-secret")]);
        response.body = br#"{"access_token":"gho_SECRET","device_code":"dev-secret"}"#.to_vec();
        let shown = format!("{response:?}");
        for secret in ["gho_SECRET", "dev-secret", "cookie-secret", "access_token"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        assert!(shown.contains("200"));
    }
    #[test]
    fn the_client_follows_the_parsed_address_not_its_spelling() {
        let http = Http::build(None).unwrap();
        let local = &http.local as *const _;
        let secure = &http.client as *const _;
        // Anything that is not plainly https goes to the client that never
        // uses a proxy, however the address is padded or capitalised.
        for url in [
            "http://127.0.0.1:9/x",
            "HTTP://127.0.0.1:9/x",
            " http://127.0.0.1:9/x",
            "http://127.0.0.1:9/x ",
            "\thttp://127.0.0.1:9/x",
            "http://127.0.0.1:9/x\n",
            "",
            "not a url",
        ] {
            assert!(std::ptr::eq(http.client_for(url), local), "{url:?}");
        }
        for url in ["https://api.github.com/user", "HTTPS://api.github.com/user"] {
            assert!(std::ptr::eq(http.client_for(url), secure), "{url:?}");
        }
        // And a padded address is not sent at all.
        for url in [
            " http://127.0.0.1:9/x",
            "http://127.0.0.1:9/x ",
            " https://api.github.com/user",
            "https://api.github.com/user\n",
        ] {
            assert_eq!(check_url(url), Err(ForgeError::InsecureUrl), "{url:?}");
        }
    }

    #[test]
    fn only_secure_or_local_urls_are_sent() {
        for ok in [
            "https://api.github.com/user",
            "http://127.0.0.1:8080/x",
            "http://localhost/x?y=1",
        ] {
            assert!(check_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://api.github.com/user",
            "ftp://x/y",
            "https://user:pw@host/x",
            "//host/x",
            "http://127.0.0.1.evil.example/x",
            "",
        ] {
            assert_eq!(check_url(bad), Err(ForgeError::InsecureUrl), "{bad}");
        }
    }
}
