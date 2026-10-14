//! The reminder inspector, laid out like Reminders.app's: title, notes, URL;
//! Date & Time (date, time, urgent); repeat and early reminder;
//! organisation (list, tags); flag and priority.
//!
//! `new_reminder` builds an editable form for a new reminder. `show_reminder`
//! shows an existing one read-only, with Complete and Delete: editing an
//! existing reminder means CRDT edits and alarm rewrites that aren't wired up yet.

use std::rc::Rc;

use adw::prelude::*;
use chrono::{Datelike, Local, NaiveDate, Timelike};
use gtk::glib;
use remindkit::{Draft, EarlyAlert, Edits, List, Priority, Reminder, Schedule};

const PRIORITIES: [(Priority, &str); 4] =
    [(Priority::None, "None"), (Priority::Low, "Low"), (Priority::Medium, "Medium"), (Priority::High, "High")];

struct Form {
    dialog: adw::Dialog,
    header: adw::HeaderBar,
    page: adw::PreferencesPage,
    title: adw::EntryRow,
    notes: gtk::TextView,
    url: adw::EntryRow,
    date: adw::ExpanderRow,
    calendar: gtk::Calendar,
    time: adw::ExpanderRow,
    hour: adw::SpinRow,
    minute: adw::SpinRow,
    urgent: adw::SwitchRow,
    repeat: adw::ComboRow,
    early: adw::ComboRow,
    list: adw::ComboRow,
    tags: adw::EntryRow,
    flag: adw::SwitchRow,
    priority: adw::ComboRow,
    lists: Vec<List>,
}

fn combo(title: &str, items: &[&str]) -> adw::ComboRow {
    let row = adw::ComboRow::new();
    row.set_title(title);
    row.set_model(Some(&gtk::StringList::new(items)));
    row
}

fn early_items() -> Vec<String> {
    std::iter::once("None".to_string()).chain(EarlyAlert::presets().into_iter().map(|(_, l)| l.to_string())).collect()
}

fn build(title: &str, lists: &[List]) -> Form {
    let page = adw::PreferencesPage::new();

    let main = adw::PreferencesGroup::new();
    let title_row = adw::EntryRow::new();
    title_row.set_title("Title");
    main.add(&title_row);
    let notes = gtk::TextView::new();
    notes.set_wrap_mode(gtk::WrapMode::WordChar);
    notes.set_top_margin(10);
    notes.set_bottom_margin(10);
    notes.set_left_margin(12);
    notes.set_right_margin(12);
    notes.add_css_class("inline");
    let notes_frame = gtk::ListBoxRow::new();
    notes_frame.set_activatable(false);
    let notes_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let notes_label = gtk::Label::new(Some("Notes"));
    notes_label.add_css_class("dim-label");
    notes_label.add_css_class("caption");
    notes_label.set_xalign(0.0);
    notes_label.set_margin_start(12);
    notes_label.set_margin_top(8);
    notes_box.append(&notes_label);
    notes.set_size_request(-1, 72);
    notes_box.append(&notes);
    notes_frame.set_child(Some(&notes_box));
    main.add(&notes_frame);
    let url = adw::EntryRow::new();
    url.set_title("URL");
    url.set_input_purpose(gtk::InputPurpose::Url);
    main.add(&url);
    page.add(&main);

    let when = adw::PreferencesGroup::new();
    when.set_title("Date &amp; Time");
    let date = adw::ExpanderRow::new();
    date.set_title("Date");
    date.set_show_enable_switch(true);
    date.set_enable_expansion(false);
    let calendar = gtk::Calendar::new();
    calendar.set_margin_top(6);
    calendar.set_margin_bottom(6);
    date.add_row(&calendar);
    when.add(&date);
    let time = adw::ExpanderRow::new();
    time.set_title("Time");
    time.set_show_enable_switch(true);
    time.set_enable_expansion(false);
    let hour = adw::SpinRow::with_range(0.0, 23.0, 1.0);
    hour.set_title("Hour");
    let minute = adw::SpinRow::with_range(0.0, 59.0, 5.0);
    minute.set_title("Minute");
    time.add_row(&hour);
    time.add_row(&minute);
    when.add(&time);
    let urgent = adw::SwitchRow::new();
    urgent.set_title("Urgent");
    urgent.set_subtitle("Shown as an urgent alert. Set it on an Apple device for now.");
    urgent.set_sensitive(false);
    when.add(&urgent);
    page.add(&when);

    let alerts = adw::PreferencesGroup::new();
    let repeat = combo("Repeat", &["Never"]);
    repeat.set_subtitle("Repeating reminders can't be created here yet");
    repeat.set_sensitive(false);
    alerts.add(&repeat);
    let early_strings = early_items();
    let early = combo("Early Reminder", &early_strings.iter().map(String::as_str).collect::<Vec<_>>());
    alerts.add(&early);
    page.add(&alerts);

    let org = adw::PreferencesGroup::new();
    org.set_title("Organisation");
    let names: Vec<&str> = lists.iter().map(|l| l.name.as_str()).collect();
    let list = combo("List", &names);
    org.add(&list);
    let tags = adw::EntryRow::new();
    tags.set_title("Tags (space-separated)");
    org.add(&tags);
    page.add(&org);

    let marks = adw::PreferencesGroup::new();
    let flag = adw::SwitchRow::new();
    flag.set_title("Flag");
    marks.add(&flag);
    let priority = combo("Priority", &PRIORITIES.map(|(_, l)| l));
    marks.add(&priority);
    page.add(&marks);

    let header = adw::HeaderBar::new();
    header.set_show_end_title_buttons(false);
    header.set_show_start_title_buttons(false);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&page));

    let dialog = adw::Dialog::new();
    dialog.set_title(title);
    dialog.set_content_width(460);
    dialog.set_content_height(760);
    dialog.set_child(Some(&view));

    // Time only makes sense with a date; date/time subtitles follow the pickers.
    let sync = {
        let (date, time, calendar, hour, minute) =
            (date.clone(), time.clone(), calendar.clone(), hour.clone(), minute.clone());
        move || {
            let d = calendar.date();
            date.set_subtitle(&format!("{}", d.format("%A, %-d %B %Y").unwrap_or_default()));
            time.set_subtitle(&format!("{:02}:{:02}", hour.value() as u32, minute.value() as u32));
            time.set_sensitive(date.enables_expansion());
            if !date.enables_expansion() {
                time.set_enable_expansion(false);
            }
        }
    };
    let sync = Rc::new(sync);
    for s in [&date, &time] {
        let sync = sync.clone();
        s.connect_enable_expansion_notify(move |_| sync());
    }
    {
        let sync = sync.clone();
        calendar.connect_day_selected(move |_| sync());
    }
    for s in [&hour, &minute] {
        let sync = sync.clone();
        s.connect_value_notify(move |_| sync());
    }
    sync();

    Form {
        dialog,
        header,
        page,
        title: title_row,
        notes,
        url,
        date,
        calendar,
        time,
        hour,
        minute,
        urgent,
        repeat,
        early,
        list,
        tags,
        flag,
        priority,
        lists: lists.to_vec(),
    }
}

impl Form {
    fn draft(&self) -> Draft {
        let buf = self.notes.buffer();
        let notes = buf.text(&buf.start_iter(), &buf.end_iter(), false).trim().to_string();
        let due_date = self.date.enables_expansion().then(|| {
            let d = self.calendar.date();
            NaiveDate::from_ymd_opt(d.year(), d.month() as u32, d.day_of_month() as u32)
        });
        let early = self.early.selected() as usize;
        Draft {
            list: self.lists.get(self.list.selected() as usize).map(|l| l.record_name.clone()).unwrap_or_default(),
            title: self.title.text().trim().to_string(),
            notes,
            due_date: due_date.flatten(),
            due_time: self
                .time
                .enables_expansion()
                .then(|| (self.hour.value() as u32, self.minute.value() as u32)),
            early_alert: early.checked_sub(1).and_then(|i| EarlyAlert::presets().get(i).map(|(a, _)| *a)),
            url: Some(self.url.text().to_string()).filter(|u| !u.trim().is_empty()),
            tags: self.tags.text().split_whitespace().map(str::to_string).collect(),
            flagged: self.flag.is_active(),
            priority: PRIORITIES[self.priority.selected() as usize].0,
        }
    }
}

/// Editable form for a new reminder in `default_list`; `on_add` gets the draft.
pub fn new_reminder(
    parent: &impl IsA<gtk::Widget>,
    lists: &[List],
    default_list: Option<&str>,
    on_add: impl Fn(Draft) + 'static,
) {
    let f = Rc::new(build("New Reminder", lists));
    if let Some(i) = default_list.and_then(|d| lists.iter().position(|l| l.record_name == d)) {
        f.list.set_selected(i as u32);
    }
    // Start with tomorrow 09:00, like Reminders' "Tomorrow" suggestion.
    let tomorrow = Local::now() + chrono::Duration::days(1);
    if let Ok(d) = glib::DateTime::from_local(tomorrow.year(), tomorrow.month() as i32, tomorrow.day() as i32, 9, 0, 0.0) {
        f.calendar.select_day(&d);
    }
    f.hour.set_value(9.0);
    f.early.set_selected(1); // "On time"

    let cancel = gtk::Button::with_label("Cancel");
    let add = gtk::Button::with_label("Add");
    add.add_css_class("suggested-action");
    add.set_sensitive(false);
    f.header.pack_start(&cancel);
    f.header.pack_end(&add);
    {
        let add = add.clone();
        f.title.connect_changed(move |e| add.set_sensitive(!e.text().trim().is_empty()));
    }
    {
        let f = f.clone();
        cancel.connect_clicked(move |_| {
            f.dialog.close();
        });
    }
    {
        let f2 = f.clone();
        let submit = Rc::new(move || {
            let draft = f2.draft();
            if draft.title.is_empty() || draft.list.is_empty() {
                return;
            }
            f2.dialog.close();
            on_add(draft);
        });
        let s = submit.clone();
        add.connect_clicked(move |_| s());
        f.title.connect_entry_activated(move |_| submit());
    }
    f.title.set_show_apply_button(false);
    f.dialog.present(Some(parent));
    f.title.grab_focus();
}

pub enum Action {
    Complete(bool),
    Delete,
    Save(Edits),
}

/// Inspector for `r`. Everything is editable except Urgent and Repeat.
pub fn show_reminder(
    parent: &impl IsA<gtk::Widget>,
    r: &Reminder,
    lists: &[List],
    urgent: Option<bool>,
    on_action: impl Fn(Action) + 'static,
) {
    let f = Rc::new(build("Details", lists));
    f.title.set_text(&r.title);
    f.notes.buffer().set_text(&r.notes);
    f.url.set_text(&r.urls.join(" "));
    if let Some(due) = &r.due {
        let local = due.at.with_timezone(&Local);
        let date = due.local_date();
        if let Ok(d) = glib::DateTime::from_local(date.year(), date.month() as i32, date.day() as i32, 12, 0, 0.0) {
            f.calendar.select_day(&d);
        }
        f.date.set_enable_expansion(true);
        if !due.all_day {
            f.time.set_enable_expansion(true);
            f.hour.set_value(local.hour() as f64);
            f.minute.set_value(local.minute() as f64);
        }
        f.date.set_expanded(false);
        f.time.set_expanded(false);
    }
    f.urgent.set_active(urgent.unwrap_or(false));
    f.urgent.set_subtitle(match urgent {
        Some(_) => "",
        None if r.urgent_asset.is_some() => "Loading…",
        None => "",
    });
    if let Some(rule) = &r.recurrence {
        f.repeat.set_subtitle(&format!("Repeats ({} fields; format not decoded yet)", rule.fields.len()));
    } else {
        f.repeat.set_subtitle("");
    }
    // An early reminder that isn't a preset gets its own entry at the end.
    let presets = EarlyAlert::presets();
    let old_early = r.early_alerts.first().copied();
    let custom_early = old_early.filter(|a| !presets.iter().any(|(p, _)| p == a));
    if let (Some(a), Some(model)) = (custom_early, f.early.model().and_downcast::<gtk::StringList>()) {
        model.append(&a.label());
    }
    let early_index = match old_early {
        None => 0,
        Some(a) => presets.iter().position(|(p, _)| *p == a).unwrap_or(presets.len()) + 1,
    };
    f.early.set_selected(early_index as u32);
    if let Some(i) = r.list.as_ref().and_then(|l| lists.iter().position(|x| &x.record_name == l)) {
        f.list.set_selected(i as u32);
    }
    f.tags.set_text(&r.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "));
    f.flag.set_active(r.flagged);
    f.priority.set_selected(PRIORITIES.iter().position(|(p, _)| *p == r.priority).unwrap_or(0) as u32);

    if !r.other_alarms.is_empty() || !r.alarms.is_empty() {
        let extra: Vec<String> = r
            .alarms
            .iter()
            .map(|t| t.with_timezone(&Local).format("%a %-d %b, %H:%M").to_string())
            .chain(r.other_alarms.iter().cloned())
            .collect();
        f.early.set_subtitle(&format!("Also alerts: {}", extra.join(", ")));
    }

    let on_action = Rc::new(on_action);
    let close = gtk::Button::from_icon_name("window-close-symbolic");
    close.set_tooltip_text(Some("Close"));
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    save.set_sensitive(false);
    f.header.pack_start(&close);
    f.header.pack_end(&save);

    let done = gtk::Button::with_label(if r.completed { "Mark Not Completed" } else { "Mark Completed" });
    done.add_css_class("pill");
    let delete = gtk::Button::with_label("Delete Reminder");
    delete.add_css_class("destructive-action");
    delete.add_css_class("pill");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::Center);
    buttons.append(&done);
    buttons.append(&delete);
    let bottom = adw::PreferencesGroup::new();
    bottom.add(&buttons);
    f.page.add(&bottom);

    let old_schedule = Schedule {
        due_date: r.due.as_ref().map(|d| d.local_date()),
        due_time: r.due.as_ref().filter(|d| !d.all_day).map(|d| {
            let t = d.at.with_timezone(&Local);
            (t.hour(), t.minute())
        }),
        early_alert: old_early,
    };
    let edits = {
        let (f, r) = (f.clone(), r.clone());
        let tag_set = |tags: &[String]| {
            let mut t: Vec<String> = tags.iter().map(|t| t.trim_start_matches('#').to_string()).filter(|t| !t.is_empty()).collect();
            t.sort();
            t.dedup();
            t
        };
        move || {
            let d = f.draft();
            let mut schedule = d.schedule();
            if f.early.selected() as usize == presets.len() + 1 {
                schedule.early_alert = custom_early;
            }
            Edits {
                title: Some(d.title.clone()).filter(|t| *t != r.title.trim() && !t.is_empty()),
                notes: Some(d.notes.clone()).filter(|n| *n != r.notes.trim()),
                flagged: Some(d.flagged).filter(|&v| v != r.flagged),
                priority: Some(d.priority).filter(|&p| p != r.priority),
                list: Some(d.list.clone()).filter(|l| !l.is_empty() && Some(l) != r.list.as_ref()),
                schedule: Some(schedule).filter(|s| *s != old_schedule),
                url: Some(d.url.clone()).filter(|u| u.as_deref().unwrap_or("").trim() != r.urls.join(" ")),
                tags: Some(d.tags.clone()).filter(|t| tag_set(t) != tag_set(&r.tags)),
            }
        }
    };
    let edits = Rc::new(edits);
    let changed = {
        let (save, edits) = (save.clone(), edits.clone());
        Rc::new(move || save.set_sensitive(!edits().is_empty()))
    };
    {
        let c = changed.clone();
        f.title.connect_changed(move |_| c());
        let c = changed.clone();
        f.notes.buffer().connect_changed(move |_| c());
        for e in [&f.url, &f.tags] {
            let c = changed.clone();
            e.connect_changed(move |_| c());
        }
        for e in [&f.date, &f.time] {
            let c = changed.clone();
            e.connect_enable_expansion_notify(move |_| c());
        }
        let c = changed.clone();
        f.calendar.connect_day_selected(move |_| c());
        for s in [&f.hour, &f.minute] {
            let c = changed.clone();
            s.connect_value_notify(move |_| c());
        }
        for row in [&f.early, &f.list, &f.priority] {
            let c = changed.clone();
            row.connect_selected_notify(move |_| c());
        }
        let c = changed.clone();
        f.flag.connect_active_notify(move |_| c());
    }
    {
        let (f2, on_action, edits) = (f.clone(), on_action.clone(), edits.clone());
        let submit = Rc::new(move || {
            let e = edits();
            if e.is_empty() {
                return;
            }
            f2.dialog.close();
            on_action(Action::Save(e));
        });
        let s = submit.clone();
        save.connect_clicked(move |_| s());
        f.title.connect_entry_activated(move |_| submit());
    }
    f.title.set_show_apply_button(false);
    f.url.set_show_apply_button(false);
    f.tags.set_show_apply_button(false);
    {
        let f = f.clone();
        close.connect_clicked(move |_| {
            f.dialog.close();
        });
    }
    {
        let (f, on_action, completed) = (f.clone(), on_action.clone(), r.completed);
        done.connect_clicked(move |_| {
            f.dialog.close();
            on_action(Action::Complete(!completed));
        });
    }
    {
        let (f, title) = (f.clone(), r.title.clone());
        delete.connect_clicked(move |_| {
            let confirm = adw::AlertDialog::new(Some(&format!("Delete \u{201c}{title}\u{201d}?")), Some("It's deleted on all your devices."));
            confirm.add_response("cancel", "Cancel");
            confirm.add_response("delete", "Delete");
            confirm.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            confirm.set_close_response("cancel");
            let (f2, on_action) = (f.clone(), on_action.clone());
            confirm.connect_response(Some("delete"), move |_, _| {
                f2.dialog.close();
                on_action(Action::Delete);
            });
            confirm.present(Some(&f.dialog));
        });
    }
    f.dialog.present(Some(parent));
}
