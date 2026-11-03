//! Plain-language search: "invoices from Els last month" becomes a query in
//! Jolta's search language, which the app shows and runs. The model only
//! writes the query; the search itself runs over all mail as usual.

use super::{Ai, Cancel, Finish, Request};
use crate::{Error, Jolta, Result, reader, search};

const LANGUAGE: &str = "Query language (all terms must match):
- a word matches anywhere in the mail; \"an exact phrase\" in double quotes
- from:name-or-address  to:name-or-address  subject:word
- has:attachment
- is:unread  is:read  is:starred  is:vip  is:newsletter  is:notification  is:needs-reply
- category:personal  category:work  category:finance  category:newsletter  category:notification
- in:inbox  in:sent  in:archive  in:drafts  in:trash  in:snoozed
- after:YYYY-MM-DD (from that day on)  before:YYYY-MM-DD (until the day before)
- a - in front excludes a term, as in -is:newsletter";

/// The query for a request, checked by Jolta's parser. One retry when the
/// model writes something the parser refuses.
pub async fn query(jolta: &Jolta, ai: &Ai, request: &str, now: i64, utc_offset: i32, cancel: &Cancel) -> Result<Option<String>> {
    let settings = jolta.store.read_async(super::settings).await?;
    if settings.provider.is_none() {
        return Ok(None);
    }
    let today = reader::format_date(now, utc_offset);
    let today = today.split(',').next().unwrap_or(&today).to_string();
    let (this_month, last_month) = month_starts(&today);
    // Weeks start on Monday; unix day 0 was a Thursday.
    let local_day = (now + utc_offset as i64).div_euclid(86_400);
    let monday = local_day - (local_day + 3).rem_euclid(7);
    let ymd = |day: i64| {
        let d = reader::format_date(day * 86_400, 0);
        let d = d.split(',').next().unwrap_or_default().to_string();
        let (start, _) = month_starts(&d);
        format!("{}-{:02}", &start[..7], d.split_whitespace().next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(1))
    };
    let (this_week, last_week) = (ymd(monday), ymd(monday - 7));
    let weekday = ["Thursday", "Friday", "Saturday", "Sunday", "Monday", "Tuesday", "Wednesday"]
        [((now + utc_offset as i64).div_euclid(86_400)).rem_euclid(7) as usize];
    let system = format!(
        "You turn a request to find email into a search query for the mail app Jolta. Answer with the query only, on one line.\n\n\
         {LANGUAGE}\n\n\
         Use as few terms as you can: a person's first name or company in from:, and one or two keywords from the request, \
         in the request's language. Leave out in: unless the request names a folder. Today is {weekday} {today}.\n\n\
         Examples:\n\
         invoices from Els last month -> from:els invoice after:{last_month} before:{this_month}\n\
         what Pieter sent last week -> from:pieter after:{last_week} before:{this_week}\n\
         unread mail from my bank -> is:unread category:finance\n\
         the pdf Tom sent about billing -> from:tom has:attachment billing\n\
         what still needs an answer from me -> is:needs-reply\n\
         mails van mama over zondag -> from:mama zondag"
    );
    let request = with_dates(request.trim(), local_day, monday, &this_month, &last_month, &ymd);
    let mut user = request.clone();
    for attempt in 0..2 {
        let ask = Request {
            system: system.clone(),
            user: user.clone(),
            max_tokens: 60,
            temperature: 0.0,
            grammar: Some(r"root ::= [^\n]+".into()),
        };
        let text = match ai.generate(&settings, &ask, cancel, &mut |_| {}).await? {
            Finish::Done(text) => text,
            Finish::Cancelled => return Ok(None),
        };
        let query = clean(&text);
        match search::parse(&query) {
            Ok(q) if !q.terms.is_empty() => return Ok(Some(query)),
            Ok(_) if attempt == 0 => user = format!("{request}\n\n(Your answer was empty. Write a query.)"),
            Err(e) if attempt == 0 => user = format!("{request}\n\n(Your query {query:?} was refused: {e}. Write a correct one.)"),
            _ => {}
        }
    }
    Err(Error::Other("The model couldn't turn that into a search. Try other words.".into()))
}

/// Small models get relative dates wrong, so the request carries them
/// worked out: "last week" becomes "last week (after:2026-10-26 before:2026-11-02)".
fn with_dates(request: &str, today: i64, monday: i64, this_month: &str, last_month: &str, ymd: &dyn Fn(i64) -> String) -> String {
    let next_month = {
        let (y, m): (i32, i32) = (this_month[..4].parse().unwrap_or(2026), this_month[5..7].parse().unwrap_or(1));
        if m == 12 { format!("{}-01-01", y + 1) } else { format!("{y}-{:02}-01", m + 1) }
    };
    let ranges: [(&[&str], String, String); 6] = [
        (&["last week", "vorige week", "verleden week", "la semaine dernière"], ymd(monday - 7), ymd(monday)),
        (&["this week", "deze week", "cette semaine"], ymd(monday), ymd(monday + 7)),
        (&["last month", "vorige maand", "verleden maand", "le mois dernier"], last_month.into(), this_month.into()),
        (&["this month", "deze maand", "ce mois"], this_month.into(), next_month),
        (&["yesterday", "gisteren", "hier"], ymd(today - 1), ymd(today)),
        (&["today", "vandaag", "aujourd'hui"], ymd(today), ymd(today + 1)),
    ];
    let lower = request.to_lowercase();
    let mut out = request.to_string();
    for (phrases, after, before) in ranges {
        if let Some(p) = phrases.iter().find(|p| {
            lower.match_indices(**p).any(|(i, _)| {
                let ok_before = lower[..i].chars().last().is_none_or(|c| !c.is_alphanumeric());
                let ok_after = lower[i + p.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric());
                ok_before && ok_after
            })
        }) {
            out.push_str(&format!(" ({p}: after:{after} before:{before})"));
        }
    }
    out
}

/// The first days of this month and last month, as YYYY-MM-DD, from a date
/// like "2 Nov 2026".
fn month_starts(today: &str) -> (String, String) {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let parts: Vec<&str> = today.split_whitespace().collect();
    let month = parts.get(1).and_then(|m| MONTHS.iter().position(|x| x == m)).unwrap_or(0) as i32 + 1;
    let year: i32 = parts.get(2).and_then(|y| y.parse().ok()).unwrap_or(2026);
    let (last_year, last) = if month == 1 { (year - 1, 12) } else { (year, month - 1) };
    (format!("{year}-{month:02}-01"), format!("{last_year}-{last:02}-01"))
}

/// The query line without the quoting and labels models add around it.
fn clean(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let line = line.trim_matches('`').trim();
    let line = line.strip_prefix("Query:").or_else(|| line.strip_prefix("query:")).unwrap_or(line).trim();
    line.strip_prefix("->").unwrap_or(line).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_starts_cross_the_year() {
        assert_eq!(month_starts("2 Nov 2026"), ("2026-11-01".into(), "2026-10-01".into()));
        assert_eq!(month_starts("5 Jan 2027"), ("2027-01-01".into(), "2026-12-01".into()));
    }

    #[test]
    fn spells_out_relative_dates() {
        let ymd = |d: i64| format!("day{d}");
        let out = with_dates("rust newsletter last week", 20, 18, "2026-11-01", "2026-10-01", &ymd);
        assert_eq!(out, "rust newsletter last week (last week: after:day11 before:day18)");
        let out = with_dates("facturen van vorige maand", 20, 18, "2026-12-01", "2026-11-01", &ymd);
        assert_eq!(out, "facturen van vorige maand (vorige maand: after:2026-11-01 before:2026-12-01)");
        assert_eq!(with_dates("hierarchy notes", 20, 18, "2026-11-01", "2026-10-01", &ymd), "hierarchy notes");
    }

    #[test]
    fn strips_wrapping() {
        assert_eq!(clean("`from:els invoice`\n"), "from:els invoice");
        assert_eq!(clean("Query: is:unread category:finance"), "is:unread category:finance");
        assert_eq!(clean("\n-> from:tom has:attachment"), "from:tom has:attachment");
    }
}
