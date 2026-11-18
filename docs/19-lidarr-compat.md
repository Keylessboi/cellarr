# 19 — Replacing Lidarr

cellarr can stand in for a Lidarr install: it serves Lidarr's API (`/api/v1`), imports Lidarr's
database, and runs the same acquisition loop — search, grab, track, import — for music. This
doc is the operating manual for that, and the maintenance manual for **keeping it compatible as
Lidarr changes**.

## What "drop-in" means here

| | |
|---|---|
| **Clients keep working** | Prowlarr (indexer sync), Octo, Hermes, maintenance scripts, dashboards: anything that talks to Lidarr's HTTP API at its old URL and key. |
| **Ids are preserved** | Artists, albums, tracks, track files and quality-profile ids carry across a migration unchanged, so a cursor, a stored profile id or a script's config still means the same thing. |
| **Files are untouched** | The importer recognizes your files where they are. Nothing is moved, renamed or re-tagged. |
| **Lidarr can keep running** | The source database is opened read-only; the migration is repeatable into a fresh cellarr database. |
| **Not covered** | Renaming/retagging existing files, metadata sidecar writers, music import lists, unmapped track files. Lidarr's native plugins (Tubifarry, etc.) run through an optional sidecar Lidarr ([20-plugins.md](20-plugins.md)); manual import of a download folder is served (`/manualimport`). The exact endpoint-level list is `crates/cellarr-api/tests/contracts/lidarr-gaps.json`. |

## Running it

```toml
# cellarr.toml (or CELLARR_MUSIC__LIDARR_PORT=8686 in the environment)
[music]
lidarr_port = 8686            # serve Lidarr's API at /api/v1 on this port
# lidarr_bind = "0.0.0.0"     # defaults to the main API's bind address
# musicbrainz_url = "https://mb.example/ws/2"   # a mirror; defaults to musicbrainz.org
```

With `lidarr_port` set, a second listener serves exactly what Lidarr serves: `/ping` and
`/api/v1/*`, with `X-Api-Key` and `X-Application-Version` as Lidarr sends them. Point your
clients' Lidarr URL at cellarr and nothing else changes. The same face is always available at
`/lidarr/api/v1` on the main port.

The API key is cellarr's (`api.api_key`). To keep existing clients' stored key working, set
cellarr's key to Lidarr's.

## Migrating

```sh
# 1. Take a consistent copy. Lidarr can keep running: this is one read transaction on a
#    read-only connection (VACUUM INTO), with no write lock and the source untouched.
cellarr snapshot /path/to/lidarr.db /tmp/lidarr-snapshot.db
# 2. Migrate the copy.
cellarr migrate /tmp/lidarr-snapshot.db
# imported music: 914 artists, 41459 albums, 953583 tracks, 35798 track files recognized in place …
```

Copying the live file by hand while Lidarr writes can give a torn database, and migrating
the live file directly reads it over minutes, so an import in between could leave an album
without its tracks. Use `snapshot`.

What is carried: artists, albums (with their monitored flags and selected release), the tracks of
each album's selected release, track files and their quality and media info, quality profiles
(groups flattened, cutoff resolved), metadata profiles, root folders, indexers, download clients,
remote path mappings, the blocklist (matched by release name, so a dead release stays dead),
every pressing of each album (with Lidarr's release ids, and a pinned pressing stays pinned),
and the **history**, row for row with Lidarr's ids, event types, download ids and data.
Download clients keep their real category (`musicCategory`), and plugin-provided clients are
carried as sidecar clients ([20-plugins.md](20-plugins.md)).

What is not: tags, import lists, notifications.

The migration is idempotent per database: to redo it, discard cellarr's database and run it again.

## Shadow mode

`CELLARR_SHADOW=true` runs cellarr as a read-only rehearsal beside a live Lidarr. It registers
no scheduled jobs, so there is no RSS sync, search, download-client polling, import or rename.
It also refuses every non-GET request with 403, on both listeners. A configured plugin sidecar
is only read for its catalog: a shadow never mirrors indexers, clients or artists into it, and an
indexer or client that needs it fails rather than writing there. Point nothing at it except
`scripts/lidarr-shadow-diff`:

```sh
scripts/lidarr-shadow-diff http://lidarr:8686 http://cellarr:8686 [--sample N] [--no-albums]
```

The script compares the two by id, with GET requests only: profiles, root folders, indexers,
download clients and their categories, artists, albums and pressings, track files, wanted and
queue. It exits 1 on any difference. `--no-albums` skips the full album list, Lidarr's heaviest
call. Mount the library read-only for a shadow; cellarr does not need write access to compare.

## Cutting over

A safe order, because nothing here is destructive until the last step:

1. **Run cellarr beside Lidarr** on its own port, with the same media and download mounts
   (same container paths as Lidarr sees: `/data/...`), a fresh data directory, and
   `CELLARR_API__API_KEY` set to Lidarr's key. Add a music download client category equal to
   Lidarr's (`musicCategory`), or let the migration carry it.
2. **Migrate** a copy of `lidarr.db`: `cellarr migrate lidarr.db`. Check the counts it prints
   against Lidarr's own (artists, albums, tracks, files).
3. **Compare** `GET /api/v1/artist`, `/wanted/missing` and `/queue` on both. They are the same
   shapes; the totals should match.
4. **Switch the clients**: repoint Prowlarr's Lidarr application, Octo, scripts and probes at
   cellarr (or give cellarr Lidarr's service name and port: `music.lidarr_port = 8686`).
   Prowlarr re-syncs the indexers.
5. **Stop Lidarr.** Keep its database and `MediaCover` for a while; nothing in cellarr
   modifies them or your music files.

Things to know:
- **In-flight downloads.** Downloads Lidarr had in flight when you switched are tracked by the
  download client's category, so cellarr sees them in the client but not in its queue. Stop
  Lidarr with the queue empty, or import the stragglers with Manual Import
  (`GET /manualimport?folder=` then `POST /command ManualImport`, as Lidarr does).
- **Torrents are never removed after import.** Seeding rules and tools that reap hardlinked
  seeds rely on that. Only Usenet downloads are cleaned up. A stalled or failed download is
  still removed when it is blocklisted.

## What it costs

See [Resource use](#resource-use) below.

## Keeping up with Lidarr

The risk in re-implementing an API is drift: Lidarr ships a new field or endpoint, a client
starts relying on it, and the replacement silently lacks it. The design answers that structurally.

### The shape of the code

```
crates/cellarr-api/src/lidarr/
  mod.rs        the router: every path, and which module serves it
  resources.rs  the ONLY place Lidarr's field names appear (cellarr record -> JSON)
  quality.rs    the ONLY place Lidarr's quality numbering is adapted (table: cellarr-core)
  handlers.rs   artists, albums, tracks, files, profiles: route -> data layer
  activity.rs   queue, history, wanted, calendar, blocklist, parse
  command.rs    integer-id commands over scheduler jobs
  release.rs    interactive search and grab
```

A handler contains no business logic: it reads or writes `cellarr_db::MusicRepo`, the grab
queue or the scheduler. Everything the *arrs share (indexers, download clients, tags, root
folders, notifications, system) is **mounted from the shim's handlers**, not re-implemented,
so a fix there fixes every face. Lidarr's behaviour lives in cellarr's own crates (parser,
decision engine, importer), which are independent of Lidarr's code.

### The contract

`crates/cellarr-api/tests/contracts/lidarr-v1.json` is the pinned contract: every path and method
Lidarr serves, and every field (with JSON type) of the resources clients read. It is *generated*
from Lidarr's published OpenAPI document and records interface facts only.

`crates/cellarr-api/tests/lidarr_contract.rs` holds cellarr to it:

- every Lidarr endpoint must be served, **or** listed in `tests/contracts/lidarr-gaps.json`
  with a reason;
- every field Lidarr sends on a resource must be present with a compatible type, or be a listed gap;
- a gap that has since been implemented, or that names something Lidarr removed, **also fails**,
  so the gaps file stays the true list of what is missing.

### When Lidarr releases

```sh
scripts/lidarr-contract --check             # is the pinned contract still current?
scripts/lidarr-contract                     # regenerate it from Lidarr's develop branch
scripts/lidarr-contract --ref v3.2.0.5000   # ...or a tag
cargo test -p cellarr-api --test lidarr_contract
```

The failing test is the work list, one line per change:

| The test says | It means | You |
|---|---|---|
| `Lidarr serves these and cellarr does not: GET /api/v1/foo` | a new endpoint | add the route in `mod.rs` and a handler, or add a gap with the reason |
| `ArtistResource.newField: missing (Lidarr sends a string)` | a new field | add it in `resources.rs` (the one place), or add a gap |
| `AlbumResource.x: Lidarr sends integer, cellarr sent "…"` | a changed type | fix the builder in `resources.rs` |
| `listed as gaps but cellarr now serves them` | you implemented one | delete the gap line |

Because resources are built in one file and routes listed in one file, each fix is local.

Two things the contract cannot see, which are checked elsewhere:

- **Quality ids.** Lidarr's quality numbering lives in its source, not its OpenAPI document.
  `cellarr_core::music::LIDARR_QUALITY_IDS` is checked against cellarr's quality catalogue by
  unit tests, and the migration reads real databases through it. When Lidarr adds a quality,
  add one row there and one to the catalogue (`MUSIC_QUALITY_NAMES`).
- **Database schema.** The importer reads Lidarr's own tables. `crates/cellarr-migrate/tests/lidarr.rs`
  builds a database with Lidarr's column names and embedded-JSON shapes; if Lidarr renames a
  column, update `lidarr.rs` and that fixture together. JSON is read tolerantly (key case
  ignored; an id may be a number or an object) because Lidarr has stored both.

### Clean-room

Nothing here is Lidarr's code. The contract is names and types from its public API description;
the quality and album-type ids are interface constants; the column names are the persisted
model. Behaviour (parsing, matching, deciding, importing) is cellarr's, learned from the
documented behaviour and tested against fixtures — see
[agents/legal-and-licensing.md](agents/legal-and-licensing.md).

## How music works in cellarr

- **Model.** `artist → album → track` are ordinary content nodes. An album is a MusicBrainz
  *release group*; its tracks come from one representative pressing (`pick_release`: the track
  count most pressings share, the fewest discs, the earliest date). Each record has a small
  sequential integer id as well as its content id; a music node's content id *embeds* its
  integer id (`seq_uuid`), so the API's integers and cellarr's UUIDs never disagree.
- **The unit is the album.** It is what is searched, matched, graded and grabbed
  (`Coordinates::Album`); its tracks matter at import and naming. Missing means "no file under
  the album", and an unmonitored artist silences all its albums.
- **Parsing.** `cellarr_parse::parse_music_title` reads `Artist - Album (Year) [FLAC]` and the
  scene `Artist-Album-WEB-FLAC-2019-GRP` layouts; it never invents a quality the title does
  not state. A bare `MP3` stays unknown until the files themselves are read.
- **Quality.** Music qualities share the catalogue, in their own rank range (`MUSIC_RANK_BASE`).
  Each face lists only its own. A file's quality comes from its title, else from its own
  stream properties (`cellarr_fs::audio`, pure Rust, no ffmpeg).
- **Import.** Each audio file is placed by its tags' disc/track number (file name as fallback)
  onto the album's tracks; a folder whose tags name a different album is refused; files that
  match no track are left behind. An upgrade (MP3 → FLAC) removes the superseded file only
  after the new one is recorded.
- **Metadata.** MusicBrainz, directly, no key, one request a second (enforced). Albums are
  filtered by the artist's *metadata profile* (Album/EP/Single/…, Studio/Live/…). Track lists
  are fetched lazily and by a slow background job (`FillTracks`), so a large library costs
  nothing until it is used. A migrated library arrives with its tracks already in place.

## Resource use

*Measured, not estimated.* The library is synthetic but sized to one real install (914 artists,
41,459 albums, ~950k tracks across two releases each, 35,798 files, 16k blocklist rows, 934k
history rows), built by `scripts/synth-lidarr-db`; the binary is the release build, run on a
4-core container. Nothing was mocked in the daemon: this is `cellarr migrate` and then the
real server answering the same calls Lidarr's clients make.

| | cellarr | Lidarr (the same install, from its own deployment notes) |
|---|---|---|
| Import of the library | **278 s**, peak **119 MiB** (history included) | n/a |
| Database on disk | **505 MB** (934k history rows carried over) | 3.9 GB (934k history rows) |
| Memory, just started | **~140 MiB** | ~900 MiB "fresh pod" |
| Memory, after a mixed workload | **~410 MiB** (**~240 MiB** with `MALLOC_ARENA_MAX=2`) | 1.5–1.7 GiB (reached 1.7 GiB and wedged; limit raised to 3 GiB) |
| Memory, empty library | **29 MiB** | n/a |
| Runtime | one 35 MB native binary; no .NET runtime | a .NET application |

Request latency on that library, best of three (the SQLite file on local disk):

| Call | Time |
|---|---|
| `GET /album?artistId=` (one artist) | 20 ms |
| `GET /track?albumId=` | 15 ms |
| `GET /queue`, `GET /history?albumId=&eventType=8` | 15–25 ms |
| `GET /wanted/missing` (page of 50 of 38,195) | ~0.5 s |
| `GET /blocklist` (page of 50 of 16,271) | ~0.45 s |
| `GET /artist` (all 914, with statistics) | ~0.45 s |
| `POST /command AlbumSearch` × 50 albums | ~0.5 s, and it returns; the search runs as jobs |
| `GET /album` (all 41,459 albums, 87 MB of JSON) | ~3.8 s |
| `GET /history/artist` (934k rows, 299 MB of JSON) | ~36 s, peak **125 MiB**: streamed in pages of 2,000 via a keyset cursor |

Lidarr's own note on this library: `GET /api/v1/artist` exceeded 180 s on network storage and
`/ping` took ~5 s on local disk, which is what its probes and the mass-search script had to work
around. cellarr's equivalents are 0.45 s and 1 ms.

Caveats, so the table is read correctly: Lidarr's figures are what its operator wrote down
about a long-running install under a multi-day mass search, not a controlled run, and this
benchmark has no indexers or download client, so it measures the library and the API, not
the cost of searching. What is certain is the shape: memory and latency here do not grow with
history (the whole-history calls stream a page at a time), nothing is a .NET runtime, and the
unfiltered calls are streamed rather than built in memory.

### Knobs

| Setting | Effect |
|---|---|
| `CELLARR_DB_CACHE_MIB` (default 16) | SQLite page cache per connection (up to 9 connections). Raise it only when the database file is on slow network storage. |
| `MALLOC_ARENA_MAX=2` | On glibc builds, stops the allocator keeping a heap arena per core; cut steady-state memory by ~40% in the benchmark. |
| `music.lidarr_port` | Serve the Lidarr face on its own listener. |

## Parity against a real Lidarr

`scripts/lidarr-compare` replays `scripts/lidarr-compare-scenario.json` against a real
Lidarr and a cellarr, then diffs status codes and JSON key/type shapes. Latest run: 41 of 56
requests identical. The rest are documented differences:

- **Extra fields in cellarr.** Several resources carry more fields than Lidarr (for example
  `cleanTitle`, `added`, `disambiguation`). Clients ignore unknown keys.
- **Lookup results.** Lidarr's lookup comes from its own metadata server and includes
  images, links, genres and overviews. cellarr's comes from MusicBrainz and has none of them.
- **Command list and bodies.** Lidarr pre-populates housekeeping commands and echoes
  extra body fields; cellarr's command log starts empty.
- **Unknown command.** Lidarr answers 500, cellarr answers 400.
- **`/parse`.** Embeds the fuller album/artist resources, so it shows the same extras.
- **System package fields.** `packageVersion`, `packageAuthor` and
  `packageUpdateMechanismMessage` are not served (see `lidarr-gaps.json`).
- **Unmapped track files.** `GET /trackfile?unmapped=true` always answers `[]`. Lidarr
  lists audio files it found under a root folder and could not link to a track (`albumId`
  0). cellarr keeps no such record: its library rescan skips music libraries, the scan only
  looks at video files, and every track file it stores is attached to a track. Answering
  honestly needs a scan of the music roots for untracked audio, which is not built; until
  then a client sees an empty list, never a made-up one. Loose files can still be found and
  placed through manual import (`GET /manualimport?folder=`).

Download clients use Lidarr's `musicCategory` field name on this face and the shared
`category` column internally. The indexer and download-client schemas are recorded from a
real Lidarr (`src/lidarr/templates/`).

## How it is tested

| Layer | Where | Runs in | What it proves |
|---|---|---|---|
| Unit and face tests | `cellarr-*/src`, `lidarr_face.rs` | `just test` | Handlers, resources, importer logic |
| Contract | `lidarr_contract.rs` | `just test` | Every Lidarr path and field is served or a documented gap |
| Whole daemon | `cellarr-cli/tests/lidarr_e2e.rs` | `just test` | A real daemon, driven only through the Lidarr API against local stubs (metadata, Torznab, qBittorrent): search, grab, download, import, history, queue, and a mis-tagged download that must not reach the library |
| Offline replay | `lidarr_replay.rs` | `just test` | cellarr against the status and shape a real Lidarr gave for the 127-request scenario. Known differences are a baseline that may only shrink |
| Live comparison | `just lidarr-compare` | by hand / before release | The same scenario against a running real Lidarr |

Refreshing for a new Lidarr release: run `just lidarr-compare --record DIR` against it,
regenerate the shapes under `tests/contracts/real-lidarr/`, and rerun the replay test with
`UPDATE_BASELINE=1`. The diff of the baseline is exactly what moved.

Not covered yet: a real Prowlarr pointed at cellarr, and a migration round trip from a
Lidarr database the scenario produced.
