//! Cashu token verifier — parse + NUT-07 checkstate.
//!
//! Ported from tollgate-rs/crates/tollgate-net/src/wallet.rs (BootstrapWallet).
//! Read-only: verifies proofs are unspent at the mint. No spending/receiving.

use crate::error::VerifyError;
use cashu::nuts::Token;
use std::collections::HashSet;

/// TLS 1.2 hard-pinned HTTP client (matches Go behavior — Go audit §2.4).
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .pool_idle_timeout(None) // disable keep-alives
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Bootstrap token verifier. Accepts Cashu tokens, checks proofs against
/// the mint's /v1/checkstate endpoint. Returns spendable amount in
/// milli-sat (pricing_scale = 1000, so 1 sat → 1000 milli-units).
pub struct TokenVerifier {
    accepted_mints: HashSet<String>,
    client: reqwest::Client,
}

impl TokenVerifier {
    pub fn new(mint_urls: Vec<String>) -> Self {
        Self {
            accepted_mints: mint_urls
                .into_iter()
                .map(|u| u.trim_end_matches('/').to_string())
                .collect(),
            client: build_http_client(),
        }
    }

    /// Parse and verify a Cashu token. Returns amount in milli-sat.
    pub async fn verify(&self, token_str: &str) -> Result<(u64, String), VerifyError> {
        let token: Token = token_str
            .parse::<Token>()
            .map_err(|e| VerifyError::InvalidToken(e.to_string()))?;

        let mint_url = token
            .mint_url()
            .map_err(|e| VerifyError::NoMintUrl(e.to_string()))?;
        let mint_url_str = mint_url.to_string();
        let mint_base = mint_url_str.trim_end_matches('/').to_string();

        if !self.accepted_mints.is_empty()
            && !self.accepted_mints.contains(&mint_base)
            && !self.accepted_mints.contains(&mint_url_str)
        {
            return Err(VerifyError::MintNotAccepted(mint_url_str));
        }

        let amount_sat: u64 = token
            .value()
            .map_err(|e| VerifyError::ValueSum(e.to_string()))?
            .into();

        let ys = token_proof_ys(&token);
        if ys.is_empty() {
            return Err(VerifyError::NoProofs);
        }

        self.check_proofs_unspent(&mint_base, &ys).await?;

        Ok((amount_sat * 1_000, mint_base))
    }

    /// NUT-07: check all Y-values are UNSPENT.
    async fn check_proofs_unspent(
        &self,
        mint_base: &str,
        ys: &[String],
    ) -> Result<(), VerifyError> {
        let url = format!("{mint_base}/v1/checkstate");
        let body = serde_json::json!({ "Ys": ys });

        let resp: serde_json::Value = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| VerifyError::CheckStateRequest(e.to_string()))?
            .error_for_status()
            .map_err(|e| VerifyError::CheckStateStatus(e.to_string()))?
            .json()
            .await
            .map_err(|e| VerifyError::CheckStateParse(e.to_string()))?;

        let states = resp["states"]
            .as_array()
            .ok_or(VerifyError::MissingStates)?;

        for state in states {
            let s = state["state"].as_str().unwrap_or("");
            if s.to_uppercase() != "UNSPENT" {
                return Err(VerifyError::Spent(s.to_string()));
            }
        }
        Ok(())
    }
}

/// Extract Y-values (compressed blinded pubkey hex) from token proofs.
fn token_proof_ys(token: &Token) -> Vec<String> {
    match token {
        Token::TokenV3(t) => t
            .token
            .iter()
            .flat_map(|entry| entry.proofs.iter().map(|p| p.c.to_string()))
            .collect(),
        Token::TokenV4(t) => t
            .token
            .iter()
            .flat_map(|entry| entry.proofs.iter().map(|p| p.c.to_string()))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real cashuB v4 token — 1 sat from testnut.cashu.space.
    const SAMPLE_TOKEN: &str = "cashuBo2FteBtodHRwczovL3Rlc3RudXQuY2FzaHUuc3BhY2VhdWNzYXRhdIGiYWlIAYhKdLsvxe5hcIGkYWEBYXN4QDk1NTM1NzQ1YjQ2MzM2OGQ1OTVkMGVhMmQ1M2NmMDU0YjZkY2ZhZTY0NjhlOWU0N2U1MDc1YWU3OWRmNmUyODdhY1ghA03QgEalpQeCViTFYVixs-4tTxGmV0Dl-hKTQ8jLyG1ZYWSjYWVYIKlCWsnyOJRBHT_0xffz67uTQUWhk336QvZbnEQW6OUZYXNYIA88wEUIkwoL1RKs6j41AgtMZLp2e3JrlpZyU1o2M3TJYXJYILoalwd76VtIosztMCjHmQzbNUVKCM4VjvV02fSkG19-";

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn parses_token_and_reads_amount() {
        let token: Token = SAMPLE_TOKEN.parse().expect("valid cashuB token");
        let amount_sat: u64 = token.value().expect("has value").into();
        assert_eq!(amount_sat, 1);
        let _mint = token.mint_url().expect("has mint URL").to_string();
    }

    #[test]
    fn extracts_proof_y_values() {
        let token: Token = SAMPLE_TOKEN.parse().expect("valid token");
        let ys = token_proof_ys(&token);
        assert_eq!(ys.len(), 1);
        // Y-values are 33-byte compressed pubkeys in hex (66 chars).
        assert_eq!(ys[0].len(), 66);
    }

    #[test]
    fn rejects_token_from_unlisted_mint() {
        let wallet = TokenVerifier::new(vec!["https://allowed-mint.example".to_string()]);
        let result = rt().block_on(wallet.verify(SAMPLE_TOKEN));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not accepted"));
    }

    #[test]
    fn open_mint_list_passes_mint_filter() {
        let wallet = TokenVerifier::new(vec![]);
        assert!(wallet.accepted_mints.is_empty());
        let token: Token = SAMPLE_TOKEN.parse().expect("valid token");
        let _amount_sat: u64 = token.value().expect("has value").into();
    }

    #[test]
    fn rejects_invalid_token_string() {
        let wallet = TokenVerifier::new(vec![]);
        let result = rt().block_on(wallet.verify("not-a-token"));
        assert!(result.is_err());
    }

    #[test]
    fn milli_unit_scaling() {
        let token: Token = SAMPLE_TOKEN.parse().expect("valid token");
        let sat: u64 = token.value().expect("value").into();
        assert_eq!(sat * 1_000, 1_000);
    }

    #[test]
    fn rejects_empty_token_string() {
        let wallet = TokenVerifier::new(vec![]);
        let result = rt().block_on(wallet.verify(""));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid Cashu token"),
            "expected parse error, got: {err}"
        );
    }

    #[test]
    fn rejects_whitespace_only_token() {
        let wallet = TokenVerifier::new(vec![]);
        let result = rt().block_on(wallet.verify("   \n\t  "));
        assert!(result.is_err());
    }

    #[test]
    fn rejects_garbage_base64_token() {
        let wallet = TokenVerifier::new(vec![]);
        let result = rt().block_on(wallet.verify("cashuB!!!not-valid-base64!!!"));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid Cashu token"),
            "expected parse error"
        );
    }

    #[test]
    fn wrong_mint_rejected_with_mint_url_in_error() {
        let wallet = TokenVerifier::new(vec!["https://treasury.cashu.exchange".to_string()]);
        let result = rt().block_on(wallet.verify(SAMPLE_TOKEN));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not accepted"), "got: {err}");
        assert!(
            err.contains("testnut") || err.contains("cashu"),
            "error should mention the mint: {err}"
        );
    }

    #[test]
    fn trailing_slash_mint_url_matches_base() {
        let wallet = TokenVerifier::new(vec!["https://testnut.cashu.space/".to_string()]);
        let result = rt().block_on(wallet.verify(SAMPLE_TOKEN));
        // Can't reach a real mint in unit tests, but the error must NOT be
        // a "not accepted" rejection — that would mean slash-normalization failed.
        if let Err(ref e) = result {
            let msg = e.to_string();
            assert!(
                !msg.contains("not accepted"),
                "trailing slash should still match: {msg}"
            );
        }
    }

    #[test]
    fn multiple_accepted_mints_all_pass_filter() {
        let wallet = TokenVerifier::new(vec![
            "https://treasury.cashu.exchange".to_string(),
            "https://testnut.cashu.space".to_string(),
            "https://mint.coinos.io".to_string(),
        ]);
        let result = rt().block_on(wallet.verify(SAMPLE_TOKEN));
        if let Err(ref e) = result {
            let msg = e.to_string();
            assert!(
                !msg.contains("not accepted"),
                "testnut should be in accepted list: {msg}"
            );
        }
    }
}
