//! Canonical mint-URL form (Go `NormalizeMintURL` parity).
//!
//! One canonical form for every persisted or compared mint URL: scheme and
//! host lowercased, a single trailing slash collapsed, default ports
//! (http:80, https:443) dropped. Go's tollwallet applies the same rules, so
//! `HTTP://Mint.Example/` and `http://mint.example` are one mint — anything
//! less produces spurious wallet-not-found / mint-not-accepted refusals.

/// Canonicalize a mint URL for comparison and persistence.
pub fn canonicalize_mint_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }

    let (scheme, rest) = match trimmed.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => return trimmed.to_ascii_lowercase(),
    };

    // Split authority from path; drop default ports.
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, p),
        None => (rest, ""),
    };
    let authority = match authority.rsplit_once(':') {
        Some((host, port)) => {
            let default_port = matches!((scheme.as_str(), port), ("http", "80") | ("https", "443"));
            if default_port || port.is_empty() {
                host.to_ascii_lowercase()
            } else {
                format!("{}:{}", host.to_ascii_lowercase(), port)
            }
        }
        None => authority.to_ascii_lowercase(),
    };

    if path.is_empty() {
        format!("{scheme}://{authority}")
    } else {
        format!("{scheme}://{authority}/{path}")
    }
}

/// Compare two mint URLs in canonical form.
pub fn mint_urls_equal(a: &str, b: &str) -> bool {
    canonicalize_mint_url(a) == canonicalize_mint_url(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases_scheme_and_host() {
        assert_eq!(
            canonicalize_mint_url("HTTP://Mint.Example"),
            "http://mint.example"
        );
    }

    #[test]
    fn collapses_trailing_slashes() {
        assert_eq!(
            canonicalize_mint_url("http://mint.example///"),
            "http://mint.example"
        );
    }

    #[test]
    fn drops_default_ports() {
        assert_eq!(
            canonicalize_mint_url("http://mint.example:80"),
            "http://mint.example"
        );
        assert_eq!(
            canonicalize_mint_url("https://mint.example:443/x"),
            "https://mint.example/x"
        );
    }

    #[test]
    fn keeps_explicit_ports() {
        assert_eq!(
            canonicalize_mint_url("HTTP://10.99.99.2:8383/"),
            "http://10.99.99.2:8383"
        );
    }

    #[test]
    fn preserves_path() {
        assert_eq!(
            canonicalize_mint_url("https://mint.example/custom/",),
            "https://mint.example/custom"
        );
    }

    #[test]
    fn equality_across_forms() {
        assert!(mint_urls_equal(
            "HTTPS://Mint.Example:443/",
            "https://mint.example"
        ));
        assert!(mint_urls_equal(
            "http://10.99.99.2:8383/",
            "http://10.99.99.2:8383"
        ));
        assert!(!mint_urls_equal(
            "http://mint.example",
            "http://mint.example:8383"
        ));
    }

    #[test]
    fn empty_stays_empty() {
        assert_eq!(canonicalize_mint_url("   "), "");
    }

    #[test]
    fn no_scheme_lowercases_whole() {
        assert_eq!(canonicalize_mint_url("Mint.Example"), "mint.example");
    }

    /// Codex P2 on #55 (r4186082594) claimed `http://[::1]` canonicalizes
    /// to `http://[:::1]`. It does not: rsplit_once's reassembly always
    /// reconstructs the authority and the `]` keeps the default-port drop
    /// from firing on address components. These tests pin that invariant
    /// (IPv6 authorities stable, canonicalization idempotent) so a future
    /// rewrite cannot silently regress it.
    #[test]
    fn ipv6_authorities_stay_stable() {
        assert_eq!(canonicalize_mint_url("http://[::1]"), "http://[::1]");
        assert_eq!(
            canonicalize_mint_url("http://[::1]:8338/"),
            "http://[::1]:8338"
        );
        assert_eq!(
            canonicalize_mint_url("HTTP://[2001:DB8::1]"),
            "http://[2001:db8::1]"
        );
        assert_eq!(
            canonicalize_mint_url("http://[2001:db8::1]:3338/path/"),
            "http://[2001:db8::1]:3338/path"
        );
        // An explicit default port on a bracketed host drops like any
        // default port, leaving the address untouched.
        assert_eq!(canonicalize_mint_url("http://[::1]:80"), "http://[::1]");
    }

    #[test]
    fn canonicalization_is_idempotent() {
        for url in [
            "HTTP://Mint.Example/",
            "http://10.99.99.2:8383/",
            "http://[::1]",
            "http://[::1]:8338",
            "http://[2001:db8::1]:3338/path/",
            "https://mint.example:443/custom/",
        ] {
            let once = canonicalize_mint_url(url);
            assert_eq!(
                once,
                canonicalize_mint_url(&once),
                "canonical form of {url} must be a fixed point"
            );
        }
    }
}
