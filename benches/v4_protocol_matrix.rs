// Copyright 2025 CloudWeGo Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    os::unix::net::SocketAddr,
    time::{Duration, Instant},
};

use shmipc::{
    Error, Listener,
    buffer::{BufferReader, BufferSlice, LinkedBuffer},
    compact::StreamExt,
    config::{Config, SizePercentPair},
    consts::MemMapType,
    session::{SessionManager, SessionManagerConfig},
    stats::StatsSnapshot,
    stream::Stream,
    transport::{DefaultUnixConnect, DefaultUnixListen},
};
use tokio::io::AsyncReadExt;

const DEFAULT_ITERS: usize = 5_000;
const DEFAULT_WARMUP_ITERS: usize = 500;
const DEFAULT_PAYLOAD_BYTES: u32 = 64;
const POLLING_INTERVAL: Duration = Duration::from_micros(100);

#[derive(Clone, Copy)]
enum ReadPath {
    Discard,
    StreamExt,
}

impl ReadPath {
    const fn name(self) -> &'static str {
        match self {
            Self::Discard => "discard",
            Self::StreamExt => "stream-ext",
        }
    }
}

#[derive(Clone, Copy)]
struct BenchMode {
    name: &'static str,
    configure: fn(Config) -> Config,
}

#[derive(Clone, Debug)]
struct BenchResult {
    mode: &'static str,
    iters: usize,
    payload_bytes: u32,
    wall: Duration,
    cpu: Duration,
    p50: Duration,
    p99: Duration,
    stats: StatsSnapshot,
}

fn main() {
    let iters = env_usize("SHMIPC_PERF_ITERS", DEFAULT_ITERS);
    let warmup_iters = env_usize("SHMIPC_PERF_WARMUP_ITERS", DEFAULT_WARMUP_ITERS);
    let payload_bytes = env_u32("SHMIPC_PERF_PAYLOAD_BYTES", DEFAULT_PAYLOAD_BYTES);
    let read_path = env_read_path("SHMIPC_PERF_READ_PATH");
    let slice_bytes = env_optional_u32("SHMIPC_PERF_SLICE_BYTES");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let modes = [
        BenchMode {
            name: "legacy-v3",
            configure: |config| config,
        },
        BenchMode {
            name: "v4-default",
            configure: Config::with_v4,
        },
        BenchMode {
            name: "v4-polling-100us",
            configure: |config| config.with_v4_event_queue_polling(POLLING_INTERVAL),
        },
        BenchMode {
            name: "v4-eventfd",
            configure: Config::with_v4_eventfd,
        },
    ];
    let modes = match env_modes("SHMIPC_PERF_MODES") {
        Some(selected_modes) => selected_modes
            .iter()
            .filter_map(|selected| modes.iter().find(|mode| mode.name == selected).copied())
            .collect::<Vec<_>>(),
        None => modes.to_vec(),
    };

    let mut results = Vec::with_capacity(modes.len());
    for mode in modes {
        results.push(runtime.block_on(run_mode(
            mode,
            iters,
            warmup_iters,
            payload_bytes,
            read_path,
            slice_bytes,
        )));
    }

    print_results(&results, warmup_iters, read_path, slice_bytes);
}

async fn run_mode(
    mode: BenchMode,
    iters: usize,
    warmup_iters: usize,
    payload_bytes: u32,
    read_path: ReadPath,
    slice_bytes: Option<u32>,
) -> BenchResult {
    let rand = rand::random::<u64>();
    let socket_path = format!("/dev/shm/shmipc-perf-{}-{rand}.sock", mode.name);
    let mut sm_config = benchmark_config(
        (mode.configure)(Config::default()),
        payload_bytes,
        slice_bytes,
    );
    sm_config
        .config_mut()
        .share_memory_path_prefix
        .push_str(&format!("-{}-{rand}", mode.name));
    sm_config = sm_config.with_session_num(1);

    let mut server = Listener::new(
        DefaultUnixListen,
        SocketAddr::from_pathname(socket_path.clone()).unwrap(),
        sm_config.config().clone(),
    )
    .await
    .unwrap();

    let server_task = tokio::spawn(async move {
        let stream = server.accept().await.unwrap();
        match read_path {
            ReadPath::Discard => serve_discard(stream, payload_bytes).await,
            ReadPath::StreamExt => serve_stream_ext(stream, payload_bytes).await,
        }
        server.close().await;
    });

    let client = SessionManager::new(
        sm_config,
        DefaultUnixConnect,
        SocketAddr::from_pathname(socket_path).unwrap(),
    )
    .await
    .unwrap();
    let mut stream = BenchClient::new(client.get_stream().unwrap(), read_path, payload_bytes);

    for _ in 0..warmup_iters {
        stream.round_trip(payload_bytes).await;
    }

    let stats_before = client.stats_snapshot();
    let cpu_before = process_cpu_time();
    let wall_start = Instant::now();
    let mut latencies = Vec::with_capacity(iters);
    for _ in 0..iters {
        let op_start = Instant::now();
        stream.round_trip(payload_bytes).await;
        latencies.push(op_start.elapsed());
    }
    let wall = wall_start.elapsed();
    let cpu = process_cpu_time().saturating_sub(cpu_before);
    let stats = client.stats_snapshot().saturating_sub(stats_before);

    stream.close().await;
    drop(stream);
    client.close().await;
    server_task.await.unwrap();

    latencies.sort_unstable();

    BenchResult {
        mode: mode.name,
        iters,
        payload_bytes,
        wall,
        cpu,
        p50: percentile(&latencies, 50),
        p99: percentile(&latencies, 99),
        stats,
    }
}

enum BenchClient {
    Discard(Stream),
    StreamExt {
        stream: StreamExt,
        read_buf: Vec<u8>,
    },
}

impl BenchClient {
    fn new(stream: Stream, read_path: ReadPath, payload_bytes: u32) -> Self {
        match read_path {
            ReadPath::Discard => Self::Discard(stream),
            ReadPath::StreamExt => Self::StreamExt {
                stream: StreamExt::new(stream),
                read_buf: vec![0; payload_bytes as usize],
            },
        }
    }

    async fn round_trip(&mut self, payload_bytes: u32) {
        match self {
            Self::Discard(stream) => {
                must_write(stream, payload_bytes).await;
                assert!(must_read(stream, payload_bytes).await);
                stream.release_read_and_reuse();
            }
            Self::StreamExt { stream, read_buf } => {
                must_write(stream.inner_mut(), payload_bytes).await;
                assert!(must_read_stream_ext(stream, read_buf).await);
                stream.inner().release_read_and_reuse();
            }
        }
    }

    async fn close(&mut self) {
        match self {
            Self::Discard(stream) => stream.close().await.unwrap(),
            Self::StreamExt { stream, .. } => stream.inner_mut().close().await.unwrap(),
        }
    }
}

async fn serve_discard(mut stream: Stream, payload_bytes: u32) {
    while must_read(&mut stream, payload_bytes).await {
        stream.recv_buf().release_previous_read();
        must_write(&mut stream, payload_bytes).await;
    }
}

async fn serve_stream_ext(stream: Stream, payload_bytes: u32) {
    let mut stream = StreamExt::new(stream);
    let mut read_buf = vec![0; payload_bytes as usize];
    while must_read_stream_ext(&mut stream, &mut read_buf).await {
        stream.inner().recv_buf().release_previous_read();
        must_write(stream.inner_mut(), payload_bytes).await;
    }
}

fn benchmark_config(
    config: Config,
    payload_bytes: u32,
    slice_bytes: Option<u32>,
) -> SessionManagerConfig {
    let buffer_slice_sizes = match slice_bytes {
        Some(size) => vec![SizePercentPair {
            size: size
                .checked_add(256)
                .expect("SHMIPC_PERF_SLICE_BYTES is too large"),
            percent: 100,
        }],
        None => vec![
            SizePercentPair {
                size: payload_bytes + 256,
                percent: 70,
            },
            SizePercentPair {
                size: (16 << 10) + 256,
                percent: 20,
            },
            SizePercentPair {
                size: (64 << 10) + 256,
                percent: 10,
            },
        ],
    };
    let mut c = SessionManagerConfig::new().with_config(Config {
        mem_map_type: MemMapType::MemMapTypeMemFd,
        queue_cap: 65536,
        connection_write_timeout: Duration::from_secs(1),
        share_memory_buffer_cap: 256 << 20,
        buffer_slice_sizes,
        ..config
    });
    c.config_mut().verify().unwrap();
    c
}

async fn must_write(s: &mut Stream, size: u32) {
    write_empty_buffer(s.send_buf(), size);
    loop {
        match s.flush(false).await {
            Err(Error::QueueFull) => {
                tokio::time::sleep(Duration::from_micros(1)).await;
            }
            Err(err) => {
                panic!("must write err:{err}");
            }
            Ok(_) => return,
        }
    }
}

fn write_empty_buffer(l: &mut LinkedBuffer, size: u32) {
    if size == 0 {
        return;
    }
    let mut wrote = 0;
    loop {
        if l.slice_list().write_slice.is_none() {
            l.alloc(size - wrote);
            l.slice_list_mut().write_slice = l.slice_list().front_slice;
        }
        wrote += write_empty_slice(l.slice_list().write_mut().unwrap(), size - wrote);
        if wrote < size {
            if l.slice_list().write().unwrap().next().is_none() {
                l.alloc(size - wrote);
            }
            l.slice_list_mut().write_slice = l.slice_list().write().unwrap().next_slice;
        } else {
            break;
        }
    }
    *l.len_mut() += size as usize;
}

fn write_empty_slice(slice: &mut BufferSlice, size: u32) -> u32 {
    slice.write_index += size as usize;
    if slice.write_index > slice.cap as usize {
        let wrote = slice.cap - (slice.write_index as u32 - size);
        slice.write_index = slice.cap as usize;
        return wrote;
    }
    size
}

async fn must_read(s: &mut Stream, size: u32) -> bool {
    match s.discard(size as usize).await {
        Ok(_) => true,
        Err(Error::StreamClosed | Error::EndOfStream) => false,
        Err(err) => panic!("must read err:{err}"),
    }
}

async fn must_read_stream_ext(stream: &mut StreamExt, buf: &mut [u8]) -> bool {
    match stream.read_exact(buf).await {
        Ok(_) => true,
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => false,
        Err(err) => panic!("must read err:{err}"),
    }
}

fn percentile(sorted: &[Duration], percentile: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() * percentile).div_ceil(100)).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_optional_u32(name: &str) -> Option<u32> {
    std::env::var(name)
        .ok()
        .map(|value| {
            value
                .parse()
                .unwrap_or_else(|_| panic!("{name} must be a positive u32"))
        })
        .inspect(|&value| {
            assert!(value > 0, "{name} must be greater than zero");
        })
}

fn env_read_path(name: &str) -> ReadPath {
    match std::env::var(name).as_deref() {
        Ok("stream-ext") => ReadPath::StreamExt,
        Ok("discard") | Err(_) => ReadPath::Discard,
        Ok(value) => panic!("{name} must be either discard or stream-ext, got {value}"),
    }
}

fn env_modes(name: &str) -> Option<Vec<String>> {
    std::env::var(name).ok().map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|mode| !mode.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

fn process_cpu_time() -> Duration {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the provided rusage pointer on success.
    let ret = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    assert_eq!(ret, 0, "getrusage failed");
    // SAFETY: ret == 0 means usage was initialized by getrusage.
    let usage = unsafe { usage.assume_init() };
    timeval_to_duration(usage.ru_utime) + timeval_to_duration(usage.ru_stime)
}

fn timeval_to_duration(time: libc::timeval) -> Duration {
    Duration::from_secs(time.tv_sec as u64) + Duration::from_micros(time.tv_usec as u64)
}

fn print_results(
    results: &[BenchResult],
    warmup_iters: usize,
    read_path: ReadPath,
    slice_bytes: Option<u32>,
) {
    println!("# shmipc V4 protocol matrix benchmark");
    println!();
    println!(
        "warmup_iters={} measured_iters={} payload_bytes={} read_path={} slice_bytes={}",
        warmup_iters,
        results[0].iters,
        results[0].payload_bytes,
        read_path.name(),
        slice_bytes
            .map(|value| value.to_string())
            .unwrap_or_else(|| "default".to_owned())
    );
    println!();
    println!(
        "| mode | ops/s | MiB/s | p50 us | p99 us | wall ms | cpu ms | cpu/wall | send polling | \
         recv polling | out bytes | in bytes |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");

    for result in results {
        let wall_secs = result.wall.as_secs_f64();
        let ops_per_sec = result.iters as f64 / wall_secs;
        let mib_per_sec =
            (result.iters as f64 * result.payload_bytes as f64 * 2.0) / wall_secs / 1024.0 / 1024.0;
        let cpu_wall = result.cpu.as_secs_f64() / wall_secs;
        println!(
            "| {} | {:.0} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {} | {} | {} | {} |",
            result.mode,
            ops_per_sec,
            mib_per_sec,
            result.p50.as_secs_f64() * 1_000_000.0,
            result.p99.as_secs_f64() * 1_000_000.0,
            result.wall.as_secs_f64() * 1_000.0,
            result.cpu.as_secs_f64() * 1_000.0,
            cpu_wall,
            result.stats.send_polling_event_count,
            result.stats.recv_polling_event_count,
            result.stats.out_flow_bytes,
            result.stats.in_flow_bytes,
        );
    }

    if let Some(legacy) = results.iter().find(|result| result.mode == "legacy-v3") {
        println!();
        println!("## Relative to legacy-v3");
        println!();
        println!("| mode | ops/s delta | p99 delta |");
        println!("|---|---:|---:|");
        let legacy_ops = legacy.iters as f64 / legacy.wall.as_secs_f64();
        let legacy_p99 = legacy.p99.as_secs_f64();
        for result in results.iter().filter(|result| result.mode != "legacy-v3") {
            let ops = result.iters as f64 / result.wall.as_secs_f64();
            let ops_delta = (ops / legacy_ops - 1.0) * 100.0;
            let p99_delta = (result.p99.as_secs_f64() / legacy_p99 - 1.0) * 100.0;
            println!(
                "| {} | {:+.2}% | {:+.2}% |",
                result.mode, ops_delta, p99_delta
            );
        }
    }
}
