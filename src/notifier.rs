//! Local alerts. Apple devices raise their own notifications; on Linux,
//! Leporello does it from the synced alarms: the due time (or each early
//! reminder relative to it) and any extra Date alarms.
//!
//! Every 30 s, alerts that came due since the last check are sent as desktop
//! notifications (portal / D-Bus via `GNotification`). Alerts missed while
//! the app wasn't running are caught up, but only from the last day.

use chrono::{DateTime, Local, Utc};
use gtk::gio;
use gtk::prelude::*;
use remindkit::Reminder;

use crate::config::State;

pub const INTERVAL_SECS: u32 = 30;
const CATCH_UP: i64 = 24 * 3600;

/// Send notifications for alerts in (`state.checked_until`, now]. `urgent`
/// tells whether a reminder is marked urgent (cached asset lookups).
pub fn check(app: &gtk::Application, reminders: &[Reminder], state: &mut State, urgent: impl Fn(&Reminder) -> bool) {
    let now = Utc::now().timestamp();
    let since = if state.checked_until == 0 { now } else { state.checked_until.max(now - CATCH_UP) };
    for r in reminders.iter().filter(|r| !r.completed) {
        for at in r.alert_times() {
            let t = at.timestamp();
            if t <= since || t > now {
                continue;
            }
            let key = format!("{}@{t}", r.record_name);
            if state.fired.insert(key) {
                send(app, r, at, urgent(r));
            }
        }
    }
    state.checked_until = now;
    // Forget alerts older than the catch-up window.
    state.fired.retain(|k| k.rsplit('@').next().and_then(|t| t.parse::<i64>().ok()).is_some_and(|t| t > now - CATCH_UP));
    state.save();
}

fn send(app: &gtk::Application, r: &Reminder, at: DateTime<Utc>, urgent: bool) {
    let n = gio::Notification::new(if r.title.is_empty() { "Reminder" } else { &r.title });
    let mut body = match &r.due {
        Some(d) if d.at != at => format!("Due {}", d.at.with_timezone(&Local).format("%a %-d %b, %H:%M")),
        _ => String::new(),
    };
    if !r.notes.is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&r.notes);
    }
    if !body.is_empty() {
        n.set_body(Some(&body));
    }
    n.set_priority(if urgent { gio::NotificationPriority::Urgent } else { gio::NotificationPriority::High });
    n.set_category(Some("reminder"));
    n.set_default_action_and_target_value("app.open-reminder", Some(&r.record_name.to_variant()));
    n.add_button_with_target_value("Mark as Completed", "app.complete-reminder", Some(&r.record_name.to_variant()));
    app.send_notification(Some(&r.record_name), &n);
    log::info!("alert: {} ({})", r.title, at.with_timezone(&Local));
}

#[cfg(test)]
mod tests {
    use super::*;
    use remindkit::{Due, Priority};

    fn reminder(due: DateTime<Utc>) -> Reminder {
        Reminder {
            record_name: "Reminder/A".into(),
            change_tag: String::new(),
            list: None,
            parent: None,
            title: "t".into(),
            notes: String::new(),
            completed: false,
            completion_date: None,
            due: Some(Due { at: due, all_day: false, time_zone: None }),
            start_date: None,
            flagged: false,
            priority: Priority::None,
            urls: vec![],
            tags: vec![],
            early_alerts: vec![],
            alarms: vec![],
            other_alarms: vec![],
            recurrence: None,
            urgent_asset: None,
            urgent_checksum: None,
            created: None,
            modified: None,
        }
    }

    #[test]
    fn due_time_is_the_alert_without_early_reminders() {
        let due = Utc::now();
        assert_eq!(reminder(due).alert_times(), vec![due]);
    }
}
