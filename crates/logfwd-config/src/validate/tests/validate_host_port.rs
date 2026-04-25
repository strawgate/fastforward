#[cfg(test)]
mod validate_host_port_tests {
    use crate::validate::*;

    fn host_port_error(addr: &str) -> String {
        validate_host_port(addr).unwrap_err().to_string()
    }

    #[test]
    fn validate_host_port_works() {
        assert!(validate_host_port("127.0.0.1:4317").is_ok());
        assert!(validate_host_port("localhost:4317").is_ok());
        assert!(validate_host_port("my-host.internal:8080").is_ok());
        assert!(validate_host_port("[::1]:4317").is_ok());
        assert!(validate_host_port("[2001:db8::1]:80").is_ok());

        assert!(host_port_error(":4317").contains("empty host"));
        assert!(host_port_error("http://localhost:4317").contains("URL"));
        assert!(host_port_error("https://localhost:4317").contains("URL"));
        assert!(host_port_error("foo:bar:4317").contains("multiple colons"));
        assert!(host_port_error("localhost").contains("missing a port"));
        assert!(host_port_error("localhost:").contains("invalid port"));
        assert!(host_port_error("localhost:999999").contains("invalid port"));
        assert!(host_port_error("[::1]").contains("missing a port"));
        assert!(host_port_error("[::1]:").contains("invalid port"));
        // Empty IPv6 brackets — []:8080 has no host
        assert!(host_port_error("[]:8080").contains("empty"));
        // Double closing bracket — [::1]]:4317 is malformed
        assert!(host_port_error("[::1]]:4317").contains("missing a port"));
        // Path-like host rejected (#1461)
        assert!(host_port_error("foo/bar:4317").contains('/'));
        // Unmatched closing bracket rejected (#1461)
        assert!(host_port_error("foo]:4317").contains(']'));
        // Unmatched opening bracket rejected (#2060)
        assert!(host_port_error("foo[bar:4317").contains('['));
    }

    #[test]
    fn validate_bind_addr_works() {
        assert!(validate_bind_addr("127.0.0.1:4317").is_ok());
        assert!(validate_bind_addr("localhost:4317").is_ok());
        assert!(validate_bind_addr("[::1]:4317").is_ok());
        assert!(validate_bind_addr("http://localhost:4317").is_err());
    }
}
