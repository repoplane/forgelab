//! Reads a forge's answer into a `Class`. Each forge signals a rate limit its own way, and one
//! of them signals a bad token with a 203, so the reading is per forge.

use std::time::Duration;

use bytes::Bytes;

use super::error::Class;

/// A non-2xx answer, classified, with the pause the server asked for if it asked for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classified {
    pub class: Class,
    pub retry_after: Option<Duration>,
}

pub type Classifier = fn(&http::Response<Bytes>) -> Classified;

fn plain(class: Class) -> Classified {
    Classified { class, retry_after: None }
}

/// `Retry-After` in seconds, plus one so that a request is never sent a hair too early.
fn retry_after(resp: &http::Response<Bytes>) -> Option<Duration> {
    let s = resp.headers().get("retry-after")?.to_str().ok()?.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(s + 1))
}

/// The reading every forge shares: the status code alone.
pub fn generic(resp: &http::Response<Bytes>) -> Classified {
    let status = resp.status().as_u16();
    let class = Class::of_status(status);
    Classified { class, retry_after: if class == Class::RateLimited { retry_after(resp) } else { None } }
}

/// Forgejo: nothing beyond the status code, except that a `Retry-After` on a 503 is a pause
/// to sit through rather than a failure.
pub fn forgejo(resp: &http::Response<Bytes>) -> Classified {
    let c = generic(resp);
    if resp.status().as_u16() == 503
        && let Some(wait) = retry_after(resp)
    {
        return Classified { class: Class::RateLimited, retry_after: Some(wait) };
    }
    c
}

/// What GitHub calls a secondary limit, in the one place it always says so.
const SECONDARY_LIMIT: &str = "secondary rate limit";

/// The pause before trying again when only the body said so. GitHub asks for at least a
/// minute, and the retry loop repeats it, so a block that outlasts one pause is still waited
/// out rather than reported.
pub const GITHUB_SECONDARY_WAIT: Duration = Duration::from_secs(60);

/// GitHub recognises both of its limits. A secondary one answers 403 or 429, sometimes with
/// Retry-After and otherwise saying so only in the body; an exhausted primary one answers 403
/// with X-RateLimit-Remaining: 0 and the reset time. A plain 403 (no permission) is neither,
/// and is not retried.
///
/// The headers cannot be relied on for a secondary limit: there is often no Retry-After, and
/// X-RateLimit-Remaining reports what is left of the *primary* budget, which a secondary limit
/// leaves untouched -- 4264 of 5000 on the one that stopped a fleet part-way through an apply.
pub fn github(resp: &http::Response<Bytes>) -> Classified {
    let status = resp.status().as_u16();
    if status != 403 && status != 429 {
        return generic(resp);
    }
    if let Some(wait) = retry_after(resp) {
        return Classified { class: Class::RateLimited, retry_after: Some(wait) };
    }
    if resp.headers().get("x-ratelimit-remaining").and_then(|v| v.to_str().ok()) == Some("0")
        && let Some(reset) = resp.headers().get("x-ratelimit-reset").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok())
    {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let wait = Duration::from_secs((reset - now).max(1) as u64);
        return Classified { class: Class::RateLimited, retry_after: Some(wait) };
    }
    let body = String::from_utf8_lossy(resp.body()).to_lowercase();
    if body.contains(SECONDARY_LIMIT) {
        return Classified { class: Class::RateLimited, retry_after: Some(GITHUB_SECONDARY_WAIT) };
    }
    if status == 429 {
        return Classified { class: Class::RateLimited, retry_after: Some(Duration::from_secs(60)) };
    }
    plain(Class::Forbidden)
}

/// GitLab: a 429, with `Retry-After` when it says.
pub fn gitlab(resp: &http::Response<Bytes>) -> Classified {
    generic(resp)
}

/// Azure DevOps: an expired or wrong PAT is answered with a 203 and a sign-in page, not a 401.
pub fn azuredevops(resp: &http::Response<Bytes>) -> Classified {
    if resp.status().as_u16() == 203 {
        return plain(Class::Auth);
    }
    generic(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(status: u16, headers: &[(&str, &str)], body: &str) -> http::Response<Bytes> {
        let mut b = http::Response::builder().status(status);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Bytes::from(body.to_string())).unwrap()
    }

    #[test]
    fn github_limits() {
        let c = github(&resp(403, &[("Retry-After", "5")], ""));
        assert_eq!(c, Classified { class: Class::RateLimited, retry_after: Some(Duration::from_secs(6)) });
        let c = github(&resp(403, &[("X-RateLimit-Remaining", "4264")], r#"{"message":"You have exceeded a secondary rate limit."}"#));
        assert_eq!(c.class, Class::RateLimited);
        assert_eq!(c.retry_after, Some(GITHUB_SECONDARY_WAIT));
        assert_eq!(github(&resp(403, &[], r#"{"message":"Must have admin rights"}"#)).class, Class::Forbidden);
        assert_eq!(github(&resp(429, &[], "")).class, Class::RateLimited);
        assert_eq!(github(&resp(404, &[], "")).class, Class::NotFound);
        let reset = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 2400).to_string();
        let c = github(&resp(403, &[("X-RateLimit-Remaining", "0"), ("X-RateLimit-Reset", &reset)], ""));
        assert_eq!(c.class, Class::RateLimited);
        assert!(c.retry_after.unwrap() > Duration::from_secs(2300));
    }

    #[test]
    fn azure_sign_in_page_is_auth() {
        assert_eq!(azuredevops(&resp(203, &[], "<html>")).class, Class::Auth);
        assert_eq!(azuredevops(&resp(429, &[("Retry-After", "1")], "")).retry_after, Some(Duration::from_secs(2)));
    }

    #[test]
    fn forgejo_503_with_retry_after_is_a_pause() {
        assert_eq!(forgejo(&resp(503, &[("Retry-After", "3")], "")).class, Class::RateLimited);
        assert_eq!(forgejo(&resp(503, &[], "")).class, Class::Transient);
    }
}
