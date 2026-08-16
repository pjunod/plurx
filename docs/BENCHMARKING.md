# Benchmarking — measure Cinema against Plex without grading different tests

Companion to [PLAYBACK.md](PLAYBACK.md) (how plurx chooses a delivery path) and
[PLAYBACK-TESTING.md](PLAYBACK-TESTING.md) (whether those paths play) — this is
the controlled A/B performance suite. It runs the same media, controller,
operation, output rung, and trial order against Cinema/plurx and Plex, then
writes raw JSONL plus p50/p95/p99, failure rates, and Cinema-to-Plex ratios.

The harness does not contain benchmark results. Numbers enter the repository
only when you run it against real servers and media; the default output is
ignored under `target/` so one machine's result cannot become a product claim by
accident.

## The v1 matrix separates paths that spend time differently

[`benchmarks/cinema-plex.example.toml`](../benchmarks/cinema-plex.example.toml)
is both the corpus manifest and the scenario contract. Its committed coverage is
checked by `make benchmark-check`.

| Case | Source → output | Clocks |
|---|---|---|
| 1080p direct startup | H.264/AAC 1080p → source bytes | engine · decoded frame |
| 4K direct startup | HEVC 2160p → source bytes | engine · decoded frame |
| 1080p transcode startup | 1080p → H.264/AAC 1080p | engine · decoded frame |
| 4K transcode startup | 2160p → H.264/AAC 1080p | engine · decoded frame |
| ±30-second seek | direct and transcode, separate scenarios | engine · decoded frame |
| ±5-minute seek | direct and transcode, separate scenarios | engine · decoded frame |
| 50 seeded seeks | identical target list for both servers | engine · decoded frame |
| Pause/resume | direct and transcode, separate scenarios | decoded frame only |
| 1 / 2 / 4 / 8 transcodes | 4K → 1080p, one row per stream | engine · decoded frame |

**Server/engine latency** stops at the first direct bytes for startup, the first
demuxed source packet for a direct seek, or the first completed HLS media
segment for a transcode. Direct seeks need the common demuxer because time does
not map honestly to a byte offset in variable-bitrate media; that clock stops
before video decode. **End-to-end latency** stops when the same FFmpeg build on
the controller decodes its first video frame. Pause/resume is end-to-end only:
a server cannot truthfully report when a client painted a frame. Never compare
a server/engine percentile with an end-to-end percentile.

The common decoder is intentionally narrower than either product's native UI.
It removes two different web applications from the A/B and gives both servers
the same demux/decode cost. It does **not** claim to measure button-click,
layout, or platform-player overhead; use the existing playback lab or a native
device trace for those surfaces.

## 1. Build one corpus — identity precedes performance

Use one ordinary 1080p file and one ordinary 4K file that are long enough for
the ±5-minute and random-seek windows. Both servers must read the same bytes
from the same storage class. A reflink or byte-identical copy is acceptable; two
different encodes of the same title are not.

```bash
# Hash each source on every server; both copies must print the same digest.
sha256sum /path/to/benchmark-1080p.mp4
sha256sum /path/to/benchmark-4k.mkv

# Capture the media facts used in the TOML. ffprobe itself is not timed.
ffprobe -v error -show_format -show_streams -of json /path/to/benchmark-1080p.mp4
ffprobe -v error -show_format -show_streams -of json /path/to/benchmark-4k.mkv
```

Record each digest, duration, dimensions, container, codecs, and overall bitrate
in the copied config. Then bind that identity to a plurx `file_id` and a Plex
`rating_key` / media index / part index. The harness queries both catalogs before
the first trial and rejects codec, container, dimensions, duration (±2 seconds),
or bitrate (±15% by default) that contradicts the manifest.

The server APIs do not expose a full-file digest, so the harness records the
configured SHA-256 in every row but cannot recalculate it remotely. The
operator-owned `sha256sum` check above is therefore part of the evidence. Do not
replace the example's all-zero digest with a guess; it is an explicit
placeholder designed to make an unprepared result visually indefensible.

## 2. Hold the A/B environment constant

The cleanest setup is one physical server, one media mount, and one GPU, with
only the product under test running at a time. Two hosts are acceptable only
when CPU, memory, GPU, driver, storage path, power profile, thermals, and network
path are equivalent and recorded outside the result.

Before a publishable run:

1. **Pin versions.** Build Cinema/plurx from the commit you mean to measure and
   record the Plex version. Plex often does not expose a commit; leave
   `server_commit` null rather than inventing one.
2. **Match the transcode contract.** Both products must produce H.264/AAC at the
   configured height and bitrate. Confirm hardware acceleration is either on
   for both or off for both. Record exceptions such as tone mapping — they are
   part of the engine being compared, not a reason to relabel the result.
3. **Remove unrelated work.** Stop scans, thumbnail generation, backup jobs,
   downloads, and other streams. Disable Plex Relay and test on the LAN so a
   cloud path cannot enter one side.
4. **Use one controller.** Keep the FFmpeg executable, controller host, switch,
   and link type fixed. `client_version` in every row fingerprints the decoder.
5. **Choose cache state honestly.** The template says `warm`. A scenario marked
   `cold` is rejected unless both servers configure `before_trial_command`.
   Those hooks must implement equivalent cache/transcode cleanup and should be
   preserved with the result.
6. **Let the harness alternate order.** Even iterations run the configured
   server order; odd iterations reverse it. This balances thermal drift and
   background load. `cooldown_seconds` separates each side of a pair.

Do not clear the OS page cache on one side and only delete a product transcode
cache on the other. “Cold” is a protocol, not a feeling.

## 3. Configure servers and credentials without storing tokens

Copy the template into ignored `target/`, replace its placeholders, and keep
tokens only in the environment variables named by the two server tables.

```bash
mkdir -p target
cp benchmarks/cinema-plex.example.toml target/cinema-plex.toml
$EDITOR target/cinema-plex.toml              # addresses, media facts, ids, hashes

export CINEMA_BENCH_TOKEN='…'                # plurx API key or login token
export PLEX_BENCH_TOKEN='…'                  # Plex server token

scripts/cinema-plex-bench validate \
  --config target/cinema-plex.toml \
  --require-v1                               # expands targets; contacts no server
```

The config rejects credentials embedded in URLs. Tokens are not written to the
manifest, raw rows, reports, or errors. For end-to-end direct/Plex HLS decoding,
FFmpeg receives the authorization header as a process argument, so run the
controller on a trusted account where other users cannot inspect its process
list.

Each server table accepts optional hook and monitor argv arrays. Arrays are
executed directly — never through a shell — and receive a credential-free trial
context JSON object on standard input.

| Setting | Meaning |
|---|---|
| `before_trial_command` | Prepare the declared cache state; mandatory for `cold`. |
| `after_trial_command` | Restore or clean product state after every trial/batch. |
| `monitor_command` | Return one cumulative resource snapshot before and after the trial. |

## 4. Capture CPU, GPU, RSS, storage, and network from the owning host

If `monitor_command` is omitted, all seven resource fields remain JSON `null` —
missing instrumentation stays missing rather than becoming zero. For a complete
run, configure a server-side sampler that prints exactly one JSON object:

```json
{
  "cpu_percent": 34.5,
  "gpu_percent": 71.0,
  "rss_bytes": 849346560,
  "storage_read_bytes": 981273645,
  "storage_write_bytes": 120938475,
  "network_rx_bytes": 873465,
  "network_tx_bytes": 918273645
}
```

CPU and GPU are instantaneous percentages; the report row uses the mean of the
before/after values. RSS is a gauge; the row uses the larger endpoint. Storage
and network values are monotonic counters; the row stores `after - before`.
Sample the product's whole process tree or container/cgroup, because the
transcoder is a child process. Host-wide network counters are acceptable only
on an otherwise quiet host and must be labeled as such with the archived
sampler.

Concurrent cases capture one resource interval around the entire batch. The
same batch values appear on each stream row with `resource_scope = "batch"`;
do not sum them. This preserves one flat raw schema without pretending a shared
GPU interval belongs to one stream.

## 5. Run a smoke pair, then the real matrix

Start with one direct and one transcode scenario at one iteration. An exact
`--output-dir` must not already exist, which prevents a second run from
overwriting evidence.

```bash
scripts/cinema-plex-bench run \
  --config target/cinema-plex.toml \
  --scenario startup-1080p-direct,startup-1080p-transcode \
  --iterations 1 \
  --output-dir target/cinema-plex-bench/smoke

# Full v1. The default creates a new UTC-stamped directory.
scripts/cinema-plex-bench run --config target/cinema-plex.toml
```

You can restrict a diagnostic pass with `--server cinema` or repeat/comma-
separate `--scenario`. A single-server run is useful for repair, but it cannot
produce Cinema-vs-Plex ratios and is not an A/B result.

Every successful run contains:

| Artifact | Job |
|---|---|
| `manifest.json` | Config copy, controller fingerprint, discovered server/media facts, completion status. |
| `raw.jsonl` | One append-and-flush measurement per attempt; survives an interrupted run. |
| `summary.json` | Machine-readable groups, p50/p95/p99, failure rates, and both ratio directions. |
| `report.md` | Human table plus the scope and ratio interpretation. |

An interrupted run keeps its raw rows and a `failed` or `running` manifest. Do
not merge partial rows with a later run: the order balance and environment may
have changed.

## Raw rows carry the facts needed to challenge a claim

Every measurement row includes these fields even when a value is `null`:

| Domain | Fields |
|---|---|
| Identity | `run_id` · `pair_id` · server/name/product/version/commit/instance |
| Client/media | client/name/version/host · media/title/SHA-256 · source dimensions/container |
| Codecs/rates | source and output video/audio codecs · requested/basis/advertised kb/s |
| Operation | scenario · operation/variant · scope · mode · cache state · iteration · seek target/delta |
| Result | `latency_ms` · `success` · typed/redacted error · operation details |
| Resources | CPU · GPU · RSS · storage read/write · network receive/transmit · resource scope |

Sessions and authorization URLs are deliberately absent. A failed attempt is a
row, not an exception that disappears from the denominator.

## Reports use distributions and make ratio direction explicit

Regenerate reports without touching either server:

```bash
scripts/cinema-plex-bench report \
  --input target/cinema-plex-bench/<run>/raw.jsonl
```

Latency p50/p95/p99 uses linear type-7 percentiles over successful samples.
Failure and error rates use every attempt. The two ratio forms are:

```text
Cinema/Plex latency ratio = Cinema percentile ÷ Plex percentile
Plex-over-Cinema speedup  = Plex percentile ÷ Cinema percentile
```

For the first form, `< 1.00` means Cinema was faster and `> 1.00` means Plex
was faster. For the speedup form, `> 1.00` means Cinema was faster. The report
prints both in JSON and the first in Markdown. A latency win accompanied by a
higher failure rate is not a win; read the error rows before making a claim.

Thirty iterations is the template floor for ordinary latency scenarios. The
seeded seek scenarios run 50 distinct positions once, and concurrency runs five
batches per level. Increase those counts when tails are unstable; never remove
failed samples or choose a friendlier seed after looking at the result.

## Non-goals — what v1 does not establish

- **No visual-quality verdict.** Matching requested bitrate and codecs does not
  prove matching VMAF, grain retention, HDR handling, or audio quality. Use the
  existing rate-control/VMAF harness for that separate question.
- **No native-UI click timing.** The common FFmpeg decoder makes playback bytes
  comparable; it does not include either product's web/native navigation.
- **No WAN or constrained-link resilience.** v1 measures an unconstrained LAN.
  Add a separately named scenario and preserve the traffic-shaping contract
  before claiming anything about remote playback or rebuffering.
- **No automatic remote cache purge.** Only an explicit, symmetric hook earns a
  `cold` label. The harness refuses to fake one.
- **No inferred resource zeroes.** A missing GPU or network sampler is `null`.
  Silence is not evidence of efficiency.
