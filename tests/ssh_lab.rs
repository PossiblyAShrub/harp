mod lab;

use std::{process::Command, time::Duration};

use lab::{LinkParams, World};

#[test]
#[ignore = "requires rootless user+net namespaces (run tests/run-lab.ysh)"]
fn namespace_cleanup_leaves_no_leaks() {
    let mut world = World::new();
    let devbox = world.machine("devbox");
    let out = devbox.exec("uname", &["-n"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "devbox");

    let inode = world.netns_inode();
    drop(world);
    std::thread::sleep(Duration::from_millis(100));

    let lsns = Command::new("lsns")
        .args(["-t", "net", "-n", "-o", "NS"])
        .output()
        .expect("run lsns");
    let listing = String::from_utf8_lossy(&lsns.stdout);
    assert!(
        !listing.lines().any(|line| line.trim() == inode),
        "netns {inode} leaked after World::drop:\n{listing}"
    );
}

#[test]
#[ignore = "requires rootless user+net namespaces (run tests/run-lab.ysh)"]
fn ssh_remote_scope_create_echo_and_persist() {
    let mut world = World::new();
    let laptop = world.machine("laptop");
    let _devbox = world.machine("devbox");

    // 1. Connect laptop -> devbox via harp --ssh devbox
    let mut t = laptop.harp(&["--ssh", "devbox"], (120, 35));
    t.wait_for(|s| s.contains("New Scope on devbox"), Duration::from_secs(10));

    // Move selection in switcher to "+ New Scope on devbox" and press Enter.
    // Switcher items with 0 scopes: [0] + New Scope, [1] + New Scope on devbox.
    t.send("\x1b[B\r");
    // Wait until the remote scope is actually attached before switching to Interacting.
    t.wait_for(|s| s.contains("MANAGING") && s.contains("scope-1"), Duration::from_secs(10));
    t.send("\r");
    t.wait_for(|s| s.contains("INTERACTING") && s.contains('#'), Duration::from_secs(5));

    t.send("echo lab-host-$(uname -n)\r");
    t.wait_for(|s| s.contains("lab-host-devbox"), Duration::from_secs(5));
    t.detach_harp();
    drop(t);

    // 2. Reconnect and confirm the remote scope is still alive on devbox
    let t2 = laptop.harp(&["--ssh", "devbox"], (120, 35));
    t2.wait_for(
        |s| s.contains("@devbox") && s.contains("1 panes"),
        Duration::from_secs(10),
    );
}

#[test]
#[ignore = "requires rootless user+net namespaces (run tests/run-lab.ysh)"]
fn ssh_uploads_binary_when_remote_lacks_harp() {
    let mut world = World::new();
    let laptop = world.machine("laptop");
    let _nobox = world.machine_without_harp("nobox");

    let mut t = laptop.harp(&["--ssh", "nobox"], (120, 35));
    t.wait_for(|s| s.contains("New Scope on nobox"), Duration::from_secs(10));
    t.send("\x1b[B\r");
    t.wait_for(|s| s.contains("MANAGING") && s.contains("scope-1"), Duration::from_secs(15));
    t.send("\r");
    t.wait_for(|s| s.contains("INTERACTING") && s.contains('#'), Duration::from_secs(5));
    t.send("echo uploaded-to-$(uname -n)\r");
    t.wait_for(|s| s.contains("uploaded-to-nobox"), Duration::from_secs(5));
}

#[test]
#[ignore = "requires rootless user+net namespaces (run tests/run-lab.ysh)"]
fn benchmark_keystroke_latency_ssh_vs_tunnel() {
    let mut world = World::new();
    let laptop = world.machine("laptop");
    let devbox = world.machine("devbox");

    // Path A: ssh devbox -t harp
    let mut direct = laptop.term("ssh", &["-t", "devbox", "harp"], (120, 35));
    direct.wait_for(|s| s.contains("MANAGING") && s.contains('#'), Duration::from_secs(10));
    direct.send("\r");
    direct.wait_for(|s| s.contains("INTERACTING") && s.contains('#'), Duration::from_secs(5));
    direct.send("stty raw -echo; cat\r");
    std::thread::sleep(Duration::from_millis(100));

    // Path B: local harp --ssh devbox
    let mut tunnel = laptop.harp(&["--ssh", "devbox"], (120, 35));
    tunnel.wait_for(|s| s.contains("New Scope on devbox"), Duration::from_secs(10));
    tunnel.send("\x1b[B\r");
    tunnel.wait_for(|s| s.contains("MANAGING") && s.contains('#'), Duration::from_secs(10));
    tunnel.send("\r");
    tunnel.wait_for(|s| s.contains("INTERACTING") && s.contains('#'), Duration::from_secs(5));
    tunnel.send("stty raw -echo; cat\r");
    std::thread::sleep(Duration::from_millis(100));

    for one_way_ms in [0u32, 15, 40] {
        world.set_link(
            &devbox,
            LinkParams {
                delay_ms: one_way_ms,
                up: true,
            },
        );
        let direct_ms = sample_echo_latencies(&mut direct, "A", one_way_ms, 20);
        let tunnel_ms = sample_echo_latencies(&mut tunnel, "B", one_way_ms, 20);
        println!(
            "one_way={}ms | ssh+harp p50={:.1}ms p95={:.1}ms | tunnel p50={:.1}ms p95={:.1}ms",
            one_way_ms,
            percentile(&direct_ms, 0.50),
            percentile(&direct_ms, 0.95),
            percentile(&tunnel_ms, 0.50),
            percentile(&tunnel_ms, 0.95),
        );
    }
}

fn sample_echo_latencies(term: &mut lab::Term, tag: &str, rtt_tag: u32, count: usize) -> Vec<f64> {
    let mut samples = Vec::with_capacity(count);
    for i in 0..count {
        let marker = format!("M{tag}{rtt_tag}_{i:02}Z");
        let payload = format!("{marker}\r\n");
        let elapsed = {
            term.send(&payload);
            term.wait_for(|s| s.contains(&marker), Duration::from_secs(5))
        };
        samples.push(elapsed.as_secs_f64() * 1000.0);
        std::thread::sleep(Duration::from_millis(20));
    }
    samples.sort_by(f64::total_cmp);
    samples
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    let idx = ((sorted.len().saturating_sub(1)) as f64 * q).round() as usize;
    sorted[idx]
}
