//! Generated certificate authorities and certificates (IR v2 `ca` and `certificate` secret sources, rezn#54).
//!
//! Both are written to the program secret store as `tls.crt` and `tls.key` (PEM; ECDSA P-256 keys in PKCS#8), with
//! plain-text metadata beside them. A CA is generated once and never regenerated: that would invalidate every
//! certificate it signed and every trust bundle. A certificate is issued again when it is missing, when its
//! `ca`, `dns_names` or `days` change, when its CA's certificate changes, or once two thirds of its lifetime have
//! passed. A new certificate has new key versions, so the pods using it are replaced like for any changed input.
use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::ir_v2::{self, Program, Spec};
use openssl::{
    asn1::{Asn1Integer, Asn1Time},
    bn::{BigNum, MsbOption},
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    x509::{
        extension::{
            AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage,
            SubjectAlternativeName, SubjectKeyIdentifier,
        },
        X509Builder, X509NameBuilder, X509,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::secret::SecretStore;

pub const CERT: &str = "tls.crt";
pub const KEY: &str = "tls.key";

/// Tolerated clock difference between this host and the clients checking a certificate.
const SKEW_MINUTES: i64 = 5;

/// What Rezn keeps about a generated secret, beside its values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// `ca` or `certificate`.
    pub kind: String,
    /// Digest of the source fields it was generated from.
    pub spec: String,
    /// Version of the CA certificate that signed it (certificates only).
    pub ca_version: Option<String>,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
}

impl Meta {
    /// Certificates are renewed once two thirds of their lifetime have passed.
    pub fn renew_after(&self) -> DateTime<Utc> {
        self.not_before + (self.not_after - self.not_before) * 2 / 3
    }
}

/// A freshly generated certificate and key.
pub struct Issued {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
}

fn new_key() -> Result<PKey<Private>> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
    Ok(PKey::from_ec_key(EcKey::generate(&group)?)?)
}

/// A random positive 127-bit serial number.
fn serial() -> Result<Asn1Integer> {
    let mut n = BigNum::new()?;
    n.rand(127, MsbOption::MAYBE_ZERO, false)?;
    Ok(n.to_asn1_integer()?)
}

fn asn1_time(t: DateTime<Utc>) -> Result<Asn1Time> {
    Ok(Asn1Time::from_unix(t.timestamp())?)
}

/// A certificate's `notAfter` as a UTC time.
pub fn not_after(cert: &X509) -> Result<DateTime<Utc>> {
    let diff = Asn1Time::from_unix(0)?.diff(cert.not_after())?;
    Utc.timestamp_opt(i64::from(diff.days) * 86_400 + i64::from(diff.secs), 0)
        .single()
        .ok_or_else(|| anyhow!("certificate notAfter out of range"))
}

/// A Common Name is at most 64 characters.
fn common_name(name: &str) -> &str {
    match name.char_indices().nth(64) {
        Some((end, _)) => &name[..end],
        None => name,
    }
}

/// A self-signed certificate authority that can sign end-entity certificates only.
pub fn issue_ca(name: &str, days: u32, now: DateTime<Utc>) -> Result<Issued> {
    let key = new_key()?;
    let mut subject = X509NameBuilder::new()?;
    subject.append_entry_by_nid(Nid::COMMONNAME, common_name(name))?;
    let subject = subject.build();
    let not_before = now - Duration::minutes(SKEW_MINUTES);
    let not_after = now + Duration::days(days.into());
    let mut builder = X509Builder::new()?;
    builder.set_version(2)?;
    let (serial, from, until) = (serial()?, asn1_time(not_before)?, asn1_time(not_after)?);
    builder.set_serial_number(&serial)?;
    builder.set_subject_name(&subject)?;
    builder.set_issuer_name(&subject)?;
    builder.set_pubkey(&key)?;
    builder.set_not_before(&from)?;
    builder.set_not_after(&until)?;
    builder.append_extension(BasicConstraints::new().critical().ca().pathlen(0).build()?)?;
    builder.append_extension(
        KeyUsage::new()
            .critical()
            .key_cert_sign()
            .crl_sign()
            .build()?,
    )?;
    let ski = SubjectKeyIdentifier::new().build(&builder.x509v3_context(None, None))?;
    builder.append_extension(ski)?;
    builder.sign(&key, MessageDigest::sha256())?;
    Ok(Issued {
        cert_pem: builder.build().to_pem()?,
        key_pem: key.private_key_to_pem_pkcs8()?,
        not_before,
        not_after,
    })
}

/// A server and client certificate for `dns_names`, signed by the CA; never valid beyond the CA.
pub fn issue(
    dns_names: &[String],
    days: u32,
    ca_pem: &[u8],
    ca_key_pem: &[u8],
    now: DateTime<Utc>,
) -> Result<Issued> {
    let ca = X509::from_pem(ca_pem).context("reading the CA certificate")?;
    let ca_key = PKey::private_key_from_pem(ca_key_pem).context("reading the CA key")?;
    let key = new_key()?;
    let mut subject = X509NameBuilder::new()?;
    if let Some(first) = dns_names.first().filter(|n| n.len() <= 64) {
        subject.append_entry_by_nid(Nid::COMMONNAME, first)?;
    }
    let not_before = now - Duration::minutes(SKEW_MINUTES);
    let not_after = (now + Duration::days(days.into())).min(not_after(&ca)?);
    let mut builder = X509Builder::new()?;
    builder.set_version(2)?;
    let (serial, from, until) = (serial()?, asn1_time(not_before)?, asn1_time(not_after)?);
    builder.set_serial_number(&serial)?;
    builder.set_subject_name(&subject.build())?;
    builder.set_issuer_name(ca.subject_name())?;
    builder.set_pubkey(&key)?;
    builder.set_not_before(&from)?;
    builder.set_not_after(&until)?;
    builder.append_extension(BasicConstraints::new().critical().build()?)?;
    builder.append_extension(KeyUsage::new().critical().digital_signature().build()?)?;
    builder.append_extension(
        ExtendedKeyUsage::new()
            .server_auth()
            .client_auth()
            .build()?,
    )?;
    let mut san = SubjectAlternativeName::new();
    for name in dns_names {
        san.dns(name);
    }
    let san = san.build(&builder.x509v3_context(Some(&ca), None))?;
    builder.append_extension(san)?;
    let ski = SubjectKeyIdentifier::new().build(&builder.x509v3_context(Some(&ca), None))?;
    builder.append_extension(ski)?;
    let aki = AuthorityKeyIdentifier::new()
        .keyid(true)
        .build(&builder.x509v3_context(Some(&ca), None))?;
    builder.append_extension(aki)?;
    builder.sign(&ca_key, MessageDigest::sha256())?;
    Ok(Issued {
        cert_pem: builder.build().to_pem()?,
        key_pem: key.private_key_to_pem_pkcs8()?,
        not_before,
        not_after,
    })
}

fn digest(value: serde_json::Value) -> String {
    let canonical = serde_json_canonicalizer::to_vec(&value).expect("JSON serializes");
    hex::encode(Sha256::digest(canonical))
}

/// Digest of a CA's source fields.
pub fn ca_spec(ca: &ir_v2::Ca) -> String {
    digest(serde_json::json!(["ca", ca.common_name, ca.days]))
}

/// Digest of a certificate's source fields; the order of `dns_names` does not matter.
pub fn certificate_spec(cert: &ir_v2::Certificate) -> String {
    let mut names = cert.dns_names.clone();
    names.sort();
    digest(serde_json::json!([
        "certificate",
        cert.ca,
        names,
        cert.days
    ]))
}

/// Why a certificate must be issued now, if it must.
pub fn due(
    meta: Option<&Meta>,
    has_keys: bool,
    spec: &str,
    ca_version: Option<&str>,
    now: DateTime<Utc>,
) -> Option<&'static str> {
    let Some(meta) = meta.filter(|_| has_keys) else {
        return Some("issued");
    };
    if meta.spec != spec {
        Some("reissued for its changed spec")
    } else if meta.ca_version.as_deref() != ca_version {
        Some("reissued by its new CA")
    } else if now >= meta.renew_after() {
        Some("renewed")
    } else {
        None
    }
}

/// Parsed metadata of a namespace's generated secrets.
pub fn metas(store: &SecretStore, namespace: &str) -> Result<BTreeMap<String, Meta>> {
    store
        .generated_meta(namespace)?
        .into_iter()
        .map(|(name, raw)| {
            let meta = serde_json::from_slice(&raw)
                .with_context(|| format!("corrupt certificate metadata of {namespace}/{name}"))?;
            Ok((name, meta))
        })
        .collect()
}

/// Generates missing CAs, then issues every certificate that is due, for each program. Returns what changed, as
/// `<namespace>/<secret>: <what>`.
pub fn maintain<'a>(
    store: &SecretStore,
    programs: impl IntoIterator<Item = &'a Program>,
    now: DateTime<Utc>,
) -> Result<Vec<String>> {
    let mut changed = Vec::new();
    for program in programs {
        let namespace = &program.namespace;
        let secrets: Vec<(&str, &ir_v2::Secret)> = program
            .resources
            .iter()
            .filter_map(|r| match &r.spec {
                Spec::Secret(s) => Some((r.name.as_str(), s)),
                _ => None,
            })
            .collect();
        let has_keys = |name: &str| -> Result<bool> {
            Ok(store
                .program_versions(namespace)?
                .get(name)
                .is_some_and(|v| v.contains_key(CERT) && v.contains_key(KEY)))
        };
        for (name, secret) in &secrets {
            let Some(ca) = &secret.source.ca else {
                continue;
            };
            if has_keys(name)? {
                continue;
            }
            let issued = issue_ca(&ca.common_name, ca.days, now)?;
            let meta = Meta {
                kind: "ca".into(),
                spec: ca_spec(ca),
                ca_version: None,
                not_before: issued.not_before,
                not_after: issued.not_after,
            };
            write(store, namespace, name, issued, &meta)?;
            changed.push(format!("{namespace}/{name}: CA generated"));
        }
        let metas = metas(store, namespace)?;
        for (name, secret) in &secrets {
            let Some(cert) = &secret.source.certificate else {
                continue;
            };
            let versions = store.program_versions(namespace)?;
            let ca_version = versions.get(&cert.ca).and_then(|v| v.get(CERT));
            let spec = certificate_spec(cert);
            let Some(what) = due(
                metas.get(*name),
                has_keys(name)?,
                &spec,
                ca_version.map(String::as_str),
                now,
            ) else {
                continue;
            };
            let read = |key: &str| -> Result<Vec<u8>> {
                Ok(store
                    .get_program(namespace, &cert.ca, key)?
                    .with_context(|| format!("CA {namespace}/{} has no {key}", cert.ca))?
                    .0)
            };
            let issued = issue(&cert.dns_names, cert.days, &read(CERT)?, &read(KEY)?, now)?;
            let meta = Meta {
                kind: "certificate".into(),
                spec,
                ca_version: ca_version.cloned(),
                not_before: issued.not_before,
                not_after: issued.not_after,
            };
            write(store, namespace, name, issued, &meta)?;
            changed.push(format!("{namespace}/{name}: certificate {what}"));
        }
    }
    Ok(changed)
}

fn write(
    store: &SecretStore,
    namespace: &str,
    name: &str,
    issued: Issued,
    meta: &Meta,
) -> Result<()> {
    let data = BTreeMap::from([
        (CERT.to_string(), issued.cert_pem),
        (KEY.to_string(), issued.key_pem),
    ]);
    store.put_generated(namespace, name, &data, &serde_json::to_vec(meta)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::{
        stack::Stack,
        x509::{
            store::X509StoreBuilder, verify::X509VerifyParam, X509StoreContext, X509VerifyResult,
        },
    };

    fn verify(ca: &[u8], leaf: &[u8], host: &str) -> X509VerifyResult {
        let mut builder = X509StoreBuilder::new().unwrap();
        builder.add_cert(X509::from_pem(ca).unwrap()).unwrap();
        let mut param = X509VerifyParam::new().unwrap();
        param.set_host(host).unwrap();
        builder.set_param(&param).unwrap();
        let store = builder.build();
        let leaf = X509::from_pem(leaf).unwrap();
        let mut context = X509StoreContext::new().unwrap();
        context
            .init(&store, &leaf, &Stack::new().unwrap(), |c| {
                c.verify_cert()?;
                Ok(c.error())
            })
            .unwrap()
    }

    fn names() -> Vec<String> {
        ["web", "web.shop", "web.shop.svc"]
            .map(String::from)
            .to_vec()
    }

    #[test]
    fn certificates_verify_against_their_ca_for_their_names_only() {
        let now = Utc::now();
        let ca = issue_ca("shop CA", 3650, now).unwrap();
        let leaf = issue(&names(), 825, &ca.cert_pem, &ca.key_pem, now).unwrap();
        for name in names() {
            assert_eq!(
                verify(&ca.cert_pem, &leaf.cert_pem, &name),
                X509VerifyResult::OK,
                "{name}"
            );
        }
        assert_ne!(
            verify(&ca.cert_pem, &leaf.cert_pem, "other.shop.svc"),
            X509VerifyResult::OK
        );
        let stranger = issue_ca("other CA", 3650, now).unwrap();
        assert_ne!(
            verify(&stranger.cert_pem, &leaf.cert_pem, "web"),
            X509VerifyResult::OK
        );

        let text =
            String::from_utf8(X509::from_pem(&leaf.cert_pem).unwrap().to_text().unwrap()).unwrap();
        for expected in [
            "DNS:web, DNS:web.shop, DNS:web.shop.svc",
            "TLS Web Server Authentication, TLS Web Client Authentication",
            "Digital Signature",
            "CA:FALSE",
            "prime256v1",
            "Subject: CN=web",
        ] {
            assert!(text.contains(expected), "{expected}:\n{text}");
        }
        let ca_text =
            String::from_utf8(X509::from_pem(&ca.cert_pem).unwrap().to_text().unwrap()).unwrap();
        for expected in [
            "CA:TRUE, pathlen:0",
            "Certificate Sign, CRL Sign",
            "Subject: CN=shop CA",
        ] {
            assert!(ca_text.contains(expected), "{expected}:\n{ca_text}");
        }
        assert!(String::from_utf8(leaf.key_pem)
            .unwrap()
            .starts_with("-----BEGIN PRIVATE KEY-----"));
        assert_eq!((leaf.not_after - now).num_days(), 825);
    }

    #[test]
    fn certificates_never_outlive_their_ca() {
        let now = Utc::now();
        let ca = issue_ca("short", 10, now).unwrap();
        let leaf = issue(&names(), 825, &ca.cert_pem, &ca.key_pem, now).unwrap();
        assert_eq!(leaf.not_after.timestamp(), ca.not_after.timestamp());
        assert_eq!(
            not_after(&X509::from_pem(&leaf.cert_pem).unwrap())
                .unwrap()
                .timestamp(),
            ca.not_after.timestamp()
        );
    }

    #[test]
    fn reissue_is_due_when_missing_changed_reissued_by_ca_or_two_thirds_through() {
        let now = Utc::now();
        let meta = Meta {
            kind: "certificate".into(),
            spec: "s".into(),
            ca_version: Some("v1".into()),
            not_before: now - Duration::days(10),
            not_after: now + Duration::days(20),
        };
        assert_eq!(due(None, true, "s", Some("v1"), now), Some("issued"));
        assert_eq!(
            due(Some(&meta), false, "s", Some("v1"), now),
            Some("issued")
        );
        assert_eq!(
            due(Some(&meta), true, "t", Some("v1"), now),
            Some("reissued for its changed spec")
        );
        assert_eq!(
            due(Some(&meta), true, "s", Some("v2"), now),
            Some("reissued by its new CA")
        );
        assert_eq!(due(Some(&meta), true, "s", Some("v1"), now), None);
        assert_eq!(meta.renew_after(), now + Duration::days(10));
        assert_eq!(
            due(Some(&meta), true, "s", Some("v1"), now + Duration::days(10)),
            Some("renewed")
        );
        assert_eq!(
            due(Some(&meta), true, "s", Some("v1"), now + Duration::days(9)),
            None
        );
    }

    #[test]
    fn specs_ignore_name_order() {
        let cert = |names: &[&str]| ir_v2::Certificate {
            ca: "ca".into(),
            dns_names: names.iter().map(|n| n.to_string()).collect(),
            days: 30,
        };
        assert_eq!(
            certificate_spec(&cert(&["a", "b"])),
            certificate_spec(&cert(&["b", "a"]))
        );
        assert_ne!(
            certificate_spec(&cert(&["a"])),
            certificate_spec(&cert(&["a", "b"]))
        );
    }
}
