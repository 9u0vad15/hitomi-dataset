//! The state DB: every known gallery with its listing kind, liveness status
//! and last check time. Lives as a release asset between daily runs.

use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::hitomi::{Gallery, Verdict};

/// `kind`: 0 = Normal (listed in hitomi's nozomi index), 1 = Forbidden (not listed).
const NORMAL: i64 = 0;
/// `status`: 0 = unchecked, 1 = alive, 2 = dead (see [`crate::hitomi::verify`]).
const ALIVE: i64 = 1;
const DEAD: i64 = 2;

/// Columns shipped in parts and the base snapshot.
pub const DATA_COLS: &str =
    "id, kind, status, title, gallery_type, language, artists, groups_, series, characters, tags";
pub const DATA_COLUMNS_DDL: &str = "
    id           INTEGER PRIMARY KEY,
    kind         INTEGER NOT NULL DEFAULT 0,
    status       INTEGER NOT NULL DEFAULT 0,
    title        TEXT NOT NULL DEFAULT '',
    gallery_type TEXT NOT NULL DEFAULT '',
    language     TEXT,
    artists      TEXT NOT NULL DEFAULT '[]',
    groups_      TEXT NOT NULL DEFAULT '[]',
    series       TEXT NOT NULL DEFAULT '[]',
    characters   TEXT NOT NULL DEFAULT '[]',
    tags         TEXT NOT NULL DEFAULT '[]'";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Day {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl std::str::FromStr for Day {
    type Err = String;
    /// `YYYY-MM-DD`.
    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("expected YYYY-MM-DD, got {s:?}");
        let mut it = s.split('-');
        let mut next = || {
            it.next()
                .and_then(|v| v.parse::<u32>().ok())
                .ok_or_else(bad)
        };
        let (year, month, day) = (next()? as i32, next()?, next()?);
        if !(1..=12).contains(&month)
            || !(1..=days_in_month(year, month)).contains(&day)
            || s.len() != 10
        {
            return Err(bad());
        }
        Ok(Day { year, month, day })
    }
}

pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 31,
    }
}

impl std::fmt::Display for Day {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS galleries ({DATA_COLUMNS_DDL}, checked_at INTEGER);
         CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TEMP TABLE full_ids (id INTEGER PRIMARY KEY);
         CREATE TEMP TABLE state_ids (id INTEGER PRIMARY KEY);"
    ))?;
    Ok(conn)
}

/// Import the galleries of an Aurelia `hitomi_galleries.db` (unchecked).
pub fn seed(conn: &Connection, app_db: &Path) -> Result<usize> {
    conn.execute("ATTACH DATABASE ?1 AS app", [app_db.to_string_lossy()])?;
    let n = conn.execute(
        "INSERT OR IGNORE INTO galleries
           (id, kind, title, gallery_type, language, artists, groups_, series, characters, tags)
         SELECT id, kind, title, gallery_type, language, artists, groups_, series, characters, tags
         FROM app.galleries",
        [],
    );
    conn.execute_batch("DETACH DATABASE app")?;
    Ok(n?)
}

pub fn has_galleries(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS (SELECT 1 FROM galleries)", [], |r| r.get(0))?)
}

pub fn meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?)
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        [key, value],
    )?;
    Ok(())
}

fn ids(conn: &Connection, sql: &str) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn mark(conn: &Connection, table: &str, ids: impl IntoIterator<Item = i64>) -> Result<()> {
    let mut stmt = conn.prepare(&format!(
        "INSERT OR IGNORE INTO temp.{table} (id) VALUES (?1)"
    ))?;
    for id in ids {
        stmt.execute([id])?;
    }
    Ok(())
}

/// Set `kind` from the nozomi listing and return the IDs not in the DB yet.
/// Refuses a listing much smaller than the Normal set (a truncated download
/// would otherwise flip most galleries to Forbidden).
pub fn sync_listing(conn: &mut Connection, nozomi: &[i64]) -> Result<Vec<i64>> {
    let normal: i64 = conn.query_row("SELECT COUNT(*) FROM galleries WHERE kind = 0", [], |r| {
        r.get(0)
    })?;
    if (nozomi.len() as i64) < normal * 9 / 10 {
        bail!(
            "nozomi lists {} galleries, DB has {normal} Normal: refusing",
            nozomi.len()
        );
    }
    let tx = conn.transaction()?;
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS nozomi (id INTEGER PRIMARY KEY); DELETE FROM temp.nozomi;",
    )?;
    {
        let mut stmt = tx.prepare("INSERT OR IGNORE INTO temp.nozomi (id) VALUES (?1)")?;
        for &id in nozomi {
            stmt.execute([id])?;
        }
    }
    let flipped = ids(
        &tx,
        "SELECT id FROM galleries WHERE kind = 0 AND id NOT IN (SELECT id FROM temp.nozomi)
         UNION ALL
         SELECT id FROM galleries WHERE kind = 1 AND id IN (SELECT id FROM temp.nozomi)",
    )?;
    mark(&tx, "state_ids", flipped.iter().copied())?;
    tx.execute(
        "UPDATE galleries
         SET kind = CASE WHEN id IN (SELECT id FROM temp.nozomi) THEN 0 ELSE 1 END
         WHERE id IN (SELECT id FROM temp.state_ids)",
        [],
    )?;
    let new = ids(
        &tx,
        "SELECT id FROM temp.nozomi WHERE id NOT IN (SELECT id FROM galleries) ORDER BY id DESC",
    )?;
    tx.execute_batch("DROP TABLE temp.nozomi")?;
    tx.commit()?;
    eprintln!(
        "listing: {} kind changes, {} new IDs",
        flipped.len(),
        new.len()
    );
    Ok(new)
}

/// Insert newly listed galleries (Normal, alive as of `now`).
pub fn insert_new(conn: &mut Connection, galleries: &[Gallery], now: i64) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(&format!(
            "INSERT OR REPLACE INTO galleries ({DATA_COLS}, checked_at)
             VALUES (?1, {NORMAL}, {ALIVE}, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
        ))?;
        for g in galleries {
            stmt.execute(params![
                g.id,
                g.title,
                g.gallery_type,
                g.language,
                g.artists,
                g.groups,
                g.series,
                g.characters,
                g.tags,
                now
            ])?;
        }
    }
    mark(&tx, "full_ids", galleries.iter().map(|g| g.id))?;
    tx.commit()?;
    Ok(())
}

/// Galleries to verify today: never-checked Forbidden ones first (`suspect`
/// budget while any remain), then everything by oldest check (`rotate`).
pub fn verify_batch(conn: &Connection, suspect: usize, rotate: usize) -> Result<Vec<i64>> {
    let suspects_left: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM galleries WHERE checked_at IS NULL AND kind = 1)",
        [],
        |r| r.get(0),
    )?;
    let budget = if suspects_left { suspect } else { rotate };
    ids(
        conn,
        &format!(
            "SELECT id FROM galleries
             ORDER BY (checked_at IS NULL AND kind = 1) DESC, checked_at IS NOT NULL, checked_at, id DESC
             LIMIT {budget}"
        ),
    )
}

/// Record verdicts: `checked_at` for each, `status` for decided ones (changes
/// go into `temp.state_ids`). `Unknown` keeps the status and comes back in
/// rotation, so a gallery that keeps failing cannot hold the suspect budget.
pub fn apply_verdicts(
    conn: &mut Connection,
    verdicts: &[(i64, Verdict)],
    now: i64,
) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut changed = Vec::new();
    {
        let mut set = tx.prepare(
            "UPDATE galleries SET status = coalesce(?2, status), checked_at = ?3 WHERE id = ?1",
        )?;
        let mut old = tx.prepare("SELECT status FROM galleries WHERE id = ?1")?;
        for &(id, verdict) in verdicts {
            let status = match verdict {
                Verdict::Alive => Some(ALIVE),
                Verdict::Dead => Some(DEAD),
                Verdict::Unknown => None,
            };
            let Some(before) = old.query_row([id], |r| r.get::<_, i64>(0)).optional()? else {
                continue;
            };
            set.execute(params![id, status, now])?;
            if status.is_some_and(|s| s != before) {
                changed.push(id);
            }
        }
    }
    mark(&tx, "state_ids", changed.iter().copied())?;
    tx.commit()?;
    Ok(changed.len())
}

/// Write the client base snapshot (data columns only) to `out`.
pub fn export_base(conn: &Connection, out: &Path) -> Result<()> {
    let _ = std::fs::remove_file(out);
    conn.execute("ATTACH DATABASE ?1 AS base", [out.to_string_lossy()])?;
    let res = conn.execute_batch(&format!(
        "CREATE TABLE base.galleries ({DATA_COLUMNS_DDL});
         INSERT INTO base.galleries ({DATA_COLS}) SELECT {DATA_COLS} FROM main.galleries;"
    ));
    conn.execute_batch("DETACH DATABASE base")?;
    res?;
    Connection::open(out)?.execute_batch("VACUUM")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn db(name: &str) -> (PathBuf, Connection) {
        let d = std::env::temp_dir().join(format!(
            "hitomi_dataset_state_{name}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let c = open(&d.join("state.db")).unwrap();
        (d, c)
    }

    fn rows(c: &Connection) -> Vec<(i64, i64, i64)> {
        c.prepare("SELECT id, kind, status FROM galleries ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn marked(c: &Connection, table: &str) -> Vec<i64> {
        ids(c, &format!("SELECT id FROM temp.{table} ORDER BY id")).unwrap()
    }

    #[test]
    fn listing_sets_kind_and_reports_new_ids() {
        let (d, mut c) = db("listing");
        c.execute_batch(
            "INSERT INTO galleries (id, kind) VALUES (1, 0), (2, 0), (3, 1), (4, 1), (5, 0)",
        )
        .unwrap();
        assert_eq!(sync_listing(&mut c, &[1, 3, 5, 9, 8]).unwrap(), [9, 8]);
        assert_eq!(
            rows(&c),
            [(1, 0, 0), (2, 1, 0), (3, 0, 0), (4, 1, 0), (5, 0, 0)]
        );
        assert_eq!(marked(&c, "state_ids"), [2, 3]);
        assert!(
            sync_listing(&mut c, &[1]).is_err(),
            "truncated listing accepted"
        );
        drop(c);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn verdicts_and_batch_order() {
        let (d, mut c) = db("verdicts");
        c.execute_batch(
            "INSERT INTO galleries (id, kind, status, checked_at) VALUES
               (1, 0, 0, NULL), (2, 1, 0, NULL), (3, 0, 1, 50), (4, 0, 1, 10), (5, 1, 0, NULL)",
        )
        .unwrap();
        // Unchecked Forbidden galleries first, with the suspect budget.
        assert_eq!(verify_batch(&c, 2, 9).unwrap(), [5, 2]);
        let verdicts = [
            (5, Verdict::Dead),
            (2, Verdict::Alive),
            (3, Verdict::Alive),
            (4, Verdict::Unknown),
            (99, Verdict::Dead),
        ];
        assert_eq!(apply_verdicts(&mut c, &verdicts, 100).unwrap(), 2);
        assert_eq!(marked(&c, "state_ids"), [2, 5]);
        assert_eq!(
            rows(&c),
            [(1, 0, 0), (2, 1, 1), (3, 0, 1), (4, 0, 1), (5, 1, 2)]
        );
        // None left: rotate budget, never checked first, then oldest check
        // (Unknown 4 was stamped too, so it waits its turn).
        assert_eq!(verify_batch(&c, 2, 9).unwrap(), [1, 5, 4, 3, 2]);
        drop(c);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn day_parsing() {
        assert_eq!(
            "2026-11-08".parse::<Day>().unwrap().to_string(),
            "2026-11-08"
        );
        for bad in ["2026-1-08", "2026-13-01", "2026-11-00", "2026-02-29", "x"] {
            assert!(bad.parse::<Day>().is_err(), "{bad}");
        }
    }
}
