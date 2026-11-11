//! 工具箱：按会话执行的远端运维工具（端口查看 / 防火墙管理）。
//!
//! 与指标采集一样走独立 exec 通道（不经 PTY、不污染终端），但这里是用户主动
//! 触发的一次性操作。需要 root 的命令按需提权：
//! - 无密码时先试 `sudo -n`，不可用则返回 [`SUDO_REQUIRED`] 由前端弹窗；
//! - 密码只经 exec 通道 stdin 交给 `sudo -S -p ''`，绝不拼进命令行、不落日志；
//! - 前端仅在内存中按会话缓存密码，断开即清。
//!
//! 单通道会话（堡垒机外部连接）沿用 `exec_collect` 的拒绝逻辑：这类主机开
//! 第二条通道会被堡垒机掐断，工具箱直接返回后端错误。

mod docker;
mod firewall;
mod ports;
mod pubkey;
mod syslog;

use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::State;
use tauri::ipc::Channel;

use crate::applog;
use crate::applog::events as ev;
use crate::ssh::manager::SessionManager;
use crate::ssh::session::ExecOutput;
use crate::ssh::sftp::TransferProgress;

pub(crate) const SUDO_REQUIRED: &str = "SUDO_PASSWORD_REQUIRED";
pub(crate) const SUDO_INCORRECT: &str = "SUDO_PASSWORD_INCORRECT";

/// 只读列举超时：慢主机上 ss/ps/docker 三段也要留余量
const LIST_TIMEOUT: Duration = Duration::from_secs(12);
/// 防火墙变更超时：firewall-cmd --reload 与 systemctl 启停较慢
const ACTION_TIMEOUT: Duration = Duration::from_secs(20);
/// 容器日志读取超时：大行数 + 慢磁盘留余量
const LOGS_TIMEOUT: Duration = Duration::from_secs(30);
/// 镜像导入导出超时：镜像动辄数 GB，只受磁盘与网络吞吐限制
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(1800);
/// 中转 tar 清理超时：单个 rm，慢磁盘也只需秒级
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Docker 配置读取 / 保存 + 服务重启
const CONFIG_TIMEOUT: Duration = Duration::from_secs(90);
/// 日志落盘上限：与远端读取上限（512 KiB 字符）留足 UTF-8 余量
const MAX_LOG_SAVE_BYTES: u64 = 8 * 1024 * 1024;

struct ToolExec {
    out: ExecOutput,
    elevated: bool,
}

fn err_str(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// 单引号 shell 转义（与 exec_collect 的包装口径一致）
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 把 sudo 密码规范成「一行 + 换行」的 PTY 首行输入；密码不进命令行与日志
fn sudo_password_line(password: &str) -> Result<Vec<u8>, String> {
    let pw = password.trim_end_matches(['\r', '\n']);
    if pw.is_empty() {
        return Err(SUDO_REQUIRED.into());
    }
    if pw.contains(['\r', '\n']) {
        return Err("sudo 密码不能包含换行".into());
    }
    let mut line = Vec::with_capacity(pw.len() + 1);
    line.extend_from_slice(pw.as_bytes());
    line.push(b'\n');
    Ok(line)
}

/// 执行 Docker 脚本：有密码直接提权；否则先普通执行，socket 权限不足再用免密 sudo 重试。
async fn run_docker(
    manager: &SessionManager,
    session_id: &str,
    script: &str,
    password: Option<&str>,
    timeout: Duration,
) -> Result<ToolExec, String> {
    if password.is_some() {
        return exec_root(manager, session_id, script, password, timeout).await;
    }
    let plain = exec_plain(manager, session_id, script, timeout).await?;
    if plain.out.output.contains(docker::DOCKER_PERMISSION_DENIED) {
        exec_root(manager, session_id, script, None, timeout).await
    } else {
        Ok(plain)
    }
}

/// 执行系统日志读取脚本：有密码直接提权；否则先普通执行，受保护日志读不了再用免密 sudo 重试。
async fn run_syslog(
    manager: &SessionManager,
    session_id: &str,
    script: &str,
    password: Option<&str>,
    timeout: Duration,
) -> Result<ToolExec, String> {
    if password.is_some() {
        return exec_root(manager, session_id, script, password, timeout).await;
    }
    let plain = exec_plain(manager, session_id, script, timeout).await?;
    if plain.out.output.contains(syslog::SYSLOG_PERMISSION_DENIED) {
        exec_root(manager, session_id, script, None, timeout).await
    } else {
        Ok(plain)
    }
}

async fn exec_plain(
    manager: &SessionManager,
    session_id: &str,
    script: &str,
    timeout: Duration,
) -> Result<ToolExec, String> {
    let out = manager
        .exec_collect(session_id, script, timeout)
        .await
        .map_err(err_str)?;
    Ok(ToolExec {
        out,
        elevated: false,
    })
}

/// 以 root 执行 `inner`（POSIX sh 脚本体）。
///
/// 密码路径先验证一次，避免把 sudo 的「密码错误」与命令本身失败混在一起；
/// 无密码路径把 `sudo -n` 的可用性判断折进同一条脚本，只用一次往返。
async fn exec_root(
    manager: &SessionManager,
    session_id: &str,
    inner: &str,
    password: Option<&str>,
    timeout: Duration,
) -> Result<ToolExec, String> {
    if let Some(pw) = password {
        let pw = pw.trim_end_matches(['\r', '\n']);
        if pw.is_empty() {
            return Err(SUDO_REQUIRED.into());
        }
        if pw.contains(['\r', '\n']) {
            return Err("sudo 密码不能包含换行".into());
        }
        let mut stdin = Vec::with_capacity(pw.len() + 1);
        stdin.extend_from_slice(pw.as_bytes());
        stdin.push(b'\n');

        let probe = manager
            .exec_collect_with_stdin(session_id, "sudo -S -p '' true", &stdin, timeout)
            .await
            .map_err(err_str)?;
        if probe.exit_code != Some(0) {
            let lower = probe.output.to_ascii_lowercase();
            if lower.contains("command not found") || lower.contains("未安装 sudo") {
                return Err("远端未安装 sudo，无法以管理员权限执行".into());
            }
            if lower.contains("tty") {
                return Err(
                    "该主机的 sudo 需要 TTY，当前远程执行通道无法交互；请配置免密 sudo 或改用 root 登录"
                        .into(),
                );
            }
            return Err(SUDO_INCORRECT.into());
        }
        let run = format!("sudo -S -p '' /bin/sh -c {}", sq(inner));
        let out = manager
            .exec_collect_with_stdin(session_id, &run, &stdin, timeout)
            .await
            .map_err(err_str)?;
        Ok(ToolExec {
            out,
            elevated: true,
        })
    } else {
        let run = format!(
            "command -v sudo >/dev/null 2>&1 || {{ echo '远端未安装 sudo，无法提权' >&2; exit 1; }}; \
             if sudo -n true 2>/dev/null; then sudo -n /bin/sh -c {inner_q}; else echo '{sentinel}'; fi",
            inner_q = sq(inner),
            sentinel = SUDO_REQUIRED
        );
        let out = manager
            .exec_collect(session_id, &run, timeout)
            .await
            .map_err(err_str)?;
        if out.output.contains("远端未安装 sudo") {
            return Err("远端未安装 sudo，无法以管理员权限执行".into());
        }
        if out.output.contains(SUDO_REQUIRED) {
            return Err(SUDO_REQUIRED.into());
        }
        Ok(ToolExec {
            out,
            elevated: true,
        })
    }
}

/// 删除镜像中转 tar：导出时文件由 root 创建（SFTP 用户因 /tmp 粘滞位无权删除），
/// 按本次执行是否提权走 exec；导入时文件由 SFTP 上传、归当前用户，可直接删。
/// 清理失败只记日志——/tmp 残留不影响功能，不能因此把成功的传输判成失败。
async fn cleanup_remote_temp(
    manager: &SessionManager,
    session_id: &str,
    path: &str,
    elevated: bool,
    password: Option<&str>,
) {
    let script = format!("rm -f {}", sq(path));
    let out = if elevated {
        exec_root(manager, session_id, &script, password, CLEANUP_TIMEOUT).await
    } else {
        exec_plain(manager, session_id, &script, CLEANUP_TIMEOUT).await
    };
    match out {
        Ok(o) if o.out.exit_code == Some(0) => {}
        Ok(o) => log::warn!(
            "Docker 中转文件清理返回非零 (session={session_id}, path={path}): exit={:?} {}",
            o.out.exit_code,
            o.out.output.trim()
        ),
        Err(e) => log::warn!("Docker 中转文件清理失败 (session={session_id}, path={path}): {e}"),
    }
}

/// 端口列表查询结果
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortsResult {
    /// 实际使用的采集工具：ss / netstat / lsof / none
    pub tool: String,
    /// true 表示以 root 视图采集（sudo 提权或本身就是 root 登录）
    pub elevated: bool,
    pub rows: Vec<ports::PortRow>,
}

/// 列举远端监听端口（用户主动触发，按需提权）。
#[tauri::command]
pub async fn toolbox_list_ports(
    session_id: String,
    sudo_password: Option<String>,
    elevate: Option<bool>,
    manager: State<'_, SessionManager>,
) -> Result<PortsResult, String> {
    let started = Instant::now();
    let elevate = elevate.unwrap_or(false) || sudo_password.is_some();
    let exec = if elevate {
        exec_root(
            &manager,
            &session_id,
            ports::PORTS_SCRIPT,
            sudo_password.as_deref(),
            LIST_TIMEOUT,
        )
        .await?
    } else {
        exec_plain(&manager, &session_id, ports::PORTS_SCRIPT, LIST_TIMEOUT).await?
    };
    let parsed = ports::parse(&exec.out.output).map_err(|e| {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_PORTS_LIST,
            Some(&session_id),
            "端口列表解析失败",
            Some(serde_json::json!({
                "elevated": exec.elevated,
                "exit_code": exec.out.exit_code,
                "timed_out": exec.out.timed_out,
                "err": e,
            })),
        );
        e
    })?;
    let result = PortsResult {
        tool: parsed.tool,
        // root 登录本身就能看到全部进程与容器归属，与是否走了 sudo 无关
        elevated: exec.elevated || parsed.uid == Some(0),
        rows: parsed.rows,
    };
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_PORTS_LIST,
        Some(&session_id),
        "端口列表采集完成",
        Some(serde_json::json!({
            "tool": result.tool,
            "count": result.rows.len(),
            "with_user": result.rows.iter().filter(|r| r.user.is_some()).count(),
            "with_pid": result.rows.iter().filter(|r| r.pid.is_some()).count(),
            "with_container": result.rows.iter().filter(|r| r.container.is_some()).count(),
            "uid": parsed.uid,
            "docker_cli": parsed.docker_cli,
            "cgroup_entries": parsed.cgroup_entries,
            "container_names": parsed.container_names,
            "published_ports": parsed.published_ports,
            "elevated": result.elevated,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "timed_out": exec.out.timed_out,
        })),
    );
    Ok(result)
}

/// 防火墙状态与规则列表（按需提权）。
#[tauri::command]
pub async fn toolbox_firewall_status(
    session_id: String,
    sudo_password: Option<String>,
    manager: State<'_, SessionManager>,
) -> Result<firewall::FirewallStatus, String> {
    let started = Instant::now();
    let exec = exec_root(
        &manager,
        &session_id,
        firewall::STATUS_SCRIPT,
        sudo_password.as_deref(),
        LIST_TIMEOUT,
    )
    .await?;
    let status = firewall::parse_status(&exec.out.output).map_err(|e| {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_FIREWALL_LIST,
            Some(&session_id),
            "防火墙规则解析失败",
            Some(serde_json::json!({
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "exit_code": exec.out.exit_code,
                "timed_out": exec.out.timed_out,
                "err": e,
            })),
        );
        e
    })?;
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_FIREWALL_LIST,
        Some(&session_id),
        "防火墙规则读取完成",
        Some(serde_json::json!({
            "backend": status.backend,
            "enabled": status.enabled,
            "count": status.rules.len(),
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "timed_out": exec.out.timed_out,
        })),
    );
    Ok(status)
}

/// 新增一条防火墙规则（端口可填 `80` 或 `8080:8090`；source 可空）。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn toolbox_firewall_add(
    session_id: String,
    port: String,
    protocol: String,
    action: String,
    source: Option<String>,
    sudo_password: Option<String>,
    manager: State<'_, SessionManager>,
) -> Result<String, String> {
    let started = Instant::now();
    let port = firewall::validate_port_spec(&port)?;
    let protocol = firewall::validate_protocol(&protocol)?;
    let action = firewall::validate_action(&action)?;
    let source = firewall::validate_source(source.as_deref().unwrap_or(""))?;
    let script = firewall::build_add_script(&port, protocol, action, source.as_deref())?;
    let exec = exec_root(
        &manager,
        &session_id,
        &script,
        sudo_password.as_deref(),
        ACTION_TIMEOUT,
    )
    .await?;
    let msg = firewall::change_result(&exec.out, "新增规则")?;
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_FIREWALL_CHANGE,
        Some(&session_id),
        "防火墙规则已新增",
        Some(serde_json::json!({
            "op": "add",
            "port": port,
            "protocol": protocol,
            "action": action,
            "has_source": source.is_some(),
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "exit_code": exec.out.exit_code,
        })),
    );
    Ok(msg)
}

/// 删除一条防火墙规则；字段来自规则列表解析结果（origin 决定用哪条删除命令），
/// 后端重新探测后端类型。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn toolbox_firewall_remove(
    session_id: String,
    port: String,
    protocol: String,
    action: String,
    source: Option<String>,
    origin: String,
    sudo_password: Option<String>,
    manager: State<'_, SessionManager>,
) -> Result<String, String> {
    let started = Instant::now();
    let port = firewall::validate_port_spec(&port)?;
    let protocol = firewall::validate_protocol(&protocol)?;
    let action = firewall::validate_action(&action)?;
    let source = firewall::validate_source(source.as_deref().unwrap_or(""))?;
    let script =
        firewall::build_remove_script(&port, protocol, action, source.as_deref(), &origin)?;
    let exec = exec_root(
        &manager,
        &session_id,
        &script,
        sudo_password.as_deref(),
        ACTION_TIMEOUT,
    )
    .await?;
    let msg = firewall::change_result(&exec.out, "删除规则")?;
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_FIREWALL_CHANGE,
        Some(&session_id),
        "防火墙规则已删除",
        Some(serde_json::json!({
            "op": "remove",
            "port": port,
            "protocol": protocol,
            "action": action,
            "origin": origin,
            "has_source": source.is_some(),
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "exit_code": exec.out.exit_code,
        })),
    );
    Ok(msg)
}

/// 把本地公钥追加到远端用户的 `~/.ssh/authorized_keys`（按需提权）。
/// 已存在相同公钥时不重复写入，返回真实路径与是否新增。
#[tauri::command]
pub async fn toolbox_deploy_pubkey(
    session_id: String,
    public_key: String,
    target_user: String,
    sudo_password: Option<String>,
    manager: State<'_, SessionManager>,
) -> Result<pubkey::PubkeyDeploy, String> {
    let started = Instant::now();
    let user = pubkey::validate_user(&target_user)?;
    let (line, key_type, body) = pubkey::normalize_key(&public_key)?;
    let script = pubkey::build_script(&user, &key_type, &body, &line);
    // 无密码先走普通通道：目标用户就是登录用户时直接成功；
    // 需要写其他用户时脚本返回 SUDO_REQUIRED，先尝试免密 sudo，再交给前端问密码。
    let exec = if sudo_password.is_some() {
        exec_root(
            &manager,
            &session_id,
            &script,
            sudo_password.as_deref(),
            ACTION_TIMEOUT,
        )
        .await?
    } else {
        let plain = exec_plain(&manager, &session_id, &script, ACTION_TIMEOUT).await?;
        if plain.out.output.contains(SUDO_REQUIRED) {
            exec_root(&manager, &session_id, &script, None, ACTION_TIMEOUT).await?
        } else {
            plain
        }
    };
    let result = pubkey::deploy_result(&exec.out).map_err(|e| {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_PUBKEY_DEPLOY,
            Some(&session_id),
            "写入公钥失败",
            Some(serde_json::json!({
                "target_user": user,
                "key_type": key_type,
                "elevated": exec.elevated,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "exit_code": exec.out.exit_code,
                "timed_out": exec.out.timed_out,
                "err": e,
            })),
        );
        e
    })?;
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_PUBKEY_DEPLOY,
        Some(&session_id),
        if result.added {
            "公钥已写入"
        } else {
            "公钥已存在"
        },
        Some(serde_json::json!({
            "target_user": user,
            "key_type": key_type,
            "path": result.path,
            "added": result.added,
            "elevated": exec.elevated,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "exit_code": exec.out.exit_code,
        })),
    );
    Ok(result)
}

/// Docker 容器与镜像列表结果
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DockerResult {
    /// true 表示以 root 视图采集（sudo 提权或本身就是 root 登录）
    pub elevated: bool,
    pub docker_version: Option<String>,
    pub compose_version: Option<String>,
    pub containers: Vec<docker::ContainerRow>,
    pub images: Vec<docker::ImageRow>,
}

/// 列举远端容器与镜像（按需提权：socket 权限不足时先试免密 sudo）。
#[tauri::command]
pub async fn toolbox_docker_list(
    session_id: String,
    sudo_password: Option<String>,
    manager: State<'_, SessionManager>,
) -> Result<DockerResult, String> {
    let started = Instant::now();
    let script = docker::build_list_script();
    let exec = run_docker(
        &manager,
        &session_id,
        &script,
        sudo_password.as_deref(),
        LIST_TIMEOUT,
    )
    .await?;
    let parsed = docker::parse_list(&exec.out.output).map_err(|e| {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_DOCKER_LIST,
            Some(&session_id),
            "Docker 列表读取失败",
            Some(serde_json::json!({
                "elevated": exec.elevated,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "exit_code": exec.out.exit_code,
                "timed_out": exec.out.timed_out,
                "err": e,
            })),
        );
        e
    })?;
    let result = DockerResult {
        elevated: exec.elevated || parsed.uid == Some(0),
        docker_version: parsed.docker_version,
        compose_version: parsed.compose_version,
        containers: parsed.containers,
        images: parsed.images,
    };
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_DOCKER_LIST,
        Some(&session_id),
        "Docker 列表读取完成",
        Some(serde_json::json!({
            "containers": result.containers.len(),
            "running": result.containers.iter().filter(|c| c.state == "running").count(),
            "images": result.images.len(),
            "docker_version": result.docker_version,
            "compose_version": result.compose_version,
            "elevated": result.elevated,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "timed_out": exec.out.timed_out,
        })),
    );
    Ok(result)
}

/// 容器启停 / 重启 / 删除，或删除镜像。目标与动作由后端校验后重建命令。
#[tauri::command]
pub async fn toolbox_docker_action(
    session_id: String,
    kind: String,
    action: String,
    target: String,
    sudo_password: Option<String>,
    manager: State<'_, SessionManager>,
) -> Result<String, String> {
    let started = Instant::now();
    let target = docker::validate_target(&kind, &target)?;
    let script = docker::build_action_script(&kind, &action, &target)?;
    let exec = run_docker(
        &manager,
        &session_id,
        &script,
        sudo_password.as_deref(),
        ACTION_TIMEOUT,
    )
    .await?;
    docker::action_result(&exec.out).map_err(|e| {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_DOCKER_ACTION,
            Some(&session_id),
            "Docker 操作失败",
            Some(serde_json::json!({
                "kind": kind,
                "action": action,
                "target": target,
                "elevated": exec.elevated,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "exit_code": exec.out.exit_code,
                "timed_out": exec.out.timed_out,
                "err": e,
            })),
        );
        e
    })?;
    let msg = match (kind.as_str(), action.as_str()) {
        ("container", "start") => format!("容器 {target} 已启动"),
        ("container", "stop") => format!("容器 {target} 已停止"),
        ("container", "restart") => format!("容器 {target} 已重启"),
        ("container", "remove") => format!("容器 {target} 已删除"),
        ("image", "remove") => format!("镜像 {target} 已删除"),
        _ => "Docker 操作已完成".into(),
    };
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_DOCKER_ACTION,
        Some(&session_id),
        &msg,
        Some(serde_json::json!({
            "kind": kind,
            "action": action,
            "target": target,
            "elevated": exec.elevated,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "exit_code": exec.out.exit_code,
        })),
    );
    Ok(msg)
}

/// 读取容器日志（末尾 N 行，可选时间戳）。
#[tauri::command]
pub async fn toolbox_docker_logs(
    session_id: String,
    container: String,
    tail: u32,
    timestamps: bool,
    sudo_password: Option<String>,
    manager: State<'_, SessionManager>,
) -> Result<docker::LogsResult, String> {
    let started = Instant::now();
    let script = docker::build_logs_script(&container, tail, timestamps)?;
    let exec = run_docker(
        &manager,
        &session_id,
        &script,
        sudo_password.as_deref(),
        LOGS_TIMEOUT,
    )
    .await?;
    let result = docker::parse_logs(&exec.out).map_err(|e| {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_DOCKER_LOGS,
            Some(&session_id),
            "容器日志读取失败",
            Some(serde_json::json!({
                "container": container,
                "tail": tail,
                "timestamps": timestamps,
                "elevated": exec.elevated,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "exit_code": exec.out.exit_code,
                "err": e,
            })),
        );
        e
    })?;
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_DOCKER_LOGS,
        Some(&session_id),
        "容器日志读取完成",
        Some(serde_json::json!({
            "container": container,
            "tail": tail,
            "chars": result.output.chars().count(),
            "truncated": result.truncated,
            "elevated": exec.elevated,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        })),
    );
    Ok(result)
}

/// 导出镜像到远端 tar 文件；文件已存在返回 `DOCKER_EXPORT_EXISTS` 交给前端确认。
#[tauri::command]
pub async fn toolbox_docker_export(
    session_id: String,
    image: String,
    path: String,
    overwrite: bool,
    sudo_password: Option<String>,
    manager: State<'_, SessionManager>,
) -> Result<String, String> {
    let started = Instant::now();
    let image = docker::validate_target("image", &image)?;
    let path = docker::validate_remote_path(&path)?;
    let script = docker::build_export_script(&image, &path, overwrite)?;
    let exec = run_docker(
        &manager,
        &session_id,
        &script,
        sudo_password.as_deref(),
        TRANSFER_TIMEOUT,
    )
    .await?;
    let size = docker::export_result(&exec.out).map_err(|e| {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_DOCKER_TRANSFER,
            Some(&session_id),
            "镜像导出失败",
            Some(serde_json::json!({
                "op": "export",
                "image": image,
                "path": path,
                "overwrite": overwrite,
                "elevated": exec.elevated,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "exit_code": exec.out.exit_code,
                "err": e,
            })),
        );
        e
    })?;
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_DOCKER_TRANSFER,
        Some(&session_id),
        "镜像导出完成",
        Some(serde_json::json!({
            "op": "export",
            "image": image,
            "path": path,
            "size": size,
            "elevated": exec.elevated,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        })),
    );
    Ok(size)
}

/// 导出镜像到本地文件：远端 `docker save` 到临时 tar → SFTP 下载 → 删除临时文件。
/// 下载进度经 [`Channel`] 推送；返回人读大小。
#[tauri::command]
pub async fn toolbox_docker_export_local(
    session_id: String,
    image: String,
    local_path: String,
    sudo_password: Option<String>,
    task_id: u64,
    channel: Channel<TransferProgress>,
    manager: State<'_, SessionManager>,
) -> Result<String, String> {
    let started = Instant::now();
    let image = docker::validate_target("image", &image)?;
    let local_path = docker::validate_local_path(&local_path)?;
    let temp = docker::temp_tar_path(&image);
    let script = docker::build_export_script(&image, &temp, true)?;
    let exec = run_docker(
        &manager,
        &session_id,
        &script,
        sudo_password.as_deref(),
        TRANSFER_TIMEOUT,
    )
    .await?;

    let size = match docker::export_result(&exec.out) {
        Ok(size) => size,
        Err(e) => {
            cleanup_remote_temp(
                &manager,
                &session_id,
                &temp,
                exec.elevated,
                sudo_password.as_deref(),
            )
            .await;
            applog::emit(
                log::Level::Warn,
                "toolbox",
                ev::TOOLBOX_DOCKER_TRANSFER,
                Some(&session_id),
                "镜像导出到本地失败",
                Some(serde_json::json!({
                    "op": "export_local",
                    "image": image,
                    "local": local_path,
                    "elevated": exec.elevated,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                    "exit_code": exec.out.exit_code,
                    "err": e,
                })),
            );
            return Err(e);
        }
    };

    let download = manager
        .sftp_download(
            &session_id,
            task_id,
            temp.clone(),
            local_path.clone(),
            None,
            Some(false),
            None,
            None,
            channel,
        )
        .await;
    cleanup_remote_temp(
        &manager,
        &session_id,
        &temp,
        exec.elevated,
        sudo_password.as_deref(),
    )
    .await;
    if let Err(e) = download {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_DOCKER_TRANSFER,
            Some(&session_id),
            "镜像下载到本地失败",
            Some(serde_json::json!({
                "op": "export_local",
                "image": image,
                "local": local_path,
                "size": size,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "err": e.to_string(),
            })),
        );
        return Err(format!("镜像已打包，但下载到本地失败：{e}"));
    }

    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_DOCKER_TRANSFER,
        Some(&session_id),
        "镜像已导出到本地",
        Some(serde_json::json!({
            "op": "export_local",
            "image": image,
            "local": local_path,
            "size": size,
            "elevated": exec.elevated,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        })),
    );
    Ok(size)
}

/// 从本地文件导入镜像：SFTP 上传到远端临时 tar → `docker load` → 删除临时文件。
/// 上传进度经 [`Channel`] 推送；返回导入摘要（与远端路径导入一致）。
#[tauri::command]
pub async fn toolbox_docker_import_local(
    session_id: String,
    local_path: String,
    custom_tag: Option<String>,
    sudo_password: Option<String>,
    task_id: u64,
    channel: Channel<TransferProgress>,
    manager: State<'_, SessionManager>,
) -> Result<String, String> {
    let started = Instant::now();
    let local_path = docker::validate_local_path(&local_path)?;
    let tag = docker::validate_import_tag(custom_tag.as_deref())?;
    let bytes = std::fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0);
    let label = std::path::Path::new(&local_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let temp = docker::temp_tar_path(label);
    // 先探测 Docker 权限：密码缺失时在文件上传前返回 SUDO_REQUIRED，
    // 避免大 tar 传完才弹提权、重试重传整份文件。
    let probe = run_docker(
        &manager,
        &session_id,
        &docker::build_probe_script(),
        sudo_password.as_deref(),
        LIST_TIMEOUT,
    )
    .await?;
    docker::probe_result(&probe.out)?;

    let script = docker::build_import_script(&temp, tag.as_deref())?;
    if let Err(e) = manager
        .sftp_upload(
            &session_id,
            task_id,
            local_path.clone(),
            temp.clone(),
            None,
            Some(false),
            None,
            None,
            channel,
        )
        .await
    {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_DOCKER_TRANSFER,
            Some(&session_id),
            "本地镜像文件上传失败",
            Some(serde_json::json!({
                "op": "import_local",
                "local": local_path,
                "bytes": bytes,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "err": e.to_string(),
            })),
        );
        // 上传中断（含取消/断链）会留下半个 tar，尽力清掉
        cleanup_remote_temp(&manager, &session_id, &temp, false, None).await;
        return Err(format!("上传镜像文件失败：{e}"));
    }

    let exec = match run_docker(
        &manager,
        &session_id,
        &script,
        sudo_password.as_deref(),
        TRANSFER_TIMEOUT,
    )
    .await
    {
        Ok(exec) => exec,
        Err(e) => {
            cleanup_remote_temp(&manager, &session_id, &temp, false, None).await;
            return Err(e);
        }
    };
    let result = docker::import_result(&exec.out);
    cleanup_remote_temp(&manager, &session_id, &temp, false, None).await;
    let summary = result.map_err(|e| {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_DOCKER_TRANSFER,
            Some(&session_id),
            "本地镜像文件导入失败",
            Some(serde_json::json!({
                "op": "import_local",
                "local": local_path,
                "bytes": bytes,
                "tagged": tag.is_some(),
                "elevated": exec.elevated,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "exit_code": exec.out.exit_code,
                "err": e,
            })),
        );
        e
    })?;

    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_DOCKER_TRANSFER,
        Some(&session_id),
        "镜像已从本地文件导入",
        Some(serde_json::json!({
            "op": "import_local",
            "local": local_path,
            "bytes": bytes,
            "tag": tag,
            "elevated": exec.elevated,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        })),
    );
    Ok(summary)
}

/// 从远端 tar 文件导入镜像；`custom_tag` 为可选完整镜像引用，导入后追加标签。
#[tauri::command]
pub async fn toolbox_docker_import(
    session_id: String,
    path: String,
    custom_tag: Option<String>,
    sudo_password: Option<String>,
    manager: State<'_, SessionManager>,
) -> Result<String, String> {
    let started = Instant::now();
    let path = docker::validate_remote_path(&path)?;
    let tag = docker::validate_import_tag(custom_tag.as_deref())?;
    let script = docker::build_import_script(&path, tag.as_deref())?;
    let exec = run_docker(
        &manager,
        &session_id,
        &script,
        sudo_password.as_deref(),
        TRANSFER_TIMEOUT,
    )
    .await?;
    let summary = docker::import_result(&exec.out).map_err(|e| {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_DOCKER_TRANSFER,
            Some(&session_id),
            "镜像导入失败",
            Some(serde_json::json!({
                "op": "import",
                "path": path,
                "tagged": tag.is_some(),
                "elevated": exec.elevated,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "exit_code": exec.out.exit_code,
                "err": e,
            })),
        );
        e
    })?;
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_DOCKER_TRANSFER,
        Some(&session_id),
        "镜像导入完成",
        Some(serde_json::json!({
            "op": "import",
            "path": path,
            "tag": tag,
            "elevated": exec.elevated,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        })),
    );
    Ok(summary)
}

/// 从自定义镜像地址（含私有仓库）拉取镜像，可选追加自定义标签。
#[tauri::command]
pub async fn toolbox_docker_pull(
    session_id: String,
    image: String,
    custom_tag: Option<String>,
    sudo_password: Option<String>,
    manager: State<'_, SessionManager>,
) -> Result<String, String> {
    let started = Instant::now();
    let image = docker::validate_target("image", &image)?;
    let tag = docker::validate_custom_tag(&image, custom_tag.as_deref())?;
    let script = docker::build_pull_script(&image, tag.as_deref())?;
    let exec = run_docker(
        &manager,
        &session_id,
        &script,
        sudo_password.as_deref(),
        TRANSFER_TIMEOUT,
    )
    .await?;
    let summary = docker::pull_result(&exec.out).map_err(|e| {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_DOCKER_TRANSFER,
            Some(&session_id),
            "镜像拉取失败",
            Some(serde_json::json!({
                "op": "pull",
                "image": image,
                "tagged": tag.is_some(),
                "elevated": exec.elevated,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "exit_code": exec.out.exit_code,
                "err": e,
            })),
        );
        e
    })?;
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_DOCKER_TRANSFER,
        Some(&session_id),
        "镜像拉取完成",
        Some(serde_json::json!({
            "op": "pull",
            "image": image,
            "tag": tag,
            "elevated": exec.elevated,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        })),
    );
    Ok(summary)
}

/// 把前端展示的容器日志写到本地文件（内容来自已读取的日志，不重新执行远端命令）。
#[tauri::command]
pub async fn toolbox_docker_logs_save(path: String, content: String) -> Result<String, String> {
    if content.len() as u64 > MAX_LOG_SAVE_BYTES {
        return Err("日志内容过大，无法保存".into());
    }
    let p = std::path::PathBuf::from(&path);
    std::fs::write(&p, content.as_bytes()).map_err(|e| format!("保存日志失败：{e}"))?;
    let abs = std::fs::canonicalize(&p).unwrap_or(p);
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_DOCKER_LOGS,
        None,
        "容器日志已保存",
        Some(serde_json::json!({
            "op": "save",
            "path": abs.display().to_string(),
            "bytes": content.len(),
        })),
    );
    Ok(abs.display().to_string())
}

/// daemon.json 顶层键列表（日志用，便于确认哪些配置被保留）
fn daemon_keys(json: Option<&str>) -> Vec<String> {
    json.and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .and_then(|v| {
            v.as_object()
                .map(|obj| obj.keys().cloned().collect::<Vec<_>>())
        })
        .unwrap_or_default()
}

/// 读取 Docker 配置：CLI 版本 + daemon.json 原文、镜像源与数据目录。
///
/// 普通用户通常读不了 `/etc/docker/daemon.json`，脚本返回 `@@UNREADABLE@@`
/// 时改用 sudo 重读；密码路径直接提权。
async fn read_docker_config(
    manager: &SessionManager,
    session_id: &str,
    password: Option<&str>,
) -> Result<docker::DockerConfig, String> {
    let script = docker::build_config_get_script();
    let exec = if password.is_some() {
        exec_root(manager, session_id, &script, password, CONFIG_TIMEOUT).await?
    } else {
        let plain = exec_plain(manager, session_id, &script, CONFIG_TIMEOUT).await?;
        if plain.out.output.contains(docker::DAEMON_UNREADABLE) {
            exec_root(manager, session_id, &script, None, CONFIG_TIMEOUT).await?
        } else {
            plain
        }
    };
    docker::parse_config(&exec.out.output)
}

#[tauri::command]
pub async fn toolbox_docker_config_get(
    session_id: String,
    sudo_password: Option<String>,
    manager: State<'_, SessionManager>,
) -> Result<docker::DockerConfig, String> {
    let cfg = read_docker_config(&manager, &session_id, sudo_password.as_deref()).await;
    if let Ok(ref c) = cfg {
        applog::emit(
            log::Level::Info,
            "toolbox",
            ev::TOOLBOX_DOCKER_CONFIG,
            Some(&session_id),
            "Docker 配置读取完成",
            Some(serde_json::json!({
                "op": "get",
                "version": c.version,
                "mirrors": c.registry_mirrors.len(),
                "data_root": c.data_root.as_deref(),
                "keys": daemon_keys(c.daemon_json.as_deref()),
                "daemon_json": c.daemon_json.is_some(),
            })),
        );
    }
    cfg
}

/// 保存 daemon.json：以前端（可视化表单 + 保存预览编辑器）给出的完整内容写入，
/// 后端只做结构校验与规范化；写入前备份，再重启 Docker 服务，重启失败会还原备份。
#[tauri::command]
pub async fn toolbox_docker_config_save(
    session_id: String,
    daemon_json: String,
    sudo_password: Option<String>,
    manager: State<'_, SessionManager>,
) -> Result<docker::DockerConfig, String> {
    let started = Instant::now();
    let merged = docker::validate_daemon_json(&daemon_json)?;
    let script = docker::build_config_save_script(&merged);
    let exec = exec_root(
        &manager,
        &session_id,
        &script,
        sudo_password.as_deref(),
        CONFIG_TIMEOUT,
    )
    .await?;
    docker::config_save_result(&exec.out).map_err(|e| {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_DOCKER_CONFIG,
            Some(&session_id),
            "Docker 配置保存失败",
            Some(serde_json::json!({
                "op": "save",
                "keys": daemon_keys(Some(&merged)),
                "bytes": merged.len(),
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "exit_code": exec.out.exit_code,
                "timed_out": exec.out.timed_out,
                "err": e,
            })),
        );
        e
    })?;
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_DOCKER_CONFIG,
        Some(&session_id),
        "Docker 配置已保存并重启服务",
        Some(serde_json::json!({
            "op": "save",
            "keys": daemon_keys(Some(&merged)),
            "bytes": merged.len(),
            "elapsed_ms": started.elapsed().as_millis() as u64,
        })),
    );
    read_docker_config(&manager, &session_id, sudo_password.as_deref()).await
}

/// 打开容器交互终端：先探活并按需提权，再在独立 SSH 通道上 `docker exec -it`。
///
/// 免密 sudo 与需要密码两种提权方式在命令里区分：需要密码时先 `sudo -v`
/// 预热本 PTY 的票据，密码作为 PTY 首行输入（不出现在命令行与日志）。
#[tauri::command]
pub async fn toolbox_docker_terminal_open(
    session_id: String,
    container: String,
    cols: u32,
    rows: u32,
    sudo_password: Option<String>,
    channel: Channel<Vec<u8>>,
    manager: State<'_, SessionManager>,
) -> Result<(), String> {
    let started = Instant::now();
    let container = docker::validate_target("container", &container)?;
    let probe = docker::build_probe_script();

    let plain = exec_plain(&manager, &session_id, &probe, LIST_TIMEOUT).await?;
    let mode = if !plain.out.output.contains(docker::DOCKER_PERMISSION_DENIED) {
        docker::probe_result(&plain.out)?;
        docker::SudoMode::None
    } else if sudo_password.is_some() {
        match exec_root(&manager, &session_id, &probe, None, LIST_TIMEOUT).await {
            Ok(_) => docker::SudoMode::Passwordless,
            Err(e) if e == SUDO_REQUIRED => {
                let exec = exec_root(
                    &manager,
                    &session_id,
                    &probe,
                    sudo_password.as_deref(),
                    LIST_TIMEOUT,
                )
                .await?;
                docker::probe_result(&exec.out)?;
                docker::SudoMode::Password
            }
            Err(e) => return Err(e),
        }
    } else {
        let exec = exec_root(&manager, &session_id, &probe, None, LIST_TIMEOUT).await?;
        docker::probe_result(&exec.out)?;
        docker::SudoMode::Passwordless
    };

    let command = docker::build_terminal_command(&container, mode)?;
    let (term, mut frame_rx) = manager
        .open_docker_terminal(&session_id, command, cols, rows)
        .await
        .map_err(err_str)?;

    if mode == docker::SudoMode::Password {
        let line = sudo_password_line(
            sudo_password
                .as_deref()
                .ok_or_else(|| SUDO_REQUIRED.to_string())?,
        )?;
        term.write(line).await.map_err(err_str)?;
    }

    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_DOCKER_TERMINAL,
        Some(&session_id),
        "容器终端已打开",
        Some(serde_json::json!({
            "op": "open",
            "container": container,
            "sudo": match mode {
                docker::SudoMode::None => "none",
                docker::SudoMode::Passwordless => "passwordless",
                docker::SudoMode::Password => "password",
            },
            "cols": cols,
            "rows": rows,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        })),
    );

    tauri::async_runtime::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            if channel.send(frame).is_err() {
                break;
            }
        }
    });
    Ok(())
}

#[tauri::command]
pub async fn toolbox_docker_terminal_write(
    session_id: String,
    data: String,
    manager: State<'_, SessionManager>,
) -> Result<(), String> {
    manager
        .docker_terminal_write(&session_id, data.into_bytes())
        .await
        .map_err(err_str)
}

#[tauri::command]
pub async fn toolbox_docker_terminal_resize(
    session_id: String,
    cols: u32,
    rows: u32,
    manager: State<'_, SessionManager>,
) -> Result<(), String> {
    manager
        .docker_terminal_resize(&session_id, cols, rows)
        .await
        .map_err(err_str)
}

#[tauri::command]
pub async fn toolbox_docker_terminal_close(
    session_id: String,
    manager: State<'_, SessionManager>,
) -> Result<(), String> {
    let closed = manager.close_docker_terminal(&session_id).await;
    if closed {
        applog::emit(
            log::Level::Info,
            "toolbox",
            ev::TOOLBOX_DOCKER_TERMINAL,
            Some(&session_id),
            "容器终端已关闭",
            Some(serde_json::json!({ "op": "close" })),
        );
    }
    Ok(())
}

/// 防火墙启停，返回操作后的状态；启用前自动放行当前 SSH 端口。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToggleResult {
    pub status: firewall::FirewallStatus,
    /// 启用时是否自动补了 SSH 端口放行
    pub ssh_port_auto_opened: bool,
}

#[tauri::command]
pub async fn toolbox_firewall_toggle(
    session_id: String,
    enable: bool,
    ssh_port: u16,
    sudo_password: Option<String>,
    manager: State<'_, SessionManager>,
) -> Result<ToggleResult, String> {
    if ssh_port == 0 {
        return Err("SSH 端口不合法".into());
    }
    let started = Instant::now();
    let status_exec = exec_root(
        &manager,
        &session_id,
        firewall::STATUS_SCRIPT,
        sudo_password.as_deref(),
        LIST_TIMEOUT,
    )
    .await?;
    let status = firewall::parse_status(&status_exec.out.output)?;

    let script = firewall::build_toggle_script(&status, enable, ssh_port)?;
    let ssh_auto = enable && firewall::ssh_rule_missing(&status, ssh_port);
    let exec = exec_root(
        &manager,
        &session_id,
        &script,
        sudo_password.as_deref(),
        ACTION_TIMEOUT,
    )
    .await?;
    firewall::change_result(&exec.out, if enable { "启用防火墙" } else { "停用防火墙" })?;

    let after = exec_root(
        &manager,
        &session_id,
        firewall::STATUS_SCRIPT,
        sudo_password.as_deref(),
        LIST_TIMEOUT,
    )
    .await?;
    let status = firewall::parse_status(&after.out.output)?;
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_FIREWALL_CHANGE,
        Some(&session_id),
        if enable {
            "防火墙已启用"
        } else {
            "防火墙已停用"
        },
        Some(serde_json::json!({
            "op": if enable { "enable" } else { "disable" },
            "backend": status.backend,
            "ssh_port": ssh_port,
            "ssh_port_auto_opened": ssh_auto,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        })),
    );
    Ok(ToggleResult {
        status,
        ssh_port_auto_opened: ssh_auto,
    })
}

/* =========================================================
 *  系统日志
 * ========================================================= */

/// 探测远端可用的系统日志来源（按发行版差异组装；只读，无需提权）。
#[tauri::command]
pub async fn toolbox_syslog_sources(
    session_id: String,
    manager: State<'_, SessionManager>,
) -> Result<syslog::SyslogInfo, String> {
    let started = Instant::now();
    let exec = exec_plain(
        &manager,
        &session_id,
        &syslog::sources_script(),
        LIST_TIMEOUT,
    )
    .await?;
    let info = syslog::parse_sources(&exec.out.output).map_err(|e| {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_SYSLOG_SOURCES,
            Some(&session_id),
            "系统日志来源探测失败",
            Some(serde_json::json!({
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "exit_code": exec.out.exit_code,
                "timed_out": exec.out.timed_out,
                "err": e,
            })),
        );
        e
    })?;
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_SYSLOG_SOURCES,
        Some(&session_id),
        "系统日志来源探测完成",
        Some(serde_json::json!({
            "os": info.os,
            "distro": info.distro,
            "count": info.sources.len(),
            "journal": info.sources.iter().any(|s| s.kind == "journal"),
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "timed_out": exec.out.timed_out,
        })),
    );
    Ok(info)
}

/// 读取某个来源的末尾 N 行（受保护日志按需提权）。
/// `since`/`until` 仅 journald 源服务端生效，文件源/macOS 由前端按时间戳过滤。
#[tauri::command]
pub async fn toolbox_syslog_read(
    session_id: String,
    source: String,
    tail: u32,
    since: Option<String>,
    until: Option<String>,
    sudo_password: Option<String>,
    manager: State<'_, SessionManager>,
) -> Result<syslog::SyslogReadResult, String> {
    let started = Instant::now();
    let script = syslog::build_read_script(&source, tail, since.as_deref(), until.as_deref())?;
    let exec = run_syslog(
        &manager,
        &session_id,
        &script,
        sudo_password.as_deref(),
        LOGS_TIMEOUT,
    )
    .await?;
    let result = syslog::parse_read(&exec.out).map_err(|e| {
        applog::emit(
            log::Level::Warn,
            "toolbox",
            ev::TOOLBOX_SYSLOG_READ,
            Some(&session_id),
            "系统日志读取失败",
            Some(serde_json::json!({
                "source": source,
                "tail": tail,
                "since": since,
                "until": until,
                "elevated": exec.elevated,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "exit_code": exec.out.exit_code,
                "timed_out": exec.out.timed_out,
                "err": e,
            })),
        );
        e
    })?;
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_SYSLOG_READ,
        Some(&session_id),
        "系统日志读取完成",
        Some(serde_json::json!({
            "source": source,
            "tail": tail,
            "since": since,
            "until": until,
            "chars": result.output.chars().count(),
            "truncated": result.truncated,
            "elevated": exec.elevated,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        })),
    );
    Ok(result)
}

/// 把前端展示的系统日志写到本地文件（内容来自已读取的日志，不重新执行远端命令）。
#[tauri::command]
pub async fn toolbox_syslog_save(path: String, content: String) -> Result<String, String> {
    if content.len() as u64 > MAX_LOG_SAVE_BYTES {
        return Err("日志内容过大，无法保存".into());
    }
    let p = std::path::PathBuf::from(&path);
    std::fs::write(&p, content.as_bytes()).map_err(|e| format!("保存日志失败：{e}"))?;
    let abs = std::fs::canonicalize(&p).unwrap_or(p);
    applog::emit(
        log::Level::Info,
        "toolbox",
        ev::TOOLBOX_SYSLOG_READ,
        None,
        "系统日志已保存",
        Some(serde_json::json!({
            "op": "save",
            "path": abs.display().to_string(),
            "bytes": content.len(),
        })),
    );
    Ok(abs.display().to_string())
}
