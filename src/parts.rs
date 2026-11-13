//! Change files ("parts") under `data/hitomi/{year}/`, and their compaction:
//! one file per day, the days 1–7 / 8–14 / 15–21 / 22–28 of the current
//! month merged into week files once the week is over, and every past month
//! merged into one month file — a finished year holds `YYYY-01.db` … `YYYY-12.db`.
//!
//! A part holds two tables, and an ID is in at most one of them:
//! - `galleries`: full rows (new galleries), in the dataset's column layout;
//! - `states`: `(id, kind, status)` changes of galleries the reader already has.
//!
//! Applying parts in `manifest.json` order (each one: upsert `galleries`, then
//! update `kind`/`status` from `states`) reproduces the dataset; reapplying a
//! part is harmless, so a merged file may overlap days a reader already applied.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::state::{days_in_month, Day, DATA_COLS, DATA_COLUMNS_DDL};

/// Parts directory, relative to the data repository root.
pub const PARTS_DIR: &str = "data/hitomi";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Span {
    Month,
    /// Week `n` (1–4) covers days `7n-6 ..= 7n`.
    Week(u32),
    Day(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartName {
    pub year: i32,
    pub month: u32,
    pub span: Span,
}

impl PartName {
    pub fn day(d: Day) -> Self {
        Self {
            year: d.year,
            month: d.month,
            span: Span::Day(d.day),
        }
    }

    pub fn file_name(&self) -> String {
        let (y, m) = (self.year, self.month);
        match self.span {
            Span::Month => format!("{y:04}-{m:02}.db"),
            Span::Week(w) => format!("{y:04}-{m:02}-w{w}.db"),
            Span::Day(d) => format!("{y:04}-{m:02}-{d:02}.db"),
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        let stem = name.strip_suffix(".db")?;
        let mut it = stem.splitn(3, '-');
        let year = it.next()?.parse().ok()?;
        let month = it.next()?.parse().ok().filter(|m| (1..=12).contains(m))?;
        let span = match it.next() {
            None => Span::Month,
            Some(w) if w.starts_with('w') => {
                Span::Week(w[1..].parse().ok().filter(|w| (1..=4).contains(w))?)
            }
            Some(d) => Span::Day(
                d.parse()
                    .ok()
                    .filter(|d| (1..=days_in_month(year, month)).contains(d))?,
            ),
        };
        let part = Self { year, month, span };
        (part.file_name() == name).then_some(part)
    }

    /// First and last day covered.
    pub fn days(&self) -> (u32, u32) {
        match self.span {
            Span::Month => (1, days_in_month(self.year, self.month)),
            Span::Week(w) => (7 * w - 6, 7 * w),
            Span::Day(d) => (d, d),
        }
    }

    fn date(&self, day: u32) -> String {
        format!("{:04}-{:02}-{day:02}", self.year, self.month)
    }

    fn last_date(&self) -> String {
        self.date(self.days().1)
    }

    fn path(&self, parts_root: &Path) -> PathBuf {
        parts_root
            .join(format!("{:04}", self.year))
            .join(self.file_name())
    }
}

/// DDL of a part's tables in database `schema` (`main` or an attached name).
fn part_ddl(schema: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {schema}.galleries ({DATA_COLUMNS_DDL});
         CREATE TABLE IF NOT EXISTS {schema}.states (
             id INTEGER PRIMARY KEY, kind INTEGER NOT NULL, status INTEGER NOT NULL);"
    )
}

/// SQL applying the part attached as `src` to the `galleries` table of `main`
/// (a reader's DB, the state, or a part being merged).
fn apply_sql() -> String {
    format!(
        "INSERT OR REPLACE INTO galleries ({DATA_COLS}) SELECT {DATA_COLS} FROM src.galleries;
         UPDATE galleries SET kind = s.kind, status = s.status
           FROM src.states s WHERE galleries.id = s.id;"
    )
}

/// Write the changes of `day` from the state DB (ID lists in `temp.full_ids` /
/// `temp.state_ids`) into a new part. A published part is never rewritten.
pub fn write_day(state: &Connection, parts_root: &Path, day: Day) -> Result<PathBuf> {
    let name = PartName::day(day);
    let path = name.path(parts_root);
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    fs::create_dir_all(path.parent().unwrap())?;
    let fresh = path.with_extension("db.new");
    let _ = fs::remove_file(&fresh);

    state.execute("ATTACH DATABASE ?1 AS p", [fresh.to_string_lossy()])?;
    let res = (|| -> Result<()> {
        state.execute_batch(&format!(
            "{}
             INSERT INTO p.galleries ({DATA_COLS})
               SELECT {DATA_COLS} FROM main.galleries WHERE id IN (SELECT id FROM temp.full_ids);
             INSERT INTO p.states (id, kind, status)
               SELECT id, kind, status FROM main.galleries
               WHERE id IN (SELECT id FROM temp.state_ids)
                 AND id NOT IN (SELECT id FROM temp.full_ids);",
            part_ddl("p")
        ))?;
        Ok(())
    })();
    state.execute_batch("DETACH DATABASE p")?;
    res?;

    fs::rename(&fresh, &path)?;
    Ok(path)
}

/// Merge `sources` (oldest first, may include `target`) into `target`, then
/// delete the other sources. A crash before the deletes leaves finer parts
/// covered by `target`; [`compact`] removes those.
fn merge_into(target: &Path, sources: &[PathBuf]) -> Result<()> {
    let tmp = target.with_extension("db.merge");
    let _ = fs::remove_file(&tmp);
    let conn = Connection::open(&tmp)?;
    conn.execute_batch(&part_ddl("main"))?;
    for src in sources {
        conn.execute("ATTACH DATABASE ?1 AS src", [src.to_string_lossy()])?;
        let res = conn.execute_batch(&format!(
            "BEGIN;
             {}
             DELETE FROM states WHERE id IN (SELECT id FROM src.galleries);
             INSERT OR REPLACE INTO states (id, kind, status)
               SELECT id, kind, status FROM src.states
               WHERE id NOT IN (SELECT id FROM galleries);
             COMMIT;",
            apply_sql()
        ));
        if res.is_err() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        conn.execute_batch("DETACH DATABASE src")?;
        res.with_context(|| format!("merge {}", src.display()))?;
    }
    conn.execute_batch("VACUUM")?;
    drop(conn);
    fs::rename(&tmp, target)?;
    for src in sources.iter().filter(|s| s.as_path() != target) {
        fs::remove_file(src)?;
    }
    Ok(())
}

fn list_parts(parts_root: &Path) -> Result<Vec<(PartName, PathBuf)>> {
    let mut parts = Vec::new();
    if !parts_root.exists() {
        return Ok(parts);
    }
    for year_dir in fs::read_dir(parts_root)? {
        let year_dir = year_dir?.path();
        if !year_dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&year_dir)? {
            let path = entry?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            match PartName::parse(name) {
                Some(p) => parts.push((p, path)),
                None if name.ends_with(".db") => bail!("unexpected part file {}", path.display()),
                None => {}
            }
        }
    }
    parts.sort_by_key(|(p, _)| (p.year, p.month, p.days().0, p.span));
    Ok(parts)
}

/// Merge finished weeks of `today`'s month into week files and every earlier
/// month into its month file.
pub fn compact(parts_root: &Path, today: Day) -> Result<()> {
    // Leftovers of an interrupted merge: already inside a coarser file.
    let all = list_parts(parts_root)?;
    for (p, path) in &all {
        let covered = all.iter().any(|(q, _)| {
            (q.year, q.month) == (p.year, p.month)
                && q.span < p.span
                && q.days().0 <= p.days().0
                && p.days().1 <= q.days().1
        });
        if covered {
            fs::remove_file(path)?;
        }
    }
    let parts = list_parts(parts_root)?;
    let mut months: Vec<(i32, u32)> = parts.iter().map(|(p, _)| (p.year, p.month)).collect();
    months.dedup();
    for (year, month) in months {
        let in_month = || {
            parts
                .iter()
                .filter(move |(p, _)| (p.year, p.month) == (year, month))
        };
        if (year, month) < (today.year, today.month) {
            let sources: Vec<PathBuf> = in_month().map(|(_, path)| path.clone()).collect();
            let target = PartName {
                year,
                month,
                span: Span::Month,
            };
            if sources.len() > 1 || in_month().any(|(p, _)| p.span != Span::Month) {
                merge_into(&target.path(parts_root), &sources)?;
            }
        } else if (year, month) == (today.year, today.month) {
            for week in (1..=4).filter(|w| today.day > 7 * w) {
                let target = PartName {
                    year,
                    month,
                    span: Span::Week(week),
                };
                let (first, last) = target.days();
                let in_week: Vec<PathBuf> = in_month()
                    .filter(|(p, _)| match p.span {
                        Span::Week(w) => w == week,
                        Span::Day(d) => (first..=last).contains(&d),
                        Span::Month => false,
                    })
                    .map(|(_, path)| path.clone())
                    .collect();
                let has_days = in_month()
                    .any(|(p, _)| matches!(p.span, Span::Day(d) if (first..=last).contains(&d)));
                if has_days {
                    merge_into(&target.path(parts_root), &in_week)?;
                }
            }
        }
    }
    Ok(())
}

/// Last day covered by any part (`YYYY-MM-DD`).
pub fn latest_day(parts_root: &Path) -> Result<Option<String>> {
    Ok(list_parts(parts_root)?
        .iter()
        .map(|(p, _)| p.last_date())
        .max())
}

/// Apply every part covering days after `after` (all if `None`) to `conn`'s
/// galleries, oldest first. Returns how many were applied.
pub fn apply_after(conn: &Connection, parts_root: &Path, after: Option<&str>) -> Result<usize> {
    let mut applied = 0;
    for (p, path) in list_parts(parts_root)? {
        if after.is_some_and(|a| p.last_date().as_str() <= a) {
            continue;
        }
        conn.execute("ATTACH DATABASE ?1 AS src", [path.to_string_lossy()])?;
        let res = conn.execute_batch(&format!("BEGIN; {} COMMIT;", apply_sql()));
        if res.is_err() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        conn.execute_batch("DETACH DATABASE src")?;
        res.with_context(|| format!("apply {}", path.display()))?;
        applied += 1;
    }
    Ok(applied)
}

/// Write `manifest.json`: the base snapshot record (`base.json`, if any) and
/// every part in apply order with its date range and SHA-256.
pub fn write_manifest(repo: &Path) -> Result<()> {
    let base = match fs::read_to_string(repo.join("base.json")) {
        Ok(s) => serde_json::from_str(&s).context("base.json")?,
        Err(_) => serde_json::Value::Null,
    };
    let mut parts = Vec::new();
    for (name, path) in list_parts(&repo.join(PARTS_DIR))? {
        let bytes = fs::read(&path)?;
        let rel = path
            .strip_prefix(repo)?
            .to_string_lossy()
            .replace('\\', "/");
        parts.push(serde_json::json!({
            "path": rel,
            "first": name.date(name.days().0),
            "last": name.last_date(),
            "size": bytes.len(),
            "sha256": sha256_hex(&bytes),
        }));
    }
    let manifest = serde_json::json!({ "base": base, "parts": parts });
    fs::write(
        repo.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)? + "\n",
    )?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hitomi_dataset_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn day(s: &str) -> Day {
        s.parse().unwrap()
    }

    fn month(year: i32, month: u32) -> PartName {
        PartName {
            year,
            month,
            span: Span::Month,
        }
    }

    /// A part with full rows `(id, kind, status, title)` and states `(id, kind, status)`.
    fn make_part(
        root: &Path,
        name: PartName,
        full: &[(i64, i64, i64, &str)],
        states: &[(i64, i64, i64)],
    ) {
        let path = name.path(root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let c = Connection::open(path).unwrap();
        c.execute_batch(&part_ddl("main")).unwrap();
        for (id, k, s, t) in full {
            c.execute(
                "INSERT INTO galleries (id, kind, status, title) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, k, s, t],
            )
            .unwrap();
        }
        for (id, k, s) in states {
            c.execute("INSERT INTO states VALUES (?1, ?2, ?3)", [id, k, s])
                .unwrap();
        }
    }

    type Rows = (Vec<(i64, i64, i64, String)>, Vec<(i64, i64, i64)>);

    fn read(path: &Path) -> Rows {
        let c = Connection::open(path).unwrap();
        let full = c
            .prepare("SELECT id, kind, status, title FROM galleries ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let states = c
            .prepare("SELECT id, kind, status FROM states ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        (full, states)
    }

    fn files(root: &Path) -> Vec<String> {
        list_parts(root)
            .unwrap()
            .into_iter()
            .map(|(p, _)| p.file_name())
            .collect()
    }

    fn ids_in(root: &Path, name: &str) -> Vec<i64> {
        read(&PartName::parse(name).unwrap().path(root))
            .0
            .iter()
            .map(|r| r.0)
            .collect()
    }

    #[test]
    fn part_names_round_trip() {
        for n in ["2026-11.db", "2026-11-w4.db", "2026-02-28.db"] {
            assert_eq!(PartName::parse(n).unwrap().file_name(), n);
        }
        for n in [
            "2026-13.db",
            "2026-11-w5.db",
            "2026-11-1.db",
            "2026-11-32.db",
            "2026-11.db.new",
        ] {
            assert!(PartName::parse(n).is_none(), "{n}");
        }
        assert_eq!(PartName::parse("2028-02.db").unwrap().days(), (1, 29));
    }

    /// Later parts win: a full row absorbs earlier and later state changes,
    /// and an ID never ends up in both tables.
    #[test]
    fn merge_applies_in_order() {
        let root = tmp("merge");
        let d1 = PartName::day(day("2026-11-01"));
        let d2 = PartName::day(day("2026-11-02"));
        make_part(&root, d1, &[(5, 0, 1, "a")], &[(6, 0, 2), (7, 1, 0)]);
        make_part(&root, d2, &[(6, 0, 1, "b")], &[(5, 1, 2), (7, 0, 1)]);
        let target = month(2026, 11).path(&root);
        merge_into(&target, &[d1.path(&root), d2.path(&root)]).unwrap();
        assert_eq!(
            read(&target),
            (
                vec![(5, 1, 2, "a".into()), (6, 0, 1, "b".into())],
                vec![(7, 0, 1)]
            )
        );
        assert_eq!(files(&root), ["2026-11.db"]);
        let _ = fs::remove_dir_all(&root);
    }

    /// Days 1–7 merge into w1 once day 8 runs; a past month becomes one file
    /// (including its week files); re-running changes nothing.
    #[test]
    fn compaction_schedule() {
        let root = tmp("compact");
        for (d, id) in [
            ("2026-10-29", 129),
            ("2026-10-31", 131),
            ("2026-11-01", 1),
            ("2026-11-02", 2),
            ("2026-11-07", 7),
            ("2026-11-08", 8),
        ] {
            make_part(&root, PartName::day(day(d)), &[(id, 0, 1, d)], &[]);
        }
        let oct_w1 = PartName {
            year: 2026,
            month: 10,
            span: Span::Week(1),
        };
        make_part(&root, oct_w1, &[(101, 0, 1, "w")], &[]);

        compact(&root, day("2026-11-07")).unwrap();
        assert_eq!(
            files(&root),
            [
                "2026-10.db",
                "2026-11-01.db",
                "2026-11-02.db",
                "2026-11-07.db",
                "2026-11-08.db"
            ]
        );
        assert_eq!(ids_in(&root, "2026-10.db"), [101, 129, 131]);

        for _ in 0..2 {
            compact(&root, day("2026-11-08")).unwrap();
            assert_eq!(
                files(&root),
                ["2026-10.db", "2026-11-w1.db", "2026-11-08.db"]
            );
        }
        assert_eq!(ids_in(&root, "2026-11-w1.db"), [1, 2, 7]);

        compact(&root, day("2026-12-01")).unwrap();
        assert_eq!(files(&root), ["2026-10.db", "2026-11.db"]);
        assert_eq!(ids_in(&root, "2026-11.db"), [1, 2, 7, 8]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn manifest_lists_parts_in_apply_order() {
        let repo = tmp("manifest");
        let root = repo.join(PARTS_DIR);
        make_part(&root, PartName::day(day("2026-11-08")), &[], &[]);
        make_part(
            &root,
            PartName {
                year: 2026,
                month: 11,
                span: Span::Week(1),
            },
            &[],
            &[],
        );
        make_part(&root, month(2026, 10), &[], &[]);
        write_manifest(&repo).unwrap();
        let m: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(repo.join("manifest.json")).unwrap()).unwrap();
        let ranges: Vec<[&str; 3]> = m["parts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| ["path", "first", "last"].map(|k| p[k].as_str().unwrap()))
            .collect();
        assert_eq!(
            ranges,
            [
                ["data/hitomi/2026/2026-10.db", "2026-10-01", "2026-10-31"],
                ["data/hitomi/2026/2026-11-w1.db", "2026-11-01", "2026-11-07"],
                ["data/hitomi/2026/2026-11-08.db", "2026-11-08", "2026-11-08"],
            ]
        );
        assert!(m["base"].is_null());
        let _ = fs::remove_dir_all(&repo);
    }

    /// A crash after replacing the week file but before deleting its days
    /// leaves days the week already contains; they must not be replayed.
    #[test]
    fn interrupted_merge_leftovers_are_dropped() {
        let root = tmp("leftover");
        let w1 = PartName {
            year: 2026,
            month: 11,
            span: Span::Week(1),
        };
        make_part(&root, w1, &[(3, 1, 2, "final")], &[]);
        make_part(
            &root,
            PartName::day(day("2026-11-03")),
            &[(3, 0, 1, "old")],
            &[],
        );
        make_part(&root, PartName::day(day("2026-11-09")), &[], &[]);
        compact(&root, day("2026-11-09")).unwrap();
        assert_eq!(files(&root), ["2026-11-w1.db", "2026-11-09.db"]);
        assert_eq!(read(&w1.path(&root)).0, [(3, 1, 2, "final".into())]);
        let _ = fs::remove_dir_all(&root);
    }

    /// Catch-up replays only parts that end after `after`, oldest first.
    #[test]
    fn apply_after_replays_newer_parts() {
        let root = tmp("apply");
        make_part(&root, PartName::day(day("2026-11-01")), &[], &[(5, 1, 2)]);
        make_part(
            &root,
            PartName::day(day("2026-11-02")),
            &[(6, 1, 1, "n")],
            &[(5, 0, 1)],
        );
        assert_eq!(latest_day(&root).unwrap().as_deref(), Some("2026-11-02"));

        let st = crate::state::open(&root.join("state.db")).unwrap();
        st.execute_batch("INSERT INTO galleries (id, kind, status) VALUES (5, 1, 2)")
            .unwrap();
        assert_eq!(apply_after(&st, &root, Some("2026-11-01")).unwrap(), 1);
        let rows: Vec<(i64, i64, i64)> = st
            .prepare("SELECT id, kind, status FROM galleries ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows, [(5, 0, 1), (6, 1, 1)]);
        drop(st);
        let _ = fs::remove_dir_all(&root);
    }
}
