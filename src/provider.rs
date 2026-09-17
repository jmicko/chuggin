//! Provider availability is separate from model reasoning recovery.
use std::time::{Duration, SystemTime};

#[derive(Debug)]
pub struct Unavailable {
    pub reason: &'static str,
    pub retry_after: Option<Duration>,
}
impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason)
    }
}
impl std::error::Error for Unavailable {}
#[derive(Debug)]
pub struct Stopped(pub String);
impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Stopped {}

pub fn retry_after(value: Option<&str>, now: SystemTime) -> Option<Duration> {
    let value = value?;
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .map(|t| t.duration_since(now).unwrap_or_default())
        })
        .map(|d| d.max(Duration::from_secs(1)))
}
pub fn classify(status: u16, message: &str, retry_after: Option<Duration>) -> Option<Unavailable> {
    let text = message.to_ascii_lowercase();
    let legacy_window = ["session limit", "weekly limit", "5-hour", "five-hour"]
        .iter()
        .any(|s| text.contains(s));
    let exhausted = (status == 402 && !legacy_window)
        || [
            "insufficient credits",
            "insufficient balance",
            "credits exhausted",
            "credit balance",
            "out of credits",
            "payment required",
            "monthly usage limit",
            "monthly limit",
        ]
        .iter()
        .any(|s| text.contains(s));
    let limited = status == 429
        || legacy_window
        || [
            "usage limit",
            "rate limit",
            "quota exceeded",
            "too many requests",
            "weekly limit",
            "session limit",
        ]
        .iter()
        .any(|s| text.contains(s));
    let auth = matches!(status, 401 | 403) && !limited && !exhausted;
    if !(exhausted || limited || auth || (500..600).contains(&status)) {
        return None;
    }
    Some(Unavailable {
        reason: if exhausted {
            "Provider credits exhausted or payment required"
        } else if auth {
            "Provider authentication or access needs attention"
        } else if limited {
            "Provider usage or rate limit reached"
        } else {
            "Provider temporarily unavailable"
        },
        retry_after,
    })
}
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(60 * (1u64 << attempt.saturating_sub(1).min(4)))
        .min(Duration::from_secs(900))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_limits_credits_and_authentication_share_wait_policy() {
        for text in [
            "usage limit reached",
            "weekly limit reached",
            "session limit exceeded",
        ] {
            assert!(classify(200, text, None).is_some());
        }
        assert!(classify(402, "", None).is_some());
        assert!(classify(200, "insufficient credits", None).is_some());
        assert_eq!(
            classify(402, "", Some(Duration::from_secs(3600)))
                .unwrap()
                .retry_after,
            Some(Duration::from_secs(3600))
        );
        assert!(classify(401, "", None).is_some());
        assert!(classify(400, "invalid tool schema", None).is_none());
        assert!(classify(503, "", None).is_some());
    }
    #[test]
    fn retry_headers_and_fallback_delays() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(
            retry_after(Some("3600"), now),
            Some(Duration::from_secs(3600))
        );
        let date = httpdate::fmt_http_date(now + Duration::from_secs(7200));
        assert_eq!(
            retry_after(Some(&date), now),
            Some(Duration::from_secs(7200))
        );
        assert_eq!(retry_after(Some("bad"), now), None);
        assert_eq!(retry_after(Some("0"), now), Some(Duration::from_secs(1)));
        assert_eq!(
            (1..=6).map(|n| backoff(n).as_secs()).collect::<Vec<_>>(),
            [60, 120, 240, 480, 900, 900]
        );
        for attempt in [100, 10_000, u32::MAX] {
            assert_eq!(backoff(attempt), Duration::from_secs(900));
        }
    }
}
