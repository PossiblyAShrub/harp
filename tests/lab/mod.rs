use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tui_term::vt100;

pub struct LinkParams {
    pub delay_ms: u32,
    pub up: bool,
}

impl Default for LinkParams {
    fn default() -> Self {
        Self {
            delay_ms: 0,
            up: true,
        }
    }
}

/// An isolated multi-machine Linux namespace lab.
///
/// Dropping `World` kills the root `unshare` process, which (via `--pid
/// --kill-child` and `PR_SET_PDEATHSIG`) terminates every machine, `sshd`,
/// `harp` server, and tears down all namespaces, veth pairs, and qdiscs.
pub struct World {
    dir: PathBuf,
    unshare: Child,
    world_init_pid: u32,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_octet: u8,
    machines: Vec<(String, u8)>,
}

impl World {
    pub fn new() -> Self {
        let dir = make_temp_dir("harp-lab");
        let init_script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("lab")
            .join("world-init.ysh");
        let harp_bin = env!("CARGO_BIN_EXE_harp");

        let mut cmd = Command::new("unshare");
        cmd.args([
            "--user",
            "--map-root-user",
            "--map-users=1:100000:65536",
            "--map-groups=1:100000:65536",
            "--net",
            "--mount",
            "--pid",
            "--fork",
            "--kill-child",
            "--mount-proc",
            "ysh",
        ])
        .arg(&init_script)
        .arg(&dir)
        .arg(harp_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

        unsafe {
            cmd.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut unshare = cmd.spawn().expect("spawn unshare for test World");
        let stdin = unshare.stdin.take().expect("world stdin");
        let mut stdout = BufReader::new(unshare.stdout.take().expect("world stdout"));

        let mut ready = String::new();
        stdout
            .read_line(&mut ready)
            .expect("read READY from world-init");
        if ready.trim() != "READY" {
            let mut err = String::new();
            if let Some(mut stderr) = unshare.stderr.take() {
                let _ = stderr.read_to_string(&mut err);
            }
            panic!("world-init failed: got {ready:?}, stderr: {err}");
        }

        let unshare_pid = unshare.id();
        let children_path = format!("/proc/{unshare_pid}/task/{unshare_pid}/children");
        let world_init_pid = fs::read_to_string(&children_path)
            .expect("read world_init pid")
            .split_whitespace()
            .next()
            .expect("world_init child pid missing")
            .parse::<u32>()
            .expect("parse world_init pid");

        Self {
            dir,
            unshare,
            world_init_pid,
            stdin,
            stdout,
            next_octet: 2,
            machines: Vec::new(),
        }
    }

    pub fn machine(&mut self, name: &str) -> Machine {
        self.add_machine(name, true)
    }

    pub fn machine_without_harp(&mut self, name: &str) -> Machine {
        self.add_machine(name, false)
    }

    fn add_machine(&mut self, name: &str, has_harp: bool) -> Machine {
        let octet = self.next_octet;
        self.next_octet += 1;

        let flag = if has_harp { 1 } else { 0 };
        let reply = self.rpc(&format!("ADD {name} {octet} {flag}"));
        let machine_world_pid: u32 = reply
            .strip_prefix("OK ")
            .unwrap_or_else(|| panic!("ADD {name} failed: {reply}"))
            .parse()
            .expect("parse machine pid");

        for (existing_name, existing_octet) in self.machines.clone() {
            let r1 = self.rpc(&format!("PEER {name} {existing_name} {existing_octet}"));
            assert_eq!(r1, "OK");
            let r2 = self.rpc(&format!("PEER {existing_name} {name} {octet}"));
            assert_eq!(r2, "OK");
        }
        self.machines.push((name.to_string(), octet));

        let mdir = self.dir.join("machines").join(name);
        Machine {
            name: name.to_string(),
            ip: format!("10.0.0.{octet}"),
            world_init_pid: self.world_init_pid,
            machine_world_pid,
            home: mdir.join("home"),
            runtime_dir: mdir.join("run"),
            bin_dir: mdir.join("bin"),
        }
    }

    pub fn set_link(&mut self, machine: &Machine, params: LinkParams) {
        let state = if params.up { "up" } else { "down" };
        let reply = self.rpc(&format!("LINK {} {} {}", machine.name, params.delay_ms, state));
        assert_eq!(reply, "OK", "LINK failed: {reply}");
    }

    /// Returns the kernel netns inode number of `world_init`, for leak checks.
    pub fn netns_inode(&self) -> String {
        let target = fs::read_link(format!("/proc/{}/ns/net", self.world_init_pid))
            .expect("readlink world netns");
        let s = target.to_string_lossy();
        s.trim_start_matches("net:[")
            .trim_end_matches(']')
            .to_string()
    }

    fn rpc(&mut self, line: &str) -> String {
        writeln!(self.stdin, "{line}").expect("write rpc to world-init");
        self.stdin.flush().expect("flush rpc");
        let mut reply = String::new();
        self.stdout
            .read_line(&mut reply)
            .expect("read rpc reply from world-init");
        if reply.is_empty() {
            let mut err = String::new();
            if let Some(mut stderr) = self.unshare.stderr.take() {
                let _ = stderr.read_to_string(&mut err);
            }
            panic!("world-init closed stdout on `{line}`; stderr:\n{err}");
        }
        reply.trim().to_string()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.unshare.kill();
        let _ = self.unshare.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[derive(Clone)]
pub struct Machine {
    pub name: String,
    #[allow(dead_code)]
    pub ip: String,
    world_init_pid: u32,
    machine_world_pid: u32,
    home: PathBuf,
    runtime_dir: PathBuf,
    bin_dir: PathBuf,
}

impl Machine {
    #[allow(dead_code)]
    pub fn home(&self) -> &Path {
        &self.home
    }

    fn path_env(&self) -> String {
        format!("{}:/usr/local/bin:/usr/bin:/bin", self.bin_dir.display())
    }

    fn nsenter_args(&self) -> [String; 12] {
        [
            "--user".into(),
            "--mount".into(),
            "--net".into(),
            "--pid".into(),
            "-t".into(),
            self.world_init_pid.to_string(),
            "--".into(),
            "nsenter".into(),
            "--net".into(),
            "--uts".into(),
            "--mount".into(),
            "--pid".into(),
        ]
    }

    pub fn exec(&self, program: &str, args: &[&str]) -> Output {
        let mut cmd = Command::new("nsenter");
        cmd.args(self.nsenter_args())
            .arg("-t")
            .arg(self.machine_world_pid.to_string())
            .arg("--")
            .arg("env")
            .arg("-i")
            .arg(format!("HOME={}", self.home.display()))
            .arg(format!("XDG_RUNTIME_DIR={}", self.runtime_dir.display()))
            .arg(format!("PATH={}", self.path_env()))
            .arg("TERM=xterm-256color")
            .arg("SHELL=/bin/sh")
            .arg(program)
            .args(args);
        cmd.output().expect("exec inside machine")
    }

    pub fn term(&self, program: &str, args: &[&str], size: (u16, u16)) -> Term {
        Term::spawn(self, program, args, size)
    }

    pub fn harp(&self, args: &[&str], size: (u16, u16)) -> Term {
        self.term("harp", args, size)
    }
}

/// Drives an interactive program in a PTY inside a `Machine`, mirroring its
/// output into an in-memory `vt100::Parser`.
pub struct Term {
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    closed: Arc<AtomicBool>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _reader: JoinHandle<()>,
}

impl Term {
    fn spawn(machine: &Machine, program: &str, args: &[&str], (cols, rows): (u16, u16)) -> Self {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let mut cmd = CommandBuilder::new("nsenter");
        for a in machine.nsenter_args() {
            cmd.arg(a);
        }
        cmd.arg("-t");
        cmd.arg(machine.machine_world_pid.to_string());
        cmd.arg("--");
        cmd.arg("env");
        cmd.arg("-i");
        cmd.arg(format!("HOME={}", machine.home.display()));
        cmd.arg(format!("XDG_RUNTIME_DIR={}", machine.runtime_dir.display()));
        cmd.arg(format!("PATH={}", machine.path_env()));
        cmd.arg("TERM=xterm-256color");
        cmd.arg("SHELL=/bin/sh");
        cmd.arg("LANG=C.UTF-8");
        cmd.arg(program);
        for a in args {
            cmd.arg(*a);
        }
        cmd.cwd(&machine.home);

        let child = pair.slave.spawn_command(cmd).expect("spawn term command");
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let writer = pair.master.take_writer().expect("pty writer");
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let closed = Arc::new(AtomicBool::new(false));

        let parser_bg = Arc::clone(&parser);
        let closed_bg = Arc::clone(&closed);
        let reader_handle = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut p = parser_bg.lock().unwrap();
                        p.process(&buf[..n]);
                    }
                }
            }
            closed_bg.store(true, Ordering::Release);
        });

        Self {
            writer,
            parser,
            closed,
            child,
            _reader: reader_handle,
        }
    }

    pub fn send(&mut self, input: &str) {
        self.writer
            .write_all(input.as_bytes())
            .expect("write to pty");
        self.writer.flush().expect("flush pty");
    }

    pub fn screen_text(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    pub fn try_wait_for(
        &self,
        pred: impl Fn(&str) -> bool,
        timeout: Duration,
    ) -> Option<Duration> {
        let start = Instant::now();
        loop {
            let text = self.screen_text();
            if pred(&text) {
                return Some(start.elapsed());
            }
            if start.elapsed() >= timeout || self.closed.load(Ordering::Acquire) {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn wait_for(&self, pred: impl Fn(&str) -> bool, timeout: Duration) -> Duration {
        self.try_wait_for(pred, timeout).unwrap_or_else(|| {
            panic!(
                "timed out after {timeout:?} waiting for screen condition.\n--- SCREEN ---\n{}\n--- END ---",
                self.screen_text()
            )
        })
    }

    /// Detach cleanly from Harp (Ctrl+Space -> Managing mode, then `q`), falling
    /// back to killing the client process.
    pub fn detach_harp(&mut self) {
        self.send("\x00");
        thread::sleep(Duration::from_millis(50));
        self.send("q");
        let start = Instant::now();
        while !self.closed.load(Ordering::Acquire) && start.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn make_temp_dir(prefix: &str) -> PathBuf {
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).expect("rng");
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{hex}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp lab dir");
    dir
}
