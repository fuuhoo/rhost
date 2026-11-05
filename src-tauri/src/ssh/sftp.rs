//! SFTP 子系统：复用已认证 SSH 连接打开 `sftp` subsystem 通道，浏览/传输远端文件。
//!
//! 与 PTY / exec 通道相互独立：每条会话懒加载一个长驻 [`RawSftpSession`]
//! （SFTP 协议内部支持多请求并发），会话断开时随 [`SshSession`] 一并丢弃；
//! 通道中途异常时缓存可失效，下次操作自动重建（列目录重试一次）。
//!
//! 之所以直接用底层 [`RawSftpSession`] 而非高层 `SftpSession`：SFTP v3 的
//! `SSH_FILEXFER_ATTRS` 只携带数字 uid/gid，属主/属组名仅存在于 NAME 响应的
//! `longname`（`ls -l` 风格文本）中，高层 read_dir 将其丢弃，故自行收发
//! READDIR 并解析 longname。
//!
//! 文件传输：固定 64KB 分块顺序 read/write，进度经 Tauri IPC
//! [`Channel`] 流式推给前端（100ms 节流，收尾必发一次）；每个任务注册
//! [`TransferCtl`]（取消令牌 + 暂停开关），支持前端暂停/继续/取消。
//!
//! 传输行为（覆盖策略/临时文件/元数据保留/断点校验/整文件哈希）由前端每次调用
//! 传入的 [`TransferPolicy`] 决定，Rust 侧刻意不读 app_config.json——行为设置的
//! 单一事实源在前端。缺省（`None`）逐项等价于本机制引入前的旧行为，故 AI 面板等
//! 旧调用方不受影响。冲突与校验结果通过 [`SshError`] 的稳定前缀
//! （`SFTP_CONFLICT:` / `SFTP_SKIPPED:` / `SFTP_VERIFY_FAILED:`）回传前端分流 UI。

use std::collections::HashMap;
use std::io::SeekFrom;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::TimeZone;
use log::debug;
use russh::client;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::rawsession::RawSftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::SshError;
use super::session::{ClientHandler, SshSession};

/// 默认单块传输大小：64KB（SFTP v3 规定服务端必须接受 ≥32KB，64KB 在多数服务端吞吐更优，
/// 且远小于整文件，保证流式不占内存）。前端可通过全局设置覆盖（8KB ~ 1MB）
const DEFAULT_CHUNK: usize = 64 * 1024;
/// 进度事件最小间隔（节流）；收尾时不受此限强制补发一次
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// 临时文件后缀：与最终路径同目录（rename 原子性要求同一文件系统），完成后原子落位
const TEMP_SUFFIX: &str = ".rhost-tmp";
/// sizeMtime 断点校验的新鲜度窗口（秒）。见 [`partial_is_fresh`] 的取舍说明。
const RESUME_FRESH_WINDOW: i64 = 24 * 3600;
/// 编辑器取回副本的大小上限：编辑对象是文本/配置一类，超大文件既不该整份落进
/// 临时目录，也不该悄悄占满 SFTP 通道（下载走正常传输队列，不受此限）。
const EDIT_FETCH_MAX: u64 = 32 * 1024 * 1024;

/// 单次传输的行为策略：前端在 invoke 时从全局设置 `savedSettings` 组装传入
/// （与 `chunk_kb`/`resume` 同一传参约定，Rust 侧刻意不读 app_config.json）。
/// 缺省（None/字段缺失）全部保持旧行为：覆盖 / 不落地临时文件 / 不保留元数据 /
/// size 断点校验 / 不哈希校验——旧调用方（如 AI 面板）不受影响。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TransferPolicy {
    /// 目标已存在且非续传候选时的处置：newer / skip / overwrite / ask
    pub overwrite_policy: String,
    /// 临时文件机制（**仅上传方向**）：字节先写 `{远端}.rhost-tmp`，完成后原子重命名。
    /// 下载刻意不走 temp：本地半截文件本身就是事实上的临时文件（续传与覆盖策略都按它
    /// 裁决），而 rename 落位会替换掉用户可能正打开的本地文件——已打开的 fd 仍指向旧
    /// inode，界面上的文件却悄悄换了内容，风险大于收益（见 [`download_file`]）。
    pub upload_temp: bool,
    /// 传输后将源文件 mtime + 权限位 SETSTAT/落地到目标（受服务器权限限制，失败仅告警）
    pub preserve_meta: bool,
    /// 续传前半截文件完整性校验：size / sizeMtime / sha256
    pub resume_check: String,
    /// 传输后整文件 SHA256 比对源与目标（不一致任务判失败）。
    /// `resume_check=sha256` 时隐含开启（见 [`verify_required`]）；0 字节文件跳过
    pub verify_hash: bool,
    /// ask 策略下用户对冲突的逐任务裁决（冲突弹窗后重投时注入）：'overwrite' | 'skip'
    pub resolve: Option<String>,
    /// 全局带宽上限（KB/s，0 = 不限速）：**同一条 (主机:端口, 用户) 链路的所有传输
    /// 共用一个令牌桶**，包括另一个标签页连同一台服务器的传输——否则设置值会被
    /// 每个标签各自跑满。实现见 [`global_meter`]。
    pub global_rate_kb: u64,
    /// 单任务带宽上限（KB/s，0 = 不限速）：只有本任务用得到
    pub task_rate_kb: u64,
}

impl Default for TransferPolicy {
    /// 保守缺省 = 本功能引入前的既有行为，保证 Option::None 路径零变化
    fn default() -> Self {
        Self {
            overwrite_policy: "overwrite".into(),
            upload_temp: false,
            preserve_meta: false,
            resume_check: "size".into(),
            verify_hash: false,
            resolve: None,
            global_rate_kb: 0,
            task_rate_kb: 0,
        }
    }
}

/* ==================== 带宽限速 ==================== */

/// 漏桶计量器：为每次申请预约「完成时刻」，`next_free` 即该桶下一次可发时刻。
/// 多任务共用一桶时各算各的配额，合计速率收敛于上限，而不是各自都跑满。
/// 刻意不持锁 sleep——[`Throttle::acquire`] 在释放锁之后才 await。
#[derive(Debug)]
struct RateMeter {
    /// 上限（B/s）；0 = 不限速
    bytes_per_s: u64,
    next_free: Instant,
}

impl RateMeter {
    fn new() -> Self {
        Self { bytes_per_s: 0, next_free: Instant::now() }
    }

    /// 申请 `n` 字节额度，返回「这块数据还需等待多久」。
    /// 每次带上当前上限 `bps`：用户在设置里改限速值时，共享桶就地换速率并
    /// 把基准时刻拉回当下（否则旧速率的预约会拖累新速率）。
    fn acquire(&mut self, bps: u64, n: u64, now: Instant) -> Duration {
        if self.bytes_per_s != bps {
            self.bytes_per_s = bps;
            self.next_free = now;
        }
        if bps == 0 || n == 0 {
            return Duration::ZERO;
        }
        let start = now.max(self.next_free);
        // 纳秒精度：1KB @ 1KB/s 也要算出准确等待，避免整数除法把限速清零
        let cost = Duration::from_nanos((n as u128 * 1_000_000_000 / bps as u128) as u64);
        let due = start + cost;
        self.next_free = due;
        due.saturating_duration_since(now)
    }
}

type StdMeter = Arc<std::sync::Mutex<RateMeter>>;

/// 跨会话共享的全局限速桶注册表，键 = `"{host}:{port}"` + 用户名。
/// 进程内静态表：条目数等于用过的不同 (链路, 用户) 组合，不随传输次数增长，
/// 也不需要把 SessionManager 的 Arc 塞进每个会话去共享。
static SFTP_RATE: std::sync::OnceLock<std::sync::Mutex<HashMap<(String, String), StdMeter>>> =
    std::sync::OnceLock::new();

fn global_meter(endpoint: &str, user: &str) -> StdMeter {
    let map = SFTP_RATE.get_or_init(Default::default);
    let mut g = map.lock().unwrap_or_else(|e| e.into_inner());
    let key = (endpoint.to_string(), user.to_string());
    g.entry(key)
        .or_insert_with(|| Arc::new(std::sync::Mutex::new(RateMeter::new())))
        .clone()
}

/// 一次传输的限速组合：两道上限同时生效，取更严的等待。
/// 两个都关闭（缺省）时 `acquire` 是空操作，故限速不启用零开销。
struct Throttle {
    global: Option<(StdMeter, u64)>,
    task: Option<(StdMeter, u64)>,
}

impl Throttle {
    fn from_policy(pol: &TransferPolicy, endpoint: &str, user: &str) -> Self {
        Self {
            global: (pol.global_rate_kb > 0)
                .then(|| (global_meter(endpoint, user), pol.global_rate_kb * 1024)),
            task: (pol.task_rate_kb > 0)
                .then(|| (Arc::new(std::sync::Mutex::new(RateMeter::new())), pol.task_rate_kb * 1024)),
        }
    }

    /// 为 `n` 字节申请额度，必要时睡到放行
    async fn acquire(&self, n: u64) {
        let now = Instant::now();
        let mut wait = Duration::ZERO;
        for (meter, bps) in self.global.iter().chain(self.task.iter()) {
            // 锁中毒（上一次 panic）不影响本任务：取回内部值继续计量
            if let Ok(mut g) = meter.lock() {
                wait = wait.max(g.acquire(*bps, n, now));
            }
        }
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

/// 覆盖策略裁决结果（目标已存在且非续传候选）
#[derive(Debug, PartialEq, Eq)]
enum Conflict {
    /// 继续传输（从头截断重传）
    Proceed,
    /// 跳过：目标保持原样，返回 SFTP_SKIPPED 协议错误供前端标记「已跳过」
    Skip,
    /// 策略为 ask 且尚无用户裁决：返回 SFTP_CONFLICT 协议错误供前端弹窗
    Ask,
}

/// 覆盖策略裁决（纯函数）。`resolve` 为用户逐任务裁决，优先于全局策略——
/// 它来自冲突弹窗的显式点击，是比设置更强的意图表达。
/// - skip → Skip；ask 无裁决 → Ask；overwrite/未知值 → Proceed（未知值降级为旧行为）；
/// - newer：两侧 mtime 齐备时仅源严格更新才覆盖（等于视为已同步，不重传）；
///   任一侧 mtime 不可得时退化为 size 比对——大小不等视为源已变更（覆盖），
///   大小相等视为已同步（跳过），与 rsync 的 size-only 保守判断同理。
fn decide_conflict(
    policy: &str,
    resolve: Option<&str>,
    src_size: u64,
    dst_size: u64,
    src_mtime: Option<i64>,
    dst_mtime: Option<i64>,
) -> Conflict {
    if let Some(r) = resolve {
        return if r == "skip" { Conflict::Skip } else { Conflict::Proceed };
    }
    match policy {
        "skip" => Conflict::Skip,
        "ask" => Conflict::Ask,
        "newer" => match (src_mtime, dst_mtime) {
            (Some(s), Some(d)) => {
                if s > d { Conflict::Proceed } else { Conflict::Skip }
            }
            _ => {
                if src_size != dst_size { Conflict::Proceed } else { Conflict::Skip }
            }
        },
        _ => Conflict::Proceed,
    }
}

/// 派生临时文件路径（纯函数）：最终路径直接追加后缀，保证同目录（rename 原子前提）
fn temp_path_of(final_path: &str) -> String {
    format!("{final_path}{TEMP_SUFFIX}")
}

/// 目录路径自身及其各级父目录（由根到叶，不含根 `/`）：`/a/b/c` → `/a`、`/a/b`、`/a/b/c`。
/// 出现 `..` 段时放弃推导返回空——路径不可信，宁可让服务端按原路径报错，
/// 也不凭猜测在远端建目录。相对路径（锚 SFTP 家目录）同样适用。
fn dir_chain_of(path: &str) -> Vec<String> {
    let absolute = path.starts_with('/');
    let mut out: Vec<String> = Vec::new();
    let mut acc = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty() && *s != ".") {
        if seg == ".." {
            return Vec::new();
        }
        acc = if acc.is_empty() {
            if absolute {
                format!("/{seg}")
            } else {
                seg.to_string()
            }
        } else {
            format!("{acc}/{seg}")
        };
        out.push(acc.clone());
    }
    out
}

/// 创建单个远端目录，返回是否真的新建（已存在 = false）。`probe` 为真时先 stat：
/// 已存在且确为目录视为成功，存在但同名的是文件则明确报错——绝不为了建目录而动用户已有文件。
/// `probe` 为假（其父目录本轮刚创建，子项不可能已存在）时直接 mkdir，省掉一次往返；
/// 整棵新树上传因此少一半请求。
async fn mkdir_one(sftp: &RawSftpSession, dir: &str, probe: bool) -> Result<bool, SshError> {
    if probe {
        if let Ok(a) = sftp.stat(dir).await {
            return if a.attrs.file_type().is_dir() {
                Ok(false)
            } else {
                Err(SshError::Channel(format!(
                    "远端 {dir} 已存在且不是目录，无法作为上级目录使用"
                )))
            };
        }
    }
    sftp.mkdir(dir, FileAttributes::default())
        .await
        .map(|_| true)
        .map_err(|e| SshError::Channel(format!("创建目录 {dir} 失败: {e}")))
}

/// 一批目标目录 → 需要创建的完整目录集合（各级父目录 + 自身，去重且父先于子）。
/// 纯函数：父先于子保证创建顺序正确，去重保证同一棵树被多次拖入也不重复请求。
fn dir_tree_of(paths: &[String]) -> Vec<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for p in paths {
        for d in dir_chain_of(p) {
            if seen.insert(d.clone()) {
                out.push(d);
            }
        }
    }
    out
}

/// 本次传输是否需要整文件 SHA256 校验（纯函数）：显式开启 `verify_hash`，
/// 或断点校验方式为 sha256——后者承诺「半截文件可信」，续传完成后必须用整文件
/// 哈希证明（仅比对偏移处前缀无法保证拼接后的整体一致），故强制开启。
fn verify_required(pol: &TransferPolicy) -> bool {
    pol.verify_hash || pol.resume_check == "sha256"
}

/// sizeMtime 断点校验：半截文件 mtime 距当前时间 24h 内视为「本客户端近期写就」，
/// 可续传；过旧或探不到 mtime 一律整文件重传。
/// 判断取舍（文档化）：客户端无法记住「上次写入的时刻」（写入后服务端 mtime 已被刷新，
/// 本地也不持久化探测记录），故以「现在」为锚的时限窗口作为「近期写过」的代理——
/// 半截文件放置超过一天，多半已被第三方改动或环境已变，续传风险高于重传成本。
fn partial_is_fresh(mtime: Option<i64>, now: i64) -> bool {
    mtime.is_some_and(|m| (now - m).abs() <= RESUME_FRESH_WINDOW)
}

/// 断点校验总裁决（纯函数）：size 恒过（既有行为）；sizeMtime 看新鲜度窗口；
/// sha256 看前缀哈希是否一致（`hash_equal` 由调用方流式计算，任何 IO 错误应传 false 从严）。
/// 未知/缺失值降级为 size，兼容旧前端与脏设置。
fn resume_check_ok(check: &str, fresh: bool, hash_equal: bool) -> bool {
    match check {
        "sizeMtime" => fresh,
        "sha256" => hash_equal,
        _ => true,
    }
}

/// 字节切片 → 小写 hex（哈希摘要展示用，与 config_io 同一实现约定）
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// READ 失败时的现场快照。SSH_FX_FAILURE 是服务端的兜底码（目录、特殊文件、
/// 后端 errno 未映射都可能归到它），只看到 `Failure: Failure` 判不出成因，
/// 所以把状态码编号与服务端原文、stat 的类型/mode/size/属主、失败偏移与请求长度一并报出。
/// 不额外发请求：出错通道可能已经半死，再等一次 lstat 会把瞬时报错拖成 20s 超时。
fn read_fail_detail(e: &SftpError, attrs: &FileAttributes, offset: u64, chunk: u32) -> String {
    let status = match e {
        SftpError::Status(s) => format!(
            "{}（code={}、服务端消息 {:?}、语言标签 {:?}、响应 id={}）",
            s.status_code,
            s.status_code as i32,
            s.error_message,
            s.language_tag,
            s.id
        ),
        other => other.to_string(),
    };
    format!(
        "{status}；stat 说 type={:?} mode=0o{:o} size={} mtime={} 属主={}:{}；失败于偏移 {offset}，本次请求 {chunk} 字节",
        attrs.file_type(),
        attrs.permissions.unwrap_or(0),
        attrs.size.unwrap_or(0),
        fmt_mtime(attrs.mtime),
        attrs.uid.unwrap_or(0),
        attrs.gid.unwrap_or(0),
    )
}

/// 单个远端目录项（字段与前端 FsEntry / 本地 LocalFsEntry 对齐）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFsEntry {
    pub name: String,
    pub is_dir: bool,
    /// 是否为符号链接。指向目录的软链 is_dir 为 true，可双击进入；
    /// 指向文件的软链 is_dir 为 false，按文件处理。
    pub is_symlink: bool,
    /// 文件字节数；目录/未知恒为 0（前端显示 —）
    pub size: u64,
    /// 本地时区 "YYYY-MM-DD HH:MM"；远端未提供时为空串
    pub mtime: String,
    /// 权限字符串 "rwxr-xr-x"
    pub perm: String,
    /// 属主名（FinalShell 的「用户」列）；longname 解析失败时回退 uid 字符串，缺失为空串
    pub owner_user: String,
    /// 属组名单独一列（FinalShell 的「用户组」列）
    pub owner_group: String,
    /// longname 首字符的类型位：`d` 目录 / `l` 链接 / `-` 普通文件 / `c`·`b` 设备 /
    /// `p` 管道 / `s` 套接字；longname 缺失或格式非标准时为空串（前端按 isDir 兜底显示）
    pub kind: String,
}

/// 一次远端目录列举结果：path 为服务端实际路径
/// （请求路径缺省时为 realpath(".") 得到的登录默认目录）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDirListing {
    pub path: String,
    pub entries: Vec<RemoteFsEntry>,
}

/// 远端路径元数据（编辑器回写前的「远端是否已被别人改过」判定用）。
/// `exists` 为假表示路径已不存在——删除同样算「变了」，不能静默写回。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteStat {
    pub exists: bool,
    pub is_dir: bool,
    pub size: u64,
    /// 修改时间（epoch 毫秒）；服务端未提供时为 0
    pub mtime_ms: i64,
}

/// 远端目录树的单个文件（字段与本地 `LocalWalkFile` 对齐）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteWalkFile {
    /// 相对根目录的路径，恒用 `/` 连接，可直接拼到本地落点之后
    pub rel: String,
    /// 远端绝对路径
    pub abs: String,
    pub size: u64,
}

/// 一次远端目录树枚举结果：与 `local_walk` 同构，让下载方向复用上传的
/// 「先建骨架、再逐文件入队」流程。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteWalkResult {
    pub root: String,
    pub name: String,
    pub is_dir: bool,
    pub files: Vec<RemoteWalkFile>,
    /// 根之下各级目录的相对路径（含空目录，不含根本身）：本地据此复刻骨架
    pub dirs: Vec<String>,
}

/// 打开 sftp 子系统通道并完成 SFTP 协议初始化（SSH_FXP_INIT / VERSION 协商）。
///
/// 失败常见原因：远端 sshd 未配置 `Subsystem sftp`（受限 Shell / 精简容器）。
async fn open_sftp(
    handle: &Arc<Mutex<client::Handle<ClientHandler>>>,
) -> Result<RawSftpSession, SshError> {
    let channel = {
        let h = handle.lock().await;
        let channel = h
            .channel_open_session()
            .await
            .map_err(|e| SshError::Channel(format!("打开 SFTP 通道失败: {e}")))?;
        channel.request_subsystem(true, "sftp").await.map_err(|e| {
            SshError::Channel(format!(
                "远端拒绝 SFTP 子系统（sftp-server 可能未启用）: {e}"
            ))
        })?;
        channel
    };
    let sftp = RawSftpSession::new(channel.into_stream());
    sftp.set_timeout(20);
    sftp.init()
        .await
        .map_err(|e| SshError::Channel(format!("SFTP 协议初始化失败: {e}")))?;
    debug!("SFTP 子系统通道已建立");
    Ok(sftp)
}

/// 单个传输任务的运行时控制：取消令牌 + 暂停开关。
///
/// 取消为单向（CancellationToken::cancel 不可逆），暂停为布尔开关
/// （watch channel，循环里 await 其变回 false 即可继续）。
pub(super) struct TransferCtl {
    pub cancel: CancellationToken,
    pub paused: watch::Sender<bool>,
}

/// 进度事件载荷（与前端约定，camelCase）。`speed` 单位 B/s，
/// `done` 为 true 表示本次为收尾包，前端可据此切到完成态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    pub task_id: u64,
    pub transferred: u64,
    pub total: u64,
    pub speed: u64,
    pub done: bool,
}

/// 每会话的 SFTP 长驻会话缓存：懒加载，坏通道可失效重建；
/// 同时持有该会话所有进行中的传输任务控制柄。
///
/// 空闲自动关闭：`idle_sec > 0` 时后台看门狗在「连续 idle_sec 秒没有任何 SFTP 请求
/// 且没有在飞的传输」时关闭通道并丢弃缓存，下次操作由 [`SftpState::get`] 透明重建。
/// 阈值秒数由前端传参（行为设置单一事实源在前端，见模块头），0 = 常驻不关。
pub(super) struct SftpState {
    inner: Arc<Mutex<Option<Arc<RawSftpSession>>>>,
    tasks: Arc<Mutex<HashMap<u64, Arc<TransferCtl>>>>,
    /// 文件管理操作（删除/重命名/复制等多请求串行操作）互斥锁，
    /// 避免递归遍历期间目录被另一个管理操作改动
    manage: Mutex<()>,
    /// 会话标签（通道建立日志事件用）
    sid: String,
    /// 全局限速桶键：`"{host}:{port}"` + 用户名（见 [`global_meter`]）
    rate_key: (String, String),
    /// 最近一次 SFTP 操作的 Unix 秒（所有请求都经 [`SftpState::get`]，故那里打点即全覆盖）
    last_use: Arc<std::sync::atomic::AtomicU64>,
    /// 空闲关闭阈值秒，0 = 不自动关闭
    idle_sec: Arc<std::sync::atomic::AtomicU32>,
}

impl SftpState {
    pub(super) fn new(
        sid: &str,
        endpoint: &str,
        user: &str,
        cancel: &CancellationToken,
    ) -> Self {
        let state = Self {
            inner: Arc::new(Mutex::new(None)),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            manage: Mutex::new(()),
            sid: sid.to_string(),
            rate_key: (endpoint.to_string(), user.to_string()),
            last_use: Arc::new(std::sync::atomic::AtomicU64::new(now_unix().max(0) as u64)),
            idle_sec: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        };
        state.spawn_idle_watch(cancel.clone());
        state
    }

    /// 设定空闲关闭阈值（前端 sftpIdleTimeoutSec；0 = 关闭该机制）
    pub(super) fn set_idle_timeout(&self, sec: u32) {
        self.idle_sec.store(sec, std::sync::atomic::Ordering::Relaxed);
    }

    fn idle_sec_loaded(&self) -> u32 {
        self.idle_sec.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 空闲看门狗：5s 一轮。挂在会话取消令牌上，断连即随会话一起退出。
    /// 有传输在飞时只顺延、不关闭——那说明通道正被使用，只是进度慢。
    fn spawn_idle_watch(&self, cancel: CancellationToken) {
        let inner = self.inner.clone();
        let tasks = self.tasks.clone();
        let last_use = self.last_use.clone();
        let idle_sec = self.idle_sec.clone();
        let sid = self.sid.clone();
        tokio::spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                let limit = idle_sec.load(Relaxed);
                if limit == 0 {
                    continue;
                }
                if !tasks.lock().await.is_empty() {
                    continue;
                }
                let idle = now_unix().max(0) as u64;
                let idle = idle.saturating_sub(last_use.load(Relaxed));
                if idle < limit as u64 {
                    continue;
                }
                let session = {
                    let mut g = inner.lock().await;
                    g.take()
                };
                if let Some(s) = session {
                    // 发送 SSH_FXP_CLOSE 失败不影响本地丢弃：通道已不可信
                    let _ = s.close_session();
                    drop(s);
                    crate::applog::emit(
                        log::Level::Info,
                        "ssh",
                        crate::applog::events::SSH_SFTP_CHANNEL_CLOSE,
                        Some(&sid),
                        "SFTP 通道空闲超时关闭",
                        Some(serde_json::json!({ "idle_sec": idle, "limit_sec": limit })),
                    );
                }
            }
        });
    }

    /// 注册一个新任务的控制柄并返回（键为前端传入的 task_id）
    pub(super) async fn register_task(&self, task_id: u64, ctl: Arc<TransferCtl>) {
        self.tasks.lock().await.insert(task_id, ctl);
    }

    /// 取指定任务控制柄（暂停/取消用）
    pub(super) async fn get_ctl(&self, task_id: u64) -> Option<Arc<TransferCtl>> {
        self.tasks.lock().await.get(&task_id).cloned()
    }

    /// 任务结束（成功/失败/取消）后从表中移除，避免表无限增长
    pub(super) async fn remove_task(&self, task_id: u64) {
        self.tasks.lock().await.remove(&task_id);
    }

    /// 取缓存的 RawSftpSession；不存在则开新通道初始化。
    /// 持锁建链，并发列目录只在首次/重建时竞争，之后共享同一协议会话。
    /// 这里同时是空闲计时的打点处：每个 SFTP 请求都经过本方法。
    async fn get(
        &self,
        handle: &Arc<Mutex<client::Handle<ClientHandler>>>,
    ) -> Result<Arc<RawSftpSession>, SshError> {
        self.last_use
            .store(now_unix().max(0) as u64, std::sync::atomic::Ordering::Relaxed);
        let mut guard = self.inner.lock().await;
        if let Some(s) = guard.as_ref() {
            return Ok(s.clone());
        }
        let s = Arc::new(open_sftp(handle).await?);
        crate::applog::emit(
            log::Level::Info,
            "ssh",
            crate::applog::events::SSH_SFTP_CHANNEL_OPEN,
            Some(&self.sid),
            "SFTP 通道建立",
            Some(serde_json::json!({ "idle_timeout_sec": self.idle_sec_loaded() })),
        );
        *guard = Some(s.clone());
        Ok(s)
    }

    /// 丢弃缓存的 SFTP 会话（通道异常后调用，下次 get 重新建立）
    async fn invalidate(&self) {
        *self.inner.lock().await = None;
    }
}

/// SFTP 权限位 → "rwxr-xr-x"（与 localfs 本地面板同一套规则，含 s/S/t/T）
fn fmt_perm(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for (bit, chr) in [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ] {
        s.push(if mode & bit != 0 { chr } else { '-' });
    }
    if mode & 0o4000 != 0 {
        s.replace_range(2..=2, if mode & 0o100 != 0 { "s" } else { "S" });
    }
    if mode & 0o2000 != 0 {
        s.replace_range(5..=5, if mode & 0o010 != 0 { "s" } else { "S" });
    }
    if mode & 0o1000 != 0 {
        s.replace_range(8..=8, if mode & 0o001 != 0 { "t" } else { "T" });
    }
    s
}

/// SFTP mtime（Unix 秒）→ 本地时间 "YYYY-MM-DD HH:MM"，与本地面板格式一致
fn fmt_mtime(ts: Option<u32>) -> String {
    match ts {
        Some(t) => chrono::Local
            .timestamp_opt(i64::from(t), 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default(),
        None => String::new(),
    }
}

/// 从 NAME 响应的 longname（sshd 生成的 `ls -l` 风格文本）取「类型位 + 属主 + 属组」。
///
/// 标准布局：`-rw-r--r-- 1 root root 924 Oct 3 11:11 passwd`，
/// 字段依次为 权限串 / 硬链接数 / owner / group / 大小 / 日期 / 文件名。
/// 文件名可含空格故不能从后向前切，但前四字段不含空格，取第 2、3 字段即可。
/// longname 缺省或格式非标准时类型位为空串、属主属组回退数字 uid/gid
/// （与 FileZilla 等客户端一致）。
fn owner_of(longname: &str, attrs: &FileAttributes) -> (String, String, String) {
    let (kind, user, group) = match parse_longname_head(longname) {
        Some(v) => v,
        None => (
            String::new(),
            attrs.uid.map(|u| u.to_string()).unwrap_or_default(),
            attrs.gid.map(|g| g.to_string()).unwrap_or_default(),
        ),
    };
    (kind, user, group)
}

/// 校验 longname 首字段为合法 `ls -l` 类型+权限串后，取出 类型位/owner/group。
fn parse_longname_head(longname: &str) -> Option<(String, String, String)> {
    let mut it = longname.split_whitespace();
    let kind_perm = it.next()?;
    let mut chars = kind_perm.bytes();
    let kind = chars.next()?;
    // 类型 1 字符 + 权限 9 字符（ACL 时可能多出 '+'/'.'），至少 10
    if kind_perm.len() < 10 || !matches!(kind, b'-' | b'd' | b'l' | b'c' | b'b' | b'p' | b's') {
        return None;
    }
    let _nlink = it.next()?;
    let user = it.next()?;
    let group = it.next()?;
    Some((
        (kind as char).to_string(),
        user.to_string(),
        group.to_string(),
    ))
}

impl SshSession {
    /// 取（或首用时开）SFTP 子系统通道——所有远端文件操作的唯一入口。
    ///
    /// 外部连接的通道门禁落在这里：`ssh://` 链接的会话只有 PTY 一条通道，
    /// 这里拒绝任何 SFTP 请求，避免在堡垒机上开出第二条通道。
    pub(super) async fn sftp_channel(&self) -> Result<Arc<RawSftpSession>, SshError> {
        if !self.sftp_allowed() {
            return Err(SshError::Channel(
                "EXTERNAL_SINGLE_CHANNEL: 外部连接只允许一条通道，文件管理请用 sftp:// 链接另开会话"
                    .to_string(),
            ));
        }
        self.sftp.get(&self.handle).await
    }

    /// 列举远端目录。`path` 为 None/空串时优先用 Shell CWD（由 OSC 6667 钩子上报），
    /// 仍无则用登录默认目录（realpath "."），保持 Shell 与 SFTP 目录一致。
    /// SFTP 通道失效时自动重建并重试一次，避免偶发断流后面板永久不可用。
    ///
    /// 仅 e2e 集成测试直接可达（doc(hidden) 防误用）：它是最轻的 SFTP 通道门禁探针，
    /// 生产调用走 `ipc::sftp_list_dir`。
    #[doc(hidden)]
    pub async fn sftp_list_dir(
        &self,
        path: Option<String>,
    ) -> Result<RemoteDirListing, SshError> {
        let wanted = match path.filter(|p| !p.is_empty()) {
            p @ Some(_) => p,
            None => self.get_cwd().await,
        };
        match self.list_remote(&wanted).await {
            Ok(res) => Ok(res),
            Err(first) => {
                debug!("SFTP 列目录失败，重建通道后重试一次: {first}");
                self.sftp.invalidate().await;
                self.list_remote(&wanted).await
            }
        }
    }

    async fn list_remote(&self, path: &Option<String>) -> Result<RemoteDirListing, SshError> {
        let sftp = self.sftp_channel().await?;
        let fut = async {
            let dir = match path {
                Some(p) => p.clone(),
                None => {
                    let name = sftp
                        .realpath(".")
                        .await
                        .map_err(|e| SshError::Channel(format!("解析远端默认目录失败: {e}")))?;
                    name.files
                        .first()
                        .map(|f| f.filename.clone())
                        .ok_or_else(|| {
                            SshError::Channel("解析远端默认目录失败: 服务端未返回路径".to_string())
                        })?
                }
            };

            // 自行收发 OPENDIR/READDIR/CLOSE：高层 read_dir 会丢掉 NAME 包里
            // 携带属主属组名的 longname。EOF 以 Status(EOF) 形式作为 Err 返回。
            let handle = sftp
                .opendir(&dir)
                .await
                .map_err(|e| SshError::Channel(format!("打开远端目录 {dir} 失败: {e}")))?
                .handle;
            let mut files = Vec::new();
            let read_result = loop {
                match sftp.readdir(handle.as_str()).await {
                    Ok(name) => files.extend(name.files),
                    Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => {
                        break Ok::<_, SshError>(());
                    }
                    Err(e) => {
                        break Err(SshError::Channel(format!("读取远端目录 {dir} 失败: {e}")));
                    }
                }
            };
            // 句柄尽量关闭；关闭失败不影响已读到的数据
            let _ = sftp.close(handle.as_str()).await;
            read_result?;

            let mut entries = Vec::with_capacity(files.len());
            for f in files {
                if f.filename == "." || f.filename == ".." {
                    continue;
                }
                let attrs = f.attrs;
                let is_symlink = attrs.file_type().is_symlink();
                // 符号链接需跟随目标判断类型：指向目录的软链按目录处理，
                // 指向文件的按文件处理；stat 失败（坏链接/权限不足）回退为非目录。
                let is_dir = if is_symlink {
                    let entry_path = format!("{dir}/{}", f.filename);
                    sftp.stat(&entry_path)
                        .await
                        .map(|a| a.attrs.file_type().is_dir())
                        .unwrap_or(false)
                } else {
                    attrs.file_type().is_dir()
                };
                let (kind, owner_user, owner_group) = owner_of(&f.longname, &attrs);
                entries.push(RemoteFsEntry {
                    is_dir,
                    is_symlink,
                    size: attrs.size.unwrap_or(0),
                    mtime: fmt_mtime(attrs.mtime),
                    perm: attrs.permissions.map(fmt_perm).unwrap_or_default(),
                    owner_user,
                    owner_group,
                    kind,
                    name: f.filename,
                });
            }
            Ok::<_, SshError>(RemoteDirListing { path: dir, entries })
        };
        // RawSftpSession 自带单请求 20s 超时，这里再给整个列目录一个总时限，
        // 慢主机/异常网络下前端能收到明确失败而非无限转圈
        tokio::time::timeout(Duration::from_secs(20), fut)
            .await
            .map_err(|_| SshError::Channel("SFTP 操作超时（20 秒）".to_string()))?
    }

    /// 在远端创建目录（单层，不递归；父目录不存在时报错）。
    pub(crate) async fn sftp_mkdir(&self, path: String) -> Result<(), SshError> {
        let sftp = self.sftp_channel().await?;
        sftp.mkdir(&path, FileAttributes::default())
            .await
            .map_err(|e| SshError::Channel(format!("创建目录 {path} 失败: {e}")))?;
        debug!("SFTP 创建目录: {path}");
        Ok(())
    }

    /// 批量创建远端目录（mkdir -p 语义，已存在视为成功，各级父目录自动补齐）。
    /// 拖拽上传整棵目录树时前端把本批目标目录一次传入：300 个文件的文件夹
    /// 只需一轮 stat/mkdir，而不是每个文件各自重复推导、逐条往返。
    pub(crate) async fn sftp_mkdir_all(&self, paths: Vec<String>) -> Result<(), SshError> {
        let dirs = dir_tree_of(&paths);
        if dirs.is_empty() {
            return Ok(());
        }
        let sftp = self.sftp_channel().await?;
        // 本轮真的新建了哪些目录：其子目录必然也不存在，可跳过 stat 直接 mkdir。
        // 只记「新建」而不记「已确认存在」——后者之下仍可能有已存在的同名子目录，
        // 少探测会误报「目录已存在」。
        let mut created: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for dir in &dirs {
            let parent_new = match dir.rsplit_once('/') {
                // 顶层（父为空串或根）落在既有工作目录里，存在性未知，需探测
                Some((p, _)) => !p.is_empty() && p != "/" && created.contains(p),
                None => false,
            };
            if mkdir_one(&sftp, dir, !parent_new).await? {
                created.insert(dir.as_str());
            }
        }
        debug!("SFTP 批量创建目录 {} 个（新建 {} 个）", dirs.len(), created.len());
        Ok(())
    }

    /// 远端路径元数据。路径不存在返回 `exists=false`（不报错），其余 IO 失败照实报错。
    pub(crate) async fn sftp_stat(&self, path: String) -> Result<RemoteStat, SshError> {
        let sftp = self.sftp_channel().await?;
        match sftp.stat(&path).await {
            Ok(a) => Ok(RemoteStat {
                exists: true,
                is_dir: a.attrs.file_type().is_dir(),
                size: a.attrs.size.unwrap_or(0),
                mtime_ms: a.attrs.mtime.map(|m| m as i64 * 1000).unwrap_or(0),
            }),
            Err(SftpError::Status(s)) if s.status_code == StatusCode::NoSuchFile => {
                Ok(RemoteStat::default())
            }
            Err(e) => Err(SshError::Channel(format!("获取远端 {path} 属性失败: {e}"))),
        }
    }

    /// 递归枚举远端目录树（下载方向的 `local_walk` 镜像）。
    /// 显式栈遍历 + 深度/文件数上限；子孙中的软链与非常规文件（设备/管道/套接字）
    /// 一律跳过——跟随软链可能成环，特殊文件读取会永久挂住通道。
    /// 根本身是软链时跟随（用户明确点了它），与普通列目录的口径一致。
    ///
    /// 仅 e2e 集成测试直接可达（doc(hidden) 防误用）：生产调用走 `ipc::sftp_walk`。
    #[doc(hidden)]
    pub async fn sftp_walk(
        &self,
        root: String,
        max_files: Option<usize>,
    ) -> Result<RemoteWalkResult, SshError> {
        let cap = max_files.unwrap_or(5000).clamp(1, 20000);
        let sftp = self.sftp_channel().await?;
        let name = base_name_of(&root);
        let attrs = sftp
            .stat(&root)
            .await
            .map_err(|e| SshError::Channel(format!("获取远端 {root} 属性失败: {e}")))?
            .attrs;
        let ftype = attrs.file_type();
        if !ftype.is_dir() {
            if !ftype.is_file() {
                return Err(SshError::Channel(format!(
                    "{name} 不是普通文件（链接或特殊文件不支持下载）"
                )));
            }
            return Ok(RemoteWalkResult {
                name: name.clone(),
                is_dir: false,
                files: vec![RemoteWalkFile {
                    rel: name,
                    abs: root.clone(),
                    size: attrs.size.unwrap_or(0),
                }],
                dirs: Vec::new(),
                root,
            });
        }
        let mut files: Vec<RemoteWalkFile> = Vec::new();
        let mut dirs: Vec<String> = Vec::new();
        let mut stack: Vec<(String, String, u32)> = vec![(root.clone(), String::new(), 0)];
        while let Some((dir, prefix, depth)) = stack.pop() {
            if depth > WALK_MAX_DEPTH {
                return Err(SshError::Channel(format!(
                    "目录层级超过 {WALK_MAX_DEPTH} 层：{dir}"
                )));
            }
            for (child_name, child_attrs) in list_children(&sftp, &dir).await? {
                let cft = child_attrs.file_type();
                if cft.is_symlink() {
                    continue;
                }
                let rel = if prefix.is_empty() {
                    child_name.clone()
                } else {
                    format!("{prefix}/{child_name}")
                };
                // dir 已是绝对路径，只能接子项名：接 rel 会把前缀重复一遍（a → a/a/b）
                let abs = join_child(&dir, &child_name);
                if cft.is_dir() {
                    dirs.push(rel.clone());
                    stack.push((abs, rel, depth + 1));
                    continue;
                }
                if !cft.is_file() {
                    continue;
                }
                if files.len() >= cap {
                    return Err(SshError::Channel(format!(
                        "目录树文件数超过上限 {cap}：{root}"
                    )));
                }
                files.push(RemoteWalkFile {
                    abs,
                    rel,
                    size: child_attrs.size.unwrap_or(0),
                });
            }
        }
        debug!(
            "SFTP 枚举目录树 {root}：{} 个文件、{} 个子目录",
            files.len(),
            dirs.len()
        );
        Ok(RemoteWalkResult {
            name,
            is_dir: true,
            files,
            dirs,
            root,
        })
    }

    /// 编辑器集成：把远端文件原样取回本地副本。不走传输队列，因此没有覆盖策略、
    /// 断点续传、哈希校验与限速——它只是「取一份可编辑的副本」，目标恒新建覆盖。
    /// 本地父目录自动创建；目录、超过 [`EDIT_FETCH_MAX`] 的大文件一律拒绝。
    /// 返回落盘字节数。
    pub(crate) async fn sftp_fetch(
        &self,
        remote_path: String,
        local_path: String,
    ) -> Result<u64, SshError> {
        let sftp = self.sftp_channel().await?;
        let local = expand_tilde(&local_path);
        let attrs = sftp
            .stat(&remote_path)
            .await
            .map_err(|e| SshError::Channel(format!("获取远端文件 {remote_path} 属性失败: {e}")))?;
        if attrs.attrs.file_type().is_dir() {
            return Err(SshError::Channel(format!("{remote_path} 是目录，不能直接编辑")));
        }
        if let Some(sz) = attrs.attrs.size {
            if sz > EDIT_FETCH_MAX {
                return Err(SshError::Channel(format!(
                    "{remote_path} 有 {sz} 字节，超过编辑器取回上限 {EDIT_FETCH_MAX} 字节（请改用下载）"
                )));
            }
        }
        if let Some(parent) = std::path::Path::new(&local).parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| SshError::Channel(format!("创建本地目录失败: {e}")))?;
            }
        }
        let mut dst = File::create(&local)
            .await
            .map_err(|e| SshError::Channel(format!("创建本地文件 {local} 失败: {e}")))?;
        let rhandle = sftp
            .open(&remote_path, OpenFlags::READ, FileAttributes::default())
            .await
            .map_err(|e| SshError::Channel(format!("打开远端文件 {remote_path} 失败: {e}")))?
            .handle;
        let mut offset = 0u64;
        let result: Result<(), SshError> = loop {
            match sftp
                .read(rhandle.as_str(), offset, DEFAULT_CHUNK as u32)
                .await
            {
                Ok(d) if d.data.is_empty() => break Ok(()),
                Ok(d) => {
                    let n = d.data.len() as u64;
                    if let Err(e) = dst.write_all(&d.data).await {
                        break Err(SshError::Channel(format!("写入本地副本失败: {e}")));
                    }
                    offset += n;
                }
                Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => break Ok(()),
                Err(e) => {
                    let detail =
                        read_fail_detail(&e, &attrs.attrs, offset, DEFAULT_CHUNK as u32);
                    log::error!("SFTP 取回编辑副本读流失败 {remote_path}: {detail}");
                    break Err(SshError::Channel(format!(
                        "读取远端失败: {detail}（{remote_path}）"
                    )));
                }
            }
        };
        let _ = sftp.close(rhandle.as_str()).await;
        result?;
        debug!("SFTP 取回编辑副本: {remote_path} → {local}（{offset} 字节）");
        Ok(offset)
    }

    /// 单文件上传：本地 → 远端。分块大小由 `chunk_kb` 指定（缺省 64KB），进度经
    /// IPC [`Channel`] 流式推送，支持暂停/取消。**断点续传**（`resume` 开启时）：
    /// 若写入目标已存在且小于本地，先按 `resume_check` 校验半截文件完整性再从
    /// 已有偏移续写（不截断）；远端最终路径已完整则直接完成；远端大于本地
    /// （源文件变更）时截断重传。`resume` 关闭时一律全量重传。
    /// 目标已存在且非续传候选（含校验失败）时按 `overwrite_policy` 裁决：
    /// skip → [`SshError::SftpSkipped`]；ask 无裁决 → [`SshError::SftpConflict`]
    /// （前端确认后带 `resolve` 重投）。`upload_temp` 开启时字节先写 `.rhost-tmp`、
    /// 完成后 posix-rename 原子落位，且该模式的断点只采信「最终路径不存在」的半截
    /// （否则残留 temp 可能来自另一份同名目标文件）；`verify_hash`/`preserve_meta` 见 [`TransferPolicy`]。
    pub(crate) async fn upload_file(
        &self,
        task_id: u64,
        local_path: String,
        remote_path: String,
        chunk_kb: Option<u32>,
        resume: Option<bool>,
        max_bytes: Option<u64>,
        policy: Option<TransferPolicy>,
        channel: Channel<TransferProgress>,
    ) -> Result<(), SshError> {
        let chunk = chunk_kb
            .unwrap_or((DEFAULT_CHUNK / 1024) as u32)
            .clamp(8, 1024) as usize
            * 1024;
        let resume = resume.unwrap_or(true);
        let pol = policy.unwrap_or_default();
        // SFTP 会话取得前置：传输后收尾（校验/重命名/保留元数据）与失败清理共用同一会话
        let sftp = self.sftp_channel().await?;
        let local = expand_tilde(&local_path);
        let temp = temp_path_of(&remote_path);
        // 字节实际写入处：temp 模式下最终路径在 rename 前分毫不动，半途失败/取消不产生损坏
        let write_path = if pol.upload_temp {
            temp.clone()
        } else {
            remote_path.clone()
        };
        let cancel = self.child_cancel();
        let (paused_tx, mut paused_rx) = watch::channel(false);
        self.sftp
            .register_task(
                task_id,
                Arc::new(TransferCtl {
                    cancel: cancel.clone(),
                    paused: paused_tx,
                }),
            )
            .await;

        // async block 返回 Result<(已传, 总大小, 本次是否写过), (错误, 已传, 总大小)>，
        // 这样内部可用 ?，且错误路径也能拿到已传字节用于收尾进度；
        // 「没写过」= 远端已完整的提前完成路径，收尾时不得 rename 不存在的 temp
        let outcome: Result<(u64, u64, bool), (SshError, u64, u64)> = async {
            let mut src = File::open(&local).await.map_err(|e| {
                (
                    SshError::Channel(format!("打开本地文件 {local} 失败: {e}")),
                    0,
                    0,
                )
            })?;
            let meta = src
                .metadata()
                .await
                .map_err(|e| {
                    (
                        SshError::Channel(format!("读取本地文件大小失败: {e}")),
                        0,
                        0,
                    )
                })?;
            let total = meta.len();
            use std::os::unix::fs::MetadataExt;
            let src_mtime = Some(meta.mtime().max(0));

            // 体积上限预检：在打开远端文件写入前拒绝，避免生成残留的远端空/截断文件
            if let Some(max) = max_bytes {
                if total > max {
                    return Err((
                        SshError::TransferRejected(format!(
                            "文件 {:.1}MB 超过 AI 传输上限（64MB），请通过 SFTP 文件面板手动传输，不要使用 scp/rsync 等命令行传输",
                            total as f64 / 1024.0 / 1024.0
                        )),
                        0u64,
                        total,
                    ));
                }
            }

            // 探测最终路径（覆盖策略裁决基准）；不存在视为 size 0
            let mut dst_exists = false;
            let mut dst_size = 0u64;
            let mut dst_mtime: Option<i64> = None;
            match sftp.lstat(&remote_path).await {
                Ok(a) => {
                    dst_exists = true;
                    dst_size = a.attrs.size.unwrap_or(0);
                    dst_mtime = a.attrs.mtime.map(i64::from);
                }
                Err(SftpError::Status(s)) if s.status_code == StatusCode::NoSuchFile => {}
                Err(e) => {
                    return Err((
                        SshError::Channel(format!("探测远端文件 {remote_path} 失败: {e}")),
                        0,
                        total,
                    ));
                }
            }
            // 续传半截所在：temp 模式在 temp 文件（最终路径是上一版完整文件，不可动），
            // 否则即最终路径本身（既有行为）
            let (partial_size, partial_mtime) = if pol.upload_temp {
                if dst_exists {
                    // 最终路径已有文件时，temp 半截不可作为断点：它可能来自另一份
                    // 同名目标的上次失败传输，从其偏移续写会绕过覆盖策略裁决，并把
                    // 旧字节混进新内容后 rename 落位（静默损坏）。此侧重传（TRUNCATE
                    // temp），由 decide_conflict 决定覆盖还是跳过。
                    (0u64, None)
                } else {
                    match sftp.lstat(&temp).await {
                        Ok(a) => (a.attrs.size.unwrap_or(0), a.attrs.mtime.map(i64::from)),
                        Err(_) => (0u64, None),
                    }
                }
            } else {
                (dst_size, dst_mtime)
            };

            // 既有快路径保留：续传开启且最终路径与本地等大 → 视为已完成
            if resume && dst_exists && dst_size == total {
                return Ok((total, total, false));
            }

            // 偏移决策：temp 模式下 partial == total 是「上次写满未 rename」的残留，
            // 按零字节续传进入 rename，正好补完上次被取消的运行
            let mut offset = 0u64;
            if resume && partial_size > 0 && partial_size <= total {
                let fresh = partial_is_fresh(partial_mtime, now_unix());
                let hash_equal = if pol.resume_check == "sha256" {
                    match (
                        local_sha256_prefix(&local, partial_size).await,
                        remote_sha256_prefix(&sftp, &write_path, partial_size).await,
                    ) {
                        (Ok(a), Ok(b)) => a == b,
                        // 任一侧哈希都取不到即无法证明半截可信：从严整文件重传
                        _ => false,
                    }
                } else {
                    false
                };
                if resume_check_ok(&pol.resume_check, fresh, hash_equal) {
                    offset = partial_size;
                }
            }

            // 覆盖策略：目标已存在且非续传候选（含断点校验失败降级重传的情形）
            if offset == 0 && dst_exists {
                match decide_conflict(
                    &pol.overwrite_policy,
                    pol.resolve.as_deref(),
                    total,
                    dst_size,
                    src_mtime,
                    dst_mtime,
                ) {
                    Conflict::Skip => {
                        return Err((SshError::SftpSkipped(remote_path.clone()), 0, total));
                    }
                    Conflict::Ask => {
                        return Err((
                            SshError::SftpConflict(format!("{remote_path}|{dst_size}|{total}")),
                            0,
                            total,
                        ));
                    }
                    Conflict::Proceed => {}
                }
            }
            if offset > 0 {
                src.seek(SeekFrom::Start(offset)).await.map_err(|e| {
                    (
                        SshError::Channel(format!("定位本地文件偏移失败: {e}")),
                        0,
                        total,
                    )
                })?;
            }

            // 续传时不带 TRUNCATE 保留已有内容；从头传时 TRUNCATE 清除残留
            let flags = if offset > 0 {
                OpenFlags::WRITE | OpenFlags::CREATE
            } else {
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE
            };
            let rhandle = sftp
                .open(&write_path, flags, FileAttributes::default())
                .await
                .map_err(|e| {
                    (
                        SshError::Channel(format!("打开远端文件 {write_path} 失败: {e}")),
                        0,
                        total,
                    )
                })?
                .handle;

            let mut buf = vec![0u8; chunk];
            let mut offset = offset;
            let mut transferred: u64 = offset; // 进度含已续传字节
            let mut last_emit = Instant::now();
            let mut last_bytes = offset;
            // 限速桶：全局桶按 (主机:端口, 用户) 跨标签共享，任务桶只归本任务
            let throttle = Throttle::from_policy(
                &pol,
                &self.sftp.rate_key.0,
                &self.sftp.rate_key.1,
            );
            // 续传起始即上报一次，前端进度条直接跳到断点位置
            if offset > 0 {
                emit_progress(&channel, task_id, transferred, total, 0, false);
            }

            loop {
                wait_if_paused(&mut paused_rx, &cancel)
                    .await
                    .map_err(|e| (e, transferred, total))?;
                if cancel.is_cancelled() {
                    return Err((SshError::Cancelled, transferred, total));
                }
                let n = src.read(&mut buf).await.map_err(|e| {
                    (
                        SshError::Channel(format!("读取本地文件失败: {e}")),
                        transferred,
                        total,
                    )
                })?;
                if n == 0 {
                    return Ok((transferred, total, true));
                }
                sftp.write(rhandle.as_str(), offset, buf[..n].to_vec())
                    .await
                    .map_err(|e| {
                        (
                            SshError::Channel(format!("写入远端失败: {e}")),
                            transferred,
                            total,
                        )
                    })?;
                offset += n as u64;
                transferred += n as u64;
                throttle.acquire(n as u64).await;

                let now = Instant::now();
                if now.duration_since(last_emit) >= PROGRESS_INTERVAL {
                    let speed = ((transferred - last_bytes) * 1000)
                        / now.duration_since(last_emit).as_millis().max(1) as u64;
                    emit_progress(&channel, task_id, transferred, total, speed, false);
                    last_emit = now;
                    last_bytes = transferred;
                }
            }
        }
        .await;

        let (result, transferred, total) = match outcome {
            Ok((t, total, wrote)) => {
                // 收尾（校验/落位/元数据）失败也是任务失败；进度按已传字节记满
                let r = self
                    .upload_finalize(
                        &sftp, &pol, &local, &remote_path, &temp, &write_path, wrote, total,
                    )
                    .await;
                (r, t, total)
            }
            Err((e, t, total)) => (Err(e), t, total),
        };

        // 失败/取消后清理：临时文件模式且关闭续传时，半截文件已无续传价值、只会
        // 污染远端目录，best-effort 删除（清理失败忽略）；开启续传则保留作下次断点——
        // 最终路径在 rename 前从未被触碰，upload_temp 的「取消不留损坏」语义不受影响。
        if result.is_err() && pol.upload_temp && !resume {
            let _ = sftp.remove(&temp).await;
        }

        self.sftp.remove_task(task_id).await;
        emit_progress(&channel, task_id, transferred, total, 0, true);
        result
    }

    /// 上传收尾（字节传完后执行）：完整性校验 → 保留元数据 → 临时文件原子落位。
    /// 校验/重命名失败是任务级错误；SETSTAT 失败仅告警——服务器普遍限制非属主
    /// 设置 mtime/mode，元数据尽力而为，不能反噬已完成的传输。
    /// `wrote` 为 false 表示「远端已完整的提前完成」：本次没写过 temp，不得 rename。
    /// `total` 为 0 时跳过校验：空文件两侧哈希必然相同（都是 SHA256 空摘要），
    /// 校验无信息量，白付一次全量读取代价。
    async fn upload_finalize(
        &self,
        sftp: &RawSftpSession,
        pol: &TransferPolicy,
        local: &str,
        remote_path: &str,
        temp: &str,
        write_path: &str,
        wrote: bool,
        total: u64,
    ) -> Result<(), SshError> {
        use std::os::unix::fs::MetadataExt;
        // 校验对象 = 本次实际写入的文件；提前完成路径写入未发生，校验最终路径
        let verify_target = if wrote { write_path } else { remote_path };
        if verify_required(pol) && total > 0 {
            let a = local_sha256_prefix(local, u64::MAX).await?;
            let b = remote_sha256_prefix(sftp, verify_target, u64::MAX).await?;
            if a != b {
                crate::applog::emit(
                    log::Level::Warn,
                    "sftp",
                    crate::applog::events::SFTP_TRANSFER_VERIFY_FAILED,
                    Some(&self.sftp.sid),
                    "上传完整性校验失败",
                    Some(serde_json::json!({
                        "path": remote_path, "localSha256": a, "remoteSha256": b
                    })),
                );
                return Err(SshError::SftpVerifyFailed(format!(
                    "SHA256 两侧不一致: 本地 {a} ≠ 远端 {b}"
                )));
            }
        }
        if pol.preserve_meta {
            if let Ok(meta) = tokio::fs::metadata(local).await {
                let mtime = meta.mtime().max(0) as u32;
                let attrs = FileAttributes {
                    mtime: Some(mtime),
                    atime: Some(mtime),
                    permissions: Some(meta.mode() & 0o7777),
                    ..Default::default()
                };
                // 属性写在实际落盘的那份文件上：temp 模式必须在 rename **之前** setstat
                // temp——rename 只换目录项，inode 属性随文件一起到位；若留到 rename 之后，
                // 最终路径会短暂以「当前时间 + 默认权限」暴露给用户
                if let Err(e) = sftp.setstat(verify_target, attrs).await {
                    log::warn!("SFTP 保留元数据 setstat 失败（不影响传输结果）{verify_target}: {e}");
                }
            }
        }
        if pol.upload_temp {
            if wrote {
                // 优先 posix-rename（原子覆盖目标）；服务端不支持扩展时降级
                // remove+rename（非严格原子，但 temp 已完整、窗口毫秒级，可接受）
                if posix_rename(sftp, temp, remote_path).await.is_err() {
                    let _ = sftp.remove(remote_path).await;
                    sftp.rename(temp, remote_path).await.map_err(|e| {
                        SshError::Channel(format!("临时文件落位为 {remote_path} 失败: {e}"))
                    })?;
                }
            } else {
                // 目标已完整、本次未写入：顺手清掉无主的旧 temp（尽力而为）
                let _ = sftp.remove(temp).await;
            }
        }
        Ok(())
    }

    /// 单文件下载：远端 → 本地。本地父目录不存在时自动创建；分块大小由
    /// `chunk_kb` 指定（缺省 64KB），进度经 IPC [`Channel`] 流式推送，支持暂停/取消。
    /// **断点续传**（`resume` 开启时）：写入目标已存在且小于远端，先按
    /// `resume_check` 校验半截文件再从已有偏移续写（APPEND）；本地已完整则直接
    /// 完成；本地大于远端（源文件变更）时截断重传。`resume` 关闭时一律全量重传。
    /// 与上传共用 [`TransferPolicy`]：覆盖策略针对**本地**最终路径裁决
    /// （skip → SftpSkipped；ask 无裁决 → SftpConflict），`preserve_meta` 把远端
    /// mtime/权限写到本地。`upload_temp` 对下载**无效**：本地半截文件即事实上的临时
    /// 文件（断点与覆盖策略都按它裁决），rename 落位会换掉用户可能正打开的本地文件。
    pub(crate) async fn download_file(
        &self,
        task_id: u64,
        remote_path: String,
        local_path: String,
        chunk_kb: Option<u32>,
        resume: Option<bool>,
        max_bytes: Option<u64>,
        policy: Option<TransferPolicy>,
        channel: Channel<TransferProgress>,
    ) -> Result<(), SshError> {
        let chunk = chunk_kb
            .unwrap_or((DEFAULT_CHUNK / 1024) as u32)
            .clamp(8, 1024) as usize
            * 1024;
        let resume = resume.unwrap_or(true);
        let pol = policy.unwrap_or_default();
        let sftp = self.sftp_channel().await?;
        let local = expand_tilde(&local_path);
        // 下载恒写最终路径（upload_temp 只作用于上传，见 TransferPolicy 字段注释）
        let write_path = local.clone();
        let cancel = self.child_cancel();
        let (paused_tx, mut paused_rx) = watch::channel(false);
        self.sftp
            .register_task(
                task_id,
                Arc::new(TransferCtl {
                    cancel: cancel.clone(),
                    paused: paused_tx,
                }),
            )
            .await;

        let outcome: Result<(u64, u64, bool), (SshError, u64, u64)> = async {
            if let Some(parent) = std::path::Path::new(&local).parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
            }

            // 远端源属性：大小 + mtime/权限（newer 裁决与 preserve_meta 的源头）
            let attrs = sftp
                .lstat(&remote_path)
                .await
                .map_err(|e| {
                    (
                        SshError::Channel(format!("获取远端文件 {remote_path} 属性失败: {e}")),
                        0,
                        0,
                    )
                })?
                .attrs;
            let total = attrs.size.unwrap_or(0);
            let src_mtime = attrs.mtime.map(i64::from);

            // 体积上限预检：在创建本地文件前拒绝，避免生成残留的本地空/截断文件
            if let Some(max) = max_bytes {
                if total > max {
                    return Err((
                        SshError::TransferRejected(format!(
                            "文件 {:.1}MB 超过 AI 传输上限（64MB），请通过 SFTP 文件面板手动传输，不要使用 scp/rsync 等命令行传输",
                            total as f64 / 1024.0 / 1024.0
                        )),
                        0u64,
                        total,
                    ));
                }
            }

            // 探测本地最终路径：既是覆盖策略裁决基准，也是断点所在
            // （下载恒写最终路径，不走 temp——见 TransferPolicy::upload_temp 注释）
            let dst_meta = tokio::fs::metadata(&local).await;
            let dst_exists = dst_meta.is_ok();
            let (dst_size, dst_mtime) = match &dst_meta {
                Ok(m) => (m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64)),
                Err(_) => (0u64, None),
            };
            let (partial_size, partial_mtime) = (dst_size, dst_mtime);

            // 既有快路径保留：续传开启且本地与远端等大 → 视为已完成
            if resume && dst_exists && dst_size == total {
                return Ok((total, total, false));
            }

            // 偏移决策 + 断点校验（temp 模式 partial == total = 上次写满未 rename，零字节续传补落位）
            let mut offset = 0u64;
            if resume && partial_size > 0 && partial_size <= total {
                let fresh = partial_is_fresh(partial_mtime, now_unix());
                let hash_equal = if pol.resume_check == "sha256" {
                    match (
                        remote_sha256_prefix(&sftp, &remote_path, partial_size).await,
                        local_sha256_prefix(&write_path, partial_size).await,
                    ) {
                        (Ok(a), Ok(b)) => a == b,
                        _ => false,
                    }
                } else {
                    false
                };
                if resume_check_ok(&pol.resume_check, fresh, hash_equal) {
                    offset = partial_size;
                }
            }

            // 覆盖策略：本地最终路径已存在且非续传候选
            if offset == 0 && dst_exists {
                match decide_conflict(
                    &pol.overwrite_policy,
                    pol.resolve.as_deref(),
                    total,
                    dst_size,
                    src_mtime,
                    dst_mtime,
                ) {
                    Conflict::Skip => {
                        return Err((SshError::SftpSkipped(local.clone()), 0, total));
                    }
                    Conflict::Ask => {
                        return Err((
                            SshError::SftpConflict(format!("{local}|{total}|{dst_size}")),
                            0,
                            total,
                        ));
                    }
                    Conflict::Proceed => {}
                }
            }
            // 续传用 APPEND 保留已有内容；从头传用 CREATE 截断
            let mut dst = if offset > 0 {
                OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&write_path)
                    .await
                    .map_err(|e| {
                        (
                            SshError::Channel(format!("打开本地文件 {write_path} 续写失败: {e}")),
                            0,
                            total,
                        )
                    })?
            } else {
                File::create(&write_path).await.map_err(|e| {
                    (
                        SshError::Channel(format!("创建本地文件 {write_path} 失败: {e}")),
                        0,
                        total,
                    )
                })?
            };

            let rhandle = sftp
                .open(&remote_path, OpenFlags::READ, FileAttributes::default())
                .await
                .map_err(|e| {
                    (
                        SshError::Channel(format!("打开远端文件 {remote_path} 失败: {e}")),
                        0,
                        total,
                    )
                })?
                .handle;

            let mut offset = offset;
            let mut transferred: u64 = offset; // 进度含已续传字节
            let mut last_emit = Instant::now();
            let mut last_bytes = offset;
            // 限速桶：与上传同一套全局/任务两道上限（见 [`Throttle`]）
            let throttle = Throttle::from_policy(
                &pol,
                &self.sftp.rate_key.0,
                &self.sftp.rate_key.1,
            );
            // 续传起始即上报一次，前端进度条直接跳到断点位置
            if offset > 0 {
                emit_progress(&channel, task_id, transferred, total, 0, false);
            }

            loop {
                wait_if_paused(&mut paused_rx, &cancel)
                    .await
                    .map_err(|e| (e, transferred, total))?;
                if cancel.is_cancelled() {
                    return Err((SshError::Cancelled, transferred, total));
                }
                // russh-sftp 的 read 在 EOF 时返回 Status(Eof) 错误，而非空数据
                let read_res = sftp.read(rhandle.as_str(), offset, chunk as u32).await;
                let data = match read_res {
                    Ok(d) => d.data,
                    Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => {
                        return Ok((transferred, total, true));
                    }
                    Err(e) => {
                        let detail = read_fail_detail(&e, &attrs, offset, chunk as u32);
                        log::error!("SFTP 下载读流失败 {remote_path}: {detail}");
                        return Err((
                            SshError::Channel(format!("读取远端失败: {detail}（{remote_path}）")),
                            transferred,
                            total,
                        ));
                    }
                };
                if data.is_empty() {
                    return Ok((transferred, total, true));
                }
                dst.write_all(&data).await.map_err(|e| {
                    (
                        SshError::Channel(format!("写入本地文件失败: {e}")),
                        transferred,
                        total,
                    )
                })?;
                offset += data.len() as u64;
                transferred += data.len() as u64;
                throttle.acquire(data.len() as u64).await;

                let now = Instant::now();
                if now.duration_since(last_emit) >= PROGRESS_INTERVAL {
                    let speed = ((transferred - last_bytes) * 1000)
                        / now.duration_since(last_emit).as_millis().max(1) as u64;
                    emit_progress(&channel, task_id, transferred, total, speed, false);
                    last_emit = now;
                    last_bytes = transferred;
                }
            }
        }
        .await;

        let (result, transferred, total) = match outcome {
            Ok((t, total, _)) => {
                let r = self
                    .download_finalize(&sftp, &pol, &remote_path, &local, total)
                    .await;
                (r, t, total)
            }
            Err((e, t, total)) => (Err(e), t, total),
        };

        // 下载失败/取消后**不删**本地半途文件：它就是下次续传的断点（既有语义），
        // 且从未有 temp→最终的 rename 步骤需要清理。

        self.sftp.remove_task(task_id).await;
        emit_progress(&channel, task_id, transferred, total, 0, true);
        result
    }

    /// 下载收尾：完整性校验（本地文件 vs 远端源）→ 把远端 mtime/权限写到本地。
    /// 校验失败是任务级错误；本地元数据设置失败仅告警。
    /// `total` 为 0 时跳过校验（空文件哈希无信息量，见 [`upload_finalize`]）。
    /// 下载不走临时文件，故此处无 rename 落位步骤（见 [`TransferPolicy::upload_temp`]）。
    async fn download_finalize(
        &self,
        sftp: &RawSftpSession,
        pol: &TransferPolicy,
        remote_path: &str,
        local: &str,
        total: u64,
    ) -> Result<(), SshError> {
        use std::os::unix::fs::PermissionsExt;
        if verify_required(pol) && total > 0 {
            let a = remote_sha256_prefix(sftp, remote_path, u64::MAX).await?;
            let b = local_sha256_prefix(local, u64::MAX).await?;
            if a != b {
                crate::applog::emit(
                    log::Level::Warn,
                    "sftp",
                    crate::applog::events::SFTP_TRANSFER_VERIFY_FAILED,
                    Some(&self.sftp.sid),
                    "下载完整性校验失败",
                    Some(serde_json::json!({
                        "path": local, "remoteSha256": a, "localSha256": b
                    })),
                );
                return Err(SshError::SftpVerifyFailed(format!(
                    "SHA256 两侧不一致: 远端 {a} ≠ 本地 {b}"
                )));
            }
        }
        if pol.preserve_meta {
            // 远端属性重取一次（lstat 便宜）；个别服务器不允许客户端写回自身 mtime，
            // 元数据尽力而为，失败不反噬已完成的传输
            if let Ok(st) = sftp.stat(remote_path).await {
                let a = st.attrs;
                if let Some(m) = a.mtime {
                    let t = std::time::UNIX_EPOCH + Duration::from_secs(u64::from(m));
                    let times = std::fs::FileTimes::new().set_modified(t).set_accessed(t);
                    match std::fs::OpenOptions::new().write(true).open(local) {
                        Ok(f) => {
                            if let Err(e) = f.set_times(times) {
                                log::warn!("SFTP 保留元数据写本地 mtime 失败（不影响传输结果）{local}: {e}");
                            }
                        }
                        Err(e) => log::warn!("SFTP 保留元数据打开本地文件失败（不影响传输结果）{local}: {e}"),
                    }
                }
                if let Some(p) = a.permissions {
                    if let Err(e) =
                        std::fs::set_permissions(local, std::fs::Permissions::from_mode(p & 0o7777))
                    {
                        log::warn!("SFTP 保留元数据写本地权限失败（不影响传输结果）{local}: {e}");
                    }
                }
            }
        }
        Ok(())
    }

    /// 设定本会话 SFTP 通道的空闲关闭阈值（秒，0 = 常驻不关）。
    /// 阈值由前端从设置传入（行为设置的单一事实源在前端），文件面板在会话激活时调用。
    pub(crate) async fn sftp_set_idle_timeout(&self, sec: u32) {
        self.sftp.set_idle_timeout(sec);
    }

    /// 暂停/继续指定传输任务（paused=true 暂停，false 继续）
    pub(crate) async fn transfer_pause(&self, task_id: u64, paused: bool) -> Result<(), SshError> {        let ctl = self.sftp.get_ctl(task_id).await.ok_or(SshError::NotFound)?;
        let _ = ctl.paused.send(paused);
        Ok(())
    }

    /// 取消指定传输任务
    pub(crate) async fn transfer_cancel(&self, task_id: u64) -> Result<(), SshError> {
        let ctl = self.sftp.get_ctl(task_id).await.ok_or(SshError::NotFound)?;
        ctl.cancel.cancel();
        Ok(())
    }

    /// 删除远端文件/目录。目录递归删除（DFS 串行）：先清空全部子孙再 rmdir；
    /// 符号链接只删链接本身，不触碰其指向的目标。整个操作持管理锁。
    pub(crate) async fn sftp_remove_path(&self, path: String) -> Result<(), SshError> {
        let _g = self.sftp.manage.lock().await;
        let sftp = self.sftp_channel().await?;
        // lstat 不跟随软链：坏链接也能取到属性
        let attrs = sftp
            .lstat(&path)
            .await
            .map_err(|e| SshError::Channel(format!("获取 {path} 属性失败: {e}")))?
            .attrs;
        remove_recursive(&sftp, &path, &attrs).await
    }

    /// 重命名/移动远端路径。优先 `posix-rename@openssh.com`（原子语义，
    /// 目标存在时覆盖）；服务端不支持时降级标准 RENAME（目标存在则报错）。
    pub(crate) async fn sftp_rename_path(
        &self,
        old_path: String,
        new_path: String,
    ) -> Result<(), SshError> {
        let _g = self.sftp.manage.lock().await;
        let sftp = self.sftp_channel().await?;
        if posix_rename(&sftp, &old_path, &new_path).await.is_ok() {
            debug!("SFTP posix-rename: {old_path} → {new_path}");
            return Ok(());
        }
        debug!("posix-rename 不可用，降级标准 rename: {old_path} → {new_path}");
        sftp.rename(&old_path, &new_path)
            .await
            .map_err(|e| SshError::Channel(format!("重命名 {old_path} → {new_path} 失败: {e}")))?;
        Ok(())
    }

    /// 创建零字节空文件（CREATE，不截断已存在文件；前端调用前做重名校验）
    pub(crate) async fn sftp_create_file(&self, path: String) -> Result<(), SshError> {
        let sftp = self.sftp_channel().await?;
        let handle = sftp
            .open(&path, OpenFlags::CREATE, FileAttributes::default())
            .await
            .map_err(|e| SshError::Channel(format!("创建文件 {path} 失败: {e}")))?
            .handle;
        let _ = sftp.close(handle.as_str()).await;
        debug!("SFTP 创建文件: {path}");
        Ok(())
    }

    /// 远端复制（同主机内）。目录递归复制；符号链接复制为指向同目标的新软链
    /// （不跟随）；文件经同一 SFTP 会话 64KB 流式 read+write，不落地本地磁盘。
    /// 整个操作持管理锁。
    pub(crate) async fn sftp_copy_path(&self, src: String, dst: String) -> Result<(), SshError> {
        let _g = self.sftp.manage.lock().await;
        let sftp = self.sftp_channel().await?;
        copy_recursive(&sftp, &src, &dst).await
    }

    /// 读取符号链接的目标路径
    pub(crate) async fn sftp_readlink(&self, path: String) -> Result<String, SshError> {
        let sftp = self.sftp_channel().await?;
        let name = sftp
            .readlink(&path)
            .await
            .map_err(|e| SshError::Channel(format!("读取链接 {path} 失败: {e}")))?;
        name.files
            .first()
            .map(|f| f.filename.clone())
            .ok_or_else(|| SshError::Channel(format!("读取链接 {path} 失败：服务端未返回目标")))
    }

    /// 修改远端权限（chmod）。`mode` 只取低 12 位（07777，含 setuid/setgid/sticky；
    /// 对话框只用低 9 位）。`recursive` 时按 `scope` 处理子孙，逐项 SETSTAT，
    /// 任一项失败即中断并回错（已改的部分保持，与 `chmod -R` 一致）。
    /// SETSTAT 由服务端 chmod(2) 落盘：对被选中的软链，改的是它的目标（链接自身
    /// 权限恒 777，不在能力范围内）；递归时子孙中的软链一律跳过，避免把改动泼到
    /// 目录目标或指回上层的环上。
    ///
    /// 仅 e2e 集成测试直接可达（doc(hidden) 防误用）：生产调用走 `ipc::sftp_set_perm`。
    #[doc(hidden)]
    pub async fn sftp_set_perm(
        &self,
        path: String,
        mode: u32,
        recursive: bool,
        scope: String,
    ) -> Result<(), SshError> {
        let _g = self.sftp.manage.lock().await;
        let sftp = self.sftp_channel().await?;
        let attrs = sftp
            .lstat(&path)
            .await
            .map_err(|e| SshError::Channel(format!("获取 {path} 属性失败: {e}")))?
            .attrs;
        set_perm_recursive(&sftp, &path, &attrs, mode & 0o7777, recursive, PermScope::parse(&scope))
            .await
    }
}

/// 递归改权限的作用范围（对齐 FinalShell「递归设置子目录」三选一）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PermScope {
    /// 应用到文件和目录
    All,
    /// 只应用到文件
    Files,
    /// 只应用到目录
    Dirs,
}

impl PermScope {
    /// 前端传字符串（与 `PortConflict::parse` 同一约定）：未知值按最保守的 `All`
    fn parse(s: &str) -> Self {
        match s {
            "files" => Self::Files,
            "dirs" => Self::Dirs,
            _ => Self::All,
        }
    }

    /// 该范围是否命中这一条目（目录判定不含软链，软链一律按文件看待）
    fn applies(self, is_dir: bool) -> bool {
        match self {
            Self::All => true,
            Self::Files => !is_dir,
            Self::Dirs => is_dir,
        }
    }
}

/// 递归改权限：先按 scope 处理自身，再（recursive 且自身是真目录时）下探子孙。
/// 子孙的类型取自 READDIR 属性，故不再逐项 lstat；递归 async fn 需 Box::pin
/// 避免无限大小 future。
async fn set_perm_recursive(
    sftp: &RawSftpSession,
    path: &str,
    attrs: &FileAttributes,
    mode: u32,
    recursive: bool,
    scope: PermScope,
) -> Result<(), SshError> {
    let ftype = attrs.file_type();
    let is_dir = ftype.is_dir() && !ftype.is_symlink();
    if scope.applies(is_dir) {
        sftp.setstat(
            path,
            FileAttributes {
                permissions: Some(mode),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| SshError::Channel(format!("修改 {path} 权限失败: {e}")))?;
    }
    if recursive && is_dir {
        for (name, child_attrs) in list_children(sftp, path).await? {
            // 子孙里的软链整条跳过：SETSTAT 跟随链接会把改动泼到目标（目标可能是
            // 目录，甚至是指回上层的环）。与被选中的根条目不同——那是用户明确点的
            // 那一个，仍然跟随（故对话框对软链给出提示）。口径同 sftp_walk 下载。
            if child_attrs.file_type().is_symlink() {
                continue;
            }
            let child = join_child(path, &name);
            Box::pin(set_perm_recursive(
                sftp, &child, &child_attrs, mode, recursive, scope,
            ))
            .await?;
        }
    }
    debug!("SFTP chmod: {path} → {mode:#o} (recursive={recursive}, scope={scope:?})");
    Ok(())
}

/// OpenSSH posix-rename 扩展名（OpenSSH 5.0+ 支持；语义同 renameat2 RENAME，
/// 目标存在时原子覆盖，标准 RENAME 则要求目标不存在）
const POSIX_RENAME: &str = "posix-rename@openssh.com";

/// 编码一个 SFTP string（u32 大端长度 + 原始字节）到 buf
fn put_sftp_string(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_be_bytes());
    buf.extend_from_slice(s.as_bytes());
}

/// 拼接父目录与子项名（父目录为 "/" 时不重复斜杠）
fn join_child(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// 远端目录树枚举的层级上限：正常工程目录远不到这个深度，触到说明踩进了
/// 异常结构（如 /var/log 之下套 /proc）
const WALK_MAX_DEPTH: u32 = 64;

/// 取路径最后一段作为条目名（忽略结尾斜杠；"/" 原样返回）
fn base_name_of(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// 列举目录的全部直接子项（跳过 "."/".."），返回 (名称, 属性)。
/// 调用方传入的 dir 必须是可打开目录；EOF 以 Status(EOF) 识别。
async fn list_children(
    sftp: &RawSftpSession,
    dir: &str,
) -> Result<Vec<(String, FileAttributes)>, SshError> {
    let handle = sftp
        .opendir(dir)
        .await
        .map_err(|e| SshError::Channel(format!("打开目录 {dir} 失败: {e}")))?
        .handle;
    let mut out = Vec::new();
    let read_result = loop {
        match sftp.readdir(handle.as_str()).await {
            Ok(name) => {
                for f in name.files {
                    if f.filename != "." && f.filename != ".." {
                        out.push((f.filename, f.attrs));
                    }
                }
            }
            Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => {
                break Ok::<_, SshError>(());
            }
            Err(e) => break Err(SshError::Channel(format!("读取目录 {dir} 失败: {e}"))),
        }
    };
    let _ = sftp.close(handle.as_str()).await;
    read_result?;
    Ok(out)
}

/// 递归删除：软链/非目录 → remove（软链只删链接本身）；目录 → 先删全部子孙再 rmdir。
/// 递归 async fn 需 Box::pin 避免无限大小 future。
async fn remove_recursive(
    sftp: &RawSftpSession,
    path: &str,
    attrs: &FileAttributes,
) -> Result<(), SshError> {
    let ftype = attrs.file_type();
    if ftype.is_symlink() || !ftype.is_dir() {
        sftp.remove(path)
            .await
            .map_err(|e| SshError::Channel(format!("删除 {path} 失败: {e}")))?;
        return Ok(());
    }
    let children = list_children(sftp, path).await?;
    for (name, child_attrs) in children {
        let child = join_child(path, &name);
        Box::pin(remove_recursive(sftp, &child, &child_attrs)).await?;
    }
    sftp.rmdir(path)
        .await
        .map_err(|e| SshError::Channel(format!("删除目录 {path} 失败: {e}")))?;
    Ok(())
}

/// 经 EXTENDED 发起 posix-rename；服务端不支持该扩展时返回 Failure，调用方据此降级
async fn posix_rename(
    sftp: &RawSftpSession,
    old_path: &str,
    new_path: &str,
) -> Result<(), SshError> {
    let mut data = Vec::with_capacity(old_path.len() + new_path.len() + 8);
    put_sftp_string(&mut data, old_path);
    put_sftp_string(&mut data, new_path);
    sftp.extended(POSIX_RENAME, data)
        .await
        .map_err(|e| SshError::Channel(format!("posix-rename 扩展请求失败: {e}")))?;
    Ok(())
}

/// 单文件同会话流式复制：固定 64KB read→write，两端句柄用完即关，
/// 不落地本地磁盘、不占整文件内存。
async fn copy_file_stream(sftp: &RawSftpSession, src: &str, dst: &str) -> Result<(), SshError> {
    let rhandle = sftp
        .open(src, OpenFlags::READ, FileAttributes::default())
        .await
        .map_err(|e| SshError::Channel(format!("打开源文件 {src} 失败: {e}")))?
        .handle;
    let whandle = sftp
        .open(
            dst,
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
            FileAttributes::default(),
        )
        .await
        .map_err(|e| SshError::Channel(format!("打开目标文件 {dst} 失败: {e}")))?
        .handle;

    let mut offset = 0u64;
    let result = loop {
        match sftp
            .read(rhandle.as_str(), offset, DEFAULT_CHUNK as u32)
            .await
        {
            Ok(d) => {
                if d.data.is_empty() {
                    break Ok::<_, SshError>(());
                }
                let n = d.data.len() as u64;
                if let Err(e) = sftp.write(whandle.as_str(), offset, d.data).await {
                    break Err(SshError::Channel(format!("写入目标 {dst} 失败: {e}")));
                }
                offset += n;
            }
            Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => break Ok(()),
            Err(e) => break Err(SshError::Channel(format!("读取源 {src} 失败: {e}"))),
        }
    };
    let _ = sftp.close(rhandle.as_str()).await;
    let _ = sftp.close(whandle.as_str()).await;
    result
}

/// 递归复制：软链 → readlink 后在目标位置建同指向软链（不跟随）；
/// 目录 → mkdir 后逐项递归；文件 → 同会话流式复制。
async fn copy_recursive(sftp: &RawSftpSession, src: &str, dst: &str) -> Result<(), SshError> {
    // lstat 不跟随软链，保证链接复制为链接
    let attrs = sftp
        .lstat(src)
        .await
        .map_err(|e| SshError::Channel(format!("获取 {src} 属性失败: {e}")))?
        .attrs;
    let ftype = attrs.file_type();
    if ftype.is_symlink() {
        let target = sftp
            .readlink(src)
            .await
            .map_err(|e| SshError::Channel(format!("读取链接 {src} 失败: {e}")))?
            .files
            .first()
            .map(|f| f.filename.clone())
            .ok_or_else(|| SshError::Channel(format!("读取链接 {src} 失败：服务端未返回目标")))?;
        sftp.symlink(dst, &target)
            .await
            .map_err(|e| SshError::Channel(format!("创建链接 {dst} 失败: {e}")))?;
    } else if ftype.is_dir() {
        sftp.mkdir(dst, FileAttributes::default())
            .await
            .map_err(|e| SshError::Channel(format!("创建目录 {dst} 失败: {e}")))?;
        for (name, _) in list_children(sftp, src).await? {
            Box::pin(copy_recursive(
                sftp,
                &join_child(src, &name),
                &join_child(dst, &name),
            ))
            .await?;
        }
    } else {
        copy_file_stream(sftp, src, dst).await?;
    }
    Ok(())
}

/// 展开路径开头的 ~ 为当前用户家目录（不引入 shellexpand 依赖）
fn expand_tilde(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}{}", home, &path[1..]);
        }
    }
    path.to_string()
}

/// 本地文件前 `n` 字节的 SHA256 小写 hex（`n` 超过文件大小即整文件；u64::MAX = 整文件）。
/// 流式分块喂哈希，绝不整读进内存；用于 sha256 断点校验与 verify_hash 完整性校验。
async fn local_sha256_prefix(path: &str, n: u64) -> Result<String, SshError> {
    use sha2::{Digest, Sha256};
    let mut f = File::open(path)
        .await
        .map_err(|e| SshError::Channel(format!("打开本地文件 {path} 计算哈希失败: {e}")))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; DEFAULT_CHUNK];
    let mut remaining = n;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let read = f
            .read(&mut buf[..want])
            .await
            .map_err(|e| SshError::Channel(format!("读取本地文件 {path} 计算哈希失败: {e}")))?;
        if read == 0 {
            break; // EOF：文件比 n 短，已按实际内容取哈希
        }
        h.update(&buf[..read]);
        remaining -= read as u64;
    }
    Ok(hex_lower(&h.finalize()))
}

/// 远端文件前 `n` 字节的 SHA256 小写 hex；流式 SFTP READ，EOF 即止。
async fn remote_sha256_prefix(
    sftp: &RawSftpSession,
    path: &str,
    n: u64,
) -> Result<String, SshError> {
    use sha2::{Digest, Sha256};
    let handle = sftp
        .open(path, OpenFlags::READ, FileAttributes::default())
        .await
        .map_err(|e| SshError::Channel(format!("打开远端文件 {path} 计算哈希失败: {e}")))?
        .handle;
    let mut h = Sha256::new();
    let mut offset = 0u64;
    let result = loop {
        let want = n.saturating_sub(offset).min(DEFAULT_CHUNK as u64) as u32;
        if want == 0 {
            break Ok::<_, SshError>(());
        }
        match sftp.read(handle.as_str(), offset, want).await {
            Ok(d) if !d.data.is_empty() => {
                offset += d.data.len() as u64;
                h.update(&d.data);
            }
            Ok(_) => break Ok(()),
            Err(SftpError::Status(st)) if st.status_code == StatusCode::Eof => break Ok(()),
            Err(e) => {
                break Err(SshError::Channel(format!("读取远端文件 {path} 计算哈希失败: {e}")))
            }
        }
    };
    let _ = sftp.close(handle.as_str()).await;
    result.map(|_| hex_lower(&h.finalize()))
}

/// 当前 Unix 秒（sizeMtime 新鲜度窗口基准）
fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 经 IPC Channel 流式推送传输进度（前端 onmessage 消费）；
/// 通道关闭时仅记 debug，不中断传输（invoke 本身仍由前端 await）。
fn emit_progress(
    channel: &Channel<TransferProgress>,
    task_id: u64,
    transferred: u64,
    total: u64,
    speed: u64,
    done: bool,
) {
    let payload = TransferProgress {
        task_id,
        transferred,
        total,
        speed,
        done,
    };
    if let Err(e) = channel.send(payload) {
        debug!("发送 SFTP 进度（Channel 已关闭）: {e}");
    }
}

/// 若任务处于暂停态则阻塞，直到被恢复或取消；取消时返回 [`SshError::Cancelled`]
async fn wait_if_paused(
    paused_rx: &mut watch::Receiver<bool>,
    cancel: &CancellationToken,
) -> Result<(), SshError> {
    if !*paused_rx.borrow() {
        return Ok(());
    }
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err(SshError::Cancelled),
            changed = paused_rx.changed() => {
                if changed.is_err() {
                    return Err(SshError::Cancelled);
                }
                if !*paused_rx.borrow() {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_chain_lists_the_directory_itself_and_its_ancestors() {
        assert_eq!(dir_chain_of("/a/b/c"), vec!["/a", "/a/b", "/a/b/c"]);
        assert_eq!(dir_chain_of("/x"), vec!["/x"]);
        assert_eq!(dir_chain_of("sub/dir"), vec!["sub", "sub/dir"]);
        assert_eq!(dir_chain_of("/a//b"), vec!["/a", "/a/b"]);
        assert_eq!(dir_chain_of("/a/./b"), vec!["/a", "/a/b"]);
        // 根目录无需创建，空串也不产生任何目录
        assert_eq!(dir_chain_of("/"), Vec::<String>::new());
        assert_eq!(dir_chain_of(""), Vec::<String>::new());
        // 含 .. 的路径放弃推导：不凭猜测在远端建目录
        assert_eq!(dir_chain_of("/a/../b"), Vec::<String>::new());
    }

    #[test]
    fn owner_of_reads_kind_user_group_from_longname() {
        let plain = FileAttributes::default();
        assert_eq!(
            owner_of("-rw-r--r-- 1 root root 924 Oct  3 11:11 passwd", &plain),
            (
                "-".to_string(),
                "root".to_string(),
                "root".to_string()
            )
        );
        // 类型位取首字符，链接与目录因此在「类型」列可分辨
        assert_eq!(
            owner_of("lrwxrwxrwx 1 www-data www-data 7 Oct  3 11:11 link -> /etc", &plain).2,
            "www-data".to_string()
        );
        assert_eq!(owner_of("drwxr-xr-x 3 u g 4096 Oct  3 11:11 d", &plain).0, "d");
        // ACL 让权限串多出 '+'/'.'，仍然合法
        assert_eq!(owner_of("-rw-r--r--+ 1 u g 10 Oct  3 11:11 f", &plain).1, "u".to_string());
    }

    #[test]
    fn owner_of_falls_back_to_numeric_ids_when_longname_is_not_ls() {
        let attrs = FileAttributes {
            uid: Some(1000),
            gid: Some(1001),
            ..Default::default()
        };
        // 部分 sshd 的 NAME 响应只回文件名；此时类型位留空，前端按 isDir 兜底
        assert_eq!(
            owner_of("passwd", &attrs),
            (String::new(), "1000".to_string(), "1001".to_string())
        );
        // 权限串长度不足（不是 10 字符的 ls 布局）同样回退
        assert_eq!(owner_of("rwx u g", &attrs).0, String::new());
        // 属性里也没有 uid/gid 时给空串，而不是 "None"
        let none = FileAttributes::default();
        assert_eq!(
            owner_of("weird", &none),
            (String::new(), String::new(), String::new())
        );
    }

    #[test]
    fn base_name_takes_the_last_segment() {
        assert_eq!(base_name_of("/a/b/c"), "c");
        assert_eq!(base_name_of("/a/b/c/"), "c");
        assert_eq!(base_name_of("sub/dir"), "dir");
        assert_eq!(base_name_of("/x"), "x");
        // 根目录与空串没有「上一级」可言，原样返回
        assert_eq!(base_name_of("/"), "/");
        assert_eq!(base_name_of(""), "");
    }

    #[test]
    fn dir_tree_dedupes_across_the_whole_batch() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();
        // 同一棵树里的多个目录：公共祖先只出现一次，且父先于子
        assert_eq!(dir_tree_of(&p(&["/a/b", "/a/c"])), p(&["/a", "/a/b", "/a/c"]));
        assert_eq!(dir_tree_of(&p(&["/a", "/a/b"])), p(&["/a", "/a/b"]));
        // 一次拖入多个顶层项：各自成链，互不合并也不漏层
        assert_eq!(dir_tree_of(&p(&["/d/proj/src", "/d/docs"])), p(&["/d", "/d/proj", "/d/proj/src", "/d/docs"]));
        // 重复拖入同一路径不产生重复请求
        assert_eq!(dir_tree_of(&p(&["x/y", "x/y"])), p(&["x", "x/y"]));
        assert_eq!(dir_tree_of(&p(&["", "/"])), Vec::<String>::new());
    }

    #[test]
    fn perm_scope_parses_frontend_strings() {
        // 前端只传 files / dirs，其余（含未知值）落到覆盖面最广的 all
        assert_eq!(PermScope::parse("files"), PermScope::Files);
        assert_eq!(PermScope::parse("dirs"), PermScope::Dirs);
        assert_eq!(PermScope::parse("all"), PermScope::All);
        assert_eq!(PermScope::parse(""), PermScope::All);
        assert!(PermScope::All.applies(true) && PermScope::All.applies(false));
        assert!(PermScope::Files.applies(false) && !PermScope::Files.applies(true));
        assert!(PermScope::Dirs.applies(true) && !PermScope::Dirs.applies(false));
    }

    #[test]
    fn fmt_perm_stays_the_9_char_form_the_dialog_parses() {
        // 权限对话框从列表的 9 字符串回填勾选态：格式一变就会静默错勾，故在此钉住
        assert_eq!(fmt_perm(0o700), "rwx------");
        assert_eq!(fmt_perm(0o644), "rw-r--r--");
        assert_eq!(fmt_perm(0o755), "rwxr-xr-x");
        assert_eq!(fmt_perm(0o1755), "rwxr-xr-t");
        assert_eq!(fmt_perm(0o2644), "rw-r-Sr--");
    }

    #[test]
    fn rate_meter_zero_is_unlimited() {
        let mut m = RateMeter::new();
        let t0 = Instant::now();
        assert_eq!(m.acquire(0, 1 << 20, t0), Duration::ZERO);
        assert_eq!(m.acquire(0, 1 << 20, t0), Duration::ZERO);
    }

    #[test]
    fn rate_meter_paces_each_chunk() {
        let mut m = RateMeter::new();
        let t0 = Instant::now();
        // 500 B/s：250 字节的块应在 t0+0.5s 付完账
        assert_eq!(m.acquire(500, 250, t0), Duration::from_millis(500));
        // 同一时刻再要 250：上一块的预约未到期，累计等待 1s
        assert_eq!(m.acquire(500, 250, t0), Duration::from_millis(1000));
        // 积压已排空（t0+1.2s > 预约的 t0+1s）：每块只付自己的账，
        // 即返回该块自身的耗时——调用点在写完之后，等的就是下一块的起点
        assert_eq!(
            m.acquire(500, 250, t0 + Duration::from_millis(1200)),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn rate_meter_shared_across_tasks() {
        // 全局桶由多个传输共用同一个 Arc：排队效果相加，合计速率不超上限
        let m = Arc::new(std::sync::Mutex::new(RateMeter::new()));
        let t0 = Instant::now();
        let w1 = m.lock().unwrap().acquire(1000, 500, t0);
        let w2 = m.lock().unwrap().acquire(1000, 500, t0);
        assert_eq!(w1, Duration::from_millis(500));
        assert_eq!(w2, Duration::from_millis(1000));
    }

    #[test]
    fn rate_meter_resets_on_rate_change() {
        let mut m = RateMeter::new();
        let t0 = Instant::now();
        assert_eq!(m.acquire(100, 100, t0), Duration::from_millis(1000));
        // 用户把上限从 100B/s 调到 10KB/s：旧速率的预约不能继续拖累新速率
        assert_eq!(m.acquire(10_000, 100, t0), Duration::from_millis(10));
    }

    #[test]
    fn throttle_only_active_when_policy_sets_rate() {
        let pol = TransferPolicy::default();
        let t = Throttle::from_policy(&pol, "h:22", "u");
        assert!(t.global.is_none() && t.task.is_none());
        let pol2 = TransferPolicy {
            global_rate_kb: 512,
            task_rate_kb: 64,
            ..Default::default()
        };
        let t2 = Throttle::from_policy(&pol2, "h:22", "u");
        assert_eq!(t2.global.as_ref().map(|g| g.1), Some(512 * 1024));
        assert_eq!(t2.task.as_ref().map(|g| g.1), Some(64 * 1024));
    }

    #[test]
    fn temp_path_appends_suffix_in_place() {
        assert_eq!(temp_path_of("/home/u/a.txt"), "/home/u/a.txt.rhost-tmp");
        assert_eq!(temp_path_of("/"), "/.rhost-tmp");
    }

    #[test]
    fn resolve_overrides_policy() {
        // ask + 用户裁决：overwrite → 继续，skip → 跳过
        assert_eq!(
            decide_conflict("ask", Some("overwrite"), 10, 5, None, None),
            Conflict::Proceed
        );
        assert_eq!(
            decide_conflict("ask", Some("skip"), 10, 5, None, None),
            Conflict::Skip
        );
        // 裁决对非 ask 策略同样优先（弹窗点击是比设置更强的意图表达）
        assert_eq!(
            decide_conflict("skip", Some("overwrite"), 10, 5, None, None),
            Conflict::Proceed
        );
    }

    #[test]
    fn decide_conflict_basic_policies() {
        assert_eq!(
            decide_conflict("skip", None, 10, 5, Some(1), Some(0)),
            Conflict::Skip
        );
        assert_eq!(
            decide_conflict("overwrite", None, 10, 5, Some(0), Some(1)),
            Conflict::Proceed
        );
        // ask 无裁决 → 交前端询问，不静默处置
        assert_eq!(
            decide_conflict("ask", None, 10, 5, Some(1), Some(1)),
            Conflict::Ask
        );
        // 未知值降级为旧行为（覆盖）
        assert_eq!(
            decide_conflict("bogus", None, 10, 5, Some(0), Some(9)),
            Conflict::Proceed
        );
    }

    #[test]
    fn decide_conflict_newer_uses_mtime() {
        // 源严格更新 → 覆盖；更新或大小相等 → 跳过
        assert_eq!(
            decide_conflict("newer", None, 10, 10, Some(100), Some(99)),
            Conflict::Proceed
        );
        assert_eq!(
            decide_conflict("newer", None, 10, 10, Some(100), Some(100)),
            Conflict::Skip
        );
        assert_eq!(
            decide_conflict("newer", None, 10, 10, Some(99), Some(100)),
            Conflict::Skip
        );
    }

    #[test]
    fn decide_conflict_newer_falls_back_to_size() {
        // 任一侧 mtime 缺失：大小不等视为源已变更（覆盖），相等视为已同步（跳过）
        assert_eq!(
            decide_conflict("newer", None, 10, 9, Some(1), None),
            Conflict::Proceed
        );
        assert_eq!(
            decide_conflict("newer", None, 10, 10, None, Some(1)),
            Conflict::Skip
        );
        assert_eq!(
            decide_conflict("newer", None, 10, 10, None, None),
            Conflict::Skip
        );
        assert_eq!(
            decide_conflict("newer", None, 10, 11, None, None),
            Conflict::Proceed
        );
    }

    #[test]
    fn partial_fresh_window() {
        let now = 1_700_000_000;
        assert!(partial_is_fresh(Some(now - 3600), now));
        assert!(partial_is_fresh(Some(now + 3600), now)); // 时钟偏移取绝对窗
        assert!(partial_is_fresh(Some(now - RESUME_FRESH_WINDOW), now));
        assert!(!partial_is_fresh(Some(now - RESUME_FRESH_WINDOW - 1), now));
        // 探不到 mtime 无法证明新鲜 → 不新鲜（重传）
        assert!(!partial_is_fresh(None, now));
    }

    #[test]
    fn resume_check_dispatch() {
        // size：恒过（既有行为）
        assert!(resume_check_ok("size", false, false));
        assert!(resume_check_ok("bogus", false, false)); // 未知值降级 size
        // sizeMtime：只看新鲜度
        assert!(resume_check_ok("sizeMtime", true, false));
        assert!(!resume_check_ok("sizeMtime", false, true));
        // sha256：只看前缀哈希一致
        assert!(resume_check_ok("sha256", false, true));
        assert!(!resume_check_ok("sha256", true, false));
    }

    #[test]
    fn hex_lower_format() {
        assert_eq!(hex_lower(&[0x0a, 0xff, 0x00]), "0aff00");
        assert_eq!(hex_lower(&[]), "");
    }

    #[test]
    fn sha256_resume_check_forces_verify() {
        // 显式开启：校验
        let mut p = TransferPolicy {
            verify_hash: true,
            ..Default::default()
        };
        assert!(verify_required(&p));
        // 缺省（旧行为）：不校验
        p.verify_hash = false;
        assert!(!verify_required(&p));
        // resumeCheck=sha256 隐含整文件校验承诺：即使用户没开 verifyHash
        p.resume_check = "sha256".into();
        assert!(verify_required(&p));
        // 其余断点校验方式不隐含
        p.resume_check = "sizeMtime".into();
        assert!(!verify_required(&p));
    }

    #[test]
    fn policy_deserializes_camel_case_with_defaults() {
        // 前端只传部分字段：其余按 Default（= 旧行为）补齐
        let p: TransferPolicy = serde_json::from_value(serde_json::json!({
            "overwritePolicy": "ask",
            "uploadTemp": true,
            "resumeCheck": "sha256",
        }))
        .unwrap();
        assert_eq!(p.overwrite_policy, "ask");
        assert!(p.upload_temp);
        assert!(!p.preserve_meta);
        assert_eq!(p.resume_check, "sha256");
        assert!(!p.verify_hash);
        assert_eq!(p.resolve, None);
        // 全缺省 = 既有行为
        let d = TransferPolicy::default();
        assert_eq!(d.overwrite_policy, "overwrite");
        assert!(!d.upload_temp && !d.verify_hash && !d.preserve_meta);
        assert_eq!(d.resume_check, "size");
    }

    #[tokio::test]
    async fn local_sha256_prefix_matches_known_digests() {
        use sha2::{Digest, Sha256};
        let dir = std::env::temp_dir().join(format!("rhost-sftp-test-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("hash-src.bin");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        tokio::fs::write(&path, &data).await.unwrap();
        let p = path.to_str().unwrap();

        // n 截断：只哈希前 n 字节
        let n = 65_536u64;
        let expect = hex_lower(&Sha256::digest(&data[..n as usize]));
        assert_eq!(local_sha256_prefix(p, n).await.unwrap(), expect);
        // u64::MAX：整文件
        let full = hex_lower(&Sha256::digest(&data));
        assert_eq!(local_sha256_prefix(p, u64::MAX).await.unwrap(), full);
        // n 超过文件大小：按实际内容截尾，与整文件一致（EOF 语义）
        assert_eq!(local_sha256_prefix(p, 10u64.pow(9)).await.unwrap(), full);
        // 空前缀 → 空串哈希
        assert_eq!(
            local_sha256_prefix(p, 0).await.unwrap(),
            hex_lower(&Sha256::digest(&[] as &[u8]))
        );
        let _ = tokio::fs::remove_file(&path).await;
        let _ = tokio::fs::remove_dir(&dir).await;
    }
}
