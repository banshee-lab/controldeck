//! SQLite persistence for metric samples with a 7-day retention policy.

use crate::collector::Sample;
use rusqlite::{Connection, params};
use std::path::Path;

pub const RETENTION_SECS: i64 = 7 * 24 * 60 * 60;

/// Open (creating directories and schema as needed) the database file.
pub fn init(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let conn = Connection::open(path)?;
    // journal_mode returns a row, so it must be run as a query. Best effort.
    let _ = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0));
    init_schema(&conn)?;
    Ok(conn)
}

pub fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS metrics (
            ts     INTEGER PRIMARY KEY,
            cpu    REAL NOT NULL,
            memory REAL NOT NULL,
            swap   REAL NOT NULL,
            disk   REAL NOT NULL
        );",
    )
}

pub fn insert(conn: &Connection, s: &Sample) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO metrics (ts, cpu, memory, swap, disk) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![s.ts, s.cpu, s.memory, s.swap, s.disk],
    )?;
    Ok(())
}

/// Samples with `ts >= since`, oldest first.
pub fn history(conn: &Connection, since: i64) -> rusqlite::Result<Vec<Sample>> {
    let mut stmt = conn
        .prepare("SELECT ts, cpu, memory, swap, disk FROM metrics WHERE ts >= ?1 ORDER BY ts ASC")?;
    let rows = stmt.query_map(params![since], |r| {
        Ok(Sample {
            ts: r.get(0)?,
            cpu: r.get(1)?,
            memory: r.get(2)?,
            swap: r.get(3)?,
            disk: r.get(4)?,
        })
    })?;
    rows.collect()
}

/// Delete everything older than the retention window. Returns rows removed.
pub fn cleanup(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM metrics WHERE ts < ?1", params![now - RETENTION_SECS])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: i64) -> Sample {
        Sample { ts, cpu: 10.0, memory: 20.0, swap: 0.0, disk: 30.0 }
    }

    fn mem_db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init_schema(&c).unwrap();
        c
    }

    #[test]
    fn insert_and_query_history() {
        let c = mem_db();
        for ts in [100, 200, 300] {
            insert(&c, &sample(ts)).unwrap();
        }
        let h = history(&c, 200).unwrap();
        assert_eq!(h.iter().map(|s| s.ts).collect::<Vec<_>>(), vec![200, 300]);
    }

    #[test]
    fn cleanup_removes_only_old_rows() {
        let c = mem_db();
        let now = 10_000_000;
        insert(&c, &sample(now - RETENTION_SECS - 1)).unwrap();
        insert(&c, &sample(now - RETENTION_SECS + 1)).unwrap();
        insert(&c, &sample(now)).unwrap();
        assert_eq!(cleanup(&c, now).unwrap(), 1);
        assert_eq!(history(&c, 0).unwrap().len(), 2);
    }

    #[test]
    fn init_creates_file_and_dirs() {
        let dir = std::env::temp_dir().join(format!("controldeck-test-{}", std::process::id()));
        let path = dir.join("nested").join("metrics.db");
        let c = init(&path).unwrap();
        insert(&c, &sample(1)).unwrap();
        assert!(path.exists());
        drop(c);
        let _ = std::fs::remove_dir_all(dir);
    }
}
