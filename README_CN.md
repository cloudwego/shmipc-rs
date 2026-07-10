# Shmipc

[![Crates.io](https://img.shields.io/crates/v/shmipc)](https://crates.io/crates/shmipc)
[![Documentation](https://docs.rs/shmipc/badge.svg)](https://docs.rs/shmipc)
[![Website](https://img.shields.io/website?up_message=cloudwego&url=https%3A%2F%2Fwww.cloudwego.io%2F)](https://www.cloudwego.io/)
[![License](https://img.shields.io/crates/l/shmipc)](#license)
[![Build Status][actions-badge]][actions-url]

[actions-badge]: https://github.com/cloudwego/shmipc-rs/actions/workflows/ci.yaml/badge.svg
[actions-url]: https://github.com/cloudwego/shmipc-rs/actions

[English](README.md) | 中文 

## 简介

Shmipc 是一个由字节跳动开发的高性能进程间通讯库。它基于 Linux 的共享内存构建，使用 unix/tcp 连接进行进程同步，实现进程间通讯零拷贝。在 IO 密集型场景或大包场景能够获得显著的性能收益。

## 特性

### 零拷贝

在工业生产环境中，Unix domain socket 和 Tcp loopback 常用于进程间通讯，读写均涉及通讯数据在用户态 buffer 与内核态 buffer 的来回拷贝。而 Shmipc 使用共享内存存放通讯数据，相对于前者没有数据拷贝。

### 批量收割IO

Shmipc 在共享内存中引入了一个 IO 队列来描述通讯数据的元信息，一个进程可以并发地将多个请求的元信息放入 IO 队列，另外一个进程只要需要一次同步就能批量收割 IO.这在 IO 密集的场景下能够有效减少进程同步带来的 system call。

## 性能测试

源码中 bench.rs 进行了 Shmipc 与 Unix domain socket 在 ping-pong 场景下不同数据包大小的性能对比，结果如下所示: 从小包到大包均有性能提升。

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

- BenchmarkParallelPingPongByUds，基于 Unix Domain Socket 进行 ping-pong 通讯。
- BenchmarkParallelPingPongByShmipc，基于 Shmipc 进行 ping-pong 通讯。
- 后缀为 ping-pong 的数据包大小， 从 64Byte ~ 4MB 不等。

### 快速开始

#### HelloWorld

- [HelloWorld客户端](examples/src/hello_world/greeter_client.rs)
- [HelloWorld服务端](examples/src/hello_world/greeter_server.rs)

#### 协议配置

`Config::default()` 保持 legacy 行为：file-path 共享内存使用 V2 协议，memfd 共享内存使用 V3
协议。V4 需要通过嵌套的 protocol 配置显式启用：

```rust
use std::time::Duration;

use shmipc::{config::Config, session::SessionManagerConfig};

let legacy = Config::default();
let v4 = Config::default().with_v4();
let v4_eventfd = Config::default().with_v4_eventfd();
let v4_polling = Config::default().with_v4_event_queue_polling(Duration::from_micros(100));

let sm_config = SessionManagerConfig::new().with_config(v4_eventfd);
```

模式选择：

| 模式 | 配置 | 适用场景 |
| ---- | ---- | -------- |
| Legacy V2/V3 | `Config::default()` | 现有部署和灰度升级。 |
| V4 default | `Config::default().with_v4()` | 首次接入 V4 的推荐起点，仍使用 Unix socket polling wakeup。 |
| V4 eventfd | `Config::default().with_v4_eventfd()` | Linux memfd 场景下的小包低延迟推荐模式。 |
| V4 polling | `Config::default().with_v4_event_queue_polling(interval)` | 减少每条消息的 wakeup 流量；延迟受协商出的 polling interval 和 OS timer 行为影响。 |

服务端接入说明：

- 服务端使用 `Config::default()` 即可同时接受 legacy V2/V3 客户端和 V4 客户端。V4 可选
  wakeup feature 会根据客户端请求协商选择。
- `eventfd_wakeup` 要求 memfd 共享内存。`MemMapType` 默认就是 memfd；如果客户端需要 V4
  eventfd，不要把服务端切到 `MemMapTypeDevShmFile`。
- polling interval 来自 V4 客户端请求，协商后 client/server 两侧共同使用。
- V4 helper 默认设置 `fallback = true`。客户端仅在 V4 初始化遇到网络/协议失败时回退到
  legacy V3；如果服务端显式拒绝 V4 negotiation，会直接返回错误，不会静默降级。如果不希望
  自动降级，可设置 `config.protocol.mode = ProtocolMode::V4 { fallback: false }`。

现有示例服务端可直接配合以下 V4 客户端使用：

- [V4 default 客户端](examples/src/hello_world/greeter_client_v4_default.rs)
- [V4 eventfd 客户端](examples/src/hello_world/greeter_client_v4_eventfd.rs)
- [V4 polling 客户端](examples/src/hello_world/greeter_client_v4_polling.rs)

从 `examples` package 运行：

```bash
cargo run -p examples --bin greeter_server
cargo run -p examples --bin greeter_client_v4_default
cargo run -p examples --bin greeter_client_v4_eventfd
cargo run -p examples --bin greeter_client_v4_polling
```

如果使用 struct literal 构造 `Config`，需要显式填写 `protocol`，或者使用
`..Config::default()` 保留默认字段，避免升级后遗漏新增配置项。

#### 关闭语义

`Stream::flush()` 的语义是数据已在本端入队，并按协商出的 wakeup 机制尝试通知对端；它不表示
对端已经 drain 共享内存队列或已经读到这些字节。`SessionManager::close()` 以及 listener/session
关闭路径是 hard close。如果应用要求最后一帧在关闭前一定被对端读到，需要在关闭 session 前做应用层
ack 或 completion barrier。
