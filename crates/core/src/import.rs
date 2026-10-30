//! Adding raw RFC 822 files to the store without a server, for fixtures and tests.

use sha2::{Digest, Sha256};

use crate::model::*;
use crate::{Kite, Result, mime, store};

/// Stores `raw` as a message in `mailbox` and returns its id. The remote id
/// is the content hash, so importing the same file twice keeps one copy.
pub fn import_raw(kite: &Kite, account: AccountId, mailbox: MailboxId, raw: &[u8]) -> Result<MessageId> {
    let hash = kite.blobs.put(raw)?;
    let (envelope, body) = mime::parse_full(raw, hash.clone());
    let date = mime::header_date(raw).unwrap_or(0);
    let message = NewMessage {
        account_id: account,
        remote_id: format!("import/{}", &hex::encode(Sha256::digest(raw))[..16]),
        mailbox_id: mailbox,
        uid: None,
        date,
        envelope,
        flags: Flags::default(),
        size: raw.len() as u32,
        thread_hint: None,
        body: Some(body),
    };
    kite.store.write(|tx| Ok(store::insert_message(tx, &message)?.id()))
}
