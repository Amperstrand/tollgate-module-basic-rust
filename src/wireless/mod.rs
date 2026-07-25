//! Wireless gateway manager — WiFi scanning, connection, and upstream gateway management.
//!
//! Manages connecting to upstream WiFi networks (reseller mode), scanning for
//! TollGate access points, monitoring signal strength, and switching gateways.

pub mod connector;
pub mod scanner;
pub mod types;
pub mod upstream_manager;

pub use connector::Connector;
pub use scanner::Scanner;
pub use types::{Gateway, NetworkInfo, StaSection, UpstreamWifiConfig};
pub use upstream_manager::{UpstreamManager, ManagerAction, ManagerStatus};
