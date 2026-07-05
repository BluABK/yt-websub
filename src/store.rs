//! Durable, append-only event log + compaction. Each notification becomes one
//! TSV line, fsync'd before the caller returns 2xx, so an acknowledged event is
//! never lost. A monotonic `seq` lets clients (streamarchiver) poll incrementally
//! with a cursor. An in-memory recent window serves the common poll cheaply and
//! backs duplicate suppression; older reads fall back to the file.

use std::collections::{HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::util::{now_unix, pct_decode, pct_encode, write_atomic};

const RECENT_CAP: usize = 2000;
const COMPACT_BYTES: u64 = 8 * 1024 * 1024;
const COMPACT_MARGIN: u64 = 1000;
/// Ack-independent retention floor: even if the consumer never advances the ack
/// cursor, compaction drops events older than this many below `max_seq`, so the
/// log cannot grow without bound and OOM the process under `MemoryMax`.
const MAX_RETAINED_EVENTS: u64 = 100_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub seq: u64,
    pub received_at: u64,
    pub kind: String,
    pub channel_id: String,
    pub video_id: String,
    pub ts: String,
    pub title: String,
}

pub struct Store {
    events_path: PathBuf,
    ack_path: PathBuf,
    log: File,
    next_seq: u64,
    recent: VecDeque<Event>,
    seen: HashSet<String>,
    ack_through: u64,
}

fn dedup_key(video_id: &str, ts: &str, kind: &str) -> String {
    format!("{}|{}|{}", video_id, ts, kind)
}

fn format_line(e: &Event) -> String {
    // Percent-encode EVERY string field, not just the title. pct_encode escapes
    // all control bytes (including TAB and newline) and '%', so no field can ever
    // contain a TAB or newline — otherwise a crafted channel_id/video_id/ts in a
    // signed-but-malformed notification could split a row (forging an event) or
    // add a stray field (silently dropping the event on reload). seq/received_at
    // are numeric and need no encoding.
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        e.seq,
        e.received_at,
        pct_encode(&e.kind),
        pct_encode(&e.channel_id),
        pct_encode(&e.video_id),
        pct_encode(&e.ts),
        pct_encode(&e.title)
    )
}

fn parse_line(line: &str) -> Option<Event> {
    let p: Vec<&str> = line.split('\t').collect();
    if p.len() != 7 {
        return None;
    }
    // pct_decode is identity for legacy raw fields (which never contained '%'),
    // so this stays backward-compatible with logs written before all-field encoding.
    Some(Event {
        seq: p[0].parse().ok()?,
        received_at: p[1].parse().ok()?,
        kind: pct_decode(p[2]),
        channel_id: pct_decode(p[3]),
        video_id: pct_decode(p[4]),
        ts: pct_decode(p[5]),
        title: pct_decode(p[6]),
    })
}

impl Store {
    pub fn open(dir: &Path) -> std::io::Result<Store> {
        fs::create_dir_all(dir)?;
        let events_path = dir.join("events.log");
        let ack_path = dir.join("ack.txt");

        let mut next_seq = 1u64;
        let mut recent: VecDeque<Event> = VecDeque::new();
        // Stream the log line-by-line rather than reading the whole file into one
        // String: an unbounded log (consumer stopped acking) must not OOM the
        // process on startup under MemoryMax.
        if let Ok(f) = File::open(&events_path) {
            for line in BufReader::new(f).lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break, // torn final line / read error: stop
                };
                if let Some(e) = parse_line(&line) {
                    if e.seq >= next_seq {
                        next_seq = e.seq.saturating_add(1); // never wrap on a forged/huge seq
                    }
                    recent.push_back(e);
                    if recent.len() > RECENT_CAP {
                        recent.pop_front();
                    }
                }
            }
        }
        let mut seen = HashSet::new();
        for e in &recent {
            seen.insert(dedup_key(&e.video_id, &e.ts, &e.kind));
        }
        let ack_through = fs::read_to_string(&ack_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let log = OpenOptions::new().create(true).append(true).open(&events_path)?;

        Ok(Store {
            events_path,
            ack_path,
            log,
            next_seq,
            recent,
            seen,
            ack_through,
        })
    }

    /// Append an event durably. Returns the assigned `seq`, or `None` if it was
    /// a duplicate of a recently-seen notification (suppressed).
    pub fn append(
        &mut self,
        kind: &str,
        channel_id: &str,
        video_id: &str,
        ts: &str,
        title: &str,
    ) -> std::io::Result<Option<u64>> {
        let key = dedup_key(video_id, ts, kind);
        if self.seen.contains(&key) {
            return Ok(None);
        }
        let e = Event {
            seq: self.next_seq,
            received_at: now_unix(),
            kind: kind.to_string(),
            channel_id: channel_id.to_string(),
            video_id: video_id.to_string(),
            ts: ts.to_string(),
            title: title.to_string(),
        };
        self.log.write_all(format_line(&e).as_bytes())?;
        self.log.flush()?;
        self.log.sync_data()?;

        self.next_seq += 1;
        self.seen.insert(key);
        self.recent.push_back(e.clone());
        if self.recent.len() > RECENT_CAP {
            if let Some(old) = self.recent.pop_front() {
                self.seen
                    .remove(&dedup_key(&old.video_id, &old.ts, &old.kind));
            }
        }
        Ok(Some(e.seq))
    }

    pub fn max_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    /// Events with `seq > after`, ascending, up to `max`. Served from the
    /// in-memory window when possible, else re-read from the file.
    pub fn events_after(&self, after: u64, max: usize) -> Vec<Event> {
        let recent_front = self.recent.front().map(|e| e.seq).unwrap_or(self.next_seq);
        if after + 1 >= recent_front {
            return self
                .recent
                .iter()
                .filter(|e| e.seq > after)
                .take(max)
                .cloned()
                .collect();
        }
        let mut out = Vec::new();
        // Stream rather than slurp the whole (possibly large) log into memory.
        if let Ok(f) = File::open(&self.events_path) {
            for line in BufReader::new(f).lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if let Some(e) = parse_line(&line) {
                    if e.seq > after {
                        out.push(e);
                        if out.len() >= max {
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    pub fn set_ack(&mut self, through: u64) {
        if through > self.ack_through {
            self.ack_through = through;
            // Durable, atomic rewrite so a torn write can't regress the ack
            // cursor (which would stall compaction) — and surface errors.
            if let Err(e) = write_atomic(&self.ack_path, self.ack_through.to_string().as_bytes()) {
                eprintln!("[store] failed to persist ack cursor: {}", e);
            }
        }
    }

    /// Rewrite the log dropping events at/below the acked horizon (minus a
    /// margin) once it grows large. `seq` values are never renumbered, so client
    /// cursors stay valid.
    pub fn maybe_compact(&mut self) -> std::io::Result<()> {
        let len = fs::metadata(&self.events_path).map(|m| m.len()).unwrap_or(0);
        if len < COMPACT_BYTES {
            return Ok(());
        }
        // Drop events at/below the acked horizon (minus a margin) AND enforce an
        // ack-independent retention floor, so a stalled ack cursor cannot let the
        // log grow without bound.
        let ack_floor = self.ack_through.saturating_sub(COMPACT_MARGIN);
        let seq_floor = self.max_seq().saturating_sub(MAX_RETAINED_EVENTS);
        let keep_above = ack_floor.max(seq_floor);

        let tmp = self.events_path.with_extension("log.tmp");
        let mut dropped: u64 = 0;
        {
            let reader = BufReader::new(File::open(&self.events_path)?);
            let mut w = BufWriter::new(File::create(&tmp)?);
            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if let Some(e) = parse_line(&line) {
                    if e.seq > keep_above {
                        w.write_all(line.as_bytes())?;
                        w.write_all(b"\n")?;
                    } else {
                        dropped += 1;
                    }
                }
            }
            w.flush()?;
            w.get_ref().sync_data()?;
        }
        // Nothing was reclaimable: don't rewrite the whole (large) file every hour
        // for zero progress — just discard the temp copy.
        if dropped == 0 {
            let _ = fs::remove_file(&tmp);
            return Ok(());
        }
        fs::rename(&tmp, &self.events_path)?;
        self.log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.events_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("yt_websub_test_{}", tag));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn append_dedup_and_cursor() {
        let dir = temp_dir("store_basic");
        let mut s = Store::open(&dir).unwrap();
        assert_eq!(s.append("new", "UC1", "vid1", "t1", "Title 1").unwrap(), Some(1));
        assert_eq!(s.append("new", "UC1", "vid2", "t2", "Title 2").unwrap(), Some(2));
        // exact duplicate suppressed
        assert_eq!(s.append("new", "UC1", "vid1", "t1", "Title 1").unwrap(), None);
        assert_eq!(s.max_seq(), 2);

        let after0 = s.events_after(0, 100);
        assert_eq!(after0.len(), 2);
        assert_eq!(after0[0].video_id, "vid1");
        let after1 = s.events_after(1, 100);
        assert_eq!(after1.len(), 1);
        assert_eq!(after1[0].seq, 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persists_across_reopen_and_skips_torn_line() {
        let dir = temp_dir("store_reopen");
        {
            let mut s = Store::open(&dir).unwrap();
            s.append("new", "UC1", "vidA", "tA", "A").unwrap();
            s.append("new", "UC1", "vidB", "tB", "B").unwrap();
        }
        // Simulate a torn final line from a crash mid-write.
        use std::io::Write as _;
        let mut f = OpenOptions::new()
            .append(true)
            .open(dir.join("events.log"))
            .unwrap();
        f.write_all(b"3\t999\tnew\tUC1\tvidC").unwrap(); // incomplete, no newline/columns
        drop(f);

        let s = Store::open(&dir).unwrap();
        assert_eq!(s.max_seq(), 2); // torn line ignored
        assert_eq!(s.events_after(0, 100).len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fields_with_tabs_and_newlines_do_not_corrupt_the_log() {
        let dir = temp_dir("store_inject");
        let mut s = Store::open(&dir).unwrap();
        // Tabs/newlines in the (normally structured) fields must be encoded, so
        // the row can't split or gain/lose a column.
        let seq = s
            .append("new", "UC1", "vid\twith\ttab", "ts\nwith\nnl", "ti\ttle\n")
            .unwrap();
        assert_eq!(seq, Some(1));
        let e = &s.events_after(0, 10)[0];
        assert_eq!(e.video_id, "vid\twith\ttab");
        assert_eq!(e.ts, "ts\nwith\nnl");
        drop(s);

        // Survives a reload intact: exactly one event, no split/dropped line.
        let s2 = Store::open(&dir).unwrap();
        assert_eq!(s2.max_seq(), 1);
        let e2 = &s2.events_after(0, 10)[0];
        assert_eq!(e2.video_id, "vid\twith\ttab");
        assert_eq!(e2.ts, "ts\nwith\nnl");
        assert_eq!(e2.title, "ti\ttle\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_does_not_wrap_next_seq_to_zero_on_forged_huge_seq() {
        let dir = temp_dir("store_seqwrap");
        {
            let _ = Store::open(&dir).unwrap();
        }
        // Hand-write a line with seq = u64::MAX (as a corrupted/hostile log would).
        use std::io::Write as _;
        let mut f = OpenOptions::new()
            .append(true)
            .open(dir.join("events.log"))
            .unwrap();
        writeln!(f, "{}\t1\tnew\tUC1\tvidX\ttsX\ttitle", u64::MAX).unwrap();
        drop(f);

        // With the old `e.seq + 1`, next_seq wrapped to 0 (release) → max_seq()==0,
        // silently breaking cursor monotonicity. saturating_add pins it at u64::MAX.
        let s = Store::open(&dir).unwrap();
        assert_eq!(s.max_seq(), u64::MAX - 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
