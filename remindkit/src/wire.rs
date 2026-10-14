//! Minimal protobuf wire-format reader/writer that keeps every field, known or
//! not, so a message can be edited and re-encoded without losing data that
//! amadeuz doesn't model.

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Varint(u64),
    Fixed64(u64),
    Bytes(Vec<u8>),
    Fixed32(u32),
}

/// A message as an ordered list of (field number, value).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Message(pub Vec<(u32, Value)>);

impl Message {
    pub fn parse(mut b: &[u8]) -> Result<Self> {
        let mut out = Vec::new();
        while !b.is_empty() {
            let key = varint(&mut b)?;
            let num = u32::try_from(key >> 3).map_err(|_| bad("field number"))?;
            let val = match key & 7 {
                0 => Value::Varint(varint(&mut b)?),
                1 => Value::Fixed64(u64::from_le_bytes(take(&mut b, 8)?.try_into().unwrap())),
                2 => {
                    let len = usize::try_from(varint(&mut b)?).map_err(|_| bad("length"))?;
                    Value::Bytes(take(&mut b, len)?.to_vec())
                }
                5 => Value::Fixed32(u32::from_le_bytes(take(&mut b, 4)?.try_into().unwrap())),
                _ => return Err(bad("wire type")),
            };
            if num == 0 {
                return Err(bad("field 0"));
            }
            out.push((num, val));
        }
        Ok(Message(out))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (num, val) in &self.0 {
            let wt = match val {
                Value::Varint(_) => 0,
                Value::Fixed64(_) => 1,
                Value::Bytes(_) => 2,
                Value::Fixed32(_) => 5,
            };
            put_varint(&mut out, (u64::from(*num) << 3) | wt);
            match val {
                Value::Varint(v) => put_varint(&mut out, *v),
                Value::Fixed64(v) => out.extend_from_slice(&v.to_le_bytes()),
                Value::Bytes(b) => {
                    put_varint(&mut out, b.len() as u64);
                    out.extend_from_slice(b);
                }
                Value::Fixed32(v) => out.extend_from_slice(&v.to_le_bytes()),
            }
        }
        out
    }

    pub fn varint(&self, num: u32) -> Option<u64> {
        self.0.iter().find_map(|(n, v)| match v {
            Value::Varint(x) if *n == num => Some(*x),
            _ => None,
        })
    }

    pub fn bytes(&self, num: u32) -> Option<&[u8]> {
        self.0.iter().find_map(|(n, v)| match v {
            Value::Bytes(b) if *n == num => Some(b.as_slice()),
            _ => None,
        })
    }

    pub fn all_bytes(&self, num: u32) -> impl Iterator<Item = &[u8]> {
        self.0.iter().filter_map(move |(n, v)| match v {
            Value::Bytes(b) if *n == num => Some(b.as_slice()),
            _ => None,
        })
    }

    pub fn all_varints(&self, num: u32) -> impl Iterator<Item = u64> + '_ {
        self.0.iter().filter_map(move |(n, v)| match v {
            Value::Varint(x) if *n == num => Some(*x),
            _ => None,
        })
    }

    /// Replace the first occurrence of `num` (or append it).
    pub fn set(&mut self, num: u32, val: Value) {
        match self.0.iter_mut().find(|(n, _)| *n == num) {
            Some(slot) => slot.1 = val,
            None => self.0.push((num, val)),
        }
    }

    /// Replace every occurrence of `num` with `vals`, at the position of the first one
    /// (or at the end if absent).
    pub fn replace_all(&mut self, num: u32, vals: Vec<Value>) {
        let pos = self.0.iter().position(|(n, _)| *n == num).unwrap_or(self.0.len());
        self.0.retain(|(n, _)| *n != num);
        let pos = pos.min(self.0.len());
        for (i, v) in vals.into_iter().enumerate() {
            self.0.insert(pos + i, (num, v));
        }
    }

    pub fn remove(&mut self, num: u32) {
        self.0.retain(|(n, _)| *n != num);
    }

    pub fn field_numbers(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.iter().map(|(n, _)| *n)
    }
}

fn bad(what: &str) -> Error {
    Error::Protocol(format!("malformed protobuf ({what})"))
}

fn varint(b: &mut &[u8]) -> Result<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let (&byte, rest) = b.split_first().ok_or_else(|| bad("truncated varint"))?;
        *b = rest;
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(bad("varint too long"))
}

fn take<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    let (x, rest) = b.split_at_checked(n).ok_or_else(|| bad("truncated field"))?;
    *b = rest;
    Ok(x)
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_unknown_fields() {
        let m = Message(vec![
            (1, Value::Varint(300)),
            (7, Value::Bytes(b"x".to_vec())),
            (9, Value::Fixed32(5)),
            (9, Value::Fixed64(6)),
        ]);
        assert_eq!(Message::parse(&m.encode()).unwrap(), m);
    }
}
