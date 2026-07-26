#[cfg(feature = "embedded-portal")]
use nftables::{
    batch::Batch,
    expr::{Expression, NamedExpression, Payload, PayloadField},
    schema::{self, NfListObject},
    stmt::{Counter as StmtCounter, Match, Operator, Statement},
    types::{NfChainPolicy, NfChainType, NfFamily, NfHook},
};

use std::net::IpAddr;

const TABLE_NAME: &str = "tollgate";
const SET_V4: &str = "authenticated_v4";
const SET_V6: &str = "authenticated_v6";

#[derive(Debug, thiserror::Error)]
pub enum NftError {
    #[error("nft apply failed: {0}")]
    Apply(String),
    #[error("nft read failed: {0}")]
    Read(String),
    #[error("counter not found for {0}")]
    CounterNotFound(String),
}

#[derive(Clone)]
pub struct NftManager {
    table: String,
}

impl Default for NftManager {
    fn default() -> Self {
        Self::new()
    }
}

impl NftManager {
    pub fn new() -> Self {
        NftManager {
            table: TABLE_NAME.to_string(),
        }
    }

    #[cfg(feature = "embedded-portal")]
    pub fn build_install_batch(&self) -> Batch<'static> {
        let mut batch = Batch::new();

        batch.add(NfListObject::Table(schema::Table {
            family: NfFamily::INet,
            name: self.table.clone().into(),
            handle: None,
        }));

        batch.add(NfListObject::Set(Box::new(schema::Set {
            family: NfFamily::INet,
            table: self.table.clone().into(),
            name: SET_V4.into(),
            set_type: schema::SetTypeValue::Single(schema::SetType::Ipv4Addr),
            ..Default::default()
        })));

        batch.add(NfListObject::Set(Box::new(schema::Set {
            family: NfFamily::INet,
            table: self.table.clone().into(),
            name: SET_V6.into(),
            set_type: schema::SetTypeValue::Single(schema::SetType::Ipv6Addr),
            ..Default::default()
        })));

        batch.add(NfListObject::Chain(schema::Chain {
            family: NfFamily::INet,
            table: self.table.clone().into(),
            name: "prerouting".into(),
            _type: Some(NfChainType::NAT),
            hook: Some(NfHook::Prerouting),
            prio: Some(-100),
            policy: Some(NfChainPolicy::Accept),
            ..Default::default()
        }));

        batch.add(NfListObject::Chain(schema::Chain {
            family: NfFamily::INet,
            table: self.table.clone().into(),
            name: "forward".into(),
            _type: Some(NfChainType::Filter),
            hook: Some(NfHook::Forward),
            prio: Some(0),
            policy: Some(NfChainPolicy::Drop),
            ..Default::default()
        }));

        self.add_forward_rules(&mut batch);
        batch
    }

    #[cfg(feature = "embedded-portal")]
    fn add_forward_rules(&self, batch: &mut Batch<'static>) {
        let table: std::borrow::Cow<'static, str> = self.table.clone().into();

        let accept_in_set =
            |batch: &mut Batch<'static>, proto: &'static str, field: &'static str, set: String| {
                batch.add(NfListObject::Rule(schema::Rule {
                    family: NfFamily::INet,
                    table: table.clone(),
                    chain: "forward".into(),
                    expr: vec![
                        Statement::Match(Match {
                            left: Expression::Named(NamedExpression::Payload(
                                Payload::PayloadField(PayloadField {
                                    protocol: proto.into(),
                                    field: field.into(),
                                }),
                            )),
                            right: Expression::String(set.into()),
                            op: Operator::IN,
                        }),
                        Statement::Accept(None),
                    ]
                    .into(),
                    ..Default::default()
                }));
            };

        accept_in_set(batch, "ip", "saddr", format!("@{SET_V4}"));
        accept_in_set(batch, "ip6", "saddr", format!("@{SET_V6}"));

        let accept_port = |batch: &mut Batch<'static>, proto: &'static str, port: u32| {
            batch.add(NfListObject::Rule(schema::Rule {
                family: NfFamily::INet,
                table: table.clone(),
                chain: "forward".into(),
                expr: vec![
                    Statement::Match(Match {
                        left: Expression::Named(NamedExpression::Payload(Payload::PayloadField(
                            PayloadField {
                                protocol: proto.into(),
                                field: "dport".into(),
                            },
                        ))),
                        right: Expression::Number(port),
                        op: Operator::EQ,
                    }),
                    Statement::Accept(None),
                ]
                .into(),
                ..Default::default()
            }));
        };

        accept_port(batch, "tcp", 53);
        accept_port(batch, "udp", 53);
        accept_port(batch, "tcp", 80);
    }

    #[cfg(feature = "embedded-portal")]
    pub fn build_add_client_batch(&self, ip: IpAddr) -> Batch<'static> {
        let mut batch = Batch::new();
        let (set_name, ip_str) = match ip {
            IpAddr::V4(v4) => (SET_V4, v4.to_string()),
            IpAddr::V6(v6) => (SET_V6, v6.to_string()),
        };
        batch.add(NfListObject::Element(schema::Element {
            family: NfFamily::INet,
            table: self.table.clone().into(),
            name: set_name.into(),
            elem: vec![Expression::String(ip_str.into())].into(),
        }));
        batch
    }

    #[cfg(feature = "embedded-portal")]
    pub fn build_remove_client_batch(&self, ip: IpAddr) -> Batch<'static> {
        let mut batch = Batch::new();
        let (set_name, ip_str) = match ip {
            IpAddr::V4(v4) => (SET_V4, v4.to_string()),
            IpAddr::V6(v6) => (SET_V6, v6.to_string()),
        };
        batch.delete(NfListObject::Element(schema::Element {
            family: NfFamily::INet,
            table: self.table.clone().into(),
            name: set_name.into(),
            elem: vec![Expression::String(ip_str.into())].into(),
        }));
        batch
    }

    pub fn counter_name(ip: &IpAddr) -> String {
        match ip {
            IpAddr::V4(v4) => format!("c-{v4}"),
            IpAddr::V6(v6) => format!("c6-{v6}"),
        }
    }

    #[cfg(feature = "embedded-portal")]
    pub fn build_create_counter_batch(&self, ip: IpAddr) -> Batch<'static> {
        let mut batch = Batch::new();
        batch.add(NfListObject::Counter(schema::Counter {
            family: NfFamily::INet,
            table: self.table.clone().into(),
            name: Self::counter_name(&ip).into(),
            ..Default::default()
        }));
        batch
    }

    #[cfg(feature = "embedded-portal")]
    pub fn build_delete_counter_batch(&self, ip: &IpAddr) -> Batch<'static> {
        let mut batch = Batch::new();
        batch.delete(NfListObject::Counter(schema::Counter {
            family: NfFamily::INet,
            table: self.table.clone().into(),
            name: Self::counter_name(ip).into(),
            ..Default::default()
        }));
        batch
    }

    #[cfg(feature = "embedded-portal")]
    pub fn build_teardown_batch(&self) -> Batch<'static> {
        let mut batch = Batch::new();
        batch.delete(NfListObject::Table(schema::Table {
            family: NfFamily::INet,
            name: self.table.clone().into(),
            handle: None,
        }));
        batch
    }

    #[cfg(feature = "embedded-portal")]
    fn apply(batch: Batch<'static>) -> Result<(), NftError> {
        let ruleset = batch.to_nftables();
        nftables::helper::apply_ruleset(&ruleset).map_err(|e| NftError::Apply(e.to_string()))
    }

    #[cfg(feature = "embedded-portal")]
    pub fn install(&self) -> Result<(), NftError> {
        Self::apply(self.build_install_batch())
    }

    #[cfg(feature = "embedded-portal")]
    pub fn teardown(&self) -> Result<(), NftError> {
        Self::apply(self.build_teardown_batch())
    }

    #[cfg(feature = "embedded-portal")]
    pub fn add_client(&self, ip: IpAddr) -> Result<(), NftError> {
        Self::apply(self.build_add_client_batch(ip))
    }

    #[cfg(feature = "embedded-portal")]
    pub fn remove_client(&self, ip: IpAddr) -> Result<(), NftError> {
        Self::apply(self.build_remove_client_batch(ip))
    }

    #[cfg(feature = "embedded-portal")]
    pub fn create_counter(&self, ip: IpAddr) -> Result<(), NftError> {
        Self::apply(self.build_create_counter_batch(ip))
    }

    #[cfg(feature = "embedded-portal")]
    pub fn delete_counter(&self, ip: &IpAddr) -> Result<(), NftError> {
        Self::apply(self.build_delete_counter_batch(ip))
    }

    #[cfg(feature = "embedded-portal")]
    pub fn poll_counter(&self, ip: &IpAddr) -> Result<(u64, u64), NftError> {
        let json = nftables::helper::get_current_ruleset_raw(
            nftables::helper::DEFAULT_NFT,
            nftables::helper::DEFAULT_ARGS,
        )
        .map_err(|e| NftError::Read(e.to_string()))?;
        let parsed: serde_json::Value =
            serde_json::from_str(&json).map_err(|e| NftError::Read(e.to_string()))?;

        let target = Self::counter_name(ip);
        if let Some(objects) = parsed.get("nftables").and_then(|v| v.as_array()) {
            for obj in objects {
                if let Some(counter) = obj.get("counter") {
                    if counter.get("name").and_then(|n| n.as_str()) == Some(&target)
                        && counter.get("table").and_then(|t| t.as_str()) == Some(&self.table)
                    {
                        let packets = counter.get("packets").and_then(|p| p.as_u64()).unwrap_or(0);
                        let bytes = counter.get("bytes").and_then(|b| b.as_u64()).unwrap_or(0);
                        return Ok((packets, bytes));
                    }
                }
            }
        }
        Err(NftError::CounterNotFound(target))
    }

    #[cfg(feature = "embedded-portal")]
    pub fn build_counter_rule_batch(&self, ip: IpAddr) -> Batch<'static> {
        let mut batch = Batch::new();
        let (proto, ip_str) = match ip {
            IpAddr::V4(v4) => ("ip", v4.to_string()),
            IpAddr::V6(v6) => ("ip6", v6.to_string()),
        };
        let counter = Self::counter_name(&ip);

        batch.add(NfListObject::Rule(schema::Rule {
            family: NfFamily::INet,
            table: self.table.clone().into(),
            chain: "forward".into(),
            expr: vec![
                Statement::Match(Match {
                    left: Expression::Named(NamedExpression::Payload(Payload::PayloadField(
                        PayloadField {
                            protocol: proto.into(),
                            field: "saddr".into(),
                        },
                    ))),
                    right: Expression::String(ip_str.into()),
                    op: Operator::EQ,
                }),
                Statement::Counter(StmtCounter::Named(counter.into())),
                Statement::Accept(None),
            ]
            .into(),
            ..Default::default()
        }));
        batch
    }

    #[cfg(feature = "embedded-portal")]
    pub fn build_delete_rule_batch(&self, handle: u32) -> Batch<'static> {
        let mut batch = Batch::new();
        batch.delete(NfListObject::Rule(schema::Rule {
            family: NfFamily::INet,
            table: self.table.clone().into(),
            chain: "forward".into(),
            handle: Some(handle),
            expr: [][..].into(),
            ..Default::default()
        }));
        batch
    }

    #[cfg(feature = "embedded-portal")]
    fn apply_with_echo(batch: Batch<'static>) -> Result<String, NftError> {
        let json_str = serde_json::to_string(&batch.to_nftables())
            .map_err(|e| NftError::Apply(e.to_string()))?;
        nftables::helper::apply_ruleset_raw(&json_str, nftables::helper::DEFAULT_NFT, &["--echo"])
            .map_err(|e| NftError::Apply(e.to_string()))
    }

    pub fn parse_rule_handle(response_json: &str, table: &str) -> Option<u32> {
        let parsed: serde_json::Value = serde_json::from_str(response_json).ok()?;
        let objects = parsed.get("nftables")?.as_array()?;
        for obj in objects {
            for key in ["add", "insert", "replace"] {
                if let Some(rule) = obj.get(key).and_then(|o| o.get("rule")) {
                    if rule.get("table").and_then(|t| t.as_str()) == Some(table)
                        && rule.get("chain").and_then(|c| c.as_str()) == Some("forward")
                    {
                        return rule
                            .get("handle")
                            .and_then(|h| h.as_u64())
                            .map(|h| h as u32);
                    }
                }
            }
            if let Some(rule) = obj.get("rule") {
                if rule.get("table").and_then(|t| t.as_str()) == Some(table)
                    && rule.get("chain").and_then(|c| c.as_str()) == Some("forward")
                {
                    return rule
                        .get("handle")
                        .and_then(|h| h.as_u64())
                        .map(|h| h as u32);
                }
            }
        }
        None
    }

    #[cfg(feature = "embedded-portal")]
    pub fn add_counter_rule(&self, ip: IpAddr) -> Result<u32, NftError> {
        let batch = self.build_counter_rule_batch(ip);
        let response = Self::apply_with_echo(batch)?;
        Self::parse_rule_handle(&response, &self.table)
            .ok_or_else(|| NftError::Apply("rule handle not found in echo response".into()))
    }

    #[cfg(feature = "embedded-portal")]
    pub fn delete_rule(&self, handle: u32) -> Result<(), NftError> {
        Self::apply(self.build_delete_rule_batch(handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn counter_name_v4() {
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100));
        assert_eq!(NftManager::counter_name(&ip), "c-192.168.1.100");
    }

    #[test]
    fn counter_name_v6() {
        let ip = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert_eq!(NftManager::counter_name(&ip), "c6-::1");
    }

    #[cfg(feature = "embedded-portal")]
    #[test]
    fn install_batch_contains_table_and_sets() {
        let mgr = NftManager::new();
        let batch = mgr.build_install_batch();
        let json = serde_json::to_string(&batch.to_nftables()).unwrap();

        assert!(json.contains("\"tollgate\""));
        assert!(json.contains("\"authenticated_v4\""));
        assert!(json.contains("\"authenticated_v6\""));
        assert!(json.contains("\"prerouting\""));
        assert!(json.contains("\"forward\""));
        assert!(json.contains("\"filter\""));
        assert!(json.contains("\"nat\""));
        assert!(json.contains("\"drop\""));
    }

    #[cfg(feature = "embedded-portal")]
    #[test]
    fn install_batch_has_forward_accept_rules() {
        let mgr = NftManager::new();
        let batch = mgr.build_install_batch();
        let json = serde_json::to_string(&batch.to_nftables()).unwrap();

        assert!(json.contains("@authenticated_v4"));
        assert!(json.contains("@authenticated_v6"));
        assert!(json.contains("\"accept\""));
        assert!(json.contains("53"));
    }

    #[cfg(feature = "embedded-portal")]
    #[test]
    fn add_client_batch_contains_v4_ip() {
        let mgr = NftManager::new();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 42));
        let batch = mgr.build_add_client_batch(ip);
        let json = serde_json::to_string(&batch.to_nftables()).unwrap();

        assert!(json.contains("10.0.0.42"));
        assert!(json.contains("\"authenticated_v4\""));
    }

    #[cfg(feature = "embedded-portal")]
    #[test]
    fn add_client_batch_targets_v6_set() {
        let mgr = NftManager::new();
        let ip = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let batch = mgr.build_add_client_batch(ip);
        let json = serde_json::to_string(&batch.to_nftables()).unwrap();

        assert!(json.contains("::1"));
        assert!(json.contains("\"authenticated_v6\""));
    }

    #[cfg(feature = "embedded-portal")]
    #[test]
    fn remove_client_batch_uses_delete() {
        let mgr = NftManager::new();
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
        let batch = mgr.build_remove_client_batch(ip);
        let json = serde_json::to_string(&batch.to_nftables()).unwrap();

        assert!(json.contains("192.168.1.50"));
        assert!(json.contains("\"delete\""));
    }

    #[cfg(feature = "embedded-portal")]
    #[test]
    fn create_counter_batch_has_correct_name() {
        let mgr = NftManager::new();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let batch = mgr.build_create_counter_batch(ip);
        let json = serde_json::to_string(&batch.to_nftables()).unwrap();

        assert!(json.contains("\"c-10.0.0.1\""));
        assert!(json.contains("\"counter\""));
    }

    #[cfg(feature = "embedded-portal")]
    #[test]
    fn teardown_batch_deletes_table() {
        let mgr = NftManager::new();
        let batch = mgr.build_teardown_batch();
        let json = serde_json::to_string(&batch.to_nftables()).unwrap();

        assert!(json.contains("\"tollgate\""));
        assert!(json.contains("\"delete\""));
    }

    #[cfg(feature = "embedded-portal")]
    #[test]
    fn poll_counter_parses_json() {
        let mock_json = r#"{"nftables":[{"counter":{"family":"inet","table":"tollgate","name":"c-10.0.0.1","handle":5,"packets":142,"bytes":9821}}]}"#;
        let parsed: serde_json::Value = serde_json::from_str(mock_json).unwrap();

        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let target = NftManager::counter_name(&ip);

        let mut found = false;
        if let Some(objects) = parsed.get("nftables").and_then(|v| v.as_array()) {
            for obj in objects {
                if let Some(counter) = obj.get("counter") {
                    if counter.get("name").and_then(|n| n.as_str()) == Some(&target) {
                        let packets = counter.get("packets").and_then(|p| p.as_u64()).unwrap_or(0);
                        let bytes = counter.get("bytes").and_then(|b| b.as_u64()).unwrap_or(0);
                        assert_eq!(packets, 142);
                        assert_eq!(bytes, 9821);
                        found = true;
                    }
                }
            }
        }
        assert!(found, "counter must be found in mock JSON");
    }

    #[cfg(feature = "embedded-portal")]
    #[test]
    fn counter_rule_batch_contains_ip_and_counter_ref() {
        let mgr = NftManager::new();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 55));
        let batch = mgr.build_counter_rule_batch(ip);
        let json = serde_json::to_string(&batch.to_nftables()).unwrap();

        assert!(json.contains("10.0.0.55"), "must match client IP");
        assert!(
            json.contains("\"c-10.0.0.55\""),
            "must reference named counter"
        );
        assert!(json.contains("\"accept\""), "must accept matched traffic");
        assert!(json.contains("\"forward\""), "must be in forward chain");
    }

    #[cfg(feature = "embedded-portal")]
    #[test]
    fn counter_rule_batch_v6_uses_ip6() {
        let mgr = NftManager::new();
        let ip = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let batch = mgr.build_counter_rule_batch(ip);
        let json = serde_json::to_string(&batch.to_nftables()).unwrap();

        assert!(json.contains("\"ip6\""), "v6 rule must use ip6 protocol");
        assert!(
            json.contains("\"c6-::1\""),
            "must reference v6 counter name"
        );
    }

    #[cfg(feature = "embedded-portal")]
    #[test]
    fn delete_rule_batch_contains_handle() {
        let mgr = NftManager::new();
        let batch = mgr.build_delete_rule_batch(77);
        let json = serde_json::to_string(&batch.to_nftables()).unwrap();

        assert!(json.contains("\"delete\""), "must be delete operation");
        assert!(json.contains("77"), "must reference handle 77");
    }

    #[test]
    fn parse_rule_handle_from_echo_response() {
        let mock = r#"{"nftables":[{"add":{"rule":{"family":"inet","table":"tollgate","chain":"forward","expr":[],"handle":42}}}]}"#;
        let handle = NftManager::parse_rule_handle(mock, "tollgate");
        assert_eq!(handle, Some(42));
    }

    #[test]
    fn parse_rule_handle_from_bare_rule() {
        let mock = r#"{"nftables":[{"rule":{"family":"inet","table":"tollgate","chain":"forward","expr":[],"handle":99}}]}"#;
        let handle = NftManager::parse_rule_handle(mock, "tollgate");
        assert_eq!(handle, Some(99));
    }

    #[test]
    fn parse_rule_handle_wrong_table_returns_none() {
        let mock = r#"{"nftables":[{"add":{"rule":{"family":"inet","table":"other","chain":"forward","expr":[],"handle":1}}}]}"#;
        let handle = NftManager::parse_rule_handle(mock, "tollgate");
        assert_eq!(handle, None);
    }

    #[test]
    fn parse_rule_handle_no_rule_returns_none() {
        let mock = r#"{"nftables":[{"counter":{"name":"foo"}}]}"#;
        let handle = NftManager::parse_rule_handle(mock, "tollgate");
        assert_eq!(handle, None);
    }
}
