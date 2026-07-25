//! Gateway prober — HTTP probe to check if a gateway is a TollGate.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct DiscoveryEvent {
    kind: u64,
    content: String,
    tags: Vec<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct GatewayInfo {
    pub metric: String,
    pub step_size: u64,
    pub price_per_step: u64,
    pub mint_url: String,
}

pub struct GatewayProber {
    client: reqwest::Client,
}

impl GatewayProber {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        GatewayProber { client }
    }

    /// Probe a gateway IP to check if it's a TollGate.
    /// A TollGate responds on port 2121 with a Nostr kind 10021 discovery event.
    pub async fn probe(&self, gateway_ip: &str) -> Result<GatewayInfo, String> {
        let url = format!("http://{gateway_ip}:2121/");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("probe {url}: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("probe {url}: HTTP {}", resp.status()));
        }

        let event: DiscoveryEvent = resp
            .json()
            .await
            .map_err(|e| format!("parse discovery event: {e}"))?;

        if event.kind != 10021 {
            return Err(format!("not a TollGate (kind={}, expected 10021)", event.kind));
        }

        let mut info = GatewayInfo {
            metric: "bytes".to_string(),
            step_size: 0,
            price_per_step: 0,
            mint_url: String::new(),
        };

        for tag in &event.tags {
            if tag.len() < 2 {
                continue;
            }
            match tag[0].as_str() {
                "metric" => info.metric = tag[1].clone(),
                "step_size" => {
                    info.step_size = tag[1].parse().unwrap_or(0);
                }
                "price_per_step" => {
                    if tag.len() >= 5 {
                        info.price_per_step = tag[2].parse().unwrap_or(0);
                        info.mint_url = tag[4].clone();
                    }
                }
                _ => {}
            }
        }

        if info.mint_url.is_empty() {
            return Err("no mint URL in discovery event".to_string());
        }

        Ok(info)
    }
}

impl Default for GatewayProber {
    fn default() -> Self {
        Self::new()
    }
}
