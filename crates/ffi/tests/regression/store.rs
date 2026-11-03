use std::path::Path;

use jolta_core::Jolta;

const SCHEMA_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../core/src/store");

fn schema_files() -> usize {
    std::fs::read_dir(SCHEMA_DIR)
        .unwrap()
        .flatten()
        .filter(|f| {
            let name = f.file_name().to_string_lossy().into_owned();
            name.starts_with("schema") && name.ends_with(".sql")
        })
        .count()
}

/// A data folder from the first release opens with every later migration
/// applied and its mail intact. A new schema file that isn't listed in
/// `MIGRATIONS` fails here.
#[test]
fn old_data_folders_upgrade() {
    let dir = std::env::temp_dir().join(format!("jolta-regression-upgrade-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("jolta.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&std::fs::read_to_string(Path::new(SCHEMA_DIR).join("schema.sql")).unwrap()).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(
            "INSERT INTO account (id, kind, address, display_name, username, imap_host, imap_port, smtp_host, smtp_port, secret_ref, created)
               VALUES (1, 'imap', 'me@test.invalid', 'Me', 'me', 'imap.test.invalid', 993, 'smtp.test.invalid', 465, 'me@test.invalid', 0);
             INSERT INTO mailbox (id, account_id, remote_id, name, role, selectable) VALUES (1, 1, 'INBOX', 'INBOX', 'inbox', 1);
             INSERT INTO thread (id, account_id) VALUES (1, 1);",
        )
        .unwrap();
    }

    let jolta = Jolta::open(&dir).expect("the old folder opens");
    let (version, accounts): (i64, i64) = jolta
        .store
        .read(|c| Ok((c.pragma_query_value(None, "user_version", |r| r.get(0))?, c.query_row("SELECT count(*) FROM account", [], |r| r.get(0))?)))
        .unwrap();
    assert_eq!(version as usize, schema_files(), "every schema file ran");
    assert_eq!(accounts, 1, "the account survived the upgrade");
    assert_eq!(jolta.store.read(jolta_core::store::accounts).unwrap()[0].address, "me@test.invalid");
    drop(jolta);

    Jolta::open(&dir).expect("opening again changes nothing");
    let _ = std::fs::remove_dir_all(&dir);
}
