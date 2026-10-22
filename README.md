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

Rule for inclusion: the server itself must do the work of each handler. A handler that hands the
work to another codebase is marked in `server.toml` under `[[offloaded]]` with a `kind` (`upload`,
`get`, `head`, `miss`) and a `reason`. With `disqualify = true` the workloads of that kind still run and keep their recorded values, but the
report draws them in purple with a DQ tag, notes the reason, leaves them out of the scale the summary
bars use, and lists the server after the others. Without it the
workload runs and is scored, and the report names what it offloads (amber text under the chart,
and in the summary's problems column). `GET` must return the bytes: a redirect to another server
is a disqualification.

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
  (created empty under the data dir), `follow_redirects` (for a server whose GET redirects to the bytes), `[[offloaded]]` (`kind`, `reason`, `disqualify`), `[[after_ready]]` (`service` + `cmd`, run with `docker compose exec` once the
  server answers, for seeding) and `[upload]`: `path`, `auth = "std" | "url"` base64
  alphabet, `bound_auth` (sign `server`, `u` and `method` tags), `sha_header`,
  `content_type`, `content` (`random`, `text`, `png`), `settle_s`, `resign`, `signers` (sign with this many
  different keys, for servers that cap uploads per key) and `max_connections` (cap upload connections for
  servers that refuse more).
- `files/`: copied into the data dir before start, which the server sees at `/data`.

`${BB_PORT}`, `${BB_PUBKEY}` and `${BB_NPUB}` are also expanded inside `files/` and
`[[after_ready]]` commands. Compose sees `BB_PORT`, `BB_DATA`, `BB_PUBKEY`, `BB_NPUB`,
`BB_UID`, `BB_SERVER_CPUS`, `BB_SERVICE_CPUS` and `BB_MEMORY`. `BB_PUBKEY` is the fixed
benchmark key that signs every upload, so a server can allow-list or register it.
`BB_NPUB` is the same key in bech32.

## Included servers

route96, ditto-blossom, hzrd149/blossom-server (Deno, 6.3.1), sebdeveloper6952/blossom-server (Go),
MonumentalSystems/blossom-rs, 0xtrr/cherry-server, quentintaranpino/nostrcheck-server,
servus-social/servus, Sakura (Odin), iqbqioza/nostrfy, barrydeen/haven and fiatjaf/pyramid (both use khatru's Blossom
package), and damus-io/afterbloom.

Not included: satellite-cdn, which needs Cloudflare R2, and nostrbuild/nostr.build, whose repo holds
only the PHP backend behind a Cloudflare Worker proxy (it answers uploads over HMAC-signed
requests from that Worker and has no public blob GET of its own).

PrimalHQ/primal-blossom-server is included with its read workloads disqualified: GET answers 302
to a separate media server (nginx in the compose project), so the Blossom code never sends the
file. Its uploads, HEAD and misses are its own and are scored, with the upload handler marked as
offloading media processing to a Julia worker that is not run here. `follow_redirects = true`
makes the load generator follow a redirect and count a GET only when the final response has the
right bytes, which is how the disqualified rows were measured before. Its schema file lacks four
tables its queries use, which `files/schema.sql` adds.

nostrcheck-server is marked as offloading uploads: it answers 202 and finishes the work in a
background queue, in the same container.

Needs docker with the compose plugin. Images are cached between runs; `--rebuild` rebuilds them without cache.

## Even durability: data in RAM

By default each server keeps its own durability settings on disk: afterbloom, haven and blossom-rs
sync each blob file, ditto-blossom batches a `syncfs`, and sakura, cherry-server, pyramid, primal and
route96 write without any sync. Their upload rates are not comparable.

`blossom-bench run --ram-data /dev/shm/blossom-bench` removes the difference. Every server's data
dir goes on tmpfs, and the databases that run in their own container (MariaDB, Postgres) get a
tmpfs data dir through their `compose.ram.yaml`. Writes are then asynchronous for everyone and an
`fsync` costs nothing, so no server is ahead because it skips a sync and none is behind because it
issues one. The run refuses to start unless the path is on tmpfs, and the report header says which
mode was used. Results go to `latest-ram.html` so they never replace the default run.

What it does not measure: the real cost of durability. It measures the software, not the disk.
Blob data counts as memory, so a run needs a few GiB free in `/dev/shm` (the standard suite writes
about 1 GiB of blobs per server, removed after each server finishes).

## Caveats

Everything runs over loopback, so network and TLS costs are absent. Uploads are
one signature per blob, made before timing starts. By default servers run with their own
durability settings, which differ a lot: afterbloom, haven and blossom-rs sync each blob file,
ditto-blossom batches a `syncfs`, and sakura, cherry-server, pyramid, primal and route96 write
without any sync, so their upload rates are not comparable. Run with `--ram-data` to remove that
difference (above). The Deno server can also answer 503 under load. Treat the ratios as indicative and rerun on the hardware that matters.
