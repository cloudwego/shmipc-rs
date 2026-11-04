# Shmipc

[![Crates.io](https://img.shields.io/crates/v/shmipc)](https://crates.io/crates/shmipc)
[![Documentation](https://docs.rs/shmipc/badge.svg)](https://docs.rs/shmipc)
[![Website](https://img.shields.io/website?up_message=cloudwego&url=https%3A%2F%2Fwww.cloudwego.io%2F)](https://www.cloudwego.io/)
[![License](https://img.shields.io/crates/l/shmipc)](#license)
[![Build Status][actions-badge]][actions-url]

[actions-badge]: https://github.com/cloudwego/shmipc-rs/actions/workflows/ci.yaml/badge.svg
[actions-url]: https://github.com/cloudwego/shmipc-rs/actions

English | [中文](README_CN.md)

## Introduction

Shmipc is a high performance inter-process communication library developed by ByteDance.
It is built on Linux's shared memory technology and uses unix or tcp connection to do process synchronization and finally implements zero copy communication across inter-processes. 
In IO-intensive or large-package scenarios, it has better performance.

## Features

### Zero copy

In an industrial production environment, the unix domain socket and tcp loopback are often used in inter-process communication.The read operation or the write operation need copy data between user space buffer and kernel space buffer. But shmipc directly store data to the share memory, so no copy compared to the former.

### Batch IO

An IO queue was mapped to share memory, which describe the metadata of communication data.
so that a process could put many request to the IO queue, and other process could handle a batch IO per synchronization. It could effectively reduce the system calls which was brought by process synchronization.

## Performance Testing

The source code bench.rs, doing a performance comparison between shmipc and unix domain socket in ping-pong scenario with different package size. The result is as follows: having a performance improvement whatever small package or large package.

```
benchmark_parallel_ping_pong_by_shmipc_64b
                        time:   [1.0227 µs 1.0662 µs 1.1158 µs]
benchmark_parallel_ping_pong_by_uds_64b
                        time:   [2.4879 µs 2.6609 µs 2.8643 µs]

benchmark_parallel_ping_pong_by_shmipc_512b
                        time:   [973.36 ns 1.0128 µs 1.0572 µs]
benchmark_parallel_ping_pong_by_uds_512b
                        time:   [2.3158 µs 2.4003 µs 2.4921 µs]

benchmark_parallel_ping_pong_by_shmipc_1024b
                        time:   [1.0084 µs 1.0509 µs 1.0996 µs]
benchmark_parallel_ping_pong_by_uds_1024b
                        time:   [2.3272 µs 2.4259 µs 2.5353 µs]

benchmark_parallel_ping_pong_by_shmipc_4096b
                        time:   [988.93 ns 1.0183 µs 1.0495 µs]
benchmark_parallel_ping_pong_by_uds_4096b
                        time:   [3.4969 µs 3.5927 µs 3.6835 µs]

benchmark_parallel_ping_pong_by_shmipc_16384b
                        time:   [1.1775 µs 1.2264 µs 1.2806 µs]
benchmark_parallel_ping_pong_by_uds_16384b
                        time:   [5.3147 µs 5.8754 µs 6.5797 µs]

benchmark_parallel_ping_pong_by_shmipc_32768b
                        time:   [1.1225 µs 1.1844 µs 1.2498 µs]
benchmark_parallel_ping_pong_by_uds_32768b
                        time:   [6.9058 µs 7.0930 µs 7.3091 µs]

benchmark_parallel_ping_pong_by_shmipc_65536b
                        time:   [1.1807 µs 1.2538 µs 1.3238 µs]
benchmark_parallel_ping_pong_by_uds_65536b
                        time:   [15.082 µs 15.419 µs 15.802 µs]

benchmark_parallel_ping_pong_by_shmipc_262144b
                        time:   [1.1137 µs 1.1673 µs 1.2253 µs]
benchmark_parallel_ping_pong_by_uds_262144b
                        time:   [68.750 µs 70.805 µs 73.121 µs]

benchmark_parallel_ping_pong_by_shmipc_524288b
                        time:   [1.1722 µs 1.2599 µs 1.3426 µs]
benchmark_parallel_ping_pong_by_uds_524288b
                        time:   [146.55 µs 150.90 µs 155.72 µs]

benchmark_parallel_ping_pong_by_shmipc_1048576b
                        time:   [2.3027 µs 2.3872 µs 2.4821 µs]
benchmark_parallel_ping_pong_by_uds_1048576b
                        time:   [341.92 µs 354.70 µs 368.29 µs]
                        
benchmark_parallel_ping_pong_by_shmipc_4194304b
                        time:   [4.7158 µs 4.8180 µs 4.9205 µs]
benchmark_parallel_ping_pong_by_uds_4194304b
                        time:   [2.1126 ms 2.3210 ms 2.5974 ms]
```

- BenchmarkParallelPingPongByUds, the ping-pong communication base on unix domain socket.
- BenchmarkParallelPingPongByShmipc, the ping-pong communication base on shmipc.
- The suffix of the testing case name is the package size of communication, which from 64 Byte to 4 MB.

### Quick start

#### HelloWorld

- [HelloWorldClient](examples/src/hello_world/greeter_client.rs)
- [HelloWorldServer](examples/src/hello_world/greeter_server.rs)

#### 0.3 breaking changes

- `Buf` and `ShmBuf` no longer take a lifetime parameter: `Buf<'shm>` is now `Buf`. A read result
  keeps its shared-memory block alive by reference count instead of by borrowing the session, so it
  can be stored and moved freely. Drop the lifetime from any signature or struct that named it.
- `ProtocolMode` gained a `V5 { fallback }` variant, so an exhaustive `match` over it needs a new
  arm.
- `Config` gained `v5_queue_publish_timeout`, and `Stats` / `StatsSnapshot` gained `v5_*` counters.
  Struct literals over those types need the new fields, or `..Default::default()`.
- `MAX_SUPPORT_PROTO_VERSION` is now `5`.

#### 0.2 read API migration

The 0.2 API separates streaming reads from exact contiguous reads:

- Replace `Stream::read()` with `Stream::read_chunk()` when consuming a stream or implementing
  `AsyncRead`. Each call returns one non-empty contiguous buffer slice.
- Replace `Stream::read_bytes(size)` with `Stream::read_exact_bytes(size)` when exactly `size`
  contiguous bytes are required. A cross-slice result may be coalesced into owned memory.

The old ambiguous method names are not retained as compatibility aliases.

#### Protocol configuration

`Config::default()` keeps the legacy behavior: file-path shared memory uses the V2 protocol and
memfd shared memory uses the V3 protocol. V4 and V5 are opt-in through the nested protocol config
helpers:

```rust
use std::time::Duration;

use shmipc::{config::Config, session::SessionManagerConfig};

let legacy = Config::default();
let v4 = Config::default().with_v4();
let v4_eventfd = Config::default().with_v4_eventfd();
let v4_polling = Config::default().with_v4_event_queue_polling(Duration::from_micros(100));
let v5 = Config::default().with_v5();
let v5_eventfd = Config::default().with_v5_eventfd();
let v5_polling = Config::default().with_v5_event_queue_polling(Duration::from_micros(100));

let sm_config = SessionManagerConfig::new().with_config(v4_eventfd);
```

Mode selection:

| Mode | Config | When to use |
| ---- | ------ | ----------- |
| Legacy V2/V3 | `Config::default()` | Existing deployments and gradual upgrades. |
| V4 default | `Config::default().with_v4()` | First V4 rollout step; keeps the Unix-socket polling wakeup path. |
| V4 eventfd | `Config::default().with_v4_eventfd()` | Recommended V4 mode for low-latency small-message workloads on Linux memfd. |
| V4 polling | `Config::default().with_v4_event_queue_polling(interval)` | Reduces per-message wakeup traffic; latency is bounded by the negotiated polling interval and OS timer behavior. |
| V5 default | `Config::default().with_v5()` | First V5 rollout step; keeps the Unix-socket polling wakeup path. |
| V5 eventfd | `Config::default().with_v5_eventfd()` | Recommended V5 mode for low-latency small-message workloads. |
| V5 polling | `Config::default().with_v5_event_queue_polling(interval)` | Same trade-off as V4 polling, on the V5 segment model. |

Server-side notes:

- A server accepts the highest version its client offered, whatever its own mode is, so a server on
  `Config::default()` serves legacy V2/V3, V4 and V5 clients alike. The server's own mode only
  decides what that process would ask for if it dialled out. Optional wakeup features are selected
  from the client request.
- `eventfd_wakeup` requires memfd shared memory. This is the default `MemMapType`; do not switch the
  server to `MemMapTypeDevShmFile` if clients need V4 eventfd.
- The polling interval is negotiated from the client request (V4 or V5) and used by both peers.
- The V4 helpers set `fallback = true`. A client falls back to legacy V3 only when initial V4 setup
  fails with a network/protocol failure. Explicit negotiation rejections are returned as errors.
  Set `config.protocol.mode = ProtocolMode::V4 { fallback: false }` when silent downgrade is not
  desired.
- The V5 helpers set `fallback = true` as well, but V5 is announced alongside V4 in the same
  negotiation, so a peer that only speaks V4 settles on V4 without a reconnect. A V5 problem
  therefore shows as a quiet downgrade rather than an error; check `Stream::v5_dynshm_stats()`.

The example server can be used with the V4 clients:

- [V4 default client](examples/src/hello_world/greeter_client_v4_default.rs)
- [V4 eventfd client](examples/src/hello_world/greeter_client_v4_eventfd.rs)
- [V4 polling client](examples/src/hello_world/greeter_client_v4_polling.rs)

Run them from the `examples` package:

```bash
cargo run -p examples --bin greeter_server
cargo run -p examples --bin greeter_client_v4_default
cargo run -p examples --bin greeter_client_v4_eventfd
cargo run -p examples --bin greeter_client_v4_polling
```

#### V5

V5 uses the same `Stream` API as V4. Reads, writes, flush and close are unchanged, and the mode
helpers mirror the V4 ones one for one, so nothing above needs redoing. What differs is the memory
model:

- Each side owns its queue and its own dynamic segment instead of both sharing one buffer pool, so
  a slow reader can no longer starve the writer's allocator.
- Buffers are allocated per message and returned once the peer has read them, so the segment grows
  and shrinks with load rather than being sized up front.
- A payload that fits the largest shared block is served from a shared region and chained onto the
  message; anything larger in a *single* `write_bytes` call gets a dedicated region of its own. The
  split is decided per call, not per message.

V5 requires memfd shared memory, which is the default `MemMapType`; `Config::verify()` rejects V5
on `MemMapTypeDevShmFile`. `Stream::v5_dynshm_stats()` reports this side's own region usage and
returns `None` on a session that did not negotiate V5.

The V5 example is a self-checking round trip rather than a greeter: it drives payloads either side
of the shared/dedicated split, verifies every echo byte for byte, and prints the segment counters.

- [V5 client](examples/src/v5/client.rs)
- [V5 server](examples/src/v5/server.rs)

```bash
cargo run -p examples --bin v5_server
cargo run -p examples --bin v5_client
```

When constructing `Config` with a struct literal, include `protocol` explicitly or use
`..Config::default()` so future config fields keep their defaults.

#### Close semantics

`Stream::flush()` means that data has been enqueued locally and the negotiated wakeup mechanism has
been attempted. It does not mean that the peer has already drained the shared-memory queue or read
the bytes. `SessionManager::close()` and listener/session close paths are hard closes. If the last
application frame must be delivered before shutdown, use an application-level acknowledgement or
completion barrier before closing the session.

## License

Shmipc is distributed under the [Apache License, version 2.0](LICENSE).
