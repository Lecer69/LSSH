//! Session recording in the asciicast v2 format, playable with
//! `asciinema play`. Only what the remote prints is recorded (the shell
//! area, without the stats bar).

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::json;

use crate::store;

pub struct Recorder {
    out: BufWriter<File>,
    start: Instant,
    /// The start of a UTF-8 character split across two reads.
    partial: Vec<u8>,
}

impl Recorder {
    /// Start recording a session with server `name` on a `cols` x `rows`
    /// screen. Returns the recorder and the file it writes to.
    pub fn create(name: &str, cols: u16, rows: u16) -> Result<(Recorder, PathBuf)> {
        Recorder::create_in(&store::data_dir()?.join("recordings"), name, cols, rows)
    }

    fn create_in(dir: &Path, name: &str, cols: u16, rows: u16) -> Result<(Recorder, PathBuf)> {
        store::create_private_dir(dir)?;
        let now = store::now();
        let path = dir.join(format!("{name}-{}.cast", local_timestamp(now)));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;

        let mut recorder = Recorder {
            out: BufWriter::new(file),
            start: Instant::now(),
            partial: Vec::new(),
        };
        let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
        let header = json!({
            "version": 2,
            "width": cols,
            "height": rows,
            "timestamp": now,
            "title": format!("lssh · {name}"),
            "env": { "TERM": term },
        });
        writeln!(recorder.out, "{header}").context("failed to write the recording")?;
        Ok((recorder, path))
    }

    /// Record output from the remote.
    pub fn output(&mut self, data: &[u8]) {
        self.partial.extend_from_slice(data);
        let complete = complete_len(&self.partial);
        if complete == 0 {
            return;
        }
        let chunk: Vec<u8> = self.partial.drain(..complete).collect();
        self.event("o", &String::from_utf8_lossy(&chunk));
    }

    /// Record a change of the shell area's size.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.event("r", &format!("{cols}x{rows}"));
    }

    fn event(&mut self, kind: &str, data: &str) {
        let elapsed = (self.start.elapsed().as_secs_f64() * 1e6).round() / 1e6;
        // A failing disk must not break the session; the file just ends.
        let _ = writeln!(self.out, "{}", json!([elapsed, kind, data]));
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if !self.partial.is_empty() {
            let rest = std::mem::take(&mut self.partial);
            self.event("o", &String::from_utf8_lossy(&rest));
        }
        let _ = self.out.flush();
    }
}

/// Length of `bytes` without a trailing, incomplete UTF-8 character.
fn complete_len(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        if byte & 0xC0 == 0x80 {
            continue; // continuation byte, keep looking for the lead byte
        }
        let needed = match byte {
            0xF0.. => 4,
            0xE0.. => 3,
            0xC0.. => 2,
            _ => 1,
        };
        return if needed > back {
            bytes.len() - back
        } else {
            bytes.len()
        };
    }
    bytes.len()
}

/// `YYYYMMDD-HHMMSS` in local time.
fn local_timestamp(secs: u64) -> String {
    let time = secs as libc::time_t;
    // SAFETY: `tm` is plain data, and localtime_r only writes into it.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&time, &mut tm);
        tm
    };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_split_utf8_characters_for_later() {
        let euro = "€".as_bytes(); // 3 bytes
        assert_eq!(complete_len(b"abc"), 3);
        assert_eq!(complete_len(&[b'a', euro[0]]), 1);
        assert_eq!(complete_len(&[b'a', euro[0], euro[1]]), 1);
        assert_eq!(complete_len(&[b'a', euro[0], euro[1], euro[2]]), 4);
        assert_eq!(complete_len(&[]), 0);
    }

    #[test]
    fn timestamp_shape() {
        let stamp = local_timestamp(1_700_000_000);
        assert_eq!(stamp.len(), 15);
        assert!(stamp.starts_with("2023111"), "{stamp}");
        assert_eq!(&stamp[8..9], "-");
    }

    #[test]
    fn writes_asciicast() {
        let dir = std::env::temp_dir().join(format!("lssh-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut recorder, path) = Recorder::create_in(&dir, "web", 80, 24).unwrap();
        let euro = "€".as_bytes();
        recorder.output(b"hi ");
        recorder.output(&euro[..1]);
        recorder.output(&euro[1..]);
        recorder.resize(100, 30);
        drop(recorder);

        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["version"], 2);
        assert_eq!(lines[0]["width"], 80);
        assert_eq!(lines[1][1], "o");
        assert_eq!(lines[1][2], "hi ");
        assert_eq!(lines[2][2], "€");
        assert_eq!(lines[3][1], "r");
        assert_eq!(lines[3][2], "100x30");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
