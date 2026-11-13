# hitomi-dataset

hitomi.la gallery metadata, verified daily, published as SQLite files.

## Layout

```
manifest.json                 base snapshot record + every part in apply order
base.json                     base snapshot record (written by publish-base)
data/hitomi/2026/2026-10.db   finished month
data/hitomi/2026/2026-11-w1.db  finished week of the current month (days 1–7)
data/hitomi/2026/2026-11-08.db  single day
```

A finished year holds exactly `YYYY-01.db` … `YYYY-12.db`. Days 29–31 stay
daily files until the month is over.

Releases:

| tag | asset | content |
|---|---|---|
| `state` | `state.db.zst` | full working DB (adds `checked_at`); input/output of each daily run |
| `base` | `base-YYYY-MM-DD.db.zst` | client snapshot as of `base.json.through` (name in `base.json.asset`) |

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
2. Apply, in `manifest.json` order, every part whose `last` is after `base.through`
   (then after the last part you applied): upsert `galleries`, then
   `UPDATE … SET kind, status` from `states`. Reapplying a part is harmless, so
   a merged week/month file may overlap days already applied.

## Daily run (`.github/workflows/daily.yml`, 20:00 UTC)

Published parts are never rewritten: a run whose date is not after the last
published day fails; the workflow skips (as a success) a day already published.
If an earlier run pushed its part but failed to upload the state, the next run
first replays the newer parts onto the state.

1. nozomi listing → set `kind`; refuse a listing < 90% of the Normal count.
2. Fetch galleries listed but missing (failures are retried next run).
3. Verify: never-checked Forbidden galleries first (80,000/run), then everything
   by oldest check (45,000/run ≈ 30-day cycle). Undecided ones keep their status
   and come back in rotation. Fails if over half are undecided.
4. Write the day's part, compact weeks/months, rewrite `manifest.json`.
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
3. Run the `daily` workflow once by hand (Actions → daily → Run workflow);
   the scheduled run later that UTC day is skipped.
4. When the never-checked Forbidden backlog is done (a few runs), run `publish-base`.
