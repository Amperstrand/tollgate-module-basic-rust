//! Identity management — load/generate secp256k1 keypairs for Nostr signing.
//!
//! Mirrors Go's identities.json model. On first run, generates a merchant
//! keypair and stores it. On subsequent runs, loads from disk. File mode 0600.

use crate::config::schema::{IdentitiesConfig, OwnedIdentity};
use crate::error::ConfigError;
use secp256k1::{Secp256k1, SecretKey};

/// A Nostr keypair for event signing.
#[derive(Debug, Clone)]
pub struct MerchantIdentity {
    pub name: String,
    pub secret_key: SecretKey,
}

impl MerchantIdentity {
    /// Load the merchant identity from identities.json, or generate a new one.
    pub fn load_or_generate() -> Result<Self, ConfigError> {
        let identities = crate::config::load_identities();

        if let Ok(Some(config)) = identities {
            if let Some(owned) = config
                .owned_identities
                .iter()
                .find(|o| o.name == "merchant")
            {
                let secret_key = SecretKey::from_str(&owned.privatekey)
                    .map_err(|e| ConfigError::InvalidKey(e.to_string()))?;
                return Ok(MerchantIdentity {
                    name: "merchant".to_string(),
                    secret_key,
                });
            }
        }

        // Generate new keypair
        let secp = Secp256k1::new();
        let (secret_key, public_key) = secp.generate_keypair(&mut rand::thread_rng());
        let pub_hex = public_key.to_string();

        tracing::info!(pubkey = %pub_hex, "generated new merchant identity");

        // Save to identities.json
        let new_config = IdentitiesConfig {
            config_version: "v0.0.1".to_string(),
            owned_identities: vec![OwnedIdentity {
                name: "merchant".to_string(),
                privatekey: secret_key.display_secret().to_string(),
            }],
            public_identities: vec![],
        };

        let json = serde_json::to_string_pretty(&new_config)?;

        let path = crate::config::identities_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, json)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(MerchantIdentity {
            name: "merchant".to_string(),
            secret_key,
        })
    }

    /// Get the public key as hex.
    pub fn pubkey_hex(&self) -> String {
        pubkey_from_secret(&self.secret_key)
    }

    /// Construct from a hex private key (tests; parity with identities.json
    /// loading).
    pub fn from_privkey_hex(hex_priv: &str) -> Result<Self, ConfigError> {
        let secret_key =
            SecretKey::from_str(hex_priv).map_err(|e| ConfigError::InvalidKey(e.to_string()))?;
        Ok(MerchantIdentity {
            name: "merchant".to_string(),
            secret_key,
        })
    }
}

/// X-only (BIP-340) public key hex for a secret key — the form go-nostr
/// uses everywhere (npub input, IPv4/MAC domain hash input).
fn pubkey_from_secret(secret: &SecretKey) -> String {
    let secp = Secp256k1::new();
    secp256k1::PublicKey::from_secret_key(&secp, secret)
        .x_only_public_key()
        .0
        .to_string()
}

// Re-export SecretKey for convenience
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Derived identity — 1:1 port of Go src/identity/identity.go (PR #193).
//
// Every formula below must stay byte-identical to the Go implementation:
// the derived IPv4/MACs/passwords are the router's stable network identity,
// and GET /identity output is compared against a live Go router by PRTA.

/// Interfaces whose MACs appear in the public identity (Go StandardInterfaces).
pub const STANDARD_INTERFACES: [&str; 3] = ["br-lan", "wlan0", "wlan1"];

/// Public, non-sensitive identity attributes (Go DerivedIdentity) — the
/// GET /identity response body.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct DerivedIdentity {
    pub npub: String,
    pub ipv4: String,
    pub macs: std::collections::BTreeMap<String, String>,
}

/// Full identity including recovery material (Go FullIdentity) — the
/// POST /identity/reveal-seed response body (loopback only).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct FullIdentity {
    pub npub: String,
    pub ipv4: String,
    pub macs: std::collections::BTreeMap<String, String>,
    pub mnemonic: String,
    pub privatekey: String,
    pub root_password: String,
    pub wifi_password: String,
}

/// HKDF-SHA256 with salt "tollgate-v1" (Go deriveHash, identity.go:259):
/// PRK = HMAC(salt, ikm); OKM = HMAC(PRK, info‖0x01) — one 32-byte block.
fn derive_hash(domain_sep: &str, key_hex: &str) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;

    let prk = {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(b"tollgate-v1")
            .expect("HMAC accepts any key length");
        mac.update(key_hex.as_bytes());
        mac.finalize().into_bytes()
    };
    let mut mac = <HmacSha256 as Mac>::new_from_slice(&prk).expect("HMAC accepts any key length");
    mac.update(domain_sep.as_bytes());
    // RFC 5869 expand block counter — T(1) = HMAC(PRK, info‖0x01).
    mac.update(&[1u8]);
    let okm = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&okm);
    out
}

/// Map a public key into RFC 6598 CGNAT space with a .1 host octet
/// (Go DeriveIPv4): 100.(64 + h[0]%64).h[1].1 — second octet stays in 64..127.
pub fn derive_ipv4(pub_hex: &str) -> String {
    let h = derive_hash("tollgate-ipv4-v1:", pub_hex);
    format!("100.{}.{}.1", 64 + (h[0] % 64), h[1])
}

/// Locally-administered unicast MAC per interface (Go DeriveMAC): first
/// octet gets bit 1 set / bit 0 cleared, the rest from the domain hash.
pub fn derive_mac(pub_hex: &str, iface: &str) -> String {
    let h = derive_hash(&format!("tollgate-mac-v1:{iface}:"), pub_hex);
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&h[..6]);
    mac[0] = (mac[0] & 0xFC) | 0x02;
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Six hyphen-joined BIP39 words selected by big-endian 16-bit chunks of the
/// domain hash (Go DeriveRootPassword / DeriveWiFiPassword v2 format).
fn six_bip39_words(domain_sep: &str, key_hex: &str) -> String {
    let h = derive_hash(domain_sep, key_hex);
    let words = bip39::Language::English.word_list();
    let mut parts = Vec::with_capacity(6);
    for i in 0..6 {
        let idx = (((h[i * 2] as usize) << 8) | h[i * 2 + 1] as usize) % words.len();
        parts.push(words[idx]);
    }
    parts.join("-")
}

pub fn derive_root_password(priv_hex: &str) -> String {
    six_bip39_words("tollgate-root-pw-v2:", priv_hex)
}

pub fn derive_wifi_password(priv_hex: &str) -> String {
    // Go passes network="private" for the router's own derived password.
    six_bip39_words("tollgate-wifi-pw-v2:private:", priv_hex)
}

/// NIP-06: BIP39 mnemonic -> seed (empty passphrase) -> BIP32
/// m/44'/1237'/0'/0/0 -> hex secp256k1 private key.
///
/// Mirrors go-nostr nip06.PrivateKeyFromSeed exactly: go-bip32 derives
/// non-hardened children from the COMPRESSED parent public key.
pub fn mnemonic_to_privkey_hex(mnemonic: &str) -> Result<String, ConfigError> {
    let m = bip39::Mnemonic::parse_in_normalized(bip39::Language::English, mnemonic.trim())
        .map_err(|_| ConfigError::Validation("invalid mnemonic".to_string()))?;
    let seed = m.to_seed("");
    bip32_derive_privkey_hex(
        &seed,
        &[
            44 + BIP32_HARDENED,
            1237 + BIP32_HARDENED,
            BIP32_HARDENED,
            0,
            0,
        ],
    )
}

const BIP32_HARDENED: u32 = 0x8000_0000;

/// Minimal BIP32 private-key chain derivation (CKDpriv), enough for fixed
/// NIP-06 paths: master = HMAC-SHA512("Bitcoin seed", seed); hardened child
/// data = 0x00‖key‖be32(idx); normal child data = compressed-pub‖be32(idx).
fn bip32_derive_privkey_hex(seed: &[u8], path: &[u32]) -> Result<String, ConfigError> {
    use hmac::Hmac;
    use hmac::Mac;
    use sha2::Sha512;
    type HmacSha512 = Hmac<Sha512>;

    let mut mac =
        <HmacSha512 as Mac>::new_from_slice(b"Bitcoin seed").expect("HMAC accepts any key length");
    mac.update(seed);
    let i = mac.finalize().into_bytes();
    let (il, ir) = i.split_at(32);

    let secp = secp256k1::Secp256k1::new();
    let mut key = secp256k1::SecretKey::from_slice(il)
        .map_err(|_| ConfigError::InvalidKey("bip32 master key".into()))?;
    let mut chaincode: [u8; 32] = ir.try_into().expect("32 bytes");

    for idx in path {
        let mut data = Vec::with_capacity(37);
        if idx & BIP32_HARDENED != 0 {
            data.push(0u8);
            data.extend_from_slice(&key.secret_bytes());
        } else {
            let pubkey = secp256k1::PublicKey::from_secret_key(&secp, &key);
            data.extend_from_slice(&pubkey.serialize()); // 33-byte compressed
        }
        data.extend_from_slice(&idx.to_be_bytes());

        let mut mac =
            <HmacSha512 as Mac>::new_from_slice(&chaincode).expect("HMAC accepts any key length");
        mac.update(&data);
        let i = mac.finalize().into_bytes();
        let (il, ir) = i.split_at(32);

        let tweak = secp256k1::Scalar::from_be_bytes(il.try_into().expect("32 bytes"))
            .map_err(|_| ConfigError::Validation("bip32 invalid tweak".into()))?;
        key = key
            .add_tweak(&tweak)
            .map_err(|_| ConfigError::Validation("bip32 derived key invalid".into()))?;
        chaincode = ir.try_into().expect("32 bytes");
    }

    Ok(hex::encode(key.secret_bytes()))
}

/// bech32 npub1… encoding of the x-only public key (go-nostr nip19 parity).
pub fn npub_from_privkey_hex(priv_hex: &str) -> Result<String, ConfigError> {
    let secret = secp256k1::SecretKey::from_str(priv_hex)
        .map_err(|e| ConfigError::InvalidKey(e.to_string()))?;
    let secp = secp256k1::Secp256k1::new();
    let pubkey = secp256k1::PublicKey::from_secret_key(&secp, &secret);
    let xonly = pubkey.x_only_public_key().0.serialize();
    let hrp = bech32::Hrp::parse("npub").map_err(|e| ConfigError::Validation(e.to_string()))?;
    bech32::encode::<bech32::Bech32>(hrp, &xonly)
        .map_err(|e| ConfigError::Validation(e.to_string()))
}

/// Public identity attributes for a hex private key (Go identity.Derive).
pub fn derive_public(priv_hex: &str) -> Result<DerivedIdentity, ConfigError> {
    let secret = secp256k1::SecretKey::from_str(priv_hex)
        .map_err(|e| ConfigError::InvalidKey(e.to_string()))?;
    let secp = secp256k1::Secp256k1::new();
    let pub_hex = secp256k1::PublicKey::from_secret_key(&secp, &secret)
        .x_only_public_key()
        .0
        .to_string();

    let npub = npub_from_privkey_hex(priv_hex)?;
    let macs = STANDARD_INTERFACES
        .iter()
        .map(|iface| (iface.to_string(), derive_mac(&pub_hex, iface)))
        .collect();

    Ok(DerivedIdentity {
        npub,
        ipv4: derive_ipv4(&pub_hex),
        macs,
    })
}

/// Full identity from a 12-word mnemonic (Go identity.DeriveFromMnemonic).
pub fn derive_full_from_mnemonic(mnemonic: &str) -> Result<FullIdentity, ConfigError> {
    let priv_hex = mnemonic_to_privkey_hex(mnemonic)?;
    let public = derive_public(&priv_hex)?;
    Ok(FullIdentity {
        npub: public.npub,
        ipv4: public.ipv4,
        macs: public.macs,
        mnemonic: mnemonic.trim().to_string(),
        privatekey: priv_hex.clone(),
        root_password: derive_root_password(&priv_hex),
        wifi_password: derive_wifi_password(&priv_hex),
    })
}

#[cfg(test)]
mod derive_tests {
    use super::*;

    // Golden vectors generated from the Go implementation
    // (identity.DeriveFromMnemonic, go1.25, 2026-09-27) — byte parity for
    // the PR #193 identity surface. If these fail after a refactor, the
    // derived network identity diverges from Go: routers would change
    // npub/IPv4/MACs/passwords for the same seed.
    const MNEMONIC_A: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const MNEMONIC_B: &str =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";

    #[test]
    fn golden_identity_mnemonic_a() {
        let full = derive_full_from_mnemonic(MNEMONIC_A).unwrap();
        assert_eq!(
            full.npub,
            "npub1az708q3kd9zy6z6f44zav5ygvdwelkzspf6mtusttx47lft2z38sghk0w7"
        );
        assert_eq!(full.ipv4, "100.112.208.1");
        assert_eq!(full.macs["br-lan"], "be:60:b5:48:e4:a8");
        assert_eq!(full.macs["wlan0"], "02:e0:35:d6:ca:88");
        assert_eq!(full.macs["wlan1"], "22:c6:48:34:41:64");
        assert_eq!(
            full.privatekey,
            "5f29af3b9676180290e77a4efad265c4c2ff28a5302461f73597fda26bb25731"
        );
        assert_eq!(
            full.root_password,
            "budget-charge-prosper-cream-slide-almost"
        );
        assert_eq!(full.wifi_password, "receive-input-mule-afford-arrow-cat");
        assert_eq!(full.mnemonic, MNEMONIC_A);
    }

    #[test]
    fn golden_identity_mnemonic_b() {
        let full = derive_full_from_mnemonic(MNEMONIC_B).unwrap();
        assert_eq!(
            full.npub,
            "npub1mx07p7jvpdf4g5lgatea9sgk6mjyfrld947k2nvmwmas94q6sjhssl4jwc"
        );
        assert_eq!(full.ipv4, "100.124.89.1");
        assert_eq!(full.macs["br-lan"], "1e:9f:c1:e3:79:07");
        assert_eq!(full.macs["wlan0"], "aa:d8:dd:be:66:a4");
        assert_eq!(full.macs["wlan1"], "ce:ab:e7:7d:0e:82");
        assert_eq!(
            full.privatekey,
            "0e0c8bb2feba07a6e464a67bb081a89383f9f017aa395f3d9abdbdc461185922"
        );
        assert_eq!(full.root_password, "what-gun-angle-sugar-approve-absorb");
        assert_eq!(full.wifi_password, "milk-famous-soap-mom-pyramid-interest");
    }

    #[test]
    fn invalid_mnemonics_are_rejected() {
        assert!(derive_full_from_mnemonic("").is_err());
        assert!(derive_full_from_mnemonic("not a valid mnemonic").is_err());
        // Valid words, invalid checksum.
        assert!(derive_full_from_mnemonic(
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon"
        )
        .is_err());
        // Leading/trailing whitespace is trimmed like Go's strings.TrimSpace.
        assert!(derive_full_from_mnemonic(&format!("  {MNEMONIC_A} \n")).is_ok());
    }

    #[test]
    fn derived_identity_json_field_order_matches_go() {
        let full = derive_full_from_mnemonic(MNEMONIC_A).unwrap();
        let json = serde_json::to_string(&full).unwrap();
        let expected_order = [
            "\"npub\"",
            "\"ipv4\"",
            "\"macs\"",
            "\"mnemonic\"",
            "\"privatekey\"",
            "\"root_password\"",
            "\"wifi_password\"",
        ];
        let mut last = 0;
        for key in expected_order {
            let pos = json.find(key).expect(key);
            assert!(pos > last, "{key} out of Go field order: {json}");
            last = pos;
        }
    }
}
