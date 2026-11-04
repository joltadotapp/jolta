//! `jolta cal fact`: provider facts the calendar design depends on, checked
//! against a real account. Each check
//! reads, or creates one event without attendees in the account's default
//! calendar and deletes it again, so nobody is emailed.

use anyhow::{Result, bail};
use chrono::{Duration, Utc};
use clap::ValueEnum;
use jolta_core::calendar::sources::graph::GraphCalendar;
use jolta_core::{Account, AccountKind};
use serde_json::{Value, json};

#[derive(Clone, Copy, ValueEnum)]
pub enum Fact {
    /// Graph honours If-Match on event PATCH.
    GraphIfMatch,
    /// Graph's calendarView/delta works on each calendar, not only the primary one.
    GraphDelta,
    /// Graph's getSchedule and findMeetingTimes work with the granted permissions.
    GraphSchedule,
    /// Graph moves an event between calendars without re-inviting (needs an attendee, so manual).
    GraphMove,
    /// Google's API can propose a new time (answered from the API reference).
    GoogleProposeTime,
}

pub async fn run(fact: Fact, account: Option<&Account>) -> Result<()> {
    match fact {
        Fact::GoogleProposeTime => {
            println!("No. The Events resource has no field or method for proposing a new time; an attendee can set only");
            println!("responseStatus and comment on their own entry (read 2026-11-04):");
            println!("https://developers.google.com/workspace/calendar/api/v3/reference/events");
            println!("Jolta can offer Propose new time on Google only as an email to the organizer (iMIP COUNTER).");
            Ok(())
        }
        Fact::GraphMove => {
            println!("Graph's event resource has no move action (its methods list, read 2026-11-04):");
            println!("https://learn.microsoft.com/en-us/graph/api/resources/event");
            println!("Moving a meeting through Graph means creating it in the other calendar and deleting the original.");
            println!("Whether attendees then get a cancellation and a new invitation needs a meeting with an attendee,");
            println!("so it is checked by hand: from the Microsoft 365 test account, invite another of Koen's own");
            println!("addresses, make the copy and delete the original, and see what reaches that address.");
            Ok(())
        }
        Fact::GraphIfMatch | Fact::GraphDelta | Fact::GraphSchedule => {
            let Some(account) = account else { bail!("name the Microsoft 365 account to check") };
            if account.kind != AccountKind::Graph {
                bail!("{} is not a Microsoft 365 account", account.address);
            }
            if !account.calendar {
                bail!("calendar is off for {}; run `jolta cal enable {}` first to grant calendar access", account.address, account.id);
            }
            let graph = GraphCalendar::new(account)?;
            match fact {
                Fact::GraphIfMatch => if_match(&graph).await,
                Fact::GraphDelta => delta(&graph).await,
                _ => schedule(&graph, &account.address).await,
            }
        }
    }
}

fn error_of(body: &Value) -> String {
    let e = &body["error"];
    format!("{} {}", e["code"].as_str().unwrap_or_default(), e["message"].as_str().unwrap_or_default()).trim().to_string()
}

/// Creates a 15-minute event tomorrow, patches it with a stale ETag and
/// then the right one, and deletes it whatever happened.
async fn if_match(graph: &GraphCalendar) -> Result<()> {
    let start = (Utc::now() + Duration::days(1)).date_naive().and_hms_opt(6, 0, 0).expect("a valid time");
    let at = |t: chrono::NaiveDateTime| json!({"dateTime": t.format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": "UTC"});
    let body = json!({
        "subject": "Jolta fact check (If-Match), deleted right away",
        "start": at(start),
        "end": at(start + Duration::minutes(15)),
        "isReminderOn": false,
        "showAs": "free",
    });
    let (status, created) = graph.request("POST", "/me/events", Some(&body), &[]).await?;
    let Some(id) = created["id"].as_str().map(str::to_string) else {
        bail!("creating the test event failed: {status} {}", error_of(&created));
    };
    let etag = created["@odata.etag"].as_str().unwrap_or_default().to_string();
    let url = format!("/me/events/{id}");
    let stale = graph.request("PATCH", &url, Some(&json!({"subject": "Jolta fact check, stale ETag"})), &[("If-Match", "W/\"AAAAAAAAAAAAAAAAAAAAAA==\"")]).await;
    let current = graph.request("PATCH", &url, Some(&json!({"subject": "Jolta fact check, current ETag"})), &[("If-Match", &etag)]).await;
    let (deleted, _) = graph.request("DELETE", &url, None, &[]).await?;
    println!("created event, ETag {etag}");
    match stale? {
        (412, _) => println!("PATCH with a stale ETag: 412 Precondition Failed. Graph honours If-Match."),
        (status, body) => println!("PATCH with a stale ETag: {status} {}. Graph does not honour If-Match.", error_of(&body)),
    }
    let (status, body) = current?;
    println!("PATCH with the current ETag: {status} {}", error_of(&body));
    println!("DELETE: {deleted}");
    if !(200..300).contains(&deleted) {
        bail!("the test event {id} was not deleted; delete it in Outlook");
    }
    Ok(())
}

/// calendarView/delta over one week on every calendar. Reads only.
async fn delta(graph: &GraphCalendar) -> Result<()> {
    let (status, list) = graph.request("GET", "/me/calendars?$select=id,name,isDefaultCalendar,canEdit,owner&$top=100", None, &[]).await?;
    if status != 200 {
        bail!("listing calendars: {status} {}", error_of(&list));
    }
    let from = Utc::now().date_naive();
    let range = format!("startDateTime={from}T00:00:00Z&endDateTime={}T00:00:00Z", from + Duration::days(7));
    for c in list["value"].as_array().into_iter().flatten() {
        let id = c["id"].as_str().unwrap_or_default();
        let url = format!("/me/calendars/{id}/calendarView/delta?{range}");
        let (status, body) = graph.request("GET", &url, None, &[("Prefer", "odata.maxpagesize=1")]).await?;
        let kind = if c["isDefaultCalendar"] == true { "default" } else if c["canEdit"] == true { "editable" } else { "read-only" };
        let owner = c["owner"]["address"].as_str().unwrap_or_default();
        let answer = if status == 200 { "delta works".to_string() } else { format!("{status} {}", error_of(&body)) };
        println!("{:<30} {kind:<9} owner {owner:<30} {answer}", c["name"].as_str().unwrap_or_default());
    }
    Ok(())
}

/// getSchedule for the user's own address and findMeetingTimes with no
/// attendees, over the next week. Both only read.
async fn schedule(graph: &GraphCalendar, me: &str) -> Result<()> {
    let start = Utc::now();
    let end = start + Duration::days(7);
    let at = |t: chrono::DateTime<Utc>| json!({"dateTime": t.format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": "UTC"});
    let body = json!({"schedules": [me], "startTime": at(start), "endTime": at(end), "availabilityViewInterval": 60});
    let (status, reply) = graph.request("POST", "/me/calendar/getSchedule", Some(&body), &[]).await?;
    match status {
        200 => println!("getSchedule: works ({} schedules)", reply["value"].as_array().map_or(0, Vec::len)),
        _ => println!("getSchedule: {status} {}", error_of(&reply)),
    }
    let body = json!({
        "timeConstraint": {"activityDomain": "work", "timeSlots": [{"start": at(start), "end": at(end)}]},
        "meetingDuration": "PT30M",
        "isOrganizerOptional": false,
    });
    let (status, reply) = graph.request("POST", "/me/findMeetingTimes", Some(&body), &[]).await?;
    match status {
        200 => println!(
            "findMeetingTimes: works ({} suggestions{})",
            reply["meetingTimeSuggestions"].as_array().map_or(0, Vec::len),
            reply["emptySuggestionsReason"].as_str().filter(|r| !r.is_empty()).map(|r| format!(", empty because {r}")).unwrap_or_default()
        ),
        _ => println!("findMeetingTimes: {status} {}", error_of(&reply)),
    }
    println!("Granted calendar scopes: Calendars.ReadWrite, Calendars.ReadWrite.Shared, MailboxSettings.ReadWrite.");
    Ok(())
}
