//! Large payload values are written once and referenced after that.
//!
//! A model conversation is sent whole with every turn, so logging each
//! request in full makes an attempt's log grow with the square of its turns.
//! Instead, the writer takes the top-level values of a message's payload out
//! of its entry: a value of at least [`MIN_VALUE`] bytes becomes a blob line,
//! written once per content, and an array becomes a run line that lists the
//! blobs of its items and points at the run holding its longest prefix
//! already written. The next turn's conversation is then one new run of its
//! new messages.
//!
//! ```text
//! {"blob":"<sha256 of data>","data":{"role":"user","content":"..."}}
//! {"run":"<prefix key>","prev":1234,"items":[5678,9012]}
//! {"seq":8,...,"event":{..."payload":{"messages":0,...}},"refs":[[61,10240]]}
//! ```
//!
//! An entry's `refs` say where a value was taken out (`[position in event,
//! offset of its blob or run line]`); a `0` holds its place. Readers put the
//! values back, byte for byte, before anything else, and the entry's hash
//! covers the restored bytes, so a changed, moved or missing blob or run
//! fails [`super::verify`] as an edited entry would. An entry without refs
//! is written exactly as before this format existed, and older logs read
//! unchanged.

use std::collections::HashMap;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

use super::MAX_ENTRY_BYTES;

/// Values smaller than this stay in their entry.
pub const MIN_VALUE: usize = 256;
/// Arrays with more items than this stay in their entry, so a run line is
/// never longer than an entry may be.
const MAX_ITEMS: usize = 4096;
/// Blobs and runs the writer remembers; past this it starts afresh, which
/// costs repeated content, never correctness.
const MAX_KNOWN: usize = 1 << 16;

const BLOB: &[u8] = b"{\"blob\":";
const RUN: &[u8] = b"{\"run\":";

type Key = [u8; 32];

/// A blob or run line rather than an entry.
pub fn is_stored(line: &[u8]) -> bool {
    line.starts_with(BLOB) || line.starts_with(RUN)
}

#[derive(Deserialize)]
struct BlobIn<'a> {
    blob: String,
    #[serde(borrow)]
    data: &'a RawValue,
}

#[derive(Deserialize)]
struct RunIn {
    run: String,
    prev: Option<u64>,
    items: Vec<u64>,
}

/// What the writer has already written, by content, and how long the file is.
#[derive(Default)]
pub struct Store {
    pub len: u64,
    blobs: HashMap<Key, u64>,
    runs: HashMap<Key, u64>,
}

fn sha(data: &[u8]) -> Key {
    Sha256::digest(data).into()
}

fn remember(map: &mut HashMap<Key, u64>, key: Key, at: u64) {
    if map.len() >= MAX_KNOWN {
        map.clear();
    }
    map.insert(key, at);
}

fn decode_key(hex_key: &str) -> Option<Key> {
    hex::decode(hex_key).ok()?.try_into().ok()
}

impl Store {
    /// Learn a blob or run line read back at `at`, to reference it again.
    /// The line was already checked with [`check_stored`].
    pub fn learn(&mut self, line: &[u8], at: u64) {
        if line.starts_with(BLOB) {
            if let Some(key) = serde_json::from_slice::<BlobIn>(line).ok().and_then(|b| decode_key(&b.blob)) {
                remember(&mut self.blobs, key, at);
            }
        } else if let Some(key) = serde_json::from_slice::<RunIn>(line).ok().and_then(|r| decode_key(&r.run)) {
            remember(&mut self.runs, key, at);
        }
    }

    /// The event with its large payload values taken out into blob and run
    /// lines, appended to `buf` (which the file continues with), and where
    /// they were taken out. `None` when nothing is taken out.
    pub fn compact(&mut self, event: &str, buf: &mut Vec<u8>) -> Option<(String, Vec<[u64; 2]>)> {
        let b = event.as_bytes();
        if !event.starts_with("{\"type\":\"message\",") {
            return None;
        }
        let envelope = field(b, 0, b"envelope")?;
        let payload = field(b, envelope.0, b"payload")?;
        if b[payload.0] != b'{' {
            return None;
        }
        let mut out = String::with_capacity(event.len());
        let mut refs = Vec::new();
        let mut last = 0;
        for (_, (start, end)) in members(b, payload.0)? {
            if end - start < MIN_VALUE {
                continue;
            }
            let at = if b[start] == b'[' {
                let items = elements(b, start)?;
                if items.is_empty() || items.len() > MAX_ITEMS {
                    continue;
                }
                self.run(buf, items.iter().map(|&(s, e)| &b[s..e]).collect())
            } else {
                self.blob(buf, &b[start..end])
            };
            out.push_str(&event[last..start]);
            refs.push([out.len() as u64, at]);
            out.push('0');
            last = end;
        }
        if refs.is_empty() {
            return None;
        }
        out.push_str(&event[last..]);
        Some((out, refs))
    }

    fn next(&self, buf: &[u8]) -> u64 {
        self.len + buf.len() as u64
    }

    fn blob(&mut self, buf: &mut Vec<u8>, data: &[u8]) -> u64 {
        let key = sha(data);
        if let Some(&at) = self.blobs.get(&key) {
            return at;
        }
        let at = self.next(buf);
        buf.extend_from_slice(BLOB);
        buf.extend_from_slice(format!("\"{}\",\"data\":", hex::encode(key)).as_bytes());
        buf.extend_from_slice(data);
        buf.extend_from_slice(b"}\n");
        remember(&mut self.blobs, key, at);
        at
    }

    /// A run of `items`, reusing the longest prefix already written.
    fn run(&mut self, buf: &mut Vec<u8>, items: Vec<&[u8]>) -> u64 {
        // The key of a prefix chains the hashes of its items.
        let mut keys = Vec::with_capacity(items.len());
        let mut key = [0u8; 32];
        for item in &items {
            let mut h = Sha256::new();
            h.update(key);
            h.update(sha(item));
            key = h.finalize().into();
            keys.push(key);
        }
        let known = keys.iter().rposition(|k| self.runs.contains_key(k));
        let prev = known.map(|i| self.runs[&keys[i]]);
        let from = known.map_or(0, |i| i + 1);
        if from == items.len() {
            return prev.expect("a whole known run has an offset");
        }
        let offsets: Vec<u64> = items[from..].iter().map(|item| self.blob(buf, item)).collect();
        let at = self.next(buf);
        let prev = prev.map_or("null".to_owned(), |p| p.to_string());
        let offsets = offsets.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        let line = format!("{{\"run\":\"{}\",\"prev\":{prev},\"items\":[{offsets}]}}\n", hex::encode(key));
        buf.extend_from_slice(line.as_bytes());
        remember(&mut self.runs, key, at);
        at
    }
}

/// Check a blob or run line read at `at`: it parses, a blob's data matches
/// its hash, and a run points only back.
pub fn check_stored(line: &[u8], at: u64) -> Result<(), String> {
    if line.starts_with(BLOB) {
        let b: BlobIn = serde_json::from_slice(line).map_err(|e| e.to_string())?;
        if decode_key(&b.blob) != Some(sha(b.data.get().as_bytes())) {
            return Err("blob does not match its hash".into());
        }
    } else {
        let r: RunIn = serde_json::from_slice(line).map_err(|e| e.to_string())?;
        if decode_key(&r.run).is_none() {
            return Err("run has no valid key".into());
        }
        if r.prev.is_some_and(|p| p >= at) || r.items.iter().any(|&i| i >= at) {
            return Err("run points forward".into());
        }
    }
    Ok(())
}

/// Reads blob and run lines back by offset, to restore entries.
pub struct Lines {
    file: std::io::BufReader<std::fs::File>,
}

impl Lines {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self { file: std::io::BufReader::with_capacity(64 * 1024, std::fs::File::open(path)?) })
    }

    fn line_at(&mut self, at: u64) -> Result<Vec<u8>, String> {
        self.file.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
        let mut line = Vec::new();
        let n = self
            .file
            .by_ref()
            .take(MAX_ENTRY_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?;
        if n == 0 || n > MAX_ENTRY_BYTES || line.last() != Some(&b'\n') {
            return Err(format!("no complete line at byte {at}"));
        }
        Ok(line)
    }

    /// The blob's data, appended to `out`.
    fn blob(&mut self, at: u64, here: u64, out: &mut String) -> Result<(), String> {
        if at >= here {
            return Err(format!("reference to byte {at} points forward"));
        }
        let line = self.line_at(at)?;
        push_blob(&line, at, out)
    }

    /// The value at `at`, a blob's data or a run's array, appended to `out`.
    fn value(&mut self, at: u64, here: u64, out: &mut String) -> Result<(), String> {
        if at >= here {
            return Err(format!("reference to byte {at} points forward"));
        }
        let line = self.line_at(at)?;
        if line.starts_with(BLOB) {
            return push_blob(&line, at, out);
        }
        if !line.starts_with(RUN) {
            return Err(format!("no blob or run at byte {at}"));
        }
        // Follow the run back to its first part, then write the items in order.
        let mut parts = Vec::new();
        let (mut next, mut bound) = (Some(at), here);
        while let Some(at) = next {
            if at >= bound {
                return Err(format!("run at byte {bound} points forward"));
            }
            let line = self.line_at(at)?;
            if !line.starts_with(RUN) {
                return Err(format!("no run at byte {at}"));
            }
            let r: RunIn = serde_json::from_slice(&line).map_err(|e| e.to_string())?;
            parts.push(r.items);
            (next, bound) = (r.prev, at);
        }
        out.push('[');
        for (i, item) in parts.into_iter().rev().flatten().enumerate() {
            if i > 0 {
                out.push(',');
            }
            self.blob(item, here, out)?;
            if out.len() > MAX_ENTRY_BYTES {
                return Err(format!("restored entry exceeds the {MAX_ENTRY_BYTES}-byte limit"));
            }
        }
        out.push(']');
        Ok(())
    }

    /// The event of the entry at `here` with its values put back.
    pub fn restore(&mut self, event: &str, refs: &[[u64; 2]], here: u64) -> Result<String, String> {
        let mut out = String::with_capacity(event.len());
        let mut last = 0usize;
        for &[pos, at] in refs {
            let pos = usize::try_from(pos).map_err(|e| e.to_string())?;
            if pos < last || event.as_bytes().get(pos) != Some(&b'0') {
                return Err(format!("reference at {pos} does not hold a value's place"));
            }
            out.push_str(&event[last..pos]);
            self.value(at, here, &mut out)?;
            if out.len() > MAX_ENTRY_BYTES {
                return Err(format!("restored entry exceeds the {MAX_ENTRY_BYTES}-byte limit"));
            }
            last = pos + 1;
        }
        out.push_str(&event[last..]);
        Ok(out)
    }
}

/// The data of the blob line read at `at`, appended to `out`.
fn push_blob(line: &[u8], at: u64, out: &mut String) -> Result<(), String> {
    if !line.starts_with(BLOB) {
        return Err(format!("no blob at byte {at}"));
    }
    let b: BlobIn = serde_json::from_slice(line).map_err(|e| e.to_string())?;
    let data = b.data.get();
    if decode_key(&b.blob) != Some(sha(data.as_bytes())) {
        return Err(format!("blob at byte {at} does not match its hash"));
    }
    out.push_str(data);
    Ok(())
}

/// The end of the JSON value starting at `i` in compact JSON.
fn value_end(b: &[u8], start: usize) -> Option<usize> {
    let (mut depth, mut in_str, mut esc) = (0usize, false, false);
    let mut i = start;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' if depth == 0 => return (i > start).then_some(i),
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                b',' | b':' if depth == 0 => return (i > start).then_some(i),
                _ => {}
            }
        }
        i += 1;
    }
    None
}

type Span = (usize, usize);

/// The key and value spans of the object starting at `start`.
fn members(b: &[u8], start: usize) -> Option<Vec<(Span, Span)>> {
    let mut out = Vec::new();
    let mut i = start + 1;
    if b.get(i) == Some(&b'}') {
        return Some(out);
    }
    loop {
        let key_end = value_end(b, i)?;
        if b.get(key_end) != Some(&b':') {
            return None;
        }
        let value_end = value_end(b, key_end + 1)?;
        out.push(((i, key_end), (key_end + 1, value_end)));
        match b.get(value_end) {
            Some(b',') => i = value_end + 1,
            Some(b'}') => return Some(out),
            _ => return None,
        }
    }
}

/// The item spans of the array starting at `start`.
fn elements(b: &[u8], start: usize) -> Option<Vec<Span>> {
    let mut out = Vec::new();
    let mut i = start + 1;
    if b.get(i) == Some(&b']') {
        return Some(out);
    }
    loop {
        let end = value_end(b, i)?;
        out.push((i, end));
        match b.get(end) {
            Some(b',') => i = end + 1,
            Some(b']') => return Some(out),
            _ => return None,
        }
    }
}

/// The value span of the member `name` in the object starting at `start`.
fn field(b: &[u8], start: usize, name: &[u8]) -> Option<Span> {
    members(b, start)?.into_iter().find(|&((ks, ke), _)| b[ks..ke] == [b"\"", name, b"\""].concat()).map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_of_compact_json_are_found() {
        let text = r#"{"a":"x\"}","b":[1,{"c":[2]},"]"],"d":null}"#;
        let b = text.as_bytes();
        let m = members(b, 0).unwrap();
        let values: Vec<&str> = m.iter().map(|&(_, (s, e))| &text[s..e]).collect();
        assert_eq!(values, [r#""x\"}""#, r#"[1,{"c":[2]},"]"]"#, "null"]);
        let items: Vec<&str> = elements(b, m[1].1 .0).unwrap().iter().map(|&(s, e)| &text[s..e]).collect();
        assert_eq!(items, ["1", r#"{"c":[2]}"#, r#""]""#]);
        assert_eq!(field(b, 0, b"d").map(|(s, e)| &text[s..e]), Some("null"));
        assert!(members(b"{}", 0).unwrap().is_empty() && elements(b"[]", 0).unwrap().is_empty());
    }
}
