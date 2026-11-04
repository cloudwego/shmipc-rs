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

use std::{os::unix::net::SocketAddr, time::Duration};

use shmipc::{
    config::Config,
    consts::MAX_BLOCK_SIZE,
    session::{SessionManager, SessionManagerConfig},
    stats::DynShmStats,
    stream::Stream,
    transport::DefaultUnixConnect,
};

const BUFFER_HEADER: usize = 24;
const ALLOCATOR_HEADER: usize = 8;
const MAX_SHARED_PAYLOAD: usize = MAX_BLOCK_SIZE - ALLOCATOR_HEADER - BUFFER_HEADER;
const PRUNE_TICK: Duration = Duration::from_millis(1500);
const ROUNDS: usize = 30;

fn used_bytes(stats: &DynShmStats) -> usize {
    stats.shared_bytes.saturating_sub(stats.shared_free_bytes) + stats.dedicated_used
}

fn total_bytes(stats: &DynShmStats) -> usize {
    stats.shared_bytes + stats.dedicated_bytes
}

/// This side's own segment, which is where everything written below is allocated from.
fn segment(stream: &Stream) -> DynShmStats {
    stream
        .v5_dynshm_stats()
        .expect("the session negotiated V5, so it has a segment")
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let dir = std::env::current_dir().unwrap();
    let uds_path = SocketAddr::from_pathname(dir.join("../ipc_test.sock")).unwrap();

    let mut config = Config::default().with_v5();
    config.share_memory_path_prefix = "/dev/shm/client.v5.default.ipc.shm".to_owned();
    let conf = SessionManagerConfig::new().with_config(config);

    let sm = SessionManager::new(conf, DefaultUnixConnect, uds_path)
        .await
        .unwrap();
    let mut stream = sm.get_stream().unwrap();

    // What the segment costs before any traffic, which is the floor everything settles back to.
    let idle = used_bytes(&segment(&stream));

    // Five routes through the segment. The middle two send the same 512 KiB and differ only in how
    // it is handed to `write_bytes`, which is the whole point: the route is decided by the size of
    // one call, not by the size of the message. The last two sit either side of the split itself.
    let small = run("one small block ", 25, 25, &mut stream, &sm).await;
    let chained = run(
        "chained blocks  ",
        512 << 10,
        MAX_SHARED_PAYLOAD,
        &mut stream,
        &sm,
    )
    .await;
    let dedicated = run("dedicated region", 512 << 10, 512 << 10, &mut stream, &sm).await;
    let at_limit = run(
        "at the limit    ",
        MAX_SHARED_PAYLOAD,
        MAX_SHARED_PAYLOAD,
        &mut stream,
        &sm,
    )
    .await;
    let over_limit = run(
        "one byte over   ",
        MAX_SHARED_PAYLOAD + 1,
        MAX_SHARED_PAYLOAD + 1,
        &mut stream,
        &sm,
    )
    .await;

    tokio::time::sleep(PRUNE_TICK).await;
    let settled = segment(&stream);

    println!();
    let mut passed = 0;
    let mut failed = 0;
    let mut check = |name: &str, ok: bool| {
        println!("  [{}] {name}", if ok { "PASS" } else { "FAIL" });
        if ok { passed += 1 } else { failed += 1 }
    };

    let mismatches = small.mismatches
        + chained.mismatches
        + dedicated.mismatches
        + at_limit.mismatches
        + over_limit.mismatches;

    check("every echo came back unchanged", mismatches == 0);
    check(
        "small payloads never left the shared area",
        small.dedicated_carved == 0,
    );
    check(
        "512 KiB in shared-sized writes never left the shared area",
        chained.dedicated_carved == 0,
    );
    check(
        "512 KiB in one write carved dedicated regions",
        dedicated.dedicated_carved > 0,
    );
    check(
        "a write of exactly the limit stayed shared",
        at_limit.dedicated_carved == 0,
    );
    check(
        "one byte over the limit went dedicated",
        over_limit.dedicated_carved > 0,
    );
    check(
        "chained replies were read across blocks",
        chained.read_copies >= ROUNDS as u64,
    );
    check(
        "whole-block replies were read without copying",
        small.read_copies == 0 && dedicated.read_copies == 0 && over_limit.read_copies == 0,
    );
    check(
        "every dedicated region was handed back",
        settled.dedicated_active == 0,
    );
    check("no release was left unreported", settled.rq_pending == 0);
    check(
        "the segment settled instead of growing with traffic",
        used_bytes(&settled) <= idle + MAX_BLOCK_SIZE,
    );

    println!(
        "\nfinal: {} B in use of {} B, idle floor {idle} B | shared {} regions, {} active | \
         dedicated {} regions, {} active | release queue pending {}",
        used_bytes(&settled),
        total_bytes(&settled),
        settled.shared_regions,
        settled.shared_active,
        settled.dedicated_regions,
        settled.dedicated_active,
        settled.rq_pending,
    );
    println!("{passed} checks passed, {failed} failed");

    sm.put_back(stream).await;
    sm.close().await;

    if failed > 0 {
        std::process::exit(1);
    }
}

struct Route {
    mismatches: usize,
    dedicated_carved: usize,
    read_copies: u64,
}

/// Sends `ROUNDS` messages of `size`, each written `chunk` bytes at a time, and checks every
/// reply against what was sent.
async fn run(
    label: &str,
    size: usize,
    chunk: usize,
    stream: &mut Stream,
    sm: &SessionManager<DefaultUnixConnect>,
) -> Route {
    let before = segment(stream);
    let before_copies = sm.stats_snapshot().v5_read_copy_count;
    let mut mismatches = 0;

    for round in 1..=ROUNDS {
        // A distinct byte per round, so a block that arrives out of order or from the wrong chain
        // is visible rather than blending into a uniform payload.
        let payload = vec![round as u8; size];

        // A length and a chunk size in front of each message: the server echoes the payload back
        // in writes of that size.
        stream
            .write_bytes(&(payload.len() as u32).to_le_bytes())
            .unwrap();
        stream.write_bytes(&(chunk as u32).to_le_bytes()).unwrap();
        for part in payload.chunks(chunk.max(1)) {
            stream.write_bytes(part).unwrap();
        }
        stream.flush(true).await.unwrap();

        let header = stream.read_exact_bytes(8).await.unwrap();
        let echoed = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        drop(header);

        let body = stream.read_exact_bytes(echoed).await.unwrap();
        if body.len() != payload.len() || body.as_ref() != payload.as_slice() {
            mismatches += 1;
        }
        drop(body);
    }

    let after = segment(stream);
    let copies = sm.stats_snapshot().v5_read_copy_count - before_copies;

    println!(
        "{label} | {ROUNDS} x {size:>7} B in {chunk:>6} B writes | dedicated regions {} -> {} | \
         reads needing a copy {copies:>2} | mismatches {mismatches}",
        before.dedicated_regions, after.dedicated_regions,
    );

    Route {
        mismatches,
        dedicated_carved: after
            .dedicated_regions
            .saturating_sub(before.dedicated_regions),
        read_copies: copies,
    }
}
