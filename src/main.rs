//! Builds the hitomi gallery dataset published in this repository.
//! See README.md for the data layout and the daily workflow.

mod hitomi;
mod parts;
mod state;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use futures_util::{stream, StreamExt};
use sha2::{Digest, Sha256};

use hitomi::Verdict;
use state::Day;

#[derive(Parser)]
#[command(about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the state DB from an Aurelia `hitomi_galleries.db` (all unchecked).
    Seed {
        #[arg(long)]
        from: PathBuf,
        #[arg(long)]
        state: PathBuf,
    },
    /// Sync with nozomi, fetch new galleries, verify a batch, write the day's
    /// part, compact parts and rewrite manifest.json.
    Daily {
        #[arg(long)]
        state: PathBuf,
        /// Data repository root (holds data/, manifest.json, base.json).
        #[arg(long)]
        repo: PathBuf,
        /// Run date, YYYY-MM-DD (UTC).
        #[arg(long)]
        date: Day,
        /// Requests in flight.
        #[arg(long, default_value_t = 16)]
        concurrency: usize,
        /// Galleries verified per run while never-checked Forbidden ones remain
        /// (~127/s on a GitHub runner at concurrency 16: ~11 min).
        #[arg(long, default_value_t = 80_000)]
        suspect_budget: usize,
        /// Galleries re-verified per run afterwards (~1.4M over 30 days).
        #[arg(long, default_value_t = 45_000)]
        rotate_budget: usize,
    },
    /// Write the client base snapshot and record it in base.json + manifest.json.
    ExportBase {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        repo: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Seed { from, state } => {
            let n = state::seed(&state::open(&state)?, &from)?;
            eprintln!("seeded {n} galleries");
            Ok(())
        }
        Cmd::Daily {
            state,
            repo,
            date,
            concurrency,
            suspect_budget,
            rotate_budget,
        } => {
            daily(
                &state,
                &repo,
                date,
                concurrency,
                suspect_budget,
                rotate_budget,
            )
            .await
        }
        Cmd::ExportBase { state, out, repo } => export_base(&state, &out, &repo),
    }
}

async fn daily(
    state_path: &Path,
    repo: &Path,
    date: Day,
    concurrency: usize,
    suspect_budget: usize,
    rotate_budget: usize,
) -> Result<()> {
    let mut conn = state::open(state_path)?;
    if !state::has_galleries(&conn)? {
        bail!(
            "{} has no galleries: wrong path, or run `seed` first",
            state_path.display()
        );
    }
    // Published parts are never rewritten, so each day runs once.
    let parts_root = repo.join(parts::PARTS_DIR);
    let latest = parts::latest_day(&parts_root)?;
    if latest.as_deref() >= Some(date.to_string().as_str()) {
        bail!(
            "{date} is not after the last published day {}",
            latest.unwrap_or_default()
        );
    }
    // A run whose parts were pushed but whose state upload failed: replay them.
    let last_run = state::meta(&conn, "last_run")?;
    if latest > last_run {
        let n = parts::apply_after(&conn, &parts_root, last_run.as_deref())?;
        state::set_meta(&conn, "last_run", latest.as_deref().unwrap_or_default())?;
        eprintln!("state caught up with {n} published parts");
    }

    let http = &hitomi::client()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

    let nozomi: Vec<i64> = hitomi::nozomi_ids(http).await?.into_iter().collect();
    let new_ids = state::sync_listing(&mut conn, &nozomi)?;
    drop(nozomi);

    // New galleries; a failed fetch stays missing and is retried next run.
    let total = new_ids.len();
    let mut fetched = stream::iter(new_ids)
        .map(|id| hitomi::fetch_gallery(http, id))
        .buffer_unordered(concurrency);
    let mut galleries = Vec::with_capacity(total);
    while let Some(g) = fetched.next().await {
        galleries.extend(g);
    }
    state::insert_new(&mut conn, &galleries, now)?;
    eprintln!("new: {}/{total} fetched", galleries.len());

    let batch = state::verify_batch(&conn, suspect_budget, rotate_budget)?;
    let total = batch.len();
    let mut checks = stream::iter(batch)
        .map(|id| async move { (id, hitomi::verify(http, id).await) })
        .buffer_unordered(concurrency);
    let mut verdicts = Vec::with_capacity(total);
    while let Some(v) = checks.next().await {
        verdicts.push(v);
        if verdicts.len() % 5000 == 0 {
            eprintln!("verify: {}/{total}", verdicts.len());
        }
    }
    let unknown = verdicts
        .iter()
        .filter(|(_, v)| *v == Verdict::Unknown)
        .count();
    if total >= 100 && unknown * 2 > total {
        bail!("{unknown}/{total} verifications failed: is the CDN blocking this runner?");
    }
    let changed = state::apply_verdicts(&mut conn, &verdicts, now)?;
    eprintln!("verify: {total} checked, {changed} status changes, {unknown} undecided");

    state::set_meta(&conn, "last_run", &date.to_string())?;
    let part = parts::write_day(&conn, &parts_root, date)?;
    eprintln!("wrote {}", part.display());
    parts::compact(&parts_root, date)?;
    parts::write_manifest(repo)
}

fn export_base(state_path: &Path, out: &Path, repo: &Path) -> Result<()> {
    let conn = state::open(state_path)?;
    let through = state::meta(&conn, "last_run")?.context("no daily run recorded yet")?;
    state::export_base(&conn, out)?;
    let mut hasher = Sha256::new();
    let size = std::io::copy(&mut std::fs::File::open(out)?, &mut hasher)?;
    let sha256: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    // Versioned name: replacing the asset never mismatches a live base.json.
    let base = serde_json::json!({
        "release": "base",
        "asset": format!("base-{through}.db.zst"),
        "through": through,
        "size": size,
        "sha256": sha256,
    });
    std::fs::write(
        repo.join("base.json"),
        serde_json::to_string_pretty(&base)? + "\n",
    )?;
    parts::write_manifest(repo)
}
