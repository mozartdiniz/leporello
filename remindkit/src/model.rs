//! Reminders, lists and their related records, built from CloudKit JSON, and
//! the records Leporello writes. Field meanings are in
//! `docs/ICLOUD_REMINDERS_PROTOCOL.md`.

use std::collections::HashMap;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{crdt, document, Result};

/// Seconds between 1970-01-01 and Apple's reference date, 2001-01-01.
const APPLE_EPOCH: f64 = 978_307_200.0;
/// `minimumSupportedVersion` Reminders writes in `DueDateDeltaAlertsData`.
const DELTA_ALERTS_VERSION: i64 = 20_230_430;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct List {
    /// `List/<UUID>`
    pub record_name: String,
    pub name: String,
    /// `#RRGGBB`
    pub color: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Priority {
    #[default]
    None,
    Low,
    Medium,
    High,
}

impl Priority {
    /// EventKit values, which Reminders stores in `Priority`.
    pub fn from_raw(v: i64) -> Self {
        match v {
            1..=4 => Priority::High,
            5 => Priority::Medium,
            6..=9 => Priority::Low,
            _ => Priority::None,
        }
    }

    pub fn raw(self) -> i64 {
        match self {
            Priority::None => 0,
            Priority::High => 1,
            Priority::Medium => 5,
            Priority::Low => 9,
        }
    }
}

/// `REMDueDateDeltaInterval` units (`-[REMDueDateDeltaInterval addedTo:]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeltaUnit {
    Minute,
    Hour,
    Day,
    Week,
    Month,
}

impl DeltaUnit {
    fn from_raw(v: i64) -> Option<Self> {
        Some(match v {
            0 => DeltaUnit::Minute,
            1 => DeltaUnit::Hour,
            2 => DeltaUnit::Day,
            3 => DeltaUnit::Week,
            4 => DeltaUnit::Month,
            _ => return None,
        })
    }

    fn raw(self) -> i64 {
        self as i64
    }
}

/// Early reminder: `count` units relative to the due date; negative = before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EarlyAlert {
    pub count: i64,
    pub unit: DeltaUnit,
}

impl EarlyAlert {
    /// The options Reminders offers in its "Early Reminder" menu.
    pub fn presets() -> Vec<(EarlyAlert, &'static str)> {
        use DeltaUnit::*;
        let a = |count, unit| EarlyAlert { count, unit };
        vec![
            (a(0, Minute), "On time"),
            (a(-5, Minute), "5 minutes before"),
            (a(-15, Minute), "15 minutes before"),
            (a(-30, Minute), "30 minutes before"),
            (a(-1, Hour), "1 hour before"),
            (a(-2, Hour), "2 hours before"),
            (a(-1, Day), "1 day before"),
            (a(-2, Day), "2 days before"),
            (a(-1, Week), "1 week before"),
            (a(-1, Month), "1 month before"),
        ]
    }

    pub fn label(&self) -> String {
        if let Some((_, l)) = Self::presets().into_iter().find(|(p, _)| p == self) {
            return l.to_string();
        }
        let unit = match self.unit {
            DeltaUnit::Minute => "minute",
            DeltaUnit::Hour => "hour",
            DeltaUnit::Day => "day",
            DeltaUnit::Week => "week",
            DeltaUnit::Month => "month",
        };
        let n = self.count.abs();
        let plural = if n == 1 { "" } else { "s" };
        let side = if self.count < 0 { "before" } else { "after" };
        format!("{n} {unit}{plural} {side}")
    }

    /// When this alert fires for a reminder due at `due`.
    pub fn fire_time(&self, due: DateTime<Utc>) -> DateTime<Utc> {
        let local = due.with_timezone(&Local);
        let shifted = match self.unit {
            DeltaUnit::Minute => Some(local + chrono::Duration::minutes(self.count)),
            DeltaUnit::Hour => Some(local + chrono::Duration::hours(self.count)),
            DeltaUnit::Day => Some(local + chrono::Duration::days(self.count)),
            DeltaUnit::Week => Some(local + chrono::Duration::weeks(self.count)),
            DeltaUnit::Month => {
                let months = chrono::Months::new(self.count.unsigned_abs() as u32);
                if self.count < 0 {
                    local.checked_sub_months(months)
                } else {
                    local.checked_add_months(months)
                }
            }
        };
        shifted.map_or(due, |d| d.with_timezone(&Utc))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Due {
    /// The instant (`DueDate`). For all-day reminders only the date matters.
    pub at: DateTime<Utc>,
    pub all_day: bool,
    /// IANA zone the time was set in; `None` = floating.
    pub time_zone: Option<String>,
}

impl Due {
    /// Calendar date the reminder is due on, as the user sees it.
    pub fn local_date(&self) -> NaiveDate {
        if self.all_day {
            // All-day dates are stored as midnight UTC of the day [unverified].
            self.at.date_naive()
        } else {
            self.at.with_timezone(&Local).date_naive()
        }
    }
}

/// How a reminder repeats. Only presence is known for now; the
/// `RecurrenceRule` value encodings are still unverified.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recurrence {
    pub record_name: String,
    /// Raw fields, for display until the format is confirmed.
    pub fields: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reminder {
    /// `Reminder/<UUID>`
    pub record_name: String,
    pub change_tag: String,
    pub list: Option<String>,
    pub parent: Option<String>,
    pub title: String,
    pub notes: String,
    pub completed: bool,
    pub completion_date: Option<DateTime<Utc>>,
    pub due: Option<Due>,
    pub start_date: Option<DateTime<Utc>>,
    pub flagged: bool,
    pub priority: Priority,
    pub urls: Vec<String>,
    pub tags: Vec<String>,
    pub early_alerts: Vec<EarlyAlert>,
    /// Absolute alarm times (Date triggers), in addition to the due date.
    pub alarms: Vec<DateTime<Utc>>,
    /// Alarms with other trigger types (location, …), shown but not edited.
    pub other_alarms: Vec<String>,
    pub recurrence: Option<Recurrence>,
    /// Download URL of `UrgentPresentationAlarmsAsData`; resolve with
    /// [`crate::ICloudClient::fetch_urgent`].
    pub urgent_asset: Option<String>,
    /// `UrgentPresentationAlarmsChecksum`: changes whenever the asset does,
    /// so it keys a cache of the urgent state.
    pub urgent_checksum: Option<String>,
    pub created: Option<DateTime<Utc>>,
    pub modified: Option<DateTime<Utc>>,
}

impl Reminder {
    pub fn uuid(&self) -> &str {
        self.record_name.split_once('/').map_or(&self.record_name, |(_, id)| id)
    }

    /// Every moment Leporello should raise a notification for.
    pub fn alert_times(&self) -> Vec<DateTime<Utc>> {
        let mut out = self.alarms.clone();
        if let Some(due) = self.due.as_ref().filter(|d| !d.all_day) {
            if self.early_alerts.is_empty() {
                out.push(due.at);
            }
            out.extend(self.early_alerts.iter().map(|a| a.fire_time(due.at)));
        }
        out.sort();
        out.dedup();
        out
    }
}

/// Every live record of the `Reminders` zone, kept up to date incrementally
/// with `changes/zone`, and the reminders and lists derived from them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    pub zone_id: Value,
    pub sync_token: Option<String>,
    records: HashMap<String, Value>,
}

impl Store {
    pub fn new(zone_id: Value) -> Self {
        Store { zone_id, sync_token: None, records: HashMap::new() }
    }

    /// Apply a `changes/zone` result (or saved records from `records/modify`).
    pub fn apply(&mut self, records: Vec<Value>, sync_token: Option<String>) {
        for r in records {
            let Some(name) = r["recordName"].as_str().map(str::to_string) else { continue };
            if r["deleted"] == true || r["fields"].is_null() {
                self.records.remove(&name);
            } else {
                self.records.insert(name, r);
            }
        }
        if sync_token.is_some() {
            self.sync_token = sync_token;
        }
    }

    pub fn record(&self, name: &str) -> Option<&Value> {
        self.records.get(name)
    }

    fn live(&self, ty: &'static str) -> impl Iterator<Item = &Value> + '_ {
        self.records.values().filter(move |r| r["recordType"] == ty && int(r, "Deleted") != Some(1))
    }

    fn related(&self, ty: &str, ids: &[String]) -> Vec<&Value> {
        ids.iter().filter_map(|id| self.records.get(&format!("{ty}/{id}"))).filter(|r| int(r, "Deleted") != Some(1)).collect()
    }

    pub fn lists(&self) -> Vec<List> {
        let mut lists: Vec<List> = self
            .live("List")
            .filter(|r| int(r, "IsGroup") != Some(1))
            .map(|r| List {
                record_name: name_of(r),
                name: string(r, "Name").unwrap_or_default(),
                color: string(r, "Color")
                    .and_then(|c| serde_json::from_str::<Value>(&c).ok())
                    .and_then(|c| c["daHexString"].as_str().map(str::to_string)),
            })
            .collect();
        // Account.ListIDsMergeableOrdering holds the sidebar order (CRDT); by name for now.
        lists.sort_by_key(|l| l.name.to_lowercase());
        lists
    }

    pub fn reminders(&self) -> Vec<Reminder> {
        self.live("Reminder").filter_map(|r| self.reminder(r)).collect()
    }

    fn reminder(&self, r: &Value) -> Option<Reminder> {
        let f = &r["fields"];
        let title = doc_text(&f["TitleDocument"]).unwrap_or_default();
        let notes = doc_text(&f["NotesDocument"]).unwrap_or_default();
        let due = timestamp(r, "DueDate").map(|at| Due {
            at,
            all_day: int(r, "AllDay") == Some(1),
            time_zone: string(r, "TimeZone"),
        });

        let mut alarms = Vec::new();
        let mut other_alarms = Vec::new();
        for alarm in self.related("Alarm", &ids(r, "AlarmIDs")) {
            let Some(tid) = string(alarm, "TriggerID") else { continue };
            let Some(trigger) = self.records.get(&format!("AlarmTrigger/{tid}")) else { continue };
            match string(trigger, "Type").as_deref() {
                Some("Date") => {
                    if let Some(t) = trigger_time(trigger) {
                        alarms.push(t);
                    }
                }
                Some(other) => other_alarms.push(string(trigger, "Title").unwrap_or_else(|| other.to_string())),
                None => {}
            }
        }
        // The due-date alarm duplicates the due date; keep only extra times.
        if let Some(d) = &due {
            alarms.retain(|t| *t != d.at);
        }

        let urls = self
            .related("Attachment", &ids(r, "AttachmentIDs"))
            .into_iter()
            .filter(|a| string(a, "Type").as_deref() == Some("URL"))
            .filter_map(|a| string(a, "URL"))
            .collect();
        let tags = self.related("Hashtag", &ids(r, "HashtagIDs")).into_iter().filter_map(|h| string(h, "Name")).collect();
        let recurrence = self.related("RecurrenceRule", &ids(r, "RecurrenceRuleIDs")).first().map(|rr| Recurrence {
            record_name: name_of(rr),
            fields: rr["fields"].as_object().cloned().unwrap_or_default(),
        });

        Some(Reminder {
            record_name: name_of(r),
            change_tag: r["recordChangeTag"].as_str().unwrap_or_default().to_string(),
            list: reference(r, "List"),
            parent: reference(r, "ParentReminder"),
            title,
            notes,
            completed: int(r, "Completed") == Some(1),
            completion_date: timestamp(r, "CompletionDate"),
            due,
            start_date: timestamp(r, "StartDate"),
            flagged: int(r, "Flagged") == Some(1),
            priority: Priority::from_raw(int(r, "Priority").unwrap_or(0)),
            urls,
            tags,
            early_alerts: early_alerts(&f["DueDateDeltaAlertsData"]),
            alarms,
            other_alarms,
            recurrence,
            urgent_asset: f["UrgentPresentationAlarmsAsData"]["value"]["downloadURL"].as_str().map(|u| u.replace("${f}", "data")),
            urgent_checksum: string(r, "UrgentPresentationAlarmsChecksum"),
            created: timestamp(r, "CreationDate"),
            modified: timestamp(r, "LastModifiedDate"),
        })
    }

    /// The account UUID Reminders writes into `DueDateDeltaAlertsData`,
    /// copied from any existing reminder (its origin is unknown).
    pub fn account_identifier(&self) -> Option<String> {
        self.live("Reminder").find_map(|r| {
            let v = bytes_json(&r["fields"]["DueDateDeltaAlertsData"])?;
            v["accountIdentifier"].as_str().map(str::to_string)
        })
    }
}

// ── Writing ──────────────────────────────────────────────────────────────────

/// Everything the "new reminder" form can set.
#[derive(Debug, Clone, Default)]
pub struct Draft {
    pub list: String,
    pub title: String,
    pub notes: String,
    /// Local date, and local time unless all-day.
    pub due_date: Option<NaiveDate>,
    pub due_time: Option<(u32, u32)>,
    pub early_alert: Option<EarlyAlert>,
    pub url: Option<String>,
    pub tags: Vec<String>,
    pub flagged: bool,
    pub priority: Priority,
}

/// Changes to an existing reminder (`None` = unchanged).
#[derive(Debug, Clone, Default)]
pub struct Edits {
    pub title: Option<String>,
    pub notes: Option<String>,
    pub flagged: Option<bool>,
    pub priority: Option<Priority>,
    /// `List/<UUID>` to move the reminder to.
    pub list: Option<String>,
    pub schedule: Option<Schedule>,
    /// `Some(None)` removes the URL.
    pub url: Option<Option<String>>,
    pub tags: Option<Vec<String>>,
}

impl Edits {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.notes.is_none()
            && self.flagged.is_none()
            && self.priority.is_none()
            && self.list.is_none()
            && self.schedule.is_none()
            && self.url.is_none()
            && self.tags.is_none()
    }
}

/// Due date, time and early reminder, which share the due-date alarm.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Schedule {
    pub due_date: Option<NaiveDate>,
    pub due_time: Option<(u32, u32)>,
    pub early_alert: Option<EarlyAlert>,
}

impl Schedule {
    /// `DueDate` instant and whether it's all-day.
    pub fn due(&self) -> Option<(DateTime<Utc>, bool)> {
        let date = self.due_date?;
        Some(match self.due_time {
            Some((h, m)) => {
                let naive = NaiveDateTime::new(date, chrono::NaiveTime::from_hms_opt(h, m, 0)?);
                (Local.from_local_datetime(&naive).earliest()?.with_timezone(&Utc), false)
            }
            None => (Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?), true),
        })
    }
}

impl Draft {
    pub fn schedule(&self) -> Schedule {
        Schedule { due_date: self.due_date, due_time: self.due_time, early_alert: self.early_alert }
    }

    /// `DueDate` instant and whether it's all-day.
    pub fn due(&self) -> Option<(DateTime<Utc>, bool)> {
        self.schedule().due()
    }
}

/// Tag names as stored: no `#`, no blanks, no repeats.
fn clean_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags.iter().map(|t| t.trim().trim_start_matches('#')).filter(|t| !t.is_empty()) {
        if !out.iter().any(|o| o == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// Builds the `records/modify` operations Leporello sends.
pub struct Writer<'a> {
    pub zone_id: &'a Value,
    /// Our CRDT replica (also used in resolution tokens).
    pub replica: Uuid,
    pub account_identifier: Option<String>,
}

impl Writer<'_> {
    fn reference(&self, record_name: &str) -> Value {
        json!({ "recordName": record_name, "action": "VALIDATE", "zoneID": self.zone_id })
    }

    fn token(&self, counter: u64, now: f64) -> Value {
        json!({ "replicaID": self.replica.to_string().to_uppercase(), "counter": counter, "modificationTime": now })
    }

    /// Operations creating a reminder and its alarm, URL and tag records.
    /// Returns (reminder record name, operations).
    pub fn create(&self, d: &Draft) -> (String, Vec<Value>) {
        let id = new_id();
        let name = format!("Reminder/{id}");
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        let now_ref = apple_time(now);
        let mut ops = Vec::new();
        let mut fields = serde_json::Map::new();
        // Like icloud.com's own Reminders app, send values without a
        // `type`: iCloud applies the zone's schema (typed fields, e.g.
        // `INT64` or an empty `STRING_LIST`, are rejected as "Unexpected
        // input").
        let mut set = |k: &str, v: Value| {
            fields.insert(k.to_string(), json!({ "value": v }));
        };
        let mut tokens = vec!["titleDocument", "list", "completed", "priority", "flagged", "creationDate", "lastModifiedDate"];

        set("TitleDocument", json!(B64.encode(document::new(&d.title, self.replica))));
        if !d.notes.is_empty() {
            set("NotesDocument", json!(B64.encode(document::new(&d.notes, self.replica))));
            tokens.push("notesDocument");
        }
        set("List", self.reference(&d.list));
        set("Completed", json!(0));
        set("Priority", json!(d.priority.raw()));
        set("Flagged", json!(i64::from(d.flagged)));
        set("CreationDate", json!(now_ms));
        set("LastModifiedDate", json!(now_ms));
        set("Deleted", json!(0));
        set("Imported", json!(0));
        set("AssignmentIDs", json!([]));
        set("RecurrenceRuleIDs", json!([]));

        let mut alarm_ids = Vec::new();
        if let Some((due, all_day)) = d.due() {
            set("DueDate", json!(due.timestamp_millis()));
            set("AllDay", json!(i64::from(all_day)));
            tokens.extend(["dueDate", "allDay"]);
            if !all_day {
                let tz = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into());
                set("TimeZone", json!(tz));
                tokens.push("timeZone");
                // Reminders keeps one Date alarm at the due time.
                let (alarm_id, alarm_ops) = self.date_alarm(&name, due, &tz);
                alarm_ids.push(alarm_id);
                ops.extend(alarm_ops);
                set("DueDateDeltaAlertsData", self.delta_alerts(&id, d.early_alert));
            }
        } else {
            set("AllDay", json!(0));
        }
        set("AlarmIDs", json!(alarm_ids));

        let mut attachment_ids = Vec::new();
        if let Some(url) = d.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            let (aid, op) = self.url_attachment(&name, url);
            ops.push(op);
            attachment_ids.push(aid);
        }
        set("AttachmentIDs", json!(attachment_ids));

        let mut hashtag_ids = Vec::new();
        for tag in clean_tags(&d.tags) {
            let (hid, op) = self.hashtag(&name, &tag);
            ops.push(op);
            hashtag_ids.push(hid);
        }
        set("HashtagIDs", json!(hashtag_ids));

        let map: serde_json::Map<String, Value> = tokens.into_iter().map(|k| (k.to_string(), self.token(1, now_ref))).collect();
        set("ResolutionTokenMap", json!(json!({ "map": map }).to_string()));

        // The reminder goes first so the references from its children validate.
        ops.insert(0, create_op(&name, "Reminder", Some(&d.list), self.zone_id, Value::Object(fields)));
        (name, ops)
    }

    /// `DueDateDeltaAlertsData` for reminder `id` (bare UUID); no early
    /// reminder is written as "on time", as Reminders does.
    fn delta_alerts(&self, id: &str, alert: Option<EarlyAlert>) -> Value {
        let alert = alert.unwrap_or(EarlyAlert { count: 0, unit: DeltaUnit::Minute });
        let data = json!({
            "dueDateDeltaAlerts": [{
                "identifier": new_id(),
                "dueDateDeltaCount": alert.count,
                "dueDateDeltaUnit": alert.unit.raw(),
                "creationDate": apple_time(Utc::now()),
                "minimumSupportedAppVersion": 0,
            }],
            "reminderIdentifier": id,
            "accountIdentifier": self.account_identifier,
            "minimumSupportedVersion": DELTA_ALERTS_VERSION,
        });
        json!(B64.encode(data.to_string()))
    }

    /// A URL `Attachment` of `reminder`. Returns (bare UUID, operation).
    fn url_attachment(&self, reminder: &str, url: &str) -> (String, Value) {
        let aid = new_id();
        let op = create_op(
            &format!("Attachment/{aid}"),
            "Attachment",
            Some(reminder),
            self.zone_id,
            json!({
                "Reminder": { "value": self.reference(reminder) },
                "Type": { "value": "URL" },
                "URL": encrypted_string(url),
                "UTI": { "value": "public.url" },
                "Deleted": { "value": 0 },
                "Imported": { "value": 0 },
            }),
        );
        (aid, op)
    }

    /// A `Hashtag` use on `reminder`. Returns (bare UUID, operation).
    fn hashtag(&self, reminder: &str, tag: &str) -> (String, Value) {
        let hid = new_id();
        let op = create_op(
            &format!("Hashtag/{hid}"),
            "Hashtag",
            None,
            self.zone_id,
            json!({
                "Reminder": { "value": self.reference(reminder) },
                "Name": encrypted_string(tag),
                "Type": { "value": 0 },
                "CreationDate": { "value": Utc::now().timestamp_millis() },
                "Deleted": { "value": 0 },
                "Imported": { "value": 0 },
            }),
        );
        (hid, op)
    }

    /// Soft-delete a child record (Alarm, AlarmTrigger, Attachment, Hashtag).
    /// Like icloud.com, only `Deleted` is written; children carry no tokens.
    fn soft_delete(&self, record: &Value) -> Value {
        json!({
            "operationType": "update",
            "record": {
                "recordName": record["recordName"],
                "recordType": record["recordType"],
                "recordChangeTag": record["recordChangeTag"],
                "zoneID": self.zone_id,
                "fields": { "Deleted": { "value": 1 } },
            },
        })
    }

    /// An `Alarm` + `AlarmTrigger` (Type Date) firing at `at`.
    fn date_alarm(&self, reminder: &str, at: DateTime<Utc>, tz: &str) -> (String, Vec<Value>) {
        let alarm_id = new_id();
        let trigger_id = new_id();
        let local = at.with_timezone(&Local);
        let components = json!({
            "era": 1, "year": local.year(), "month": local.month(), "day": local.day(),
            "hour": local.hour(), "minute": local.minute(), "second": 0, "nanosecond": 0,
            "timeZone": { "identifier": tz },
        });
        let alarm = create_op(
            &format!("Alarm/{alarm_id}"),
            "Alarm",
            Some(reminder),
            self.zone_id,
            json!({
                "Reminder": { "value": self.reference(reminder) },
                "AlarmUID": { "value": new_id() },
                "TriggerID": { "value": trigger_id },
                "Deleted": { "value": 0 },
                "Imported": { "value": 0 },
            }),
        );
        let trigger = create_op(
            &format!("AlarmTrigger/{trigger_id}"),
            "AlarmTrigger",
            Some(&format!("Alarm/{alarm_id}")),
            self.zone_id,
            json!({
                "Alarm": { "value": self.reference(&format!("Alarm/{alarm_id}")) },
                "Type": { "value": "Date" },
                "DateComponentsData": { "value": B64.encode(components.to_string()) },
                "Deleted": { "value": 0 },
                "Imported": { "value": 0 },
            }),
        );
        (alarm_id, vec![alarm, trigger])
    }

    /// Update `fields` of an existing record, bumping their resolution tokens.
    /// `fields` maps CloudKit keys to `{type, value}`; `tokens` are the
    /// matching Core Data property names.
    pub fn update(&self, record: &Value, fields: serde_json::Map<String, Value>, tokens: &[&str]) -> Value {
        let now_ref = apple_time(Utc::now());
        let mut map: Value = string(record, "ResolutionTokenMap")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({ "map": {} }));
        for &key in tokens {
            let counter = map["map"][key]["counter"].as_u64().unwrap_or(0) + 1;
            map["map"][key] = self.token(counter, now_ref);
        }
        let mut fields = fields;
        fields.insert("ResolutionTokenMap".into(), json!({ "value": map.to_string() }));
        json!({
            "operationType": "update",
            "record": {
                "recordName": record["recordName"],
                "recordType": record["recordType"],
                "recordChangeTag": record["recordChangeTag"],
                "zoneID": self.zone_id,
                "fields": fields,
            },
        })
    }

    /// Operations applying `e` to the reminder `record` (empty when nothing
    /// changed). Title and notes are CRDT edits as our replica. Child records
    /// that change (due-date alarm, URL, tags) are soft-deleted and recreated,
    /// as icloud.com does; other alarms are left alone.
    pub fn edit(&self, store: &Store, record: &Value, e: &Edits) -> Result<Vec<Value>> {
        let name = name_of(record);
        let id = name.split_once('/').map_or(name.as_str(), |(_, id)| id).to_string();
        let mut f = serde_json::Map::new();
        let mut tokens = Vec::new();
        let mut children = Vec::new();
        let set = |f: &mut serde_json::Map<String, Value>, k: &str, v: Value| {
            f.insert(k.to_string(), json!({ "value": v }));
        };

        for (key, token, new) in [("TitleDocument", "titleDocument", &e.title), ("NotesDocument", "notesDocument", &e.notes)] {
            let Some(new) = new else { continue };
            let blob = record["fields"][key]["value"].as_str().and_then(|v| B64.decode(v).ok());
            let doc = match blob {
                Some(blob) => {
                    let old = document::text(&blob)?;
                    if old == *new {
                        continue;
                    }
                    crdt::edit(&blob, &old, new, self.replica)?
                }
                None if new.is_empty() => continue,
                None => document::new(new, self.replica),
            };
            set(&mut f, key, json!(B64.encode(doc)));
            tokens.push(token);
        }
        if let Some(flagged) = e.flagged.filter(|&v| int(record, "Flagged").is_none_or(|o| (o == 1) != v)) {
            set(&mut f, "Flagged", json!(i64::from(flagged)));
            tokens.push("flagged");
        }
        if let Some(p) = e.priority.filter(|p| int(record, "Priority") != Some(p.raw())) {
            set(&mut f, "Priority", json!(p.raw()));
            tokens.push("priority");
        }
        let moved_to = e.list.as_deref().filter(|l| reference(record, "List").as_deref() != Some(l));
        if let Some(list) = moved_to {
            set(&mut f, "List", self.reference(list));
            tokens.push("list");
        }

        if let Some(sched) = &e.schedule {
            let old = timestamp(record, "DueDate").map(|at| (at, int(record, "AllDay") == Some(1)));
            let new = sched.due();
            let due_changed = old != new;
            if due_changed {
                // Replace the alarm at the old due time (Reminders keeps one).
                let mut alarm_ids = ids(record, "AlarmIDs");
                if let Some((old_at, false)) = old {
                    for alarm in store.related("Alarm", &alarm_ids.clone()) {
                        let Some(trigger) = string(alarm, "TriggerID").and_then(|t| store.records.get(&format!("AlarmTrigger/{t}"))) else { continue };
                        if string(trigger, "Type").as_deref() == Some("Date") && trigger_time(trigger) == Some(old_at) {
                            children.push(self.soft_delete(alarm));
                            children.push(self.soft_delete(trigger));
                            let aid = name_of(alarm);
                            alarm_ids.retain(|i| aid != format!("Alarm/{i}"));
                        }
                    }
                }
                // There's no "remove field"; icloud.com sends null [unverified here].
                match new {
                    None => {
                        set(&mut f, "DueDate", Value::Null);
                        set(&mut f, "AllDay", json!(0));
                        set(&mut f, "TimeZone", Value::Null);
                    }
                    Some((at, all_day)) => {
                        set(&mut f, "DueDate", json!(at.timestamp_millis()));
                        set(&mut f, "AllDay", json!(i64::from(all_day)));
                        if all_day {
                            set(&mut f, "TimeZone", Value::Null);
                        } else {
                            let tz = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into());
                            set(&mut f, "TimeZone", json!(tz));
                            let (alarm_id, ops) = self.date_alarm(&name, at, &tz);
                            alarm_ids.push(alarm_id);
                            children.extend(ops);
                        }
                    }
                }
                tokens.extend(["dueDate", "allDay", "timeZone"]);
                set(&mut f, "AlarmIDs", json!(alarm_ids));
            }
            let timed = matches!(new, Some((_, false)));
            let old_alert = early_alerts(&record["fields"]["DueDateDeltaAlertsData"]).first().copied();
            if timed && (due_changed || sched.early_alert != old_alert) {
                set(&mut f, "DueDateDeltaAlertsData", self.delta_alerts(&id, sched.early_alert));
            } else if !timed && record["fields"].get("DueDateDeltaAlertsData").is_some() && due_changed {
                set(&mut f, "DueDateDeltaAlertsData", Value::Null);
            }
        }

        if let Some(url) = &e.url {
            let url = url.as_deref().map(str::trim).filter(|u| !u.is_empty());
            let mut attachment_ids = ids(record, "AttachmentIDs");
            let old: Vec<&Value> = store
                .related("Attachment", &attachment_ids)
                .into_iter()
                .filter(|a| string(a, "Type").as_deref() == Some("URL"))
                .collect();
            let old_urls: Vec<String> = old.iter().filter_map(|a| string(a, "URL")).collect();
            if old_urls != url.into_iter().map(str::to_string).collect::<Vec<_>>() {
                for a in old {
                    children.push(self.soft_delete(a));
                    let aid = name_of(a);
                    attachment_ids.retain(|i| aid != format!("Attachment/{i}"));
                }
                if let Some(url) = url {
                    let (aid, op) = self.url_attachment(&name, url);
                    children.push(op);
                    attachment_ids.push(aid);
                }
                set(&mut f, "AttachmentIDs", json!(attachment_ids));
            }
        }

        if let Some(tags) = &e.tags {
            let tags = clean_tags(tags);
            let mut hashtag_ids = ids(record, "HashtagIDs");
            let old = store.related("Hashtag", &hashtag_ids);
            let mut changed = false;
            for h in &old {
                if !string(h, "Name").is_some_and(|n| tags.contains(&n)) {
                    children.push(self.soft_delete(h));
                    let hid = name_of(h);
                    hashtag_ids.retain(|i| hid != format!("Hashtag/{i}"));
                    changed = true;
                }
            }
            let old_names: Vec<String> = old.iter().filter_map(|h| string(h, "Name")).collect();
            for tag in tags.iter().filter(|t| !old_names.contains(t)) {
                let (hid, op) = self.hashtag(&name, tag);
                children.push(op);
                hashtag_ids.push(hid);
                changed = true;
            }
            if changed {
                set(&mut f, "HashtagIDs", json!(hashtag_ids));
            }
        }

        if f.is_empty() && children.is_empty() {
            return Ok(Vec::new());
        }
        set(&mut f, "LastModifiedDate", json!(Utc::now().timestamp_millis()));
        tokens.push("lastModifiedDate");
        let mut op = self.update(record, f, &tokens);
        if let Some(list) = moved_to {
            op["record"]["parent"] = json!({ "recordName": list });
        }
        // The reminder first, so new children's references validate.
        children.insert(0, op);
        Ok(children)
    }

    /// Mark a reminder done or not done.
    pub fn set_completed(&self, record: &Value, done: bool) -> Value {
        let now_ms = Utc::now().timestamp_millis();
        let mut f = serde_json::Map::new();
        f.insert("Completed".into(), json!({ "value": i64::from(done) }));
        f.insert("LastModifiedDate".into(), json!({ "value": now_ms }));
        // CloudKit's web API has no null value; reopening leaves the old
        // CompletionDate behind, which `Completed` = 0 overrides.
        if !done {
            return self.update(record, f, &["completed", "lastModifiedDate"]);
        }
        f.insert("CompletionDate".into(), json!({ "value": now_ms }));
        self.update(record, f, &["completed", "completionDate", "lastModifiedDate"])
    }

    /// Soft-delete, as Reminders does (`Deleted` = 1; CloudKit purges later).
    pub fn delete(&self, record: &Value) -> Value {
        let mut f = serde_json::Map::new();
        f.insert("Deleted".into(), json!({ "value": 1 }));
        f.insert("LastModifiedDate".into(), json!({ "value": Utc::now().timestamp_millis() }));
        self.update(record, f, &["lastModifiedDate"])
    }
}

/// `parent` is the CloudKit parent Reminders sets (Reminder → List,
/// Alarm/Attachment → Reminder, AlarmTrigger → Alarm).
fn create_op(record_name: &str, ty: &str, parent: Option<&str>, zone_id: &Value, fields: Value) -> Value {
    let mut record = json!({ "recordName": record_name, "recordType": ty, "zoneID": zone_id, "fields": fields });
    if let Some(parent) = parent {
        record["parent"] = json!({ "recordName": parent });
    }
    json!({ "operationType": "create", "record": record })
}

/// An encrypted STRING field (Attachment.URL, Hashtag.Name). Unlike other
/// fields it needs its type, or iCloud expects base64 bytes.
fn encrypted_string(s: &str) -> Value {
    json!({ "value": s, "type": "STRING", "isEncrypted": true })
}

fn new_id() -> String {
    Uuid::new_v4().to_string().to_uppercase()
}

/// Seconds since 2001-01-01, as in resolution tokens and alert JSON.
pub fn apple_time(t: DateTime<Utc>) -> f64 {
    t.timestamp_millis() as f64 / 1000.0 - APPLE_EPOCH
}

// ── Field readers ────────────────────────────────────────────────────────────

fn name_of(r: &Value) -> String {
    r["recordName"].as_str().unwrap_or_default().to_string()
}

fn int(r: &Value, key: &str) -> Option<i64> {
    r["fields"][key]["value"].as_i64()
}

fn string(r: &Value, key: &str) -> Option<String> {
    r["fields"][key]["value"].as_str().map(str::to_string)
}

fn timestamp(r: &Value, key: &str) -> Option<DateTime<Utc>> {
    r["fields"][key]["value"].as_i64().and_then(DateTime::from_timestamp_millis)
}

fn reference(r: &Value, key: &str) -> Option<String> {
    r["fields"][key]["value"]["recordName"].as_str().map(str::to_string)
}

fn ids(r: &Value, key: &str) -> Vec<String> {
    r["fields"][key]["value"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect()
}

fn doc_text(field: &Value) -> Option<String> {
    let bytes = B64.decode(field["value"].as_str()?).ok()?;
    match document::text(&bytes) {
        Ok(t) => Some(t),
        Err(e) => {
            log::warn!("undecodable document: {e}");
            None
        }
    }
}

fn bytes_json(field: &Value) -> Option<Value> {
    let bytes = B64.decode(field["value"].as_str()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn early_alerts(field: &Value) -> Vec<EarlyAlert> {
    let Some(v) = bytes_json(field) else { return Vec::new() };
    v["dueDateDeltaAlerts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some(EarlyAlert { count: a["dueDateDeltaCount"].as_i64()?, unit: DeltaUnit::from_raw(a["dueDateDeltaUnit"].as_i64()?)? })
        })
        .collect()
}

/// Instant of a Date trigger's `DateComponentsData`, in its own time zone
/// (local zone when the components carry none).
fn trigger_time(trigger: &Value) -> Option<DateTime<Utc>> {
    let c = bytes_json(&trigger["fields"]["DateComponentsData"])?;
    let n = |k: &str| c[k].as_i64();
    let date = NaiveDate::from_ymd_opt(n("year")? as i32, n("month")? as u32, n("day")? as u32)?;
    let time = chrono::NaiveTime::from_hms_opt(n("hour").unwrap_or(0) as u32, n("minute").unwrap_or(0) as u32, n("second").unwrap_or(0) as u32)?;
    let naive = NaiveDateTime::new(date, time);
    // Zone names other than the local one would need a tz database; the
    // components almost always carry the user's own zone.
    Local.from_local_datetime(&naive).earliest().map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone() -> Value {
        json!({ "zoneName": "Reminders", "ownerRecordName": "_x" })
    }

    #[test]
    fn early_alert_parsing_and_labels() {
        let data = json!({ "dueDateDeltaAlerts": [{ "dueDateDeltaCount": -5, "dueDateDeltaUnit": 0 }] });
        let field = json!({ "type": "ENCRYPTED_BYTES", "value": B64.encode(data.to_string()) });
        let alerts = early_alerts(&field);
        assert_eq!(alerts, vec![EarlyAlert { count: -5, unit: DeltaUnit::Minute }]);
        assert_eq!(alerts[0].label(), "5 minutes before");
        assert_eq!(EarlyAlert { count: -3, unit: DeltaUnit::Day }.label(), "3 days before");
    }

    #[test]
    fn create_matches_apple_record_shape() {
        let zone = zone();
        let w = Writer { zone_id: &zone, replica: Uuid::new_v4(), account_identifier: None };
        let d = Draft {
            list: "List/L1".into(),
            title: "x".into(),
            url: Some("https://example.com".into()),
            tags: vec!["t".into()],
            ..Default::default()
        };
        let (_, ops) = w.create(&d);
        let rec = &ops[0]["record"];
        assert_eq!(rec["parent"]["recordName"], "List/L1");
        for op in &ops {
            for (k, v) in op["record"]["fields"].as_object().unwrap() {
                assert!(v.get("type").is_none() || v["isEncrypted"] == true, "{k} is sent with a type");
            }
        }
    }

    #[test]
    fn created_reminder_reads_back() {
        let z = zone();
        let w = Writer { zone_id: &z, replica: Uuid::from_bytes([3; 16]), account_identifier: Some("ACC".into()) };
        let draft = Draft {
            list: "List/L1".into(),
            title: "Listen to this podcast".into(),
            notes: "Episode 4".into(),
            due_date: NaiveDate::from_ymd_opt(2026, 10, 9),
            due_time: Some((9, 0)),
            early_alert: Some(EarlyAlert { count: -15, unit: DeltaUnit::Minute }),
            url: Some("https://open.spotify.com/episode/x".into()),
            tags: vec!["#podcasts".into()],
            flagged: true,
            priority: Priority::High,
        };
        let (name, ops) = w.create(&draft);
        // Reminder, Alarm, AlarmTrigger, Attachment, Hashtag.
        assert_eq!(ops.len(), 5);
        assert_eq!(ops[0]["record"]["recordName"], name.as_str());

        let mut store = Store::new(z.clone());
        let mut list = json!({ "recordName": "List/L1", "recordType": "List", "fields": { "Name": { "value": "Reminders" } } });
        list["recordChangeTag"] = json!("a");
        let mut records: Vec<Value> = ops.into_iter().map(|o| o["record"].clone()).collect();
        records.push(list);
        store.apply(records, None);

        let r = &store.reminders()[0];
        assert_eq!(r.title, "Listen to this podcast");
        assert_eq!(r.notes, "Episode 4");
        assert_eq!(r.list.as_deref(), Some("List/L1"));
        assert_eq!(r.urls, vec!["https://open.spotify.com/episode/x"]);
        assert_eq!(r.tags, vec!["podcasts"]);
        assert!(r.flagged);
        assert_eq!(r.priority, Priority::High);
        assert_eq!(r.early_alerts, vec![EarlyAlert { count: -15, unit: DeltaUnit::Minute }]);
        let due = r.due.as_ref().unwrap();
        assert!(!due.all_day);
        assert_eq!(due.at.with_timezone(&Local).hour(), 9);
        // The due-time alarm is folded into the due date, not listed again.
        assert!(r.alarms.is_empty());
        assert_eq!(r.alert_times(), vec![due.at - chrono::Duration::minutes(15)]);
        assert_eq!(store.lists()[0].name, "Reminders");
    }

    #[test]
    fn update_bumps_resolution_tokens() {
        let z = zone();
        let w = Writer { zone_id: &z, replica: Uuid::from_bytes([3; 16]), account_identifier: None };
        let tokens = json!({ "map": { "completed": { "counter": 4, "replicaID": "X", "modificationTime": 1.0 } } }).to_string();
        let rec = json!({
            "recordName": "Reminder/A", "recordType": "Reminder", "recordChangeTag": "t1",
            "fields": { "ResolutionTokenMap": { "type": "STRING", "value": tokens } },
        });
        let op = w.set_completed(&rec, true);
        assert_eq!(op["record"]["recordChangeTag"], "t1");
        assert_eq!(op["record"]["fields"]["Completed"]["value"], 1);
        let map: Value = serde_json::from_str(op["record"]["fields"]["ResolutionTokenMap"]["value"].as_str().unwrap()).unwrap();
        assert_eq!(map["map"]["completed"]["counter"], 5);
        assert_eq!(map["map"]["completionDate"]["counter"], 1);
        assert_eq!(map["map"]["completed"]["replicaID"], "03030303-0303-0303-0303-030303030303");
    }

    #[test]
    fn edit_changes_only_what_differs() {
        let z = zone();
        let w = Writer { zone_id: &z, replica: Uuid::from_bytes([3; 16]), account_identifier: None };
        let title = B64.encode(document::new("Buy milk", Uuid::from_bytes([9; 16])));
        let rec = json!({
            "recordName": "Reminder/A", "recordType": "Reminder", "recordChangeTag": "t1",
            "fields": {
                "TitleDocument": { "type": "ENCRYPTED_BYTES", "value": title },
                "Flagged": { "type": "INT64", "value": 0 },
                "Priority": { "type": "INT64", "value": 0 },
            },
        });
        let store = Store::new(z.clone());
        let same = Edits {
            title: Some("Buy milk".into()),
            notes: Some(String::new()),
            flagged: Some(false),
            priority: Some(Priority::None),
            schedule: Some(Schedule::default()),
            url: Some(None),
            tags: Some(vec![]),
            ..Default::default()
        };
        assert!(w.edit(&store, &rec, &same).unwrap().is_empty());

        let e = Edits { title: Some("Buy oat milk".into()), notes: Some("2 litres".into()), flagged: Some(true), ..same };
        let ops = w.edit(&store, &rec, &e).unwrap();
        assert_eq!(ops.len(), 1);
        let f = &ops[0]["record"]["fields"];
        let text = |k: &str| document::text(&B64.decode(f[k]["value"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!(text("TitleDocument"), "Buy oat milk");
        assert_eq!(text("NotesDocument"), "2 litres");
        assert_eq!(f["Flagged"]["value"], 1);
        assert!(f.get("Priority").is_none());
        let map: Value = serde_json::from_str(f["ResolutionTokenMap"]["value"].as_str().unwrap()).unwrap();
        for k in ["titleDocument", "notesDocument", "flagged", "lastModifiedDate"] {
            assert_eq!(map["map"][k]["counter"], 1, "{k}");
        }
        assert!(map["map"].get("priority").is_none());
    }

    /// Round trip through a store: create, then edit date, list, URL and tags.
    #[test]
    fn edit_replaces_children() {
        let z = zone();
        let w = Writer { zone_id: &z, replica: Uuid::from_bytes([3; 16]), account_identifier: None };
        let day = NaiveDate::from_ymd_opt(2030, 5, 1).unwrap();
        let d = Draft {
            list: "List/L1".into(),
            title: "x".into(),
            due_date: Some(day),
            due_time: Some((9, 0)),
            url: Some("https://a.example".into()),
            tags: vec!["keep".into(), "drop".into()],
            ..Default::default()
        };
        let (name, ops) = w.create(&d);
        let with_tags = |ops: &[Value]| -> Vec<Value> {
            ops.iter().map(|o| {
                let mut r = o["record"].clone();
                r["recordChangeTag"] = json!("t");
                r
            }).collect()
        };
        let mut store = Store::new(z.clone());
        store.apply(with_tags(&ops), None);
        let old_alarm = store.reminders()[0].clone();
        assert_eq!(old_alarm.due.as_ref().unwrap().local_date(), day);

        let e = Edits {
            list: Some("List/L2".into()),
            schedule: Some(Schedule { due_date: Some(day), due_time: Some((10, 30)), early_alert: Some(EarlyAlert { count: -5, unit: DeltaUnit::Minute }) }),
            url: Some(Some("https://b.example".into())),
            tags: Some(vec!["keep".into(), "#new".into()]),
            ..Default::default()
        };
        let rec = store.record(&name).unwrap().clone();
        let ops = w.edit(&store, &rec, &e).unwrap();
        let r = &ops[0]["record"];
        assert_eq!(r["recordName"], name.as_str());
        assert_eq!(r["parent"]["recordName"], "List/L2");
        let deletes = ops.iter().filter(|o| o["record"]["fields"]["Deleted"]["value"] == 1 && o["operationType"] == "update").count();
        assert_eq!(deletes, 4, "alarm, trigger, URL, #drop");

        // Fold the edit back in (updates merge their fields) and read it.
        let mut merged = Vec::new();
        for o in with_tags(&ops) {
            let mut base = store.record(o["recordName"].as_str().unwrap()).cloned().unwrap_or_else(|| o.clone());
            for (k, v) in o["fields"].as_object().unwrap() {
                base["fields"][k] = v.clone();
            }
            if let Some(p) = o.get("parent") { base["parent"] = p.clone(); }
            merged.push(base);
        }
        store.apply(merged, None);
        let r = store.reminders().into_iter().find(|r| r.record_name == name).unwrap();
        assert_eq!(r.list.as_deref(), Some("List/L2"));
        let due = r.due.unwrap();
        assert_eq!(due.at.with_timezone(&Local).hour(), 10);
        assert!(r.alarms.is_empty(), "old 9:00 alarm gone");
        assert_eq!(r.early_alerts, vec![EarlyAlert { count: -5, unit: DeltaUnit::Minute }]);
        assert_eq!(r.urls, vec!["https://b.example"]);
        let mut tags = r.tags.clone();
        tags.sort();
        assert_eq!(tags, vec!["keep", "new"]);

        // Clearing the date nulls DueDate and drops the alarm.
        let rec = store.record(&name).unwrap().clone();
        let ops = w.edit(&store, &rec, &Edits { schedule: Some(Schedule::default()), ..Default::default() }).unwrap();
        assert_eq!(ops[0]["record"]["fields"]["DueDate"]["value"], Value::Null);
        assert_eq!(ops.len(), 3);
    }
}
