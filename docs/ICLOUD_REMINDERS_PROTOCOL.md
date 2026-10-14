# iCloud Reminders — Protocol Research

How Leporello reads and writes the user's own iCloud reminders from Linux.
Goal: list, create, edit, complete and schedule reminders with everything the
macOS inspector offers (notes, URL, date/time, urgent, repeat, early reminder,
list, tags, flag, priority), and raise local alerts when they're due.

Each fact is tagged by where it came from:

- **[RE]** observed in Apple's shipped binaries with REA/Hopper (macOS 26.5, October 2026)
- **[live]** observed against the user's account through CloudKit Web Services (2026-10-13)
- **[public]** public knowledge, not yet confirmed here
- **[unknown]** open question; verify before relying on it

---

## Where the logic lives

| Component | Role |
|---|---|
| `/System/Applications/Reminders.app` | UI only. Entitlements: CloudKit, container `com.apple.reminders`, environment `Production` **[RE]** |
| `ReminderKit.framework` (dyld cache) | Client object model (`REMReminder`, `REMDueDateDeltaInterval`, `REMCRMergeableStringDocument`) and the topotext CRDT **[RE]** |
| `/usr/libexec/remindd` | Daemon: Core Data store + CloudKit sync. Every `REMCD* newlyCreatedRecord` / `mergeDataFromRecord:` lives here **[RE]** |

`remindd` is an ordinary on-disk Mach-O, so it can be opened directly.
ReminderKit was pulled out of the shared cache with
`ipsw dyld extract …/dyld_shared_cache_arm64e ReminderKit`.

### Reading Swift record keys out of `remindd`

The record builders are Swift. Hopper's pseudocode stops at a Swift jump
table, and short keys (≤ 15 UTF-8 bytes, e.g. `DueDate`) never appear as C
strings: Swift stores them as *small strings*, built in registers with
`mov`/`movk` immediates. The keys were recovered by disassembling each
builder (`otool -arch arm64e -tv`) and decoding the immediates as
little-endian ASCII. Longer keys (`TitleDocumentAsset`,
`DueDateDeltaAlertsData`, …) are ordinary C strings next to
`remindd/REMCDList+CloudKit.swift`.

| Builder | Address (remindd, arm64e) |
|---|---|
| `-[REMCDReminder newlyCreatedRecord]` → Swift body | `0x10038bcd0` |
| `-[REMCDList newlyCreatedRecord]` | `0x10034d5cc` |
| `-[REMCDAlarm newlyCreatedRecord]` | `0x10019adbc` |
| `-[REMCDAlarmTrigger newlyCreatedRecord]` | `0x10078a248` |
| `-[REMCDRecurrenceRule newlyCreatedRecord]` | `0x1004439ec` |
| `-[REMCDHashtag newlyCreatedRecord]` | `0x10021f040` |
| `-[REMCDAttachment newlyCreatedRecord]` | `0x1003b7500` |

---

## Transport: CloudKit Web Services **[live]**

Same approach and same sign-in as amadeuz's iCloud Notes: `idmsa.apple.com`
SRP-6a + 2FA → `setup.icloud.com` → the account's `ckdatabasews` URL.
amadeuz's saved web session works unchanged against this container.

- `GET  {ckdatabasews}/database/1/com.apple.reminders/production/private/zones/list`
- `POST …/changes/zone` (paged with `syncToken` / `moreComing`)
- `POST …/records/modify`

Zones in the private database: `Reminders` (all data), `RemindersMigration`,
`_defaultZone`. Lists shared *with* the user live in the **shared** database
(`…/production/shared/…`), not here; the `Account` record maps them in
`SharedListIDsToOwnerNames`. **[live]** Shared lists are out of scope for v1.

"Encrypted" fields (`ENCRYPTED_BYTES`) come back as plain base64 for the
signed-in user, as with Notes. Advanced Data Protection blocks web access
unless "Access iCloud Data on the Web" is on **[public]**.

Live account at first probe: 692 `Reminder`, 641 `Alarm`, 639 `AlarmTrigger`,
93 `Attachment`, 18 `ListSection`, 3 `List`, 1 `Hashtag`, 1 `Account`, plus
438 tombstones.

### Record naming **[live]**

`recordName` is `<Type>/<UUID>`: `Reminder/EA034C64-…`, `List/FE60416A-…`,
`Alarm/…`, `AlarmTrigger/…`, `Attachment/…`. ID lists on a reminder
(`AlarmIDs`, `AttachmentIDs`, `HashtagIDs`, `RecurrenceRuleIDs`) hold the
**bare UUIDs**; references (`List`, `Reminder`, `Alarm`) hold full record
names with `action: "VALIDATE"`.

### Deletion

Two levels: CloudKit tombstones (`deleted: true`, no fields) and a soft
`Deleted` = 1 field (Core Data's `markedForDeletion`). Hide both.

---

## `Reminder` **[RE]** keys, **[live]** types

| Field | CloudKit type | Meaning |
|---|---|---|
| `TitleDocument` / `TitleDocumentAsset` | ENCRYPTED_BYTES / ASSETID | Title, topotext CRDT (below) |
| `NotesDocument` / `NotesDocumentAsset` | ENCRYPTED_BYTES / ASSETID | Notes, same format; absent when empty |
| `List` | REFERENCE | Owning list |
| `Completed` | INT64 | 0/1 |
| `CompletionDate` | TIMESTAMP | ms since 1970 |
| `DueDate` | TIMESTAMP | ms since 1970 (UTC instant) |
| `AllDay` | INT64 | 1 = date without time |
| `TimeZone` | STRING | IANA, e.g. `Europe/London`; absent = floating |
| `StartDate` | TIMESTAMP | |
| `Flagged` | INT64 | |
| `Priority` | INT64 | 0 none; EventKit convention 1 high, 5 medium, 9 low **[public]** |
| `AlarmIDs` | STRING_LIST | `Alarm` UUIDs |
| `AttachmentIDs` | STRING_LIST | `Attachment` UUIDs (URLs, images) |
| `HashtagIDs` | STRING_LIST | `Hashtag` UUIDs (tags) |
| `RecurrenceRuleIDs` | STRING_LIST | `RecurrenceRule` UUIDs |
| `AssignmentIDs` | STRING_LIST | Shared-list assignments |
| `ParentReminder` | REFERENCE | Subtasks |
| `DueDateDeltaAlertsData` | ENCRYPTED_BYTES | Early reminder, JSON (below) |
| `UrgentPresentationAlarmsAsData` + `…Checksum` | ASSETID + STRING | Urgent toggle, JSON asset (below) |
| `CreationDate`, `LastModifiedDate` | TIMESTAMP | |
| `ResolutionTokenMap` | STRING | Per-field merge clocks, JSON (below) |
| `Deleted`, `Imported` | INT64 | |
| also seen in RE | | `ContactHandles`, `BatchCreationID`, `ICSData`, `IcsUrl`, `UserActivity`, `AlternativeDisplayDateDateForCalendar`, `MinimumSupportedAppVersion` |

There is **no URL field**: the URL in the inspector is an `Attachment`.

### Title and notes documents **[RE] [live]**

`REMCRMergeableStringDocument` wraps a `TTMergeableStringVersionedDocument`,
the same topotext CRDT as iCloud Notes (ReminderKit links
`CRProtobuf/protobuf-lite/topotext.pb.cc`). Bytes:

```
gzip( Document { 1: 0, 2: Version { 1: 0, 2: 0, 3: topotext.String {
        2: "visible text",
        3: Substring…   (start sentinel, text, end sentinel)
        4: VectorTimestamp { 1: { 1: replica UUID, 2: {1: textClock}, 2: {1: styleClock} } }
        5: AttributeRun { 1: length, 5: 0 } } } } )
```

A freshly created title written by `remindd` is byte-for-byte what
`remindkit::document::new` produces (unit test
`new_document_matches_apple_layout`). Edits must be CRDT operations from our
own replica (`remindkit::crdt`, ported from amadeuz), never a rebuilt document,
or Apple devices merge duplicated text.

### `ResolutionTokenMap` **[live]**

```json
{"map":{"titleDocument":{"replicaID":"3E1F…","counter":1,"modificationTime":813491505.859},
        "dueDate":{…}, "completed":{…}, "list":{…}, …}}
```

Per-field last-writer-wins clock. Keys are the Core Data property names
(lower camel case). `modificationTime` is seconds since 2001-01-01 (Apple
reference date). Records Apple imported carry `counter: 0,
modificationTime: 0`. **When changing a field, bump its entry** (our replica
ID, `counter + 1`, now); otherwise a device holding an older value may win
the merge **[RE: "Key in CKRecord's resolution token map is unrecognized"
and `mergeData(from:)` consult it; exact merge rule unknown]**.

### Early reminder: `DueDateDeltaAlertsData` **[RE] [live]**

```json
{"dueDateDeltaAlerts":[{"identifier":"C47C4F4B-…","dueDateDeltaCount":-5,"dueDateDeltaUnit":0,
                        "creationDate":717528112.355,"minimumSupportedAppVersion":0}],
 "reminderIdentifier":"<reminder UUID>","accountIdentifier":"<account UUID>",
 "minimumSupportedVersion":20230430}
```

Unit, from `-[REMDueDateDeltaInterval addedTo:]`: **0 minute, 1 hour, 2 day,
3 week (count × 7 days), 4 month**. The count is added to the due date, so
early alerts are negative (`-5, 0` = 5 minutes before). `0, 0` = at the due
time (59 of the 60 records in the live account). `creationDate` is
reference-date seconds. Where `accountIdentifier` comes from: **[unknown]**
(not the CloudKit owner record name; probably the local `REMCDAccount` UUID —
copy it from any existing reminder).

### Urgent: `UrgentPresentationAlarmsAsData` **[live]**

An asset (downloaded from `downloadURL` with `${f}` replaced), JSON:

```json
{"account":[{"personID":"YQb2…=","isEnabled":false,"modifiedOn":813493856.44}],
 "minimumSupportedVersion":20251103}
```

`isEnabled` is the "Urgent" toggle, per person (`personID` is salted with
`Account.PersonIDSalt`). `…Checksum` is a 512-bit hash of the asset —
algorithm **[unknown]** (SHA-512 of the bytes is the first guess). Writing
urgent requires uploading an asset (`assets/upload`) — v2.

---

## `Alarm` and `AlarmTrigger` **[RE] [live]**

```
Reminder.AlarmIDs ──► Alarm/<id>  { Reminder: ref, TriggerID: "<uuid>", AlarmUID,
                                    AcknowledgedDate, DueDateResolutionTokenAsNonce, Deleted, Imported }
                      AlarmTrigger/<TriggerID> { Alarm: ref, Type: "Date",
                                    DateComponentsData: base64 JSON, Deleted, Imported }
```

`DateComponentsData` (BYTES) is JSON-encoded `DateComponents`:

```json
{"era":1,"year":2026,"month":10,"day":10,"hour":10,"minute":0,"second":0,"nanosecond":0,
 "timeZone":{"identifier":"Europe/London"}}
```

Other trigger types (RE): `Location` (`Latitude`, `Longitude`, `Radius`,
`Proximity`, `Address`, `Title`, `LocationUID`, `ReferenceFrameString`),
`TimeInterval`, `Vehicle`, `Event`. Leporello only creates `Date` triggers
and shows the others read-only.

`AcknowledgedDate` is set when the alert is dismissed/completed on a device.
`DueDateResolutionTokenAsNonce` ties the alarm to a due-date version **[RE
name; semantics unknown]**.

## `Attachment` (URL) **[live]**

```
Attachment/<id> { Reminder: ref, Type: "URL", URL: "https://odysseusai.dev/",
                  UTI: "public.url", MetadataAsset | Metadata (link preview), Deleted, Imported }
```

Image attachments: `Type` image, `FileAsset`, `FileName`, `FileSize`,
`Width`, `Height`.

## `Hashtag` (tags) **[live]**

```
Hashtag/<id> { Reminder: ref, Name: "como-atuamos", Type: 0, CreationDate, Deleted, Imported }
```

One record per tag *use*. Tags also appear inline in the title document as
topotext hashtag attachments ("Added TTREMHashtag to this
REMMutableCRMergeableStringDocument") **[RE]** — whether the record alone is
enough for Apple to show the tag: **[unknown]**.

## `RecurrenceRule` **[RE]** — no live sample yet

Keys from the builder: `Reminder`, `Frequency`, `Interval`, `EndDate`,
`OccurrenceCount`, `DaysOfTheWeek`, `DaysOfTheMonth`, `DaysOfTheYear`,
`WeeksOfTheYear`, `MonthsOfTheYear`, `SetPositions`, `FirstDayOfTheWeek`.
Value encodings **[unknown]** — the live account has no repeating reminders.
To learn them: create one repeating reminder on the Mac, then
`cargo run -p remindkit --example probe -- --find RecurrenceRule`.

## `List` **[live]**

`Name`, `Color` (JSON: `{"daHexString":"#FF9500","red":1,"green":0.58,"blue":0,
"alpha":1,"colorRGBSpace":2,"ckSymbolicColorName":"custom",…}`),
`BadgeEmblem`, `IsGroup`, `SortingStyle`, `ReminderIDs` (JSON array string —
manual order), `IsLinkedToAccount`, grocery fields, `ResolutionTokenMap`.

## `Account` **[live]**

`ListIDsMergeableOrdering` (sidebar order), `SharedListIDsToOwnerNames`,
`PersonIDSalt`, migration flags.

---

## Writing a new reminder — plan

One atomic `records/modify` in zone `Reminders`:

1. `Reminder/<new UUID>`: `TitleDocument` (new document, our replica),
   `NotesDocument` if any, `List`, `Completed` 0, `Priority`, `Flagged`,
   `AllDay`, `DueDate` + `TimeZone`, `CreationDate`, `LastModifiedDate`,
   `AlarmIDs`/`AttachmentIDs`/`HashtagIDs`/`RecurrenceRuleIDs`,
   `DueDateDeltaAlertsData`, `ResolutionTokenMap` for every field set,
   `Deleted` 0, `Imported` 0.
2. One `Alarm` + `AlarmTrigger` (`Type: Date`) at the due time, or at the due
   time plus the early-reminder delta.
3. `Attachment` (`Type: URL`) if a URL was given.
4. `Hashtag` per tag.

Record shape rules **[live]**:
- Fields are sent as `{"value": ...}` with **no `type`**, as icloud.com's
  Reminders app does (its models only set `fields[k].value`; CloudKit JS
  contains no type names). iCloud applies the zone schema. Sending typed
  fields (`INT64`, `STRING_LIST`, even `EMPTY_LIST` for empty lists) made
  the whole batch fail with `BAD_REQUEST: BadRequestException: Unexpected
  input` (two attempts on 2026-10-14; nothing was saved). The exception
  is encrypted strings/doubles, which the web app marks
  `{"type": "STRING", "isEncrypted": true}` (only AlarmTrigger location
  fields, `Attachment.URL`, `Hashtag.Name`, `List.Name`). Untyped, these
  fail with `BAD_REQUEST: byte values must be base64 encoded`. A record's
  `isEncrypted: true` on read marks them. Source:
  `/applications/reminders2/<build>/en-us/main.js` on www.icloud.com.
- Every child record has a CloudKit `parent` (`{"recordName": ...}`):
  Reminder → List, Alarm/Attachment → Reminder, AlarmTrigger → Alarm.
  Hashtags have none.
- There's no null value in the web API. Uncompleted reminders simply have
  no `CompletionDate`, so reopening leaves the old one in place.

Open questions for the first live write **[unknown]**:
- Whether the new reminder must also be appended to `List.ReminderIDs`
  to appear (likely not: that's manual ordering).
- Whether `DueDateDeltaAlertsData` alone produces the early alert on Apple
  devices, or a second `Alarm` is also expected.
- `MinimumSupportedAppVersion` value for new records.

Verify by creating one reminder from Leporello and one identical reminder
on the Mac, and diffing the records with the probe.
