//! Title and notes documents (`TitleDocument`, `NotesDocument`).
//!
//! Both are `TTMergeableStringVersionedDocument`s: gzip of a protobuf
//! `Document { 1: 0, 2: Version { 1: 0, 2: 0, 3: topotext.String } }`, the
//! same layout as an iCloud Notes body without the Notes wrapper. Observed
//! live: Reminders writes plain attribute runs (`{ 1: length, 5: 0 }`), no
//! paragraph styles. Editing an existing document goes through [`crate::crdt`].

use std::io::{Read, Write};

use prost::Message;
use uuid::Uuid;

use crate::{Error, Result};

/// Clock value Apple uses for the end sentinel.
const END_CLOCK: u32 = u32::MAX;

#[derive(Clone, PartialEq, Message)]
struct Document {
    #[prost(uint32, optional, tag = "1")]
    serialization_version: Option<u32>,
    #[prost(message, repeated, tag = "2")]
    versions: Vec<Version>,
}

#[derive(Clone, PartialEq, Message)]
struct Version {
    #[prost(uint32, optional, tag = "1")]
    serialization_version: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    minimum_supported_version: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "3")]
    data: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct TopoString {
    #[prost(string, optional, tag = "2")]
    string: Option<String>,
    #[prost(message, repeated, tag = "3")]
    substrings: Vec<Substring>,
    #[prost(message, optional, tag = "4")]
    timestamp: Option<VectorTimestamp>,
    #[prost(message, repeated, tag = "5")]
    attribute_runs: Vec<AttributeRun>,
}

#[derive(Clone, PartialEq, Message)]
struct Substring {
    #[prost(message, optional, tag = "1")]
    char_id: Option<CharId>,
    #[prost(uint32, optional, tag = "2")]
    length: Option<u32>,
    #[prost(message, optional, tag = "3")]
    timestamp: Option<CharId>,
    #[prost(bool, optional, tag = "4")]
    tombstone: Option<bool>,
    #[prost(uint32, repeated, packed = "false", tag = "5")]
    children: Vec<u32>,
}

#[derive(Clone, PartialEq, Message)]
struct CharId {
    #[prost(uint32, optional, tag = "1")]
    replica: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    clock: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
struct VectorTimestamp {
    #[prost(message, repeated, tag = "1")]
    clocks: Vec<Clock>,
}

#[derive(Clone, PartialEq, Message)]
struct Clock {
    #[prost(bytes = "vec", optional, tag = "1")]
    replica_uuid: Option<Vec<u8>>,
    /// [text clock, style clock]
    #[prost(message, repeated, tag = "2")]
    replica_clocks: Vec<ReplicaClock>,
}

#[derive(Clone, PartialEq, Message)]
struct ReplicaClock {
    #[prost(uint32, optional, tag = "1")]
    clock: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
struct AttributeRun {
    #[prost(uint32, optional, tag = "1")]
    length: Option<u32>,
    /// Font hints (topotext `AttributeRun.5`); Reminders writes 0 on new text.
    #[prost(uint32, optional, tag = "5")]
    font_hints: Option<u32>,
}

fn char_id(replica: u32, clock: u32) -> Option<CharId> {
    Some(CharId { replica: Some(replica), clock: Some(clock) })
}

/// A brand-new document containing `text`, written by `replica`, gzipped.
/// It has no history, so Apple devices have nothing to merge against.
pub fn new(text: &str, replica: Uuid) -> Vec<u8> {
    // NSString lengths: UTF-16 code units. The text clock advances one per unit.
    let len = text.encode_utf16().count() as u32;

    let mut substrings = vec![Substring {
        char_id: char_id(0, 0),
        length: Some(0),
        timestamp: char_id(0, 0),
        tombstone: None,
        children: vec![1],
    }];
    if len > 0 {
        substrings.push(Substring {
            char_id: char_id(1, 0),
            length: Some(len),
            timestamp: char_id(1, 0),
            tombstone: None,
            children: vec![2],
        });
    }
    substrings.push(Substring {
        char_id: char_id(0, END_CLOCK),
        length: Some(0),
        timestamp: char_id(0, END_CLOCK),
        tombstone: None,
        children: vec![],
    });

    let string = TopoString {
        string: Some(text.to_string()),
        substrings,
        timestamp: Some(VectorTimestamp {
            clocks: vec![Clock {
                replica_uuid: Some(replica.as_bytes().to_vec()),
                replica_clocks: vec![ReplicaClock { clock: Some(len) }, ReplicaClock { clock: Some(1) }],
            }],
        }),
        attribute_runs: if len > 0 { vec![AttributeRun { length: Some(len), font_hints: Some(0) }] } else { vec![] },
    };

    let raw = Document {
        serialization_version: Some(0),
        versions: vec![Version {
            serialization_version: Some(0),
            minimum_supported_version: Some(0),
            data: Some(string.encode_to_vec()),
        }],
    }
    .encode_to_vec();
    gzip(&raw)
}

/// Visible text of a document (the topotext `String.2`).
pub fn text(blob: &[u8]) -> Result<String> {
    let raw = decompress(blob)?;
    let doc = Document::decode(&raw[..]).map_err(|e| Error::Protocol(format!("document: {e}")))?;
    let Some(data) = doc.versions.last().and_then(|v| v.data.as_deref()) else {
        return Ok(String::new());
    };
    let s = TopoString::decode(data).map_err(|e| Error::Protocol(format!("document text: {e}")))?;
    Ok(s.string.unwrap_or_default())
}

pub(crate) fn gzip(raw: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(raw).expect("in-memory gzip");
    enc.finish().expect("in-memory gzip")
}

pub(crate) fn decompress(blob: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let res = match blob {
        [0x1f, 0x8b, ..] => flate2::read::GzDecoder::new(blob).read_to_end(&mut out),
        [0x78, ..] => flate2::read::ZlibDecoder::new(blob).read_to_end(&mut out),
        _ => return Ok(blob.to_vec()),
    };
    res.map_err(|e| Error::Protocol(format!("document decompression failed: {e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// A `TitleDocument` written by Reminders on macOS 26 ("Testar").
    const APPLE_TITLE: &str = "H4sIAAAAAAAAE+NgEMrjYBBgkMoQYgtJLS5JLJIS4GIBiQDFwLQGI1iEESjCJgWmNZikxLg4gHL/gYAfqA7OVpLhkuIS0G7yFzrp6KE18/rX+kizO3FCTBxsQMyoxcLBpsEAAEEozw5yAAAA";

    #[test]
    fn reads_apple_title() {
        let blob = base64::engine::general_purpose::STANDARD.decode(APPLE_TITLE).unwrap();
        assert_eq!(text(&blob).unwrap(), "Testar");
    }

    #[test]
    fn new_document_round_trips() {
        let blob = new("Pay the café bill 🧾", Uuid::from_bytes([7; 16]));
        assert_eq!(text(&blob).unwrap(), "Pay the café bill 🧾");
        assert_eq!(text(&new("", Uuid::nil())).unwrap(), "");
    }

    /// Same structure Apple writes: compare everything but the replica UUID.
    #[test]
    fn new_document_matches_apple_layout() {
        let apple = decompress(&base64::engine::general_purpose::STANDARD.decode(APPLE_TITLE).unwrap()).unwrap();
        let apple_doc = Document::decode(&apple[..]).unwrap();
        let apple_str = TopoString::decode(apple_doc.versions[0].data.as_deref().unwrap()).unwrap();
        let uuid = Uuid::from_slice(apple_str.timestamp.as_ref().unwrap().clocks[0].replica_uuid.as_deref().unwrap()).unwrap();
        let ours = decompress(&new("Testar", uuid)).unwrap();
        assert_eq!(ours, apple);
    }
}
