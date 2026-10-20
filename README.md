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

Output goes to `results/<stamp>.{json,md}` and `results/latest.md`.

## What it measures

Per server and workload: ok requests per second and MB/s, p50/p90/p99/p99.9 latency,
CPU cores used by the server process tree (including a database container), requests
per CPU-second, and peak anonymous and file-backed memory.

A request only counts as ok when the status is right and, for GETs, the body is the
full expected length. After each upload workload a sample of blobs is fetched and
re-hashed, and wrong readbacks are reported next to the throughput.

Servers are started from an empty data directory for each run. CPU sets are checked so
server and load generator never share a core. If the load generator is more than 85%
busy on its cores, the report says the number may be its limit.

## Workloads

`suites/*.toml`. `upload` workloads send `count` distinct blobs of `size` bytes, each with
its own signed BUD-11 token. `get`, `head` and `miss` workloads read from the blobs of the
upload workloads named in `blobs`. `zipf` skews reads toward a hot set. `content = "text"`
makes compressible bodies.

## Adding a server

Drop a file in `servers/`. Fields:

- `name`, `port`, `ready_path`, `notes`
- `[source]`: `git` or `path`, optional `rev`, `build` commands, `bin`
- `[[services]]`: containers to start first (`image`, `publish`, `env`, `ready_cmd`)
- `setup`: shell commands run in the data dir after it is emptied
- `[[files]]`: config files written before start
- `[run]`: `cmd`, `cwd`, `env`, `startup_timeout_s`
- `[upload]`: `path`, `auth = "std" | "url"` base64 alphabet, `sha_header`
- `url = "http://host:port"` instead of `[run]` benchmarks a server that is already running

`{port}`, `{data}`, `{src}` and `{bin}` are expanded in commands, env and files.

## Included servers

route96, ditto-blossom, hzrd149/blossom-server (Deno, pinned to 6.3.1) and
sebdeveloper6952/blossom-server (Go). Building them needs cargo, go, npm and docker.

## Caveats

Everything runs over loopback, so network and TLS costs are absent. Uploads are
one signature per blob, made before timing starts. Servers run with default durability
settings, which differ: ditto-blossom syncs every upload, and the Deno server can answer
503 under load. Treat the ratios as indicative and rerun on the hardware that matters.
