# blossom-bench

Throughput, latency, CPU and memory comparison of Blossom servers. It starts each
server from a TOML file, pins it to its own CPUs, drives it with the same
signed-upload and download workloads, reads blobs back to check them, and writes a
side-by-side table.

```sh
cargo build --release
./target/release/blossom-bench list
./target/release/blossom-bench run                          # standard suite, every server
./target/release/blossom-bench run -s route96 -s ditto-blossom --baseline route96
./target/release/blossom-bench run --suite suites/quick.toml
./target/release/blossom-bench report results/<stamp>.json --baseline route96
```

Output goes to `results/<stamp>.{json,md,html}` and `results/latest.{md,html}`. The HTML page is
self-contained (no network, no build step): a ratio heat map, per-workload throughput and
efficiency bars with tabs, latency and memory tables, and the notes. Re-render one from saved
results with `blossom-bench report results/<stamp>.json --html out.html`.

## What it measures

Per server and workload: ok requests per second and MB/s, p50/p90/p99/p99.9 latency,
CPU cores used by the server process tree (including a database container), requests
per CPU-second, and peak anonymous and file-backed memory.

A request only counts as ok when the status is right and, for GETs, the body is the
full expected length. After each upload workload a sample of blobs is fetched and
re-hashed, and wrong readbacks are reported next to the throughput.

Servers are started from an empty data directory for each run, and a failing response body is quoted in the report. CPU sets are checked so
server and load generator never share a core. If the load generator is more than 85%
busy on its cores, the report says the number may be its limit.

Rule for inclusion: the server itself must answer `GET /<sha256>` with the blob bytes.

## Workloads

`suites/*.toml`. `upload` workloads send `count` distinct blobs of `size` bytes, each with
its own signed BUD-11 token. `get`, `head` and `miss` workloads read from the blobs of the
upload workloads named in `blobs`. `zipf` skews reads toward a hot set. `content = "text"`
makes compressible bodies.

## How servers are run

Every server is a Docker Compose project in `servers/<name>/`. The server container and
each database or cache it needs get host networking, the same memory cap and file limit,
and a fixed set of cores: the server on `server_cpus`, everything else on `service_cpus`.
`servers/common.yaml` holds those limits as two services (`app` and `aux`) that each
compose file extends, so no server can run with different limits. Databases count towards
the server's CPU and memory.

## Adding a server

Make `servers/<name>/` with:

- `compose.yaml`: a service named `server` that extends `../common.yaml` service `app`,
  plus any databases extending `aux`. Give databases a healthcheck and make `server`
  depend on them with `condition: service_healthy`. Build from the project's git URL
  (`build.context`) with either its own Dockerfile or `dockerfile_inline`, or use a
  published `image`.
- `server.toml`: `name`, `port`, `ready_path`, `notes`, `startup_timeout_s`, `data_dirs`
  (created empty under the data dir), `follow_redirects` (for a server whose GET redirects to the bytes), `[[after_ready]]` (`service` + `cmd`, run with `docker compose exec` once the
  server answers, for seeding) and `[upload]`: `path`, `auth = "std" | "url"` base64
  alphabet, `bound_auth` (sign `server`, `u` and `method` tags), `sha_header`,
  `content_type`, `content` (`random`, `text`, `png`), `settle_s`.
- `files/`: copied into the data dir before start, which the server sees at `/data`.

`${BB_PORT}`, `${BB_PUBKEY}` and `${BB_NPUB}` are also expanded inside `files/` and
`[[after_ready]]` commands. Compose sees `BB_PORT`, `BB_DATA`, `BB_PUBKEY`, `BB_NPUB`,
`BB_UID`, `BB_SERVER_CPUS`, `BB_SERVICE_CPUS` and `BB_MEMORY`. `BB_PUBKEY` is the fixed
benchmark key that signs every upload, so a server can allow-list or register it.
`BB_NPUB` is the same key in bech32.

## Included servers

route96, ditto-blossom, hzrd149/blossom-server (Deno, 6.3.1), sebdeveloper6952/blossom-server (Go),
MonumentalSystems/blossom-rs, 0xtrr/cherry-server, quentintaranpino/nostrcheck-server,
servus-social/servus, Sakura (Odin), barrydeen/haven and fiatjaf/pyramid (both use khatru's Blossom
package), and damus-io/afterbloom.

Not included: satellite-cdn, which needs Cloudflare R2, and nostrbuild/nostr.build, whose repo holds
only the PHP backend behind a Cloudflare Worker proxy (it answers uploads over HMAC-signed
requests from that Worker and has no public blob GET of its own).

Disqualified: PrimalHQ/primal-blossom-server. It does not serve blobs itself: GET answers 302 to a
separate media server (nginx in production), so the Blossom server only does the lookup. Every
included server answers GET with the bytes, so a server that cannot is not compared. Its compose
project is kept in `disqualified/` and can be run with `--servers disqualified`; the load generator
follows the redirect (`follow_redirects = true`) and counts a GET only when the final response has
the right bytes. Its schema file lacks four tables its queries use, which `files/schema.sql` adds.

Needs docker with the compose plugin. Images are cached between runs; `--rebuild` rebuilds them without cache.

## Caveats

Everything runs over loopback, so network and TLS costs are absent. Uploads are
one signature per blob, made before timing starts. Servers run with default durability
settings, which differ: ditto-blossom syncs every upload, and the Deno server can answer
503 under load. Treat the ratios as indicative and rerun on the hardware that matters.
