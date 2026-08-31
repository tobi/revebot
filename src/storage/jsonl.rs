//! One session, one file, one line per transaction.
//!
//! The file is not the state; it is the **replay recipe** for the in-memory
//! maps (`docs/harness.md` §1.7). A transaction of one write is one JSON
//! object line; several writes are one array line. A torn final line is
//! discarded *whole* — including every element of an array — which is what
//! makes "no crash prefix inside a transaction" true here. A malformed line
//! anywhere *else* is not something a crash can do, so it is corruption and we
//! refuse to open.
//!
//! Every append is flushed. An agent that reports work it did not persist is
//! worse than an agent that is slow.
//!
//! **One writer per session, enforced.** The open file holds an exclusive OS
//! lock for as long as the storage lives; a second process opening the same
//! session gets [`StorageError::Locked`] instead of a silently corrupt
//! interleaving.
//!
//! **Snapshot compaction.** Every register `set` appends a line, so a 30-turn
//! run leaves dozens of dead `op.state` revisions behind once the terminal
//! transaction deletes the register. On open, when the dead-write ratio is
//! high, the file is rewritten as header plus surviving writes **in seq
//! order** through a temp file and an atomic rename. Surviving lines keep
//! their `seq`; the gaps are legal. Grouping by kind (entries, then usage,
//! then registers) would put an early usage `seq` after a later entry and
//! fail the monotonicity check on the next open.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write as IoWrite};
use std::path::{Path, PathBuf};

use crate::entry::{FORMAT_VERSION, Header, Line, RegisterWrite, STORAGE_VERSION, Write};
use crate::storage::{Result, Storage, StorageError};

#[derive(Debug)]
pub struct Sink {
    file: File,
    path: PathBuf,
}

impl Sink {
    pub fn append(&mut self, line: &Line) -> Result<()> {
        let mut text = serde_json::to_string(line).expect("a session line must serialise");
        text.push('\n');
        self.file.write_all(text.as_bytes())?;
        self.file.flush()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Dead register writes beyond which open rewrites the file.
const COMPACT_DEAD_WRITES: usize = 64;

struct Replay {
    header: Option<Header>,
    writes: Vec<Write>,
    /// Byte length of the intact prefix.
    valid_len: u64,
    /// Register writes superseded by a later set or delete on the same key.
    dead_writes: usize,
    /// Unique seqs, but not in file order (a grouped snapshot from an
    /// earlier compact). Replay sorts them; open rewrites the file.
    out_of_order: bool,
}

impl Storage {
    /// Create or reopen a JSONL-backed session.
    pub fn open(
        path: impl AsRef<Path>,
        id: impl Into<String>,
        cwd: Option<String>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        if file.try_lock().is_err() {
            return Err(StorageError::Locked(path.display().to_string()));
        }

        let replay = read_lines(&path)?;
        let existed = replay.header.is_some() || !replay.writes.is_empty();

        // Drop a torn tail before we append anything after it.
        let current_len = file.metadata()?.len();
        if replay.valid_len < current_len {
            file.set_len(replay.valid_len)?;
            file.sync_all()?;
        }

        let header = match replay.header {
            Some(header) => header,
            None if existed => {
                return Err(StorageError::Corrupt {
                    line: 1,
                    reason: "missing header".into(),
                });
            }
            None => Header::new(id, cwd),
        };
        if header.v != FORMAT_VERSION {
            return Err(StorageError::Version(header.v, FORMAT_VERSION));
        }
        if header.storage_version > STORAGE_VERSION {
            return Err(StorageError::StorageVersion(
                header.storage_version,
                STORAGE_VERSION,
            ));
        }

        file.seek(SeekFrom::End(0))?;
        let mut storage = Storage::with_header(header.clone(), Some(Sink { file, path }));
        if existed {
            for write in replay.writes {
                storage.replay(write);
            }
            if replay.out_of_order
                || (replay.dead_writes >= COMPACT_DEAD_WRITES
                    && replay.dead_writes > storage.register_count())
            {
                storage.compact_file()?;
            }
        } else {
            let sink = storage.sink.as_mut().expect("sink");
            sink.append(&Line::header(header))?;
        }
        Ok(storage)
    }

    /// Rewrite the file as header plus surviving writes in seq order, via
    /// temp file and atomic rename. Logical state is unchanged.
    pub fn compact_file(&mut self) -> Result<()> {
        let Some(sink) = self.sink.as_mut() else {
            return Ok(());
        };
        let path = sink.path.clone();
        let temp = path.with_extension("jsonl.compact");
        {
            let mut out = std::io::BufWriter::new(File::create(&temp)?);
            let mut line = |line: &Line| -> Result<()> {
                let mut text = serde_json::to_string(line).expect("serialise");
                text.push('\n');
                out.write_all(text.as_bytes())?;
                Ok(())
            };
            line(&Line::header(self.header.clone()))?;
            let mut writes: Vec<Write> = self
                .scan_entries(super::Order::OldestFirst)
                .into_iter()
                .map(|entry| Write::Entry(entry.clone()))
                .chain(self.all_usage().iter().cloned().map(Write::Usage))
                .chain(self.all_registers().map(|register| {
                    Write::Register(RegisterWrite::Set {
                        seq: register.seq,
                        namespace: register.namespace,
                        key: register.key.clone(),
                        value: register.value.clone(),
                    })
                }))
                .collect();
            writes.sort_by_key(Write::seq);
            for write in writes {
                line(&Line::Single(write))?;
            }
            out.flush()?;
            out.get_ref().sync_all()?;
        }
        // Rename over the locked file; the lock follows our open descriptor,
        // so reopen the new file and lock it before releasing the old one.
        std::fs::rename(&temp, &path)?;
        let mut file = OpenOptions::new().append(true).read(true).open(&path)?;
        if file.try_lock().is_err() {
            return Err(StorageError::Locked(path.display().to_string()));
        }
        file.seek(SeekFrom::End(0))?;
        self.sink = Some(Sink { file, path });
        Ok(())
    }

    pub fn path(&self) -> Option<&Path> {
        self.sink.as_ref().map(|s| s.path())
    }
}

/// Parse every line, returning the decoded writes and the byte length of the
/// intact prefix.
fn read_lines(path: &Path) -> Result<Replay> {
    let mut replay = Replay {
        header: None,
        writes: Vec::new(),
        valid_len: 0,
        dead_writes: 0,
        out_of_order: false,
    };
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(replay),
        Err(e) => return Err(e.into()),
    };

    let mut reader = BufReader::new(file);
    let mut raw = Vec::new();
    let mut number = 0usize;
    let mut last_seq = 0u64;
    let mut seen = HashSet::new();
    let mut live: std::collections::HashMap<(crate::entry::Namespace, String), ()> =
        std::collections::HashMap::new();

    loop {
        raw.clear();
        let read = reader.read_until(b'\n', &mut raw)?;
        if read == 0 {
            break;
        }
        number += 1;
        let complete = raw.last() == Some(&b'\n');
        let text = String::from_utf8_lossy(&raw);
        let trimmed = text.trim_end_matches(['\n', '\r']);

        if trimmed.is_empty() {
            if complete {
                replay.valid_len += read as u64;
            }
            continue;
        }

        let parsed = serde_json::from_str::<Line>(trimmed);
        let at_eof = reader.fill_buf()?.is_empty();
        let line = match parsed {
            Ok(line) if complete => line,
            // Parsed but no trailing newline, or unparseable at the very end:
            // the write was cut off. It was never acknowledged; drop it whole.
            Ok(_) => break,
            Err(e) if at_eof => {
                let _ = e;
                break;
            }
            Err(e) => {
                return Err(StorageError::Corrupt {
                    line: number,
                    reason: e.to_string(),
                });
            }
        };

        let writes = match line {
            Line::Header(h) => {
                if replay.header.is_some() {
                    return Err(StorageError::Corrupt {
                        line: number,
                        reason: "second header".into(),
                    });
                }
                replay.header = Some(h.header);
                replay.valid_len += read as u64;
                continue;
            }
            Line::Single(w) => vec![w],
            Line::Batch(ws) => ws,
        };
        for write in &writes {
            let seq = write.seq();
            if !seen.insert(seq) {
                return Err(StorageError::Corrupt {
                    line: number,
                    reason: format!("duplicate seq {seq}"),
                });
            }
            if seq <= last_seq {
                replay.out_of_order = true;
            }
            last_seq = last_seq.max(seq);
            if let Write::Register(r) = write {
                let key = match r {
                    RegisterWrite::Set { namespace, key, .. } => (*namespace, key.clone()),
                    RegisterWrite::Delete { namespace, key, .. } => (*namespace, key.clone()),
                };
                if live.remove(&key).is_some() {
                    replay.dead_writes += 1;
                }
                match r {
                    RegisterWrite::Set { .. } => {
                        live.insert(key, ());
                    }
                    RegisterWrite::Delete { .. } => {
                        replay.dead_writes += 1;
                    }
                }
            }
        }
        replay.writes.extend(writes);
        replay.valid_len += read as u64;
    }
    if replay.out_of_order {
        replay.writes.sort_by_key(Write::seq);
    }
    Ok(replay)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Entry, Header, Line, Namespace, RegisterWrite, Transaction};
    use crate::storage::Order;
    use serde_json::json;

    fn user(text: &str) -> Entry {
        Entry::message(json!({"role": "user", "content": text}))
    }

    fn temp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        (dir, path)
    }

    fn tx(writes: Vec<Write>) -> Transaction {
        Transaction { writes }
    }

    #[test]
    fn a_reopened_session_is_the_session_it_was() {
        let (_dir, path) = temp();
        let (a, b, seq) = {
            let mut s = Storage::open(&path, "s1", Some("workspace".into())).unwrap();
            let a = user("one");
            let b = user("two").with_parent(Some(a.id.clone()));
            s.commit(tx(vec![
                Write::entry(a.clone()),
                Write::set(Namespace::LaneLeaf, "main", a.id.as_str()),
            ]))
            .unwrap();
            s.commit(tx(vec![
                Write::entry(b.clone()),
                Write::set(Namespace::LaneLeaf, "main", b.id.as_str()),
                Write::set(Namespace::FactLabel, a.id.as_str(), "checkpoint"),
            ]))
            .unwrap();
            (a.id, b.id, s.seq())
        };

        let s = Storage::open(&path, "s1", None).unwrap();
        assert_eq!(s.seq(), seq, "the counter resumes, it does not restart");
        assert_eq!(
            s.register(Namespace::LaneLeaf, "main").unwrap().value,
            b.as_str()
        );
        assert_eq!(
            s.register(Namespace::FactLabel, a.as_str()).unwrap().value,
            "checkpoint"
        );
        assert_eq!(s.entry(&b).unwrap().parent_id, Some(a));
        assert_eq!(s.header().v, FORMAT_VERSION);
        assert_eq!(s.stats().message_count, 2);
    }

    #[test]
    fn a_torn_array_line_is_discarded_whole() {
        let (_dir, path) = temp();
        let a = {
            let mut s = Storage::open(&path, "s1", None).unwrap();
            let a = user("one");
            s.commit(tx(vec![Write::entry(a.clone())])).unwrap();
            a.id
        };

        // Die mid-write of a two-write transaction: the first element is a
        // complete JSON object, but the line is not.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(
            br#"[{"kind":"register","op":"set","seq":2,"namespace":"fact.name","key":"","value":"torn"},{"kind":"entry","id":"e_torn","se"#,
        )
        .unwrap();
        f.flush().unwrap();
        drop(f);

        let mut s = Storage::open(&path, "s1", None).unwrap();
        assert!(
            s.register(Namespace::FactName, "").is_none(),
            "no element of a torn transaction survives"
        );
        assert_eq!(s.entry_count(), 1);

        // Appends resume cleanly, and the file stays valid throughout.
        let b = user("two").with_parent(Some(a.clone()));
        s.commit(tx(vec![Write::entry(b.clone())])).unwrap();
        drop(s);
        let text = std::fs::read_to_string(&path).unwrap();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            serde_json::from_str::<Line>(line).expect("every line parses");
        }
        let s = Storage::open(&path, "s1", None).unwrap();
        assert!(s.entry(&b.id).is_some());
    }

    #[test]
    fn a_malformed_line_in_the_middle_is_corruption() {
        let (_dir, path) = temp();
        {
            let mut s = Storage::open(&path, "s1", None).unwrap();
            s.commit(tx(vec![Write::entry(user("one"))])).unwrap();
            s.commit(tx(vec![Write::entry(user("two"))])).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines[1] = "{not json at all";
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

        let err = Storage::open(&path, "s1", None).unwrap_err();
        assert!(matches!(err, StorageError::Corrupt { .. }), "got {err}");
    }

    #[test]
    fn a_non_increasing_seq_is_corruption() {
        let (_dir, path) = temp();
        std::fs::write(
            &path,
            "{\"kind\":\"header\",\"v\":4,\"id\":\"s1\",\"storageVersion\":1}\n\
             {\"kind\":\"register\",\"op\":\"set\",\"seq\":5,\"namespace\":\"fact.name\",\"key\":\"\",\"value\":\"a\"}\n\
             {\"kind\":\"register\",\"op\":\"set\",\"seq\":5,\"namespace\":\"fact.name\",\"key\":\"\",\"value\":\"b\"}\n\
             {\"kind\":\"register\",\"op\":\"set\",\"seq\":6,\"namespace\":\"fact.name\",\"key\":\"\",\"value\":\"c\"}\n",
        )
        .unwrap();
        let err = Storage::open(&path, "s1", None).unwrap_err();
        assert!(
            matches!(err, StorageError::Corrupt { line: 3, .. }),
            "got {err}"
        );
        assert!(err.to_string().contains("duplicate seq 5"), "got {err}");
    }

    #[test]
    fn a_kind_grouped_snapshot_reopens_and_rewrites_in_seq_order() {
        let (_dir, path) = temp();
        let mut entry = user("hi");
        entry.seq = 8;
        let usage = crate::entry::UsageRow {
            id: crate::ids::UsageId::new(),
            seq: 3,
            usage: crate::entry::Usage {
                input: 4,
                output: 1,
                cached_input: 0,
            },
            entry_id: Some(entry.id.clone()),
            adjustment: false,
            details: None,
        };
        let mut body = String::new();
        body.push_str(&serde_json::to_string(&Line::header(Header::new("s1", None))).unwrap());
        body.push('\n');
        body.push_str(&serde_json::to_string(&Line::Single(Write::Entry(entry.clone()))).unwrap());
        body.push('\n');
        body.push_str(&serde_json::to_string(&Line::Single(Write::Usage(usage))).unwrap());
        body.push('\n');
        body.push_str(
            &serde_json::to_string(&Line::Single(Write::Register(RegisterWrite::Set {
                seq: 1,
                namespace: Namespace::LaneLeaf,
                key: "main".into(),
                value: json!(entry.id.as_str()),
            })))
            .unwrap(),
        );
        body.push('\n');
        std::fs::write(&path, &body).unwrap();

        let s = Storage::open(&path, "s1", None).unwrap();
        assert_eq!(s.entry_count(), 1);
        assert_eq!(s.usage_count(), 1);
        assert_eq!(s.seq(), 8);
        assert_eq!(
            s.register(Namespace::LaneLeaf, "main")
                .map(|r| r.value.clone()),
            Some(json!(entry.id.as_str()))
        );

        let rewritten = std::fs::read_to_string(&path).unwrap();
        let mut last = 0u64;
        for line in rewritten.lines().skip(1) {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let seq = v["seq"].as_u64().expect(line);
            assert!(seq > last, "seq {seq} after {last} in {rewritten}");
            last = seq;
        }
        assert_eq!(last, 8);
    }

    #[test]
    fn a_future_format_version_is_refused_rather_than_guessed_at() {
        let (_dir, path) = temp();
        std::fs::write(
            &path,
            "{\"kind\":\"header\",\"v\":99,\"id\":\"s1\",\"storageVersion\":1}\n",
        )
        .unwrap();
        let err = Storage::open(&path, "s1", None).unwrap_err();
        assert!(matches!(err, StorageError::Version(99, 4)), "got {err}");
    }

    #[test]
    fn a_newer_storage_version_is_refused() {
        let (_dir, path) = temp();
        std::fs::write(
            &path,
            "{\"kind\":\"header\",\"v\":4,\"id\":\"s1\",\"storageVersion\":7}\n",
        )
        .unwrap();
        let err = Storage::open(&path, "s1", None).unwrap_err();
        assert!(
            matches!(err, StorageError::StorageVersion(7, 1)),
            "got {err}"
        );
    }

    #[test]
    fn a_second_process_cannot_open_a_live_session() {
        let (_dir, path) = temp();
        let first = Storage::open(&path, "s1", None).unwrap();
        let err = Storage::open(&path, "s1", None).unwrap_err();
        assert!(matches!(err, StorageError::Locked(_)), "got {err}");
        drop(first);
        Storage::open(&path, "s1", None).expect("the lock is released with the owner");
    }

    #[test]
    fn snapshot_compaction_keeps_logical_state_and_drops_dead_revisions() {
        let (_dir, path) = temp();
        let (entries, seq) = {
            let mut s = Storage::open(&path, "s1", None).unwrap();
            let a = user("a");
            s.commit(tx(vec![Write::entry(a.clone())])).unwrap();
            for i in 0..100 {
                s.commit(tx(vec![Write::set(
                    Namespace::OpState,
                    "op1",
                    json!({"rev": i}),
                )]))
                .unwrap();
            }
            s.commit(tx(vec![
                Write::delete(Namespace::OpState, "op1"),
                Write::set(Namespace::LaneLeaf, "main", a.id.as_str()),
            ]))
            .unwrap();
            (s.entry_count(), s.seq())
        };
        let before = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(before > 100);

        let s = Storage::open(&path, "s1", None).unwrap();
        let after = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(after < 10, "compacted to {after} lines");
        assert_eq!(s.entry_count(), entries);
        assert_eq!(s.seq(), seq, "surviving seq values are preserved");
        assert!(s.register(Namespace::OpState, "op1").is_none());
        assert!(s.register(Namespace::LaneLeaf, "main").is_some());

        // And the compacted file is a normal file: appends and reopen work.
        let mut s = s;
        s.commit(tx(vec![Write::set(Namespace::FactName, "", "after")]))
            .unwrap();
        drop(s);
        let s = Storage::open(&path, "s1", None).unwrap();
        assert_eq!(s.register(Namespace::FactName, "").unwrap().value, "after");
    }

    #[test]
    fn memory_and_jsonl_agree() {
        let (_dir, path) = temp();
        let mut disk = Storage::open(&path, "s1", None).unwrap();
        let mut mem = Storage::memory("s1");
        let a = user("one");
        for backend in [&mut disk, &mut mem] {
            backend
                .commit(tx(vec![
                    Write::entry(a.clone()),
                    Write::set(Namespace::LaneLeaf, "main", a.id.as_str()),
                ]))
                .unwrap();
            backend
                .commit(tx(vec![Write::set(Namespace::FactName, "", "parity")]))
                .unwrap();
        }
        assert_eq!(disk.seq(), mem.seq());
        assert_eq!(
            disk.scan_entries(Order::OldestFirst).len(),
            mem.scan_entries(Order::OldestFirst).len()
        );
        assert_eq!(disk.register_count(), mem.register_count());
        assert_eq!(
            disk.register(Namespace::FactName, "").map(|r| &r.value),
            mem.register(Namespace::FactName, "").map(|r| &r.value)
        );
    }
}
