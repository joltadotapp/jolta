//! Leaving a mailing list in one step, from the message's own headers:
//! `List-Unsubscribe` (RFC 2369) and one-click `List-Unsubscribe-Post`
//! (RFC 8058).

use std::time::Duration;

use mail_parser::MessageParser;

use crate::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    /// An HTTPS address that unsubscribes on a POST, with no page to visit.
    OneClick(String),
    /// An email to send; the list removes whoever sends it.
    Mail { to: String, subject: String, body: String },
    /// A web page where the reader confirms by hand.
    Page(String),
}

/// The best way the message offers to leave its list: one click, then an
/// email, then a web page.
pub fn method(raw: &[u8]) -> Option<Method> {
    let msg = MessageParser::new().parse_headers(raw)?;
    let header = |name: &str| {
        msg.root().headers().iter().find(|h| h.raw_name().eq_ignore_ascii_case(name)).map(|h| {
            String::from_utf8_lossy(h.raw_value()).split_whitespace().collect::<Vec<_>>().join(" ")
        })
    };
    let list = header("List-Unsubscribe")?;
    let urls: Vec<&str> = list
        .split('<')
        .skip(1)
        .filter_map(|part| part.split_once('>').map(|(url, _)| url.trim()))
        .collect();
    let one_click = header("List-Unsubscribe-Post").is_some_and(|v| v.replace(' ', "").eq_ignore_ascii_case("List-Unsubscribe=One-Click"));
    let https = urls.iter().find(|u| u.to_ascii_lowercase().starts_with("https://"));
    if one_click {
        if let Some(url) = https {
            return Some(Method::OneClick(url.to_string()));
        }
    }
    let mail = urls
        .iter()
        .filter(|u| u.to_ascii_lowercase().starts_with("mailto:"))
        .find_map(|u| mail(&u[7..]));
    if mail.is_some() {
        return mail;
    }
    urls.iter()
        .find(|u| {
            let u = u.to_ascii_lowercase();
            u.starts_with("https://") || u.starts_with("http://")
        })
        .map(|u| Method::Page(u.to_string()))
}

/// The email for `list@example.com?subject=...` from a mailto URL. The
/// sender wrote the URL and Unsubscribe sends without a review, so it goes
/// to one address only, with Jolta's body and a short plain subject.
fn mail(rest: &str) -> Option<Method> {
    let (to, query) = rest.split_once('?').unwrap_or((rest, ""));
    let to = percent_decode(to);
    let to = to.trim();
    if to.contains([',', ';']) || to.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let [(_, to)] = crate::compose::parse_addresses(to).try_into().ok()?;
    let mut subject = String::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key.eq_ignore_ascii_case("subject") {
            subject = percent_decode(value).chars().filter(|c| !c.is_control()).take(100).collect::<String>().trim().to_string();
        }
    }
    if subject.is_empty() {
        subject = "unsubscribe".into();
    }
    Some(Method::Mail { to, subject, body: "unsubscribe".into() })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[i], bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Sends the RFC 8058 POST. No cookies and no referrer go with it.
pub async fn one_click(url: &str) -> Result<()> {
    crate::oauth::init_tls();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| Error::Other(format!("unsubscribe: {e}")))?;
    let response = client
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("List-Unsubscribe=One-Click")
        .send()
        .await
        .map_err(|e| Error::Other(format!("the list's unsubscribe address did not answer: {e}")))?;
    if response.status().is_success() || response.status().is_redirection() {
        Ok(())
    } else {
        Err(Error::Other(format!("the list refused the unsubscribe request ({})", response.status())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(headers: &str) -> Vec<u8> {
        format!("From: news@shop.test\r\nSubject: Deals\r\n{headers}\r\nbody\r\n").into_bytes()
    }

    #[test]
    fn prefers_one_click_then_mail_then_page() {
        let both = "List-Unsubscribe: <mailto:leave@shop.test?subject=stop%20please>,\r\n <https://shop.test/u?id=1>\r\n";
        assert_eq!(
            method(&raw(&format!("{both}List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n"))),
            Some(Method::OneClick("https://shop.test/u?id=1".into()))
        );
        assert_eq!(
            method(&raw(both)),
            Some(Method::Mail { to: "leave@shop.test".into(), subject: "stop please".into(), body: "unsubscribe".into() }),
            "without the Post header the https address is a page, so the email wins"
        );
        assert_eq!(
            method(&raw("List-Unsubscribe: <https://shop.test/page>\r\n")),
            Some(Method::Page("https://shop.test/page".into()))
        );
        assert_eq!(
            method(&raw("List-Unsubscribe: <http://shop.test/u>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n")),
            Some(Method::Page("http://shop.test/u".into())),
            "one-click needs https"
        );
        assert_eq!(method(&raw("")), None);
    }

    #[test]
    fn mailto_without_subject_says_unsubscribe() {
        assert_eq!(
            method(&raw("List-Unsubscribe: <mailto:list-leave%2B42@lists.test>\r\n")),
            Some(Method::Mail { to: "list-leave+42@lists.test".into(), subject: "unsubscribe".into(), body: "unsubscribe".into() })
        );
    }

    #[test]
    fn mailto_to_more_than_one_address_is_never_sent() {
        let payload = "<mailto:out@sender.test,colleague@victim.test,%20boss@victim.test?subject=Re%3A%20invoice&body=Please%20pay>";
        assert_eq!(method(&raw(&format!("List-Unsubscribe: {payload}\r\n"))), None);
        assert_eq!(
            method(&raw(&format!("List-Unsubscribe: {payload}, <https://sender.test/leave>\r\n"))),
            Some(Method::Page("https://sender.test/leave".into())),
            "an unusable mailto falls back to the page"
        );
        for to in ["out@sender.test;boss@victim.test", "out@sender.test%2Cboss@victim.test", "out@sender.test%3Bboss@victim.test"] {
            assert_eq!(method(&raw(&format!("List-Unsubscribe: <mailto:{to}>\r\n"))), None, "{to}");
        }
        let built = match method(&raw("List-Unsubscribe: <mailto:out@sender.test,boss@victim.test>\r\n")) {
            Some(Method::Mail { to, .. }) => crate::compose::parse_addresses(&to),
            _ => Vec::new(),
        };
        assert!(built.len() <= 1, "unsubscribe would write to {built:?}");
    }

    #[test]
    fn mailto_body_and_subject_come_from_jolta_not_the_sender() {
        let Some(Method::Mail { to, subject, body }) = method(&raw(
            "List-Unsubscribe: <mailto:leave@shop.test?subject=Re%3A%20invoice%0D%0ABcc%3A%20boss@victim.test&body=Please%20pay%20to%20the%20new%20account.>\r\n",
        )) else {
            panic!("a single-address mailto unsubscribes by email")
        };
        assert_eq!(to, "leave@shop.test");
        assert_eq!(body, "unsubscribe");
        assert!(!subject.chars().any(char::is_control), "{subject:?}");
        let long = "x".repeat(500);
        let Some(Method::Mail { subject, .. }) = method(&raw(&format!("List-Unsubscribe: <mailto:leave@shop.test?subject={long}>\r\n")))
        else {
            panic!()
        };
        assert!(subject.chars().count() <= 100, "{} characters", subject.chars().count());
    }

    #[test]
    fn page_must_be_http_or_https() {
        assert_eq!(method(&raw("List-Unsubscribe: <httpx://shop.test/u>\r\n")), None);
        assert_eq!(method(&raw("List-Unsubscribe: <http-evil:open>\r\n")), None);
        assert_eq!(
            method(&raw("List-Unsubscribe: <HTTP://shop.test/u>\r\n")),
            Some(Method::Page("HTTP://shop.test/u".into()))
        );
    }
}
