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
results with `blossom-bench report results/<stamp>.json --baseline route96 --html out.html`.

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

## Workloads

`suites/*.toml`. `upload` workloads send `count` distinct blobs of `size` bytes, each with
its own signed BUD-11 token. `get`, `head` and `miss` workloads read from the blobs of the
upload workloads named in `blobs`. `zipf` skews reads toward a hot set. `content = "text"`
makes compressible bodies.

## How servers are run

Every server and every database runs in a Docker container, started the same way:
host networking, `--cpuset-cpus` for its CPU list, the same memory cap and file
limit, and the caller's uid. Images are built from the project's own source with a
Dockerfile written in the server definition (or the project's own Dockerfile), or
pulled when the project publishes one. Databases and caches get their own cores
(`service_cpus`) and count towards the server's CPU and memory.

## Adding a server

Drop a file in `servers/`. Fields:

- `name`, `port`, `ready_path`, `notes`
- `[source]`: `git` (with optional `rev`) or `path`; it becomes the docker build context
- `[image]`: `dockerfile_inline`, `dockerfile` (relative to the source) or `prebuilt`
- `[[services]]`: containers started first (`image`, `args`, `env`, `ready_cmd`)
- `[[files]]`: config files written into the data dir before start
- `[[after_ready]]`: `service` + `cmd` run with `docker exec` once the server answers, for seeding
- `[run]`: `cmd`, `entrypoint`, `env`, `mounts`, `startup_timeout_s`
- `[upload]`: `path`, `auth = "std" | "url"` base64 alphabet, `bound_auth` (sign `server`, `u` and
  `method` tags), `sha_header`, `content_type`, `content` (`random`, `text`, `png`), `settle_s`
- `url = "http://host:port"` instead of a source benchmarks a server that is already running

`{port}`, `{data}`, `{src}` and `{pubkey}` are expanded in args, env, files and hooks. `{pubkey}` is the
fixed benchmark key that signs every upload, so a server can allow-list or register it.

## Included servers

route96, ditto-blossom, hzrd149/blossom-server (Deno, 6.3.1), sebdeveloper6952/blossom-server (Go),
MonumentalSystems/blossom-rs, 0xtrr/cherry-server, quentintaranpino/nostrcheck-server and
servus-social/servus. Not included: khatru, which is a relay library with no storage backend of
its own, and satellite-cdn, which needs Cloudflare R2.

Needs docker. Images are cached between runs; `--rebuild` rebuilds them without cache.

## Caveats

Everything runs over loopback, so network and TLS costs are absent. Uploads are
one signature per blob, made before timing starts. Servers run with default durability
settings, which differ: ditto-blossom syncs every upload, and the Deno server can answer
503 under load. Treat the ratios as indicative and rerun on the hardware that matters.
