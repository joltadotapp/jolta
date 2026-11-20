//! While Jolta.app or `jolta watch` syncs the data folder, a `jolta` command
//! must not write to the servers or save sync progress beside it.

use std::fs::File;

use chrono::Utc;
use jolta_core::Jolta;
use jolta_core::calendar::ops::{self, CalendarOpId, Change, Edit, OpState};
use jolta_core::calendar::series::Horizon;
use jolta_core::calendar::{EventData, Local, Span, Zone, tz};
use jolta_core::store;

/// A data folder another process syncs: it holds the lock the engine takes.
struct Synced {
    jolta: Jolta,
    dir: std::path::PathBuf,
    held: Option<File>,
}

impl Synced {
    fn new(name: &str) -> Synced {
        jolta_core::secrets::use_memory_store();
        let dir = std::env::temp_dir().join(format!("jolta-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let jolta = Jolta::open(&dir).unwrap();
        let held = File::create(dir.join("engine.lock")).unwrap();
        held.try_lock().expect("nothing else holds the new folder");
        Synced { jolta, dir, held: Some(held) }
    }

    fn queue_local_edit(&self) -> CalendarOpId {
        let here = tz::here();
        let calendar = self.jolta.store.write(|tx| ops::new_local_calendar(tx, "Test", None)).unwrap();
        let wall = (Utc::now() + chrono::Duration::days(1)).with_timezone(&here).naive_local();
        let start = Local { wall, zone: Zone::Tz(here) };
        let event = EventData { title: "Lunch".into(), ..EventData::new("lunch@test", Span::Timed { start, end: Local { wall: wall + chrono::Duration::minutes(30), ..start } }) };
        let edit = Edit::quiet(calendar.id, Change::Create(vec![event]));
        self.jolta.store.write(|tx| ops::queue(tx, &edit, Utc::now().timestamp(), &Horizon::around(Utc::now(), here))).unwrap().unwrap()
    }

    /// The other program stops syncing after `secs`.
    fn release_after(&mut self, secs: u64) -> std::thread::JoinHandle<()> {
        let held = self.held.take();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            drop(held);
        })
    }

    fn state(&self, op: CalendarOpId) -> OpState {
        self.jolta.store.read(|c| store::calendar_op(c, op)).unwrap().state
    }
}

impl Drop for Synced {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_calendar_edit_leaves_its_op_to_the_program_that_syncs() {
    let folder = Synced::new("edit");
    let op = folder.queue_local_edit();
    crate::cal::send_calendar_ops(&folder.jolta).await.expect("the edit is saved, so the command succeeds");
    assert_eq!(folder.state(op), OpState::Pending, "the program that syncs the folder sends the op, not this command");
}

#[tokio::test(flavor = "multi_thread")]
async fn ops_send_refuses_while_another_program_syncs() {
    let folder = Synced::new("ops-send");
    let op = folder.queue_local_edit();
    let refused = crate::cal_edit::list_ops(&folder.jolta, true).await.expect_err("jolta cal ops --send refuses");
    assert!(refused.to_string().contains("is syncing this data folder"), "{refused}");
    assert_eq!(folder.state(op), OpState::Pending);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_shot_sync_refuses_while_another_program_syncs() {
    let folder = Synced::new("sync");
    let refused = crate::sync(&folder.jolta, None, false).await.expect_err("jolta sync refuses");
    assert!(refused.to_string().contains("is syncing this data folder"), "{refused}");
    let refused = crate::cal::run(&folder.jolta, crate::cal::CalCommand::Sync { calendar: None }).await.expect_err("jolta cal sync refuses");
    assert!(refused.to_string().contains("is syncing this data folder"), "{refused}");
}

/// The commands that write to a provider directly, then sync the calendar
/// or replay the queue, refuse before writing anything.
#[tokio::test(flavor = "multi_thread")]
async fn direct_calendar_writes_refuse_while_another_program_syncs() {
    use crate::cal::{CalCommand, DeclineArg, OutsideArg};
    let folder = Synced::new("direct");
    let away = |from: Option<&str>, off: bool| CalCommand::Away {
        account: Some("me@test.invalid".into()),
        from: from.map(str::to_string),
        until: from.map(str::to_string),
        message: String::new(),
        decline: DeclineArg::None,
        outside: OutsideArg::All,
        off,
    };
    let commands = [
        ("cal add", CalCommand::Add { message: 1, calendar: None, list: false }),
        ("cal update", CalCommand::Update { message: 1 }),
        ("cal remove", CalCommand::Remove { message: 1 }),
        ("cal enable", CalCommand::Enable { account: "me@test.invalid".into(), port: 0 }),
        ("cal away --from", away(Some("2026-12-01"), false)),
        ("cal away --off", away(None, true)),
    ];
    for (name, command) in commands {
        let refused = crate::cal::run(&folder.jolta, command).await.expect_err(name);
        assert!(refused.to_string().contains("is syncing this data folder"), "{name}: {refused}");
    }
}

/// With --wait, an edit waits for a program that only held the folder for
/// a while, then sends its op itself.
#[tokio::test(flavor = "multi_thread")]
async fn a_waiting_edit_sends_once_the_folder_is_free() {
    let mut folder = Synced::new("wait");
    let op = folder.queue_local_edit();
    let sync_ends = folder.release_after(2);
    crate::cal_edit::settle_op(&folder.jolta, op, true).await.expect("sent once the folder is free");
    sync_ends.join().unwrap();
    assert_eq!(folder.state(op), OpState::Done, "the command waited for the folder and sent the op");
}

/// With --wait, an edit gives up on a folder that stays busy, and says the
/// change is saved and still queued.
#[tokio::test(flavor = "multi_thread")]
async fn a_waiting_edit_fails_when_the_folder_stays_busy() {
    let folder = Synced::new("wait-busy");
    let op = folder.queue_local_edit();
    let failed = crate::cal_edit::wait_for_op(&folder.jolta, op, std::time::Duration::from_secs(1)).await.expect_err("the op wasn't sent");
    assert!(failed.to_string().starts_with("The change is saved, but still queued"), "{failed}");
    assert_eq!(folder.state(op), OpState::Pending);
}
