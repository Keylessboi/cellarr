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
| **Not covered** | Lidarr's own plugin system (Tubifarry, etc.), manual import of loose files, renaming/retagging existing files, metadata sidecar writers, music import lists. The exact endpoint-level list is `crates/cellarr-api/tests/contracts/lidarr-gaps.json`. |

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
# 1. Stop (or leave running) Lidarr; copy its database somewhere cellarr can read.
cellarr migrate /path/to/lidarr.db
# imported music: 914 artists, 41459 albums, 953583 tracks, 35798 track files recognized in place …
```

What is carried: artists, albums (with their monitored flags and selected release), the tracks of
each album's selected release, track files and their quality and media info, quality profiles
(groups flattened, cutoff resolved), metadata profiles, root folders, indexers, download clients,
remote path mappings, and the blocklist (matched by release name, so a dead release stays dead).

What is not: **history** (cellarr starts a fresh one; Lidarr's is mostly search noise, ~934k
rows in the reference install), tags, import lists, notifications.

The migration is idempotent per database: to redo it, discard cellarr's database and run it again.

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

Two things to know. Downloads Lidarr had in flight when you switched are tracked by the
download client's category, so cellarr will see them in the client but they are not in its
queue; remove and re-search, or let them finish and import once via a rescan. And the
**force-import step of a maintenance script** (`GET /manualimport`) has no equivalent yet: a
completed download that cellarr cannot place is held with its reason in the decision log
(`GET /api/v1/queue` shows it) rather than force-imported.

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

*Measured, not estimated — see the numbers below, produced by `scripts/synth-lidarr-db` (a
database at the size of one real install: 914 artists, 41,459 albums, ~950k tracks, 35,798
files) and the release binary.*

RESOURCE_NUMBERS_PLACEHOLDER
