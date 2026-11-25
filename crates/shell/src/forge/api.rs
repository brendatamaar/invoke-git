//! The GitHub and GitLab API calls this phase needs: sign-in (device flow,
//! PKCE code exchange, refresh), who am I, and the list of repositories.
//!
//! Each function takes the HTTP client and the base address it talks to, so
//! tests point it at a local mock. Parsing is the boundary: everything a
//! server sends is checked here before it becomes a core type. In
//! particular, a clone address must be http(s): git would run a command for
//! `ext::` addresses.

use invoke_core::forge::{
    CreatePrInput, Origin, PrFilter, PrState, PullRequest, RemoteRepo, ReviewStatus, Visibility,
};

use super::codec::{form_body, next_link, pct_encode};
use super::error::{ForgeError, clean_code};
use super::http::{Http, HttpResponse, origin_of_url, status_error};
use super::json::Json;
use super::secrets::{Credential, Secret};

/// What the app asks GitHub for: private repositories and PRs need `repo`.
pub const GITHUB_SCOPE: &str = "repo";
/// What the app asks GitLab for: `api` covers listing projects, listing MRs,
/// and creating MRs (upgraded from `read_api` in P9).
pub const GITLAB_SCOPE: &str = "api";

/// Repositories asked for per page (both forges cap at 100).
const PAGE_SIZE: u32 = 100;
/// Pull/merge requests asked for per page.
const PR_PAGE_SIZE: u32 = 50;

/// Where GitHub's pieces live. `Default` is github.com; tests aim it at a
/// local server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubUrls {
    /// `https://api.github.com`
    pub api: String,
    /// `https://github.com` (the OAuth pages)
    pub login: String,
}

impl Default for GitHubUrls {
    fn default() -> Self {
        Self {
            api: "https://api.github.com".into(),
            login: "https://github.com".into(),
        }
    }
}

/// One page of repositories and where the next one is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoPage {
    pub repos: Vec<RemoteRepo>,
    /// An address on the same server, or `None` at the end.
    pub next: Option<String>,
}

/// One page of pull/merge requests and where the next one is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrPage {
    pub prs: Vec<PullRequest>,
    /// An address on the same server, or `None` at the end.
    pub next: Option<String>,
}

// ---------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------

fn parse_json(response: &HttpResponse) -> Result<Json, ForgeError> {
    Json::parse(&response.text()).map_err(|_| ForgeError::BadResponse("not JSON"))
}

/// An OAuth token endpoint answers `200` with a token, or `4xx` (or, on
/// GitHub, `200`) with `{"error": "..."}`. Returns the JSON when it is a
/// token answer, and the OAuth error otherwise.
fn token_answer(response: &HttpResponse) -> Result<Json, OAuthAnswer> {
    let json = match parse_json(response) {
        Ok(json) => json,
        Err(error) => return Err(OAuthAnswer::Failed(status_error(response).unwrap_or(error))),
    };
    if let Some(code) = json.str_field("error") {
        return Err(OAuthAnswer::Error(clean_code(code)));
    }
    if let Some(error) = status_error(response) {
        return Err(OAuthAnswer::Failed(error));
    }
    Ok(json)
}

enum OAuthAnswer {
    /// The server's OAuth error code, cleaned.
    Error(String),
    Failed(ForgeError),
}

impl OAuthAnswer {
    fn into_error(self) -> ForgeError {
        match self {
            OAuthAnswer::Error(code) => ForgeError::OAuth(code),
            OAuthAnswer::Failed(error) => error,
        }
    }
}

/// A token answer as a credential: the access token, and a refresh token and
/// expiry when the server gave them.
fn credential_from(json: &Json, now: u64) -> Result<Credential, ForgeError> {
    let access = json
        .str_field("access_token")
        .and_then(Secret::new)
        .ok_or(ForgeError::BadResponse("no access token"))?;
    let refresh = json.str_field("refresh_token").and_then(Secret::new);
    let expires_at = json
        .get("expires_in")
        .and_then(Json::as_u64)
        .map(|secs| now.saturating_add(secs));
    Ok(Credential {
        access,
        refresh,
        expires_at,
    })
}

fn same_origin(a: &str, b: &str) -> bool {
    matches!((origin_of_url(a), origin_of_url(b)), (Some(a), Some(b)) if a == b)
}

/// The next page's address, accepted only when it stays on the server the
/// token was sent to (a hostile `Link` must not collect the token).
fn next_page(response: &HttpResponse, base: &str) -> Option<String> {
    let next = next_link(response.header("link")?)?;
    same_origin(&next, base).then_some(next)
}

/// A repository address git may clone and keep as `origin`: http(s) only,
/// and nothing that could carry a credential into the repo's config: no
/// user name or password (`origin_of_url` refuses those), no query and no
/// fragment.
fn clone_url_ok(url: &str) -> bool {
    !url.starts_with('-') && !url.contains(['?', '#']) && origin_of_url(url).is_some()
}

fn api_get(
    http: &Http,
    url: &str,
    token: &Secret,
    accept: &str,
    extra: &[(&str, &str)],
) -> Result<HttpResponse, ForgeError> {
    let response = http.get(url, Some(token), accept, extra)?;
    match status_error(&response) {
        Some(error) => Err(error),
        None => Ok(response),
    }
}

fn api_post_json(
    http: &Http,
    url: &str,
    body: &Json,
    token: &Secret,
    accept: &str,
    extra: &[(&str, &str)],
) -> Result<HttpResponse, ForgeError> {
    let response = http.post_json(url, body.encode(), Some(token), accept, extra)?;
    match status_error(&response) {
        Some(error) => Err(error),
        None => Ok(response),
    }
}

/// A web URL returned by a forge (e.g. a PR page): must be secure and bound to
/// `expected_origin` (so a hostile server cannot point "Open in browser" to an
/// unrelated host).
fn web_url_ok(url: &str, expected_origin: Option<&Origin>) -> bool {
    if url.starts_with('-') {
        return false;
    }
    let Some(origin) = origin_of_url(url) else {
        return false;
    };
    if !origin.is_secure() {
        return false;
    }
    match expected_origin {
        Some(expected) => origin == *expected,
        None => true,
    }
}

// ---------------------------------------------------------------------------
// GitHub
// ---------------------------------------------------------------------------

pub mod github {
    use super::*;

    const GITHUB_HEADERS: [(&str, &str); 1] = [("X-GitHub-Api-Version", "2022-11-28")];
    const ACCEPT: &str = "application/vnd.github+json";

    /// What `POST /login/device/code` returns.
    #[derive(Clone, Debug)]
    pub struct DeviceCode {
        pub device_code: Secret,
        pub user_code: String,
        pub verification_uri: String,
        pub expires_in: u64,
        pub interval: u32,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum DevicePoll {
        Pending,
        SlowDown,
        Granted(Credential),
        Denied,
        Expired,
    }

    pub fn request_device_code(
        http: &Http,
        urls: &GitHubUrls,
        client_id: &str,
    ) -> Result<DeviceCode, ForgeError> {
        let body = form_body(&[("client_id", client_id), ("scope", GITHUB_SCOPE)]);
        let response = http.post_form(&format!("{}/login/device/code", urls.login), body, None)?;
        let json = token_answer(&response).map_err(OAuthAnswer::into_error)?;
        let device_code = json
            .str_field("device_code")
            .and_then(Secret::new)
            .ok_or(ForgeError::BadResponse("no device code"))?;
        let user_code = json
            .str_field("user_code")
            .filter(|code| {
                !code.is_empty()
                    && code.len() <= 32
                    && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            })
            .ok_or(ForgeError::BadResponse("no user code"))?
            .to_owned();
        let verification_uri = json
            .str_field("verification_uri")
            .filter(|uri| origin_of_url(uri).is_some_and(|o| o.is_secure()))
            .ok_or(ForgeError::BadResponse("no verification address"))?
            .to_owned();
        Ok(DeviceCode {
            device_code,
            user_code,
            verification_uri,
            expires_in: json.get("expires_in").and_then(Json::as_u64).unwrap_or(900),
            interval: json
                .get("interval")
                .and_then(Json::as_u64)
                .map_or(5, |secs| secs.min(300) as u32),
        })
    }

    pub fn poll_device_code(
        http: &Http,
        urls: &GitHubUrls,
        client_id: &str,
        device_code: &Secret,
        now: u64,
    ) -> Result<DevicePoll, ForgeError> {
        let body = form_body(&[
            ("client_id", client_id),
            ("device_code", device_code.expose()),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ]);
        let response = http.post_form(
            &format!("{}/login/oauth/access_token", urls.login),
            body,
            None,
        )?;
        match token_answer(&response) {
            Ok(json) => Ok(DevicePoll::Granted(credential_from(&json, now)?)),
            Err(OAuthAnswer::Error(code)) => match code.as_str() {
                "authorization_pending" => Ok(DevicePoll::Pending),
                "slow_down" => Ok(DevicePoll::SlowDown),
                "access_denied" => Ok(DevicePoll::Denied),
                "expired_token" => Ok(DevicePoll::Expired),
                _ => Err(ForgeError::OAuth(code)),
            },
            Err(other) => Err(other.into_error()),
        }
    }

    /// The login name of the token's owner.
    pub fn current_user(http: &Http, api: &str, token: &Secret) -> Result<String, ForgeError> {
        let response = api_get(http, &format!("{api}/user"), token, ACCEPT, &GITHUB_HEADERS)?;
        parse_json(&response)?
            .str_field("login")
            .map(str::to_owned)
            .ok_or(ForgeError::BadResponse("no login"))
    }

    pub fn list_repos_page(
        http: &Http,
        api: &str,
        token: &Secret,
        cursor: Option<&str>,
    ) -> Result<RepoPage, ForgeError> {
        let url = match cursor {
            Some(cursor) if same_origin(cursor, api) => cursor.to_owned(),
            Some(_) => return Err(ForgeError::InsecureUrl),
            None => format!(
                "{api}/user/repos?per_page={PAGE_SIZE}&sort=pushed&affiliation={}",
                pct_encode("owner,collaborator,organization_member")
            ),
        };
        let response = api_get(http, &url, token, ACCEPT, &GITHUB_HEADERS)?;
        let json = parse_json(&response)?;
        let items = json
            .as_array()
            .ok_or(ForgeError::BadResponse("not a list"))?;
        let repos = items.iter().filter_map(parse_repo).collect();
        Ok(RepoPage {
            repos,
            next: next_page(&response, api),
        })
    }

    pub(super) fn parse_repo(item: &Json) -> Option<RemoteRepo> {
        let full_name = item.str_field("full_name")?.to_owned();
        let clone_url = item.str_field("clone_url")?.to_owned();
        if full_name.is_empty() || !clone_url_ok(&clone_url) {
            return None;
        }
        let visibility = match item.str_field("visibility") {
            Some("public") => Visibility::Public,
            Some("internal") => Visibility::Internal,
            Some(_) => Visibility::Private,
            None if item.get("private").and_then(Json::as_bool) == Some(false) => {
                Visibility::Public
            }
            None => Visibility::Private,
        };
        Some(RemoteRepo {
            full_name,
            clone_url,
            description: item.str_field("description").unwrap_or_default().to_owned(),
            visibility,
            default_branch: item.str_field("default_branch").map(str::to_owned),
            updated_at: item
                .str_field("pushed_at")
                .or_else(|| item.str_field("updated_at"))
                .map(str::to_owned),
        })
    }

    /// The expected web origin for GitHub PR links (`https://github.com`, or
    /// unrestricted to loopback when `api` is a local mock server).
    fn expected_web_origin(api: &str) -> Option<Origin> {
        let github_web = Origin::parse("https://github.com").ok()?;
        if let Some(api_origin) = origin_of_url(api)
            && !api_origin.is_https()
        {
            return None;
        }
        Some(github_web)
    }

    pub fn list_prs_page(
        http: &Http,
        api: &str,
        token: &Secret,
        owner: &str,
        repo: &str,
        filter: PrFilter,
        cursor: Option<&str>,
    ) -> Result<PrPage, ForgeError> {
        let url = match cursor {
            Some(cursor) if same_origin(cursor, api) => cursor.to_owned(),
            Some(_) => return Err(ForgeError::InsecureUrl),
            None => format!(
                "{api}/repos/{}/{}/pulls?state={}&sort=updated&direction=desc&per_page={PR_PAGE_SIZE}",
                pct_encode(owner),
                pct_encode(repo),
                filter.github_param()
            ),
        };
        let response = api_get(http, &url, token, ACCEPT, &GITHUB_HEADERS)?;
        let json = parse_json(&response)?;
        let items = json
            .as_array()
            .ok_or(ForgeError::BadResponse("not a list"))?;
        let web_origin = expected_web_origin(api);
        Ok(PrPage {
            prs: items
                .iter()
                .filter_map(|item| parse_pr(item, web_origin.as_ref()))
                .collect(),
            next: next_page(&response, api),
        })
    }

    pub fn get_pr(
        http: &Http,
        api: &str,
        token: &Secret,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest, ForgeError> {
        let url = format!(
            "{api}/repos/{}/{}/pulls/{number}",
            pct_encode(owner),
            pct_encode(repo)
        );
        let response = api_get(http, &url, token, ACCEPT, &GITHUB_HEADERS)?;
        let json = parse_json(&response)?;
        let web_origin = expected_web_origin(api);
        parse_pr(&json, web_origin.as_ref()).ok_or(ForgeError::BadResponse("invalid pull request"))
    }

    /// Aggregates GitHub PR reviews across all pages (up to 10 pages) by taking
    /// each reviewer's latest decisive state (`CHANGES_REQUESTED` > `APPROVED` >
    /// `COMMENTED` > `PENDING`).
    pub fn list_pr_reviews(
        http: &Http,
        api: &str,
        token: &Secret,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<ReviewStatus, ForgeError> {
        let mut all_items = Vec::new();
        let mut next_url = Some(format!(
            "{api}/repos/{}/{}/pulls/{number}/reviews?per_page=100",
            pct_encode(owner),
            pct_encode(repo)
        ));
        for _ in 0..10 {
            let Some(url) = next_url.take() else {
                break;
            };
            let response = api_get(http, &url, token, ACCEPT, &GITHUB_HEADERS)?;
            let json = parse_json(&response)?;
            let items = json
                .as_array()
                .ok_or(ForgeError::BadResponse("not a list"))?;
            all_items.extend(items.iter().cloned());
            next_url = next_page(&response, api);
        }
        Ok(summarize_github_reviews(&all_items))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_pr(
        http: &Http,
        api: &str,
        token: &Secret,
        owner: &str,
        repo: &str,
        head: &str,
        head_repo: Option<&str>,
        input: &CreatePrInput,
    ) -> Result<PullRequest, ForgeError> {
        let url = format!(
            "{api}/repos/{}/{}/pulls",
            pct_encode(owner),
            pct_encode(repo)
        );
        let mut fields = vec![
            ("title".into(), Json::Str(input.title.clone())),
            ("body".into(), Json::Str(input.body.clone())),
            ("head".into(), Json::Str(head.to_owned())),
            ("base".into(), Json::Str(input.base_branch.clone())),
            ("draft".into(), Json::Bool(input.draft)),
        ];
        if let Some(head_repo) = head_repo {
            fields.push(("head_repo".into(), Json::Str(head_repo.to_owned())));
        }
        let payload = Json::Object(fields);
        let response = api_post_json(http, &url, &payload, token, ACCEPT, &GITHUB_HEADERS)?;
        let json = parse_json(&response)?;
        let web_origin = expected_web_origin(api);
        parse_pr(&json, web_origin.as_ref()).ok_or(ForgeError::BadResponse("invalid pull request"))
    }

    pub(super) fn parse_pr(item: &Json, expected_origin: Option<&Origin>) -> Option<PullRequest> {
        let number = item
            .get("number")
            .and_then(Json::as_u64)
            .filter(|n| *n > 0)?;
        let title = item.str_field("title")?.to_owned();
        let web_url = item.str_field("html_url")?.to_owned();
        if title.is_empty() || !web_url_ok(&web_url, expected_origin) {
            return None;
        }
        let merged = item
            .get("merged_at")
            .and_then(Json::as_str)
            .is_some_and(|s| !s.is_empty())
            || item.get("merged").and_then(Json::as_bool) == Some(true);
        let state = if merged {
            PrState::Merged
        } else {
            match item.str_field("state") {
                Some("open") => PrState::Open,
                Some("closed") => PrState::Closed,
                _ => return None,
            }
        };
        let draft = item.get("draft").and_then(Json::as_bool).unwrap_or(false);
        let author = item
            .get("user")
            .and_then(|u| u.str_field("login"))
            .unwrap_or("unknown")
            .to_owned();
        let head = item.get("head")?;
        let head_branch = head.str_field("ref")?.to_owned();
        let head_repo = head
            .get("repo")
            .and_then(|r| r.str_field("full_name"))
            .map(str::to_owned);
        let head_sha = head.str_field("sha").map(str::to_owned);
        let base_branch = item.get("base")?.str_field("ref")?.to_owned();
        let comments_count = match (
            item.get("comments").and_then(Json::as_u64),
            item.get("review_comments").and_then(Json::as_u64),
        ) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        let mergeable = item.get("mergeable").and_then(Json::as_bool);
        let review_status = if item
            .get("requested_reviewers")
            .and_then(Json::as_array)
            .is_some_and(|r| !r.is_empty())
        {
            ReviewStatus::Pending
        } else {
            ReviewStatus::None
        };
        Some(PullRequest {
            number,
            title,
            body: item.str_field("body").unwrap_or_default().to_owned(),
            state,
            draft,
            author,
            head_branch,
            head_repo,
            head_sha,
            base_branch,
            web_url,
            updated_at: item.str_field("updated_at").map(str::to_owned),
            review_status,
            comments_count,
            mergeable,
        })
    }

    pub(super) fn summarize_github_reviews(items: &[Json]) -> ReviewStatus {
        use std::collections::BTreeMap;
        let mut latest_by_user: BTreeMap<String, &str> = BTreeMap::new();
        let mut any_comment = false;
        let mut any_pending = false;
        for item in items {
            let Some(state) = item.str_field("state") else {
                continue;
            };
            let user = item
                .get("user")
                .and_then(|u| u.str_field("login"))
                .unwrap_or("")
                .to_ascii_lowercase();
            match state {
                "APPROVED" | "CHANGES_REQUESTED" => {
                    latest_by_user.insert(user, state);
                }
                "DISMISSED" => {
                    latest_by_user.remove(&user);
                }
                "COMMENTED" => any_comment = true,
                "PENDING" => any_pending = true,
                _ => {}
            }
        }
        if latest_by_user.values().any(|s| *s == "CHANGES_REQUESTED") {
            ReviewStatus::ChangesRequested
        } else if latest_by_user.values().any(|s| *s == "APPROVED") {
            ReviewStatus::Approved
        } else if any_comment {
            ReviewStatus::Commented
        } else if any_pending {
            ReviewStatus::Pending
        } else {
            ReviewStatus::None
        }
    }
}

// ---------------------------------------------------------------------------
// GitLab
// ---------------------------------------------------------------------------

pub mod gitlab {
    use super::*;

    const ACCEPT: &str = "application/json";

    /// The page the user approves the app on.
    pub fn authorize_url(
        origin: &Origin,
        client_id: &str,
        redirect_uri: &str,
        state: &str,
        challenge: &str,
    ) -> String {
        format!(
            "{}/oauth/authorize?{}",
            origin.as_str(),
            form_body(&[
                ("client_id", client_id),
                ("redirect_uri", redirect_uri),
                ("response_type", "code"),
                ("state", state),
                ("scope", GITLAB_SCOPE),
                ("code_challenge", challenge),
                ("code_challenge_method", "S256"),
            ])
        )
    }

    pub fn exchange_code(
        http: &Http,
        origin: &Origin,
        client_id: &str,
        redirect_uri: &str,
        code: &str,
        verifier: &str,
        now: u64,
    ) -> Result<Credential, ForgeError> {
        let body = form_body(&[
            ("client_id", client_id),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ]);
        let response = http.post_form(&format!("{}/oauth/token", origin.as_str()), body, None)?;
        let json = token_answer(&response).map_err(OAuthAnswer::into_error)?;
        credential_from(&json, now)
    }

    pub fn refresh(
        http: &Http,
        origin: &Origin,
        client_id: &str,
        refresh_token: &Secret,
        now: u64,
    ) -> Result<Credential, ForgeError> {
        let body = form_body(&[
            ("client_id", client_id),
            ("refresh_token", refresh_token.expose()),
            ("grant_type", "refresh_token"),
        ]);
        let response = http.post_form(&format!("{}/oauth/token", origin.as_str()), body, None)?;
        match token_answer(&response) {
            Ok(json) => credential_from(&json, now),
            // A refresh the server refuses means the grant is gone.
            Err(OAuthAnswer::Error(code)) if code == "invalid_grant" => {
                Err(ForgeError::Unauthorized)
            }
            Err(other) => Err(other.into_error()),
        }
    }

    pub fn current_user(
        http: &Http,
        origin: &Origin,
        token: &Secret,
    ) -> Result<String, ForgeError> {
        let url = format!("{}/api/v4/user", origin.as_str());
        let response = api_get(http, &url, token, ACCEPT, &[])?;
        parse_json(&response)?
            .str_field("username")
            .map(str::to_owned)
            .ok_or(ForgeError::BadResponse("no username"))
    }

    pub fn list_repos_page(
        http: &Http,
        origin: &Origin,
        token: &Secret,
        cursor: Option<&str>,
    ) -> Result<RepoPage, ForgeError> {
        let url = match cursor {
            Some(cursor) if same_origin(cursor, origin.as_str()) => cursor.to_owned(),
            Some(_) => return Err(ForgeError::InsecureUrl),
            None => format!(
                "{}/api/v4/projects?membership=true&archived=false&order_by=last_activity_at&per_page={PAGE_SIZE}",
                origin.as_str()
            ),
        };
        let response = api_get(http, &url, token, ACCEPT, &[])?;
        let json = parse_json(&response)?;
        let items = json
            .as_array()
            .ok_or(ForgeError::BadResponse("not a list"))?;
        Ok(RepoPage {
            repos: items.iter().filter_map(parse_repo).collect(),
            next: next_page(&response, origin.as_str()),
        })
    }

    pub(super) fn parse_repo(item: &Json) -> Option<RemoteRepo> {
        let full_name = item.str_field("path_with_namespace")?.to_owned();
        let clone_url = item.str_field("http_url_to_repo")?.to_owned();
        if full_name.is_empty() || !clone_url_ok(&clone_url) {
            return None;
        }
        let visibility = match item.str_field("visibility") {
            Some("public") => Visibility::Public,
            Some("internal") => Visibility::Internal,
            _ => Visibility::Private,
        };
        Some(RemoteRepo {
            full_name,
            clone_url,
            description: item.str_field("description").unwrap_or_default().to_owned(),
            visibility,
            default_branch: item.str_field("default_branch").map(str::to_owned),
            updated_at: item.str_field("last_activity_at").map(str::to_owned),
        })
    }

    fn expected_web_origin(origin: &Origin) -> Option<Origin> {
        Some(origin.clone())
    }

    pub fn list_mrs_page(
        http: &Http,
        origin: &Origin,
        token: &Secret,
        project_path: &str,
        filter: PrFilter,
        cursor: Option<&str>,
    ) -> Result<PrPage, ForgeError> {
        let url = match cursor {
            Some(cursor) if same_origin(cursor, origin.as_str()) => cursor.to_owned(),
            Some(_) => return Err(ForgeError::InsecureUrl),
            None => format!(
                "{}/api/v4/projects/{}/merge_requests?state={}&order_by=updated_at&sort=desc&per_page={PR_PAGE_SIZE}",
                origin.as_str(),
                pct_encode(project_path),
                filter.gitlab_param()
            ),
        };
        let response = api_get(http, &url, token, ACCEPT, &[])?;
        let json = parse_json(&response)?;
        let items = json
            .as_array()
            .ok_or(ForgeError::BadResponse("not a list"))?;
        let web_origin = expected_web_origin(origin);
        Ok(PrPage {
            prs: items
                .iter()
                .filter_map(|item| parse_mr(item, web_origin.as_ref()))
                .collect(),
            next: next_page(&response, origin.as_str()),
        })
    }

    pub fn get_mr(
        http: &Http,
        origin: &Origin,
        token: &Secret,
        project_path: &str,
        iid: u64,
    ) -> Result<PullRequest, ForgeError> {
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests/{iid}",
            origin.as_str(),
            pct_encode(project_path)
        );
        let response = api_get(http, &url, token, ACCEPT, &[])?;
        let json = parse_json(&response)?;
        let web_origin = expected_web_origin(origin);
        parse_mr(&json, web_origin.as_ref()).ok_or(ForgeError::BadResponse("invalid merge request"))
    }

    pub fn get_mr_approvals(
        http: &Http,
        origin: &Origin,
        token: &Secret,
        project_path: &str,
        iid: u64,
    ) -> Result<ReviewStatus, ForgeError> {
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests/{iid}/approvals",
            origin.as_str(),
            pct_encode(project_path)
        );
        let response = api_get(http, &url, token, ACCEPT, &[])?;
        let json = parse_json(&response)?;
        // Prefer the server's explicit aggregate `approved` boolean; never
        // override `approved: false` when only 1 of N required approvers signed.
        let approved = match json.get("approved").and_then(Json::as_bool) {
            Some(flag) => flag,
            None => json
                .get("approved_by")
                .and_then(Json::as_array)
                .is_some_and(|a| !a.is_empty()),
        };
        if approved {
            Ok(ReviewStatus::Approved)
        } else {
            Ok(ReviewStatus::Pending)
        }
    }

    pub fn get_project_id(
        http: &Http,
        origin: &Origin,
        token: &Secret,
        project_path: &str,
    ) -> Result<u64, ForgeError> {
        let url = format!(
            "{}/api/v4/projects/{}",
            origin.as_str(),
            pct_encode(project_path)
        );
        let response = api_get(http, &url, token, ACCEPT, &[])?;
        let json = parse_json(&response)?;
        json.get("id")
            .and_then(Json::as_u64)
            .ok_or(ForgeError::BadResponse("no project id"))
    }

    pub fn create_mr(
        http: &Http,
        origin: &Origin,
        token: &Secret,
        source_project_path: &str,
        source_branch: &str,
        input: &CreatePrInput,
        target_project_id: Option<u64>,
    ) -> Result<PullRequest, ForgeError> {
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests",
            origin.as_str(),
            pct_encode(source_project_path)
        );
        let title = if input.draft && !input.title.starts_with("Draft:") {
            format!("Draft: {}", input.title)
        } else {
            input.title.clone()
        };
        let mut fields = vec![
            ("title".into(), Json::Str(title)),
            ("description".into(), Json::Str(input.body.clone())),
            ("source_branch".into(), Json::Str(source_branch.to_owned())),
            ("target_branch".into(), Json::Str(input.base_branch.clone())),
        ];
        if let Some(id) = target_project_id {
            fields.push(("target_project_id".into(), Json::Number(id as f64)));
        }
        let payload = Json::Object(fields);
        let response = api_post_json(http, &url, &payload, token, ACCEPT, &[])?;
        let json = parse_json(&response)?;
        let web_origin = expected_web_origin(origin);
        parse_mr(&json, web_origin.as_ref()).ok_or(ForgeError::BadResponse("invalid merge request"))
    }

    pub(super) fn parse_mr(item: &Json, expected_origin: Option<&Origin>) -> Option<PullRequest> {
        let number = item.get("iid").and_then(Json::as_u64).filter(|n| *n > 0)?;
        let title = item.str_field("title")?.to_owned();
        let web_url = item.str_field("web_url")?.to_owned();
        if title.is_empty() || !web_url_ok(&web_url, expected_origin) {
            return None;
        }
        let state = match item.str_field("state") {
            Some("opened") => PrState::Open,
            Some("merged") => PrState::Merged,
            Some("closed") => PrState::Closed,
            _ => return None,
        };
        let draft = item.get("draft").and_then(Json::as_bool).unwrap_or(false)
            || item
                .get("work_in_progress")
                .and_then(Json::as_bool)
                .unwrap_or(false)
            || title.starts_with("Draft:");
        let author = item
            .get("author")
            .and_then(|a| a.str_field("username"))
            .unwrap_or("unknown")
            .to_owned();
        let head_branch = item.str_field("source_branch")?.to_owned();
        let base_branch = item.str_field("target_branch")?.to_owned();
        let head_sha = item.str_field("sha").map(str::to_owned);
        let comments_count = item.get("user_notes_count").and_then(Json::as_u64);
        let mergeable = item
            .get("has_conflicts")
            .and_then(Json::as_bool)
            .map(|conflicts| !conflicts);
        let review_status = if item
            .get("reviewers")
            .and_then(Json::as_array)
            .is_some_and(|r| !r.is_empty())
        {
            ReviewStatus::Pending
        } else {
            ReviewStatus::None
        };
        Some(PullRequest {
            number,
            title,
            body: item.str_field("description").unwrap_or_default().to_owned(),
            state,
            draft,
            author,
            head_branch,
            head_repo: None,
            head_sha,
            base_branch,
            web_url,
            updated_at: item.str_field("updated_at").map(str::to_owned),
            review_status,
            comments_count,
            mergeable,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(text: &str) -> Json {
        Json::parse(text).unwrap()
    }

    #[test]
    fn a_github_repo_becomes_a_remote_repo() {
        let repo = github::parse_repo(&json(
            r#"{"full_name":"ana/tool","clone_url":"https://github.com/ana/tool.git",
                "description":null,"private":true,"visibility":"private",
                "default_branch":"main","pushed_at":"2026-01-02T03:04:05Z"}"#,
        ))
        .unwrap();
        assert_eq!(repo.full_name, "ana/tool");
        assert_eq!(repo.visibility, Visibility::Private);
        assert_eq!(repo.description, "");
        assert_eq!(repo.updated_at.as_deref(), Some("2026-01-02T03:04:05Z"));
        // `private: false` without a visibility field is public.
        let open = github::parse_repo(&json(
            r#"{"full_name":"a/b","clone_url":"https://github.com/a/b.git","private":false}"#,
        ))
        .unwrap();
        assert_eq!(open.visibility, Visibility::Public);
    }

    #[test]
    fn a_gitlab_project_becomes_a_remote_repo() {
        let repo = gitlab::parse_repo(&json(
            r#"{"path_with_namespace":"grp/sub/tool","http_url_to_repo":"https://gitlab.com/grp/sub/tool.git",
                "visibility":"internal","last_activity_at":"2026-02-02T00:00:00Z","default_branch":"trunk"}"#,
        ))
        .unwrap();
        assert_eq!(repo.full_name, "grp/sub/tool");
        assert_eq!(repo.visibility, Visibility::Internal);
        assert_eq!(repo.default_branch.as_deref(), Some("trunk"));
    }

    #[test]
    fn a_clone_address_git_could_run_a_command_for_is_dropped() {
        for url in [
            "ext::sh -c touch% /tmp/pwned",
            "https://x-access-token:ghp_SECRET@github.com/a/b.git",
            "https://ghp_SECRET@github.com/a/b.git",
            "https://github.com/a/b.git?access_token=ghp_SECRET",
            "https://github.com/a/b.git#ghp_SECRET",
            "file:///etc/passwd",
            "--upload-pack=evil",
            "git@github.com:a/b.git",
            "ssh://host/x",
            "",
        ] {
            assert!(!clone_url_ok(url), "{url}");
            let item = format!(
                r#"{{"full_name":"a/b","clone_url":{url:?},"path_with_namespace":"a/b","http_url_to_repo":{url:?}}}"#
            );
            assert!(github::parse_repo(&json(&item)).is_none(), "{url}");
            assert!(gitlab::parse_repo(&json(&item)).is_none(), "{url}");
        }
        assert!(clone_url_ok("https://github.com/a/b.git"));
    }

    #[test]
    fn an_item_without_a_name_or_address_is_skipped() {
        assert!(github::parse_repo(&json(r#"{"full_name":"a/b"}"#)).is_none());
        assert!(github::parse_repo(&json(r#"{"clone_url":"https://x/y.git"}"#)).is_none());
        assert!(github::parse_repo(&json("[]")).is_none());
        assert!(gitlab::parse_repo(&json(r#"{"http_url_to_repo":"https://x/y.git"}"#)).is_none());
    }

    #[test]
    fn the_authorize_address_carries_pkce_and_state() {
        let origin = Origin::parse("https://gitlab.example.com").unwrap();
        let url = gitlab::authorize_url(
            &origin,
            "cid",
            "http://127.0.0.1:47821/callback",
            "st&te",
            "chal",
        );
        assert!(url.starts_with("https://gitlab.example.com/oauth/authorize?"));
        for part in [
            "client_id=cid",
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A47821%2Fcallback",
            "response_type=code",
            "state=st%26te",
            "code_challenge=chal",
            "code_challenge_method=S256",
            "scope=api",
        ] {
            assert!(url.contains(part), "{part} in {url}");
        }
    }

    #[test]
    fn a_next_page_on_another_server_is_not_followed() {
        let response = |link: &str| HttpResponse::for_test(200, &[("link", link)]);
        let ok = response(r#"<https://api.github.com/user/repos?page=2>; rel="next""#);
        assert_eq!(
            next_page(&ok, "https://api.github.com").as_deref(),
            Some("https://api.github.com/user/repos?page=2")
        );
        let evil = response(r#"<https://evil.example/steal>; rel="next""#);
        assert_eq!(next_page(&evil, "https://api.github.com"), None);
        let downgrade = response(r#"<http://api.github.com/user/repos?page=2>; rel="next""#);
        assert_eq!(next_page(&downgrade, "https://api.github.com"), None);
    }
}
