//! Minimal read-only X.509 support for the `ssl` CLI commands.
//!
//! Extracts just enough from a DER certificate — SANs, subject CommonName,
//! validity window — to answer "does this certificate cover this router?"
//! the way Go's `x509.Certificate.VerifyHostname` does, without pulling a
//! full ASN.1/x509 dependency into the router binary. CN-without-SAN is
//! deliberately NOT coverage: modern browsers ignore the CN when no SAN
//! extension is present.

use std::net::IpAddr;

#[derive(Debug, thiserror::Error)]
pub enum X509Error {
    #[error("truncated DER")]
    Truncated,
    #[error("unexpected tag {tag:#04x}, expected {expected}")]
    UnexpectedTag { tag: u8, expected: &'static str },
    #[error("unsupported time tag {tag:#04x}")]
    BadTime { tag: u8 },
    #[error("invalid time string")]
    BadTimeString,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CertSummary {
    pub dns_sans: Vec<String>,
    pub ip_sans: Vec<IpAddr>,
    pub subject_cn: String,
    /// Seconds since the Unix epoch, UTC.
    pub not_before: i64,
    /// Seconds since the Unix epoch, UTC.
    pub not_after: i64,
}

/// Read one DER TLV. Returns the tag, the content slice, and the total
/// number of bytes the TLV occupied (tag + length header + content).
fn read_tlv(buf: &[u8]) -> Option<(u8, &[u8], usize)> {
    if buf.len() < 2 {
        return None;
    }
    let tag = buf[0];
    let first = buf[1];
    let (len, header) = if first & 0x80 == 0 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || buf.len() < 2 + n {
            return None;
        }
        let mut len: usize = 0;
        for b in &buf[2..2 + n] {
            len = (len << 8) | *b as usize;
        }
        (len, 2 + n)
    };
    if buf.len() < header + len {
        return None;
    }
    Some((tag, &buf[header..header + len], header + len))
}

fn children(content: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let mut rest = content;
    while let Some((tag, inner, total)) = read_tlv(rest) {
        out.push((tag, inner));
        rest = &rest[total..];
    }
    out
}

const TAG_SEQUENCE: u8 = 0x30;
const TAG_OID: u8 = 0x06;
const TAG_BOOL: u8 = 0x01;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_BIT_STRING: u8 = 0x03;
const TAG_INTEGER: u8 = 0x02;
const TAG_CONTEXT_3: u8 = 0xA3;
const TAG_CONTEXT_1: u8 = 0xA1;
const TAG_UTCTIME: u8 = 0x17;
const TAG_GENERALIZEDTIME: u8 = 0x18;

const OID_SUBJECT_ALT_NAME: [u8; 3] = [0x55, 0x1D, 0x11];
const OID_COMMON_NAME: [u8; 3] = [0x55, 0x04, 0x03];
const OID_RSA_ENCRYPTION: [u8; 9] = [0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];
const OID_EC_PUBLIC_KEY: [u8; 7] = [0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];

/// Read one DER TLV, keeping the raw encoded bytes (tag + header +
/// content) so key components can be compared byte-for-byte.
fn read_tlv_raw(buf: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (tag, content, total) = read_tlv(buf)?;
    Some((tag, content, &buf[..total]))
}

fn children_raw(content: &[u8]) -> Vec<(u8, &[u8], &[u8])> {
    let mut out = Vec::new();
    let mut rest = content;
    while let Some((tag, inner, raw)) = read_tlv_raw(rest) {
        out.push((tag, inner, raw));
        rest = &rest[raw.len()..];
    }
    out
}

fn parse_asn1_time(tag: u8, body: &[u8]) -> Result<i64, X509Error> {
    let s = std::str::from_utf8(body).map_err(|_| X509Error::BadTimeString)?;
    let s = s.trim_end_matches('Z');
    let digits: Vec<u32> = s
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .filter_map(|c| c.to_digit(10))
        .collect();

    let (year, rest) = match tag {
        TAG_UTCTIME => {
            if digits.len() < 10 {
                return Err(X509Error::BadTimeString);
            }
            let yy = digits[0] * 10 + digits[1];
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, &digits[2..])
        }
        TAG_GENERALIZEDTIME => {
            if digits.len() < 12 {
                return Err(X509Error::BadTimeString);
            }
            (
                digits[0] * 1000 + digits[1] * 100 + digits[2] * 10 + digits[3],
                &digits[4..],
            )
        }
        _ => return Err(X509Error::BadTime { tag }),
    };

    if rest.len() < 10 {
        return Err(X509Error::BadTimeString);
    }
    let two = |i: usize| rest[i] * 10 + rest[i + 1];
    let month = two(0);
    let day = two(2);
    let hour = two(4);
    let minute = two(6);
    let second = two(8);

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(X509Error::BadTimeString);
    }

    Ok(days_from_civil(i64::from(year), month, day) * 86_400
        + i64::from(hour) * 3_600
        + i64::from(minute) * 60
        + i64::from(second))
}

/// Days since 1970-01-01 for a proleptic Gregorian civil date
/// (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date for a day count (inverse of `days_from_civil`), used to render
/// expiry dates in the Go parity format YYYY-MM-DD.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn parse_san_extension(content: &[u8], summary: &mut CertSummary) -> Result<(), X509Error> {
    let (_, names, _) = read_tlv(content).ok_or(X509Error::Truncated)?;
    for (tag, body) in children(names) {
        match tag {
            0x82 => {
                summary
                    .dns_sans
                    .push(String::from_utf8_lossy(body).to_string());
            }
            0x87 => match body.len() {
                4 => {
                    let octets: [u8; 4] = body.try_into().unwrap_or([0; 4]);
                    summary.ip_sans.push(IpAddr::from(octets));
                }
                16 => {
                    let octets: [u8; 16] = body.try_into().unwrap_or([0; 16]);
                    summary.ip_sans.push(IpAddr::from(octets));
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(())
}

fn parse_subject_cn(content: &[u8], summary: &mut CertSummary) {
    for (_, rdn_content) in children(content) {
        for atv in children(rdn_content) {
            let atv_children = children(atv.1);
            if atv_children.len() < 2 {
                continue;
            }
            let (oid_tag, oid) = atv_children[0];
            let (val_tag, val) = atv_children[1];
            if oid_tag == TAG_OID
                && *oid == OID_COMMON_NAME
                && (val_tag == 0x0C || val_tag == 0x13 || val_tag == 0x16)
                && summary.subject_cn.is_empty()
            {
                summary.subject_cn = String::from_utf8_lossy(val).to_string();
            }
        }
    }
}

/// Parse the parts of a DER certificate the `ssl` commands need.
pub fn parse_certificate(der: &[u8]) -> Result<CertSummary, X509Error> {
    let (cert_tag, cert_body, _) = read_tlv(der).ok_or(X509Error::Truncated)?;
    if cert_tag != TAG_SEQUENCE {
        return Err(X509Error::UnexpectedTag {
            tag: cert_tag,
            expected: "Certificate SEQUENCE",
        });
    }
    let (tbs_tag, tbs_body, _) = read_tlv(cert_body).ok_or(X509Error::Truncated)?;
    if tbs_tag != TAG_SEQUENCE {
        return Err(X509Error::UnexpectedTag {
            tag: tbs_tag,
            expected: "TBSCertificate SEQUENCE",
        });
    }

    // [version]? serial signature issuer validity subject spki [3]extensions
    let mut ordered = children(tbs_body);
    if ordered.first().is_some_and(|(t, _)| *t == 0xA0) {
        ordered.remove(0);
    }
    if ordered.len() < 6 {
        return Err(X509Error::Truncated);
    }

    let mut summary = CertSummary {
        dns_sans: Vec::new(),
        ip_sans: Vec::new(),
        subject_cn: String::new(),
        not_before: 0,
        not_after: 0,
    };

    let times = children(ordered[3].1);
    if times.len() < 2 {
        return Err(X509Error::Truncated);
    }
    summary.not_before = parse_asn1_time(times[0].0, times[0].1)?;
    summary.not_after = parse_asn1_time(times[1].0, times[1].1)?;

    parse_subject_cn(ordered[4].1, &mut summary);

    if let Some((_, exts_wrapper)) = ordered.iter().find(|(t, _)| *t == TAG_CONTEXT_3) {
        if let Some((_, ext_seq, _)) = read_tlv(exts_wrapper) {
            for (_, ext) in children(ext_seq) {
                let parts = children(ext);
                if parts.len() < 2 {
                    continue;
                }
                let oid = parts[0].1;
                let value_index = if parts[1].0 == TAG_BOOL { 2 } else { 1 };
                if parts.len() <= value_index {
                    continue;
                }
                if *oid == OID_SUBJECT_ALT_NAME && parts[value_index].0 == TAG_OCTET_STRING {
                    parse_san_extension(parts[value_index].1, &mut summary)?;
                }
            }
        }
    }

    Ok(summary)
}

/// Errors from validating that a TLS private key is parseable and pairs
/// with a certificate (Codex P2 on #54: `ssl apply` must reject a broken
/// or mismatched pair before touching uhttpd or installing files).
#[derive(Debug, thiserror::Error)]
pub enum KeyPairError {
    #[error("certificate is malformed: {0}")]
    Cert(String),
    #[error("private key is malformed: {0}")]
    Key(String),
    #[error("private key algorithm does not match the certificate's")]
    AlgorithmMismatch,
    #[error("private key does not match the certificate")]
    Mismatch,
}

/// The public-key material of a certificate's SubjectPublicKeyInfo,
/// in directly comparable form.
enum CertPublicKey<'a> {
    /// Uncompressed EC point, as carried in both the SPKI BIT STRING and
    /// the SEC1/PKCS#8 [1] publicKey field.
    EcPoint(&'a [u8]),
    /// Raw DER TLVs of the RSAPublicKey's modulus and exponent INTEGERs.
    RsaNAndE(&'a [u8], &'a [u8]),
}

fn cert_public_key(cert_der: &[u8]) -> Result<CertPublicKey<'_>, KeyPairError> {
    let err = |m: String| KeyPairError::Cert(m);
    let (_, cert_body, _) = read_tlv(cert_der).ok_or_else(|| err("truncated".into()))?;
    if cert_body.first() != Some(&TAG_SEQUENCE) {
        return Err(err("not a certificate SEQUENCE".into()));
    }
    let (_, tbs_body, _) = read_tlv(cert_body).ok_or_else(|| err("truncated TBS".into()))?;
    let mut ordered = children(tbs_body);
    if ordered.first().is_some_and(|(t, _)| *t == 0xA0) {
        ordered.remove(0);
    }
    // [serial] signature issuer validity subject spki
    let spki = ordered
        .get(5)
        .ok_or_else(|| err("missing SubjectPublicKeyInfo".into()))?;
    let spki_parts = children(spki.1);
    let alg_children = spki_parts
        .first()
        .filter(|(t, _)| *t == TAG_SEQUENCE)
        .map(|(_, alg_seq)| children(alg_seq))
        .ok_or_else(|| err("missing SPKI algorithm".into()))?;
    let alg = alg_children
        .first()
        .filter(|(t, _)| *t == TAG_OID)
        .map(|(_, oid)| *oid)
        .ok_or_else(|| err("missing SPKI algorithm OID".into()))?;
    let pk = spki_parts
        .get(1)
        .filter(|(t, _)| *t == TAG_BIT_STRING)
        .map(|(_, content)| content)
        .ok_or_else(|| err("missing SubjectPublicKey BIT STRING".into()))?;
    // BIT STRING: first content byte is the unused-bit count (0 for keys).
    let pk = pk
        .get(1..)
        .ok_or_else(|| err("empty SubjectPublicKey".into()))?;

    if alg == OID_EC_PUBLIC_KEY {
        Ok(CertPublicKey::EcPoint(pk))
    } else if alg == OID_RSA_ENCRYPTION {
        let (seq_tag, seq_content, _) =
            read_tlv_raw(pk).ok_or_else(|| err("bad RSAPublicKey".into()))?;
        if seq_tag != TAG_SEQUENCE {
            return Err(err("bad RSAPublicKey".into()));
        }
        let ints = children_raw(seq_content);
        let (_, _, n) = ints
            .first()
            .filter(|(t, _, _)| *t == TAG_INTEGER)
            .ok_or_else(|| err("missing RSA modulus".into()))?;
        let (_, _, e) = ints
            .get(1)
            .filter(|(t, _, _)| *t == TAG_INTEGER)
            .ok_or_else(|| err("missing RSA exponent".into()))?;
        Ok(CertPublicKey::RsaNAndE(n, e))
    } else {
        Err(err(format!(
            "unsupported public-key algorithm OID {}",
            alg.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )))
    }
}

/// Compare a DER private key (PKCS#8, SEC1 EC, or PKCS#1 RSA) against the
/// certificate's public key. Structural comparison only — no bignum or
/// curve math, no new dependencies (musl-safe, binary-size parity with
/// the module's hand-rolled DER approach).
pub fn private_key_matches_certificate(
    cert_der: &[u8],
    key_der: &[u8],
) -> Result<(), KeyPairError> {
    let cert_pk = cert_public_key(cert_der)?;
    let key_public = key_public_material(key_der)?;

    match (&cert_pk, &key_public) {
        // KNOWN LIMITATION (Codex P2 on #58): the EC arm trusts the
        // OPTIONAL public point embedded in the SEC1 structure instead of
        // deriving it from the private scalar — a crafted/damaged key can
        // keep the certificate's point while carrying a different scalar,
        // and a valid SEC1 key that omits the point is rejected. Deriving
        // requires curve math (a new dependency or hand-rolled scalar
        // multiplication — both out of scope for this module); tracked in
        // the follow-up issue linked from the PR thread.
        (CertPublicKey::EcPoint(cert_point), KeyPublicMaterial::EcPoint(key_point)) => {
            if cert_point == key_point {
                Ok(())
            } else {
                Err(KeyPairError::Mismatch)
            }
        }
        (CertPublicKey::RsaNAndE(cert_n, cert_e), KeyPublicMaterial::RsaNAndE(key_n, key_e)) => {
            if cert_n == key_n && cert_e == key_e {
                Ok(())
            } else {
                Err(KeyPairError::Mismatch)
            }
        }
        _ => Err(KeyPairError::AlgorithmMismatch),
    }
}

enum KeyPublicMaterial<'a> {
    EcPoint(&'a [u8]),
    RsaNAndE(&'a [u8], &'a [u8]),
}

fn key_public_material(key_der: &[u8]) -> Result<KeyPublicMaterial<'_>, KeyPairError> {
    let err = |m: &str| KeyPairError::Key(m.to_string());
    let (tag, body, _) = read_tlv(key_der).ok_or_else(|| err("truncated"))?;
    if tag != TAG_SEQUENCE {
        return Err(err("not a key SEQUENCE"));
    }
    let parts = children_raw(body);
    let first_tag = parts.first().map(|(t, _, _)| *t);

    // PKCS#8 PrivateKeyInfo: SEQUENCE { version INTEGER 0, algorithm,
    // privateKey OCTET STRING }
    if first_tag == Some(TAG_INTEGER) && parts.len() >= 3 {
        let is_pkcs8 = parts[1].0 == TAG_SEQUENCE && parts[2].0 == TAG_OCTET_STRING;
        if is_pkcs8 {
            let alg_oid = children(parts[1].1)
                .first()
                .filter(|(t, _)| *t == TAG_OID)
                .map(|(_, oid)| oid.to_vec())
                .ok_or_else(|| err("missing PKCS#8 algorithm OID"))?;
            let inner = parts[2].1;
            if alg_oid == OID_EC_PUBLIC_KEY {
                return sec1_public_key(inner);
            }
            if alg_oid == OID_RSA_ENCRYPTION {
                return rsa_public_key(inner);
            }
            return Err(err("unsupported PKCS#8 key algorithm"));
        }
    }

    // SEC1 ECPrivateKey: SEQUENCE { version INTEGER 1, privateKey OCTET
    // STRING, [0] parameters?, [1] publicKey BIT STRING }
    if first_tag == Some(TAG_INTEGER) && parts.len() >= 2 && parts[1].0 == TAG_OCTET_STRING {
        return sec1_public_key(key_der);
    }

    // PKCS#1 RSAPrivateKey: SEQUENCE { version INTEGER 0, n, e, d, ... }
    if first_tag == Some(TAG_INTEGER) && parts.len() >= 3 && parts[1].0 == TAG_INTEGER {
        return rsa_public_key(key_der);
    }

    Err(err("unrecognized private key structure"))
}

/// Extract the [1] publicKey point from a SEC1 ECPrivateKey DER.
fn sec1_public_key(sec1_der: &[u8]) -> Result<KeyPublicMaterial<'_>, KeyPairError> {
    let err = |m: &str| KeyPairError::Key(m.to_string());
    let (tag, body, _) = read_tlv(sec1_der).ok_or_else(|| err("truncated EC key"))?;
    if tag != TAG_SEQUENCE {
        return Err(err("not an EC key SEQUENCE"));
    }
    let parts = children_raw(body);
    // Go's x509.ParseECPrivateKey requires the embedded public key; a key
    // without it cannot be paired against a certificate here either.
    // The [1] context element's CONTENT is the BIT STRING TLV — parse it
    // directly rather than skipping a fixed 2-byte header (long-form
    // lengths on P-521 keys make the [1] header 3+ bytes; slicing
    // raw[2..] would then misparse a valid key — Codex P2 on #58).
    let (_, bit_content, _) = parts
        .iter()
        .find(|(t, _, _)| *t == TAG_CONTEXT_1)
        .and_then(|(_, content, _)| read_tlv_raw(content))
        .filter(|(t, _, _)| *t == TAG_BIT_STRING)
        .ok_or_else(|| err("EC key carries no embedded public point"))?;
    bit_content
        .get(1..)
        .map(KeyPublicMaterial::EcPoint)
        .ok_or_else(|| err("empty EC public point"))
}

/// Extract the modulus and exponent TLVs from a PKCS#1 RSAPrivateKey DER.
fn rsa_public_key(rsa_der: &[u8]) -> Result<KeyPublicMaterial<'_>, KeyPairError> {
    let err = |m: &str| KeyPairError::Key(m.to_string());
    let (tag, body, _) = read_tlv(rsa_der).ok_or_else(|| err("truncated RSA key"))?;
    if tag != TAG_SEQUENCE {
        return Err(err("not an RSA key SEQUENCE"));
    }
    let parts = children_raw(body);
    let (_, _, n) = parts
        .get(1)
        .filter(|(t, _, _)| *t == TAG_INTEGER)
        .ok_or_else(|| err("missing RSA modulus"))?;
    let (_, _, e) = parts
        .get(2)
        .filter(|(t, _, _)| *t == TAG_INTEGER)
        .ok_or_else(|| err("missing RSA exponent"))?;
    Ok(KeyPublicMaterial::RsaNAndE(n, e))
}

/// Go `x509.Certificate.VerifyHostname` semantics, restricted to what the
/// `ssl` commands compare against: case-insensitive match, or a leftmost
/// `*` label matching exactly one label of the host name.
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let p = pattern.trim_end_matches('.').to_lowercase();
    let h = host.trim_end_matches('.').to_lowercase();
    if let Some(p_rest) = p.strip_prefix("*.") {
        if let Some((_, h_rest)) = h.split_once('.') {
            return !h_rest.is_empty() && h_rest == p_rest;
        }
        return false;
    }
    p == h
}

/// Decode standard-alphabet base64, ignoring ASCII whitespace. No padding
/// recovery: PEM bodies are always padded.
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let bytes: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let body = match bytes.iter().position(|&b| b == b'=') {
        Some(i) => {
            if bytes[i + 1..].iter().any(|&b| b != b'=') || bytes.len() % 4 != 0 {
                return None;
            }
            &bytes[..i]
        }
        None => &bytes[..],
    };

    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    for chunk in body.chunks(4) {
        match chunk.len() {
            4 => {
                let n = (val(chunk[0])? << 18)
                    | (val(chunk[1])? << 12)
                    | (val(chunk[2])? << 6)
                    | val(chunk[3])?;
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
                out.push(n as u8);
            }
            3 => {
                let n = (val(chunk[0])? << 18) | (val(chunk[1])? << 12) | (val(chunk[2])? << 6);
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
            }
            2 => {
                let n = (val(chunk[0])? << 18) | (val(chunk[1])? << 12);
                out.push((n >> 16) as u8);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Decode the first PEM block of `block_type` ("CERTIFICATE",
/// "PRIVATE KEY", ...). `None` when no such block exists.
pub fn pem_decode(data: &str, block_type: &str) -> Option<Vec<u8>> {
    pem_blocks(data)
        .into_iter()
        .find(|(t, _)| t == block_type)
        .map(|(_, der)| der)
}

/// Decode every PEM block in `data` as `(type, DER bytes)` pairs — used to
/// split a combined cert+key file.
pub fn pem_blocks(data: &str) -> Vec<(String, Vec<u8>)> {
    let mut blocks = Vec::new();
    let mut current_type: Option<String> = None;
    let mut current_b64 = String::new();

    for line in data.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("-----BEGIN ") {
            if let Some(t) = rest.strip_suffix("-----") {
                current_type = Some(t.to_string());
                current_b64.clear();
            }
        } else if let Some(rest) = line.strip_prefix("-----END ") {
            if rest.strip_suffix("-----") == current_type.as_deref() {
                if let (Some(t), Some(der)) = (current_type.take(), base64_decode(&current_b64)) {
                    blocks.push((t, der));
                }
                current_b64.clear();
            }
        } else if current_type.is_some() {
            current_b64.push_str(line);
        }
    }
    blocks
}

#[cfg(test)]
mod tests;
