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
const TAG_CONTEXT_3: u8 = 0xA3;
const TAG_UTCTIME: u8 = 0x17;
const TAG_GENERALIZEDTIME: u8 = 0x18;

const OID_SUBJECT_ALT_NAME: [u8; 3] = [0x55, 0x1D, 0x11];
const OID_COMMON_NAME: [u8; 3] = [0x55, 0x04, 0x03];

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
