//! The S/MIME status of received mail, and its readable content.
//!
//! macOS's CMS decoder checks each signature; the signer's certificate is
//! then checked against macOS trust with the S/MIME policy, offline, so
//! opening a thread never waits on the network for revocation data.
//! Encrypted mail is recognised but not decrypted.

use std::borrow::Cow;
use std::ptr;

use core_foundation::base::TCFType;
use core_foundation::date::CFDate;
use mail_parser::{MessageParser, MessagePart};
use security_framework::certificate::SecCertificate;
use security_framework::cms::CMSDecoder;
use security_framework::policy::SecPolicy;
use security_framework::trust::SecTrust;
use security_framework_sys::cms::{CMSDecoderCopySignerCert, CMSDecoderCopySignerStatus, CMSSignerStatus};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Protection {
    None,
    Signed(Signature),
    Encrypted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub signer_email: Option<String>,
    pub signer_name: Option<String>,
    pub verdict: Verdict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Signature intact, certificate trusted, its address is the From address.
    Trusted,
    /// Signature intact, certificate not trusted; `reason` is macOS's text.
    Untrusted { reason: String },
    /// Signature intact and trusted, but the certificate is for another address.
    OtherSender,
    /// The signature does not match the content.
    Altered,
}

/// How a message is wrapped, read from its top-level Content-Type.
enum Wrapping {
    /// `multipart/signed`: the signed entity in clear, the signature beside it.
    Clear { content: Vec<u8>, signature: Vec<u8> },
    /// `application/pkcs7-mime` signed-data: the entity inside the signature.
    Opaque(Vec<u8>),
    Encrypted,
}

const SIGNED_DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02];
const ENVELOPED_DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x03];
const AUTH_ENVELOPED_DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x10, 0x01, 0x17];

enum Outer {
    Clear,
    Opaque { smime_type: Option<String> },
}

/// What the top-level Content-Type says, read from the header block alone.
fn outer(raw: &[u8]) -> Option<Outer> {
    let headers = MessageParser::new().parse_headers(raw)?;
    let ct = headers.root().content_type()?;
    let subtype = ct.subtype().unwrap_or_default().to_ascii_lowercase();
    if ct.ctype().eq_ignore_ascii_case("multipart")
        && subtype == "signed"
        && ct.attribute("protocol").is_some_and(|p| {
            p.eq_ignore_ascii_case("application/pkcs7-signature") || p.eq_ignore_ascii_case("application/x-pkcs7-signature")
        })
    {
        return Some(Outer::Clear);
    }
    (ct.ctype().eq_ignore_ascii_case("application") && (subtype == "pkcs7-mime" || subtype == "x-pkcs7-mime"))
        .then(|| Outer::Opaque { smime_type: ct.attribute("smime-type").map(str::to_ascii_lowercase) })
}

/// Whether a message's header block says S/MIME, signed or encrypted.
pub fn is_smime(header: &[u8]) -> bool {
    outer(header).is_some()
}

fn wrapping(raw: &[u8]) -> Option<Wrapping> {
    let outer = outer(raw)?;
    let msg = MessageParser::new().parse(raw)?;
    let root = msg.root().root_part();
    let smime_type = match outer {
        Outer::Opaque { smime_type } => smime_type,
        Outer::Clear => {
            let mut parts = root.children();
            let (content, signature) = (parts.next()?, parts.next()?);
            return Some(Wrapping::Clear { content: crlf(content.raw()), signature: signature.decoded().into_owned() });
        }
    };
    match smime_type.as_deref() {
        Some("signed-data") => Some(Wrapping::Opaque(root.decoded().into_owned())),
        Some("enveloped-data" | "authenveloped-data") => Some(Wrapping::Encrypted),
        Some(_) => None,
        None => opaque_kind(&root),
    }
}

/// An `application/pkcs7-mime` part without `smime-type`, told apart by the
/// content type OID at the start of its CMS ContentInfo.
fn opaque_kind(part: &MessagePart) -> Option<Wrapping> {
    let der = part.decoded();
    match content_type_oid(&der)? {
        SIGNED_DATA => Some(Wrapping::Opaque(der.into_owned())),
        ENVELOPED_DATA | AUTH_ENVELOPED_DATA => Some(Wrapping::Encrypted),
        _ => None,
    }
}

/// The OID bytes of `ContentInfo ::= SEQUENCE { contentType OID, ... }`.
fn content_type_oid(der: &[u8]) -> Option<&[u8]> {
    if der.first() != Some(&0x30) {
        return None;
    }
    let len = *der.get(1)?;
    let at = match len {
        0x80 => 2,
        l if l < 0x80 => 2,
        l => 2 + (l & 0x7f) as usize,
    };
    if der.get(at) != Some(&0x06) {
        return None;
    }
    let n = *der.get(at + 1)? as usize;
    der.get(at + 2..at + 2 + n)
}

/// The bytes a clear signature covers use CRLF line ends (RFC 8551 3.1.1),
/// whatever the stored copy has.
fn crlf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 32);
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
}

/// The message as a reader should see it. For S/MIME-signed mail: the outer
/// headers (From, Subject and so on) on top of the signed entity, so its
/// text and attachments are the message's and the signature is not an
/// attachment. Anything else comes back as it is.
pub fn readable(raw: &[u8]) -> Cow<'_, [u8]> {
    let inner = match wrapping(raw) {
        Some(Wrapping::Clear { content, .. }) => content,
        Some(Wrapping::Opaque(der)) => match decode(&der, None).and_then(|d| d.get_content().ok()) {
            Some(content) if !content.is_empty() => content,
            _ => return Cow::Borrowed(raw),
        },
        Some(Wrapping::Encrypted) | None => return Cow::Borrowed(raw),
    };
    let mut out = outer_headers(raw);
    out.extend_from_slice(&inner);
    Cow::Owned(out)
}

/// The header block minus the fields that describe the outer body, such as
/// an opaque message's `Content-Disposition: attachment; filename=smime.p7m`.
fn outer_headers(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut keep = true;
    for line in raw.split_inclusive(|&b| b == b'\n') {
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        if !line.starts_with(b" ") && !line.starts_with(b"\t") {
            let name = line.split(|&b| b == b':').next().unwrap_or_default();
            let name = String::from_utf8_lossy(name).trim().to_ascii_lowercase();
            keep = !name.starts_with("content-") && name != "mime-version";
        }
        if keep {
            out.extend_from_slice(line);
        }
    }
    out
}

/// Whether `raw` is signed or encrypted, and what its signature is worth for
/// a message from `from_addr`.
pub fn inspect(raw: &[u8], from_addr: &str) -> Protection {
    let decoder = match wrapping(raw) {
        None => return Protection::None,
        Some(Wrapping::Encrypted) => return Protection::Encrypted,
        // A signature that carries its own content would be checked against
        // that content, not the text beside it that the reader shows.
        Some(Wrapping::Clear { signature, .. }) if decode(&signature, None).is_some_and(|d| d.get_content().is_ok_and(|c| !c.is_empty())) => None,
        Some(Wrapping::Clear { content, signature }) => decode(&signature, Some(&content)),
        Some(Wrapping::Opaque(der)) => decode(&der, None),
    };
    let Some(decoder) = decoder else {
        return Protection::Signed(Signature { signer_email: None, signer_name: None, verdict: Verdict::Altered });
    };
    match signature(&decoder, from_addr) {
        Some(s) => Protection::Signed(s),
        None => Protection::None,
    }
}

fn decode(der: &[u8], detached: Option<&[u8]>) -> Option<CMSDecoder> {
    let decoder = CMSDecoder::create().ok()?;
    decoder.update_message(der).ok()?;
    if let Some(content) = detached {
        decoder.set_detached_content(content).ok()?;
    }
    decoder.finalize_message().ok()?;
    Some(decoder)
}

/// The first signer's signature. `None` when the message has no signer.
fn signature(decoder: &CMSDecoder, from_addr: &str) -> Option<Signature> {
    if decoder.get_num_signers().ok()? == 0 {
        return None;
    }
    let (status, trust) = signer_status(decoder);
    let cert = signer_cert(decoder);
    let mut addresses: Vec<String> = cert.as_ref().and_then(|c| c.email_addresses().ok()).unwrap_or_default();
    let signer_email = addresses
        .iter()
        .find(|a| a.eq_ignore_ascii_case(from_addr))
        .or(addresses.first())
        .cloned();
    let signer_name = cert
        .map(|c| c.subject_summary())
        .filter(|n| !n.is_empty() && !signer_email.as_ref().is_some_and(|e| e.eq_ignore_ascii_case(n)));
    let verdict = match (status, trust) {
        (CMSSignerStatus::kCMSSignerValid, Some(mut trust)) => verdict(evaluate(decoder, &mut trust), &addresses, from_addr),
        (CMSSignerStatus::kCMSSignerValid, None) => verdict(Err("no certificate".into()), &addresses, from_addr),
        _ => Verdict::Altered,
    };
    Some(Signature { signer_email, signer_name, verdict })
}

/// The verdict on an intact signature. Untrusted wins over another sender:
/// an address in a certificate nobody vouches for says nothing.
fn verdict(trust: Result<(), String>, cert_addresses: &[String], from_addr: &str) -> Verdict {
    if let Err(reason) = trust {
        return Verdict::Untrusted { reason };
    }
    if cert_addresses.iter().any(|a| a.eq_ignore_ascii_case(from_addr.trim())) {
        Verdict::Trusted
    } else {
        Verdict::OtherSender
    }
}

/// Trust as of the signing time when the signature states a past one, so a
/// certificate that expired after the mail was signed still counts. No
/// network: revocation comes from what macOS has cached.
fn evaluate(decoder: &CMSDecoder, trust: &mut SecTrust) -> Result<(), String> {
    if let Ok(signed) = decoder.get_signer_signing_time(0)
        && signed <= CFDate::now().abs_time()
    {
        trust.set_trust_verify_date(&CFDate::new(signed)).map_err(|e| e.to_string())?;
    }
    trust.set_network_fetch_allowed(false).map_err(|e| e.to_string())?;
    trust.evaluate_with_error().map_err(|e| e.description().to_string().trim_end_matches('.').to_string())
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecPolicyAppleSMIME: core_foundation::string::CFStringRef;
    fn SecPolicyCreateWithProperties(
        identifier: core_foundation::base::CFTypeRef,
        properties: core_foundation::dictionary::CFDictionaryRef,
    ) -> security_framework_sys::base::SecPolicyRef;
}

fn smime_policy() -> SecPolicy {
    unsafe { SecPolicy::wrap_under_create_rule(SecPolicyCreateWithProperties(kSecPolicyAppleSMIME.cast(), ptr::null())) }
}

/// Signer 0's status without a trust evaluation (the crate's wrapper always
/// evaluates, with network access); `evaluate` does that offline instead.
fn signer_status(decoder: &CMSDecoder) -> (CMSSignerStatus, Option<SecTrust>) {
    let policy = smime_policy();
    let mut status = CMSSignerStatus::kCMSSignerUnsigned;
    let mut trust = ptr::null_mut();
    let mut cert_result = 0;
    let rc = unsafe {
        CMSDecoderCopySignerStatus(
            decoder.as_concrete_TypeRef(),
            0,
            policy.as_CFTypeRef(),
            0,
            &mut status,
            &mut trust,
            &mut cert_result,
        )
    };
    let trust = (!trust.is_null()).then(|| unsafe { SecTrust::wrap_under_create_rule(trust) });
    if rc != 0 { (CMSSignerStatus::kCMSSignerUnsigned, trust) } else { (status, trust) }
}

fn signer_cert(decoder: &CMSDecoder) -> Option<SecCertificate> {
    let mut cert = ptr::null_mut();
    let rc = unsafe { CMSDecoderCopySignerCert(decoder.as_concrete_TypeRef(), 0, &mut cert) };
    (rc == 0 && !cert.is_null()).then(|| unsafe { SecCertificate::wrap_under_create_rule(cert) })
}

impl Protection {
    /// One sentence for the reader and the CLI. `None` for unprotected mail.
    pub fn detail(&self) -> Option<String> {
        match self {
            Protection::None => None,
            Protection::Encrypted => Some("This message is encrypted. Jolta can't open encrypted mail yet.".into()),
            Protection::Signed(s) => Some(match &s.verdict {
                Verdict::Trusted => format!("Signed by {}. Certificate trusted by macOS.", s.signer()),
                Verdict::Untrusted { reason } => format!("Signed by {}. {}", s.signer(), s.untrusted(reason)),
                Verdict::OtherSender => format!("Signed by {}, not the sender.", s.signer()),
                Verdict::Altered => "The message changed after it was signed, or the signature is damaged.".into(),
            }),
        }
    }
}

impl Signature {
    fn signer(&self) -> String {
        match (&self.signer_name, &self.signer_email) {
            (Some(n), Some(e)) => format!("{n} <{e}>"),
            (None, Some(e)) => e.clone(),
            (Some(n), None) => n.clone(),
            (None, None) => "a certificate without an address".into(),
        }
    }

    /// macOS says `“Name” certificate is revoked`; the name is the signer's
    /// own certificate or one in its chain, such as a blocked intermediate.
    fn untrusted(&self, reason: &str) -> String {
        let Some((name, rest)) = reason.strip_prefix('“').and_then(|r| r.split_once("” certificate ")) else {
            return format!("The certificate isn't trusted: {reason}.");
        };
        let own = [&self.signer_name, &self.signer_email].iter().any(|n| n.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(name)));
        if own { format!("The certificate {rest}.") } else { format!("The certificate “{name}” in its chain {rest}.") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::attachments;

    const CLEAR: &[u8] = include_bytes!("../../../tests/fixtures/smime/clear-signed.eml");
    const ALTERED: &[u8] = include_bytes!("../../../tests/fixtures/smime/clear-signed-altered.eml");
    const OTHER_FROM: &[u8] = include_bytes!("../../../tests/fixtures/smime/clear-signed-other-from.eml");
    const OPAQUE: &[u8] = include_bytes!("../../../tests/fixtures/smime/opaque-signed.eml");
    const ENCRYPTED: &[u8] = include_bytes!("../../../tests/fixtures/smime/encrypted.eml");
    const PGP: &[u8] = include_bytes!("../../../tests/fixtures/smime/pgp-signed.eml");

    fn names(raw: &[u8]) -> Vec<String> {
        attachments(raw).into_iter().map(|a| a.name).collect()
    }

    fn without_smime_type(raw: &[u8]) -> Vec<u8> {
        String::from_utf8_lossy(raw).replace(" smime-type=signed-data;", "").replace(" smime-type=enveloped-data;", "").into_bytes()
    }

    fn untrusted(p: Protection) -> Signature {
        match p {
            Protection::Signed(s @ Signature { verdict: Verdict::Untrusted { .. }, .. }) => s,
            other => panic!("expected an intact signature from an untrusted CA, got {other:?}"),
        }
    }

    #[test]
    fn clear_signed_shows_the_signed_entity_and_its_signer() {
        let s = untrusted(inspect(CLEAR, "signer@example.com"));
        assert_eq!(s.signer_email.as_deref(), Some("signer@example.com"));
        assert_eq!(s.signer_name.as_deref(), Some("Test Signer"));
        assert_eq!(names(CLEAR), ["notes.txt"], "the signature is not an attachment");
        let (envelope, body) = crate::mime::parse_full(CLEAR, String::new());
        assert_eq!(envelope.subject, "Clear-signed numbers");
        assert!(body.text.contains("quarterly numbers"), "{}", body.text);
    }

    #[test]
    fn one_changed_byte_breaks_the_signature() {
        assert_eq!(
            inspect(ALTERED, "signer@example.com"),
            Protection::Signed(Signature {
                signer_email: Some("signer@example.com".into()),
                signer_name: Some("Test Signer".into()),
                verdict: Verdict::Altered
            })
        );
    }

    #[test]
    fn an_untrusted_certificate_for_another_address_stays_untrusted() {
        untrusted(inspect(OTHER_FROM, "other@example.com"));
    }

    #[test]
    fn a_trusted_certificate_must_name_the_sender() {
        let certs = ["signer@example.com".to_string()];
        assert_eq!(verdict(Ok(()), &certs, "Signer@Example.com"), Verdict::Trusted);
        assert_eq!(verdict(Ok(()), &certs, "other@example.com"), Verdict::OtherSender);
        assert_eq!(verdict(Ok(()), &[], "signer@example.com"), Verdict::OtherSender);
        assert_eq!(
            verdict(Err("expired".into()), &certs, "other@example.com"),
            Verdict::Untrusted { reason: "expired".into() }
        );
    }

    #[test]
    fn opaque_signed_mail_shows_its_inner_body() {
        for raw in [OPAQUE.to_vec(), without_smime_type(OPAQUE)] {
            let s = untrusted(inspect(&raw, "signer@example.com"));
            assert_eq!(s.signer_email.as_deref(), Some("signer@example.com"));
            assert_eq!(names(&raw), ["notes.txt"], "no smime.p7m");
            assert!(!String::from_utf8_lossy(&readable(&raw)).contains("smime.p7m"), "the outer Content-Disposition is gone");
            let (envelope, body) = crate::mime::parse_full(&raw, String::new());
            assert_eq!(envelope.from_addr, "signer@example.com");
            assert!(body.text.contains("quarterly numbers"), "{}", body.text);
        }
    }

    #[test]
    fn encrypted_mail_keeps_its_attachment() {
        for raw in [ENCRYPTED.to_vec(), without_smime_type(ENCRYPTED)] {
            assert_eq!(inspect(&raw, "signer@example.com"), Protection::Encrypted);
            assert!(matches!(readable(&raw), Cow::Borrowed(_)));
            assert_eq!(names(&raw), ["smime.p7m"]);
        }
    }

    #[test]
    fn pgp_signed_mail_is_left_alone() {
        assert_eq!(inspect(PGP, "signer@example.com"), Protection::None);
        assert!(matches!(readable(PGP), Cow::Borrowed(b) if b == PGP));
        assert_eq!(names(PGP), ["signature.asc"]);
    }

    /// The body of a message's last base64 part: its signature.
    fn signature_body(raw: &str) -> &str {
        let part = raw.rsplit_once("Content-Transfer-Encoding: base64").unwrap().1;
        let body = part.split_once("\n\n").unwrap().1;
        body.split("\n------").next().unwrap()
    }

    #[test]
    fn a_signature_carrying_its_own_content_does_not_vouch_for_the_text_beside_it() {
        let (clear, opaque) = (String::from_utf8_lossy(CLEAR), String::from_utf8_lossy(OPAQUE));
        let replay = clear.replace(signature_body(&clear), signature_body(&opaque)).replace("costs flat", "costs down");
        assert_ne!(replay, clear);
        assert!(
            matches!(inspect(replay.as_bytes(), "signer@example.com"), Protection::Signed(Signature { verdict: Verdict::Altered, .. })),
            "{:?}",
            inspect(replay.as_bytes(), "signer@example.com")
        );
    }

    #[test]
    fn untrusted_reasons_read_as_one_sentence() {
        let s = untrusted(inspect(CLEAR, "signer@example.com"));
        assert_eq!(
            Protection::Signed(s.clone()).detail().unwrap(),
            "Signed by Test Signer <signer@example.com>. The certificate is not trusted."
        );
        assert_eq!(s.untrusted("“Some CA” certificate is blocked"), "The certificate “Some CA” in its chain is blocked.");
        assert_eq!(s.untrusted("“signer@example.com” certificate is revoked"), "The certificate is revoked.");
        assert_eq!(s.untrusted("no certificate"), "The certificate isn't trusted: no certificate.");
    }

    #[test]
    fn signed_text_uses_crlf_line_ends() {
        assert_eq!(crlf(b"a\nb\r\nc\n"), b"a\r\nb\r\nc\r\n");
    }
}
