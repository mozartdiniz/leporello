# Leporello

Your iCloud Reminders on Linux, in a native GNOME app.

- Today, Scheduled, All, Flagged, Urgent and Completed, plus your lists
- New reminders with notes, URL, date and time, early reminder, list,
  tags, flag and priority
- Complete and delete reminders; changes sync with your iPhone and Mac
- Desktop notifications when a reminder is due, even with the window closed

Leporello talks to iCloud the way icloud.com does (CloudKit Web Services).
Your password goes only to Apple, and the session is stored in your system
keyring. With Advanced Data Protection, enable "Access iCloud Data on the
Web" first.

Not yet: repeating reminders and the Urgent toggle (both are shown, but
can't be set), editing existing reminders, and lists shared with you.

## Build

```sh
cargo run          # needs GTK 4 and libadwaita ≥ 1.5
just install       # ~/.local, plus autostart for alerts
```

See `docs/ICLOUD_REMINDERS_PROTOCOL.md` for how Apple stores reminders.
