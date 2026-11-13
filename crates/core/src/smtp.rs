//! Sending over SMTP with lettre. Messages are built by `compose` and sent
//! as raw bytes; lettre only handles the transport.

use lettre::address::Envelope;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use lettre::transport::smtp::authentication::Mechanism;
use lettre::transport::smtp::client::{Tls, TlsParameters};

use crate::model::{Account, AccountKind, Security, SmtpAuth};
use crate::{Error, Result, oauth, secrets};

fn smtp_error(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("SMTP: {e}"))
}

/// A transport for the account's SMTP server with the given passwords.
fn transport(account: &Account, password: &str, smtp_password: Option<&str>) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    let host = account.smtp_host.as_str();
    let params = TlsParameters::builder(host.to_string())
        .dangerous_accept_invalid_certs(!account.verify_certs)
        .build_rustls()
        .map_err(smtp_error)?;
    let tls = match account.smtp_security {
        Security::Tls => Tls::Wrapper(params),
        Security::StartTls => Tls::Required(params),
        Security::Plain => Tls::None,
    };
    let builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host).port(account.smtp_port).tls(tls);
    let builder = match (account.kind, account.smtp_auth) {
        (AccountKind::Gmail, _) => builder
            .credentials(Credentials::new(account.username.clone(), password.to_string()))
            .authentication(vec![Mechanism::Xoauth2]),
        (_, SmtpAuth::NoAuth) => builder,
        (_, SmtpAuth::Separate) => builder
            .credentials(Credentials::new(account.smtp_username.clone(), smtp_password.unwrap_or_default().to_string()))
            .authentication(vec![Mechanism::Plain, Mechanism::Login]),
        (_, SmtpAuth::SameAsIncoming) => builder
            .credentials(Credentials::new(account.username.clone(), password.to_string()))
            .authentication(vec![Mechanism::Plain, Mechanism::Login]),
    };
    Ok(builder.build())
}

/// Connects and signs in with these settings, without sending anything.
pub async fn check(account: &Account, password: &str, smtp_password: Option<&str>) -> Result<()> {
    let ok = transport(account, password, smtp_password)?
        .test_connection()
        .await
        .map_err(|e| Error::Auth(format!("{} (SMTP): {e}", account.smtp_host)))?;
    if ok { Ok(()) } else { Err(Error::Auth(format!("{} (SMTP) did not answer", account.smtp_host))) }
}

pub async fn send(account: &Account, sender: &str, recipients: &[String], raw: &[u8]) -> Result<()> {
    let password = if account.kind == AccountKind::Gmail {
        oauth::access_token(account).await?
    } else {
        secrets::password(&account.secret_ref)?
    };
    let smtp_password = match account.smtp_auth {
        SmtpAuth::Separate => Some(secrets::password(&secrets::smtp_ref(&account.secret_ref))?),
        _ => None,
    };
    let transport = transport(account, &password, smtp_password.as_deref())?;
    let envelope = Envelope::new(
        Some(sender.parse().map_err(smtp_error)?),
        recipients.iter().map(|r| r.parse()).collect::<std::result::Result<_, _>>().map_err(smtp_error)?,
    )
    .map_err(smtp_error)?;
    transport
        .send_raw(&envelope, raw)
        .await
        .map_err(|e| {
            if e.is_permanent() {
                Error::Refused(format!("{} said: {e}", account.smtp_host))
            } else {
                Error::Unavailable(format!("SMTP: {e}"))
            }
        })?;
    Ok(())
}
