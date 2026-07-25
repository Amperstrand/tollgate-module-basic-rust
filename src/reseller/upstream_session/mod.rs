//! Upstream session — connects to an upstream TollGate, pays for access, tracks usage.

use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Response from the upstream TollGate after a successful payment.
#[derive(Debug, Clone, Deserialize)]
struct SessionGrantEvent {
    kind: u64,
    tags: Vec<Vec<String>>,
}

/// Result of a payment attempt to an upstream TollGate.
#[derive(Debug, Clone)]
pub struct UpstreamPaymentResult {
    pub success: bool,
    pub allotment: u64,
    pub metric: String,
    pub step_size: u64,
    pub expiry: u64,
    pub error: Option<String>,
}

/// An active upstream session — tracks the connection and usage.
#[derive(Debug, Clone)]
pub struct UpstreamSession {
    pub gateway_ip: String,
    pub gateway_interface: String,
    pub allotment: u64,
    pub used: u64,
    pub metric: String,
    pub step_size: u64,
    pub expiry: u64,
    pub renewal_threshold: f64,
}

impl UpstreamSession {
    pub fn new(gateway_ip: &str, interface: &str) -> Self {
        UpstreamSession {
            gateway_ip: gateway_ip.to_string(),
            gateway_interface: interface.to_string(),
            allotment: 0,
            used: 0,
            metric: "bytes".to_string(),
            step_size: 0,
            expiry: 0,
            renewal_threshold: 0.8,
        }
    }

    /// Check if usage has crossed the renewal threshold (default 80%).
    pub fn needs_renewal(&self) -> bool {
        if self.allotment == 0 {
            return false;
        }
        let usage_ratio = self.used as f64 / self.allotment as f64;
        usage_ratio >= self.renewal_threshold
    }

    /// Check if the session has expired.
    pub fn is_expired(&self) -> bool {
        if self.expiry == 0 {
            return true;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.expiry <= now
    }

    /// Check if the session is active (not expired, usage under allotment).
    pub fn is_active(&self) -> bool {
        !self.is_expired() && self.used < self.allotment
    }

    /// Calculate remaining allotment.
    pub fn remaining(&self) -> u64 {
        self.allotment.saturating_sub(self.used)
    }

    /// Update usage and return whether renewal is needed.
    pub fn update_usage(&mut self, used: u64) -> bool {
        self.used = used;
        self.needs_renewal()
    }

    /// Send a Cashu payment to the upstream TollGate.
    /// POSTs the token to http://<gateway>:2121/ and parses the response.
    pub async fn send_payment(
        &self,
        token: &str,
        client: &reqwest::Client,
    ) -> UpstreamPaymentResult {
        let url = format!("http://{}:2121/", self.gateway_ip);

        let resp = match client
            .post(&url)
            .header("Content-Type", "text/plain")
            .body(token.to_string())
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return UpstreamPaymentResult {
                    success: false,
                    allotment: 0,
                    metric: String::new(),
                    step_size: 0,
                    expiry: 0,
                    error: Some(format!("payment request failed: {e}")),
                };
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return UpstreamPaymentResult {
                success: false,
                allotment: 0,
                metric: String::new(),
                step_size: 0,
                expiry: 0,
                error: Some(format!("payment rejected: HTTP {status} — {body}")),
            };
        }

        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => {
                return UpstreamPaymentResult {
                    success: false,
                    allotment: 0,
                    metric: String::new(),
                    step_size: 0,
                    expiry: 0,
                    error: Some(format!("failed to read response: {e}")),
                };
            }
        };

        Self::parse_session_grant(&body)
    }

    /// Parse the Nostr kind 1022 session grant event from the payment response.
    pub fn parse_session_grant(body: &str) -> UpstreamPaymentResult {
        let event: SessionGrantEvent = match serde_json::from_str(body) {
            Ok(e) => e,
            Err(e) => {
                return UpstreamPaymentResult {
                    success: false,
                    allotment: 0,
                    metric: String::new(),
                    step_size: 0,
                    expiry: 0,
                    error: Some(format!("failed to parse session grant: {e}")),
                };
            }
        };

        if event.kind != 1022 {
            return UpstreamPaymentResult {
                success: false,
                allotment: 0,
                metric: String::new(),
                step_size: 0,
                expiry: 0,
                error: Some(format!("expected kind 1022, got {}", event.kind)),
            };
        }

        let mut result = UpstreamPaymentResult {
            success: true,
            allotment: 0,
            metric: "bytes".to_string(),
            step_size: 0,
            expiry: 0,
            error: None,
        };

        for tag in &event.tags {
            if tag.len() < 2 {
                continue;
            }
            match tag[0].as_str() {
                "allotment" => result.allotment = tag[1].parse().unwrap_or(0),
                "metric" => result.metric = tag[1].clone(),
                "step_size" => result.step_size = tag[1].parse().unwrap_or(0),
                "expiry" => result.expiry = tag[1].parse().unwrap_or(0),
                _ => {}
            }
        }

        if result.allotment == 0 {
            result.success = false;
            result.error = Some("no allotment in session grant".to_string());
        }

        result
    }

    /// Apply a successful payment result to this session.
    pub fn apply_payment(&mut self, result: &UpstreamPaymentResult) {
        if result.success {
            self.allotment = result.allotment;
            self.used = 0;
            self.metric = result.metric.clone();
            self.step_size = result.step_size;
            self.expiry = result.expiry;
        }
    }
}

#[cfg(test)]
mod tests;
