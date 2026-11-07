//! 动态指标采集器（0x06 帧）：按固定间隔在独立 exec 通道采集 /proc 数据，
//! 与 PTY 流物理隔离。单飞、错峰、可取消；前端用心跳维持，超时自动停。
//!
//! 生命周期：
//! - [`spawn`] 在会话 cancel 下派生 child token，返回 [`CollectorHandle`]；
//! - 前端每 5s 调 `metrics_heartbeat` 续约，超过 [`HEARTBEAT_TTL`] 未续约
//!   （监控面板隐藏/页面关闭/崩溃）采集任务自行退出；
//! - 会话 `shutdown()` 取消根 token，子任务同步取消，无任务泄漏。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::debug;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::applog;
use crate::applog::events as ev;
use crate::metrics::{
    CpuMetricsPayload, CpuTimes, DISK_INTERVAL_ROUNDS, DiskMetrics, DockerScanScope,
    GpuItemPayload, MemMetrics, MetricsPayload, NetCounters, NetMetricsPayload, ProcsPayload,
    RawMetrics, build_procs_payload, byte_rate, container_name_for, cpu_util, metrics_script,
    parse_metrics_output,
};
use crate::ssh::frame::{FrameType, encode_frame};
use crate::ssh::session::SshSession;

/// 单轮采集（含建 exec 通道）超时；与间隔默认值一致，慢主机不堆积
const ROUND_TIMEOUT: Duration = Duration::from_secs(3);
/// 连续失败多少轮后进入慢轮询退避
const FAILURE_BACKOFF_THRESHOLD: u32 = 3;
/// 退避态轮询间隔：故障主机上降低重试频率，继续重试而非放弃，任一轮成功即恢复
const BACKOFF_INTERVAL: Duration = Duration::from_secs(10);
/// docker cgroup 全量扫描周期（轮）：日常只增量扫描新 PID，定期全量兜底
/// （容器内子进程未被观察到、cgroup 迁移等），按 3s 间隔约 2 分钟一次。
const DOCKER_FULL_SCAN_ROUNDS: u32 = 40;
/// 增量扫描 PID 上限：新进程过多（批量拉起/重启风暴）时直接回退全量，
/// 避免脚本参数过长；全量本就是一次 grep，不会更慢。
const DOCKER_INCREMENTAL_MAX_PIDS: usize = 200;
/// 容器名映射刷新周期（轮）：名称变化缓慢，日常只在首次/未知容器时查询，
/// 按 3s 间隔约 5 分钟兜底刷新一次，处理容器改名。
const DOCKER_NAMES_REFRESH_ROUNDS: u32 = 100;

/// 根据连续失败轮数决定下一轮节拍：未达阈值用正常间隔，达到 [`FAILURE_BACKOFF_THRESHOLD`]
/// 降到 [`BACKOFF_INTERVAL`]。纯函数便于单测。
pub(crate) fn round_delay(interval: Duration, consecutive_failures: u32) -> Duration {
    if consecutive_failures >= FAILURE_BACKOFF_THRESHOLD {
        BACKOFF_INTERVAL
    } else {
        interval
    }
}
/// 心跳续约 TTL：前端 5s 一次心跳，9s 无心跳判定无人消费，自动停止
pub(crate) const HEARTBEAT_TTL: Duration = Duration::from_secs(9);

/// 采集任务句柄：由 SshSession 持有，用于续约/停止
pub(crate) struct CollectorHandle {
    cancel: CancellationToken,
    last_heartbeat: Arc<StdMutex<Instant>>,
    /// 进程弹窗订阅开关：打开才随帧附全量进程，关闭即回到 Top 榜
    procs_full: Arc<AtomicBool>,
    /// 会话标签（stop 日志事件用）
    sid: String,
}

impl CollectorHandle {
    /// 刷新续约时间（前端心跳）
    pub(crate) fn heartbeat(&self) {
        *self.last_heartbeat.lock().unwrap() = Instant::now();
    }

    /// 进程弹窗打开/关闭：切换是否随帧附全量进程列表
    pub(crate) fn set_procs_full(&self, on: bool) {
        self.procs_full.store(on, Ordering::Relaxed);
    }

    /// 主动停止采集（重复 start 时先停旧任务，或前端显式 stop）
    pub(crate) fn stop(&self) {
        self.cancel.cancel();
        applog::emit(
            log::Level::Debug,
            "metrics",
            ev::METRICS_COLLECT_STOP,
            Some(&self.sid),
            "指标采集停止",
            Some(serde_json::json!({ "reason": "manual" })),
        );
    }
}

/// 启动一个采集任务。`root` 为会话根取消令牌，任务挂其 child token 上。
/// `iface` 为默认路由网卡名（0x05 已采集），网络计数优先取该网卡；None 时汇总非 lo。
pub(crate) fn spawn(
    session: Arc<SshSession>,
    interval: Duration,
    root: CancellationToken,
    frame_tx: mpsc::Sender<Vec<u8>>,
    iface: Option<String>,
) -> CollectorHandle {
    let sid = session.sid().to_string();
    applog::emit(
        log::Level::Debug,
        "metrics",
        ev::METRICS_COLLECT_START,
        Some(&sid),
        "指标采集启动",
        Some(serde_json::json!({
            "interval_ms": interval.as_millis() as u64,
            "iface": iface,
        })),
    );
    let cancel = root.child_token();
    let last_heartbeat = Arc::new(StdMutex::new(Instant::now()));
    let procs_full = Arc::new(AtomicBool::new(false));
    tokio::spawn(run(
        session,
        interval,
        cancel.clone(),
        frame_tx,
        last_heartbeat.clone(),
        iface,
        procs_full.clone(),
    ));
    CollectorHandle {
        cancel,
        last_heartbeat,
        procs_full,
        sid,
    }
}

/// 采集主循环：立即采首帧 → 等待 interval（无追赶突发），串行天然单飞。
async fn run(
    session: Arc<SshSession>,
    interval: Duration,
    cancel: CancellationToken,
    frame_tx: mpsc::Sender<Vec<u8>>,
    last_heartbeat: Arc<StdMutex<Instant>>,
    iface: Option<String>,
    procs_full: Arc<AtomicBool>,
) {
    // 启动错峰 0~300ms：多个会话同时连接时避免 exec 通道齐步走
    let jitter_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos() % 300))
        .unwrap_or(0);
    tokio::select! {
        _ = cancel.cancelled() => return,
        _ = sleep(Duration::from_millis(jitter_ms)) => {}
    }

    let mut prev_cpu: Option<CpuTimes> = None;
    // 各段最近一次有效值：本轮该段缺失/为空时沿用上一次，界面不回落到 0/占位。
    // 采集失败轮根本不发帧（前端保留上一帧），这里覆盖的是「执行成功但某段空缺」的半坏帧。
    let mut cpu_util_cache: f32 = 0.0;
    let mut load_cache: Option<[f32; 3]> = None;
    let mut mem_cache = MemMetrics::default();
    let mut net_cache = NetMetricsPayload {
        rx_rate: 0.0,
        tx_rate: 0.0,
        rx_bytes: 0,
        tx_bytes: 0,
    };
    // 网卡速率需 (采样时刻, 计数) 二元组做差；墙钟差兼容后台 Tab 节流后的非固定间隔
    let mut prev_net: Option<(Instant, NetCounters)> = None;
    // 磁盘低频夹带：第 0 轮（首帧）立即采，之后每 DISK_INTERVAL_ROUNDS 轮一次；
    // 非夹带轮/df 失败沿用上一次缓存，UI 不闪空
    let mut tick: u32 = 0;
    let mut disks_cache: Vec<DiskMetrics> = Vec::new();
    // 进程 CPU 差值需 per-pid (采样时刻, 累计节拍) 表；每轮以本轮存活进程重建，
    // 退出的进程自然淘汰，无泄漏
    let mut prev_procs: HashMap<u32, (Instant, u64)> = HashMap::new();
    let mut procs_cache: Option<ProcsPayload> = None;
    // docker 归属缓存：pid→容器 ID（增量维护）与容器 ID→名称（首次/未知容器时查询）。
    // 名称查询一次 docker ps 即可，绝不每轮重复；cgroup 日常只扫新 PID。
    let mut docker_cg: HashMap<u32, String> = HashMap::new();
    let mut docker_names: HashMap<String, String> = HashMap::new();
    // 已扫过 cgroup 的 PID（含非容器进程）：增量扫描只看差集；按存活进程裁剪防泄漏
    let mut docker_scanned: HashSet<u32> = HashSet::new();
    // 已确认查不到名称的容器 ID：避免对无名容器每轮重发 docker ps
    let mut docker_unknown: HashSet<String> = HashSet::new();
    let mut docker_names_loaded = false;
    // 出现新的无名容器 ID 时置位，下一轮补一次名称查询后清除
    let mut docker_names_refresh = false;
    // GPU 静态探测态：None=尚未探测（首轮乐观带段试一次）；Some(false)=空输出判定无卡，
    // 后续轮次永久不带段（省 nvidia-smi 冷启动开销）；Some(true)=每轮带段。
    // 已确认有卡后单轮空输出视为瞬态失败，沿用缓存、不改判，避免驱动抖动导致区块闪没。
    let mut gpu_supported: Option<bool> = None;
    let mut gpus_cache: Vec<GpuItemPayload> = Vec::new();
    // 连续 exec 失败计数：成功即清零；计满 FAILURE_BACKOFF_THRESHOLD 轮降为慢轮询
    let mut consecutive_failures: u32 = 0;
    // 是否曾有过成功采集：用于区分「环境不支持」（无 procfs 的主机，首轮即失败，
    // DEBUG）与「运行时降级」（本来采得到中途连续失败，WARN）
    let mut ever_succeeded = false;
    let mut seq: u64 = 0;

    loop {
        // 心跳 TTL 检查：无人消费则静默退出（不取消会话本身）
        if last_heartbeat.lock().unwrap().elapsed() > HEARTBEAT_TTL {
            debug!("指标采集心跳超时，自动停止");
            applog::emit(
                log::Level::Debug,
                "metrics",
                ev::METRICS_COLLECT_STOP,
                Some(session.sid()),
                "指标采集停止",
                Some(serde_json::json!({ "reason": "heartbeat_timeout" })),
            );
            return;
        }

        // 采样时刻取 exec 之前：远端 /proc 读数近似此刻，相邻轮次差值即真实墙钟间隔
        let sampled_at = Instant::now();
        let round = tick;
        let with_disk = round.is_multiple_of(DISK_INTERVAL_ROUNDS);
        tick = tick.wrapping_add(1);
        // 未判定无卡前轮轮带段（首轮探测 + 已确认有卡后的常态采集）
        let with_gpu = gpu_supported != Some(false);
        // docker 归属：首帧/周期全量，其余轮只扫上一轮之后新出现的 PID；
        // 名称映射仅在首次、未知容器、周期兜底时查询（docker ps 不随每轮走）。
        let full_docker = round.is_multiple_of(DOCKER_FULL_SCAN_ROUNDS);
        let new_pids: Vec<u32> = prev_procs
            .keys()
            .copied()
            .filter(|pid| !docker_scanned.contains(pid))
            .collect();
        let docker_scope = if full_docker || new_pids.len() > DOCKER_INCREMENTAL_MAX_PIDS {
            DockerScanScope::All
        } else if !new_pids.is_empty() {
            DockerScanScope::Pids(&new_pids)
        } else if docker_names_refresh {
            DockerScanScope::NamesOnly
        } else {
            DockerScanScope::Off
        };
        let with_docker_names = match docker_scope {
            DockerScanScope::Off => false,
            DockerScanScope::NamesOnly => true,
            DockerScanScope::All => {
                !docker_names_loaded
                    || docker_names_refresh
                    || round.is_multiple_of(DOCKER_NAMES_REFRESH_ROUNDS)
            }
            DockerScanScope::Pids(_) => !docker_names_loaded || docker_names_refresh,
        };
        // 超时返回的是半截输出：缺 @@END@@ 说明脚本没跑完，本帧指标与进程快照都不可信；
        // 通道提前关闭时还会出现“成功但缺段”的输出。两类坏帧都按失败轮处理
        // （不发帧、沿用上一帧），绝不把 0%/N/A 的坏帧推给界面。
        let round_result: Result<RawMetrics, String> = match session
            .exec_collect(
                &metrics_script(with_disk, with_gpu, docker_scope, with_docker_names),
                ROUND_TIMEOUT,
            )
            .await
        {
            Ok(exec) if exec.timed_out && !exec.output.contains("@@END@@") => Err(format!(
                "采集超时且输出不完整（{} 字节，缺 @@END@@）",
                exec.output.len()
            )),
            Ok(exec) => {
                let raw = parse_metrics_output(&exec.output, iface.as_deref());
                // CPU 与内存段都缺 = 远端输出不可信（正常主机两者必有）；
                // 保留上一帧比显示 0%/N/A 更接近真实
                if raw.cpu.is_none() && raw.mem.total == 0 {
                    Err(format!(
                        "输出缺少 CPU/内存段（{} 字节）",
                        exec.output.len()
                    ))
                } else {
                    Ok(raw)
                }
            }
            Err(e) => Err(e.to_string()),
        };
        match round_result {
            Ok(mut raw) => {
                // exec 成功即解除退避（即使部分 marker 段缺失也算通道健康）
                consecutive_failures = 0;
                ever_succeeded = true;
                // CPU 利用率需相邻两次快照差值；首帧（或本轮缺 stat）沿用上一次值
                let util = match (prev_cpu, raw.cpu) {
                    (Some(p), Some(c)) => cpu_util(p, c),
                    _ => cpu_util_cache,
                };
                if raw.cpu.is_some() {
                    prev_cpu = raw.cpu;
                    cpu_util_cache = util;
                }
                // 负载：loadavg 缺失时沿用上一次，避免界面周期性跳 N/A
                let load = raw.load.or(load_cache);
                if raw.load.is_some() {
                    load_cache = raw.load;
                }
                // 内存：MemTotal 为 0 视为本轮内存段缺失，沿用上一次快照（不推 0%）
                if raw.mem.total > 0 {
                    mem_cache = raw.mem;
                }

                // 网络速率同理：缺 netdev 时沿用上一次速率与计数，不把 0 推给界面
                let net = if let Some(cur) = raw.net {
                    let mut n = NetMetricsPayload {
                        rx_rate: 0.0,
                        tx_rate: 0.0,
                        rx_bytes: cur.rx_bytes,
                        tx_bytes: cur.tx_bytes,
                    };
                    if let Some((prev_at, prev)) = prev_net {
                        let dt = sampled_at.saturating_duration_since(prev_at);
                        n.rx_rate = byte_rate(prev.rx_bytes, cur.rx_bytes, dt);
                        n.tx_rate = byte_rate(prev.tx_bytes, cur.tx_bytes, dt);
                    }
                    prev_net = Some((sampled_at, cur));
                    net_cache = n;
                    n
                } else {
                    net_cache
                };

                // 磁盘仅夹带轮有新解析结果（df 失败时为 None）；空结果视为本轮失败，沿用缓存
                if let Some(disks) = raw.disks {
                    if !disks.is_empty() {
                        disks_cache = disks;
                    }
                }

                // docker 增量合并：名称段是完整快照才整体替换；
                // cgroup 全量轮整体替换（同时清掉已退出归属），增量轮只合并新 PID
                let names_scanned = raw.docker.as_ref().is_some_and(|s| s.names_scanned);
                if let Some(scan) = raw.docker.take() {
                    if scan.names_scanned {
                        docker_names = scan.names;
                        docker_names_loaded = true;
                    }
                    match docker_scope {
                        DockerScanScope::All => {
                            docker_cg = scan.cg_ids;
                            docker_scanned = prev_procs.keys().copied().collect();
                        }
                        DockerScanScope::Pids(pids) => {
                            docker_cg.extend(scan.cg_ids);
                            docker_scanned.extend(pids.iter().copied());
                        }
                        DockerScanScope::NamesOnly | DockerScanScope::Off => {}
                    }
                }
                // 进程段空输出（/proc 不可读等）按本轮失败处理：不推空榜单、不清归属缓存
                if raw.procs.as_ref().is_some_and(|p| p.procs.is_empty()) {
                    raw.procs = None;
                }
                // 按本轮存活进程裁剪缓存（退出的 PID 自然淘汰），并补齐容器归属标注。
                // 首轮全量后把当前存活集记为已扫，否则下一轮会把全部旧进程当“新增”再全量一次；
                // 后续全量仍以「上轮进程集」为已扫基线，保证全量期间新起的进程下轮增量补扫。
                if let Some(proc_raw) = &mut raw.procs {
                    let alive: HashSet<u32> = proc_raw.procs.iter().map(|p| p.pid).collect();
                    docker_cg.retain(|pid, _| alive.contains(pid));
                    if round == 0 {
                        docker_scanned.extend(alive.iter().copied());
                    }
                    docker_scanned.retain(|pid| alive.contains(pid));
                    for snap in &mut proc_raw.procs {
                        if let Some(cid) = docker_cg.get(&snap.pid) {
                            // 名字查不到时给 12 位短 ID（与 docker ps 一致），
                            // 别把 64 位长 ID 直接糊到列表上
                            snap.container = container_name_for(&docker_names, cid)
                                .map(str::to_string)
                                .unwrap_or_else(|| cid.chars().take(12).collect());
                        }
                    }
                }
                // 名称缓存：已确认查不到的容器 ID 记入 unknown，避免每轮重发 docker ps；
                // 新出现的无名 ID 置位 refresh，下一轮补查一次（容器刚启动可能还没进 ps）
                if names_scanned {
                    docker_unknown = docker_cg
                        .values()
                        .filter(|cid| container_name_for(&docker_names, cid).is_none())
                        .cloned()
                        .collect();
                    docker_names_refresh = false;
                }
                if docker_cg.values().any(|cid| {
                    container_name_for(&docker_names, cid).is_none()
                        && !docker_unknown.contains(cid)
                }) {
                    docker_names_refresh = true;
                }

                // 进程：本轮有 stat 快照才算榜（首帧新进程 CPU 全 0，次帧起有效）；
                // 段缺失时沿用上一次榜单；prev 表用本轮存活集合重建
                if let Some(proc_raw) = &raw.procs {
                    let payload_procs = build_procs_payload(
                        proc_raw,
                        &prev_procs,
                        sampled_at,
                        procs_full.load(Ordering::Relaxed),
                    );
                    prev_procs = proc_raw
                        .procs
                        .iter()
                        .map(|p| (p.pid, (sampled_at, p.cpu_ticks)))
                        .collect();
                    procs_cache = Some(payload_procs);
                }

                // GPU：Some(空) 在未探测时终审为无卡；非空更新缓存；
                // None（未带段）与有卡后的瞬时空输出均不改变现状
                if let Some(gpus) = raw.gpus {
                    if gpus.is_empty() {
                        if gpu_supported.is_none() {
                            gpu_supported = Some(false);
                            // 无 GPU 是云主机/虚拟机的常态环境事实而非故障，记 DEBUG：
                            // 默认面板不显示，排查"为何没有 GPU 区块"时可调出
                            applog::emit(
                                log::Level::Debug,
                                "metrics",
                                ev::METRICS_COLLECT_DEGRADED,
                                Some(session.sid()),
                                "GPU 指标不可用，跳过",
                                Some(serde_json::json!({
                                    "component": "gpu",
                                    "reason": "not_present",
                                })),
                            );
                        }
                    } else {
                        gpu_supported = Some(true);
                        gpus_cache = gpus;
                    }
                }

                seq += 1;

                let payload = MetricsPayload {
                    seq,
                    cpu: CpuMetricsPayload { util, load },
                    mem: mem_cache,
                    net,
                    disks: disks_cache.clone(),
                    procs: procs_cache.clone().unwrap_or(ProcsPayload {
                        total: 0,
                        top: Vec::new(),
                        all: Vec::new(),
                    }),
                    gpus: gpus_cache.clone(),
                };
                let json = serde_json::to_vec(&payload).unwrap_or_default();
                // 帧接收端关闭（连接终结）：任务退出
                if frame_tx
                    .send(encode_frame(FrameType::Metrics, &json))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // 采集失败静默降级：跳过本轮不发帧（前端保留最后一帧），累计失败数，
            // 绝不影响 PTY 主通道；连续失败达阈值后由下方节拍切慢轮询
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                debug!("指标采集本轮失败（连续第 {consecutive_failures} 轮）: {e}");
                // 刚达阈值记一次降级事件（边沿触发）；之后每轮不再重复，
                // 成功清零后若再次达到阈值才会再记。
                // 首轮从未成功过 → 目标环境不支持（无 procfs 等，常态，DEBUG）；
                // 曾成功后再连续失败 → 真运行时降级（WARN）。
                if consecutive_failures == FAILURE_BACKOFF_THRESHOLD {
                    if ever_succeeded {
                        applog::emit(
                            log::Level::Warn,
                            "metrics",
                            ev::METRICS_COLLECT_DEGRADED,
                            Some(session.sid()),
                            "采集连续失败，指标降级为慢轮询",
                            Some(serde_json::json!({
                                "component": "exec",
                                "reason": "runtime_failure",
                                "consecutive": consecutive_failures,
                                "backoff_ms": BACKOFF_INTERVAL.as_millis() as u64,
                                "err": e.to_string(),
                            })),
                        );
                    } else {
                        applog::emit(
                            log::Level::Debug,
                            "metrics",
                            ev::METRICS_COLLECT_DEGRADED,
                            Some(session.sid()),
                            "目标主机不支持指标采集，转入慢轮询",
                            Some(serde_json::json!({
                                "component": "exec",
                                "reason": "unsupported",
                                "err": e.to_string(),
                            })),
                        );
                    }
                }
            }
        }

        // 节拍：正常用 interval，连续失败达阈值降为 10s 慢轮询。
        // 注意退避 sleep(10s) 长于心跳 TTL(9s) 但不会误杀活跃会话——前端无论 collector
        // 睡多久都每 5s 续约 last_heartbeat；影响仅为「UI 消失后」最坏 ~10s+9s 才自停。
        // cancel 仍立即醒，会话 shutdown 不被拖延。
        let delay = round_delay(interval, consecutive_failures);
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = sleep(delay) => {}
        }
    }
}

#[cfg(test)]
mod backoff_tests {
    use super::{BACKOFF_INTERVAL, round_delay};
    use std::time::Duration;

    #[test]
    fn delay_normal_until_threshold_then_backoff() {
        let normal = Duration::from_secs(3);
        // 0/1/2 次失败：维持正常间隔
        assert_eq!(round_delay(normal, 0), normal);
        assert_eq!(round_delay(normal, 1), normal);
        assert_eq!(round_delay(normal, 2), normal);
        // 连续 3 次起降为 10s 慢轮询
        assert_eq!(round_delay(normal, 3), BACKOFF_INTERVAL);
        assert_eq!(round_delay(normal, 10), BACKOFF_INTERVAL);
    }

    #[test]
    fn delay_recovers_immediately_on_success() {
        // 成功清零后（调用方传 0）立即恢复正常间隔，不残留退避
        assert_eq!(
            round_delay(Duration::from_secs(2), 0),
            Duration::from_secs(2)
        );
    }
}
