#[cfg(test)]
mod validate_endpoint_url_tests {
    use crate::validate::validate_endpoint_url;

    #[test]
    fn endpoint_url_requires_http_or_https_with_host() {
        assert!(validate_endpoint_url("http://localhost:4317").is_ok());
        assert!(validate_endpoint_url("https://example.com/path").is_ok());
        assert!(validate_endpoint_url("HTTP://EXAMPLE.COM").is_ok());

        for bad in [
            "http:///bulk",
            "https://?x=1",
            "http://   ",
            "ftp://example.com",
        ] {
            assert!(
                validate_endpoint_url(bad).is_err(),
                "expected endpoint validation error for {bad}"
            );
        }
    }

    #[test]
    fn endpoint_url_redacts_userinfo_for_malformed_urls() {
        let err = validate_endpoint_url("https://user:secret@/bulk")
            .expect_err("must fail")
            .to_string();
        assert!(
            err.contains("***redacted***") && !err.contains("secret"),
            "malformed endpoint errors must redact userinfo: {err}"
        );
    }
}
