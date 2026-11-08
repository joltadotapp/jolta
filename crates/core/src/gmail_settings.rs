//! Gmail's vacation responder, read with the mail sign-in. Changing it
//! needs `gmail.settings.basic`, a restricted scope Jolta doesn't ask for,
//! so Jolta links to Gmail's own setting instead.

use serde_json::Value;

use crate::calendar::sources::{http_client, provider_url};
use crate::{Account, Error, Result, oauth};

const VACATION: &str = "https://www.googleapis.com/gmail/v1/users/me/settings/vacation";

/// `users.settings.vacation`. A refusal names the vacation responder, not
/// the calendar as the calendar requests' errors do.
pub async fn vacation(account: &Account) -> Result<Value> {
    let token = oauth::access_token(account).await?;
    let resp = http_client(true)?
        .get(provider_url(VACATION, "www.googleapis.com")?)
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| if e.is_connect() { Error::Offline(format!("Gmail settings: {e}")) } else { Error::Other(format!("Gmail settings: {e}")) })?;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap_or_default();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    match status {
        200 => Ok(body),
        401 => Err(Error::Auth(format!("Google refused the sign-in for {}. Sign in again.", account.address))),
        _ => Err(Error::Other(format!(
            "Gmail didn't give the vacation responder for {} ({status}: {})",
            account.address,
            body["error"]["message"].as_str().unwrap_or("no reason given")
        ))),
    }
}
