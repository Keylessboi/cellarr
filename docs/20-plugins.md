# Plugins

cellarr takes two kinds of plugins. Both appear to *arr clients exactly like built-in
implementations: they are listed in `/indexer/schema` and `/downloadclient/schema` with their
own fields, configured through the same API, and used by the pipeline like any other indexer or
download client.

| | WASM plugins | Native Lidarr plugins |
|---|---|---|
| What | Indexers and download clients compiled to WASM against `crates/cellarr-plugins/wit/plugins.wit` | Lidarr's own .NET plugins (`Lidarr.Plugin.*.dll`), unmodified |
| Runs in | cellarr's process, sandboxed | A stock Lidarr run beside cellarr (the sidecar) |
| Cost | A few MB per plugin, only while called | A whole Lidarr: roughly 150–250 MB |
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

## Tests

- `crates/cellarr-plugins/tests/host.rs`: the two example WASM plugins, loaded and run (search,
  qBittorrent session, host allow-listing, fuel exhaustion, the directory loader).
- `crates/cellarr-cli/tests/lidarr_e2e.rs`: the whole daemon, driven through the Lidarr API, with
  built-in, WASM-plugin and sidecar implementations. The sidecar test runs a real Lidarr and only
  runs when `LIDARR_SIDECAR_BIN` points at a Lidarr install (`just sidecar-test`).
