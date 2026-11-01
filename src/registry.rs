//! Discovery of the newest OpenCode V2 release tag from the GHCR registry.
//!
//! Upstream publishes only immutable `2.x.y` release tags for V2 and keeps
//! the `latest` tag pointing at the V1 release (anomalyco/opencode#48546),
//! so there is no floating tag to track. `--build` therefore queries the
//! registry tag list for the newest `2.x.y` and builds the variants FROM it.
//! Run-time image resolution never talks to the registry; this module is
//! only reachable through `--build`.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::time::{Duration, Instant};

const REGISTRY: &str = "ghcr.io";
const REPOSITORY: &str = "anomalyco/opencode";
/// The tag list is long (thousands of tags), so fetch it in pages and follow
/// the `Link` headers until it is exhausted.
const PAGE_SIZE: usize = 1000;
const MAX_PAGES: usize = 10;
/// Belt-and-braces cap on the whole discovery (per-call timeouts already bound
/// each request; this catches anything ureq's per-call accounting might miss).
const TOTAL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(alias = "access_token")]
    token: String,
}

#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    tags: Vec<String>,
}

/// Result of a registry release lookup.
#[derive(Debug)]
pub struct Latest {
    /// The newest `2.x.y` release image reference, e.g.
    /// `ghcr.io/anomalyco/opencode:2.0.19`, or `None` when the registry
    /// carried no `2.x.y` tags.
    pub image: Option<String>,
    /// True when the tag list was cut off before the registry said it was
    /// complete, so `image` may be `None` because of truncation rather than
    /// a genuine absence of V2 release tags.
    pub truncated: bool,
}

/// Look up the newest OpenCode V2 release tag (`2.x.y`) on GHCR and return
/// it as an image reference, e.g. `ghcr.io/anomalyco/opencode:2.0.19`.
///
/// Returns an error when the registry cannot be reached, so the caller
/// decides how to fall back.
pub fn latest_base_image() -> Result<Latest> {
    let agent = ureq::config::Config::builder()
        .timeout_per_call(Some(Duration::from_secs(20)))
        .timeout_global(Some(TOTAL_TIMEOUT))
        .build()
        .new_agent();

    let token = fetch_token(&agent)?;
    let (tags, truncated) = fetch_all_tags(&agent, &token)?;
    Ok(Latest {
        image: parse_latest_v2(&tags).map(|tag| format!("{REGISTRY}/{REPOSITORY}:{tag}")),
        truncated,
    })
}

/// Look up the newest OpenCode V2 release, retrying transport failures until
/// `timeout` elapses. `--build --wait-for-registry` uses this so a machine
/// that wakes before its network is ready does not silently skip the run.
///
/// A successful lookup is returned immediately. A truncated tag list is also
/// returned as-is: truncation is a deterministic result, not a transport
/// failure, and the caller's existing keep-images fallback handles it.
pub fn latest_base_image_with_retry(timeout: Duration) -> Result<Latest> {
    latest_base_image_with_retry_using(
        latest_base_image,
        timeout,
        Duration::from_secs(5),
        std::thread::sleep,
    )
}

fn latest_base_image_with_retry_using(
    mut discover: impl FnMut() -> Result<Latest>,
    timeout: Duration,
    initial_delay: Duration,
    mut sleep: impl FnMut(Duration),
) -> Result<Latest> {
    let deadline = Instant::now() + timeout;
    let mut delay = initial_delay;
    let mut first_failure = true;

    loop {
        match discover() {
            Ok(latest) => return Ok(latest),
            Err(err) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(err).with_context(|| {
                        format!(
                            "registry did not become reachable within {}s",
                            timeout.as_secs()
                        )
                    });
                }

                let remaining = deadline.saturating_duration_since(now);
                let sleep_for = delay.min(remaining);
                if first_failure {
                    eprintln!(
                        "Registry not reachable ({err:#}); waiting up to {}s...",
                        timeout.as_secs()
                    );
                    first_failure = false;
                } else {
                    eprintln!(
                        "Registry still not reachable ({err:#}); retrying in {}s ({}s left)...",
                        sleep_for.as_secs(),
                        remaining.as_secs()
                    );
                }
                sleep(sleep_for);
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
}

fn fetch_token(agent: &ureq::Agent) -> Result<String> {
    let url = format!("https://{REGISTRY}/token?scope=repository:{REPOSITORY}:pull");
    let mut resp = agent
        .get(&url)
        .call()
        .with_context(|| format!("failed to fetch registry token from {url}"))?;
    let parsed: TokenResponse = resp
        .body_mut()
        .read_json()
        .context("failed to parse registry token response")?;
    Ok(parsed.token)
}

/// Fetch the tag list for the repository, following pagination links.
/// Returns `(tags, truncated)` where `truncated` is set when the fetch hit
/// `MAX_PAGES` with a further page still pending.
fn fetch_all_tags(agent: &ureq::Agent, token: &str) -> Result<(Vec<String>, bool)> {
    let mut tags = Vec::new();
    let mut url: Option<String> = Some(format!(
        "https://{REGISTRY}/v2/{REPOSITORY}/tags/list?n={PAGE_SIZE}"
    ));
    for _ in 0..MAX_PAGES {
        let Some(current) = url.take() else {
            break;
        };
        let mut resp = agent
            .get(&current)
            .header("Authorization", format!("Bearer {token}"))
            .call()
            .with_context(|| format!("failed to fetch tag list from {current}"))?;
        let next = resp
            .headers()
            .get("Link")
            .and_then(|value| value.to_str().ok())
            .and_then(next_url);
        let parsed: TagsResponse = resp
            .body_mut()
            .read_json()
            .context("failed to parse registry tag list")?;
        tags.extend(parsed.tags);
        url = next;
    }
    // Reaching the page budget with a next link still pending means the list
    // was cut off; the caller needs to know so it doesn't mistake the absence
    // of a `2.x.y` tag for the real thing.
    let truncated = url.is_some();
    Ok((tags, truncated))
}

/// Extract the `rel="next"` URL from a `Link` header, if present, resolved
/// against the registry origin: GHCR emits relative paths, but some
/// registries emit absolute URLs, so pass those through untouched.
fn next_url(link: &str) -> Option<String> {
    let url = parse_next_link(link)?;
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(url)
    } else {
        Some(format!("https://{REGISTRY}{url}"))
    }
}

/// Pull the URL out of the `rel="next"` entry of a `Link` header.
fn parse_next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let part = part.trim();
        if !part.contains("rel=\"next\"") {
            return None;
        }
        let url = part.split_once('>')?.0.strip_prefix('<')?.trim();
        if url.is_empty() {
            None
        } else {
            Some(url.to_string())
        }
    })
}

/// The newest `2.x.y` release tag in a raw tag list, or `None`.
///
/// Only `2.<minor>.<patch>` tags qualify: everything else (`latest`, `beta`,
/// `dev`, `1.x`, `0.0.0-dev-*`, malformed tags) is ignored. Versions compare
/// numerically, so `2.0.10` beats `2.0.9`.
pub fn parse_latest_v2(tags: &[String]) -> Option<String> {
    tags.iter()
        .filter_map(|tag| parse_v2(tag).map(|version| (version, tag)))
        .max_by_key(|(version, _)| *version)
        .map(|(_, tag)| tag.clone())
}

fn parse_v2(tag: &str) -> Option<(u32, u32)> {
    let minor_patch = tag.strip_prefix("2.")?;
    let (minor, patch) = minor_patch.split_once('.')?;
    Some((minor.parse().ok()?, patch.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn picks_numeric_max() {
        assert_eq!(
            parse_latest_v2(&s(&["2.0.9", "2.0.10", "2.0.11"])),
            Some("2.0.11".to_string())
        );
        assert_eq!(
            parse_latest_v2(&s(&["2.0.18", "2.0.19", "2.0.10"])),
            Some("2.0.19".to_string())
        );
    }

    #[test]
    fn picks_numeric_max_across_minors() {
        assert_eq!(
            parse_latest_v2(&s(&["2.9.99", "2.10.0", "2.10.1"])),
            Some("2.10.1".to_string())
        );
        assert_eq!(
            parse_latest_v2(&s(&["2.10.0", "2.9.99"])),
            Some("2.10.0".to_string())
        );
    }

    #[test]
    fn ignores_non_v2_release_tags() {
        let tags = s(&[
            "latest",
            "beta",
            "dev",
            "1.0.196",
            "1.0.94",
            "0.0.0-beta-202603181544",
            "0.0.0-dev-202602201301",
            "2.0",
            "2",
            "v2.0.1",
            "2.0.1.1",
            "2.1.0-beta.1",
        ]);
        assert_eq!(parse_latest_v2(&tags), None);
    }

    #[test]
    fn mixes_release_and_non_release() {
        let tags = s(&[
            "latest",
            "0.0.0-dev-202602201301",
            "2.0.7",
            "2.0.5",
            "1.0.196",
        ]);
        assert_eq!(parse_latest_v2(&tags), Some("2.0.7".to_string()));
    }

    #[test]
    fn empty_list_is_none() {
        assert_eq!(parse_latest_v2(&[]), None);
    }

    #[test]
    fn parses_next_link() {
        assert_eq!(
            parse_next_link("</v2/anomalyco/opencode/tags/list?last=2.0.9&n=1000>; rel=\"next\""),
            Some("/v2/anomalyco/opencode/tags/list?last=2.0.9&n=1000".to_string())
        );
        assert_eq!(parse_next_link(""), None);
        assert_eq!(parse_next_link("<>; rel=\"prev\""), None);
        assert_eq!(
            parse_next_link("</a>; rel=\"prev\", </b>; rel=\"next\""),
            Some("/b".to_string())
        );
        assert_eq!(
            parse_next_link(
                "</v2/anomalyco/opencode/tags/list?last=2.0.9&n=1000>; rel=\"next\"; type=\"text/plain\""
            ),
            Some("/v2/anomalyco/opencode/tags/list?last=2.0.9&n=1000".to_string())
        );
    }

    #[test]
    fn resolves_next_url() {
        assert_eq!(
            next_url("</v2/anomalyco/opencode/tags/list?last=2.0.9&n=1000>; rel=\"next\""),
            Some("https://ghcr.io/v2/anomalyco/opencode/tags/list?last=2.0.9&n=1000".to_string())
        );
        // Absolute Link URLs (emitted by some registries) pass through as-is.
        assert_eq!(
            next_url(
                "<https://ghcr.io/v2/anomalyco/opencode/tags/list?last=2.0.9&n=1000>; rel=\"next\""
            ),
            Some("https://ghcr.io/v2/anomalyco/opencode/tags/list?last=2.0.9&n=1000".to_string())
        );
        assert_eq!(next_url(""), None);
    }

    #[test]
    fn retries_until_discovery_succeeds() {
        let attempts = std::cell::Cell::new(0);
        let latest = latest_base_image_with_retry_using(
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() < 3 {
                    Err(anyhow::anyhow!("offline"))
                } else {
                    Ok(Latest {
                        image: Some("ghcr.io/anomalyco/opencode:2.0.19".to_string()),
                        truncated: false,
                    })
                }
            },
            Duration::from_secs(1),
            Duration::ZERO,
            |_| {},
        )
        .unwrap();

        assert_eq!(attempts.get(), 3);
        assert_eq!(
            latest.image.as_deref(),
            Some("ghcr.io/anomalyco/opencode:2.0.19")
        );
    }

    #[test]
    fn retry_gives_up_at_deadline() {
        let attempts = std::cell::Cell::new(0);
        let err = latest_base_image_with_retry_using(
            || {
                attempts.set(attempts.get() + 1);
                Err(anyhow::anyhow!("offline"))
            },
            Duration::ZERO,
            Duration::ZERO,
            |_| {},
        )
        .unwrap_err();

        assert_eq!(attempts.get(), 1);
        assert!(err
            .to_string()
            .contains("registry did not become reachable"));
    }
}
