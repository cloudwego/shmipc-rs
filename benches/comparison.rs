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

use rand::seq::SliceRandom;
use shmipc::{
    Error, Listener,
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
const DEFAULT_ROUNDS: usize = 5;
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
    let rounds = env_usize("SHMIPC_PERF_ROUNDS", DEFAULT_ROUNDS);
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
        BenchMode {
            name: "v5-default",
            configure: Config::with_v5,
        },
        BenchMode {
            name: "v5-polling-100us",
            configure: |config| config.with_v5_event_queue_polling(POLLING_INTERVAL),
        },
        BenchMode {
            name: "v5-eventfd",
            configure: Config::with_v5_eventfd,
        },
    ];
    let modes = match env_modes("SHMIPC_PERF_MODES") {
        Some(selected_modes) => selected_modes
            .iter()
            .filter_map(|selected| modes.iter().find(|mode| mode.name == selected).copied())
            .collect::<Vec<_>>(),
        None => modes.to_vec(),
    };

    let mut modes = modes;
    let mut rng = rand::rng();

    let mut by_mode = modes
        .iter()
        .map(|mode| (mode.name, Vec::with_capacity(rounds)))
        .collect::<Vec<(&'static str, Vec<BenchResult>)>>();

    for _ in 0..rounds {
        modes.shuffle(&mut rng);
        for mode in &modes {
            let result = runtime.block_on(run_mode(
                *mode,
                iters,
                warmup_iters,
                payload_bytes,
                read_path,
                slice_bytes,
            ));
            let bucket = by_mode
                .iter_mut()
                .find(|(name, _)| *name == mode.name)
                .expect("every mode was given a bucket");
            bucket.1.push(result);
            // Idle time
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    let results = by_mode
        .iter()
        .map(|(_, rounds)| median_round_by_ops(rounds))
        .collect::<Vec<_>>();

    print_results(&results, warmup_iters, read_path, slice_bytes, rounds);
    if rounds > 1 {
        print_spread(&by_mode);
    }
}

/// The middle round by throughput, taken whole so its wall time, CPU time and counters all
/// describe one run rather than being averaged across runs that never happened together.
fn median_round_by_ops(results: &[BenchResult]) -> BenchResult {
    let mut sorted = results.to_vec();
    sorted.sort_by(|left, right| ops(left).total_cmp(&ops(right)));
    sorted.swap_remove(sorted.len() / 2)
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
        stream.release_read_and_reuse();
        must_write(&mut stream, payload_bytes).await;
    }
}

async fn serve_stream_ext(stream: Stream, payload_bytes: u32) {
    let mut stream = StreamExt::new(stream);
    let mut read_buf = vec![0; payload_bytes as usize];
    while must_read_stream_ext(&mut stream, &mut read_buf).await {
        stream.inner().release_read_and_reuse();
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
        connection_write_timeout: Duration::from_secs(1),
        share_memory_buffer_cap: 256 << 20,
        buffer_slice_sizes,
        ..config
    });
    c.config_mut().verify().unwrap();
    c
}

/// A flush the queue refuses drops the message, so it is written again before the next try.
async fn must_write(s: &mut Stream, size: u32) {
    loop {
        if size > 0 {
            s.reserve(size as usize).expect("reserve payload");
        }
        match s.flush(false).await {
            Ok(()) => return,
            Err(Error::QueueFull) => tokio::time::sleep(Duration::from_micros(1)).await,
            Err(err) => panic!("must write err:{err}"),
        }
    }
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
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionAborted
            ) =>
        {
            false
        }
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

fn print_table(header: &[&str], rows: &[Vec<String>]) {
    let widths = (0..header.len())
        .map(|column| {
            rows.iter()
                .map(|row| row[column].len())
                .chain([header[column].len(), 3])
                .max()
                .expect("the chain always yields the header width")
        })
        .collect::<Vec<_>>();

    let line = |cells: &[String]| {
        let mut out = String::from("|");
        for (column, cell) in cells.iter().enumerate() {
            let width = widths[column];
            if column == 0 {
                out.push_str(&format!(" {cell:<width$} |"));
            } else {
                out.push_str(&format!(" {cell:>width$} |"));
            }
        }
        out
    };

    let header_cells = header.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>();
    println!("{}", line(&header_cells));

    let mut separator = String::from("|");
    for (column, width) in widths.iter().enumerate() {
        if column == 0 {
            separator.push_str(&format!("{}|", "-".repeat(width + 2)));
        } else {
            separator.push_str(&format!("{}:|", "-".repeat(width + 1)));
        }
    }
    println!("{separator}");

    for row in rows {
        println!("{}", line(row));
    }
}

fn print_results(
    results: &[BenchResult],
    warmup_iters: usize,
    read_path: ReadPath,
    slice_bytes: Option<u32>,
    rounds: usize,
) {
    println!("# shmipc V4/V5 protocol matrix benchmark");
    println!();
    println!(
        "rounds={} warmup_iters={} measured_iters={} payload_bytes={} read_path={} slice_bytes={}",
        rounds,
        warmup_iters,
        results[0].iters,
        results[0].payload_bytes,
        read_path.name(),
        slice_bytes
            .map(|value| value.to_string())
            .unwrap_or_else(|| "default".to_owned()),
    );
    println!();

    let rows = results
        .iter()
        .map(|result| {
            let wall_secs = result.wall.as_secs_f64();
            let mib_per_sec = (result.iters as f64 * result.payload_bytes as f64 * 2.0)
                / wall_secs
                / 1024.0
                / 1024.0;
            vec![
                result.mode.to_owned(),
                format!("{:.0}", ops(result)),
                format!("{mib_per_sec:.2}"),
                format!("{:.2}", result.p50.as_secs_f64() * 1_000_000.0),
                format!("{:.2}", result.p99.as_secs_f64() * 1_000_000.0),
                format!("{:.2}", wall_secs * 1_000.0),
                format!("{:.2}", result.cpu.as_secs_f64() * 1_000.0),
                format!("{:.2}", result.cpu.as_secs_f64() / wall_secs),
                result.stats.send_polling_event_count.to_string(),
                result.stats.recv_polling_event_count.to_string(),
                result.stats.out_flow_bytes.to_string(),
                result.stats.in_flow_bytes.to_string(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &[
            "mode",
            "ops/s",
            "MiB/s",
            "p50 us",
            "p99 us",
            "wall ms",
            "cpu ms",
            "cpu/wall",
            "send polling",
            "recv polling",
            "out bytes",
            "in bytes",
        ],
        &rows,
    );

    if let Some(legacy) = results.iter().find(|result| result.mode == "legacy-v3") {
        let legacy_ops = ops(legacy);
        let legacy_p99 = legacy.p99.as_secs_f64();
        let rows = results
            .iter()
            .filter(|result| result.mode != "legacy-v3")
            .map(|result| {
                vec![
                    result.mode.to_owned(),
                    format!("{:+.2}%", (ops(result) / legacy_ops - 1.0) * 100.0),
                    format!(
                        "{:+.2}%",
                        (result.p99.as_secs_f64() / legacy_p99 - 1.0) * 100.0
                    ),
                ]
            })
            .collect::<Vec<_>>();
        println!();
        println!("## Relative to legacy-v3");
        println!();
        print_table(&["mode", "ops/s delta", "p99 delta"], &rows);
    }

    let pairs = [
        ("v4-default", "v5-default"),
        ("v4-polling-100us", "v5-polling-100us"),
        ("v4-eventfd", "v5-eventfd"),
    ];
    let rows = pairs
        .iter()
        .filter_map(|(v4_name, v5_name)| {
            let v4 = results.iter().find(|result| result.mode == *v4_name)?;
            let v5 = results.iter().find(|result| result.mode == *v5_name)?;
            Some(vec![
                v4_name.trim_start_matches("v4-").to_owned(),
                format!("{:.0}", ops(v4)),
                format!("{:.0}", ops(v5)),
                format!("{:+.2}%", (ops(v5) / ops(v4) - 1.0) * 100.0),
                format!(
                    "{:+.2}%",
                    (v5.p99.as_secs_f64() / v4.p99.as_secs_f64() - 1.0) * 100.0
                ),
            ])
        })
        .collect::<Vec<_>>();
    if !rows.is_empty() {
        println!();
        println!("## V5 relative to V4 (same wakeup mode)");
        println!();
        print_table(
            &["wakeup", "v4 ops/s", "v5 ops/s", "ops/s delta", "p99 delta"],
            &rows,
        );
    }
}

/// How far each mode moved between rounds, which is what says whether a delta in the tables above
/// is a real difference or just the noise floor of this machine.
fn print_spread(by_mode: &[(&'static str, Vec<BenchResult>)]) {
    let rows = by_mode
        .iter()
        .filter(|(_, results)| !results.is_empty())
        .map(|(mode, results)| {
            let mut throughput = results.iter().map(ops).collect::<Vec<_>>();
            throughput.sort_by(f64::total_cmp);
            let min = throughput[0];
            let median = throughput[throughput.len() / 2];
            let max = throughput[throughput.len() - 1];
            vec![
                (*mode).to_owned(),
                format!("{min:.0}"),
                format!("{median:.0}"),
                format!("{max:.0}"),
                format!("{:.2}%", (max / min - 1.0) * 100.0),
            ]
        })
        .collect::<Vec<_>>();

    println!();
    println!("## Spread across rounds");
    println!();
    print_table(
        &["mode", "min ops/s", "median ops/s", "max ops/s", "spread"],
        &rows,
    );
}

fn ops(result: &BenchResult) -> f64 {
    result.iters as f64 / result.wall.as_secs_f64()
}
