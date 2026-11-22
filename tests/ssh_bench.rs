mod lab;

use std::time::{Duration, Instant};

use lab::{LinkParams, Machine, Term, World};

fn set_symmetric_link(
    world: &mut World,
    a: &Machine,
    b: &Machine,
    rtt_ms: u32,
    rate_kbit: Option<u32>,
) {
    let half = rtt_ms / 2;
    let rest = rtt_ms - half;
    world.set_link(
        a,
        LinkParams {
            delay_ms: half,
            rate_kbit,
            up: true,
        },
    );
    world.set_link(
        b,
        LinkParams {
            delay_ms: rest,
            rate_kbit,
            up: true,
        },
    );
}

fn open_direct_harp(laptop: &Machine) -> Term {
    let mut t = laptop.term("ssh", &["-t", "devbox", "harp"], (120, 35));
    t.wait_for(
        |s| s.contains("MANAGING") && s.contains('#'),
        Duration::from_secs(15),
    );
    t.send("\r");
    t.wait_for(
        |s| s.contains("INTERACTING") && s.contains('#'),
        Duration::from_secs(5),
    );
    t
}

fn open_tunnel_new_scope(laptop: &Machine) -> Term {
    let mut t = laptop.harp(&["--ssh", "devbox"], (120, 35));
    t.wait_for(|s| s.contains("New Scope on devbox"), Duration::from_secs(15));
    // Move past "+ New Scope" to "+ New Scope on devbox"
    t.send("\x1b[B\r");
    t.wait_for(
        |s| s.contains("MANAGING") && s.contains('#'),
        Duration::from_secs(15),
    );
    t.send("\r");
    t.wait_for(
        |s| s.contains("INTERACTING") && s.contains('#'),
        Duration::from_secs(5),
    );
    t
}

fn start_raw_cat(term: &mut Term) {
    term.send("stty raw -echo; cat\r");
    std::thread::sleep(Duration::from_millis(120));
}

fn sample_echo_latencies(term: &mut Term, tag: &str, count: usize) -> Vec<f64> {
    // 5 warmup samples so TCP slow-start / SSH control master settle
    for w in 0..5 {
        let marker = format!("W{tag}_{w}Z");
        term.send(&format!("{marker}\r\n"));
        term.wait_for(|s| s.contains(&marker), Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(15));
    }

    let mut samples = Vec::with_capacity(count);
    for i in 0..count {
        let marker = format!("M{tag}_{i:02}Z");
        let payload = format!("{marker}\r\n");
        term.send(&payload);
        let elapsed = term.wait_for(|s| s.contains(&marker), Duration::from_secs(5));
        samples.push(elapsed.as_secs_f64() * 1000.0);
        // Pace slightly above 16ms so each keystroke starts from idle
        std::thread::sleep(Duration::from_millis(22));
    }
    samples.sort_by(f64::total_cmp);
    samples
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    let idx = ((sorted.len().saturating_sub(1)) as f64 * q).round() as usize;
    sorted[idx]
}

#[test]
#[ignore = "benchmark: run ./tests/run-bench.ysh"]
fn bench_connect_and_attach_latency() {
    println!("\n=== bench_connect_and_attach_latency ===");
    for rtt_ms in [0u32, 30, 80] {
        let mut world = World::new();
        let laptop = world.machine("laptop");
        let devbox = world.machine("devbox");
        set_symmetric_link(&mut world, &laptop, &devbox, rtt_ms, None);

        // 1. Cold connect: harp --ssh devbox -> switcher shows "New Scope on devbox"
        let t0 = Instant::now();
        let mut t = laptop.harp(&["--ssh", "devbox"], (120, 35));
        t.wait_for(|s| s.contains("New Scope on devbox"), Duration::from_secs(15));
        let cold_connect_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // 2. Create remote scope on devbox
        let t1 = Instant::now();
        t.send("\x1b[B\r");
        t.wait_for(
            |s| s.contains("MANAGING") && s.contains('#'),
            Duration::from_secs(15),
        );
        let create_scope_ms = t1.elapsed().as_secs_f64() * 1000.0;
        t.detach_harp();
        drop(t);

        // 3. Warm reconnect + attach existing remote scope
        let t2 = Instant::now();
        let mut t_warm = laptop.harp(&["--ssh", "devbox"], (120, 35));
        t_warm.wait_for(|s| s.contains("@devbox"), Duration::from_secs(15));
        let warm_list_ms = t2.elapsed().as_secs_f64() * 1000.0;

        let t3 = Instant::now();
        t_warm.send("\r");
        t_warm.wait_for(
            |s| {
                !s.contains("Resume or Create Scope")
                    && !s.contains("Switch Scope")
                    && s.contains("c: copy selection")
            },
            Duration::from_secs(15),
        );
        let attach_ms = t3.elapsed().as_secs_f64() * 1000.0;

        println!(
            "rtt={rtt_ms:>3}ms | cold_connect={cold_connect_ms:6.1}ms | create_scope={create_scope_ms:6.1}ms | warm_list={warm_list_ms:6.1}ms | attach={attach_ms:6.1}ms"
        );
    }
}

#[test]
#[ignore = "benchmark: run ./tests/run-bench.ysh"]
fn bench_keystroke_rtt_sweep() {
    println!("\n=== bench_keystroke_rtt_sweep ===");
    let mut world = World::new();
    let laptop = world.machine("laptop");
    let devbox = world.machine("devbox");

    let mut direct = open_direct_harp(&laptop);
    start_raw_cat(&mut direct);

    let mut tunnel = open_tunnel_new_scope(&laptop);
    start_raw_cat(&mut tunnel);

    let mut direct_p50s = Vec::new();
    let mut tunnel_p50s = Vec::new();
    let rtts = [0u32, 20, 50, 100];

    for &rtt_ms in &rtts {
        set_symmetric_link(&mut world, &laptop, &devbox, rtt_ms, None);
        let tag_a = format!("A{rtt_ms}");
        let tag_b = format!("B{rtt_ms}");
        let direct_ms = sample_echo_latencies(&mut direct, &tag_a, 30);
        let tunnel_ms = sample_echo_latencies(&mut tunnel, &tag_b, 30);
        let d_p50 = percentile(&direct_ms, 0.50);
        let t_p50 = percentile(&tunnel_ms, 0.50);
        direct_p50s.push(d_p50);
        tunnel_p50s.push(t_p50);

        println!(
            "rtt={rtt_ms:>3}ms | ssh+harp p50={:6.1}ms p95={:6.1}ms p99={:6.1}ms | tunnel p50={:6.1}ms p95={:6.1}ms p99={:6.1}ms",
            d_p50,
            percentile(&direct_ms, 0.95),
            percentile(&direct_ms, 0.99),
            t_p50,
            percentile(&tunnel_ms, 0.95),
            percentile(&tunnel_ms, 0.99),
        );
    }

    let delta_rtt = (rtts[3] - rtts[0]) as f64;
    let direct_slope = (direct_p50s[3] - direct_p50s[0]) / delta_rtt;
    let tunnel_slope = (tunnel_p50s[3] - tunnel_p50s[0]) / delta_rtt;
    println!(
        "slope (d_p50 / d_RTT): ssh+harp={direct_slope:.2}x | tunnel={tunnel_slope:.2}x"
    );
}

#[test]
#[ignore = "benchmark: run ./tests/run-bench.ysh"]
fn bench_burst_typing_latency() {
    println!("\n=== bench_burst_typing_latency (rtt=50ms, 20 chars @ 10ms) ===");
    let mut world = World::new();
    let laptop = world.machine("laptop");
    let devbox = world.machine("devbox");
    set_symmetric_link(&mut world, &laptop, &devbox, 50, None);

    let mut direct = open_direct_harp(&laptop);
    start_raw_cat(&mut direct);

    let mut tunnel = open_tunnel_new_scope(&laptop);
    start_raw_cat(&mut tunnel);

    for (label, term) in [("ssh+harp", &mut direct), ("tunnel  ", &mut tunnel)] {
        let mut burst_ms = Vec::new();
        for b in 0..10 {
            let prefix = format!("B{b}_");
            let end_marker = format!("E{b}Z");
            let start = Instant::now();
            for ch in prefix.chars() {
                term.send(&ch.to_string());
                std::thread::sleep(Duration::from_millis(10));
            }
            for i in 0..10 {
                term.send(&format!("{i}"));
                std::thread::sleep(Duration::from_millis(10));
            }
            term.send(&format!("{end_marker}\r\n"));
            term.wait_for(|s| s.contains(&end_marker), Duration::from_secs(5));
            burst_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            std::thread::sleep(Duration::from_millis(50));
        }
        burst_ms.sort_by(f64::total_cmp);
        println!(
            "{label} | burst_total p50={:6.1}ms p95={:6.1}ms",
            percentile(&burst_ms, 0.50),
            percentile(&burst_ms, 0.95),
        );
    }
}

#[test]
#[ignore = "benchmark: run ./tests/run-bench.ysh"]
fn bench_scrollback_bloat_latency() {
    println!("\n=== bench_scrollback_bloat_latency (rtt=40ms, rate=5mbit) ===");
    let mut world = World::new();
    let laptop = world.machine("laptop");
    let devbox = world.machine("devbox");
    set_symmetric_link(&mut world, &laptop, &devbox, 40, Some(5_000));

    let mut direct = open_direct_harp(&laptop);
    let mut tunnel = open_tunnel_new_scope(&laptop);

    for (label, term) in [("ssh+harp", &mut direct), ("tunnel  ", &mut tunnel)] {
        start_raw_cat(term);
        let clean = sample_echo_latencies(term, "C", 15);
        // Exit raw cat and generate 2000 lines of scrollback
        term.send("\x03");
        std::thread::sleep(Duration::from_millis(100));
        term.send("seq 1 2000; echo SB_DONE\r");
        term.wait_for(|s| s.contains("SB_DONE"), Duration::from_secs(15));

        start_raw_cat(term);
        // Each marker ends with \r\n, scrolling a new line and changing the
        // scrollback hash on every response.
        let with_sb = sample_echo_latencies(term, "S", 15);

        println!(
            "{label} | 0-lines p50={:6.1}ms p95={:6.1}ms | 2000-lines(+scroll) p50={:6.1}ms p95={:6.1}ms",
            percentile(&clean, 0.50),
            percentile(&clean, 0.95),
            percentile(&with_sb, 0.50),
            percentile(&with_sb, 0.95),
        );
    }
}
