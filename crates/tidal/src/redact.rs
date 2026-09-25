//! Redaction for everything Zeke's CLI and app print or log: tokens, client
//! secrets and stream URLs never reach the terminal.

use regex::Regex;
use std::sync::LazyLock;

static RULES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        // DASH manifests passed as data: URIs carry the segment URLs.
        (r"data:([a-z/+.-]+);base64,[A-Za-z0-9+/=]+", "data:$1;base64,<redacted>"),
        // Long base64 runs, e.g. a manifest quoted in an error body.
        (r"[A-Za-z0-9+/]{200,}={0,2}", "<base64 redacted>"),
        // Any URL: keep scheme and host, drop path and query.
        (r#"(https?://[^/\s"'<>]+)/[^\s"'<>]*"#, "$1/<redacted>"),
        // JWTs (TIDAL access tokens).
        (r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*", "<token>"),
        (r"(?i)\bBearer\s+\S+", "Bearer <token>"),
        // key=value and "key": "value" forms of secrets.
        (
            r#"(?i)\b(access_token|refresh_token|client_secret|code_verifier|code|token)(["']?\s*[:=]\s*["']?)[^&"'\s,}]+"#,
            "$1$2<redacted>",
        ),
    ]
    .into_iter()
    .map(|(re, with)| (Regex::new(re).expect("redaction regex"), with))
    .collect()
});

pub fn redact(text: &str) -> String {
    RULES
        .iter()
        .fold(text.to_string(), |acc, (re, with)| re.replace_all(&acc, *with).into_owned())
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn stream_urls_lose_path_and_query() {
        let out = redact("GET https://sp-ad-fa.audio.tidal.com/mediatracks/abc/0.flac?token=xyz ok");
        assert_eq!(out, "GET https://sp-ad-fa.audio.tidal.com/<redacted> ok");
    }

    #[test]
    fn manifests_and_tokens_are_hidden() {
        assert_eq!(
            redact("uri=data:application/dash+xml;base64,PE1QRD4uLi4="),
            "uri=data:application/dash+xml;base64,<redacted>"
        );
        assert_eq!(redact("Authorization: Bearer abc.def"), "Authorization: Bearer <token>");
        assert_eq!(redact("x eyJhbGciOiJI.eyJzdWIiOjF9.sig y"), "x <token> y");
        assert_eq!(
            redact(r#"{"access_token":"s3cr3t","expires_in":1}"#),
            r#"{"access_token":"<redacted>","expires_in":1}"#
        );
        assert_eq!(
            redact("client_secret=abc&refresh_token=def"),
            "client_secret=<redacted>&refresh_token=<redacted>"
        );
    }

    #[test]
    fn a_quoted_manifest_is_hidden() {
        let body = format!(r#"{{"manifest":"{}"}}"#, "QUJD".repeat(80));
        assert_eq!(redact(&body), r#"{"manifest":"<base64 redacted>"}"#);
    }

    #[test]
    fn ordinary_text_is_untouched() {
        let line = "[alsa-writer] resampling: 96kHz -> 48kHz (rate 48000, S32LE)";
        assert_eq!(redact(line), line);
    }
}
