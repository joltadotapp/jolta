//! Address suggestions for the To, Cc and Bcc fields. They come from your
//! own mail, so they work from the first sync with nothing to import: the
//! people you sent to, the people you replied to, and optionally an address
//! book the app hands over (the Mac's Contacts).

use std::collections::HashMap;

use rusqlite::{Connection, params};

use crate::model::*;
use crate::search::sort_text;
use crate::{Result, compose, store};

/// A message sent to someone counts more than replying to them, which
/// counts more than an address-book entry nobody wrote to.
const SENT: f64 = 1.0;
const REPLIED: f64 = 0.5;
const BOOK: f64 = 0.05;
/// How fast old mail stops counting: one message today weighs as much as
/// two from a month ago.
const DECAY_DAYS: f64 = 30.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    pub name: String,
    pub address: String,
}

struct Entry {
    person: Person,
    /// When `person.name` was seen; newer mail renames.
    named_at: i64,
    /// Folded name words and address parts, for prefix matching.
    words: Vec<String>,
    folded_address: String,
    score: f64,
    by_account: HashMap<AccountId, f64>,
}

/// Who a name typed in quick create is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Match {
    One(Person),
    /// None or several: the people to pick from, best first.
    Unclear(Vec<Person>),
}

/// Everyone Jolta can suggest, ranked. Build it once and query it per keystroke.
pub struct People {
    entries: Vec<Entry>,
}

/// Removes an address from suggestions until you send it mail again.
pub fn forget(conn: &Connection, address: &str, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO forgotten_address (address, at) VALUES (?1, ?2) ON CONFLICT (address) DO UPDATE SET at = ?2",
        params![address.trim(), now],
    )?;
    Ok(())
}

impl People {
    /// Reads sent and replied-to mail. `book` is (name, address) pairs from
    /// an address book.
    pub fn build(conn: &Connection, now: i64, book: &[(String, String)]) -> Result<People> {
        let mine: Vec<String> = store::own_addresses(conn)?.iter().map(|a| a.to_lowercase()).collect();
        let forgotten: HashMap<String, i64> = conn
            .prepare("SELECT lower(address), at FROM forgotten_address")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut people: HashMap<String, Entry> = HashMap::new();
        let mut add = |name: &str, address: &str, account: Option<AccountId>, date: i64, weight: f64| {
            let key = address.trim().to_lowercase();
            // `.invalid` can never receive mail (RFC 2606); synthetic test accounts use it.
            let unreachable = !key.contains('@') || key.ends_with(".invalid");
            if unreachable || mine.contains(&key) || forgotten.get(&key).is_some_and(|&at| date <= at) {
                return;
            }
            let age_days = ((now - date).max(0) as f64) / 86_400.0;
            let points = weight / (1.0 + age_days / DECAY_DAYS);
            let e = people.entry(key).or_insert_with(|| Entry {
                person: Person { name: String::new(), address: address.trim().to_string() },
                named_at: i64::MIN,
                words: Vec::new(),
                folded_address: String::new(),
                score: 0.0,
                by_account: HashMap::new(),
            });
            e.score += points;
            if let Some(account) = account {
                *e.by_account.entry(account).or_default() += points;
            }
            let name = name.trim().trim_matches(['"', '\'']).trim();
            if !name.is_empty() && !name.eq_ignore_ascii_case(address) && date >= e.named_at {
                e.person.name = name.to_string();
                e.named_at = date;
            }
        };

        let mut sent = conn.prepare(
            "SELECT m.account_id, m.date, m.to_addrs, m.cc_addrs FROM message m
             WHERE EXISTS (SELECT 1 FROM own_address a WHERE a.account_id = m.account_id AND a.address = m.from_addr COLLATE NOCASE)",
        )?;
        let mut rows = sent.query([])?;
        while let Some(r) = rows.next()? {
            let (account, date): (AccountId, i64) = (r.get(0)?, r.get(1)?);
            let list = format!("{}, {}", r.get::<_, String>(2)?, r.get::<_, String>(3)?);
            for (name, address) in compose::parse_addresses(&list) {
                add(&name, &address, Some(account), date, SENT);
            }
        }
        let mut replied = conn.prepare("SELECT account_id, date, from_name, from_addr FROM message WHERE (flags & ?1) != 0")?;
        let mut rows = replied.query([Flags::ANSWERED])?;
        while let Some(r) = rows.next()? {
            let name: String = r.get(2)?;
            let address: String = r.get(3)?;
            add(&name, &address, Some(r.get(0)?), r.get(1)?, REPLIED);
        }
        // An address book entry is named by its owner, so its name wins.
        for (name, address) in book {
            if !forgotten.contains_key(&address.trim().to_lowercase()) {
                add(name, address, None, i64::MAX, BOOK);
            }
        }

        let mut entries: Vec<Entry> = people.into_values().collect();
        for e in &mut entries {
            e.folded_address = sort_text(&e.person.address);
            let (local, domain) = e.folded_address.split_once('@').unwrap_or((&e.folded_address, ""));
            e.words = sort_text(&e.person.name)
                .split(|c: char| !c.is_alphanumeric())
                .chain(local.split(['.', '_', '-', '+']))
                .chain(std::iter::once(domain))
                .filter(|w| !w.is_empty())
                .map(str::to_string)
                .collect();
        }
        entries.sort_by(|a, b| b.score.total_cmp(&a.score));
        Ok(People { entries })
    }

    /// The best matches for what was typed: every typed word starts a word
    /// of the name or address, or the address itself. Mail written from
    /// `account` counts double, and addresses in `skip` are left out.
    pub fn suggest(&self, typed: &str, account: Option<AccountId>, skip: &[String], limit: usize) -> Vec<Person> {
        let typed = sort_text(typed.trim());
        let words: Vec<&str> = typed.split_whitespace().collect();
        if words.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(f64, &Entry)> = self
            .entries
            .iter()
            .filter(|e| !skip.iter().any(|s| s.trim().eq_ignore_ascii_case(&e.person.address)))
            .filter(|e| {
                e.folded_address.starts_with(typed.as_str())
                    || words.iter().all(|t| e.words.iter().any(|w| w.starts_with(t)))
            })
            .map(|e| (e.score + account.and_then(|a| e.by_account.get(&a)).copied().unwrap_or(0.0), e))
            .collect();
        hits.sort_by(|a, b| b.0.total_cmp(&a.0));
        hits.into_iter().take(limit).map(|(_, e)| e.person.clone()).collect()
    }

    /// The person a name means when exactly one person the user has written
    /// to has every typed word as a whole word of their name or address
    /// ("Jan" is Jan Peeters, not Janine). An address is that address. An
    /// address-book entry nobody wrote to never counts, so a guess never
    /// becomes a guest.
    pub fn resolve(&self, typed: &str) -> Match {
        let typed = typed.trim();
        if typed.contains('@') {
            let known = self.entries.iter().find(|e| e.person.address.eq_ignore_ascii_case(typed));
            return Match::One(known.map_or_else(|| Person { name: String::new(), address: typed.to_string() }, |e| e.person.clone()));
        }
        let folded = sort_text(typed);
        let words: Vec<&str> = folded.split_whitespace().collect();
        let mut clear = self.entries.iter().filter(|e| !e.by_account.is_empty() && !words.is_empty() && words.iter().all(|t| e.words.iter().any(|w| w == t)));
        match (clear.next(), clear.next()) {
            (Some(one), None) => Match::One(one.person.clone()),
            _ => Match::Unclear(self.suggest(typed, None, &[], 5)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    const NOW: i64 = 1_800_000_000;

    fn store_with_mail() -> (std::path::PathBuf, store::Store) {
        let dir = std::env::temp_dir().join(format!("jolta-people-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = store::Store::open(&dir.join("jolta.sqlite")).unwrap();
        s.write(|tx| {
            tx.execute_batch(
                "INSERT INTO account (id, kind, address, username, secret_ref, created)
                 VALUES (1, 'imap', 'me@home.test', 'me', 'a1', 0), (2, 'imap', 'me@work.test', 'me', 'a2', 0);",
            )?;
            let mail = |id: i64, account: i64, from: &str, to: &str, date: i64, flags: u32| {
                tx.execute(
                    "INSERT INTO message (id, account_id, remote_id, thread_id, from_addr, to_addrs, date, flags)
                     VALUES (?1, ?2, ?1, ?1, ?3, ?4, ?5, ?6)",
                    params![id, account, from, to, date, flags],
                )
            };
            mail(1, 1, "me@home.test", "Jan Peeters <jan@example.com>, me@work.test", NOW - DAY, 0)?;
            mail(2, 1, "me@home.test", "\"Émile Dubois\" <emile.dubois@example.org>", NOW - 400 * DAY, 0)?;
            mail(3, 1, "me@home.test", "Emile D. <emile.dubois@example.org>", NOW - 300 * DAY, 0)?;
            mail(4, 2, "ME@work.test", "Janine Claes <janine@work.test>", NOW - 2 * DAY, 0)?;
            mail(5, 1, "news@shop.test", "me@home.test", NOW - DAY, 0)?;
            mail(6, 1, "Bart@friends.test", "me@home.test", NOW - 10 * DAY, Flags::ANSWERED)?;
            Ok(())
        })
        .unwrap();
        (dir, s)
    }

    fn addresses(list: Vec<Person>) -> Vec<String> {
        list.into_iter().map(|p| p.address).collect()
    }

    #[test]
    fn an_alias_is_the_user_and_mail_from_it_counts_as_sent() {
        let (dir, s) = store_with_mail();
        s.write(|tx| {
            store::add_alias(tx, AccountId(1), "shop@home.test", "")?;
            tx.execute(
                "INSERT INTO message (id, account_id, remote_id, thread_id, from_addr, to_addrs, date, flags)
                 VALUES (7, 1, 7, 7, 'Shop@home.test', 'Greta <greta@example.com>, shop@home.test', ?1, 0)",
                [NOW - DAY],
            )?;
            Ok(())
        })
        .unwrap();
        let people = s.read(|c| People::build(c, NOW, &[])).unwrap();
        assert_eq!(addresses(people.suggest("gre", None, &[], 5)), ["greta@example.com"], "written to from the alias");
        assert!(people.suggest("shop", None, &[], 5).is_empty(), "the alias itself is never suggested");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn suggests_people_you_wrote_to_ranked_by_recency_and_account() {
        let (dir, s) = store_with_mail();
        let people = s.read(|c| People::build(c, NOW, &[])).unwrap();
        assert_eq!(addresses(people.suggest("jan", None, &[], 5)), ["jan@example.com", "janine@work.test"]);
        assert_eq!(
            addresses(people.suggest("jan", Some(AccountId(2)), &[], 5)),
            ["janine@work.test", "jan@example.com"],
            "mail from the chosen account counts double"
        );
        let emile = people.suggest("emile", None, &[], 5);
        assert_eq!(emile, [Person { name: "Emile D.".into(), address: "emile.dubois@example.org".into() }], "newest name wins");
        assert_eq!(addresses(people.suggest("Dub", None, &[], 5)), ["emile.dubois@example.org"], "address parts match");
        assert_eq!(addresses(people.suggest("jan pe", None, &[], 5)), ["jan@example.com"], "every typed word must match");
        assert_eq!(addresses(people.suggest("bart", None, &[], 5)), ["Bart@friends.test"], "senders you replied to count");
        assert!(people.suggest("news", None, &[], 5).is_empty(), "senders you never answered don't");
        assert!(people.suggest("me", None, &[], 5).is_empty(), "your own addresses don't");
        assert!(people.suggest("jan", None, &["JAN@example.com".into()], 5).iter().all(|p| p.address != "jan@example.com"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Decision 10 in the C3 spec: one clear match the user has written to
    /// becomes a guest; anything else is for the user to pick.
    #[test]
    fn resolves_a_name_only_on_one_clear_match() {
        let (dir, s) = store_with_mail();
        let book = vec![("Jan Zonder".to_string(), "zonder@example.net".to_string())];
        let people = s.read(|c| People::build(c, NOW, &book)).unwrap();
        let jan = Person { name: "Jan Peeters".into(), address: "jan@example.com".into() };
        assert_eq!(people.resolve("Jan"), Match::One(jan.clone()), "Janine and the book's Jan don't make it unclear");
        assert_eq!(people.resolve("jan peeters"), Match::One(jan));
        assert_eq!(people.resolve("Bart"), Match::One(Person { name: String::new(), address: "Bart@friends.test".into() }), "someone you replied to");
        let Match::Unclear(choices) = people.resolve("Jani") else { panic!("a prefix is a guess") };
        assert_eq!(addresses(choices), ["janine@work.test"]);
        assert_eq!(people.resolve("Zonder"), Match::Unclear(vec![Person { name: "Jan Zonder".into(), address: "zonder@example.net".into() }]), "nobody wrote to the book's entry");
        assert_eq!(people.resolve("Nobody"), Match::Unclear(Vec::new()));
        assert_eq!(people.resolve("new@example.org"), Match::One(Person { name: String::new(), address: "new@example.org".into() }));
        s.write(|tx| {
            tx.execute(
                "INSERT INTO message (id, account_id, remote_id, thread_id, from_addr, to_addrs, date) VALUES (8, 1, '8', 8, 'me@home.test', 'Jan Claes <jc@example.com>', ?1)",
                [NOW],
            )?;
            Ok(())
        })
        .unwrap();
        let people = s.read(|c| People::build(c, NOW, &[])).unwrap();
        let Match::Unclear(choices) = people.resolve("Jan") else { panic!("two Jans") };
        assert!(addresses(choices).starts_with(&["jc@example.com".to_string(), "jan@example.com".to_string()]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn forgotten_addresses_come_back_after_new_mail() {
        let (dir, s) = store_with_mail();
        s.write(|tx| forget(tx, "jan@example.com", NOW)).unwrap();
        let people = s.read(|c| People::build(c, NOW, &[])).unwrap();
        assert_eq!(addresses(people.suggest("jan", None, &[], 5)), ["janine@work.test"]);
        s.write(|tx| {
            tx.execute(
                "INSERT INTO message (id, account_id, remote_id, thread_id, from_addr, to_addrs, date)
                 VALUES (7, 1, '7', 7, 'me@home.test', 'jan@example.com', ?1)",
                [NOW + 60],
            )?;
            Ok(())
        })
        .unwrap();
        let people = s.read(|c| People::build(c, NOW + 120, &[])).unwrap();
        assert!(addresses(people.suggest("jan", None, &[], 5)).contains(&"jan@example.com".to_string()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn address_book_names_win_and_book_only_people_rank_last() {
        let (dir, s) = store_with_mail();
        let book = vec![("Jan Peeters (work)".to_string(), "jan@example.com".to_string()), ("Jan Zonder".to_string(), "zonder@example.net".to_string())];
        let people = s.read(|c| People::build(c, NOW, &book)).unwrap();
        let jan = people.suggest("jan", None, &[], 5);
        assert_eq!(jan[0], Person { name: "Jan Peeters (work)".into(), address: "jan@example.com".into() });
        assert_eq!(jan.last().unwrap().address, "zonder@example.net");
        s.write(|tx| forget(tx, "zonder@example.net", NOW)).unwrap();
        let people = s.read(|c| People::build(c, NOW, &book)).unwrap();
        assert!(people.suggest("zonder", None, &[], 5).is_empty(), "a forgotten book entry stays forgotten");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
