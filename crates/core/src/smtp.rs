//! Sending over SMTP with lettre. Messages are built by `compose` and sent
//! as raw bytes; lettre only handles the transport.

use lettre::address::Envelope;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use crate::model::Account;
use crate::{Error, Result, secrets};

fn smtp_error(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("SMTP: {e}"))
}

pub async fn send(account: &Account, sender: &str, recipients: &[String], raw: &[u8]) -> Result<()> {
    let password = secrets::password(&account.secret_ref)?;
    let host = account.smtp_host.as_str();
    // Port 465 speaks TLS from the first byte; anything else upgrades with STARTTLS.
    let builder = if account.smtp_port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(host)
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
    }
    .map_err(smtp_error)?;
    let transport =
        builder.port(account.smtp_port).credentials(Credentials::new(account.username.clone(), password)).build();
    let envelope = Envelope::new(
        Some(sender.parse().map_err(smtp_error)?),
        recipients.iter().map(|r| r.parse()).collect::<std::result::Result<_, _>>().map_err(smtp_error)?,
    )
    .map_err(smtp_error)?;
    transport.send_raw(&envelope, raw).await.map_err(smtp_error)?;
    Ok(())
}
