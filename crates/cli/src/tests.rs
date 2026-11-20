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
    _held: File,
}

impl Synced {
    fn new(name: &str) -> Synced {
        jolta_core::secrets::use_memory_store();
        let dir = std::env::temp_dir().join(format!("jolta-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let jolta = Jolta::open(&dir).unwrap();
        let held = File::create(dir.join("engine.lock")).unwrap();
        held.try_lock().expect("nothing else holds the new folder");
        Synced { jolta, dir, _held: held }
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
