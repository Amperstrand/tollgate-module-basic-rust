//! Types for the upstream detector.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    InterfaceUp,
    InterfaceDown,
    RouteDeleted,
    AddressAdded,
    AddressDeleted,
}

#[derive(Debug, Clone)]
pub struct NetworkEvent {
    pub event_type: EventType,
    pub interface_name: String,
    pub gateway_ip: Option<String>,
}

impl NetworkEvent {
    pub fn new(event_type: EventType, interface_name: &str) -> Self {
        NetworkEvent {
            event_type,
            interface_name: interface_name.to_string(),
            gateway_ip: None,
        }
    }
}
