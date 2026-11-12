//! 主机指标采集：结构化数据（区别于 MOTD 的终端渲染指令）。
//!
//! - [`HostInfo`]（帧 0x05）：连接时采集一次的静态信息，由 MOTD 同批并行
//!   exec 的原始输出解析而来，不额外开通道；
//! - 动态 Metrics（帧 0x06）：Step 2 起由 MetricsCollector 周期采集。
//!
//! 解析全部为本机纯函数（输入是远端命令文本），可离线单测；
//! 任一字段缺失/异常降级为 `N/A` 或 0，绝不影响连接主流程。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;

pub mod collector;

/// 无法采集时的展示占位（与 MOTD 口径一致）
const NA: &str = "N/A";

/// 主机静态信息（右侧栏「系统信息」区块，0x05 帧 payload）。
/// 数值型字段无法采集时为 0，文本型为 "N/A"，由前端决定占位展示。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HostInfo {
    /// 系统名：Linux / Darwin / ...
    pub os: String,
    /// 发行版全名（PRETTY_NAME，如 Ubuntu 22.04.4 LTS）
    pub distro: String,
    /// 内核版本
    pub kernel: String,
    /// 机器架构 x86_64 / arm64 / aarch64
    pub arch: String,
    /// CPU 型号（/proc/cpuinfo model name）
    pub cpu_model: String,
    /// 逻辑核数 nproc
    pub cores: u32,
    /// 时区，规范化为 "UTC+00:00" 形式
    pub timezone: String,
    /// 系统运行时长中文文本（规范化 uptime 的 "up" 段，
    /// 如 "5 小时 52 分" / "2 天 3 小时 12 分" / "42 分"）
    pub uptime: String,
    /// 默认路由网卡名（ip route 的 dev 字段；可能为空）
    pub iface: String,
    /// 服务器 IPv4（默认路由 src 字段）
    pub ip: String,
    /// 主机是否存在 /proc（false 时动态区块整体不支持，如 macOS/BSD）
    pub procfs: bool,
}

/// 取 raw[key] 首个非空行
fn first_line<'a>(raw: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    raw.get(key)
        .and_then(|s| s.lines().map(str::trim).find(|l| !l.is_empty()))
}

/// uname -srm 首行按空白取第 i 段
fn uname_field(raw: &HashMap<String, String>, i: usize) -> String {
    raw.get("uname")
        .and_then(|s| s.lines().next())
        .and_then(|l| l.split_whitespace().nth(i))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| NA.into())
}

/// /etc/os-release 的 PRETTY_NAME，去掉两侧引号；缺失回退系统名
fn parse_distro(raw: &HashMap<String, String>, fallback_os: &str) -> String {
    raw.get("osrel")
        .and_then(|s| {
            s.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("PRETTY_NAME=")
                    .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
            })
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback_os.to_string())
}

/// ARM Ltd（implementer 0x41）常见 CPU part → 商用名。
/// 未收录的新型号走十六进制 part 兜底，不猜测。
fn arm_part_name(part_hex: &str) -> Option<&'static str> {
    Some(match part_hex.to_ascii_lowercase().as_str() {
        "0xd01" => "ARM Cortex-A32",
        "0xd03" => "ARM Cortex-A53",
        "0xd04" => "ARM Cortex-A35",
        "0xd05" => "ARM Cortex-A55",
        "0xd07" => "ARM Cortex-A57",
        "0xd08" => "ARM Cortex-A72",
        "0xd09" => "ARM Cortex-A73",
        "0xd0a" => "ARM Cortex-A75",
        "0xd0b" => "ARM Cortex-A76",
        "0xd0c" => "ARM Neoverse N1",
        "0xd0d" => "ARM Cortex-A77",
        "0xd0e" => "ARM Cortex-A76AE",
        "0xd40" => "ARM Neoverse V1",
        "0xd41" => "ARM Cortex-A78",
        "0xd42" => "ARM Cortex-A78C",
        "0xd44" => "ARM Cortex-X1",
        "0xd46" => "ARM Cortex-A510",
        "0xd47" => "ARM Cortex-A710",
        "0xd48" => "ARM Cortex-X2",
        "0xd49" => "ARM Neoverse N2",
        "0xd4a" => "ARM Neoverse E1",
        "0xd4d" => "ARM Cortex-A715",
        "0xd4e" => "ARM Cortex-X3",
        "0xd4f" => "ARM Neoverse V2",
        _ => return None,
    })
}

/// Apple（implementer 0x61）M 系列 performance/efficiency core part → 商品名。
fn apple_part_name(part_hex: &str) -> Option<&'static str> {
    Some(match part_hex.to_ascii_lowercase().as_str() {
        "0x022" | "0x023" => "Apple M1",
        "0x024" | "0x025" => "Apple M2",
        _ => return None,
    })
}

/// 解析 CPU 型号。cpuinfo raw 为打标签行（MODEL=/HW=/PART=/IMPL=）：
/// x86 直接取 MODEL；ARM 优先 Hardware，其次 part 映射，再次 implementer 族名；
/// Apple Silicon 虚拟机常只暴露 implementer 0x61，显示 "Apple Silicon"。
fn parse_cpu_model(raw: &HashMap<String, String>) -> String {
    let s = match raw.get("cpuinfo") {
        Some(s) if !s.trim().is_empty() => s,
        _ => return NA.into(),
    };

    let mut model = None;
    let mut hw = None;
    let mut part = None;
    let mut implementer = None;
    for line in s.lines().map(str::trim) {
        let (key, val) = match line.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        let val = val.trim();
        if val.is_empty() {
            continue;
        }
        match key {
            "MODEL" if model.is_none() => model = Some(val),
            "HW" if hw.is_none() => hw = Some(val),
            // 0x000 是虚拟环境的无效占位（Apple VZ），等同无 part
            "PART" if part.is_none() && !val.eq_ignore_ascii_case("0x000") => part = Some(val),
            "IMPL" if implementer.is_none() => implementer = Some(val),
            _ => {}
        }
    }

    if let Some(m) = model {
        return m.to_string();
    }
    if let Some(h) = hw {
        return h.to_string();
    }

    let impl_l = implementer.map(|s| s.to_ascii_lowercase());
    // 有 part：按厂商映射；映射不到显示 "ARM64 · part 0xXXX"
    if let Some(p) = part {
        let mapped = match impl_l.as_deref() {
            Some("0x41") => arm_part_name(p).map(str::to_string),
            Some("0x61") => apple_part_name(p).map(str::to_string),
            _ => None,
        };
        return mapped.unwrap_or_else(|| format!("ARM64 · part {p}"));
    }

    // 无 part（Apple Silicon 虚拟机常见）
    match impl_l.as_deref() {
        Some("0x61") => "Apple Silicon".to_string(),
        Some(imp) => format!("ARM64 · implementer {imp}"),
        None => NA.into(),
    }
}

/// date +%z 的 "+0000" 规范化为 "UTC+00:00"；无法识别返回 N/A
fn parse_timezone(raw: &HashMap<String, String>) -> String {
    let tz = match first_line(raw, "tz") {
        Some(t) => t,
        None => return NA.into(),
    };
    let b = tz.as_bytes();
    if b.len() != 5 || (b[0] != b'+' && b[0] != b'-') || !b[1..].iter().all(u8::is_ascii_digit) {
        return NA.into();
    }
    format!("UTC{}{}:{}", b[0] as char, &tz[1..3], &tz[3..5])
}

/// uptime 开机时长段：" up " 后到第一个逗号，再规范化为中文时长。
///
/// 常见段格式（GNU/busybox 一致）：
/// - `42 min`        → "42 分"
/// - `5:52`          → "5 小时 52 分"（不满 1 天的 HH:MM，易被误读为分:秒）
/// - `2 days,  3:12` → "2 天 3 小时 12 分"
/// - `1 day, 3 min`  → "1 天 3 分"
fn parse_uptime(raw: &HashMap<String, String>) -> String {
    // 时长段从 " up " 开始，到 "N user(s)" 逗号段之前结束——时长本身可能含
    // 一个逗号（"2 days,  3:12"），故先按逗号定位 users 段再拼接其前面的段。
    let seg = raw
        .get("uptime")
        .and_then(|s| s.split(" up ").nth(1))
        .map(|after| {
            let parts: Vec<&str> = after.split(',').collect();
            let user_idx = parts
                .iter()
                .position(|p| p.trim().ends_with(" users") || p.trim().ends_with(" user"));
            match user_idx {
                // 时长 = users 段之前的全部段（至多两段：N days + HH:MM/N min）
                Some(i) => parts[..i].join(","),
                None => parts.first().copied().unwrap_or("").to_string(),
            }
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    seg.map(|s| humanize_uptime(&s))
        .unwrap_or_else(|| NA.into())
}

/// 把 uptime 时长段规范化为中文；无法识别时保留原文（不丢信息）
fn humanize_uptime(seg: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut rest = seg.trim();

    // 含逗号且头段是 "N day/days"：先抽天数，余下部分继续解析
    if let Some((head, tail)) = seg.split_once(',') {
        let head = head.trim();
        if let Some(n) = head
            .strip_suffix("days")
            .or_else(|| head.strip_suffix("day"))
            .map(str::trim)
            .and_then(|t| t.parse::<u32>().ok())
        {
            parts.push(format!("{n} 天"));
            rest = tail.trim();
        }
    }

    if rest.contains(':') {
        // HH:MM（GNU/busybox 均为小时:分钟，不是分:秒）
        let mut it = rest.split(':');
        let h: Option<u32> = it.next().and_then(|t| t.trim().parse().ok());
        let m: Option<u32> = it.next().and_then(|t| t.trim().parse().ok());
        if let Some(h) = h
            && h > 0
        {
            parts.push(format!("{h} 小时"));
        }
        if let Some(m) = m {
            parts.push(format!("{m} 分"));
        }
    } else if let Some(n) = rest
        .strip_suffix("mins")
        .or_else(|| rest.strip_suffix("min"))
        .map(str::trim)
        .and_then(|t| t.parse::<u32>().ok())
    {
        parts.push(format!("{n} 分"));
    }

    if parts.is_empty() {
        seg.trim().to_string()
    } else {
        parts.join(" ")
    }
}

/// 从 MOTD 采集原始输出构建静态主机信息。
///
/// 约定的 raw key：uname / uptime / proc / ip / osrel / cores / cpuinfo / tz
/// （见 `motd::collect_parallel`）。ip 行格式为 "<src 地址> <dev 网卡名>"，
/// 网卡名在回退 hostname -I 时可能缺失。
pub fn build_host_info(raw: &HashMap<String, String>) -> HostInfo {
    let os = uname_field(raw, 0);
    let kernel = uname_field(raw, 1);
    let arch = uname_field(raw, 2);

    let distro = parse_distro(raw, &os);

    let cpu_model = parse_cpu_model(raw);

    let cores = first_line(raw, "cores")
        .and_then(|s| s.split_whitespace().next())
        .and_then(|t| t.parse().ok())
        .unwrap_or(0);

    let timezone = parse_timezone(raw);
    let uptime = parse_uptime(raw);

    let mut ip = NA.to_string();
    let mut iface = String::new();
    if let Some(line) = first_line(raw, "ip") {
        let mut it = line.split_whitespace();
        if let Some(addr) = it.next() {
            ip = addr.to_string();
        }
        iface = it.next().unwrap_or("").to_string();
    }

    // proc 计数命令在无 /proc 主机上输出空；能解析出数字即证明 /proc 可用
    let procfs = first_line(raw, "proc").is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit()));

    HostInfo {
        os,
        distro,
        kernel,
        arch,
        cpu_model,
        cores,
        timezone,
        uptime,
        iface,
        ip,
        procfs,
    }
}

// ───────────────────────── 动态指标（0x06） ─────────────────────────

/// 每轮都跑的高频段：CPU/负载/内存/Swap/网卡/进程（procfs，开销极低）。
/// 进程段先输出 CLK_TCK/PAGESIZE 两个换算常量（各占一行，缺失时解析端回退默认值），
/// 再逐行输出每个进程的原始 /proc/pid/stat（CPU 差值需每轮连续快照）。
/// docker 扫描（cgroup+容器名映射）开销大，独立成按需段：首帧/周期全量、
/// 其余轮只增量扫新 PID，名称映射单独缓存——否则整轮超 3s 时限会把磁盘段一起拖垮。
const METRICS_SCRIPT_BASE: &str = "echo @@STAT@@\nhead -n 1 /proc/stat 2>/dev/null\necho @@LOAD@@\ncat /proc/loadavg 2>/dev/null\necho @@MEM@@\ncat /proc/meminfo 2>/dev/null\necho @@NET@@\ncat /proc/net/dev 2>/dev/null\necho @@PROC@@\ngetconf CLK_TCK 2>/dev/null\ngetconf PAGESIZE 2>/dev/null\nfor p in /proc/[0-9]*; do read -r s < \"$p/stat\" 2>/dev/null && echo \"$s\"; done\n";
/// docker cgroup 全量扫描段：逐进程提取 cgroup 十六进制容器 ID 并输出
/// `cgpid:<pid>:<cid>`；一次批量 grep + awk 完成逐进程提取，避免千级进程主机
/// 逐 PID spawn grep/head 超过整轮 3s 时限（实测 5018ms → 1099ms）。
/// 全量扫描开销大，日常只对新增 PID 做增量扫描，仅首帧与周期兜底走全量。
const DOCKER_CGROUP_ALL_SECTION: &str = "echo @@DOCKER@@\ngrep -aHoE '[a-f0-9]{12,64}' /proc/[0-9]*/cgroup 2>/dev/null | awk -F'[:/]' '!seen[$3]++ {print \"cgpid:\" $3 \":\" $NF}'\n";
/// 容器 ID→容器名映射段：优先 `docker ps`（daemon 查询，不依赖 data-root 路径，
/// rootless/snap/自定义目录都覆盖），CLI 缺席时才回退枚举 <data-root>/containers/*/config.v2.json。
/// 名称变化缓慢，collector 只在首次/出现未知容器时查询并跨轮缓存。
const DOCKER_NAMES_SECTION: &str = "echo @@DOCKERNAMES@@\nif command -v docker >/dev/null 2>&1; then docker ps --no-trunc --format '{{.ID}} {{.Names}}' 2>/dev/null | while read -r cid nm; do [ -n \"$cid\" ] && [ -n \"$nm\" ] && echo \"docker:$cid:$nm\"; done; else dr=''; if [ -f /etc/docker/daemon.json ]; then dr=$(sed -n 's/.*\"data-root\"[[:space:]]*:[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p' /etc/docker/daemon.json | head -1); fi\nfor root in \"$dr\" /var/lib/docker /home/docker \"$HOME/.local/share/docker\"; do [ -n \"$root\" ] && [ -d \"$root/containers\" ] || continue; for f in \"$root\"/containers/*/config.v2.json; do [ -f \"$f\" ] || continue; cid=${f##*/containers/}; cid=${cid%%/*}; name=$(grep -o '\"Name\":\"[^\"]*\"' \"$f\" 2>/dev/null | head -1 | cut -d'\"' -f4); [ -n \"$name\" ] && echo \"docker:$cid:${name#/}\"; done; done; fi\n";
/// 磁盘低频段：每 [`DISK_INTERVAL_ROUNDS`] 轮夹带一次（df 需枚举挂载点，开销略高）。
/// 60s 量级足够——容量变化慢，且非夹带轮沿用缓存，界面不会空窗。
/// df 段脚本：优先 `df -PkT`（GNU/busybox 支持额外输出文件系统类型列，
/// FinalShell 磁盘表同款信息）；macOS/BSD 的 df 无 -T 会整体失败，
/// `||` 回落到纯 `df -Pk`（解析端兼容两种列布局）。
const DISK_SCRIPT_SECTION: &str = "echo @@DISK@@\ndf -PkT 2>/dev/null || df -Pk 2>/dev/null\n";
/// GPU 段：nvidia-smi 每张卡输出一行 CSV（noheader/nounits），字段顺序固定 8 列：
/// index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,power.limit。
/// 仅 collector 静态探测确认有 GPU 的主机才每轮夹带；无卡/无命令主机探测一次后永久跳过
///（nvidia-smi 冷启动 100~300ms，不能每轮白付）。
const GPU_SCRIPT_SECTION: &str = "echo @@GPU@@\nnvidia-smi --query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,power.limit --format=csv,noheader,nounits 2>/dev/null\n";

/// docker cgroup 扫描范围：按轮次需要选择全量/增量/仅名称，避免每轮都全量 grep。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DockerScanScope<'a> {
    /// 本轮不涉及 docker
    Off,
    /// 只刷新容器名映射（一次 docker ps），不动 cgroup 归属
    NamesOnly,
    /// 全量扫描所有 /proc/<pid>/cgroup（首帧/周期兜底）
    All,
    /// 只扫描指定 PID（增量发现新进程），PID 由数字格式化，无注入面
    Pids(&'a [u32]),
}

/// 增量扫描段：只 grep 指定 PID 的 cgroup 文件。
/// `pids` 必须非空（空列表会让 grep 读 stdin 卡到超时）。
fn docker_cgroup_pids_section(pids: &[u32]) -> String {
    let mut files = String::new();
    for pid in pids {
        files.push_str(&format!("/proc/{pid}/cgroup "));
    }
    format!(
        "grep -aHoE '[a-f0-9]{{12,64}}' {files}2>/dev/null | awk -F'[:/]' '!seen[$3]++ {{print \"cgpid:\" $3 \":\" $NF}}'\n"
    )
}

/// 构造单轮采集脚本：一次 exec 取回全部数据，marker 行分段。
/// `with_disk` 时夹带 `df -Pk` 段；`docker` 指定 cgroup 扫描范围
/// （行协议见 DOCKER_MAP_PREFIX/CGPID_PREFIX）；`with_docker_names` 时夹带
/// 容器名查询段（需同时带 `@@DOCKER@@` 标记，故 scope 不能为 Off）；
/// `with_gpu` 时夹带 nvidia-smi 段。
/// 由 `/bin/sh -c` 执行，busybox 兼容。
pub(crate) fn metrics_script(
    with_disk: bool,
    with_gpu: bool,
    docker: DockerScanScope<'_>,
    with_docker_names: bool,
) -> String {
    let mut s = String::from(METRICS_SCRIPT_BASE);
    if with_disk {
        s.push_str(DISK_SCRIPT_SECTION);
    }
    match docker {
        DockerScanScope::Off => {}
        DockerScanScope::NamesOnly => s.push_str("echo @@DOCKER@@\n"),
        DockerScanScope::All => s.push_str(DOCKER_CGROUP_ALL_SECTION),
        DockerScanScope::Pids(pids) => {
            s.push_str("echo @@DOCKER@@\n");
            if !pids.is_empty() {
                s.push_str(&docker_cgroup_pids_section(pids));
            }
        }
    }
    if with_docker_names && docker != DockerScanScope::Off {
        s.push_str(DOCKER_NAMES_SECTION);
    }
    if with_gpu {
        s.push_str(GPU_SCRIPT_SECTION);
    }
    s.push_str("echo @@END@@");
    s
}

/// `@@PROC@@` 段实际落输出：`docker:<容器全 ID>:<容器名>`（名可含冒号）
const DOCKER_MAP_PREFIX: &str = "docker:";
/// `cgpid:<pid>:<容器ID>`（脚本侧已从 /proc/pid/cgroup 提取十六进制 ID）
const CGPID_PREFIX: &str = "cgpid:";



/// /proc/stat 首行的累计 CPU 节拍快照。idle 按惯例包含 iowait。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CpuTimes {
    /// 全部节拍（user+nice+system+idle+iowait+irq+softirq+steal）
    pub total: u64,
    /// 空闲节拍（idle + iowait）
    pub idle: u64,
}

impl CpuTimes {
    /// 解析 `/proc/stat` 聚合行：`cpu  user nice system idle iowait irq softirq steal ...`
    fn parse(line: &str) -> Option<Self> {
        let mut it = line.split_whitespace();
        if it.next()? != "cpu" {
            return None;
        }
        let fields: Vec<u64> = it.map(|t| t.parse().unwrap_or(0)).collect();
        if fields.len() < 4 {
            return None;
        }
        // 只累加前 8 列；后续 guest/guest_nice 已包含在 user 中，重复计入会失真
        let total = fields.iter().take(8).sum();
        let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
        Some(Self { total, idle })
    }
}

/// 相邻两次 /proc/stat 快照的 CPU 使用率（0~100，保留小数）。
/// total 无增量（同值快照）时返回 0，避免除零。
pub(crate) fn cpu_util(prev: CpuTimes, cur: CpuTimes) -> f32 {
    let dt = cur.total.saturating_sub(prev.total);
    if dt == 0 {
        return 0.0;
    }
    let busy = dt.saturating_sub(cur.idle.saturating_sub(prev.idle));
    ((busy as f32 / dt as f32) * 100.0).clamp(0.0, 100.0)
}

/// 跨采集任务保留的 CPU 采样状态。collector 会因切换会话、面板隐藏等停止并在
/// 返回时重启；若上一快照随任务丢掉，重启后的首帧没有差值可算，只能推 0%，
/// 界面就出现「CPU 忽然归零」。把快照与上次利用率存在会话上，重启首帧仍可
/// 给出真实值（间隔很短）或沿用上次值（暂停过久）。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CpuSampleState {
    /// 上一次 /proc/stat 快照
    pub prev: Option<CpuTimes>,
    /// 最近一次算出的利用率；无可用差值时沿用它，不报 0
    pub util: f32,
    /// prev 的采样时刻
    pub at: Option<Instant>,
}

/// 用本轮快照推进 CPU 状态并返回利用率。
/// 距上一快照在 `max_gap` 内才做差值，暂停过久则沿用上次值，
/// 避免把整段暂停时间平均进来（或直接报 0）。纯函数便于单测。
pub(crate) fn advance_cpu(
    state: &mut CpuSampleState,
    cur: CpuTimes,
    now: Instant,
    max_gap: Duration,
) -> f32 {
    let util = match (state.prev, state.at) {
        // 快照倒退（远端重启）或同值也不算差值，沿用上次值而不是报 0
        (Some(prev), Some(at))
            if now.saturating_duration_since(at) <= max_gap && cur.total > prev.total =>
        {
            cpu_util(prev, cur)
        }
        _ => state.util,
    };
    *state = CpuSampleState {
        prev: Some(cur),
        util,
        at: Some(now),
    };
    util
}

/// 解析 `/proc/loadavg` 首行：`0.42 0.38 0.31 2/321 12345`，取前 3 列
/// （1/5/15 分钟平均运行队列长度，非百分比）。缺列/非法返回 None。
pub(crate) fn parse_loadavg(line: &str) -> Option<[f32; 3]> {
    let mut it = line.split_whitespace();
    let l1 = it.next()?.parse().ok()?;
    let l5 = it.next()?.parse().ok()?;
    let l15 = it.next()?.parse().ok()?;
    Some([l1, l5, l15])
}

/// 内存与 Swap 快照（字节）。无法采集字段为 0；
/// 内存 used = total - available，Swap used = swap_total - swap_free。
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq)]
pub(crate) struct MemMetrics {
    /// MemTotal（字节）
    pub total: u64,
    /// MemAvailable；极老内核无此值时回退 MemFree（字节）
    pub available: u64,
    /// 已用 = total - available（字节）
    pub used: u64,
    /// SwapTotal（字节，无 swap 主机为 0，前端据此隐藏 Swap 区块）
    pub swap_total: u64,
    /// SwapFree（字节）
    pub swap_free: u64,
    /// Swap 已用 = swap_total - swap_free（字节）
    pub swap_used: u64,
}

impl MemMetrics {
    /// 解析 `/proc/meminfo`：数值列单位固定为 kB，换算为字节。
    fn parse(text: &str) -> Self {
        let mut total_kb = None;
        let mut available_kb = None;
        let mut free_kb = None;
        let mut swap_total_kb = None;
        let mut swap_free_kb = None;
        for line in text.lines() {
            let (key, rest) = match line.split_once(':') {
                Some(kv) => kv,
                None => continue,
            };
            let kb = rest
                .split_whitespace()
                .next()
                .and_then(|t| t.parse::<u64>().ok());
            match key.trim() {
                "MemTotal" => total_kb = kb,
                "MemAvailable" => available_kb = kb,
                "MemFree" => free_kb = kb,
                "SwapTotal" => swap_total_kb = kb,
                "SwapFree" => swap_free_kb = kb,
                _ => {}
            }
        }
        let total = total_kb.unwrap_or(0) * 1024;
        let available = available_kb.or(free_kb).unwrap_or(0) * 1024;
        let swap_total = swap_total_kb.unwrap_or(0) * 1024;
        let swap_free = swap_free_kb.unwrap_or(0) * 1024;
        MemMetrics {
            total,
            available,
            used: total.saturating_sub(available),
            swap_total,
            swap_free,
            swap_used: swap_total.saturating_sub(swap_free),
        }
    }
}

/// 选定网卡（或非 lo 网卡汇总）的收发计数快照，取自 `/proc/net/dev`。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct NetCounters {
    /// 累计接收字节
    pub rx_bytes: u64,
    /// 累计发送字节
    pub tx_bytes: u64,
}

impl NetCounters {
    /// 解析 `/proc/net/dev`。
    ///
    /// 行格式（冒号后固定 16 列）：
    /// `  eth0: rxBytes rxPkts ... | txBytes txPkts ...`
    /// 仅取收/发字节（rx=列0、tx=列8）；选择口径：`iface` 命中时只取该网卡
    /// （默认路由网卡），`iface` 为 None 或指定网卡不存在时汇总全部非 lo 网卡。
    fn parse(text: &str, iface: Option<&str>) -> Option<Self> {
        let mut wanted: Option<NetCounters> = None;
        let mut sum = NetCounters::default();
        let mut any = false;
        for line in text.lines() {
            let (name, rest) = match line.split_once(':') {
                Some(kv) => kv,
                // 表头两行无冒号，直接跳过
                None => continue,
            };
            let name = name.trim();
            if name.is_empty() || name == "lo" {
                continue;
            }
            let fields: Vec<u64> = rest
                .split_whitespace()
                .map(|t| t.parse().unwrap_or(0))
                .collect();
            // 只需收/发字节：rx 列0、tx 列8，故至少 9 列
            if fields.len() < 9 {
                continue;
            }
            let cur = NetCounters {
                rx_bytes: fields[0],
                tx_bytes: fields[8],
            };
            any = true;
            sum.rx_bytes += cur.rx_bytes;
            sum.tx_bytes += cur.tx_bytes;
            if iface.is_some_and(|i| i == name) {
                wanted = Some(cur);
            }
        }
        // 未出现任何网卡行（段缺失/空文件）返回 None，调用方据此不更新 prev 快照
        if any {
            Some(wanted.unwrap_or(sum))
        } else {
            None
        }
    }
}

/// 相邻两次网卡计数的字节速率（bytes/s）：字节增量 ÷ 墙钟秒数。
/// 计数器倒退（异常快照）saturating 为 0；dt 非正返回 0，避免除零。
pub(crate) fn byte_rate(prev: u64, cur: u64, dt: Duration) -> f64 {
    let secs = dt.as_secs_f64();
    if secs <= f64::EPSILON {
        return 0.0;
    }
    cur.saturating_sub(prev) as f64 / secs
}

/// 磁盘（挂载路径表）低频夹带轮次：默认 2s 间隔下约每 60s 一次。
/// 首帧（第 0 轮）无条件采一次，面板打开即有磁盘数据。
pub(crate) const DISK_INTERVAL_ROUNDS: u32 = 30;

/// 单个挂载点磁盘用量（字节），取自 `df -PkT`/`df -Pk` 行；百分比由后端按 used/total 算好。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct DiskMetrics {
    /// 挂载点（POSIX `-P` 保证其位于行尾，允许含空格）
    pub mount: String,
    /// 文件系统类型（ext4/xfs/overlay/tmpfs…，FinalShell 磁盘表同款列）；
    /// df 不支持 -T 时为空串，前端不展示
    pub fs: String,
    /// 总容量（字节）
    pub total: u64,
    /// 已用（字节）
    pub used: u64,
    /// 可用（字节）
    pub available: u64,
    /// 使用率百分比 0~100（total=0 时为 0）
    pub pct: u8,
}

/// df 第一列中纯内核伪文件系统：监控无意义。overlay/真实块设备（/dev/*）不在此列。
/// tmpfs/devtmpfs/none 刻意不滤：它们有真实容量（/dev/shm、/run、udev），
/// FinalShell 的磁盘表按「全挂载点列表」展示，滤掉它们正是「比 FinalShell 少很多」
/// 的主因之一；proc/sysfs/cgroup 等零容量伪 fs 由 total==0 兜底滤除。
const PSEUDO_FS: &[&str] = &[
    "sysfs",
    "proc",
    "cgroup",
    "cgroup2",
    "mqueue",
    "devpts",
    "securityfs",
    "pstore",
    "debugfs",
    "tracefs",
    "configfs",
    "fusectl",
    "hugetlbfs",
    "ramfs",
    "autofs",
    "binfmt_misc",
    "nsfs",
    "rpc_pipefs",
    "selinuxfs",
    "bpf",
    "efivarfs",
];

/// Docker/容器运行时注入的单文件 bind 挂载：df 里表现为独立条目，
/// 但与根分区同设备（如 /dev/vdb1 → /etc/hosts），展示即重复噪音。
const BIND_MOUNT_BLACKLIST: &[&str] = &["/etc/hosts", "/etc/resolv.conf", "/etc/hostname"];

/// 挂载点层级深度：组件数越少越浅（`/` 为 0 最浅）
fn mount_depth(mount: &str) -> usize {
    if mount == "/" {
        return 0;
    }
    mount.trim_end_matches('/').matches('/').count()
}

impl DiskMetrics {
    /// 解析一行 df 输出（兼容两种列布局，表头/残缺行/伪 fs/黑名单挂载点返回 None）：
    /// - `df -Pk`：  `Filesystem 1024-blocks Used Available Capacity Mounted on`（6 列）
    /// - `df -PkT`：`Filesystem Type 1024-blocks Used Available Capacity Mounted on`（7 列）
    /// 数值 kB 换算为字节。
    fn parse_df_line(line: &str) -> Option<Self> {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 {
            return None;
        }
        // -PkT 比 -Pk 多一个 Type 列：判定依据是第 2 列非数字且第 3 列为数字
        //（6 列布局第 2 列必为数字；Type 恒非数字，ext4/xfs/overlay/tmpfs…）
        let (device, fs, rest) = match fields[1].parse::<u64>() {
            Err(_) if fields[2].parse::<u64>().is_ok() => (fields[0], fields[1], &fields[2..]),
            _ => (fields[0], "", &fields[1..]),
        };
        if PSEUDO_FS.contains(&device) {
            return None;
        }
        // 表头与非数字行自然在此过滤
        let total_kb: u64 = rest[0].parse().ok()?;
        let used_kb: u64 = rest[1].parse().ok()?;
        let avail_kb: u64 = rest[2].parse().ok()?;
        // 数值 kB 换算为字节；rest[0..4] = total/used/avail/capacity，
        // POSIX -P：挂载点恒在行尾（含空格时占多列）
        let mount = rest[4..].join(" ");
        if mount.is_empty() || BIND_MOUNT_BLACKLIST.contains(&mount.as_str()) {
            return None;
        }
        let total = total_kb * 1024;
        if total == 0 {
            return None;
        }
        let used = used_kb * 1024;
        let available = avail_kb * 1024;
        let pct = ((used as u128 * 100 + total as u128 / 2) / total as u128).min(100) as u8;
        Some(DiskMetrics {
            mount,
            fs: fs.to_string(),
            total,
            used,
            available,
            pct,
        })
    }
}

/// 解析 `df -PkT`/`df -Pk` 整段：过滤内核伪 fs/容器注入挂载，重复挂载点去重，
/// 按挂载点由浅到深排序。
///
/// 与 FinalShell 对齐（其磁盘表为全挂载点列表）：不再按容量三元组去重——
/// 同容量去重会吞掉 btrfs 子卷/同规格多分区，更会把容器里每层 overlay
/// 全部吞进根分区（FinalShell 对这些逐条展示）。零容量伪 fs 由
/// `parse_df_line` 的 total==0 兜底滤除，表体在前端内部滚动。
pub(crate) fn parse_disks(text: &str) -> Vec<DiskMetrics> {
    let mut disks: Vec<DiskMetrics> = Vec::new();
    for line in text.lines().skip(1) {
        // skip(1) 丢表头；行首空白不影响 split_whitespace
        if let Some(d) = DiskMetrics::parse_df_line(line)
            && !disks.iter().any(|x| x.mount == d.mount)
        {
            disks.push(d);
        }
    }
    // 更浅的挂载点在前；同深按路径字典序
    disks.sort_by(|a, b| {
        mount_depth(&a.mount)
            .cmp(&mount_depth(&b.mount))
            .then_with(|| a.mount.cmp(&b.mount))
    });
    disks
}

/// MiB → 字节（nvidia-smi 显存单位为二进制 MiB）
const MIB: u64 = 1024 * 1024;

/// 单卡 GPU 动态值（nvidia-smi CSV 一行；字段与前端 GpuMetricsData 对齐）
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct GpuItemPayload {
    /// 卡序号（nvidia-smi index）
    pub index: u32,
    /// 型号名
    pub name: String,
    /// 核心利用率 %
    pub util: f32,
    /// 显存已用（字节）
    pub mem_used: u64,
    /// 显存总量（字节）
    pub mem_total: u64,
    /// 核心温度 °C
    pub temp: f32,
    /// 当前功耗 W
    pub power: f32,
    /// 功耗上限 W（不支持/未知时为 0，前端隐藏功耗）
    pub power_limit: f32,
}

/// nvidia-smi 浮点字段解析：nounits 模式下不支持的字段输出 `[N/A]`，解析失败统一记 0。
fn parse_gpu_f32(s: &str) -> f32 {
    s.trim().parse().unwrap_or(0.0)
}

/// 解析 nvidia-smi `--format=csv,noheader,nounits` 整段：每卡一行固定 8 列。
/// 缺列/残行/空型号跳过；显存 MiB 换算字节；结果按卡序号升序。段为空（无卡/无命令）
/// 时返回空 vec，collector 据此永久关闭 GPU 段。
pub(crate) fn parse_gpus(text: &str) -> Vec<GpuItemPayload> {
    let mut gpus = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if fields.len() != 8 {
            continue;
        }
        let Ok(index) = fields[0].parse::<u32>() else {
            continue;
        };
        let name = fields[1].trim();
        if name.is_empty() {
            continue;
        }
        let mem_used_mib: u64 = fields[3].parse().unwrap_or(0);
        let mem_total_mib: u64 = fields[4].parse().unwrap_or(0);
        gpus.push(GpuItemPayload {
            index,
            name: name.to_string(),
            util: parse_gpu_f32(fields[2]),
            mem_used: mem_used_mib.saturating_mul(MIB),
            mem_total: mem_total_mib.saturating_mul(MIB),
            temp: parse_gpu_f32(fields[5]),
            power: parse_gpu_f32(fields[6]),
            power_limit: parse_gpu_f32(fields[7]),
        });
    }
    gpus.sort_by_key(|g| g.index);
    gpus
}

/// Top 进程榜单条数（与右侧栏进程表展示行数一致）
pub(crate) const TOP_PROCS: usize = 5;
/// Linux 绝大多数平台的时钟节拍/页大小回退值（getconf 不可用时）
const FALLBACK_CLK_TCK: u64 = 100;
const FALLBACK_PAGE_SIZE: u64 = 4096;

/// 单个进程的一轮 /proc/pid/stat 原始快照（CPU 时间为累计节拍，rss 为页数）
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProcSnapshot {
    pub pid: u32,
    /// comm（可含空格/括号，已从 () 中取出，未做截断）
    pub name: String,
    /// utime+stime 累计（字段 14+15，单位 CLK_TCK）
    pub cpu_ticks: u64,
    /// 常驻内存页数（字段 24）
    pub rss_pages: u64,
    /// 所属 docker 容器名（非容器进程为空串；容器识别见 docker_container_id）
    pub container: String,
}

/// 进程段原始结果：换算常量 + 全量进程快照（total = 列表长度）
#[derive(Debug, Clone)]
pub(crate) struct ProcRaw {
    pub clk_tck: u64,
    pub page_size: u64,
    pub procs: Vec<ProcSnapshot>,
}

impl ProcSnapshot {
    /// 解析一行 `/proc/pid/stat`。进程名可能含空格/括号，必须按首个 '(' 与
    /// 最后一个 ')' 定位；')' 之后字段 3(state) 为索引 0，故：
    /// utime=字段14→索引11，stime=15→12，rss=字段24→索引21。
    fn parse_stat_line(line: &str) -> Option<Self> {
        let open = line.find('(')?;
        let close = line.rfind(')')?;
        if close <= open {
            return None;
        }
        let pid: u32 = line[..open].trim().parse().ok()?;
        let name = line[open + 1..close].to_string();
        let rest: Vec<&str> = line[close + 1..].split_whitespace().collect();
        // 至少需要到 rss（')' 后第 22 个 token）
        if rest.len() < 22 {
            return None;
        }
        let utime: u64 = rest[11].parse().ok()?;
        let stime: u64 = rest[12].parse().ok()?;
        let rss_pages: u64 = rest[21].parse().ok()?;
        Some(ProcSnapshot {
            pid,
            name,
            cpu_ticks: utime + stime,
            rss_pages,
            container: String::new(),
        })
    }
}

/// 解析 `@@PROC@@` 段：前两行若为纯数字依次为 CLK_TCK/PAGESIZE（缺失回退
/// 100/4096），其余为 stat 行。docker 行在独立 `@@DOCKER@@` 段，由
/// [`parse_docker_scan`] 另行消费。
/// 段不存在/无有效进程返回 None（collector 沿用缓存）。
pub(crate) fn parse_procs(text: &str) -> Option<ProcRaw> {
    let mut clk_tck = FALLBACK_CLK_TCK;
    let mut page_size = FALLBACK_PAGE_SIZE;
    let mut procs = Vec::new();
    let mut header_step = 0u8;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // 段头最多消费前两个纯数字行；stat 行首形如 "1 (init) ..."，不会误判
        if header_step < 2 && trimmed.bytes().all(|b| b.is_ascii_digit()) {
            let v: u64 = trimmed.parse().ok()?;
            if header_step == 0 {
                if v > 0 {
                    clk_tck = v;
                }
            } else if v > 0 {
                page_size = v;
            }
            header_step += 1;
            continue;
        }
        header_step = 2;
        if let Some(p) = ProcSnapshot::parse_stat_line(trimmed) {
            procs.push(p);
        }
    }
    (!procs.is_empty()).then_some(ProcRaw {
        clk_tck,
        page_size,
        procs,
    })
}

/// docker 扫描结果（按轮次增量采集，collector 跨轮缓存）：
/// 进程 pid→容器 ID（cgpid 行）、容器 ID→容器名（docker 行，ID 全小写）。
#[derive(Debug, Clone, Default)]
pub(crate) struct DockerScan {
    pub names: std::collections::HashMap<String, String>,
    pub cg_ids: std::collections::HashMap<u32, String>,
    /// 本帧是否执行了容器名查询段：true 时 names 是完整快照（可整体替换），
    /// false 时 names 为空、调用方保留旧缓存
    pub names_scanned: bool,
}

/// 容器名查询：全 ID 精确命中优先，其次以 cgpid 的截断 ID 为前缀匹配
pub(crate) fn container_name_for<'a>(
    names: &'a std::collections::HashMap<String, String>,
    cid: &str,
) -> Option<&'a str> {
    if let Some(n) = names.get(cid) {
        return Some(n);
    }
    names
        .iter()
        .find(|(id, _)| id.starts_with(cid))
        .map(|(_, n)| n.as_str())
}

/// 解析 `@@DOCKER@@` 段中的 docker 行：
/// `docker:<容器全 ID>:<容器名>` 与 `cgpid:<pid>:<容器ID>`。
/// 段是否存在由 [`parse_metrics_output`] 判定，这里只做行解析。
pub(crate) fn parse_docker_scan(text: &str) -> DockerScan {
    let mut scan = DockerScan::default();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(DOCKER_MAP_PREFIX) {
            if let Some((id, name)) = rest.split_once(':') {
                if !id.is_empty() && !name.is_empty() {
                    scan.names
                        .insert(id.trim().to_ascii_lowercase(), name.trim().to_string());
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix(CGPID_PREFIX) {
            if let Some((pid_s, cid)) = rest.split_once(':') {
                if let Ok(pid) = pid_s.parse::<u32>() {
                    if !cid.is_empty() {
                        scan.cg_ids
                            .insert(pid, cid.trim().to_ascii_lowercase());
                    }
                }
            }
        }
    }
    scan
}

/// 单轮采集的原始解析结果（尚未与上一轮做差）
#[derive(Debug, Clone, Default)]
pub(crate) struct RawMetrics {
    pub cpu: Option<CpuTimes>,
    /// 1/5/15 分钟负载；loadavg 缺失时为 None
    pub load: Option<[f32; 3]>,
    pub mem: MemMetrics,
    /// 选定网卡计数；/proc/net/dev 缺失时为 None（collector 不更新 prev）
    pub net: Option<NetCounters>,
    /// df 段只在夹带轮存在；None 表示本轮未采磁盘，collector 沿用上一次缓存
    pub disks: Option<Vec<DiskMetrics>>,
    /// 进程段每轮都采；None（段缺失/空）时 collector 沿用上一次榜单
    pub procs: Option<ProcRaw>,
    /// docker 段仅 docker 轮夹带；None 表示本轮未采，collector 沿用缓存。
    /// Some(空) 表示扫描成功但无容器（或仅名称查询），collector 据此清空归属缓存
    pub docker: Option<DockerScan>,
    /// GPU 段仅支持的主机夹带；None=本轮未带段，Some(vec![])=带段但无卡/命令失败，
    /// Some(非空)=正常卡列表。collector 据 Some(空) 永久关闭后续 GPU 段
    pub gpus: Option<Vec<GpuItemPayload>>,
}

/// 解析 [`metrics_script`] 的输出：扫描 marker 行确定分段，再分别解析。
/// `iface` 为默认路由网卡名（0x05 已采集），网络计数优先取该网卡；
/// 缺段/空输出时对应字段降级为 None/0，不报错。
pub(crate) fn parse_metrics_output(text: &str, iface: Option<&str>) -> RawMetrics {
    let mut cpu = None;
    let mut load = None;
    let mut section = "";
    let mut mem_lines = String::new();
    let mut net_lines = String::new();
    let mut proc_lines = String::new();
    let mut disk_lines = String::new();
    let mut disk_present = false;
    let mut docker_lines = String::new();
    let mut docker_present = false;
    let mut docker_names_present = false;
    let mut gpu_lines = String::new();
    let mut gpu_present = false;

    for line in text.lines() {
        let trimmed = line.trim();
        match trimmed {
            "@@STAT@@" => section = "stat",
            "@@LOAD@@" => section = "load",
            "@@MEM@@" => section = "mem",
            "@@NET@@" => section = "net",
            "@@PROC@@" => section = "proc",
            "@@DOCKER@@" => {
                section = "docker";
                docker_present = true;
            }
            "@@DOCKERNAMES@@" => {
                section = "docker";
                docker_names_present = true;
            }
            "@@DISK@@" => {
                section = "disk";
                disk_present = true;
            }
            "@@GPU@@" => {
                section = "gpu";
                gpu_present = true;
            }
            "@@END@@" => section = "",
            _ if section == "stat" && cpu.is_none() => {
                if let Some(t) = CpuTimes::parse(trimmed) {
                    cpu = Some(t);
                }
            }
            _ if section == "load" && load.is_none() => {
                if let Some(l) = parse_loadavg(trimmed) {
                    load = Some(l);
                }
            }
            _ if section == "mem" => {
                mem_lines.push_str(line);
                mem_lines.push('\n');
            }
            _ if section == "net" => {
                net_lines.push_str(line);
                net_lines.push('\n');
            }
            _ if section == "proc" => {
                proc_lines.push_str(line);
                proc_lines.push('\n');
            }
            _ if section == "docker" => {
                docker_lines.push_str(line);
                docker_lines.push('\n');
            }
            _ if section == "disk" => {
                disk_lines.push_str(line);
                disk_lines.push('\n');
            }
            _ if section == "gpu" => {
                gpu_lines.push_str(line);
                gpu_lines.push('\n');
            }
            _ => {}
        }
    }
    let mem = MemMetrics::parse(&mem_lines);
    let net = NetCounters::parse(&net_lines, iface);
    let procs = parse_procs(&proc_lines);
    // docker 段只在 docker 轮存在：marker 存在即为权威快照（哪怕无容器），
    // 缺 marker 才是「本轮未采集」；名称段是否执行由 @@DOCKERNAMES@@ 单独标记
    let docker = if docker_present {
        let mut scan = parse_docker_scan(&docker_lines);
        scan.names_scanned = docker_names_present;
        Some(scan)
    } else {
        None
    };
    // df 段缺失（非夹带轮）或存在但为空（df 失败）都返回 None：
    // 前者本就无新数据，后者按静默降级沿用上一次缓存，不把列表清空。
    let disks = if disk_present {
        let parsed = parse_disks(&disk_lines);
        (!parsed.is_empty()).then_some(parsed)
    } else {
        None
    };
    // GPU 段与磁盘不同：marker 存在时空输出也要保留为 Some(空 vec)，
    // collector 靠它区分「未带段」(None) 与「带段但无卡」(Some([])→永久关闭)
    let gpus = if gpu_present {
        Some(parse_gpus(&gpu_lines))
    } else {
        None
    };

    RawMetrics {
        cpu,
        load,
        mem,
        net,
        disks,
        procs,
        docker,
        gpus,
    }
}

/// 0x06 帧 payload：动态指标 JSON（snake_case 紧凑序列化）。
/// 当前含 cpu(util/load)/mem(含 swap)/net/disks/procs，Step 7 向后兼容扩展 gpus。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct MetricsPayload {
    /// 采集序号，从 1 递增；首帧 CPU/网络因无前置快照差值为 0
    pub seq: u64,
    pub cpu: CpuMetricsPayload,
    pub mem: MemMetrics,
    pub net: NetMetricsPayload,
    /// 真实挂载点磁盘用量（低频夹带，非夹带轮沿用上一次结果）
    pub disks: Vec<DiskMetrics>,
    /// 进程总数与 CPU Top N 榜单
    pub procs: ProcsPayload,
    /// GPU 卡列表（无 GPU 主机为空数组，前端整块隐藏）
    pub gpus: Vec<GpuItemPayload>,
}

/// 进程区动态值：全量进程数 + CPU 占用 Top [`TOP_PROCS`]
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProcsPayload {
    /// 主机当前进程总数（/proc/[0-9]* 成功读取数）
    pub total: usize,
    pub top: Vec<ProcItemPayload>,
    /// 全量进程（按 CPU 降序）：仅进程弹窗打开期间由前端订阅才填充，
    /// 千级进程主机约 130KB/轮，常态不订阅则一个空数组、不占带宽
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub all: Vec<ProcItemPayload>,
}

/// 单个 Top 进程
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct ProcItemPayload {
    pub pid: u32,
    /// comm（已去括号；远端字符串，前端插值自动转义）
    pub name: String,
    /// 区间内 CPU 占用百分比（单核 100 封顶，多核可超 100）；首帧为 0
    pub cpu: f32,
    /// 常驻内存字节（rss 页数 × 页大小）
    pub rss: u64,
    /// 所属 docker 容器名（非容器进程为空串；容器识别见 docker_container_id）
    pub container: String,
}

/// 依据本轮全量进程快照与上一轮 (采样时刻, 累计节拍) 表计算 Top 榜单。
/// `prev` 中不存在的进程（本轮新出现）CPU 记 0；节拍倒退/墙钟差非正也记 0。
/// 排序：CPU 降序 → RSS 降序 → pid 升序（空闲时内存大户优先，次序稳定可测）。
/// `full` 为真时额外带上全量列表（进程弹窗订阅），否则只留 Top [`TOP_PROCS`]。
pub(crate) fn build_procs_payload(
    cur: &ProcRaw,
    prev: &HashMap<u32, (Instant, u64)>,
    now: Instant,
    full: bool,
) -> ProcsPayload {
    let mut items: Vec<ProcItemPayload> = cur
        .procs
        .iter()
        .map(|p| {
            let cpu = match prev.get(&p.pid) {
                Some((prev_at, prev_ticks)) => {
                    let dt = now.saturating_duration_since(*prev_at);
                    let secs = dt.as_secs_f64();
                    let delta = p.cpu_ticks.saturating_sub(*prev_ticks);
                    if secs > f64::EPSILON && cur.clk_tck > 0 {
                        (delta as f64 * 100.0 / cur.clk_tck as f64 / secs) as f32
                    } else {
                        0.0
                    }
                }
                None => 0.0,
            };
            ProcItemPayload {
                pid: p.pid,
                name: p.name.clone(),
                cpu,
                rss: p.rss_pages.saturating_mul(cur.page_size),
                container: p.container.clone(),
            }
        })
        .collect();
    items.sort_by(|a, b| {
        b.cpu
            .partial_cmp(&a.cpu)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.rss.cmp(&a.rss))
            .then_with(|| a.pid.cmp(&b.pid))
    });
    let top = items.iter().take(TOP_PROCS).cloned().collect();
    ProcsPayload {
        total: cur.procs.len(),
        top,
        all: if full { items } else { Vec::new() },
    }
}

/// CPU 动态值
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CpuMetricsPayload {
    /// 总体使用率百分比（0~100，一位小数）
    pub util: f32,
    /// 1/5/15 分钟平均负载（运行队列长度，非百分比）；无法采集为 null
    pub load: Option<[f32; 3]>,
}

/// 网络动态值（速率 bytes/s，计数为累计字节/次数）
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct NetMetricsPayload {
    /// 接收速率 bytes/s（相邻快照字节差值 ÷ 墙钟秒数）
    pub rx_rate: f64,
    /// 发送速率 bytes/s
    pub tx_rate: f64,
    /// 累计接收字节
    pub rx_bytes: u64,
    /// 累计发送字节
    pub tx_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("uname".into(), "Linux 6.8.0-45-generic x86_64\n".into());
        m.insert(
            "uptime".into(),
            "14:23:01 up 2 days,  3:12, 2 users,  load average: 0.1, 0.2, 0.3".into(),
        );
        m.insert("proc".into(), "187\n".into());
        m.insert("ip".into(), "10.20.3.41 eth0\n".into());
        m.insert(
            "osrel".into(),
            "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 22.04.4 LTS\"\nID=ubuntu\n".into(),
        );
        m.insert("cores".into(), "32\n".into());
        m.insert(
            "cpuinfo".into(),
            "MODEL=Intel(R) Xeon(R) Platinum 8469 CPU @ 2.50GHz\n".into(),
        );
        m.insert("tz".into(), "+0000\n".into());
        m
    }

    #[test]
    fn host_info_full_linux() {
        let h = build_host_info(&sample());
        assert_eq!(h.os, "Linux");
        assert_eq!(h.kernel, "6.8.0-45-generic");
        assert_eq!(h.arch, "x86_64");
        assert_eq!(h.distro, "Ubuntu 22.04.4 LTS");
        assert_eq!(h.cpu_model, "Intel(R) Xeon(R) Platinum 8469 CPU @ 2.50GHz");
        assert_eq!(h.cores, 32);
        assert_eq!(h.timezone, "UTC+00:00");
        assert_eq!(h.uptime, "2 天 3 小时 12 分");
        assert_eq!(h.ip, "10.20.3.41");
        assert_eq!(h.iface, "eth0");
        assert!(h.procfs);
    }

    #[test]
    fn host_info_timezone_offsets() {
        let mut m = HashMap::new();
        m.insert("tz".into(), "+0800".into());
        assert_eq!(parse_timezone(&m), "UTC+08:00");
        m.insert("tz".into(), "-0530".into());
        assert_eq!(parse_timezone(&m), "UTC-05:30");
        m.insert("tz".into(), "UTC".into());
        assert_eq!(parse_timezone(&m), NA);
        assert_eq!(parse_timezone(&HashMap::new()), NA);
    }

    #[test]
    fn host_info_degrades_when_empty() {
        let h = build_host_info(&HashMap::new());
        assert_eq!(h.os, NA);
        assert_eq!(h.distro, NA); // 回退系统名，系统名本身也 N/A
        assert_eq!(h.kernel, NA);
        assert_eq!(h.arch, NA);
        assert_eq!(h.cpu_model, NA);
        assert_eq!(h.timezone, NA);
        assert_eq!(h.uptime, NA);
        assert_eq!(h.cores, 0);
        assert_eq!(h.ip, NA);
        assert_eq!(h.iface, "");
        assert!(!h.procfs);
    }

    #[test]
    fn host_info_distro_fallback_and_ip_without_iface() {
        // 无 os-release（如裸busybox）：发行版回退系统名
        let mut m = HashMap::new();
        m.insert("uname".into(), "Linux 5.10.0 aarch64".into());
        m.insert("ip".into(), "192.168.1.5\n".into()); // hostname -I 回退，无网卡名
        let h = build_host_info(&m);
        assert_eq!(h.distro, "Linux");
        assert_eq!(h.ip, "192.168.1.5");
        assert_eq!(h.iface, "");
        assert!(!h.procfs);
    }

    #[test]
    fn host_info_uptime_variants() {
        // 单数 1 user + 分钟
        let mut m = HashMap::new();
        m.insert(
            "uptime".into(),
            " 10:02:43 up 42 min, 1 user, load average: 0.10, 0.08, 0.02".into(),
        );
        assert_eq!(parse_uptime(&m), "42 分");

        // busybox/Debian 容器实测：HH:MM 不满 1 天（users 段仅作切分锚点）
        let mut m = HashMap::new();
        m.insert(
            "uptime".into(),
            " 08:38:43 up  5:52,  0 users,  load average: 0.00, 0.07, 0.07".into(),
        );
        assert_eq!(parse_uptime(&m), "5 小时 52 分");

        // HH:MM 0 小时段（如 "up 0:05"）不显示小时
        let mut m = HashMap::new();
        m.insert("uptime".into(), "x up 0:05, 0 users".into());
        assert_eq!(parse_uptime(&m), "5 分");

        // 1 天 + 分钟（无 HH:MM）
        let mut m = HashMap::new();
        m.insert("uptime".into(), "x up 1 day, 3 min, 0 users".into());
        assert_eq!(parse_uptime(&m), "1 天 3 分");

        // 无 uptime 字段：降级 N/A
        assert_eq!(parse_uptime(&HashMap::new()), NA);
    }

    #[test]
    fn cpu_model_arm_variants() {
        // 树莓派类：Hardware 字段优先
        let mut m = HashMap::new();
        m.insert(
            "cpuinfo".into(),
            "IMPL=0x41\nPART=0xd08\nHW=Raspberry Pi 4 Model B Rev 1.4\n".into(),
        );
        assert_eq!(parse_cpu_model(&m), "Raspberry Pi 4 Model B Rev 1.4");

        // 标准 ARM64 主机：part 映射（多核重复块只取首个）
        let mut m = HashMap::new();
        m.insert(
            "cpuinfo".into(),
            "IMPL=0x41\nPART=0xd08\nIMPL=0x41\nPART=0xd08\n".into(),
        );
        assert_eq!(parse_cpu_model(&m), "ARM Cortex-A72");

        // 未知 part：十六进制兜底，不臆造型号
        let mut m = HashMap::new();
        m.insert("cpuinfo".into(), "IMPL=0x41\nPART=0xdead\n".into());
        assert_eq!(parse_cpu_model(&m), "ARM64 · part 0xdead");

        // Apple Silicon 虚拟机：只有 implementer 0x61
        let mut m = HashMap::new();
        m.insert("cpuinfo".into(), "IMPL=0x61\n".into());
        assert_eq!(parse_cpu_model(&m), "Apple Silicon");

        // Apple VZ 同时给无效占位 part 0x000：应忽略 part 走族名
        let mut m = HashMap::new();
        m.insert("cpuinfo".into(), "IMPL=0x61\nPART=0x000\n".into());
        assert_eq!(parse_cpu_model(&m), "Apple Silicon");

        // Apple M1 part
        let mut m = HashMap::new();
        m.insert("cpuinfo".into(), "IMPL=0x61\nPART=0x023\n".into());
        assert_eq!(parse_cpu_model(&m), "Apple M1");

        // 无任何 CPU 信息
        assert_eq!(parse_cpu_model(&HashMap::new()), NA);
    }
}

#[cfg(test)]
mod metrics_dynamic_tests {
    use super::*;

    #[test]
    fn cpu_util_basic() {
        // 间隔内 total +100，idle +75 → 25% 占用
        let p = CpuTimes {
            total: 1000,
            idle: 800,
        };
        let c = CpuTimes {
            total: 1100,
            idle: 875,
        };
        assert!((cpu_util(p, c) - 25.0).abs() < 1e-6);
    }

    #[test]
    fn cpu_util_clamped_and_zero_delta() {
        let p = CpuTimes {
            total: 1000,
            idle: 800,
        };
        // 完全空闲 → 0%
        let c = CpuTimes {
            total: 1200,
            idle: 1000,
        };
        assert_eq!(cpu_util(p, c), 0.0);
        // 完全繁忙 → 100%
        let c = CpuTimes {
            total: 1200,
            idle: 800,
        };
        assert_eq!(cpu_util(p, c), 100.0);
        // 无增量 → 0，不除零
        assert_eq!(cpu_util(p, p), 0.0);
        // idle 倒退（异常快照）→ saturating，结果不越界
        let c = CpuTimes {
            total: 1100,
            idle: 0,
        };
        assert_eq!(cpu_util(p, c), 100.0);
    }

    #[test]
    fn advance_cpu_keeps_last_value_across_restarts() {
        let mut st = CpuSampleState::default();
        let t0 = Instant::now();
        let max_gap = Duration::from_secs(6);
        let s1 = CpuTimes {
            total: 1000,
            idle: 800,
        };
        let s2 = CpuTimes {
            total: 1100,
            idle: 820,
        };
        // 无前置快照（首次连接）：沿用初始 0，并记录本轮快照
        assert_eq!(advance_cpu(&mut st, s1, t0, max_gap), 0.0);
        assert_eq!(st.prev, Some(s1));
        // 短间隔重启后首帧：差值有效，得到真实 80%，而不是 0%
        let util = advance_cpu(&mut st, s2, t0 + Duration::from_secs(2), max_gap);
        assert!((util - 80.0).abs() < 1e-6);
        // 暂停超过窗口：沿用上次 80%，不把暂停期平均进来
        let s3 = CpuTimes {
            total: 2000,
            idle: 1200,
        };
        assert_eq!(
            advance_cpu(&mut st, s3, t0 + Duration::from_secs(120), max_gap),
            80.0
        );
        // 已用新快照续上：下一轮恢复实时差值
        let s4 = CpuTimes {
            total: 2100,
            idle: 1250,
        };
        let util = advance_cpu(&mut st, s4, t0 + Duration::from_secs(122), max_gap);
        assert!((util - 50.0).abs() < 1e-6);
        // 节拍倒退/同值（远端重启）：沿用上次值，不把 0 推给界面
        let reset = CpuTimes {
            total: 100,
            idle: 80,
        };
        assert_eq!(
            advance_cpu(&mut st, reset, t0 + Duration::from_secs(124), max_gap),
            50.0
        );
    }

    #[test]
    fn parse_full_script_output() {
        let out = concat!(
            "@@STAT@@\n",
            "cpu  10000 100 2000 80000 500 0 100 0 0 0\n",
            "cpu0 1 1 1 1\n",
            "@@LOAD@@\n",
            "0.42 0.38 0.31 2/321 12345\n",
            "@@MEM@@\n",
            "MemTotal:        8167628 kB\n",
            "MemFree:         2000000 kB\n",
            "MemAvailable:    5000000 kB\n",
            "SwapTotal:       2097152 kB\n",
            "SwapFree:        1048576 kB\n",
            "Buffers:          100000 kB\n",
            "@@NET@@\n",
            "Inter-|   Receive                                                |  Transmit\n",
            " face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n",
            "    lo:     530       7    1    2    0     0          0         0      530       7    0    0    0     0       0          0\n",
            "  eth0: 2063672   16521    3    4    0     0          0         0  2386376   11963    5    6    0     0       0          0\n",
            "@@END@@\n",
        );
        let r = parse_metrics_output(out, Some("eth0"));
        let cpu = r.cpu.expect("cpu parsed");
        assert_eq!(cpu.idle, 80000 + 500);
        assert_eq!(cpu.total, 10000 + 100 + 2000 + 80000 + 500 + 100);
        assert_eq!(r.load, Some([0.42, 0.38, 0.31]));
        assert_eq!(r.mem.total, 8_167_628 * 1024);
        assert_eq!(r.mem.available, 5_000_000 * 1024);
        assert_eq!(r.mem.used, (8_167_628 - 5_000_000) * 1024);
        assert_eq!(r.mem.swap_total, 2_097_152 * 1024);
        assert_eq!(r.mem.swap_free, 1_048_576 * 1024);
        assert_eq!(r.mem.swap_used, 1_048_576 * 1024);
        // 网络：只取指定默认网卡 eth0，lo 的流量不计入
        let net = r.net.expect("net parsed");
        assert_eq!(net.rx_bytes, 2_063_672);
        assert_eq!(net.tx_bytes, 2_386_376);
        // 非夹带轮：无 DISK 段 → None（collector 沿用缓存）
        assert!(r.disks.is_none());
        // 该样本无 PROC 段
        assert!(r.procs.is_none());
    }

    #[test]
    fn parse_loadavg_variants() {
        // 标准格式（含可能的两位整数负载）
        assert_eq!(
            parse_loadavg("0.01 0.03 0.04 1/362 3582"),
            Some([0.01, 0.03, 0.04])
        );
        assert_eq!(parse_loadavg("2.15 1.80 1.02"), Some([2.15, 1.80, 1.02]));
        // 带前后空白
        assert_eq!(parse_loadavg("  1.0 2.0 3.0 \n"), Some([1.0, 2.0, 3.0]));
        // 缺列 / 非法 / 空行 → None
        assert_eq!(parse_loadavg("0.5 0.2"), None);
        assert_eq!(parse_loadavg("a b c"), None);
        assert_eq!(parse_loadavg(""), None);
    }

    #[test]
    fn parse_meminfo_fallback_free_and_bytes() {
        let out =
            "@@STAT@@\ncpu  1 0 0 9 0\n@@MEM@@\nMemTotal: 1024 kB\nMemFree: 512 kB\n@@END@@\n";
        let r = parse_metrics_output(out, None);
        assert_eq!(r.mem.total, 1024 * 1024);
        // 无 MemAvailable 时回退 MemFree
        assert_eq!(r.mem.available, 512 * 1024);
        assert_eq!(r.mem.used, 512 * 1024);
        // meminfo 无 Swap 行：swap 全 0（前端据此隐藏 Swap 区块）
        assert_eq!(r.mem.swap_total, 0);
        assert_eq!(r.mem.swap_free, 0);
        assert_eq!(r.mem.swap_used, 0);
        // 无 LOAD 段：负载为 None
        assert!(r.load.is_none());
        // 无 NET 段：网卡计数为 None（不更新 prev 快照）
        assert!(r.net.is_none());
    }

    #[test]
    fn parse_degrades_on_missing_sections() {
        // 空输出：全部降级
        let r = parse_metrics_output("", None);
        assert!(r.cpu.is_none());
        assert!(r.load.is_none());
        assert_eq!(r.mem, MemMetrics::default());
        assert!(r.net.is_none());

        // 非 cpu 开头的 stat 段 / 残缺 meminfo
        let r = parse_metrics_output(
            "@@STAT@@\n???\n@@LOAD@@\n???\n@@MEM@@\nGarbage\n@@END@@\n",
            None,
        );
        assert!(r.cpu.is_none());
        assert!(r.load.is_none());
        assert_eq!(r.mem.total, 0);
        assert_eq!(r.mem.used, 0);
    }

    #[test]
    fn parse_darwin_like_no_procfs_sections_present_but_empty() {
        // macOS 等无 procfs 环境：cat /proc/* 全部静默失败，但 marker 分段仍在；
        // getconf 可用而 /proc 循环无输出（PROC 仅剩两行数字）；df 只有表头。
        // 期望：所有指标降级为空/零且不 panic，collector 据此仍能正常产出一帧。
        let out = concat!(
            "@@STAT@@\n",
            "@@LOAD@@\n",
            "@@MEM@@\n",
            "@@NET@@\n",
            "@@DISK@@\n",
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n",
            "@@PROC@@\n",
            "100\n",
            "4096\n",
            "@@GPU@@\n",
            "@@END@@\n",
        );
        let r = parse_metrics_output(out, None);
        assert!(r.cpu.is_none(), "无 /proc/stat 应为 None");
        assert!(r.load.is_none(), "无 /proc/loadavg 应为 None");
        assert_eq!(r.mem, MemMetrics::default(), "无 /proc/meminfo 应为零值");
        assert!(r.net.is_none(), "无 /proc/net/dev 应为 None");
        assert!(
            r.disks.unwrap_or_default().is_empty(),
            "仅表头应解析为零个磁盘"
        );
        assert!(
            r.procs.is_none(),
            "只有 getconf 两行无 stat 行应判空（沿用缓存）"
        );
        assert!(
            r.gpus.as_ref().is_some_and(Vec::is_empty),
            "GPU 段存在但无 nvidia-smi 输出应为 Some(空)（据以永久关段）"
        );
    }

    #[test]
    fn cpu_util_from_two_real_snapshots() {
        let r1 = parse_metrics_output(
            "@@STAT@@\ncpu  100 0 100 800 0 0 0 0\n@@MEM@@\nMemTotal: 2048 kB\nMemAvailable: 1024 kB\n@@END@@\n",
            None,
        );
        let r2 = parse_metrics_output(
            "@@STAT@@\ncpu  200 0 200 900 0 0 0 0\n@@MEM@@\nMemTotal: 2048 kB\nMemAvailable: 1024 kB\n@@END@@\n",
            None,
        );
        // total +300, idle +100 → busy 200/300 ≈ 66.67%
        let util = cpu_util(r1.cpu.unwrap(), r2.cpu.unwrap());
        assert!((util - 66.6667).abs() < 0.01);
    }

    #[test]
    fn parse_netdev_iface_selection() {
        let text = concat!(
            "Inter-|   Receive                                                |  Transmit\n",
            " face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n",
            "    lo:     530       7    0    0    0     0          0         0      530       7    0    0    0     0       0          0\n",
            "  eth0: 2063672   16521    0    0    0     0          0         0  2386376   11963    0    0    0     0       0          0\n",
            " wlan0:  100000     100    2    3    0     0          0         0   200000     200    4    5    0     0       0          0\n",
        );
        // 指定默认路由网卡：只取 eth0
        let eth = NetCounters::parse(text, Some("eth0")).expect("eth0");
        assert_eq!(eth.rx_bytes, 2_063_672);
        assert_eq!(eth.tx_bytes, 2_386_376);

        // 指定另一张网卡：只取 wlan0
        let wlan = NetCounters::parse(text, Some("wlan0")).expect("wlan0");
        assert_eq!(wlan.rx_bytes, 100_000);
        assert_eq!(wlan.tx_bytes, 200_000);

        // iface=None：汇总全部非 lo 网卡（lo 的 530 不计入）
        let sum = NetCounters::parse(text, None).expect("sum");
        assert_eq!(sum.rx_bytes, 2_063_672 + 100_000);
        assert_eq!(sum.tx_bytes, 2_386_376 + 200_000);

        // 指定不存在的网卡：回退汇总非 lo
        let fallback = NetCounters::parse(text, Some("eth9")).expect("fallback");
        assert_eq!(fallback.rx_bytes, 2_063_672 + 100_000);

        // 只有表头 / 空段：None
        assert!(NetCounters::parse("", None).is_none());
        assert!(NetCounters::parse("Inter-|\n face |bytes\n", None).is_none());
        // 只有 lo：视为无有效网卡
        assert!(
            NetCounters::parse("    lo: 100 1 0 0 0 0 0 0 100 1 0 0 0 0 0 0\n", None).is_none()
        );
    }

    #[test]
    fn byte_rate_variants() {
        // 3000 字节 / 2 秒 = 1500 bytes/s
        assert!((byte_rate(0, 3000, Duration::from_secs(2)) - 1500.0).abs() < 1e-6);
        // 零间隔不除零
        assert_eq!(byte_rate(0, 3000, Duration::ZERO), 0.0);
        // 计数器倒退（异常快照/回绕）saturating 为 0，不出现负速率
        assert_eq!(byte_rate(5000, 1000, Duration::from_secs(1)), 0.0);
        // 无增量
        assert_eq!(byte_rate(100, 100, Duration::from_secs(1)), 0.0);
    }

    #[test]
    fn parse_disks_filter_dedup_sort() {
        // 取自真实 Docker Desktop 容器 + 一台真实多盘 Linux 主机
        let text = concat!(
            "Filesystem     1024-blocks    Used Available Capacity Mounted on\n",
            "overlay          301924352 2835180 299089172       1% /\n",
            "tmpfs                65536       0     65536       0% /dev\n",
            "shm                4098048       0   4098048       0% /dev/shm\n",
            "/dev/vdb1        301924352 2835180 299089172       1% /config\n",
            "/dev/vdb1        301924352 2835180 299089172       1% /etc/hosts\n",
            "tmpfs                    4       0         4       0% /proc/asound\n",
            "/dev/sda1        104857600 52428800  52428800      50% /data\n",
            "/dev/sdb1            20480    10240     10240      50% /mnt/my data\n",
        );
        let disks = parse_disks(text);
        // 全挂载点列表（与 FinalShell 对齐）：tmpfs/shm（/dev、/dev/shm，有真实容量）
        // 保留；/etc/hosts 容器注入挂载滤除；同容量的 / 与 /config（btrfs 子卷/
        // 同规格分区常见）不再被三元组去重吞并；重复挂载点只按路径去重
        let mounts: Vec<&str> = disks.iter().map(|d| d.mount.as_str()).collect();
        assert_eq!(
            mounts,
            vec![
                "/", "/config", "/data", "/dev", "/dev/shm", "/mnt/my data", "/proc/asound"
            ]
        );

        let root = &disks[0];
        assert_eq!(root.total, 301_924_352 * 1024);
        assert_eq!(root.used, 2_835_180 * 1024);
        assert_eq!(root.available, 299_089_172 * 1024);
        // 2835180/301924352 ≈ 0.94% → 四舍五入 1
        assert_eq!(root.pct, 1);

        let config = &disks[1];
        assert_eq!(config.pct, 1);
        assert_eq!(config.total, 301_924_352 * 1024);

        let data = &disks[2];
        assert_eq!(data.pct, 50);
        assert_eq!(data.total, 104_857_600 * 1024);

        // 挂载点含空格：-P 行尾多列正确拼回
        assert_eq!(disks[5].mount, "/mnt/my data");
        assert_eq!(disks[5].total, 20_480 * 1024);
    }

    #[test]
    fn parse_disks_type_column() {
        // `df -PkT`（7 列，含 Type 列；macOS/BSD 无 -T 回落 6 列，两种布局都要能解析）
        let text = concat!(
            "Filesystem Type 1024-blocks Used Available Capacity Mounted on\n",
            "/dev/sda2 ext4 40890580 12345678 26440468 32% /\n",
            "tmpfs tmpfs 8388608 0 8388608 0% /dev/shm\n",
            "overlay overlay 40890580 12345678 26440468 32% /var/lib/docker/overlay2/x/merged\n",
        );
        let disks = parse_disks(text);
        let mounts: Vec<&str> = disks.iter().map(|d| d.mount.as_str()).collect();
        assert_eq!(mounts, vec!["/", "/dev/shm", "/var/lib/docker/overlay2/x/merged"]);
        assert_eq!(disks[0].fs, "ext4");
        assert_eq!(disks[0].pct, 30);
        assert_eq!(disks[0].total, 40_890_580 * 1024);
        assert_eq!(disks[1].fs, "tmpfs");
        assert_eq!(disks[2].fs, "overlay");
    }

    #[test]
    fn parse_disks_without_type_column_has_empty_fs() {
        let text = "Filesystem 1K-blocks Used Available Use% Mounted on\n/dev/sda1 1024 512 512 50% /\n";
        let disks = parse_disks(text);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].fs, "");
        assert_eq!(disks[0].mount, "/");
    }

    #[test]
    fn parse_disks_only_header() {
        assert!(parse_disks("Filesystem 1K-blocks Used Available Use% Mounted on\n").is_empty());
    }

    #[test]
    fn disk_section_presence_and_script_builder() {
        // 夹带轮：DISK 段存在 → 解析出磁盘
        let with = parse_metrics_output(
            "@@STAT@@\ncpu  1 0 0 9 0\n@@MEM@@\nMemTotal: 1024 kB\nMemFree: 512 kB\n@@DISK@@\nFilesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 1024 512 512 50% /\n@@END@@\n",
            None,
        );
        let disks = with.disks.expect("disks present");
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].mount, "/");
        assert_eq!(disks[0].pct, 50);

        // DISK 段存在但为空（df 失败被 2>/dev/null 吞掉）→ None，沿用缓存
        let empty = parse_metrics_output("@@DISK@@\n@@END@@\n", None);
        assert!(empty.disks.is_none());

        // 脚本构造：默认不带 df/GPU/docker；各段在 END 前
        let base = metrics_script(
            false,
            false,
            DockerScanScope::Off,
            false,
        );
        assert!(base.contains("@@NET@@"));
        assert!(!base.contains("@@DISK@@"));
        assert!(!base.contains("@@GPU@@"));
        assert!(base.contains("@@PROC@@"));
        assert!(!base.contains("@@DOCKER@@"));
        assert!(!base.contains("cgpid:"));
        let with_script = metrics_script(
            true,
            true,
            DockerScanScope::All,
            true,
        );
        let disk_pos = with_script.find("@@DISK@@").expect("disk marker");
        let gpu_pos = with_script.find("@@GPU@@").expect("gpu marker");
        let end_pos = with_script.find("@@END@@").expect("end marker");
        assert!(disk_pos < end_pos);
        assert!(gpu_pos < end_pos);
        assert!(with_script.contains("cgpid:"));
        assert!(with_script.contains("@@DOCKERNAMES@@"));
        assert!(with_script.contains("docker ps"));
        // 只开 GPU 不带磁盘/docker
        let gpu_only = metrics_script(
            false,
            true,
            DockerScanScope::Off,
            false,
        );
        assert!(gpu_only.contains("@@GPU@@"));
        assert!(!gpu_only.contains("@@DISK@@"));
        assert!(!gpu_only.contains("@@DOCKER@@"));
    }

    #[test]
    fn docker_incremental_script_scans_only_given_pids() {
        // 增量轮只 grep 指定 PID 的 cgroup，绝不出现 /proc/[0-9]* 全量通配
        let pids = [123u32, 456];
        let s = metrics_script(
            false,
            false,
            DockerScanScope::Pids(&pids),
            false,
        );
        assert!(s.contains("@@DOCKER@@"));
        assert!(s.contains("/proc/123/cgroup"));
        assert!(s.contains("/proc/456/cgroup"));
        // 进程统计段本来就有 /proc/[0-9]*，这里只断言 cgroup 扫描没用全量通配
        assert!(!s.contains("/proc/[0-9]*/cgroup"));
        assert!(!s.contains("@@DOCKERNAMES@@"));
        // 名称查询只在明确要求且非 Off 时出现
        let names = metrics_script(
            false,
            false,
            DockerScanScope::NamesOnly,
            true,
        );
        assert!(names.contains("@@DOCKER@@"));
        assert!(names.contains("@@DOCKERNAMES@@"));
        assert!(!names.contains("cgpid:"));
        assert!(!names.contains("/proc/[0-9]*/cgroup"));
        // Off + 名称要求：名称段必须被忽略（没有 @@DOCKER@@ 标记会丢行）
        let off = metrics_script(false, false, DockerScanScope::Off, true);
        assert!(!off.contains("@@DOCKERNAMES@@"));
    }

    #[test]
    fn docker_section_presence_and_names_flag() {
        // 段存在但无任何容器行 → Some(空)，表示「扫描成功、无容器」而非「未扫描」，
        // 名称未查询时 names_scanned=false，名称缓存不可被空 map 覆盖
        let raw = parse_metrics_output(
            "@@DOCKER@@\n@@DOCKERNAMES@@\n@@END@@\n",
            None,
        );
        let scan = raw.docker.expect("docker section present");
        assert!(scan.cg_ids.is_empty());
        assert!(scan.names.is_empty());
        assert!(scan.names_scanned);
        assert!(container_name_for(&scan.names, "abcdef123456").is_none());

        // 只有 cgroup 段：names_scanned=false，collector 应保留旧名称缓存
        let raw = parse_metrics_output(
            "@@DOCKER@@\ncgpid:42:aaaaaaaaaaaa\n@@END@@\n",
            None,
        );
        let scan = raw.docker.expect("docker section present");
        assert!(!scan.names_scanned);
        assert_eq!(scan.cg_ids.get(&42).map(String::as_str), Some("aaaaaaaaaaaa"));

        // 完全没有 marker → None（本轮未采集，沿用缓存）
        let raw = parse_metrics_output("@@PROC@@\n1 (init) S 1 1 1 0 -1\n@@END@@\n", None);
        assert!(raw.docker.is_none());
    }

    #[test]
    fn docker_name_prefix_match() {
        // docker ps 给全 ID，cgroup 给截断 ID：前缀匹配
        let raw = parse_metrics_output(
            "@@DOCKER@@\ncgpid:7:abcdef123456\n@@DOCKERNAMES@@\ndocker:abcdef1234567890:web-1\n@@END@@\n",
            None,
        );
        let scan = raw.docker.expect("docker section present");
        assert_eq!(
            container_name_for(&scan.names, "abcdef123456"),
            Some("web-1")
        );
        // 精确命中优先
        assert_eq!(
            container_name_for(&scan.names, "abcdef1234567890"),
            Some("web-1")
        );
    }

    #[test]
    fn parse_gpus_csv_mib_and_na() {
        // 真实 nvidia-smi csv,noheader,nounits 两卡输出：型号名含空格；
        // 卡1 功耗 [N/A]（部分虚拟化/笔记本场景），卡2 全部有效；乱序给入验证按 index 排序
        let text = concat!(
            "1, NVIDIA GeForce RTX 4090, 88, 20000, 24576, 71, [N/A], [N/A]\n",
            "0, NVIDIA A100-SXM4-40GB, 34, 8216, 40960, 56, 182.34, 400.00\n",
        );
        let gpus = parse_gpus(text);
        assert_eq!(gpus.len(), 2);
        // index 升序
        assert_eq!(gpus[0].index, 0);
        assert_eq!(gpus[0].name, "NVIDIA A100-SXM4-40GB");
        assert!((gpus[0].util - 34.0).abs() < 1e-4);
        // MiB → 字节（8216 MiB、40960 MiB）
        assert_eq!(gpus[0].mem_used, 8216 * 1024 * 1024);
        assert_eq!(gpus[0].mem_total, 40960 * 1024 * 1024);
        assert!((gpus[0].temp - 56.0).abs() < 1e-4);
        assert!((gpus[0].power - 182.34).abs() < 1e-2);
        assert!((gpus[0].power_limit - 400.0).abs() < 1e-4);
        // [N/A] 字段统一记 0（前端据此隐藏功耗）
        assert_eq!(gpus[1].power, 0.0);
        assert_eq!(gpus[1].power_limit, 0.0);
        assert_eq!(gpus[1].mem_used, 20000 * 1024 * 1024);
    }

    #[test]
    fn parse_gpus_bad_lines_and_empty() {
        // 残行（列数不足/非数字 index/空型号）全部跳过，不误伤其他卡
        let text = concat!(
            "0, NVIDIA H100, 10, 1024, 8192, 40, 70.5, 700\n",
            "broken line\n",
            "x, Bad Index, 1, 1, 1, 1, 1, 1\n",
            "2, , 1, 1, 1, 1, 1, 1\n",
            "3, Too Few, 1, 1\n",
        );
        let gpus = parse_gpus(text);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].index, 0);

        // 空段（无卡主机 nvidia-smi 缺失，2>/dev/null 后无任何输出）
        assert!(parse_gpus("").is_empty());
        assert!(parse_gpus("\n  \n").is_empty());
    }

    #[test]
    fn gpu_section_presence_semantics() {
        // 带段且有卡 → Some(非空)
        let ok = parse_metrics_output(
            "@@GPU@@\n0, NVIDIA A100, 34, 8216, 40960, 56, 182.34, 400.00\n@@END@@\n",
            None,
        );
        let gpus = ok.gpus.expect("gpu section present");
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].mem_total, 40960 * 1024 * 1024);

        // 带段但空（无卡/命令不存在）→ Some(空 vec)，与「未带段」严格区分：
        // collector 正是靠 Some(空) 永久关闭 GPU 段
        let empty = parse_metrics_output("@@GPU@@\n@@END@@\n", None);
        assert!(empty.gpus.is_some_and(|v| v.is_empty()));

        // 未带 GPU 段 → None，不应误改 collector 的探测结论
        let absent = parse_metrics_output("@@STAT@@\ncpu 1 0 0 9 0\n@@END@@\n", None);
        assert!(absent.gpus.is_none());
    }

    #[test]
    fn parse_procs_real_stat_lines() {
        // 真实 busybox 容器输出 + 一个含空格/括号的极端进程名
        let text = concat!(
            "100\n",
            "4096\n",
            "1 (s6-svscan) S 0 1 1 0 -1 4194560 1418 192172 81 111 1 5 60 112 20 0 1 0 1800 450560 0 1 2 3\n",
            "184 (sshd.pam) S 40 184 184 0 -1 4194560 20994 1721288 120 1958 55 33 759 932 20 0 1 0 1815 6418432 256 1 2 3\n",
            "42 (my (weird) proc) R 1 42 42 0 -1 0 0 0 0 0 10 20 0 0 20 0 1 0 0 0 16 1 2 3\n",
        );
        let raw = parse_procs(text).expect("procs");
        assert_eq!(raw.clk_tck, 100);
        assert_eq!(raw.page_size, 4096);
        assert_eq!(raw.procs.len(), 3);

        // s6-svscan：utime=1 stime=5 → ticks=6；rss=字段24=0
        let init = &raw.procs[0];
        assert_eq!(init.pid, 1);
        assert_eq!(init.name, "s6-svscan");
        assert_eq!(init.cpu_ticks, 6);
        assert_eq!(init.rss_pages, 0);

        // sshd.pam：utime=55 stime=33 → 88；rss=256 页
        let sshd = &raw.procs[1];
        assert_eq!(sshd.pid, 184);
        assert_eq!(sshd.cpu_ticks, 88);
        assert_eq!(sshd.rss_pages, 256);

        // 进程名按最后一个 ')' 定位，内含空格/括号也正确
        assert_eq!(raw.procs[2].pid, 42);
        assert_eq!(raw.procs[2].name, "my (weird) proc");
        assert_eq!(raw.procs[2].cpu_ticks, 30);
        assert_eq!(raw.procs[2].rss_pages, 16);

        // getconf 两行都缺失：直接以 stat 行起头（非纯数字）→ 回退默认值
        let noheader = parse_procs("1 (init) S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 20 0 1 0 0 0 8 1 2 3\n")
            .expect("fallback procs");
        assert_eq!(noheader.clk_tck, 100);
        assert_eq!(noheader.page_size, 4096);
        assert_eq!(noheader.procs[0].pid, 1);
        assert_eq!(noheader.procs[0].rss_pages, 8);

        // 空段 / 残缺行 → None
        assert!(parse_procs("").is_none());
        assert!(parse_procs("garbage line without parens\n").is_none());
    }

    #[test]
    fn build_procs_payload_full_only_when_subscribed() {
        let raw = ProcRaw {
            clk_tck: 100,
            page_size: 4096,
            procs: (1..=7)
                .map(|pid| ProcSnapshot {
                    pid,
                    name: format!("p{pid}"),
                    cpu_ticks: 0,
                    rss_pages: pid as u64,
                    container: String::new(),
                })
                .collect(),
        };
        let prev = HashMap::new();
        let now = Instant::now();
        let off = build_procs_payload(&raw, &prev, now, false);
        assert!(off.all.is_empty());
        assert_eq!(off.top.len(), TOP_PROCS);
        // 未订阅时帧里不出现 all 键：千级进程主机不多花一个字节
        assert!(!serde_json::to_string(&off).unwrap().contains("\"all\""));

        let on = build_procs_payload(&raw, &prev, now, true);
        assert_eq!(on.all.len(), 7);
        assert_eq!(on.top.as_slice(), &on.all[..TOP_PROCS]);
        assert!(serde_json::to_string(&on).unwrap().contains("\"all\""));
    }

    #[test]
    fn build_procs_payload_top_and_delta() {
        fn raw_with(ticks_rss: &[(u32, u64, u64)]) -> ProcRaw {
            ProcRaw {
                clk_tck: 100,
                page_size: 4096,
                procs: ticks_rss
                    .iter()
                    .map(|&(pid, ticks, rss_pages)| ProcSnapshot {
                        pid,
                        name: format!("p{pid}"),
                        cpu_ticks: ticks,
                        rss_pages,
                        container: String::new(),
                    })
                    .collect(),
            }
        }

        let now = Instant::now();
        let sec1 = Duration::from_secs(1);
        // 7 个进程：pid1 +100 ticks/1s = 100%；pid2 无增量(rss10页)；pid3 无 prev 新进程(rss20页)；
        // pid4 +50=50%(rss40页)；pid5~7 均 0% 且 rss=0（验证平手 RSS 降序→pid 升序与截断）
        let cur = raw_with(&[
            (1, 200, 256),
            (2, 100, 10),
            (3, 10, 20),
            (4, 50, 40),
            (5, 0, 0),
            (6, 0, 0),
            (7, 0, 0),
        ]);
        let prev: HashMap<u32, (Instant, u64)> = HashMap::from([
            (1, (now - sec1, 100)),
            (2, (now - sec1, 100)),
            (4, (now - sec1, 0)),
            (5, (now - sec1, 0)),
            (6, (now - sec1, 0)),
            (7, (now - sec1, 0)),
        ]);

        let payload = build_procs_payload(&cur, &prev, now, false);
        assert_eq!(payload.total, 7);
        assert_eq!(payload.top.len(), TOP_PROCS);
        let pids: Vec<u32> = payload.top.iter().map(|p| p.pid).collect();
        // 100% → 50% → 0% 组按 RSS 降序（pid3 20页 > pid2 10页）→ rss 相同按 pid
        assert_eq!(pids, vec![1, 4, 3, 2, 5]);

        let top1 = &payload.top[0];
        assert!((top1.cpu - 100.0).abs() < 1e-4, "pid1 cpu: {}", top1.cpu);
        assert_eq!(top1.rss, 256 * 4096);
        // pid4：50 ticks / 100 tck / 1s = 50%
        assert!((payload.top[1].cpu - 50.0).abs() < 1e-4);
        // 0% 组（pid3 新进程 rss20、pid2 无增量 rss10）均为 0，按 RSS 降序
        assert_eq!(payload.top[2].pid, 3);
        assert_eq!(payload.top[2].cpu, 0.0);
        assert_eq!(payload.top[3].pid, 2);
        assert_eq!(payload.top[3].cpu, 0.0);

        // 首帧（prev 全空）：全部 0%，按 RSS 降序取前 5
        let empty_prev = HashMap::new();
        let first = build_procs_payload(&cur, &empty_prev, now, false);
        assert_eq!(
            first.top.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![1, 4, 3, 2, 5]
        );
        assert!(first.top.iter().all(|p| p.cpu == 0.0));

        // 节拍倒退（异常快照）saturating 为 0，不出现负百分比
        let mut back = HashMap::new();
        back.insert(1u32, (now - sec1, 9999));
        let backed = build_procs_payload(&raw_with(&[(1, 100, 1)]), &back, now, false);
        assert_eq!(backed.top[0].cpu, 0.0);
    }
}
