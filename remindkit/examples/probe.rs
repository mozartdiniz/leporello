//! Check iCloud Reminders access from the command line.
//!
//!     cargo run -p remindkit --example probe              # zones + schema (field names/types, no values)
//!     cargo run -p remindkit --example probe -- --dump N  # full fields of the N most recent reminders
//!     cargo run -p remindkit --example probe -- --forget  # delete the saved session
//!
//! The session is kept in the OS keyring under "io.github.mozartdiniz.Leporello.probe".
//! On first run it tries amadeuz's probe session (same Apple sign-in) before
//! asking for a password.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::Value;

use remindkit::wire::{Message, Value as W};
use remindkit::{Error, ICloudClient, SignInOutcome};

const KEYRING_SERVICE: &str = "io.github.mozartdiniz.Leporello.probe";
const AMADEUZ_SERVICE: &str = "com.amadeuz.Notes.icloud-probe";
const KEYRING_USER: &str = "session";

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).expect("keyring");
    if args.iter().any(|a| a == "--forget") {
        entry.delete_credential().ok();
        println!("Saved session deleted.");
        return;
    }

    let saved = entry.get_password().ok().or_else(|| {
        let s = keyring::Entry::new(AMADEUZ_SERVICE, KEYRING_USER).ok()?.get_password().ok()?;
        println!("Trying amadeuz's saved session.");
        Some(s)
    });
    let client = match saved.and_then(|s| ICloudClient::restore(&s).ok()) {
        Some(mut c) => match c.validate().await {
            Ok(()) => {
                println!("Reusing saved session for {}", c.apple_id().unwrap_or("?"));
                c
            }
            Err(e) => {
                println!("Saved session not usable ({e}); signing in again.");
                sign_in(Some(c)).await
            }
        },
        None => sign_in(None).await,
    };
    entry.set_password(&client.save()).expect("save session to keyring");

    let zones = client.list_zones().await.unwrap_or_else(|e| fail(e));
    println!("\nZones in com.apple.reminders:");
    for z in &zones {
        println!("  {}", z["zoneID"]);
    }
    let Some(zone) = zones.iter().find(|z| z["zoneID"]["zoneName"] == "Reminders") else {
        fail(Error::Protocol("no Reminders zone".into()));
    };
    let changes = client.zone_changes(&zone["zoneID"], None).await.unwrap_or_else(|e| fail(e));
    let records = changes.records;
    println!("{} records, sync token {}", records.len(), changes.sync_token.is_some());

    if let Some(i) = args.iter().position(|a| a == "--export-store") {
        // Seed the app's cache (dev aid: see the window with real data).
        let path = args.get(i + 1).expect("--export-store <path>");
        let mut store = remindkit::Store::new(zone["zoneID"].clone());
        store.apply(records, changes.sync_token);
        std::fs::write(path, serde_json::to_vec(&store).expect("serialize")).expect("write store");
        println!("{} reminders written to {path}", store.reminders().len());
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--field") {
        // Full value of one field across the first few records that have it.
        let key = args.get(i + 1).cloned().unwrap_or_default();
        let limit: usize = args.get(i + 2).and_then(|n| n.parse().ok()).unwrap_or(3);
        for r in records.iter().filter(|r| !r["fields"][&key].is_null()).take(limit) {
            let v = &r["fields"][&key]["value"];
            println!("\n{} {key}:", r["recordName"]);
            match v.as_str().and_then(|b| B64.decode(b).ok()) {
                Some(b) if r["fields"][&key]["type"] != "STRING" => {
                    println!("{}", String::from_utf8_lossy(&b));
                    dump_bytes(&b, 1);
                }
                _ => println!("{v}"),
            }
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--find") {
        let needle = args.get(i + 1).cloned().unwrap_or_default();
        for r in records.iter().filter(|r| r.to_string().contains(&needle)) {
            dump_record(r);
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--dump") {
        let n: usize = args.get(i + 1).and_then(|n| n.parse().ok()).unwrap_or(3);
        let mut reminders: Vec<&Value> = records.iter().filter(|r| r["recordType"] == "Reminder").collect();
        reminders.sort_by_key(|r| std::cmp::Reverse(r["modified"]["timestamp"].as_i64().unwrap_or(0)));
        for r in reminders.into_iter().take(n) {
            dump_record(r);
            for key in ["UrgentPresentationAlarmsAsData", "MetadataAsset"] {
                if let Some(url) = r["fields"][key]["value"]["downloadURL"].as_str() {
                    let url = url.replace("${f}", "data");
                    match client.download_asset(&url).await {
                        Ok(b) => {
                            println!("  {key} asset ({} bytes): {}", b.len(), truncate(&String::from_utf8_lossy(&b), 400));
                            dump_bytes(&b, 2);
                        }
                        Err(e) => println!("  {key} asset: {e}"),
                    }
                }
            }
            // Related records (alarms, attachments, recurrence) by reference.
            for (key, ty) in [("AlarmIDs", "Alarm"), ("AttachmentIDs", "Attachment"), ("RecurrenceRuleIDs", "RecurrenceRule"), ("HashtagIDs", "Hashtag")] {
                for name in referenced(&r["fields"][key]) {
                    let full = format!("{ty}/{name}");
                    if let Some(rel) = records.iter().find(|x| x["recordName"] == name || x["recordName"] == full.as_str()) {
                        dump_record(rel);
                        if rel["recordType"] == "Alarm" {
                            for t in referenced(&rel["fields"]["TriggerID"]) {
                                let full = format!("AlarmTrigger/{t}");
                                if let Some(tr) = records.iter().find(|x| x["recordName"] == full.as_str()) {
                                    dump_record(tr);
                                }
                            }
                        }
                    }
                }
            }
        }
        return;
    }

    // Schema survey: record type → field → (CloudKit type, count). No values.
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut schema: BTreeMap<String, BTreeMap<String, (String, usize)>> = BTreeMap::new();
    for r in &records {
        let ty = r["recordType"].as_str().unwrap_or(if r["deleted"] == true { "(deleted)" } else { "?" });
        *counts.entry(ty.to_string()).or_default() += 1;
        if let Some(fields) = r["fields"].as_object() {
            let s = schema.entry(ty.to_string()).or_default();
            for (k, v) in fields {
                let e = s.entry(k.clone()).or_insert((v["type"].as_str().unwrap_or("?").to_string(), 0));
                e.1 += 1;
            }
        }
    }
    println!("\nRecord types:");
    for (ty, n) in &counts {
        println!("  {ty}: {n}");
    }
    for (ty, fields) in &schema {
        println!("\n{ty}");
        for (k, (t, n)) in fields {
            println!("  {k:<40} {t:<16} {n}");
        }
    }
}

fn referenced(v: &Value) -> Vec<String> {
    let one = |x: &Value| x["recordName"].as_str().map(str::to_string);
    match &v["value"] {
        Value::Array(a) => a.iter().filter_map(one).collect(),
        Value::String(s) => vec![s.clone()],
        o @ Value::Object(_) => one(o).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn dump_record(r: &Value) {
    println!("\n── {} {}", r["recordType"].as_str().unwrap_or("?"), r["recordName"].as_str().unwrap_or("?"));
    let Some(fields) = r["fields"].as_object() else { return };
    for (k, v) in fields {
        let ty = v["type"].as_str().unwrap_or("?");
        println!("  {k} ({ty}): {}", truncate(&v["value"].to_string(), 200));
        if ty == "BYTES" || ty == "ENCRYPTED_BYTES" {
            if let Some(bytes) = v["value"].as_str().and_then(|b| B64.decode(b).ok()) {
                dump_bytes(&bytes, 2);
            }
        }
    }
}

/// Generic protobuf view of a byte field (gunzipped when needed).
fn dump_bytes(bytes: &[u8], depth: usize) {
    let bytes = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        if flate2::read::GzDecoder::new(bytes).read_to_end(&mut out).is_err() {
            return;
        }
        println!("{}(gzip, {} bytes)", " ".repeat(depth * 2), out.len());
        out
    } else {
        bytes.to_vec()
    };
    let Ok(msg) = Message::parse(&bytes) else {
        println!("{}raw {:?}", " ".repeat(depth * 2), truncate(&String::from_utf8_lossy(&bytes), 120));
        return;
    };
    if depth > 8 {
        return;
    }
    for (num, val) in &msg.0 {
        let pad = " ".repeat(depth * 2);
        match val {
            W::Varint(v) => println!("{pad}{num}: {v}"),
            W::Bytes(b) => {
                let printable = std::str::from_utf8(b).ok().filter(|s| s.chars().all(|c| !c.is_control() || c == '\n'));
                match printable {
                    Some(s) if !s.is_empty() => println!("{pad}{num}: {:?}", truncate(s, 120)),
                    _ => {
                        println!("{pad}{num}: <{} bytes>", b.len());
                        if Message::parse(b).is_ok() {
                            dump_bytes(b, depth + 1);
                        }
                    }
                }
            }
            other => println!("{pad}{num}: {other:?}"),
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

async fn sign_in(previous: Option<ICloudClient>) -> ICloudClient {
    // Keep the old client so Apple's trust token can skip 2FA.
    let mut client = previous.unwrap_or_default();
    let apple_id = match client.apple_id() {
        Some(id) => id.to_string(),
        None => prompt("Apple ID: "),
    };
    let password = rpassword::prompt_password("Password: ").expect("read password");

    match client.sign_in(&apple_id, &password).await {
        Ok(SignInOutcome::SignedIn) => {}
        Ok(SignInOutcome::NeedsCode { phones }) => loop {
            for (i, p) in phones.iter().enumerate() {
                println!("  s{}: send SMS to {}", i + 1, p.number);
            }
            let code = prompt("Code shown on your Apple device ('r' to resend, 's<N>' for SMS): ");
            if code == "r" {
                match client.resend_code().await {
                    Ok(()) => println!("New code requested."),
                    Err(e) => println!("Resend failed: {e}"),
                }
                continue;
            }
            if let Some(n) = code.strip_prefix('s').and_then(|n| n.parse::<usize>().ok()) {
                match phones.get(n.wrapping_sub(1)) {
                    Some(p) => match client.request_sms(p.id).await {
                        Ok(()) => println!("SMS sent to {}.", p.number),
                        Err(e) => println!("SMS failed: {e}"),
                    },
                    None => println!("No phone s{n}."),
                }
                continue;
            }
            match client.submit_code(&code).await {
                Ok(()) => break,
                Err(Error::BadCode) => println!("Wrong code, try again."),
                Err(e) => fail(e),
            }
        },
        Err(e) => fail(e),
    }
    println!("Signed in.");
    client
}

fn prompt(label: &str) -> String {
    print!("{label}");
    io::stdout().flush().ok();
    let mut s = String::new();
    io::stdin().read_line(&mut s).expect("read stdin");
    s.trim().to_string()
}

fn fail(e: Error) -> ! {
    eprintln!("error: {e}");
    std::process::exit(1);
}
