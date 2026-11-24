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

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    /// Header names in lower case.
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
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
    client: reqwest::blocking::Client,
}

/// The origin part of `url` (`scheme://authority`), parsed with the same
/// rules as an instance address.
pub fn origin_of_url(url: &str) -> Option<Origin> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Origin::parse(&format!("{scheme}://{}", &rest[..end])).ok()
}

impl Http {
    pub fn new() -> Result<Http, ForgeError> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("invoke-git/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ForgeError::Network)?;
        Ok(Http { client })
    }

    pub fn get(
        &self,
        url: &str,
        bearer: Option<&Secret>,
        accept: &str,
        extra: &[(&str, &str)],
    ) -> Result<HttpResponse, ForgeError> {
        check_url(url)?;
        let mut request = self.client.get(url).header("Accept", accept);
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
            .client
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
