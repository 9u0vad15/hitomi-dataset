# hitomi-dataset

hitomi.la gallery metadata, updated every 4 hours, published as SQLite files.

## Layout

```
manifest.json                 base snapshot record + every part in apply order
base.json                     base snapshot record (written by publish-base)
data/hitomi/2026/2026-10.db   finished month
data/hitomi/2026/2026-11-w1.db  finished week of the current month (days 1–7)
data/hitomi/2026/2026-11-08.db  finished day
data/hitomi/2026/2026-11-09-h13.db  one run of the current day (started 13:xx KST)
```

A finished year holds exactly `YYYY-01.db` … `YYYY-12.db`. Days 29–31 stay
daily files until the month is over.

Releases:

| tag | asset | content |
|---|---|---|
| `state` | `state.db.zst` | full working DB (adds `checked_at`); input/output of each run |
| `base` | `base-YYYY-MM-DDThh.db.zst` | client snapshot taken on `base.json.through` (name in `base.json.asset`); republished monthly |

## Data

`galleries` columns: `id, kind, status, title, gallery_type, language,
artists, groups_, series, characters, tags` (multi-value fields as JSON arrays).

- `kind`: `0` Normal = listed in hitomi's `index-all.nozomi`, `1` Forbidden = not listed.
- `status`: `0` unchecked, `1` alive, `2` dead.
  Alive = `galleries/{id}.js` and the first image's thumbnail are served.
  Dead = `.js` 404, no files, or thumbnail 404 (the original images were 404
  in every sampled case). Forbidden/`blocked` galleries are usually still alive.

Each part has `galleries` (full rows: new galleries) and `states`
(`id, kind, status` changes of known galleries); an ID is in at most one.

### Applying (client)

1. Download the `base.json.asset` file from the `base` release, check `sha256`/`size` from `base.json` after decompressing.
2. Apply, in `manifest.json` order, every part whose `last` is on or after
   `base.through` (then on or after the `last` of the last part you applied):
   upsert `galleries`, then `UPDATE … SET kind, status` from `states`.
   The last day is applied again because it may have gained runs; reapplying a
   part is harmless, so a merged day/week/month file may overlap parts already
   applied.

## Run (`.github/workflows/daily.yml`, every 4 hours; GitHub may start it hours late)

A run is named by the KST date and hour it starts at. Published parts are never
rewritten: a run whose hour is not after the last published run fails; the
workflow skips (as a success) an hour already published.
If an earlier run pushed its part but failed to upload the state, the next run
first replays the newer parts onto the state.

1. nozomi listing → set `kind`; refuse a listing < 90% of the Normal count.
2. Fetch galleries listed but missing (failures are retried next run).
3. Verify 10,000 galleries: never-checked Forbidden ones first, then everything
   by oldest check (≈ 23-day cycle). Undecided ones keep their status
   and come back in rotation. Fails if over half are undecided.
4. Write the run's part, compact days/weeks/months, rewrite `manifest.json`.
5. Commit parts, then upload the state.

## Setup

1. Push this repository first (a release needs at least one commit).
2. Seed the state from an existing app DB, compress it, and attach it to a
   release tagged `state` (Releases → Draft a new release; assets up to 2 GB,
   unlike the 25 MB limit for repository files):
   ```
   cargo run --release -- seed --from <hitomi_galleries.db> --state work/state.db
   zstd -10 work/state.db -o work/state.db.zst
   ```
   Without the zstd CLI, Python 3.14+ writes the same format:
   `python -c "import compression.zstd as z,shutil;shutil.copyfileobj(open('work/state.db','rb'),z.open('work/state.db.zst','wb',level=10))"`
3. Run the `daily` workflow once by hand (Actions → daily → Run workflow).
   Part dates and hours are KST.
4. When the never-checked Forbidden backlog is done, run `publish-base` once;
   after that it runs on the 1st of each month.
