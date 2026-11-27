# Plugins

cellarr takes two kinds of plugins. Both appear to *arr clients exactly like built-in
implementations: they are listed in `/indexer/schema` and `/downloadclient/schema` with their
own fields, configured through the same API, and used by the pipeline like any other indexer or
download client.

| | WASM plugins | Native Lidarr plugins |
|---|---|---|
| What | Indexers and download clients compiled to WASM against `crates/cellarr-plugins/wit/plugins.wit` | Lidarr's own .NET plugins (`Lidarr.Plugin.*.dll`), unmodified |
| Runs in | cellarr's process, sandboxed | A stock Lidarr run beside cellarr (the sidecar) |
| Cost | A few MB per plugin, only while called | A whole Lidarr: about 300 MB resident when idle (measured, Lidarr 3.1, empty library) |
| Required | No (feature `plugins`, on by default; `--no-default-features` drops it) | No (off until configured) |

## WASM plugins

Drop a plugin into `<data_dir>/plugins` (or `plugins.dir`):

```text
plugins/
  my-indexer.wasm
  my-client/plugin.wasm
  my-client/plugin.json      {"allowHosts": ["https://api.example"], "fuel": 2000000000, "memoryMib": 64}
```

Each plugin exports `plugin` (its name, version, protocol and settings) and either `indexer` or
`download-client`. It imports one capability, `http`, which only reaches the hosts the plugin's own
settings name (an indexer's URL, a client's host and port) plus any `allowHosts` in its manifest.
There is no filesystem, no environment and no sockets. Each call runs in a fresh instance with a
fuel budget, a 60-second wall-clock limit and a memory cap, so a misbehaving plugin cannot hang or
bloat the daemon.

Writing one is ordinary Rust: a `cdylib` crate using `wit-bindgen`, built with
`cargo build --release --target wasm32-unknown-unknown`. The host turns that core module into a
component at load time. `examples/plugins/` has two working plugins, a Torznab indexer and a
qBittorrent client (`just plugin-examples` rebuilds them). Other languages work too, as long as
they produce a component for the same WIT world.

`GET /api/v1/plugin` lists what loaded; a plugin that failed to load is logged and skipped.

## Native Lidarr plugins (the sidecar)

Lidarr plugins are built against Lidarr's internals, so the only faithful host for them is Lidarr.
The sidecar is a stock Lidarr (3.x loads plugins from `<its data>/plugins/<owner>/<name>/`) run
headless beside cellarr. cellarr never links Lidarr code: it talks to the sidecar over Lidarr's
public HTTP API.

```sh
CELLARR_PLUGINS__DOTNET_SIDECAR_URL=http://127.0.0.1:8687
CELLARR_PLUGINS__DOTNET_SIDECAR_KEY=<the sidecar's API key>
# optional: a folder both can see, used as the sidecar's staging root
CELLARR_PLUGINS__DOTNET_SIDECAR_ROOT=/data/sidecar-staging
```

How it works:

- **Discovery.** Implementations the sidecar offers beyond Lidarr's built-ins came from plugins;
  cellarr lists them in its schemas, fields included. (`dotnet_sidecar_expose_builtins = true`
  also offers the sidecar's built-ins as `<Name>ViaLidarr`, which is how the bridge is tested.)
- **Configuration.** An indexer or client configured in cellarr with one of those implementations
  is mirrored into the sidecar as `cellarr:<id>`, its settings kept in step.
- **Search.** The sidecar searches by album, so cellarr mirrors the album: it adds the artist to
  the sidecar unmonitored (it never searches or downloads on its own) and asks for that album's
  releases, keeping the ones the mirrored indexer found.
- **Download.** A release the sidecar found is grabbed from its own cache. One cellarr found is
  pushed, with the album monitored only for the push. Progress comes from the sidecar's queue.
- **Import.** The sidecar imports finished downloads into its own root folder, a staging area,
  never your library. cellarr then imports from there with its own matching and safety rules, and
  removing the download deletes the staged copy. A download the sidecar will not import (it
  refuses, or retries forever) is handed to cellarr straight from the download folder.
- **Install.** `POST /api/v1/command {"name": "InstallPlugin", "githubUrl": ...}` installs a
  plugin into the sidecar, which restarts itself to load it; `GET /api/v1/system/plugins` lists
  them. A Lidarr UI or script that manages plugins works unchanged.

Limits: RSS sync uses cellarr's own indexers, not the sidecar's; the sidecar needs network access
to Lidarr's metadata service to add artists; and a plugin whose download client produces files in
its own protocol works only through the sidecar's own client.

### Licensing

The sidecar is an unmodified Lidarr the user installs; cellarr only calls its documented HTTP API
and ships no Lidarr code or binaries. This keeps cellarr's licence independent of Lidarr's
(GPLv3). Distributing a bundle that ships Lidarr alongside cellarr would be a separate decision
for a human to make.

## Native Soulseek (slskd)

Soulseek no longer needs the sidecar. The implementation name `Slskd` (what Tubifarry
registers, and what a migrated Lidarr database carries) is built natively: cellarr talks to
slskd's HTTP API itself, as both the indexer and the download client.

A row builds natively when its settings have `baseUrl` and `apiKey` and do not say
`"native": false`. Any other row (and a row on a build without the native kind) takes the
sidecar plugin path described above, unchanged.

| Setting | Side | Default | Meaning |
|---|---|---|---|
| `baseUrl`, `apiKey` | both | required | slskd's address and an API key (`X-API-Key`). A non-admin key is enough |
| `downloadRoot` | client | `/data/torrents/soulseek` | where slskd's completed folders are, as cellarr sees them |
| `searchTimeoutSecs` | indexer | 15 | how long a Soulseek search runs |
| `minimumFiles` | indexer | 3 | folders with fewer audio files are not albums |
| `maxReleases` | indexer | 10 | releases kept per search |
| `blockedUsers` | indexer | none | comma list (or array) of users never downloaded from |
| `maximumQueueLength` | indexer | 0 (off) | skip peers whose upload queue is longer |
| `pollIntervalMs` | indexer | 1000 | search poll interval |

How it works (`crates/cellarr-download/src/slskd.rs`):

- **Search.** `POST /api/v0/searches` with `artist album` (and once more with the album's
  bracketed edition suffix removed if the first finds nothing), poll `GET /searches/{id}` until
  complete, read `/responses`, delete the search. Files are grouped by (user, folder) and a
  folder is kept when its last two path parts contain the album title, the artist is in the path
  or in half the file names, and it has `minimumFiles` unlocked audio files of one format
  (majority extension, FLAC on a tie). Order: lossless, free upload slot, shorter queue, more
  files, faster peer, then name. Each folder is a release titled `Artist - Album [FLAC]` /
  `[MP3 320]`, protocol torrent, size = sum of files, no seeders.
- **Grab.** The release's `download_url` is `slskd:<base64url json>` naming the user, folder and
  exact files, so nothing is remembered between search and grab. The client enqueues them with
  `POST /transfers/downloads/{user}`. The download id carries the same target, so status survives
  a restart.
- **Routing.** A `slskd:` release is routed like a plugin protocol (`SoulseekDownloadProtocol`):
  only a Soulseek client takes it, and a Soulseek client never takes an ordinary release.
- **Status.** `GET /transfers/downloads`, restricted to this grab's files. Queued/Downloading by
  state, progress from bytes; any file that ended not-succeeded fails the whole download (with
  the state and exception in the message, so it is blocklisted and re-searched); all succeeded is
  Completed.
- **Where the files are.** slskd's non-admin key cannot read `/options`, so nothing is read from
  there. The folder is `<downloadRoot>/<user>/<last folder name>`, or
  `<downloadRoot>/<last folder name>` when only that exists. No remote path mapping is involved.
  The normal import then adopts or hardlinks from that folder.
- **Removal.** Each transfer is deleted from slskd (`DELETE .../{user}/{id}?remove=true`, which
  cancels an active one). With "delete data" only the files this grab fetched are deleted, and
  the folder if that leaves it empty, never anything outside the download root.
- **Lidarr face.** `/downloadclient/schema` and `/indexer/schema` list a native `Slskd`
  template (protocol `SoulseekDownloadProtocol`) unless a plugin already offers `Slskd`.
- **Migration.** A Lidarr `Slskd` indexer or client row keeps kind `slskd`; `BaseUrl`/`ApiKey`
  spellings are normalised to `baseUrl`/`apiKey` and the trailing slash is dropped.

Not covered compared with Tubifarry: fuzzy or accent-folding matching (only case, punctuation
and `&`/`and` are normalised), matching against the album's real track list or count (the
album's track list is not available to the indexer, so an incomplete folder with at least
`minimumFiles` tracks can be offered), multi-disc folders split across `CD1`/`CD2`
subfolders (each disc folder is a separate candidate), retries of failed files (a failed file
fails the grab and the pipeline re-searches), peer reputation beyond the blocklist and queue
limit, and extra file types (covers, cue sheets). RSS does not apply to Soulseek.

Tests: `crates/cellarr-download/tests/slskd_api.rs` (a fake slskd over real HTTP),
`crates/cellarr-jobs/tests/slskd_e2e.rs` (search, grab, status and import through the pipeline).

## Native Apple Music (AppleMusicarr)

The AppleMusicarr plugin no longer needs the sidecar either. The kinds `applemusicarr`,
`applemusicarrindexer` and `applemusicarrdownloadclient` (the last two are what a migrated
Lidarr row carries, being the plugin's implementation names lowercased) are built natively:
cellarr searches the Apple Music catalogue itself and hands albums to the existing
`applemusic-decryptor` service, as both the indexer and the download client.

**DRM decryption stays in the decryptor service.** cellarr only reads the public catalogue and
drives the decryptor's job API. It does not fetch, decrypt or tag audio.

A row builds natively when it has what its half needs and does not say `"native": false`:

| Side | Needs |
|---|---|
| indexer | `developerToken` |
| download client | `developerToken`, `musicUserToken`, an http(s) `decryptorUrl` |

Any other row (and a row on a build without the native kind) takes the sidecar plugin path
described above, unchanged. The field names are the plugin's own, so a migrated row works as it
is; PascalCase spellings are accepted too.

| Setting | Side | Default | Meaning |
|---|---|---|---|
| `developerToken` | both | required | MusicKit developer token (a JWT). Secret |
| `musicUserToken` | client (indexer: web-player tokens only) | required on the client | Sent to the decryptor with every album. Secret |
| `storefront` | both | `us` | two-letter storefront |
| `baseUrl` | indexer/client | `https://api.music.apple.com/v1/` | API base; the default is swapped for `amp-api.music.apple.com` when the token is the web player's (`iss` = `AMPWebPlay`), with its `Origin`/`Referer` and `media-user-token` cookie |
| `earlyReleaseLimit` | indexer | 14 | ignore albums releasing further ahead (days); `null` = no window |
| `searchLimit` | indexer | 25 | albums per query, 1-100 |
| `decryptorUrl` | client | required | e.g. `http://applemusic-decryptor.lidarr.svc.cluster.local:8080` |
| `downloadPath` | client | `/data/torrents/apple` | the decryptor's `OUTPUT_DIR` volume, as cellarr sees it |
| `preferredQuality` | client | AAC 256 | only for a link that names no quality |
| `decryptorTimeoutMinutes` | client | 180 | an unfinished job older than this fails |

How it works (`crates/cellarr-download/src/applemusic.rs`):

- **Search.** `GET {api}/catalog/{storefront}/search?term=..&types=albums&limit=..` with the
  developer token as a bearer. Tiers: `artist album`, then `artist`, then `album`; the first
  tier that yields a release wins. Only albums whose title equals the wanted one count, after
  folding case, accents and punctuation and dropping Apple's ` - Single`/` - EP` suffix.
  Containment is deliberately not used, so a remix of the album is not offered as the album.
  No match is a valid empty answer. A rejected token is an error (not "no results") and its
  text diagnoses the token (not a JWT, truncated, expired) without repeating it.
- **Releases.** One per quality: `Artist - Album [AAC] [256]`, `[ALAC] [LOSSLESS]` and
  `[AAC] [VBR] [ATMOS]` (the last only unless Apple lists the album's capabilities without
  Atmos; it parses as AAC VBR, which is what the file reads back as). `download_url` is the
  plugin's `applemusicarr://album/<id>?quality=<aac256|alac|atmos>`, the guid
  `applemusicarr:<id>:<quality>`; the size is estimated from the duration and the tier's
  nominal bitrate.
- **Routing.** An `applemusicarr://` release is routed like a plugin protocol
  (`AppleMusicarrDownloadProtocol`): only an Apple Music client takes it.
- **Grab.** `GET {api}/catalog/{storefront}/albums/{id}?include=tracks` for the track list, then
  `POST {decryptor}/v1/download` with `albumId`, `artist`, `album`, `storefront`, `quality`,
  `musicUserToken`, `directory` (`Artist/Album`, sanitised, relative to the decryptor's
  `OUTPUT_DIR`) and `tracks[{id,title,trackNumber,discNumber}]`; the answer is `{job_id}`. The
  download id carries the job id, so status survives a restart.
- **Status.** `GET {decryptor}/v1/job/{id}`: `queued` is Queued, `running` Downloading (progress
  is its percent), `completed` is Completed with the content path `<downloadPath>/<directory
  the job reports>` (a directory with `..` or a root is refused), `failed`/`cancelled` is Failed
  with the job's error. A completed job's partial-success note is logged and the album is
  imported as it is. A job the decryptor has forgotten (it keeps jobs in memory) is not found.
- **Removal.** A job that is still running is cancelled (`DELETE /v1/job/{id}`). With "delete
  data" only the files the job reported are deleted, then the folder if empty, never anything
  outside the download root.
- **Lidarr face.** `/downloadclient/schema` and `/indexer/schema` list a native `AppleMusicarr`
  template (protocol `AppleMusicarrDownloadProtocol`) unless a plugin already offers one.
- **Migration.** A Lidarr AppleMusicarr row keeps the kind the plugin named itself; its
  settings are carried verbatim (PascalCase spellings normalised, the decryptor URL's trailing
  slash dropped).
- **Secrets.** Both tokens are redacted in `Debug` and never appear in an error message or log.

Not covered compared with the plugin: signing a developer token from a MusicKit `.p8` key
(`privateKeyPath`/`keyId`/`teamId`; such a row has no `developerToken` and takes the plugin
path), the `APPLEMUSICARR_*` environment-variable fallbacks, sharing one row's tokens with the
other half (the client row needs its own `developerToken` and `musicUserToken`), the plugin's
Lidarr-side Test buttons, the file-name quality marker the plugin used for Lidarr's custom
formats, and deleting the whole album folder on removal (cellarr deletes only the files the job
reported). Two deliberate differences: a search whose every query failed (not auth) is an
error, not an empty result; and a bad-looking token is only diagnosed after Apple refuses it,
rather than blocking the request.

Cover art. A `cover.jpg` the decryptor writes next to the audio is not copied into the library.
This is not specific to Apple Music: the import carries sibling extra files (subtitles, `.nfo`) only
when they share the media file's stem (`Track.en.srt` beside `Track.flac`), for every download
client, and no client copies folder-level art such as `cover.jpg` or `folder.jpg`. Library cover art
comes from cellarr's own metadata/media-cover handling instead. Matching other clients here means
there is nothing to add; copying folder art would be a general import feature, left out for now.

Tests: `crates/cellarr-download/tests/applemusic_api.rs` (a fake catalogue and a fake decryptor
over real HTTP; they accept any token and record what they were sent),
`crates/cellarr-jobs/tests/applemusic_e2e.rs` (search, grab, status and import through the
pipeline into a temp library). Nothing in the tests contacts Apple or a real decryptor.

## Tests

- `crates/cellarr-plugins/tests/host.rs`: the two example WASM plugins, loaded and run (search,
  qBittorrent session, host allow-listing, fuel exhaustion, the directory loader).
- `crates/cellarr-cli/tests/lidarr_e2e.rs`: the whole daemon, driven through the Lidarr API, with
  built-in, WASM-plugin and sidecar implementations. The sidecar test runs a real Lidarr and only
  runs when `LIDARR_SIDECAR_BIN` points at a Lidarr install (`just sidecar-test`).
