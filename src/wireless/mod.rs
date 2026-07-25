//! Wireless gateway manager — WiFi scanning, connection, and upstream gateway management.
//!
//! Manages connecting to upstream WiFi networks (reseller mode), scanning for
//! TollGate access points, monitoring signal strength, and switching gateways.

pub mod types;
pub mod scanner;

pub use types::{Gateway, NetworkInfo, StaSection, UpstreamWifiConfig};
pub use scanner::Scanner;
