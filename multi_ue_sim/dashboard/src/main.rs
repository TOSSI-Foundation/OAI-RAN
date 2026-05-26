// Copyright 2025-2026 coRAN LABS Private Limited
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

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, Block, Borders, Cell, Chart, Dataset, GraphType, LegendPosition,
        Paragraph, Row, Sparkline, Table, TableState, Wrap,
    },
    Terminal,
};
use serde::Deserialize;

const PROXY_STATUS:  &str = "/tmp/multi_ue_proxy_status.json";
const TRAFFIC_STATUS:&str = "/tmp/multi_ue_traffic_status.json";
const PIDS_FILE:     &str = "/tmp/multi_ue_pids.json";
const TRAFFIC_SOCK:  &str = "/tmp/multi_ue_traffic.sock";
const MODE_FILE:     &str = "/tmp/multi_ue_mode.json";
const NS_PREFIX:     &str = "uesim";

fn log_dir()  -> String { format!("{}/logs", scripts_dir()) }
fn log_proxy()   -> String { format!("{}/proxy_nue.log",   log_dir()) }
fn log_gnb()     -> String { format!("{}/gnb.log",         log_dir()) }
fn log_nr_ue_all()-> String { format!("{}/traffic_nue.log",log_dir()) }
fn log_traffic() -> String { format!("{}/traffic_nue.log", log_dir()) }
fn log_nr_ue(i: usize) -> String { format!("{}/ue{}_nue.log", log_dir(), i) }
fn ns_name(i: usize)   -> String { format!("{}{}", NS_PREFIX, i) }
fn veth_host(i: usize) -> String { format!("{}h{}", NS_PREFIX, i) }
fn veth_ns_if(i: usize)-> String { format!("{}n{}", NS_PREFIX, i) }
fn ns_host_ip(i: usize)-> String { format!("10.{}.1.100", 200 + i) }
fn ns_ue_ip(i: usize)  -> String { format!("10.{}.1.1", 200 + i) }
fn ns_subnet(i: usize) -> String { format!("10.{}.1.0/24", 200 + i) }

#[derive(Parser)]
#[command(name = "ue-sim", about = "Multi-UE OAI simulation manager")]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Start {
        #[arg(long, default_value = "10")]
        num_ues: usize,
        #[arg(long, default_value = "001010000000001")]
        imsi_base: String,
        #[arg(long, default_value = "fec86ba6eb707ed08905757b1bb44b8f")]
        key: String,
        #[arg(long, default_value = "C42449363BBAD02B66D16BC975D77CC1")]
        opc: String,
        #[arg(long, default_value = "oai")]
        dnn: String,
        #[arg(long, help = "Use OAI rfsimulator with per-UE Linux namespaces")]
        rfsim: bool,
        #[arg(long, default_value = "tcp://127.0.0.1:4556")]
        gnb_dl: String,
        #[arg(long, default_value = "tcp://127.0.0.1:4557")]
        gnb_ul: String,
        #[arg(long, default_value = "5000")]
        base_port: u16,
        #[arg(long)]
        ue_binary: Option<PathBuf>,
        #[arg(long)]
        uecap_file: Option<PathBuf>,
        #[arg(long, help = "OAI nr-softmodem binary (rfsim mode)")]
        gnb_binary: Option<PathBuf>,
        #[arg(long, help = "gNB config file (rfsim mode)")]
        gnb_config: Option<PathBuf>,
        #[arg(long, help = "UE DL frequency Hz (rfsim mode, default 3619200000)")]
        dl_freq: Option<u64>,
        #[arg(long)]
        iperf_server: Option<String>,
    },
    Stop,
    Status {
        #[arg(long, short, help = "Live TUI dashboard")]
        watch: bool,
    },
    Traffic {
        ue_id: usize,
        pattern: String,
    },
    Logs {
        #[arg(long, default_value = "ue",
              help = "Log source: gnb | proxy | ue | ue1..ueN | traffic")]
        source: String,
        #[arg(long, short, help = "Follow (tail -f)")]
        follow: bool,
    },
    Monitor {
        #[arg(long, help = "Number of UEs (default: read /tmp/run_nue.n)")]
        num_ues: Option<usize>,
        #[arg(long, default_value = "1000", help = "Refresh interval (ms)")]
        interval_ms: u64,
    },
}

#[derive(Deserialize, Default)]
struct ProxyStatus {
    running: Option<bool>,
    n_ues: Option<usize>,
    dl_frames: Option<u64>,
    ul_frames: Option<u64>,
    dl_errors: Option<u64>,
    ul_errors: Option<u64>,
    uptime_s: Option<u64>,
}

#[derive(Deserialize, Default, Clone)]
struct UeMetrics {
    ip: Option<String>,
    pattern: Option<String>,
    status: Option<String>,
    dl_mbps: Option<f64>,
    ul_mbps: Option<f64>,
    latency_ms: Option<f64>,
    loss_pct: Option<f64>,
}

#[derive(Deserialize, Default)]
struct TrafficStatus {
    num_ues: Option<usize>,
    ues: Option<HashMap<String, UeMetrics>>,
}

#[derive(Clone, Copy, PartialEq)]
enum LogSrc { Radio, NrUe, Traffic }

impl LogSrc {
    fn path(self, rfsim: bool, ue_idx: usize) -> String {
        match self {
            LogSrc::Radio   => if rfsim { log_gnb() } else { log_proxy() },
            LogSrc::NrUe    => if rfsim { log_nr_ue(ue_idx) } else { log_nr_ue_all() },
            LogSrc::Traffic => log_traffic(),
        }
    }
    fn label(self, rfsim: bool, ue_idx: usize) -> String {
        match self {
            LogSrc::Radio   => if rfsim { "gnb.log".to_string() } else { "proxy.log".to_string() },
            LogSrc::NrUe    => if rfsim { format!("nr-ue{}.log", ue_idx) } else { "nr-ue.log".to_string() },
            LogSrc::Traffic => "traffic.log".to_string(),
        }
    }
    fn next(self) -> LogSrc {
        match self {
            LogSrc::Radio   => LogSrc::NrUe,
            LogSrc::NrUe    => LogSrc::Traffic,
            LogSrc::Traffic => LogSrc::Radio,
        }
    }
}

struct DashState {
    log_src: LogSrc,
    log_scroll: usize,
    ue_table: TableState,
    ue_count: usize,
    rfsim: bool,
    ue_log_idx: usize,
}

impl DashState {
    fn new() -> Self {
        let mut ts = TableState::default();
        ts.select(Some(0));
        let rfsim = read_mode();
        let ue_count = if rfsim { read_mode_num_ues() } else { 0 };
        Self {
            log_src: LogSrc::NrUe,
            log_scroll: 0,
            ue_table: ts,
            ue_count,
            rfsim,
            ue_log_idx: 1,
        }
    }
    fn ue_up(&mut self) {
        let s = self.ue_table.selected().unwrap_or(0);
        if s > 0 { self.ue_table.select(Some(s - 1)); }
    }
    fn ue_down(&mut self) {
        if self.ue_count == 0 { return; }
        let s = self.ue_table.selected().unwrap_or(0);
        if s + 1 < self.ue_count { self.ue_table.select(Some(s + 1)); }
    }
    fn log_up(&mut self, n: usize) { self.log_scroll = self.log_scroll.saturating_add(n); }
    fn log_down(&mut self, n: usize) { self.log_scroll = self.log_scroll.saturating_sub(n); }
    fn log_end(&mut self) { self.log_scroll = 0; }
    fn ue_log_next(&mut self) {
        let max = self.ue_count.max(1);
        self.ue_log_idx = (self.ue_log_idx % max) + 1;
        self.log_scroll = 0;
        self.log_src = LogSrc::NrUe;
    }
    fn ue_log_prev(&mut self) {
        let max = self.ue_count.max(1);
        if self.ue_log_idx <= 1 { self.ue_log_idx = max; }
        else { self.ue_log_idx -= 1; }
        self.log_scroll = 0;
        self.log_src = LogSrc::NrUe;
    }
}

fn read_proxy_status() -> ProxyStatus {
    fs::read_to_string(PROXY_STATUS)
        .ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn read_traffic_status() -> TrafficStatus {
    fs::read_to_string(TRAFFIC_STATUS)
        .ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn read_log_tail(path: &str) -> Vec<String> {
    let content = fs::read_to_string(path).unwrap_or_default();
    let v: Vec<&str> = content.lines().collect();
    let start = v.len().saturating_sub(2000);
    v[start..].iter().map(|s| s.to_string()).collect()
}

fn read_mode() -> bool {
    fs::read_to_string(MODE_FILE)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("mode").and_then(|m| m.as_str()).map(|s| s == "rfsim"))
        .unwrap_or(false)
}

fn read_mode_num_ues() -> usize {
    fs::read_to_string(MODE_FILE)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("num_ues").and_then(|n| n.as_u64()).map(|n| n as usize))
        .unwrap_or(0)
}

fn gnb_is_running() -> bool {
    let pids = load_pids();
    if let Some(&pid) = pids.get("gnb") {
        std::path::Path::new(&format!("/proc/{}", pid)).exists()
    } else {
        false
    }
}

fn log_style(line: &str) -> Style {
    if line.contains("] [E] ") || line.contains("ERROR") || line.contains("[ERR") {
        Style::default().fg(Color::Rgb(235, 70, 70))
    } else if line.contains("] [W] ") || line.contains("WARN") {
        Style::default().fg(Color::Rgb(210, 140, 0))
    } else if line.contains("] [I] ") || line.contains("INFO")
           || line.contains("[start]") || line.contains("[stop]")
           || line.contains("[proxy]") || line.contains("[traffic]") {
        Style::default().fg(Color::Rgb(0, 185, 90))
    } else if line.contains("] [D] ") || line.contains("DEBUG") {
        Style::default().fg(Color::Rgb(65, 155, 235))
    } else if line.contains("] [T] ") || line.contains("TRACE") {
        Style::default().fg(Color::Rgb(190, 70, 190))
    } else {
        Style::default().fg(Color::Rgb(185, 175, 155))
    }
}

fn status_style(s: &str) -> (String, Style) {
    match s {
        "active" => ("● active".into(),
            Style::default().fg(Color::Rgb(0, 175, 80)).add_modifier(Modifier::BOLD)),
        "waiting_iface" => ("○ waiting...".into(),
            Style::default().fg(Color::Rgb(210, 140, 0))),
        "no_ip" => ("○ no_ip".into(),
            Style::default().fg(Color::Rgb(235, 70, 70))),
        "no_iperf_server" => ("○ no iperf".into(),
            Style::default().fg(Color::Rgb(235, 70, 70))),
        other => (other.to_string(), Style::default().fg(Color::Rgb(185, 175, 155))),
    }
}

fn pattern_style(p: &str) -> Style {
    match p {
        "speedtest" => Style::default().fg(Color::Rgb(60, 150, 235)).add_modifier(Modifier::BOLD),
        "video"     => Style::default().fg(Color::Rgb(190, 70, 190)).add_modifier(Modifier::BOLD),
        "load"      => Style::default().fg(Color::Rgb(215, 125, 0)).add_modifier(Modifier::BOLD),
        _           => Style::default().fg(Color::Rgb(175, 165, 145)),
    }
}

fn fmt_uptime(s: u64) -> String {
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

fn sim_root() -> PathBuf {
    std::env::current_exe()
        .ok().and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_ue_binary() -> PathBuf {
    let root = sim_root();
    let c = root.parent().unwrap_or(&root).join("build/nr-uesoftmodem");
    if c.exists() { c } else { PathBuf::from("nr-uesoftmodem") }
}

fn default_uecap() -> PathBuf {
    let root = sim_root();
    let c = root.parent().unwrap_or(&root)
        .join("targets/PROJECTS/GENERIC-NR-5GC/CONF/uecap_ports1.xml");
    if c.exists() { c } else { PathBuf::from("uecap_ports1.xml") }
}

fn default_gnb_binary() -> PathBuf {
    let root = sim_root();
    let c = root.parent().unwrap_or(&root).join("build/nr-softmodem");
    if c.exists() { c } else { PathBuf::from("nr-softmodem") }
}

fn default_gnb_config() -> PathBuf {
    let root = sim_root();
    let c = root.parent().unwrap_or(&root)
        .join("targets/PROJECTS/GENERIC-NR-5GC/CONF/gnb.sa.band78.fr1.106PRB.pci0.rfsim.conf");
    if c.exists() { c } else { PathBuf::from("gnb.sa.band78.fr1.106PRB.pci0.rfsim.conf") }
}

fn save_pids(pids: &HashMap<String, u32>) {
    if let Ok(j) = serde_json::to_string_pretty(pids) { let _ = fs::write(PIDS_FILE, j); }
}

fn load_pids() -> HashMap<String, u32> {
    fs::read_to_string(PIDS_FILE)
        .ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn kill_pid(pid: u32) {
    let _ = Command::new("kill").args(["-15", &pid.to_string()]).output();
    std::thread::sleep(Duration::from_millis(400));
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
}

fn rc(prog: &str, args: &[&str]) {
    let _ = Command::new(prog).args(args).output();
}

fn create_ue_namespace(i: usize) {
    let ns    = ns_name(i);
    let hif   = veth_host(i);
    let nif   = veth_ns_if(i);
    let h_ip  = format!("{}/24", ns_host_ip(i));
    let n_ip  = format!("{}/24", ns_ue_ip(i));
    let gw    = ns_host_ip(i);
    let sub   = ns_subnet(i);

    rc("ip", &["netns", "add", &ns]);
    rc("ip", &["link", "add", &hif, "type", "veth", "peer", "name", &nif]);
    rc("ip", &["link", "set", &nif, "netns", &ns]);
    rc("ip", &["addr", "add", &h_ip, "dev", &hif]);
    rc("ip", &["link", "set", &hif, "up"]);
    rc("ip", &["netns", "exec", &ns, "ip", "link", "set", "lo", "up"]);
    rc("ip", &["netns", "exec", &ns, "ip", "addr", "add", &n_ip, "dev", &nif]);
    rc("ip", &["netns", "exec", &ns, "ip", "link", "set", &nif, "up"]);
    rc("ip", &["netns", "exec", &ns, "ip", "route", "add", "default", "via", &gw]);
    rc("iptables", &["-t", "nat", "-A", "POSTROUTING", "-s", &sub, "-j", "MASQUERADE"]);
    rc("iptables", &["-A", "FORWARD", "-i", "lo", "-o", &hif, "-j", "ACCEPT"]);
    rc("iptables", &["-A", "FORWARD", "-o", "lo", "-i", &hif, "-j", "ACCEPT"]);
}

fn delete_ue_namespace(i: usize) {
    let ns  = ns_name(i);
    let hif = veth_host(i);
    let sub = ns_subnet(i);
    let _ = Command::new("iptables")
        .args(["-t", "nat", "-D", "POSTROUTING", "-s", &sub, "-j", "MASQUERADE"])
        .output();
    let _ = Command::new("iptables")
        .args(["-D", "FORWARD", "-i", "lo", "-o", &hif, "-j", "ACCEPT"])
        .output();
    let _ = Command::new("iptables")
        .args(["-D", "FORWARD", "-o", "lo", "-i", &hif, "-j", "ACCEPT"])
        .output();
    let _ = Command::new("ip").args(["link", "delete", &hif]).output();
    let _ = Command::new("ip").args(["netns", "delete", &ns]).output();
}

fn cleanup_ue_namespaces() {
    if let Ok(out) = Command::new("ip").args(["netns", "list"]).output() {
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        for line in text.lines() {
            let ns = line.split_whitespace().next().unwrap_or("");
            if let Some(idx_str) = ns.strip_prefix(NS_PREFIX) {
                if let Ok(i) = idx_str.parse::<usize>() {
                    delete_ue_namespace(i);
                }
            }
        }
    }
}

fn stop_existing_simulation() {
    let _ = Command::new("pkill").args(["-9", "-f", "zmq_proxy.py"]).output();
    let _ = Command::new("pkill").args(["-9", "nr-uesoftmodem"]).output();
    let _ = Command::new("pkill").args(["-9", "nr-softmodem"]).output();
    let _ = Command::new("pkill").args(["-9", "-f", "ue_traffic.py"]).output();
    std::thread::sleep(Duration::from_millis(800));
    cleanup_ue_namespaces();
    let _ = fs::remove_file(PIDS_FILE);
    let _ = fs::remove_file(PROXY_STATUS);
    let _ = fs::remove_file(TRAFFIC_STATUS);
    let _ = fs::remove_file(MODE_FILE);
}

fn launch_traffic(root: &PathBuf, num_ues: usize, iperf_server: &Option<String>,
                  ns_prefix: Option<&str>) -> u32 {
    let tlog = fs::File::create(log_traffic()).unwrap();
    let mut targs = vec![
        root.join("traffic/ue_traffic.py").to_str().unwrap().to_string(),
        "--num-ues".to_string(), num_ues.to_string(),
    ];
    if let Some(pfx) = ns_prefix {
        targs.push("--ns-prefix".to_string()); targs.push(pfx.to_string());
    }
    if let Some(ref srv) = iperf_server {
        targs.push("--iperf-server".to_string()); targs.push(srv.clone());
    }
    let child: Child = Command::new("python3")
        .args(&targs)
        .stdout(Stdio::from(tlog.try_clone().unwrap()))
        .stderr(Stdio::from(tlog))
        .spawn().expect("failed to start ue_traffic.py");
    child.id()
}

fn cmd_start_zmq(
    num_ues: usize, imsi_base: &str, key: &str, opc: &str, dnn: &str,
    gnb_dl: &str, gnb_ul: &str, base_port: u16,
    ue_binary: PathBuf, dl_freq: u64,
    iperf_server: Option<String>,
) {
    println!("[start] stopping any existing simulation...");
    stop_existing_simulation();

    let root    = sim_root();
    let gen     = root.join("config/gen_ue_config.py");
    let lib_dir = ue_binary.parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let _ = fs::write(MODE_FILE, format!(r#"{{"mode":"zmq","num_ues":{}}}"#, num_ues));

    rc("sysctl", &["-w", "net.ipv4.ip_forward=1"]);

    let imsi_int: u64 = imsi_base.parse().unwrap_or(1010000000001);
    let imsi_len = imsi_base.len();

    println!("[start] creating {} namespace(s)...", num_ues);
    for i in 1..=num_ues {
        let imsi      = format!("{:0>width$}", imsi_int + (i as u64) - 1, width = imsi_len);
        let conf_path = format!("/tmp/multi_ue_nr-ue{}.conf", i);
        let host_ip   = ns_host_ip(i);
        println!("[start]   ns={}  imsi={}  host-ip={}", ns_name(i), imsi, host_ip);
        create_ue_namespace(i);
        let _ = Command::new("python3")
            .args([gen.to_str().unwrap(),
                   "--num-ues",    "1",
                   "--imsi-base",  &imsi,
                   "--key",        key,
                   "--opc",        opc,
                   "--dnn",        dnn,
                   "--base-port",  &base_port.to_string(),
                   "--ns-host-ip", &host_ip,
                   "--output",     &conf_path])
            .output();
    }

    println!("[start] launching ZMQ proxy  (ns-prefix={})", NS_PREFIX);
    let plog = fs::File::create(log_proxy()).unwrap();
    let pchild: Child = Command::new("python3")
        .args([root.join("proxy/zmq_proxy.py").to_str().unwrap(),
               "--num-ues",     &num_ues.to_string(),
               "--gnb-dl-addr", gnb_dl,
               "--gnb-ul-addr", gnb_ul,
               "--base-port",   &base_port.to_string(),
               "--ns-prefix",   NS_PREFIX])
        .stdout(Stdio::from(plog.try_clone().unwrap()))
        .stderr(Stdio::from(plog))
        .spawn().expect("failed to start zmq_proxy.py");
    let proxy_pid = pchild.id();
    println!("[start] proxy pid={}  (waiting 600ms...)", proxy_pid);
    std::thread::sleep(Duration::from_millis(600));

    let mut pids: HashMap<String, u32> = HashMap::new();
    pids.insert("proxy".to_string(), proxy_pid);

    for i in 1..=num_ues {
        let conf_path = format!("/tmp/multi_ue_nr-ue{}.conf", i);
        let log_path  = log_nr_ue(i);
        let ns        = ns_name(i);

        println!("[start] launching UE{}  ns={}", i, ns);
        let ulog = fs::File::create(&log_path).unwrap();
        let ue_child: Child = Command::new("ip")
            .args(["netns", "exec", &ns,
                   ue_binary.to_str().unwrap(),
                   "-O",            &conf_path,
                   "--device.name", "oai_zmqdevif",
                   "-r",            "106",
                   "--numerology",  "1",
                   "--band",        "78",
                   "-C",            &dl_freq.to_string()])
            .env("LD_LIBRARY_PATH", &lib_dir)
            .stdout(Stdio::from(ulog.try_clone().unwrap()))
            .stderr(Stdio::from(ulog))
            .spawn().expect("failed to start nr-uesoftmodem");

        println!("[start]   UE{} pid={}  log={}", i, ue_child.id(), log_path);
        pids.insert(format!("ue_{}", i), ue_child.id());
        std::thread::sleep(Duration::from_millis(300));
    }

    let traffic_pid = launch_traffic(&root, num_ues, &iperf_server, Some(NS_PREFIX));
    pids.insert("traffic".to_string(), traffic_pid);
    save_pids(&pids);

    println!("\nAll {} UEs started  (ZMQ + namespaces {}1..{}{})",
             num_ues, NS_PREFIX, NS_PREFIX, num_ues);
    println!("Run:  ue-sim status --watch   for live dashboard");
    println!("Logs: {}  ..  {}", log_nr_ue(1), log_nr_ue(num_ues));
    println!("      {}", log_proxy());
}

fn cmd_start_rfsim(
    num_ues: usize, imsi_base: &str, key: &str, opc: &str, dnn: &str,
    ue_binary: PathBuf, _uecap_file: PathBuf,
    gnb_binary: PathBuf, gnb_config: PathBuf, dl_freq: u64,
    iperf_server: Option<String>,
) {
    println!("[start] stopping any existing simulation...");
    stop_existing_simulation();

    let root = sim_root();
    let gen  = root.join("config/gen_ue_config.py");

    let _ = fs::write(MODE_FILE,
        format!(r#"{{"mode":"rfsim","num_ues":{}}}"#, num_ues));

    rc("sysctl", &["-w", "net.ipv4.ip_forward=1"]);

    println!("[start] launching OAI gNB (rfsim)  config={}", gnb_config.display());
    let glog = fs::File::create(log_gnb()).unwrap();
    let gchild: Child = Command::new(&gnb_binary)
        .args(["-O", gnb_config.to_str().unwrap(), "--sa", "--rfsim"])
        .stdout(Stdio::from(glog.try_clone().unwrap()))
        .stderr(Stdio::from(glog))
        .spawn().expect("failed to start nr-softmodem");
    let gnb_pid = gchild.id();
    println!("[start] gnb pid={}  (waiting 4s for rfsim server...)", gnb_pid);
    std::thread::sleep(Duration::from_millis(4000));

    let mut pids: HashMap<String, u32> = HashMap::new();
    pids.insert("gnb".to_string(), gnb_pid);

    let imsi_int: u64 = imsi_base.parse().unwrap_or(1010000000001);
    let imsi_len = imsi_base.len();

    for i in 1..=num_ues {
        let imsi = format!("{:0>width$}", imsi_int + (i as u64) - 1, width = imsi_len);
        let conf_path  = format!("/tmp/multi_ue_nr-ue{}.conf", i);
        let log_path   = log_nr_ue(i);
        let server_addr = ns_host_ip(i);
        let telnet_port = (9095 + i).to_string();
        let ns = ns_name(i);

        println!("[start] UE{}  ns={}  imsi={}  server={}", i, ns, imsi, server_addr);

        create_ue_namespace(i);

        let _ = Command::new("python3")
            .args([gen.to_str().unwrap(),
                   "--num-ues", "1",
                   "--imsi-base", &imsi,
                   "--key", key, "--opc", opc, "--dnn", dnn,
                   "--rfsim", "--output", &conf_path])
            .output();

        let ulog = fs::File::create(&log_path).unwrap();
        let ue_child: Child = Command::new("ip")
            .args(["netns", "exec", &ns,
                   ue_binary.to_str().unwrap(),
                   "-O", &conf_path,
                   "--rfsim", "--sa",
                   "-r", "106",
                   "--numerology", "1",
                   "--band", "78",
                   "-C", &dl_freq.to_string(),
                   "--rfsimulator.[0].serveraddr", &server_addr,
                   "--telnetsrv",
                   "--telnetsrv.listenport", &telnet_port])
            .stdout(Stdio::from(ulog.try_clone().unwrap()))
            .stderr(Stdio::from(ulog))
            .spawn().expect("failed to start nr-uesoftmodem");

        println!("[start]   pid={}  log={}", ue_child.id(), log_path);
        pids.insert(format!("ue_{}", i), ue_child.id());
        std::thread::sleep(Duration::from_millis(300));
    }

    println!("[start] launching traffic engine (namespace mode)");
    let traffic_pid = launch_traffic(&root, num_ues, &iperf_server, Some(NS_PREFIX));
    pids.insert("traffic".to_string(), traffic_pid);
    save_pids(&pids);

    println!("\nAll {} UEs started  (namespaces {}1..{}{})",
             num_ues, NS_PREFIX, NS_PREFIX, num_ues);
    println!("Run:  ue-sim status --watch   for live dashboard");
    println!("Logs: {}  ..  {}", log_nr_ue(1), log_nr_ue(num_ues));
}

fn cmd_stop() {
    let pids = load_pids();
    for (name, pid) in &pids {
        println!("[stop] killing {} (pid={})", name, pid);
        kill_pid(*pid);
    }
    println!("[stop] pkill cleanup + namespace teardown...");
    stop_existing_simulation();
    println!("[stop] done");
}

fn cmd_traffic(ue_id: usize, pattern: &str) {
    let valid = ["idle", "speedtest", "video", "gaming"];
    if !valid.contains(&pattern) {
        eprintln!("Unknown pattern '{}'. Valid: {}", pattern, valid.join(", ")); return;
    }
    let cmd = serde_json::json!({"ue_id": ue_id, "pattern": pattern});
    match UnixStream::connect(TRAFFIC_SOCK) {
        Ok(mut s) => {
            let _ = s.write_all(cmd.to_string().as_bytes());
            let mut buf = [0u8; 256];
            if let Ok(n) = io::Read::read(&mut s, &mut buf) {
                println!("{}", String::from_utf8_lossy(&buf[..n]));
            }
        }
        Err(e) => eprintln!("Cannot connect to traffic engine: {}", e),
    }
}

fn cmd_logs(source: &str, follow: bool) {
    let rfsim = read_mode();
    let path: String = if source.starts_with("ue") && source.len() > 2 {
        if let Ok(i) = source[2..].parse::<usize>() {
            log_nr_ue(i)
        } else if rfsim {
            log_nr_ue(1)
        } else {
            log_nr_ue_all()
        }
    } else {
        match source {
            "traffic" => log_traffic(),
            "gnb"     => log_gnb(),
            "proxy"   => log_proxy(),
            _         => if rfsim { log_nr_ue(1) } else { log_nr_ue_all() },
        }
    };
    if follow {
        let _ = Command::new("tail").args(["-f", "-n", "50", &path]).status();
    } else {
        if let Ok(out) = Command::new("tail").args(["-n", "200", &path]).output() {
            print!("{}", String::from_utf8_lossy(&out.stdout));
        }
    }
}

fn render_status_once() {
    let rfsim = read_mode();
    let ps = read_proxy_status();
    let ts = read_traffic_status();
    println!("─── Multi-UE Simulation Status ──────────────────────────────────────────");
    if rfsim {
        println!("gNB: {}   UEs: {}   Mode: rfsim (per-namespace)",
            if gnb_is_running() { "● RUNNING" } else { "○ STOPPED" },
            ts.num_ues.unwrap_or(0));
    } else {
        println!("Proxy: {}  Uptime: {}  DL: {} fr  UL: {} fr  Errors: {}",
            if ps.running.unwrap_or(false) { "● RUNNING" } else { "○ STOPPED" },
            fmt_uptime(ps.uptime_s.unwrap_or(0)),
            ps.dl_frames.unwrap_or(0), ps.ul_frames.unwrap_or(0),
            ps.dl_errors.unwrap_or(0) + ps.ul_errors.unwrap_or(0));
    }
    println!("{:<4} {:<17} {:<15} {:<10} {:<9} {:<9} {:<10}",
        "UE", "IP", "Status", "Pattern", "DL Mbps", "UL Mbps", "Lat ms");
    println!("{}", "─".repeat(76));
    if let Some(ues) = &ts.ues {
        let mut ids: Vec<usize> = ues.keys().filter_map(|k| k.parse().ok()).collect();
        ids.sort();
        for id in ids {
            if let Some(m) = ues.get(&id.to_string()) {
                println!("{:<4} {:<17} {:<15} {:<10} {:<9} {:<9} {:<10}",
                    id, m.ip.as_deref().unwrap_or("—"),
                    m.status.as_deref().unwrap_or("—"),
                    m.pattern.as_deref().unwrap_or("idle"),
                    m.dl_mbps.filter(|&v| v > 0.0).map(|v| format!("{:.2}",v)).unwrap_or("—".into()),
                    m.ul_mbps.filter(|&v| v > 0.0).map(|v| format!("{:.2}",v)).unwrap_or("—".into()),
                    m.latency_ms.map(|v| format!("{:.1}",v)).unwrap_or("—".into()));
            }
        }
    } else {
        println!("(no UE data — traffic engine not running)");
    }
}

fn ue_detail_lines(id: usize, m: &UeMetrics, rfsim: bool) -> Vec<Line<'static>> {
    let iface = if rfsim {
        format!("oaitun_ue1 (in uesim{})", id)
    } else {
        format!("oaitun_ue{}", id)
    };
    let ip = m.ip.clone().unwrap_or_else(|| "— (waiting for registration)".into());
    let ip_style = if m.ip.is_some() {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Yellow)
    };
    let status_str = m.status.clone().unwrap_or_else(|| "—".into());
    let (slabel, sstyle) = status_style(&status_str);
    let pat = m.pattern.clone().unwrap_or_else(|| "idle".into());
    let pstyle = pattern_style(&pat);
    let dl  = m.dl_mbps.filter(|&v| v > 0.0).map(|v| format!("{:.2} Mbps", v)).unwrap_or("—".into());
    let ul  = m.ul_mbps.filter(|&v| v > 0.0).map(|v| format!("{:.2} Mbps", v)).unwrap_or("—".into());
    let lat = m.latency_ms.map(|v| format!("{:.1} ms", v)).unwrap_or("—".into());
    let loss = m.loss_pct.filter(|&v| v > 0.0).map(|v| format!("{:.1}%", v)).unwrap_or("0.0%".into());

    fn kv(k: &'static str, v: String, vstyle: Style) -> Line<'static> {
        Line::from(vec![
            Span::styled(k, Style::default().fg(Color::DarkGray)),
            Span::styled(v, vstyle),
        ])
    }

    vec![
        kv("  Interface : ", iface, Style::default().fg(Color::White)),
        kv("  IP        : ", ip, ip_style),
        kv("  Status    : ", slabel, sstyle),
        kv("  Pattern   : ", pat, pstyle),
        Line::from(""),
        kv("  DL        : ", dl,  Style::default().fg(Color::Cyan)),
        kv("  UL        : ", ul,  Style::default().fg(Color::Magenta)),
        kv("  Latency   : ", lat, Style::default().fg(Color::Yellow)),
        kv("  Loss      : ", loss, Style::default().fg(Color::White)),
    ]
}

fn run_dashboard() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = DashState::new();
    let tick = Duration::from_millis(500);
    let mut last_tick = Instant::now();

    loop {
        let ps      = read_proxy_status();
        let ts      = read_traffic_status();
        let rfsim   = state.rfsim;

        let ts_n = ts.ues.as_ref().map(|u| u.len()).unwrap_or(0)
            .max(ts.num_ues.unwrap_or(0));
        state.ue_count = if rfsim {
            ts_n.max(read_mode_num_ues())
        } else {
            ts_n
        };
        if state.ue_log_idx == 0 || state.ue_log_idx > state.ue_count.max(1) {
            state.ue_log_idx = 1;
        }

        let ue_rows: Vec<(usize, UeMetrics)> = {
            let mut v: Vec<(usize, UeMetrics)> = ts.ues.as_ref()
                .map(|m| m.iter()
                    .filter_map(|(k, v)| k.parse::<usize>().ok().map(|id| (id, v.clone())))
                    .collect())
                .unwrap_or_default();
            v.sort_by_key(|(id, _)| *id);
            v
        };

        let log_src    = state.log_src;
        let log_scroll = state.log_scroll;
        let ue_log_idx = state.ue_log_idx;
        let sel_idx    = state.ue_table.selected().unwrap_or(0);
        let log_path   = log_src.path(rfsim, ue_log_idx);
        let all_logs   = read_log_tail(&log_path);

        let sel_detail: Option<(usize, UeMetrics)> =
            ue_rows.get(sel_idx).map(|(id, m)| (*id, m.clone()));

        {
            let table_state = &mut state.ue_table;
            let ue_count    = state.ue_count;

            terminal.draw(|f| {
                let area = f.area();

                let vchunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(4), Constraint::Min(0), Constraint::Length(1)])
                    .split(area);

                let (radio_label, radio_ok, line2) = if rfsim {
                    let ok = gnb_is_running();
                    let n  = ts.num_ues.unwrap_or(ue_count);
                    let lbl = if ok {
                        Span::styled("● RUNNING",
                            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
                    } else {
                        Span::styled("○ STOPPED",
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
                    };
                    let l2 = Line::from(vec![
                        Span::styled(" Mode: ", Style::default().fg(Color::DarkGray)),
                        Span::styled("rfsim/ns", Style::default().fg(Color::Cyan)),
                        Span::styled(format!("   UEs: {}  ns: {}1..{}{}", n, NS_PREFIX, NS_PREFIX, n),
                            Style::default().fg(Color::White)),
                    ]);
                    (" gNB: ", lbl, l2)
                } else {
                    let ok  = ps.running.unwrap_or(false);
                    let n   = ps.n_ues.unwrap_or(ue_count);
                    let up  = ps.uptime_s.unwrap_or(0);
                    let dl_f = ps.dl_frames.unwrap_or(0);
                    let ul_f = ps.ul_frames.unwrap_or(0);
                    let errs = ps.dl_errors.unwrap_or(0) + ps.ul_errors.unwrap_or(0);
                    let lbl = if ok {
                        Span::styled("● RUNNING",
                            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
                    } else {
                        Span::styled("○ STOPPED",
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
                    };
                    let l2 = Line::from(vec![
                        Span::raw(" "),
                        Span::styled("DL ", Style::default().fg(Color::Cyan)),
                        Span::styled(format!("{} frames", dl_f), Style::default().fg(Color::White)),
                        Span::raw("   "),
                        Span::styled("UL ", Style::default().fg(Color::Magenta)),
                        Span::styled(format!("{} frames", ul_f), Style::default().fg(Color::White)),
                        Span::raw("   "),
                        Span::styled(format!("Uptime: {}  Errors: {}", fmt_uptime(up), errs),
                            if errs > 0 { Style::default().fg(Color::Red).add_modifier(Modifier::BOLD) }
                            else { Style::default().fg(Color::DarkGray) }),
                        Span::styled(format!("  UEs: {}", n), Style::default().fg(Color::White)),
                    ]);
                    (" Proxy: ", lbl, l2)
                };

                let hdr = Paragraph::new(vec![
                    Line::from(vec![Span::raw(radio_label), radio_ok]),
                    line2,
                ])
                .block(Block::default().borders(Borders::ALL)
                    .title(Span::styled(" ue-sim  Multi-UE Simulation ",
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))));
                f.render_widget(hdr, vchunks[0]);

                let hchunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
                    .split(vchunks[1]);

                let col_w = [
                    Constraint::Length(3),
                    Constraint::Length(16),
                    Constraint::Length(14),
                    Constraint::Length(11),
                    Constraint::Length(8),
                    Constraint::Length(8),
                    Constraint::Min(1),
                ];
                let hdr_row = Row::new(
                    ["#", "IP Address", "Status", "Pattern", "DL Mbps", "UL Mbps", "Lat ms"]
                    .iter().map(|h| Cell::from(*h).style(
                        Style::default().fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)))
                ).height(1).bottom_margin(1);

                let rows: Vec<Row> = if ue_rows.is_empty() {
                    vec![Row::new(vec![
                        Cell::from(""),
                        Cell::from("waiting for traffic engine...").style(
                            Style::default().fg(Color::DarkGray)),
                        Cell::from(""), Cell::from(""), Cell::from(""), Cell::from(""), Cell::from(""),
                    ])]
                } else {
                    ue_rows.iter().map(|(id, m)| {
                        let status_s = m.status.as_deref().unwrap_or("—");
                        let pat_s    = m.pattern.as_deref().unwrap_or("idle");
                        let (slabel, sstyle) = status_style(status_s);
                        Row::new(vec![
                            Cell::from(id.to_string()),
                            Cell::from(m.ip.clone().unwrap_or_else(|| "—".into())),
                            Cell::from(slabel).style(sstyle),
                            Cell::from(pat_s).style(pattern_style(pat_s)),
                            Cell::from(m.dl_mbps.filter(|&v| v > 0.0)
                                .map(|v| format!("{:.2}", v)).unwrap_or("—".into())),
                            Cell::from(m.ul_mbps.filter(|&v| v > 0.0)
                                .map(|v| format!("{:.2}", v)).unwrap_or("—".into())),
                            Cell::from(m.latency_ms
                                .map(|v| format!("{:.1}", v)).unwrap_or("—".into())),
                        ]).height(1)
                    }).collect()
                };

                let table = Table::new(rows, col_w)
                    .header(hdr_row)
                    .block(Block::default().borders(Borders::ALL)
                        .title(Span::styled(" UE Status ",
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))))
                    .row_highlight_style(
                        Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
                    .highlight_symbol("►");
                f.render_stateful_widget(table, hchunks[0], table_state);

                let vright = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(13), Constraint::Min(0)])
                    .split(hchunks[1]);

                let detail_title = match &sel_detail {
                    Some((id, _)) => format!(" UE {}  ↑↓ to select ", id),
                    None          => " UE Detail  ↑↓ to select ".to_string(),
                };
                let detail_lines: Vec<Line> = match &sel_detail {
                    Some((id, m)) => ue_detail_lines(*id, m, rfsim),
                    None => vec![
                        Line::from(Span::styled("  (start simulation first)",
                            Style::default().fg(Color::DarkGray))),
                    ],
                };
                let detail = Paragraph::new(detail_lines)
                    .block(Block::default().borders(Borders::ALL)
                        .title(Span::styled(detail_title,
                            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))));
                f.render_widget(detail, vright[0]);

                let log_h  = vright[1].height.saturating_sub(2) as usize;
                let total  = all_logs.len();
                let offset = log_scroll.min(total.saturating_sub(log_h));
                let end    = total.saturating_sub(offset);
                let start  = end.saturating_sub(log_h);

                let log_lines: Vec<Line> = all_logs[start..end].iter()
                    .map(|line| Line::from(Span::styled(line.clone(), log_style(line))))
                    .collect();

                let log_lbl = log_src.label(rfsim, ue_log_idx);
                let log_title = if offset == 0 {
                    if rfsim && log_src == LogSrc::NrUe {
                        format!(" {} ● live  [/]:UE  Tab → ", log_lbl)
                    } else {
                        format!(" {} ● live  Tab → ", log_lbl)
                    }
                } else if rfsim && log_src == LogSrc::NrUe {
                    format!(" {} ↑{}  [/]:UE  End→tail  Tab → ", log_lbl, offset)
                } else {
                    format!(" {} ↑{}  End→tail  Tab → ", log_lbl, offset)
                };
                let log_title_style = if offset == 0 {
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                };

                let log_para = Paragraph::new(log_lines)
                    .block(Block::default().borders(Borders::ALL)
                        .title(Span::styled(log_title, log_title_style)))
                    .wrap(Wrap { trim: false });
                f.render_widget(log_para, vright[1]);

                let (tab_hint, ue_hint) = if rfsim {
                    ("gnb/nr-ueN/traffic", "  [/]:prev/next UE log")
                } else {
                    ("proxy/nr-ue/traffic", "")
                };
                let footer = Paragraph::new(Line::from(vec![
                    Span::styled(" q", Style::default().fg(Color::Yellow)), Span::raw("/Esc:quit  "),
                    Span::styled("Tab", Style::default().fg(Color::Yellow)),
                    Span::raw(format!(":cycle log ({})  ", tab_hint)),
                    Span::styled("↑↓", Style::default().fg(Color::Yellow)), Span::raw(":select UE  "),
                    Span::styled("PgUp/Dn", Style::default().fg(Color::Yellow)), Span::raw(":scroll log  "),
                    Span::styled("End", Style::default().fg(Color::Yellow)), Span::raw(":tail"),
                    Span::styled(ue_hint, Style::default().fg(Color::Cyan)),
                ])).style(Style::default().fg(Color::DarkGray));
                f.render_widget(footer, vchunks[2]);
            })?;
        }

        let timeout = tick.checked_sub(last_tick.elapsed()).unwrap_or_default();
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Tab       => { state.log_src = state.log_src.next(); state.log_scroll = 0; },
                    KeyCode::Char('[') => state.ue_log_prev(),
                    KeyCode::Char(']') => state.ue_log_next(),
                    KeyCode::Up        => state.ue_up(),
                    KeyCode::Down      => state.ue_down(),
                    KeyCode::PageUp    => state.log_up(10),
                    KeyCode::PageDown  => state.log_down(10),
                    KeyCode::End       => state.log_end(),
                    _ => {}
                }
            }
        }
        if last_tick.elapsed() >= tick { last_tick = Instant::now(); }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

#[derive(Deserialize, Default)]
#[allow(dead_code)] // running/n_ues parsed from the proxy JSON but liveness now via pgrep
struct NueProxyStatus {
    running: Option<bool>,
    n_ues:   Option<usize>,
    dl:      Option<u64>,
    ul:      Option<u64>,
    dl_err:  Option<u64>,
    ul_err:  Option<u64>,
}

#[derive(Clone)]
#[allow(dead_code)] // sync/sib1/... feed `stage`; kept for a future per-UE detail view
struct UeConn {
    id: usize,
    sync: u32, sib1: u32, rar_ok: u32, rar_fail: u32, rrc: u32, reg: u32,
    stage: &'static str,
    ip: String,
    has_tun: bool,   // oaitun_ue1 is LIVE with a 10.x IP — i.e. data plane actually works
}

fn count_marker(hay: &str, needle: &str) -> u32 {
    hay.matches(needle).count() as u32
}

fn read_nue_proxy_status() -> NueProxyStatus {
    fs::read_to_string(PROXY_STATUS)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn live_tun_ip(i: usize) -> String {
    if let Ok(out) = Command::new("ip")
        .args(["netns", "exec", &format!("ue{}", i), "ip", "-4", "addr", "show", "oaitun_ue1"])
        .output()
    {
        for l in String::from_utf8_lossy(&out.stdout).lines() {
            let l = l.trim();
            if l.starts_with("inet ") {
                if let Some(a) = l.split_whitespace().nth(1).and_then(|x| x.split('/').next()) {
                    if a.starts_with("10.") { return a.to_string(); }
                }
            }
        }
    }
    String::new()
}

fn log_assigned_ip(log: &str) -> String {
    for l in log.lines().rev() {
        for tag in ["UE IPv4:", "IPv4 "] {
            if let Some(p) = l.find(tag) {
                let ip: String = l[p + tag.len()..].trim_start().chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.').collect();
                if ip.starts_with("10.") && ip.matches('.').count() == 3 { return ip; }
            }
        }
    }
    String::new()
}

fn parse_ue_conn(i: usize) -> UeConn {
    match fs::read_to_string(log_nr_ue(i)) {
        Ok(log) => {
            let sync     = count_marker(&log, "pbch decoded sucessfully");
            let sib1     = count_marker(&log, "SIB1 decoded");
            let rar_ok   = count_marker(&log, "RAR-Msg2 decoded");
            let rar_fail = count_marker(&log, "RAR reception failed");
            let rrc      = count_marker(&log, "RRCSetupComplete");
            let reg      = count_marker(&log, "Registration Accept");

            let all_lines: Vec<&str> = log.lines().collect();
            let recent_start = all_lines.len().saturating_sub(250);
            let recent: String = all_lines[recent_start..].join("\n");
            let r_reg  = count_marker(&recent, "Registration Accept") > 0;
            let r_rrc  = count_marker(&recent, "RRCSetupComplete") > 0;
            let r_rar  = count_marker(&recent, "RAR-Msg2 decoded") > 0;
            let r_sib1 = count_marker(&recent, "SIB1 decoded") > 0;
            let r_sync = count_marker(&recent, "pbch decoded sucessfully") > 0;

            let live_ip = if reg > 0 || rrc > 0 { live_tun_ip(i) } else { String::new() };
            let has_tun = !live_ip.is_empty();

            let stage = if has_tun       { "DATA" }
                else if r_reg            { "REGISTERED" }
                else if r_rrc            { "RRC" }
                else if r_rar            { "RAR" }
                else if r_sib1           { "SIB1" }
                else if r_sync           { "SYNC" }   // re-syncing after prior RRC/REG
                else if reg > 0          { "REGISTERED" }
                else if rrc > 0          { "RRC" }
                else if rar_ok > 0       { "RAR" }
                else if sib1 > 0         { "SIB1" }
                else if sync > 0         { "SYNC" }
                else                     { "--" };

            let ip = if has_tun {
                live_ip
            } else {
                let a = log_assigned_ip(&log);
                if a.is_empty() { "—".to_string() } else { format!("{}↓", a) }
            };
            UeConn { id: i, sync, sib1, rar_ok, rar_fail, rrc, reg, stage, ip, has_tun }
        }
        Err(_) => UeConn {
            id: i, sync: 0, sib1: 0, rar_ok: 0, rar_fail: 0, rrc: 0, reg: 0,
            stage: "(no log)", ip: "—".to_string(), has_tun: false,
        },
    }
}

const RTT_HIST: usize = 240;

fn scripts_dir() -> String {
    if let Ok(v) = std::env::var("MULTI_UE_SIM") {
        if !v.is_empty() { return v; }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(p) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            return p.to_string_lossy().into_owned();
        }
    }
    std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| ".".into())
}

#[derive(Default, Clone)]
struct LiveMetric {
    rtt: VecDeque<f64>,   // RTT history in ms (oldest front)
    last_rtt: Option<f64>,
    recv: u64,            // ping replies seen
    dl_mbps: f64,
    ul_mbps: f64,
    tput: VecDeque<f64>,  // live throughput history in Mbps (oaitun_ue1 rx+tx rate)
    last_tput: Option<f64>,
}
type Shared = Arc<Mutex<HashMap<usize, LiveMetric>>>;

struct UeView {
    conn: UeConn,
    last_rtt: Option<f64>,
    recv: u64,
    dl_mbps: f64,
    ul_mbps: f64,
    rtt_hist: Vec<f64>,
    last_tput: Option<f64>,
    tput_hist: Vec<f64>,
}

#[derive(Clone, Copy, PartialEq)]
enum ChartMetric { AvgRtt, AllRtt, SelRtt, Bitrate }

fn discover_ext_dn_ip() -> Option<String> {
    let out = Command::new("docker")
        .args(["exec", "oai-ext-dn", "ip", "-4", "addr", "show"])
        .stderr(Stdio::null()).output().ok()?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let l = line.trim();
        if l.starts_with("inet ") && !l.contains("127.0.0.1") {
            if let Some(ip) = l.split_whitespace().nth(1).and_then(|x| x.split('/').next()) {
                return Some(ip.to_string());
            }
        }
    }
    None
}

fn ping_target(_ext_dn_ip: &str) -> String {
    std::env::var("PING_TARGET").ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "8.8.8.8".to_string())
}

fn iperf_srv(ext_dn_ip: &str) -> Option<String> {
    Some(std::env::var("IPERF_SRV").ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| ext_dn_ip.to_string()))
}

fn ping_thread(ue: usize, target: String, shared: Shared, stop: Arc<AtomicBool>) {
    let ns = format!("ue{ue}");
    let mut tun_ip = live_tun_ip(ue);
    if tun_ip.is_empty() {
        for _ in 0..20 {
            if stop.load(Ordering::Relaxed) { return; }
            thread::sleep(Duration::from_secs(1));
            tun_ip = live_tun_ip(ue);
            if !tun_ip.is_empty() { break; }
        }
    }
    if tun_ip.is_empty() { return; }   // no tunnel — UE never reached DATA stage, skip
    ensure_iperf_route(ue, &tun_ip);

    let mut child = match Command::new("ip")
        .args(["netns", "exec", &ns, "ping", "-i", "1", "-W", "3", "-I", "oaitun_ue1", &target])
        .stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null()).spawn()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines() {
            if stop.load(Ordering::Relaxed) { break; }
            let line = match line { Ok(l) => l, Err(_) => break };
            if let Some(p) = line.find("time=") {
                let v: String = line[p + 5..].chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.').collect();
                if let Ok(rtt) = v.parse::<f64>() {
                    if let Ok(mut m) = shared.lock() {
                        let e = m.entry(ue).or_default();
                        e.recv += 1;
                        e.last_rtt = Some(rtt);
                        e.rtt.push_back(rtt);
                        while e.rtt.len() > RTT_HIST { e.rtt.pop_front(); }
                    }
                }
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn tput_thread(ue: usize, shared: Shared, stop: Arc<AtomicBool>) {
    let ns = format!("ue{ue}");
    let mut prev: Option<(u64, u64, std::time::Instant)> = None;
    while !stop.load(Ordering::Relaxed) {
        let mut counters: Option<(u64, u64)> = None;   // (rx_bytes, tx_bytes)
        if let Ok(o) = Command::new("ip")
            .args(["netns", "exec", &ns, "cat", "/proc/net/dev"])
            .stderr(Stdio::null()).output()
        {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                if let Some(rest) = line.trim().strip_prefix("oaitun_ue1:") {
                    let f: Vec<&str> = rest.split_whitespace().collect();
                    if f.len() >= 9 {
                        let rx = f[0].parse::<u64>().unwrap_or(0);
                        let tx = f[8].parse::<u64>().unwrap_or(0);
                        counters = Some((rx, tx));
                    }
                }
            }
        }
        let now = std::time::Instant::now();
        if let (Some((rx, tx)), Some((prx, ptx, pt))) = (counters, prev) {
            let dt = now.duration_since(pt).as_secs_f64();
            if dt > 0.0 {
                let dl = (rx.saturating_sub(prx) as f64 * 8.0) / 1e6 / dt;  // downlink (rx) Mbps
                let ul = (tx.saturating_sub(ptx) as f64 * 8.0) / 1e6 / dt;  // uplink (tx) Mbps
                if let Ok(mut m) = shared.lock() {
                    let e = m.entry(ue).or_default();
                    e.dl_mbps = dl;            // LIVE downlink rate -> table DL column
                    e.ul_mbps = ul;            // LIVE uplink rate   -> table UL column
                    e.last_tput = Some(dl + ul);
                    e.tput.push_back(dl + ul); // total -> throughput graph
                    while e.tput.len() > RTT_HIST { e.tput.pop_front(); }
                }
            }
        }
        if let Some((rx, tx)) = counters { prev = Some((rx, tx, now)); }
        thread::sleep(Duration::from_millis(1000));
    }
}

fn ensure_iperf_route(ue: usize, tun_ip: &str) {
    if tun_ip.is_empty() { return; }
    let ns = format!("ue{ue}");
    let _ = Command::new("ip")
        .args(["netns", "exec", &ns, "ip", "rule", "add",
               "from", tun_ip, "lookup", "200", "priority", "100"])
        .stderr(Stdio::null()).output();
    let _ = Command::new("ip")
        .args(["netns", "exec", &ns, "ip", "route", "replace",
               "default", "dev", "oaitun_ue1", "table", "200"])
        .stderr(Stdio::null()).output();
    let _ = Command::new("ip")
        .args(["netns", "exec", &ns, "sysctl", "-qw",
               "net.core.rmem_max=67108864",
               "net.core.wmem_max=67108864",
               "net.ipv4.tcp_rmem=4096 1048576 67108864",
               "net.ipv4.tcp_wmem=4096 1048576 67108864"])
        .stderr(Stdio::null()).output();
}

fn iperf_run(ue: usize, srv: &str, port: u16, ip: &str, reverse: bool) -> f64 {
    ensure_iperf_route(ue, ip);
    let ns = format!("ue{ue}");
    let port_s = port.to_string();
    let mut args = vec!["netns", "exec", &ns, "iperf3",
                        "-c", srv, "-p", &port_s, "-B", ip,
                        "-t", "30", "-P", "4", "-w", "4M", "-J"];
    if reverse { args.push("-R"); }
    let out = match Command::new("ip").args(&args).stderr(Stdio::null()).output() {
        Ok(o) => o,
        Err(_) => return 0.0,
    };
    let txt = String::from_utf8_lossy(&out.stdout);
    let key = "\"bits_per_second\":";
    let (mut last, mut idx) = (0.0_f64, 0usize);
    while let Some(p) = txt[idx..].find(key) {
        let s = idx + p + key.len();
        let num: String = txt[s..].chars().skip_while(|c| c.is_whitespace())
            .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')).collect();
        if let Ok(bps) = num.parse::<f64>() { last = bps; }
        idx = s;
    }
    last / 1e6
}

fn iperf_thread(ue: usize, srv: String, port: u16, ip: String, _shared: Shared) {
    thread::sleep(Duration::from_millis(3500));
    let _ = iperf_run(ue, &srv, port, &ip, true);
    let _ = iperf_run(ue, &srv, port, &ip, false);
}

fn traffic_thread(ue: usize, srv: String, port: u16, ip: String, kind: &'static str) {
    thread::sleep(Duration::from_millis(1000));
    ensure_iperf_route(ue, &ip);
    let ns = format!("ue{ue}");
    let port_s = port.to_string();
    let mut a: Vec<String> = ["netns", "exec", &ns, "iperf3",
                              "-c", &srv, "-p", &port_s, "-B", &ip,
                              "-u", "-J", "-t", "30"].iter().map(|s| s.to_string()).collect();
    if kind == "video" {
        a.extend(["-b".into(), "8M".into(), "-R".into()]);
    } else {
        a.extend(["-b".into(), "1M".into(), "-l".into(), "100".into()]);
    }
    let _ = Command::new("ip").args(&a)
        .stdout(Stdio::null()).stderr(Stdio::null()).status();
}

fn spawn_bg(cmd: String) {
    let _ = Command::new("bash").arg("-c").arg(cmd)
        .stdout(Stdio::null()).stderr(Stdio::null()).stdin(Stdio::null()).spawn();
}

fn netns_has_proc(ns: &str) -> bool {
    Command::new("ip").args(["netns", "pids", ns]).output()
        .map(|o| !o.stdout.trim_ascii().is_empty()).unwrap_or(false)
}

fn running_ue_ids() -> Vec<usize> {
    let mut ids = Vec::new();
    if let Ok(out) = Command::new("ip").args(["netns", "list"]).output() {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(name) = line.split_whitespace().next() {
                if let Some(rest) = name.strip_prefix("ue") {
                    if let Ok(i) = rest.parse::<usize>() {
                        if netns_has_proc(name) { ids.push(i); }
                    }
                }
            }
        }
    }
    ids.sort_unstable();
    ids
}

fn proc_count(pat: &str) -> usize {
    Command::new("pgrep").args(["-c", "-f", pat]).output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn gnb_radio_ready() -> bool {
    use std::net::TcpStream;
    let port: u16 = std::env::var("GNB_DL_PORT")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(4556);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

fn stage_color(stage: &str) -> Color {
    match stage {
        "DATA"       => Color::Rgb(0,  140, 60),   // forest green  — data plane up (pingable)
        "REGISTERED" => Color::Rgb(20, 115, 20),   // darker green  — control plane only
        "RRC"        => Color::Rgb(0,  95,  175),  // deep blue
        "RAR"        => Color::Rgb(40, 130, 200),  // sky blue
        "SIB1"       => Color::Rgb(155, 95,  0),   // amber
        "SYNC"       => Color::Rgb(130, 20, 130),  // dark purple
        _            => Color::Rgb(140, 130, 115), // stone grey
    }
}

#[allow(clippy::too_many_arguments)]
fn render_dash(
    f: &mut ratatui::Frame<'_>,
    views: &[UeView],
    ps: &NueProxyStatus,
    gnb_up: bool,
    gnb_dup: bool,
    proxy_up: bool,
    proxy_dup: bool,
    sel: usize,
    n_set: usize,
    ping_on: bool,
    load_on: bool,
    load_focus_idx: usize,
    target: &str,
    srv: &Option<String>,
    metric: ChartMetric,
    status: &str,
) {
    let n = views.len();
    let reg_n = views.iter().filter(|v| v.conn.reg > 0).count();
    let data_n = views.iter().filter(|v| v.conn.has_tun).count();   // working data plane (pingable)
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(6), Constraint::Length(14), Constraint::Length(2)])
        .split(f.area());

    let border_st = Style::default().fg(Color::Rgb(190, 188, 182));
    let title_st  = Style::default().fg(Color::Rgb(225, 222, 215)).add_modifier(Modifier::BOLD);

    let gnb = if gnb_dup {
        Span::styled("⚠ gNB DUP", Style::default().fg(Color::Rgb(185, 35, 35)).add_modifier(Modifier::BOLD | Modifier::RAPID_BLINK))
    } else if gnb_up {
        Span::styled("● gNB UP", Style::default().fg(Color::Rgb(0, 140, 60)).add_modifier(Modifier::BOLD))
    } else {
        Span::styled("○ gNB DOWN", Style::default().fg(Color::Rgb(185, 35, 35)).add_modifier(Modifier::BOLD))
    };
    let prx = if proxy_dup {
        Span::styled("proxy x2!", Style::default().fg(Color::Rgb(185, 35, 35)).add_modifier(Modifier::BOLD))
    } else if proxy_up {
        Span::styled("proxy UP", Style::default().fg(Color::Rgb(0, 140, 60)))
    } else {
        Span::styled("proxy down", Style::default().fg(Color::Rgb(185, 35, 35)))
    };
    let run_col = if n > 0 { Color::Rgb(210, 207, 198) } else { Color::Rgb(130, 128, 122) };
    let reg_col = if n > 0 && reg_n == n { Color::Rgb(0, 175, 80) } else { Color::Rgb(215, 135, 0) };
    let hdr = Paragraph::new(Line::from(vec![
        Span::raw(" "), gnb, Span::raw("  "), prx, Span::raw("   "),
        Span::styled(format!("running {} UEs", n), Style::default().fg(run_col).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled(format!("REGISTERED {}/{}", reg_n, n), Style::default().fg(reg_col).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(format!("DATA {}/{}", data_n, n),
            Style::default().fg(if n > 0 && data_n == n { Color::Rgb(0, 175, 80) } else { Color::Rgb(215, 135, 0) }).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled(format!("start N={}", n_set), Style::default().fg(Color::Rgb(0, 95, 170))),
        Span::raw("   "),
        Span::styled(
            if load_on {
                format!("LOAD:UE{}", if !views.is_empty() { views[load_focus_idx % views.len()].conn.id } else { 0 })
            } else if ping_on { format!("ping→{}", target) } else { "ping:off".into() },
            Style::default().fg(if load_on { Color::Rgb(215, 135, 0) } else if ping_on { Color::Rgb(0, 175, 80) } else { Color::Rgb(130, 120, 105) })),
        Span::raw("   "),
        Span::styled(format!("iperf:{}", srv.clone().unwrap_or_else(|| "unset".into())), Style::default().fg(Color::Rgb(120, 110, 98))),
        Span::raw("   "),
        Span::styled(
            format!("IQ dl {} ul {} err {}", ps.dl.unwrap_or(0), ps.ul.unwrap_or(0),
                ps.dl_err.unwrap_or(0) + ps.ul_err.unwrap_or(0)),
            Style::default().fg(Color::Rgb(120, 110, 98))),
        if gnb_dup {
            Span::styled("   ⚠ DUPLICATE gNB — press r to rebuild", Style::default().fg(Color::Rgb(185, 35, 35)).add_modifier(Modifier::BOLD))
        } else { Span::raw("") },
    ]))
    .block(Block::default().borders(Borders::ALL).border_style(border_st)
        .title(Span::styled(" Multi-UE Dashboard ", title_st)));
    f.render_widget(hdr, root[0]);

    let mid = Layout::default().direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)]).split(root[1]);

    let header = Row::new(vec!["UE", "STAGE", "IP", "RTT", "rx", "DL Mbps", "UL Mbps"])
        .style(Style::default().fg(Color::Rgb(205, 202, 195)).add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
    let rows: Vec<Row> = views.iter().enumerate().map(|(i, v)| {
        let rtt = v.last_rtt.map(|r| format!("{:.0}ms", r)).unwrap_or_else(|| "-".into());
        let fmt_rate = |mbps: f64| -> String {
            if mbps >= 0.5        { format!("{:.2}", mbps) }
            else if mbps >= 0.001 { format!("{:.0}K", mbps * 1000.0) }
            else                  { "-".into() }
        };
        let dl = fmt_rate(v.dl_mbps);
        let ul = fmt_rate(v.ul_mbps);
        let mut style = Style::default().fg(stage_color(v.conn.stage));
        if load_on && !views.is_empty() && i == load_focus_idx % views.len() {
            style = style.bg(Color::Rgb(45, 50, 65)).add_modifier(Modifier::BOLD);
        } else if i == sel {
            style = style.bg(Color::Rgb(38, 42, 52)).add_modifier(Modifier::BOLD);
        }
        Row::new(vec![
            v.conn.id.to_string(), v.conn.stage.to_string(), v.conn.ip.clone(),
            rtt, v.recv.to_string(), dl, ul,
        ]).style(style)
    }).collect();
    let widths = [
        Constraint::Length(4), Constraint::Length(11), Constraint::Length(13),
        Constraint::Length(7), Constraint::Length(6), Constraint::Length(7), Constraint::Length(7),
    ];
    let table = Table::new(rows, widths).header(header)
        .block(Block::default().borders(Borders::ALL).border_style(border_st).title(" UEs (↑↓ select) "));
    f.render_widget(table, mid[0]);

    const PALETTE: [Color; 8] = [
        Color::Rgb(0,  115, 185),   // blue
        Color::Rgb(195,  95,   0),  // amber
        Color::Rgb(0,  140,  60),   // green
        Color::Rgb(140,  30, 140),  // purple
        Color::Rgb(0,  155, 170),   // teal
        Color::Rgb(185,  35,  35),  // red
        Color::Rgb(80,  145,   0),  // lime
        Color::Rgb(100,  85,  65),  // brown
    ];
    let per_ue: Vec<(usize, Vec<(f64, f64)>)> = views.iter().map(|v| {
        (v.conn.id, v.rtt_hist.iter().enumerate().map(|(i, &y)| (i as f64, y)).collect())
    }).collect();
    let per_tput: Vec<(usize, Vec<(f64, f64)>)> = views.iter().map(|v| {
        (v.conn.id, v.tput_hist.iter().enumerate().map(|(i, &y)| (i as f64, y)).collect())
    }).collect();
    let avg_series: Vec<(f64, f64)> = {
        let maxlen = views.iter().map(|v| v.rtt_hist.len()).max().unwrap_or(0);
        (0..maxlen).map(|i| {
            let vals: Vec<f64> = views.iter().filter_map(|v| v.rtt_hist.get(i).copied()).collect();
            let y = if vals.is_empty() { 0.0 } else { vals.iter().sum::<f64>() / vals.len() as f64 };
            (i as f64, y)
        }).collect()
    };
    let ymax = {
        let m = match metric {
            ChartMetric::SelRtt => per_ue.get(sel).map(|t| t.1.iter().map(|p| p.1).fold(1.0, f64::max)).unwrap_or(1.0),
            ChartMetric::Bitrate => views.iter().flat_map(|v| v.tput_hist.iter().cloned()).fold(0.1, f64::max),
            _ => views.iter().flat_map(|v| v.rtt_hist.iter().cloned()).fold(1.0, f64::max),
        };
        (m * 1.25).max(if metric == ChartMetric::Bitrate { 0.1 } else { 1.0 })
    };
    let xmax = match metric {
        ChartMetric::Bitrate => views.iter().map(|v| v.tput_hist.len()).max().unwrap_or(1),
        _ => views.iter().map(|v| v.rtt_hist.len()).max().unwrap_or(1),
    }.max(1) as f64;
    let (title, datasets): (String, Vec<Dataset>) = match metric {
        ChartMetric::AvgRtt => (
            " avg RTT (ms) — g: all UEs ".to_string(),
            vec![Dataset::default().marker(symbols::Marker::Braille).graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Rgb(0, 115, 185))).data(&avg_series)],
        ),
        ChartMetric::AllRtt => (
            " all UEs RTT (ms) — g: selected ".to_string(),
            per_ue.iter().enumerate().map(|(idx, (id, pts))| {
                Dataset::default().name(format!("UE{}", id))
                    .marker(symbols::Marker::Braille).graph_type(GraphType::Line)
                    .style(Style::default().fg(PALETTE[idx % PALETTE.len()])).data(pts)
            }).collect(),
        ),
        ChartMetric::SelRtt => {
            let pts: &[(f64, f64)] = per_ue.get(sel).map(|t| t.1.as_slice()).unwrap_or(&[]);
            (format!(" UE{} RTT (ms) — g: bitrate ", views.get(sel).map(|v| v.conn.id).unwrap_or(0)),
             vec![Dataset::default().marker(symbols::Marker::Braille).graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Rgb(0, 115, 185))).data(pts)])
        }
        ChartMetric::Bitrate => (
            " all UEs THROUGHPUT (Mbps) — g: avg RTT  [p starts it, i=iperf spikes] ".to_string(),
            per_tput.iter().enumerate().map(|(idx, (id, pts))| {
                Dataset::default().name(format!("UE{}", id))
                    .marker(symbols::Marker::Braille).graph_type(GraphType::Line)
                    .style(Style::default().fg(PALETTE[idx % PALETTE.len()])).data(pts)
            }).collect(),
        ),
    };
    let yunit = if metric == ChartMetric::Bitrate { "Mbps" } else { "ms" };
    let yfmt = |v: f64| if metric == ChartMetric::Bitrate { format!("{:.1}", v) } else { format!("{:.0}", v) };
    let axis_dim = Color::Rgb(150, 148, 142);
    let axis_bold = Color::Rgb(210, 207, 198);
    let ylabels = vec![
        Span::styled("0", Style::default().fg(axis_dim)),
        Span::styled(yfmt(ymax / 2.0), Style::default().fg(axis_dim)),
        Span::styled(format!("{} {}", yfmt(ymax), yunit),
                     Style::default().fg(axis_bold).add_modifier(Modifier::BOLD)),
    ];
    let secs = xmax as u64;   // each sample ≈ 1 s (ping -i1 / tput 1s)
    let xlabels = vec![
        Span::styled(format!("-{}s", secs), Style::default().fg(axis_dim)),
        Span::styled(format!("-{}s", secs / 2), Style::default().fg(axis_dim)),
        Span::styled("now", Style::default().fg(axis_bold).add_modifier(Modifier::BOLD)),
    ];
    let chart = Chart::new(datasets)
        .block(Block::default().borders(Borders::ALL).border_style(border_st).title(title))
        .x_axis(Axis::default()
            .title(Span::styled("time →", Style::default().fg(axis_dim)))
            .style(Style::default().fg(axis_dim))
            .bounds([0.0, xmax])
            .labels(xlabels))
        .y_axis(Axis::default()
            .title(Span::styled(yunit, Style::default().fg(axis_dim)))
            .style(Style::default().fg(axis_dim))
            .bounds([0.0, ymax])
            .labels(ylabels))
        .legend_position(match metric {
            ChartMetric::AllRtt | ChartMetric::Bitrate => Some(LegendPosition::TopLeft),
            _ => None,
        })
        .hidden_legend_constraints((Constraint::Ratio(1, 3), Constraint::Ratio(1, 1)));
    f.render_widget(chart, mid[1]);

    let bottom = Layout::default().direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(32), Constraint::Percentage(68)]).split(root[2]);

    if !views.is_empty() {
        let n = views.len();
        let row1_n = (n + 1) / 2;  // first row: ceiling half
        let row2_n = n / 2;         // second row: floor half (0 when n==1)
        let spark_rows = if row2_n > 0 {
            Layout::default().direction(Direction::Vertical)
                .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
                .split(bottom[0])
        } else {
            Layout::default().direction(Direction::Vertical)
                .constraints([Constraint::Min(0)])
                .split(bottom[0])
        };

        let render_spark_row = |f: &mut ratatui::Frame<'_>, offset: usize, count: usize, area: ratatui::layout::Rect| {
            if count == 0 { return; }
            let cells = Layout::default().direction(Direction::Horizontal)
                .constraints(vec![Constraint::Ratio(1, count as u32); count])
                .split(area);
            for j in 0..count {
                let idx = offset + j;
                if idx >= views.len() { break; }
                let v = &views[idx];
                let data: Vec<u64> = v.rtt_hist.iter().map(|&r| r.round().max(0.0) as u64).collect();
                let rtt_s = v.last_rtt.map(|r| format!("{:.0}ms", r)).unwrap_or_else(|| "-".into());
                let tput_s = v.last_tput.filter(|&t| t > 0.001).map(|t| {
                    if t >= 0.5 { format!(" {:.1}M", t) } else { format!(" {:.0}K", t * 1000.0) }
                }).unwrap_or_default();
                let load_marker = if load_on && idx == load_focus_idx % views.len() { "▶" } else { "" };
                let lbl = format!("{}UE{} {}{}", load_marker, v.conn.id, rtt_s, tput_s);
                let spark = Sparkline::default()
                    .block(Block::default().borders(Borders::ALL).border_style(border_st).title(lbl))
                    .data(&data)
                    .style(Style::default().fg(stage_color(v.conn.stage)));
                f.render_widget(spark, cells[j]);
            }
        };

        render_spark_row(f, 0, row1_n, spark_rows[0]);
        if row2_n > 0 { render_spark_row(f, row1_n, row2_n, spark_rows[1]); }
    } else {
        let empty = Paragraph::new("  No UEs. Run:  sudo bash clean_start.sh N")
            .style(Style::default().fg(Color::Rgb(165, 150, 125)))
            .block(Block::default().borders(Borders::ALL).border_style(border_st).title(" per-UE RTT "));
        f.render_widget(empty, bottom[0]);
    }

    let sel_id = views.get(sel).map(|v| v.conn.id).unwrap_or(0);
    let log_path = log_nr_ue(sel_id);
    let log_content = std::fs::read_to_string(&log_path).unwrap_or_default();
    let log_lines_all: Vec<&str> = log_content.lines().collect();
    let log_box_h = bottom[1].height.saturating_sub(2) as usize;
    let log_start = log_lines_all.len().saturating_sub(log_box_h);
    let live_log_lines: Vec<Line> = log_lines_all[log_start..].iter()
        .map(|&line| {
            let trimmed = if let Some(p) = line.rfind("] ") { &line[p+2..] } else { line };
            Line::from(Span::styled(trimmed.to_string(), log_style(line)))
        })
        .collect();
    let log_title = if sel_id > 0 {
        format!(" UE{} log (live) ", sel_id)
    } else {
        " UE log ".to_string()
    };
    let live_log = Paragraph::new(live_log_lines)
        .block(Block::default().borders(Borders::ALL).border_style(border_st)
            .title(Span::styled(log_title, Style::default().fg(Color::Rgb(225, 222, 215)).add_modifier(Modifier::BOLD))))
        .wrap(Wrap { trim: true });
    f.render_widget(live_log, bottom[1]);

    let key_act  = Style::default().fg(Color::Rgb(0,  185,  80)).add_modifier(Modifier::BOLD);
    let key_warn = Style::default().fg(Color::Rgb(230,  60, 60)).add_modifier(Modifier::BOLD);
    let key_nav  = Style::default().fg(Color::Rgb(205, 202, 195)).add_modifier(Modifier::BOLD);
    let key_info = Style::default().fg(Color::Rgb(160, 188, 215));
    let key_dim  = Style::default().fg(Color::Rgb(148, 146, 140));
    let key_load = if load_on { Style::default().fg(Color::Rgb(215, 135, 0)).add_modifier(Modifier::BOLD) } else { key_info };
    let keys = Paragraph::new(Line::from(vec![
        Span::styled(" s", key_act), Span::raw(" start  "),
        Span::styled("r", key_nav),  Span::raw(" restart  "),
        Span::styled("x", key_warn), Span::raw(" stop  "),
        Span::styled("+", key_act),  Span::raw(" add-UE  "),
        Span::styled("-", key_dim),  Span::raw(" N  "),
        Span::styled("p", key_info), Span::raw(" ping  "),
        Span::styled("i", key_info), Span::raw(" speedtest  "),
        Span::styled("v", key_info), Span::raw(" video  "),
        Span::styled("L", key_load), Span::raw(if load_on { " load-dist●  " } else { " load-dist  " }),
        Span::styled("g", key_info), Span::raw(" graph  "),
        Span::styled("↑↓", key_dim), Span::raw(" select  "),
        Span::styled("q", key_dim),  Span::raw(" quit  "),
        Span::styled(status, Style::default().fg(Color::Rgb(210, 207, 198))),
    ]));
    f.render_widget(keys, root[3]);
}

fn cmd_monitor(num_ues: Option<usize>, interval_ms: u64) -> Result<(), Box<dyn std::error::Error>> {
    let shared: Shared = Arc::new(Mutex::new(HashMap::new()));
    let mut ping_stops: Vec<Arc<AtomicBool>> = Vec::new();
    let mut ping_on = false;
    let mut tput_stops: HashMap<usize, Arc<AtomicBool>> = HashMap::new();
    let ext_dn_ip = discover_ext_dn_ip().unwrap_or_else(|| "192.168.70.135".to_string());
    let target = ping_target(&ext_dn_ip);
    let srv = iperf_srv(&ext_dn_ip);
    let active_traffic: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let mut prev_traffic_n: usize = 0;
    let load_focus: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let mut load_stop: Option<Arc<AtomicBool>> = None;
    let mut load_on = false;
    let mut n_set = num_ues.unwrap_or_else(|| {
        fs::read_to_string("/tmp/run_nue.n").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(4)
    });
    let mut sel = 0usize;
    let mut metric = ChartMetric::AvgRtt;
    let mut status = String::from("ready — press p to ping, i speedtest, v video, L load-dist");
    const STATUS_DEFAULT: &str = "ready — press p to ping, i speedtest, v video, L load-dist";

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let tick = Duration::from_millis(interval_ms.max(100));

    let res = (|| -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let ue_ids = running_ue_ids();
            let conns: Vec<UeConn> = ue_ids.iter().map(|&i| parse_ue_conn(i)).collect();
            let views: Vec<UeView> = {
                let snap = shared.lock().unwrap();
                conns.into_iter().map(|c| {
                    let m = snap.get(&c.id);
                    UeView {
                        last_rtt: m.and_then(|x| x.last_rtt),
                        recv: m.map(|x| x.recv).unwrap_or(0),
                        dl_mbps: m.map(|x| x.dl_mbps).unwrap_or(0.0),
                        ul_mbps: m.map(|x| x.ul_mbps).unwrap_or(0.0),
                        rtt_hist: m.map(|x| x.rtt.iter().copied().collect()).unwrap_or_default(),
                        last_tput: m.and_then(|x| x.last_tput),
                        tput_hist: m.map(|x| x.tput.iter().copied().collect()).unwrap_or_default(),
                        conn: c,
                    }
                }).collect()
            };
            if !views.is_empty() && sel >= views.len() { sel = views.len() - 1; }

            let data_ids: std::collections::HashSet<usize> =
                views.iter().filter(|v| v.conn.has_tun).map(|v| v.conn.id).collect();
            for &id in &data_ids {
                if !tput_stops.contains_key(&id) {
                    let st = Arc::new(AtomicBool::new(false));
                    let (sh, stc) = (shared.clone(), st.clone());
                    thread::spawn(move || tput_thread(id, sh, stc));
                    tput_stops.insert(id, st);
                }
            }
            tput_stops.retain(|id, st| {
                if data_ids.contains(id) { true } else { st.store(true, Ordering::Relaxed); false }
            });

            if status == STATUS_DEFAULT {
                let has_data = views.iter().any(|v| v.conn.stage == "DATA");
                let has_rrc  = views.iter().any(|v| v.conn.stage == "RRC");
                if has_rrc && has_data {
                    status = "UE stuck at RRC (AMF timing) — retry with: sudo PROXY_WAVE_SIZE=1 bash clean_start.sh N".into();
                }
            }
            if load_on && !views.is_empty() {
                let fi = load_focus.load(Ordering::Relaxed) % views.len();
                let fid = views[fi].conn.id;
                status = format!("LOAD DIST: focus UE{} ({}/{}) pinging all → {} [L to stop]",
                    fid, fi + 1, views.len(), target);
            }
            let gnb_up = gnb_radio_ready();
            let ocu_n = proc_count("ocu -c");
            let odu_n = proc_count("odu -c");
            let gnb_dup = ocu_n > 1 || odu_n > 1;
            let proxy_n = proc_count("zmq_proxy.py");
            let proxy_up = proxy_n > 0;
            let proxy_dup = proxy_n > 1;
            let ps = read_nue_proxy_status();
            let lfi = if load_on && !views.is_empty() { load_focus.load(Ordering::Relaxed) % views.len() } else { 0 };
            terminal.draw(|f| render_dash(f, &views, &ps, gnb_up, gnb_dup, proxy_up, proxy_dup, sel, n_set, ping_on, load_on, lfi, &target, &srv, metric, &status))?;

            if event::poll(tick)? {
                if let Event::Key(k) = event::read()? {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Up   => sel = sel.saturating_sub(1),
                        KeyCode::Down => if sel + 1 < views.len() { sel += 1; },
                        KeyCode::Char('+') | KeyCode::Char('=') => {
                            if ue_ids.is_empty() {
                                n_set = (n_set + 1).min(32);
                            } else {
                                status = format!("adding 1 UE live (UE{} joins, others kept)…", ue_ids.len() + 1);
                                spawn_bg(format!("bash {}/mue_add.sh", scripts_dir()));
                            }
                        }
                        KeyCode::Char('-') | KeyCode::Char('_') => {
                            if ue_ids.is_empty() {
                                n_set = n_set.saturating_sub(1).max(1);
                            } else {
                                status = "live remove not supported — use x to stop all, or r to restart".into();
                            }
                        }
                        KeyCode::Char('g') => metric = match metric {
                            ChartMetric::AvgRtt => ChartMetric::AllRtt,
                            ChartMetric::AllRtt => ChartMetric::SelRtt,
                            ChartMetric::SelRtt => ChartMetric::Bitrate,
                            ChartMetric::Bitrate => ChartMetric::AvgRtt,
                        },
                        KeyCode::Char('s') => {
                            for st in &ping_stops { st.store(true, Ordering::Relaxed); }
                            ping_stops.clear(); ping_on = false;
                            if gnb_up {
                                status = format!("(re)starting {} UEs (your gNB kept)…", n_set);
                                spawn_bg(format!("bash {}/dash_ctl.sh restart {}", scripts_dir(), n_set));
                            } else {
                                status = "gNB is DOWN — start your gNB first, then press s".into();
                            }
                        }
                        KeyCode::Char('r') => {
                            for s in &ping_stops { s.store(true, Ordering::Relaxed); }
                            ping_stops.clear(); ping_on = false;
                            if gnb_up {
                                status = format!("restarting {} UEs (your gNB kept)…", n_set);
                                spawn_bg(format!("bash {}/dash_ctl.sh restart {}", scripts_dir(), n_set));
                            } else {
                                status = "gNB is DOWN — start your gNB first".into();
                            }
                        }
                        KeyCode::Char('x') => {
                            for s in &ping_stops { s.store(true, Ordering::Relaxed); }
                            ping_stops.clear(); ping_on = false;
                            status = "stopping UEs…".into();
                            spawn_bg(format!("bash {}/dash_ctl.sh stop", scripts_dir()));
                        }
                        KeyCode::Char('p') => {
                            if ping_on {
                                for s in &ping_stops { s.store(true, Ordering::Relaxed); }
                                ping_stops.clear(); ping_on = false;
                                status = "ping off".into();
                            } else if !ue_ids.is_empty() {
                                for &i in &ue_ids {
                                    let st = Arc::new(AtomicBool::new(false));
                                    let (sh, tg, stc) = (shared.clone(), target.clone(), st.clone());
                                    thread::spawn(move || ping_thread(i, tg, sh, stc));
                                    ping_stops.push(st);
                                }
                                ping_on = true;
                                status = format!("ping → {} ({} UEs, DL/UL always live)  [g: graph modes]", target, ue_ids.len());
                            } else {
                                status = "no UEs running — press s to start".into();
                            }
                        }
                        KeyCode::Char('i') => {
                            if let Some(s) = &srv {
                                spawn_bg("docker exec oai-ext-dn pkill -9 iperf3 2>/dev/null || true".to_string());
                                spawn_bg("docker exec oai-ext-dn sysctl -qw net.core.rmem_max=67108864 net.core.wmem_max=67108864 net.ipv4.tcp_rmem='4096 1048576 67108864' net.ipv4.tcp_wmem='4096 1048576 67108864' 2>/dev/null || true".to_string());
                                let mut k2 = 0;
                                for v in &views {
                                    if v.conn.has_tun {
                                        let port = 5200u16 + v.conn.id as u16;
                                        spawn_bg(format!(
                                            "docker exec -d oai-ext-dn sh -c 'sleep 2; iperf3 -s -p {}' 2>/dev/null || true", port));
                                        let (sh, sr, ip, id) = (shared.clone(), s.clone(), v.conn.ip.clone(), v.conn.id);
                                        thread::spawn(move || iperf_thread(id, sr, port, ip, sh));
                                        k2 += 1;
                                    }
                                }
                                status = if k2 > 0 { format!("speedtest → {} ({} UEs w/ data plane, ~65s DL+UL)… [press p to see DL/UL live]", s, k2) }
                                         else { "no UEs with a live tunnel (DATA stage) to test".into() };
                            } else {
                                status = "set IPERF_SRV=<ip> to enable speedtest".into();
                            }
                        }
                        KeyCode::Char('v') => {
                            if let Some(s) = &srv {
                                spawn_bg("docker exec oai-ext-dn pkill -9 iperf3 2>/dev/null || true".to_string());
                                let mut k2 = 0;
                                for v in &views {
                                    if v.conn.has_tun {
                                        let port = 5200u16 + v.conn.id as u16;
                                        spawn_bg(format!(
                                            "docker exec -d oai-ext-dn sh -c 'sleep 2; iperf3 -s -p {}' 2>/dev/null || true", port));
                                        let (sr, ip, id) = (s.clone(), v.conn.ip.clone(), v.conn.id);
                                        let at = active_traffic.clone();
                                        at.fetch_add(1, Ordering::Relaxed);
                                        thread::spawn(move || {
                                            traffic_thread(id, sr, port, ip, "video");
                                            at.fetch_sub(1, Ordering::Relaxed);
                                        });
                                        k2 += 1;
                                    }
                                }
                                status = if k2 > 0 { format!("video → {} UEs (30s) — press p then g to watch bitrate", k2) }
                                         else { "no UEs with a live tunnel (DATA stage) to test".into() };
                            } else {
                                status = "set IPERF_SRV=<ip> for traffic tests".into();
                            }
                        }
                        KeyCode::Char('L') | KeyCode::Char('l') => {
                            if load_on {
                                if let Some(s) = &load_stop { s.store(true, Ordering::Relaxed); }
                                load_stop = None;
                                for s in &ping_stops { s.store(true, Ordering::Relaxed); }
                                ping_stops.clear(); ping_on = false; load_on = false;
                                status = "load distribution off".into();
                            } else if !ue_ids.is_empty() {
                                for &i in &ue_ids {
                                    let st = Arc::new(AtomicBool::new(false));
                                    let (sh, tg, stc) = (shared.clone(), target.clone(), st.clone());
                                    thread::spawn(move || ping_thread(i, tg, sh, stc));
                                    ping_stops.push(st);
                                }
                                ping_on = true;
                                load_on = true;
                                load_focus.store(0, Ordering::Relaxed);
                                let stop = Arc::new(AtomicBool::new(false));
                                let focus_c = load_focus.clone();
                                let stop_c  = stop.clone();
                                thread::spawn(move || {
                                    while !stop_c.load(Ordering::Relaxed) {
                                        thread::sleep(Duration::from_secs(5));
                                        if stop_c.load(Ordering::Relaxed) { break; }
                                        focus_c.fetch_add(1, Ordering::Relaxed);
                                    }
                                });
                                load_stop = Some(stop);
                                status = format!("LOAD DIST: pinging all {} UEs, focus cycles every 5s [L to stop]", ue_ids.len());
                            } else {
                                status = "no UEs running — press s to start".into();
                            }
                        }
                        _ => {}
                    }
                }
            }
            let cur_traffic = active_traffic.load(Ordering::Relaxed);
            if cur_traffic == 0 && prev_traffic_n > 0 && !load_on {
                status = "traffic finished — press g (then p if sampling is off) to see throughput graph".to_string();
            }
            prev_traffic_n = cur_traffic;
        }
        Ok(())
    })();

    for s in &ping_stops { s.store(true, Ordering::Relaxed); }
    if let Some(s) = &load_stop { s.store(true, Ordering::Relaxed); }
    for s in tput_stops.values() { s.store(true, Ordering::Relaxed); }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    println!("quit — stopping UEs + proxy and deleting all ue* netns…");
    let _ = Command::new("bash")
        .arg(format!("{}/dash_ctl.sh", scripts_dir()))
        .arg("stop")
        .status();
    println!("done — UEs + netns cleaned. (your gNB is left running.)");
    res
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Commands::Start {
            num_ues, imsi_base, key, opc, dnn, rfsim,
            gnb_dl, gnb_ul, base_port,
            ue_binary, uecap_file, gnb_binary, gnb_config, dl_freq,
            iperf_server,
        } => {
            let ue_bin  = ue_binary.unwrap_or_else(default_ue_binary);
            let uecap   = uecap_file.unwrap_or_else(default_uecap);
            if rfsim {
                let gnb_bin = gnb_binary.unwrap_or_else(default_gnb_binary);
                let gnb_cfg = gnb_config.unwrap_or_else(default_gnb_config);
                let freq    = dl_freq.unwrap_or(3619200000);
                cmd_start_rfsim(num_ues, &imsi_base, &key, &opc, &dnn,
                                ue_bin, uecap, gnb_bin, gnb_cfg, freq, iperf_server);
            } else {
                let freq = dl_freq.unwrap_or(3489420000);
                cmd_start_zmq(num_ues, &imsi_base, &key, &opc, &dnn,
                              &gnb_dl, &gnb_ul, base_port, ue_bin, freq, iperf_server);
            }
        }
        Commands::Stop => cmd_stop(),
        Commands::Status { watch } => {
            if watch { if let Err(e) = run_dashboard() { eprintln!("dashboard error: {}", e); } }
            else     { render_status_once(); }
        }
        Commands::Traffic { ue_id, pattern }  => cmd_traffic(ue_id, &pattern),
        Commands::Logs { source, follow }     => cmd_logs(&source, follow),
        Commands::Monitor { num_ues, interval_ms } => {
            if let Err(e) = cmd_monitor(num_ues, interval_ms) {
                eprintln!("monitor error: {}", e);
            }
        }
    }
}
