//! Configuration structs — 1:1 mirror of Go `config_manager` package.
//!
//! These structs serialize/deserialize to the exact same JSON as the Go
//! binary. Field names, casing, and `omitempty` behavior must match.

use serde::{Deserialize, Serialize};

// ── Main Config ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub config_version: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub accepted_mints: Vec<MintConfig>,
    #[serde(default)]
    pub profit_share: Vec<ProfitShareConfig>,
    #[serde(default)]
    pub step_size: u64,
    #[serde(default)]
    pub margin: Option<f64>,
    #[serde(default = "default_metric")]
    pub metric: String,
    #[serde(default)]
    pub show_setup: bool,
    #[serde(default)]
    pub reseller_mode: bool,
    #[serde(default)]
    pub redirect_url: Option<String>,
    #[serde(default)]
    pub auth_delay_seconds: Option<i32>,
    #[serde(default)]
    pub upstream_detector: UpstreamDetectorConfig,
    #[serde(default)]
    pub upstream_session_manager: UpstreamSessionManagerConfig,
    #[serde(default)]
    pub upstream_wifi: UpstreamWifiConfig,
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_metric() -> String {
    "bytes".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self::new_default()
    }
}

impl Config {
    pub fn new_default() -> Self {
        Config {
            config_version: "v0.0.8".to_string(),
            log_level: "info".to_string(),
            accepted_mints: vec![MintConfig::default_production("https://mint.coinos.io")],
            profit_share: vec![
                ProfitShareConfig {
                    factor: 0.79,
                    identity: "owner".to_string(),
                },
                ProfitShareConfig {
                    factor: 0.07,
                    identity: "c08r4d0r".to_string(),
                },
                ProfitShareConfig {
                    factor: 0.07,
                    identity: "amperstrand".to_string(),
                },
                ProfitShareConfig {
                    factor: 0.07,
                    identity: "origami74".to_string(),
                },
            ],
            step_size: 22020096, // 21 MiB
            margin: Some(0.1),
            metric: "bytes".to_string(),
            show_setup: true,
            reseller_mode: false,
            redirect_url: None,
            auth_delay_seconds: None,
            upstream_detector: UpstreamDetectorConfig::default(),
            upstream_session_manager: UpstreamSessionManagerConfig::default(),
            upstream_wifi: UpstreamWifiConfig::default(),
        }
    }

    /// Validate that profit_share factors sum to ~1.0.
    pub fn validate_profit_share(&self) -> Result<(), crate::error::ConfigError> {
        if self.profit_share.is_empty() {
            return Err(crate::error::ConfigError::Validation(
                "profit_share is empty: at least one entry required".to_string(),
            ));
        }
        let sum: f64 = self.profit_share.iter().map(|p| p.factor).sum();
        if (sum - 1.0).abs() > 1e-6 {
            return Err(crate::error::ConfigError::Validation(format!(
                "profit_share factors must sum to 1.0, got {} ({:.1}% will remain in wallet each payout cycle)",
                sum,
                (1.0 - sum) * 100.0
            )));
        }
        Ok(())
    }

    /// Comprehensive validation of all config fields.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if let Err(e) = self.validate_profit_share() {
            errors.push(e.to_string());
        }

        if self.accepted_mints.is_empty() {
            errors.push("accepted_mints is empty: at least one mint required".to_string());
        }

        if self.metric != "bytes" && self.metric != "milliseconds" {
            errors.push(format!(
                "metric must be 'bytes' or 'milliseconds', got '{}'",
                self.metric
            ));
        }

        if self.step_size == 0 {
            errors.push("step_size must be greater than 0".to_string());
        }

        for (i, mint) in self.accepted_mints.iter().enumerate() {
            if mint.url.is_empty() {
                errors.push(format!("accepted_mints[{i}].url is empty"));
            }
            if !mint.url.starts_with("https://") && !mint.url.starts_with("http://") {
                errors.push(format!(
                    "accepted_mints[{i}].url must start with http:// or https://, got '{}'",
                    mint.url
                ));
            }
            if mint.price_unit != "sat" && mint.price_unit != "sats" {
                errors.push(format!(
                    "accepted_mints[{i}].price_unit must be 'sat' or 'sats', got '{}'",
                    mint.price_unit
                ));
            }
        }

        if let Some(margin) = self.margin {
            if !(0.0..=1.0).contains(&margin) {
                errors.push(format!(
                    "margin must be between 0.0 and 1.0, got {}",
                    margin
                ));
            }
        }

        for (i, ps) in self.profit_share.iter().enumerate() {
            if ps.factor < 0.0 || ps.factor > 1.0 {
                errors.push(format!(
                    "profit_share[{i}].factor must be between 0.0 and 1.0, got {}",
                    ps.factor
                ));
            }
            if ps.identity.is_empty() {
                errors.push(format!("profit_share[{i}].identity is empty"));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Ensure config_version is current. Returns true if the version was updated.
    pub fn ensure_current_version(&mut self) -> bool {
        const CURRENT_VERSION: &str = "v0.0.8";
        if self.config_version != CURRENT_VERSION {
            tracing::info!(
                old = %self.config_version,
                new = CURRENT_VERSION,
                "migrating config version"
            );
            self.config_version = CURRENT_VERSION.to_string();
            true
        } else {
            false
        }
    }

    /// Ensure the config has sensible defaults for missing critical fields.
    /// Returns true if any changes were made.
    pub fn ensure_defaults(&mut self) -> bool {
        let mut changed = false;

        if self.accepted_mints.is_empty() {
            self.accepted_mints = vec![MintConfig::default_production("https://mint.coinos.io")];
            changed = true;
        }

        if self.profit_share.is_empty() {
            self.profit_share = Config::new_default().profit_share;
            changed = true;
        }

        if self.step_size == 0 {
            self.step_size = 22020096;
            changed = true;
        }

        if self.log_level.is_empty() {
            self.log_level = "info".to_string();
            changed = true;
        }

        if self.metric.is_empty() {
            self.metric = "bytes".to_string();
            changed = true;
        }

        if self.ensure_current_version() {
            changed = true;
        }

        changed
    }
}

// ── MintConfig ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MintConfig {
    pub url: String,
    #[serde(default)]
    pub min_balance: u64,
    #[serde(default)]
    pub balance_tolerance_percent: u64,
    #[serde(default)]
    pub payout_interval_seconds: u64,
    #[serde(default)]
    pub min_payout_amount: u64,
    #[serde(default = "default_price_per_step")]
    pub price_per_step: u64,
    #[serde(default = "default_price_unit")]
    pub price_unit: String,
    #[serde(default, rename = "purchase_min_steps")]
    pub min_purchase_steps: u64,
}

fn default_price_per_step() -> u64 {
    1
}
fn default_price_unit() -> String {
    "sats".to_string()
}

impl MintConfig {
    pub fn default_production(url: &str) -> Self {
        MintConfig {
            url: url.to_string(),
            min_balance: 64,
            balance_tolerance_percent: 10,
            payout_interval_seconds: 60,
            min_payout_amount: 128,
            price_per_step: 1,
            price_unit: "sats".to_string(),
            min_purchase_steps: 0,
        }
    }
}

// ── ProfitShareConfig ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProfitShareConfig {
    pub factor: f64,
    pub identity: String,
}

// ── UpstreamDetectorConfig ───────────────────────────────────────────
/// Duration fields use Go's string format ("10s", "2s", "5m0s").
/// We keep them as strings for 1:1 JSON compat.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamDetectorConfig {
    #[serde(default = "default_probe_timeout")]
    pub probe_timeout: String,
    #[serde(default = "default_probe_retry_count")]
    pub probe_retry_count: i32,
    #[serde(default = "default_probe_retry_delay")]
    pub probe_retry_delay: String,
    #[serde(default)]
    pub require_valid_signature: bool,
    #[serde(default = "default_ignore_interfaces")]
    pub ignore_interfaces: Vec<String>,
    #[serde(default)]
    pub only_interfaces: Vec<String>,
    #[serde(default = "default_discovery_timeout")]
    pub discovery_timeout: String,
}

fn default_probe_timeout() -> String {
    "10s".to_string()
}
fn default_probe_retry_count() -> i32 {
    3
}
fn default_probe_retry_delay() -> String {
    "2s".to_string()
}
fn default_ignore_interfaces() -> Vec<String> {
    vec![
        "lo".into(),
        "docker0".into(),
        "br-lan".into(),
        "hostap0".into(),
    ]
}
fn default_discovery_timeout() -> String {
    "5m0s".to_string()
}

impl Default for UpstreamDetectorConfig {
    fn default() -> Self {
        UpstreamDetectorConfig {
            probe_timeout: default_probe_timeout(),
            probe_retry_count: default_probe_retry_count(),
            probe_retry_delay: default_probe_retry_delay(),
            require_valid_signature: true,
            ignore_interfaces: default_ignore_interfaces(),
            only_interfaces: vec![],
            discovery_timeout: default_discovery_timeout(),
        }
    }
}

// ── UpstreamSessionManagerConfig ─────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamSessionManagerConfig {
    #[serde(default)]
    pub max_price_per_millisecond: f64,
    #[serde(default)]
    pub max_price_per_byte: f64,
    #[serde(default)]
    pub trust: TrustConfig,
    #[serde(default)]
    pub sessions: SessionConfig,
    #[serde(default)]
    pub usage_tracking: UsageTrackingConfig,
}

impl Default for UpstreamSessionManagerConfig {
    fn default() -> Self {
        UpstreamSessionManagerConfig {
            max_price_per_millisecond: 0.002777777778,
            max_price_per_byte: 0.00003725782414,
            trust: TrustConfig::default(),
            sessions: SessionConfig::default(),
            usage_tracking: UsageTrackingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustConfig {
    #[serde(default = "default_trust_policy")]
    pub default_policy: String,
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub blocklist: Vec<String>,
}

fn default_trust_policy() -> String {
    "trust_all".to_string()
}

impl Default for TrustConfig {
    fn default() -> Self {
        TrustConfig {
            default_policy: default_trust_policy(),
            allowlist: vec![],
            blocklist: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    #[serde(default = "default_session_inc_ms")]
    pub preferred_session_increments_milliseconds: u64,
    #[serde(default = "default_session_inc_bytes")]
    pub preferred_session_increments_bytes: u64,
    #[serde(default = "default_ms_renewal_offset")]
    pub millisecond_renewal_offset: u64,
    #[serde(default = "default_bytes_renewal_offset")]
    pub bytes_renewal_offset: u64,
}

fn default_session_inc_ms() -> u64 {
    60000
}
fn default_session_inc_bytes() -> u64 {
    131100000
}
fn default_ms_renewal_offset() -> u64 {
    10000
}
fn default_bytes_renewal_offset() -> u64 {
    131100000
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            preferred_session_increments_milliseconds: default_session_inc_ms(),
            preferred_session_increments_bytes: default_session_inc_bytes(),
            millisecond_renewal_offset: default_ms_renewal_offset(),
            bytes_renewal_offset: default_bytes_renewal_offset(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageTrackingConfig {
    #[serde(default = "default_data_monitor_interval")]
    pub data_monitoring_interval: String,
}

fn default_data_monitor_interval() -> String {
    "0.5s".to_string()
}

impl Default for UsageTrackingConfig {
    fn default() -> Self {
        UsageTrackingConfig {
            data_monitoring_interval: default_data_monitor_interval(),
        }
    }
}

// ── UpstreamWifiConfig ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamWifiConfig {
    #[serde(default = "default_scan_interval")]
    pub scan_interval_seconds: i32,
    #[serde(default = "default_fast_check")]
    pub fast_check_seconds: i32,
    #[serde(default = "default_lost_threshold")]
    pub lost_threshold: i32,
    #[serde(default = "default_hysteresis_db")]
    pub hysteresis_db: i32,
    #[serde(default = "default_signal_floor")]
    pub signal_floor: i32,
    #[serde(default = "default_blacklist_ttl")]
    pub blacklist_ttl_minutes: i32,
    #[serde(default = "default_emergency_penalty")]
    pub emergency_penalty: i32,
    #[serde(default = "default_max_failures")]
    pub max_consecutive_failures: i32,
    #[serde(default = "default_switch_cooldown")]
    pub switch_cooldown_minutes: i32,
    #[serde(default = "default_startup_grace")]
    pub startup_grace_seconds: i32,
    #[serde(default = "default_post_switch_wait")]
    pub post_switch_wait_seconds: i32,
    #[serde(default = "default_dhcp_timeout")]
    pub dhcp_timeout_seconds: i32,
    #[serde(default = "default_manual_pause")]
    pub manual_pause_seconds: i32,
}

fn default_scan_interval() -> i32 {
    300
}
fn default_fast_check() -> i32 {
    30
}
fn default_lost_threshold() -> i32 {
    2
}
fn default_hysteresis_db() -> i32 {
    12
}
fn default_signal_floor() -> i32 {
    -85
}
fn default_blacklist_ttl() -> i32 {
    60
}
fn default_emergency_penalty() -> i32 {
    20
}
fn default_max_failures() -> i32 {
    3
}
fn default_switch_cooldown() -> i32 {
    10
}
fn default_startup_grace() -> i32 {
    90
}
fn default_post_switch_wait() -> i32 {
    5
}
fn default_dhcp_timeout() -> i32 {
    180
}
fn default_manual_pause() -> i32 {
    120
}

impl Default for UpstreamWifiConfig {
    fn default() -> Self {
        UpstreamWifiConfig {
            scan_interval_seconds: default_scan_interval(),
            fast_check_seconds: default_fast_check(),
            lost_threshold: default_lost_threshold(),
            hysteresis_db: default_hysteresis_db(),
            signal_floor: default_signal_floor(),
            blacklist_ttl_minutes: default_blacklist_ttl(),
            emergency_penalty: default_emergency_penalty(),
            max_consecutive_failures: default_max_failures(),
            switch_cooldown_minutes: default_switch_cooldown(),
            startup_grace_seconds: default_startup_grace(),
            post_switch_wait_seconds: default_post_switch_wait(),
            dhcp_timeout_seconds: default_dhcp_timeout(),
            manual_pause_seconds: default_manual_pause(),
        }
    }
}

// ── IdentitiesConfig ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentitiesConfig {
    #[serde(default)]
    pub config_version: String,
    #[serde(default)]
    pub owned_identities: Vec<OwnedIdentity>,
    #[serde(default)]
    pub public_identities: Vec<PublicIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OwnedIdentity {
    pub name: String,
    #[serde(rename = "privatekey")]
    pub privatekey: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublicIdentity {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pubkey: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lightning_address: Option<String>,
}

// ── JSON schema (mirrors Go config_manager/config_schema.go) ────────

fn field(
    name: &str,
    json_key: &str,
    ftype: &str,
    description: &str,
    default: serde_json::Value,
    required: bool,
    editable: bool,
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "json_key": json_key,
        "type": ftype,
        "description": description,
        "default": default,
        "required": required,
        "editable": editable,
    })
}

fn with_children(mut v: serde_json::Value, children: Vec<serde_json::Value>) -> serde_json::Value {
    v["children"] = serde_json::Value::Array(children);
    v
}

fn str_children() -> Vec<serde_json::Value> {
    vec![serde_json::json!({ "type": "string" })]
}

/// Field-schema description of `config.json`, mirroring Go's
/// `GetConfigSchema()`. Defaults are this implementation's actual defaults
/// (they diverge from the Go table where the two repos intentionally
/// differ, e.g. byte step sizes).
pub fn config_schema() -> serde_json::Value {
    serde_json::Value::Array(vec![
        field("ConfigVersion", "config_version", "string", "Configuration file version", "v0.0.8".into(), true, false),
        field("LogLevel", "log_level", "string", "Logging verbosity", "info".into(), true, true),
        field("Metric", "metric", "string", "Metering metric type", "bytes".into(), true, true),
        field("StepSize", "step_size", "uint64", "Step size in bytes (if metric=bytes) or milliseconds (if metric=milliseconds)", 22020096u64.into(), true, true),
        field("Margin", "margin", "float64", "Margin factor (0.0-1.0)", 0.1f64.into(), false, true),
        field("ShowSetup", "show_setup", "bool", "Show setup wizard on first access", true.into(), true, true),
        field("ResellerMode", "reseller_mode", "bool", "Enable reseller mode for upstream gateway discovery", false.into(), true, true),
        field("AuthDelaySeconds", "auth_delay_seconds", "int", "Delay in seconds before authorizing MAC after payment (0 = immediate)", 0.into(), false, true),
        field("RedirectURL", "redirect_url", "string", "URL to redirect clients to after payment (empty = no redirect)", "".into(), false, true),
        with_children(
            field("AcceptedMints", "accepted_mints", "array", "List of accepted Cashu mints", serde_json::Value::Null, true, true),
            vec![
                field("URL", "url", "string", "Mint URL", serde_json::Value::Null, true, true),
                field("MinBalance", "min_balance", "uint64", "Minimum balance before auto-replenish (sats)", 64u64.into(), true, true),
                field("BalanceTolerancePercent", "balance_tolerance_percent", "uint64", "Tolerance percentage for balance checks", 10u64.into(), true, true),
                field("PayoutIntervalSeconds", "payout_interval_seconds", "uint64", "Seconds between payout rounds", 60u64.into(), true, true),
                field("MinPayoutAmount", "min_payout_amount", "uint64", "Minimum payout amount in sats", 128u64.into(), true, true),
                field("PricePerStep", "price_per_step", "uint64", "Price per step in sats", 1u64.into(), true, true),
                field("PriceUnit", "price_unit", "string", "Price unit", "sats".into(), true, true),
                field("MinPurchaseSteps", "purchase_min_steps", "uint64", "Minimum number of steps per purchase", 0u64.into(), true, true),
            ],
        ),
        with_children(
            field("ProfitShare", "profit_share", "array", "Profit sharing configuration", serde_json::Value::Null, true, true),
            vec![
                field("Factor", "factor", "float64", "Share ratio (0.0\u{2013}1.0). All factors MUST sum to 1.0. Use 0.79 not 79\u{2014}this is a ratio, not a percentage.", serde_json::Value::Null, true, true),
                field("Identity", "identity", "string", "Identity name from identities.json", serde_json::Value::Null, true, true),
            ],
        ),
        with_children(
            field("UpstreamDetector", "upstream_detector", "object", "Upstream gateway detector configuration", serde_json::Value::Null, true, true),
            vec![
                field("ProbeTimeout", "probe_timeout", "duration", "Timeout for each probe", "10s".into(), true, true),
                field("ProbeRetryCount", "probe_retry_count", "int", "Number of probe retries", 3.into(), true, true),
                field("ProbeRetryDelay", "probe_retry_delay", "duration", "Delay between retries", "2s".into(), true, true),
                field("RequireValidSignature", "require_valid_signature", "bool", "Require valid NIP-70 signature", true.into(), true, true),
                with_children(field("IgnoreInterfaces", "ignore_interfaces", "array", "Interfaces to ignore", serde_json::json!(["lo", "docker0", "br-lan", "hostap0"]), false, true), str_children()),
                with_children(field("OnlyInterfaces", "only_interfaces", "array", "Only probe these interfaces (empty = all)", serde_json::json!([]), false, true), str_children()),
                field("DiscoveryTimeout", "discovery_timeout", "duration", "Deduplication window", "5m0s".into(), true, true),
            ],
        ),
        with_children(
            field("UpstreamSessionManager", "upstream_session_manager", "object", "Upstream session manager configuration", serde_json::Value::Null, true, true),
            vec![
                field("MaxPricePerMillisecond", "max_price_per_millisecond", "float64", "Max sats per millisecond", 0.002777777778f64.into(), true, true),
                field("MaxPricePerByte", "max_price_per_byte", "float64", "Max sats per byte", 0.00003725782414f64.into(), true, true),
                with_children(
                    field("Trust", "trust", "object", "Trust policy", serde_json::Value::Null, true, true),
                    vec![
                        field("DefaultPolicy", "default_policy", "string", "Default trust policy", "trust_all".into(), true, true),
                        with_children(field("Allowlist", "allowlist", "array", "Trusted pubkeys", serde_json::json!([]), false, true), str_children()),
                        with_children(field("Blocklist", "blocklist", "array", "Blocked pubkeys", serde_json::json!([]), false, true), str_children()),
                    ],
                ),
                with_children(
                    field("Sessions", "sessions", "object", "Session settings", serde_json::Value::Null, true, true),
                    vec![
                        field("PreferredSessionIncrementsMilliseconds", "preferred_session_increments_milliseconds", "uint64", "Preferred time session increment (ms)", 60000u64.into(), true, true),
                        field("PreferredSessionIncrementsBytes", "preferred_session_increments_bytes", "uint64", "Preferred data session increment (bytes)", 131100000u64.into(), true, true),
                        field("MillisecondRenewalOffset", "millisecond_renewal_offset", "uint64", "Renew this many ms before expiry", 10000u64.into(), true, true),
                        field("BytesRenewalOffset", "bytes_renewal_offset", "uint64", "Renew this many bytes before limit", 131100000u64.into(), true, true),
                    ],
                ),
                with_children(
                    field("UsageTracking", "usage_tracking", "object", "Usage tracking settings", serde_json::Value::Null, true, true),
                    vec![
                        field("DataMonitoringInterval", "data_monitoring_interval", "duration", "How often to check data usage", "0.5s".into(), true, true),
                    ],
                ),
            ],
        ),
        with_children(
            field("UpstreamWifi", "upstream_wifi", "object", "Upstream WiFi scanning and selection configuration", serde_json::Value::Null, true, true),
            vec![
                field("ScanIntervalSeconds", "scan_interval_seconds", "int", "Seconds between full WiFi scans", 300.into(), true, true),
                field("FastCheckSeconds", "fast_check_seconds", "int", "Seconds between fast signal checks", 30.into(), true, true),
                field("LostThreshold", "lost_threshold", "int", "Consecutive fast-check failures before marking as lost", 2.into(), true, true),
                field("HysteresisDB", "hysteresis_db", "int", "Signal hysteresis in dB to prevent flapping", 12.into(), true, true),
                field("SignalFloor", "signal_floor", "int", "Minimum signal strength in dBm to consider a network usable", (-85).into(), true, true),
                field("BlacklistTTLMinutes", "blacklist_ttl_minutes", "int", "Minutes before a blacklisted network is retried", 60.into(), true, true),
                field("EmergencyPenalty", "emergency_penalty", "int", "Penalty score added on emergency disconnect", 20.into(), true, true),
                field("MaxConsecutiveFailures", "max_consecutive_failures", "int", "Consecutive failures before emergency scan", 3.into(), true, true),
                field("SwitchCooldownMinutes", "switch_cooldown_minutes", "int", "Minimum minutes between network switches", 10.into(), true, true),
                field("StartupGraceSeconds", "startup_grace_seconds", "int", "Grace period on startup before scoring", 90.into(), true, true),
                field("PostSwitchWaitSeconds", "post_switch_wait_seconds", "int", "Seconds to wait after a switch before scoring", 5.into(), true, true),
                field("DHCPTimeoutSeconds", "dhcp_timeout_seconds", "int", "Timeout for DHCP after connecting to a network", 180.into(), true, true),
                field("ManualPauseSeconds", "manual_pause_seconds", "int", "Seconds to pause scanning after manual intervention", 120.into(), true, true),
            ],
        ),
    ])
}

/// Field-schema description of `identities.json`, mirroring Go's
/// `GetIdentitiesSchema()`.
pub fn identities_schema() -> serde_json::Value {
    serde_json::Value::Array(vec![
        field("ConfigVersion", "config_version", "string", "Identities file version", "v0.0.1".into(), true, false),
        with_children(
            field("OwnedIdentities", "owned_identities", "array", "Identities with private keys (managed by the system)", serde_json::Value::Null, true, false),
            vec![
                field("Name", "name", "string", "Identity name", serde_json::Value::Null, true, false),
                field("PrivateKey", "privatekey", "string", "Nostr private key (sensitive)", serde_json::Value::Null, true, false),
            ],
        ),
        with_children(
            field("PublicIdentities", "public_identities", "array", "Public identities for profit sharing and trust", serde_json::Value::Null, true, true),
            vec![
                field("Name", "name", "string", "Identity name", serde_json::Value::Null, true, true),
                field("PubKey", "pubkey", "string", "Nostr public key \u{2014} not currently used for payouts (lightning_address is used instead)", serde_json::Value::Null, false, true),
                field("LightningAddress", "lightning_address", "string", "Lightning address for payouts", serde_json::Value::Null, false, true),
            ],
        ),
    ])
}

// ── InstallConfig (install.json) ─────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstallConfig {
    #[serde(default)]
    pub config_version: String,
    #[serde(default)]
    pub package_path: String,
    #[serde(default)]
    pub ip_address_randomized: bool,
    #[serde(default)]
    pub install_time: u64,
    #[serde(default)]
    pub download_time: u64,
    #[serde(default)]
    pub release_channel: String,
    #[serde(default)]
    pub ensure_default_timestamp: u64,
    #[serde(default)]
    pub installed_version: String,
}
