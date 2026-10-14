//! Write a made-up Reminders store for UI work without an iCloud account:
//!
//!     cargo run -p remindkit --example fixture -- ~/.cache/leporello/store.json
//!
//! Records are built with the same `Writer` the app uses, so they're shaped
//! like the ones it creates.

use chrono::{Duration, Local};
use remindkit::{DeltaUnit, Draft, EarlyAlert, Priority, Store, Writer};
use serde_json::json;
use uuid::Uuid;

fn main() {
    let path = std::env::args().nth(1).expect("usage: fixture <store.json>");
    let zone = json!({ "zoneName": "Reminders", "ownerRecordName": "_fixture" });
    let mut store = Store::new(zone.clone());
    let list = |id: &str, name: &str, color: &str| {
        json!({
            "recordName": format!("List/{id}"), "recordType": "List", "recordChangeTag": "1",
            "fields": {
                "Name": { "type": "STRING", "value": name },
                "Color": { "type": "STRING", "value": json!({ "daHexString": color }).to_string() },
            },
        })
    };
    let mut records = vec![
        list("L1", "Reminders", "#FF9500"),
        list("L2", "Rome", "#3584E4"),
        list("L3", "Groceries", "#E01B24"),
    ];
    let w = Writer { zone_id: &zone, replica: Uuid::new_v4(), account_identifier: Some("FIXTURE".into()) };
    let today = Local::now().date_naive();
    let drafts = [
        Draft {
            list: "List/L1".into(),
            title: "Listen to this podcast".into(),
            notes: "https://open.spotify.com/episode/2LGQIt1LjkgeBiQHclV6Td".into(),
            due_date: Some(today - Duration::days(4)),
            due_time: Some((9, 0)),
            url: Some("https://open.spotify.com/episode/2LGQIt1LjkgeBiQHclV6Td".into()),
            ..Default::default()
        },
        Draft {
            list: "List/L1".into(),
            title: "Testar".into(),
            due_date: Some(today - Duration::days(3)),
            due_time: Some((10, 0)),
            url: Some("https://odysseusai.dev/".into()),
            ..Default::default()
        },
        Draft {
            list: "List/L1".into(),
            title: "Pay the council tax".into(),
            due_date: Some(today),
            due_time: Some((18, 30)),
            early_alert: Some(EarlyAlert { count: -1, unit: DeltaUnit::Hour }),
            flagged: true,
            priority: Priority::High,
            tags: vec!["home".into()],
            ..Default::default()
        },
        Draft {
            list: "List/L2".into(),
            title: "Book the Vatican museums".into(),
            notes: "Early entry, 2 adults".into(),
            due_date: Some(today + Duration::days(2)),
            due_time: None,
            ..Default::default()
        },
        Draft {
            list: "List/L1".into(),
            title: "Mudar o seguro da Aviva".into(),
            due_date: Some(today + Duration::days(4)),
            due_time: Some((11, 0)),
            ..Default::default()
        },
        Draft { list: "List/L3".into(), title: "Oat milk".into(), ..Default::default() },
    ];
    for d in &drafts {
        let (_, ops) = w.create(d);
        records.extend(ops.into_iter().map(|o| {
            let mut r = o["record"].clone();
            r["recordChangeTag"] = json!("1");
            r
        }));
    }
    store.apply(records, None);
    std::fs::write(&path, serde_json::to_vec(&store).expect("serialize")).expect("write");
    println!("{} reminders written to {path}", store.reminders().len());
}
