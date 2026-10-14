# Leporello — Project Brief for Claude

## Vision

The user's iCloud Reminders on Linux, in a native GNOME app shaped like
Reminders.app. Scope: see reminders (Today, Scheduled, All, Flagged,
Urgent, Completed, per list), create reminders with everything the macOS
inspector offers (notes, URL, date/time, early reminder, list, tags, flag,
priority), complete/delete them, and get desktop alerts when they come due.
Not in scope: grocery lists, sections, templates and the other specialised
checklist features.

Sibling of amadeuz (iCloud Notes), figaro (Apple Music) and scrawl
(Preview). Same stack and approach: study Apple's binaries, then call the
web-services API directly. Sign-in and the topotext CRDT come from amadeuz.

Read `docs/ICLOUD_REMINDERS_PROTOCOL.md` before changing anything that
touches records.

## Stack

- Rust 2021, `gtk4` 0.10 (`gnome_46`), `libadwaita` 0.8 (`v1_5`); UI built in
  Rust, no Blueprint/Meson. `cargo run` works on macOS with Homebrew GTK too
- tokio for network; GTK awaits tokio `JoinHandle`s through `crate::bg`, so
  UI code stays on the main thread
- `keyring` for the iCloud web session

## Layout

```
remindkit/    protocol crate, no GTK
  auth.rs, srp.rs  iCloud web sign-in (SRP-6a + 2FA), copied from amadeuz
  cloudkit.rs      CloudKit Web Services: zones, changes/zone, records/modify
  document.rs      TitleDocument/NotesDocument: read + create (topotext)
  crdt.rs          editing an existing document as our own replica (from amadeuz)
  model.rs         Store (all zone records + sync token) → Reminder/List;
                   Writer builds create/update/complete/delete operations
  examples/probe.rs    live account: schema survey, --dump N, --find, --field, --export-store
  examples/fixture.rs  fake store for UI work
src/
  main.rs       app, tokio runtime, accels, --background, LEPORELLO_SNAPSHOT
  window.rs     window + controller (session, sync, writes, rendering)
  editor.rs     inspector dialog: new reminder, details (both editable)
  notifier.rs   desktop alerts from synced alarms
  config.rs     ~/.cache/leporello/{store,state}.json
  secrets.rs    keyring
docs/         protocol research
data/         .desktop + autostart entry
```

## Key decisions

- **CloudKit Web Services** (`com.apple.reminders`, private DB, zone
  `Reminders`) with the icloud.com web session, as in amadeuz. Not the
  native `cloudd` transport (device-bound auth).
- **Whole-zone cache + incremental sync.** `Store` keeps every record and
  the `syncToken`; `changes/zone` every 60 s. Reminders are derived on read.
- **Titles/notes are CRDTs.** New ones are built from scratch (byte-identical
  to what `remindd` writes); edits must go through `crdt.rs`, never rebuild.
- **Every field write bumps `ResolutionTokenMap`** (per-field LWW clock).
- **Alerts are local.** The app stays alive after the window closes (the
  window hides; Ctrl+Q quits) and autostarts with `--background`.
- **Existing reminders are fully editable** (`Writer::edit`) except Repeat
  and Urgent. Changed child records (due-date alarm, URL, tags) are
  soft-deleted and recreated, as icloud.com does.
- Repeat and Urgent can't be *set* yet: RecurrenceRule encodings are
  unverified, and Urgent needs an asset upload. Both are *shown*.
- Shared lists (CloudKit shared database) are not loaded yet.

## Status (2026-10-14, Linux)

Running signed in on Linux and installed with `just install`. Verified live
by the user: reading, create, complete, delete, and editing every field
(title, notes, date/time, early reminder, list, URL, tags, flag, priority),
with the changes showing up on the Mac.

Still open: Repeat and Urgent writes, shared lists, and the protocol doc's
open questions (e.g. `List.ReminderIDs` ordering).

## Commands

`cargo run`, `cargo test`, `just probe [--dump 3 | --find <uuid> | --field <Key> [n]]`,
`just fixture`, `just install`.
Render the window to a PNG without screen capture:
`LEPORELLO_SNAPSHOT=/tmp/out.png [LEPORELLO_SNAPSHOT_SCRIPT=win.new-reminder] cargo run`.

On macOS, each rebuilt probe binary triggers a Keychain "allow access"
prompt; the probe blocks until it's answered.
