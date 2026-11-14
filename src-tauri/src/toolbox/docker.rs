//! Docker 管理：容器 / 镜像列表、容器启停、日志、交互式终端、镜像导入导出，
//! 以及 daemon.json 的镜像源配置。
//!
//! 列表与一次性操作走独立 exec 通道，远端只跑只读命令或明确的单个动作；
//! docker socket 通常仅 root 或 docker 组可访问，普通用户无权限时返回
//! [`DOCKER_PERMISSION_DENIED`]，由调用方决定是否用 sudo 重试。
//!
//! 操作目标（容器名 / ID、镜像引用）、远端文件路径与 daemon.json 里的字段
//! 均先经白名单校验，再由后端重建 shell 脚本，前端文本不直接拼进命令。

use serde::Serialize;
use serde_json::json;

use super::sq;
use crate::ssh::session::ExecOutput;

/// 普通用户没有 docker socket 权限：前端据此弹 sudo 后重试
pub(super) const DOCKER_PERMISSION_DENIED: &str = "DOCKER_PERMISSION_DENIED";

const DOCKER_MISSING: &str = "DOCKER_MISSING";
const DOCKER_DAEMON_DOWN: &str = "DOCKER_DAEMON_DOWN";
const DOCKER_ERROR: &str = "DOCKER_ERROR";
const ACTION_OK: &str = "DOCKER_ACTION_OK";
const ACTION_FAILED: &str = "DOCKER_ACTION_FAILED";

const LOGS_MARKER: &str = "@@LOGS@@";
const EXPORT_OK: &str = "DOCKER_EXPORT_OK";
const EXPORT_EXISTS: &str = "DOCKER_EXPORT_EXISTS";
const EXPORT_FAILED: &str = "DOCKER_EXPORT_FAILED";
const IMPORT_OK: &str = "DOCKER_IMPORT_OK";
const IMPORT_MISSING: &str = "DOCKER_IMPORT_MISSING";
const IMPORT_FAILED: &str = "DOCKER_IMPORT_FAILED";
const PULL_OK: &str = "DOCKER_PULL_OK";
const PULL_FAILED: &str = "DOCKER_PULL_FAILED";
const TAG_FAILED: &str = "DOCKER_TAG_FAILED";
const CONFIG_OK: &str = "DOCKER_CONFIG_OK";
const CONFIG_FAILED: &str = "DOCKER_CONFIG_FAILED";
const CONFIG_RESTART_FAILED: &str = "DOCKER_CONFIG_RESTART_FAILED";
const TERMINAL_READY: &str = "DOCKER_TERMINAL_READY";

/// 远端 daemon 配置文件（rootless 场景不在此列）
pub(super) const DAEMON_JSON_PATH: &str = "/etc/docker/daemon.json";
/// 普通用户读不了 daemon.json：调用方据此提权重读
pub(super) const DAEMON_UNREADABLE: &str = "@@UNREADABLE@@";

/// 远端报错正文截断上限（错误横幅只展示摘要）
const MAX_DETAIL: usize = 400;
/// 日志回传上限：超出只保留末尾，前端明确提示已截断
const MAX_LOG_CHARS: usize = 512 * 1024;
/// 镜像源条目数上限
const MAX_MIRRORS: usize = 10;
/// data-root 路径长度上限
const MAX_DATA_ROOT: usize = 512;
/// daemon.json 内容长度上限（保存预览可直接编辑）
const MAX_DAEMON_JSON: usize = 64 * 1024;

/// 脚本公共前缀：固定使用 PATH 中的 `docker` 并做存在性检查
fn prelude() -> String {
    format!(
        "DOCKER='docker'\n\
         if [ ! -x \"$DOCKER\" ] && ! command -v \"$DOCKER\" >/dev/null 2>&1; then\n  \
         echo '{missing}'\n  exit 4\nfi\n",
        missing = DOCKER_MISSING,
    )
}

fn permission_case() -> String {
    format!(
        "case \"$err\" in\n    *[Pp]ermission*denied*) echo '{denied}' ;;\n    \
         *[Cc]annot*connect*|*daemon*running*) echo '{down}' ;;\n    \
         *) echo '{err}'; printf '%s\\n' \"$err\" ;;\n  esac",
        denied = DOCKER_PERMISSION_DENIED,
        down = DOCKER_DAEMON_DOWN,
        err = DOCKER_ERROR,
    )
}

/// 列表脚本：先确认 CLI 与 daemon 可用，再输出容器 / 镜像 / 网络 / 数据卷四段。
///
/// `--no-trunc` 保留完整 ID 供操作；`\t` 分隔，状态、端口、创建时间里的
/// 空格不会打乱列。daemon 不可用时区分权限不足与未启动，前者交给提权重试。
/// 容器状态由 `Status` 文本推导：19.03 等旧版 `docker ps` 模板没有 `.State` 字段。
/// 网络用 `docker network inspect` 的 JSON：一次拿齐驱动、IPAM 与已连接容器；
/// 数据卷同样用 `docker volume inspect`，拿到驱动、挂载点与创建时间。
pub(super) fn build_list_script() -> String {
    format!(
        r#"{prelude}echo '@@DIAG@@'
echo "UID=$(id -u 2>/dev/null)"
err=$("$DOCKER" info 2>&1 >/dev/null)
if [ $? -ne 0 ]; then
  {case}
  exit 5
fi
docker_ver=$("$DOCKER" version --format '{{{{.Server.Version}}}}' 2>/dev/null)
if [ -z "$docker_ver" ]; then
  docker_ver=$("$DOCKER" version --format '{{{{.Client.Version}}}}' 2>/dev/null)
fi
echo "DOCKER_VERSION=$docker_ver"
compose_ver=$("$DOCKER" compose version --short 2>/dev/null)
if [ -z "$compose_ver" ]; then
  compose_ver=$(docker-compose version --short 2>/dev/null)
fi
if [ -z "$compose_ver" ]; then
  compose_ver=$(docker-compose --version 2>/dev/null)
fi
echo "COMPOSE_VERSION=$compose_ver"
echo '@@CONTAINERS@@'
"$DOCKER" ps -a --no-trunc --format '{{{{.ID}}}}\t{{{{.Names}}}}\t{{{{.Image}}}}\t{{{{.Status}}}}\t{{{{.Ports}}}}\t{{{{.CreatedAt}}}}' 2>&1
echo '@@IMAGES@@'
"$DOCKER" images --no-trunc --format '{{{{.ID}}}}\t{{{{.Repository}}}}\t{{{{.Tag}}}}\t{{{{.CreatedSince}}}}\t{{{{.Size}}}}\t{{{{.CreatedAt}}}}' 2>&1
echo '@@NETWORKS@@'
net_ids=$("$DOCKER" network ls -q 2>/dev/null)
if [ -n "$net_ids" ]; then
  "$DOCKER" network inspect $net_ids 2>&1
fi
echo '@@VOLUMES@@'
vol_names=$("$DOCKER" volume ls -q 2>/dev/null)
if [ -n "$vol_names" ]; then
  "$DOCKER" volume inspect $vol_names 2>&1
fi
echo '@@END@@'
"#,
        prelude = prelude(),
        case = permission_case(),
    )
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerRow {
    pub id: String,
    pub name: String,
    pub image: String,
    /// created / running / paused / restarting / removing / exited / dead
    pub state: String,
    /// docker ps 的 Status 列，如 `Up 3 days`、`Exited (0) 2 hours ago`
    pub status: String,
    pub ports: String,
    pub created: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRow {
    pub id: String,
    /// 悬空镜像为 `<none>`
    pub repository: String,
    pub tag: String,
    /// 相对时间（如 `2 weeks ago`），仅用于展示
    pub created: String,
    /// `docker images .CreatedAt` 原始时间戳，供前端按时间排序
    pub created_at: String,
    pub size: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkRow {
    pub id: String,
    pub name: String,
    pub driver: String,
    /// local / swarm
    pub scope: String,
    /// 多个 IPAM 段用 `, ` 连接；未配置时为空
    pub subnet: String,
    pub gateway: String,
    pub internal: bool,
    pub ipv6: bool,
    /// 已连接容器，形如 `web-1 (172.18.0.2/16)`，按名称排序
    pub containers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeRow {
    pub name: String,
    pub driver: String,
    /// local / global
    pub scope: String,
    /// 宿主机上的数据目录；部分旧版 daemon 不返回
    pub mountpoint: String,
    /// 原始创建时间；部分 daemon 不返回该字段
    pub created_at: String,
}

#[derive(Debug)]
pub(super) struct DockerList {
    /// 远端 exec 通道的 euid（root 登录也是 0，与是否走 sudo 无关）
    pub uid: Option<u32>,
    /// 守护进程版本，daemon 不支持时退化为客户端版本
    pub docker_version: Option<String>,
    /// Docker Compose 版本（v2 插件或独立的 docker-compose）
    pub compose_version: Option<String>,
    pub containers: Vec<ContainerRow>,
    pub images: Vec<ImageRow>,
    pub networks: Vec<NetworkRow>,
    pub volumes: Vec<VolumeRow>,
}

/// 解析列表输出；权限不足 / CLI 缺失 / daemon 未启动均转换为可读错误。
pub(super) fn parse_list(output: &str) -> Result<DockerList, String> {
    if output.contains(DOCKER_MISSING) {
        return Err("远端未安装 docker CLI，无法管理容器与镜像".into());
    }
    if output.contains(DOCKER_PERMISSION_DENIED) {
        return Err(DOCKER_PERMISSION_DENIED.into());
    }
    if output.contains(DOCKER_DAEMON_DOWN) {
        return Err("无法连接 Docker 守护进程，请确认 docker 服务已启动".into());
    }
    if let Some((_, rest)) = output.split_once(DOCKER_ERROR) {
        let detail = one_line(rest);
        return Err(if detail.is_empty() {
            "读取 Docker 信息失败".into()
        } else {
            format!("读取 Docker 信息失败：{detail}")
        });
    }

    let mut uid: Option<u32> = None;
    let mut docker_version: Option<String> = None;
    let mut compose_version: Option<String> = None;
    let mut containers_out = String::new();
    let mut images_out = String::new();
    let mut networks_out = String::new();
    let mut volumes_out = String::new();
    let mut section = "";
    for line in output.lines() {
        match line.trim_end() {
            "@@DIAG@@" => {
                section = "diag";
                continue;
            }
            "@@CONTAINERS@@" => {
                section = "containers";
                continue;
            }
            "@@IMAGES@@" => {
                section = "images";
                continue;
            }
            "@@NETWORKS@@" => {
                section = "networks";
                continue;
            }
            "@@VOLUMES@@" => {
                section = "volumes";
                continue;
            }
            "@@END@@" => {
                section = "end";
                continue;
            }
            _ => {}
        }
        match section {
            "diag" => {
                if let Some(v) = line.strip_prefix("UID=") {
                    uid = v.trim().parse().ok();
                } else if let Some(v) = line.strip_prefix("DOCKER_VERSION=") {
                    docker_version = clean_version(v);
                } else if let Some(v) = line.strip_prefix("COMPOSE_VERSION=") {
                    compose_version = clean_version(v);
                }
            }
            "containers" => {
                containers_out.push_str(line);
                containers_out.push('\n');
            }
            "images" => {
                images_out.push_str(line);
                images_out.push('\n');
            }
            "networks" => {
                networks_out.push_str(line);
                networks_out.push('\n');
            }
            "volumes" => {
                volumes_out.push_str(line);
                volumes_out.push('\n');
            }
            _ => {}
        }
    }

    let containers = parse_containers(&containers_out);
    if containers.is_empty() && has_content(&containers_out) {
        return Err(format!(
            "无法解析 docker 容器列表：{}",
            one_line(&containers_out)
        ));
    }
    let images = parse_images(&images_out);
    if images.is_empty() && has_content(&images_out) {
        return Err(format!(
            "无法解析 docker 镜像列表：{}",
            one_line(&images_out)
        ));
    }
    let networks = parse_networks(&networks_out)?;
    let volumes = parse_volumes(&volumes_out)?;
    Ok(DockerList {
        uid,
        docker_version,
        compose_version,
        containers,
        images,
        networks,
        volumes,
    })
}

/// 归一化版本输出：`docker-compose --version` 会带前缀与 build 信息，
/// 只保留第一个数字开头的词（如 `1.25.0`）。
fn clean_version(raw: &str) -> Option<String> {
    let line = raw.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    let token = line
        .split(|c: char| c.is_whitespace() || c == ',')
        .find(|t| {
            t.trim_start_matches('v')
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        })
        .unwrap_or(line);
    Some(token.trim_start_matches('v').to_string())
}

fn has_content(text: &str) -> bool {
    text.lines().any(|l| !l.trim().is_empty())
}

fn parse_containers(text: &str) -> Vec<ContainerRow> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.trim_end().splitn(6, '\t');
            let id = f.next()?.trim();
            let name = f.next()?.trim();
            let image = f.next()?.trim();
            let status = f.next()?.trim();
            let ports = f.next()?.trim();
            let created = f.next().unwrap_or("").trim();
            if id.is_empty() || name.is_empty() {
                return None;
            }
            Some(ContainerRow {
                id: id.to_string(),
                name: name.to_string(),
                image: image.to_string(),
                state: container_state(status),
                status: status.to_string(),
                ports: ports.to_string(),
                created: created.to_string(),
            })
        })
        .collect()
}

/// 从 `docker ps` 的 Status 文本推导状态（旧版模板没有 `.State`）。
fn container_state(status: &str) -> String {
    let word = status
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match word.as_str() {
        "up" => "running".into(),
        "removal" => "removing".into(),
        "" => "unknown".into(),
        _ => word,
    }
}

fn parse_images(text: &str) -> Vec<ImageRow> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.trim_end().splitn(6, '\t');
            let id = f.next()?.trim();
            let repository = f.next()?.trim();
            let tag = f.next()?.trim();
            let created = f.next()?.trim();
            let size = f.next().unwrap_or("").trim();
            let created_at = f.next().unwrap_or("").trim();
            if id.is_empty() || repository.is_empty() || tag.is_empty() {
                return None;
            }
            Some(ImageRow {
                id: id.to_string(),
                repository: repository.to_string(),
                tag: tag.to_string(),
                created: created.to_string(),
                created_at: created_at.to_string(),
                size: size.to_string(),
            })
        })
        .collect()
}

/// 解析 `docker network inspect` 的 JSON 数组；空输出表示没有网络。
fn parse_networks(text: &str) -> Result<Vec<NetworkRow>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| {
        format!("无法解析 docker 网络列表：{}", one_line(text))
    })?;
    let items = value
        .as_array()
        .ok_or_else(|| "无法解析 docker 网络列表".to_string())?;
    let mut rows: Vec<NetworkRow> = items.iter().filter_map(network_row).collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(rows)
}

fn network_row(value: &serde_json::Value) -> Option<NetworkRow> {
    let name = value.get("Name").and_then(|v| v.as_str())?.trim();
    if name.is_empty() {
        return None;
    }
    let mut subnets: Vec<String> = Vec::new();
    let mut gateways: Vec<String> = Vec::new();
    if let Some(configs) = value.pointer("/IPAM/Config").and_then(|v| v.as_array()) {
        for conf in configs {
            if let Some(s) = conf.get("Subnet").and_then(|v| v.as_str()) {
                push_trimmed(&mut subnets, s);
            }
            if let Some(g) = conf.get("Gateway").and_then(|v| v.as_str()) {
                push_trimmed(&mut gateways, g);
            }
        }
    }
    Some(NetworkRow {
        id: value
            .get("Id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        name: name.to_string(),
        driver: value
            .get("Driver")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        scope: value
            .get("Scope")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        subnet: subnets.join(", "),
        gateway: gateways.join(", "),
        internal: value
            .get("Internal")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        ipv6: value
            .get("EnableIPv6")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        containers: network_containers(value),
    })
}

fn push_trimmed(out: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty() && !out.iter().any(|v| v == value) {
        out.push(value.to_string());
    }
}

fn network_containers(value: &serde_json::Value) -> Vec<String> {
    let Some(items) = value.get("Containers").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut rows: Vec<(String, String)> = items
        .iter()
        .filter_map(|(id, c)| {
            let name = c.get("Name").and_then(|v| v.as_str()).unwrap_or("").trim();
            let name = if name.is_empty() {
                id.chars().take(12).collect::<String>()
            } else {
                name.to_string()
            };
            Some((name, container_addr(c)))
        })
        .collect();
    rows.sort();
    rows.into_iter()
        .map(|(name, addr)| {
            if addr.is_empty() {
                name
            } else {
                format!("{name} ({addr})")
            }
        })
        .collect()
}

fn container_addr(container: &serde_json::Value) -> String {
    for field in ["IPv4Address", "IPv6Address"] {
        if let Some(addr) = container
            .get(field)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            return addr.to_string();
        }
    }
    for field in ["IPv4Addresses", "IPv6Addresses"] {
        if let Some(addr) = container
            .get(field)
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            return addr.to_string();
        }
    }
    String::new()
}

/// 解析 `docker volume inspect` 的 JSON 数组；空输出表示没有数据卷。
fn parse_volumes(text: &str) -> Result<Vec<VolumeRow>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|_| format!("无法解析 docker 数据卷列表：{}", one_line(text)))?;
    let items = value
        .as_array()
        .ok_or_else(|| "无法解析 docker 数据卷列表".to_string())?;
    let mut rows: Vec<VolumeRow> = items.iter().filter_map(volume_row).collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(rows)
}

fn volume_row(value: &serde_json::Value) -> Option<VolumeRow> {
    let name = value.get("Name").and_then(|v| v.as_str())?.trim();
    if name.is_empty() {
        return None;
    }
    Some(VolumeRow {
        name: name.to_string(),
        driver: value
            .get("Driver")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        scope: value
            .get("Scope")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        mountpoint: value
            .get("Mountpoint")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        created_at: value
            .get("CreatedAt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// 容器操作白名单：start / stop / restart / remove
const CONTAINER_ACTIONS: [&str; 4] = ["start", "stop", "restart", "remove"];
/// 镜像操作白名单：仅 remove
const IMAGE_ACTIONS: [&str; 1] = ["remove"];
/// 网络操作白名单：仅 remove
const NETWORK_ACTIONS: [&str; 1] = ["remove"];
/// 数据卷操作白名单：仅 remove
const VOLUME_ACTIONS: [&str; 1] = ["remove"];

pub(super) fn validate_action(kind: &str, action: &str) -> Result<(), String> {
    let ok = match kind {
        "container" => CONTAINER_ACTIONS.contains(&action),
        "image" => IMAGE_ACTIONS.contains(&action),
        "network" => NETWORK_ACTIONS.contains(&action),
        "volume" => VOLUME_ACTIONS.contains(&action),
        _ => return Err("不支持的 Docker 对象类型".into()),
    };
    if ok {
        Ok(())
    } else {
        Err(format!("不支持的操作：{action}"))
    }
}

/// 容器名 / ID、镜像引用、网络名与数据卷名的字符集校验，禁止前导 `-` 与 shell 元字符。
pub(super) fn validate_target(kind: &str, input: &str) -> Result<String, String> {
    let target = input.trim();
    if target.is_empty() {
        return Err("缺少操作目标".into());
    }
    if target.len() > 255 {
        return Err("操作目标过长".into());
    }
    let mut chars = target.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return Err("操作目标不合法".into());
    }
    let rest_ok = match kind {
        "container" | "network" | "volume" => {
            chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        }
        "image" => chars.all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | ':' | '/' | '@')
        }),
        _ => false,
    };
    if !rest_ok {
        return Err("操作目标包含不允许的字符".into());
    }
    Ok(target.to_string())
}

pub(super) fn build_action_script(
    kind: &str,
    action: &str,
    target: &str,

) -> Result<String, String> {
    validate_action(kind, action)?;
    let target = validate_target(kind, target)?;
    if kind == "network" && ["bridge", "host", "none"].contains(&target.as_str()) {
        return Err("默认网络 bridge / host / none 不可删除".into());
    }
    let cmd = match (kind, action) {
        ("container", "start") => "start",
        ("container", "stop") => "stop",
        ("container", "restart") => "restart",
        ("container", "remove") => "rm",
        ("image", "remove") => "rmi",
        ("network", "remove") => "network rm",
        ("volume", "remove") => "volume rm",
        _ => return Err("不支持的操作".into()),
    };
    Ok(format!(
        r#"{prelude}err=$("$DOCKER" {cmd} {target_q} 2>&1)
rc=$?
if [ $rc -ne 0 ]; then
  {case}
  exit 5
fi
echo '{ok}'
"#,
        prelude = prelude(),
        cmd = cmd,
        target_q = sq(&target),
        case = permission_case(),
        ok = ACTION_OK,
    ))
}

/// 操作结果：成功返回 Ok，失败返回可读错误（docker 原文只取摘要）。
pub(super) fn action_result(out: &ExecOutput) -> Result<(), String> {
    if out.timed_out {
        return Err("Docker 操作超时，请稍后重试".into());
    }
    if out.output.contains(DOCKER_PERMISSION_DENIED) {
        return Err(DOCKER_PERMISSION_DENIED.into());
    }
    if out.output.contains(ACTION_OK) {
        return Ok(());
    }
    if out.output.contains(DOCKER_MISSING) {
        return Err("远端未安装 docker CLI".into());
    }
    for marker in [ACTION_FAILED, DOCKER_ERROR] {
        if let Some((_, rest)) = out.output.split_once(marker) {
            let detail = one_line(rest);
            return Err(if detail.is_empty() {
                format!(
                    "Docker 操作失败（退出码 {:?}）",
                    out.exit_code.unwrap_or(-1)
                )
            } else {
                format!("Docker 操作失败：{detail}")
            });
        }
    }
    Err(format!(
        "Docker 操作失败（退出码 {:?}）",
        out.exit_code.unwrap_or(-1)
    ))
}

/* =========================================================
 *  自定义网络
 * ========================================================= */

/// 可创建的驱动白名单：host / none 无自定义配置意义，交给默认网络
const NETWORK_DRIVERS: [&str; 4] = ["bridge", "overlay", "macvlan", "ipvlan"];

#[derive(Debug, Clone, PartialEq)]
pub(super) struct NetworkSpec {
    pub name: String,
    pub driver: String,
    pub subnet: Option<String>,
    pub gateway: Option<String>,
}

/// 网络名称：字母或数字开头，仅 `_.-` 与字母数字，最长 64。
pub(super) fn validate_network_name(input: &str) -> Result<String, String> {
    let name = input.trim();
    if name.is_empty() {
        return Err("请填写网络名称".into());
    }
    if name.len() > 64 {
        return Err("网络名称不能超过 64 个字符".into());
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return Err("网络名称需以字母或数字开头".into());
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) {
        return Err("网络名称只能包含字母、数字与 _ . -".into());
    }
    Ok(name.to_string())
}

/// 子网 CIDR 校验并归一化（`ip/prefix`），网关单独校验 IP 并要求先填子网。
pub(super) fn validate_network_spec(
    name: &str,
    driver: &str,
    subnet: Option<&str>,
    gateway: Option<&str>,
) -> Result<NetworkSpec, String> {
    let name = validate_network_name(name)?;
    let driver = driver.trim().to_ascii_lowercase();
    if !NETWORK_DRIVERS.contains(&driver.as_str()) {
        return Err("不支持的驱动，可选 bridge / overlay / macvlan / ipvlan".into());
    }
    let subnet = subnet
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(validate_cidr)
        .transpose()?;
    let gateway = gateway
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|g| {
            g.parse::<std::net::IpAddr>()
                .map(|ip| ip.to_string())
                .map_err(|_| "网关地址不是合法 IP".to_string())
        })
        .transpose()?;
    if gateway.is_some() && subnet.is_none() {
        return Err("填写网关前需先填写子网".into());
    }
    Ok(NetworkSpec {
        name,
        driver,
        subnet,
        gateway,
    })
}

fn validate_cidr(input: &str) -> Result<String, String> {
    let input = input.trim();
    let (addr, prefix) = input
        .split_once('/')
        .ok_or_else(|| "子网需为 CIDR 格式，如 172.20.0.0/16".to_string())?;
    let ip: std::net::IpAddr = addr
        .parse()
        .map_err(|_| "子网地址不是合法 IP".to_string())?;
    let prefix: u8 = prefix
        .parse()
        .map_err(|_| "子网前缀不合法".to_string())?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    if prefix > max {
        return Err("子网前缀超出范围".into());
    }
    Ok(format!("{ip}/{prefix}"))
}

pub(super) fn build_network_create_script(spec: &NetworkSpec) -> String {
    let mut cmd = format!("network create --driver {}", sq(&spec.driver));
    if let Some(subnet) = &spec.subnet {
        cmd.push_str(&format!(" --subnet {}", sq(subnet)));
    }
    if let Some(gateway) = &spec.gateway {
        cmd.push_str(&format!(" --gateway {}", sq(gateway)));
    }
    cmd.push(' ');
    cmd.push_str(&sq(&spec.name));
    format!(
        r#"{prelude}err=$("$DOCKER" {cmd} 2>&1)
rc=$?
if [ $rc -ne 0 ]; then
  {case}
  exit 5
fi
echo '{ok}'
"#,
        prelude = prelude(),
        cmd = cmd,
        case = permission_case(),
        ok = ACTION_OK,
    )
}

/* =========================================================
 *  数据卷
 * ========================================================= */

#[derive(Debug, Clone, PartialEq)]
pub(super) struct VolumeSpec {
    pub name: String,
    pub driver: String,
}

/// 数据卷名称：字母或数字开头，仅 `_.-` 与字母数字，最长 255。
pub(super) fn validate_volume_name(input: &str) -> Result<String, String> {
    let name = input.trim();
    if name.is_empty() {
        return Err("请填写数据卷名称".into());
    }
    if name.len() > 255 {
        return Err("数据卷名称不能超过 255 个字符".into());
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return Err("数据卷名称需以字母或数字开头".into());
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) {
        return Err("数据卷名称只能包含字母、数字与 _ . -".into());
    }
    Ok(name.to_string())
}

/// 卷驱动可来自插件，不穷举；留空按 local，名称只做字符集校验。
pub(super) fn validate_volume_spec(name: &str, driver: &str) -> Result<VolumeSpec, String> {
    let name = validate_volume_name(name)?;
    let trimmed = driver.trim().to_ascii_lowercase();
    let driver = if trimmed.is_empty() {
        "local".to_string()
    } else {
        trimmed
    };
    let ok = driver.len() <= 64
        && driver
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && driver
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !ok {
        return Err("数据卷驱动只能包含字母、数字与 _ . -".into());
    }
    Ok(VolumeSpec { name, driver })
}

pub(super) fn build_volume_create_script(spec: &VolumeSpec) -> String {
    format!(
        r#"{prelude}err=$("$DOCKER" volume create --driver {driver_q} {name_q} 2>&1)
rc=$?
if [ $rc -ne 0 ]; then
  {case}
  exit 5
fi
echo '{ok}'
"#,
        prelude = prelude(),
        driver_q = sq(&spec.driver),
        name_q = sq(&spec.name),
        case = permission_case(),
        ok = ACTION_OK,
    )
}

/* =========================================================
 *  容器日志
 * ========================================================= */

/// tail 行数夹取：10~5000，异常值直接拒绝（前端只会传预设值）
pub(super) fn validate_tail(tail: u32) -> Result<u32, String> {
    if (10..=5000).contains(&tail) {
        Ok(tail)
    } else {
        Err("日志行数需在 10~5000 之间".into())
    }
}

pub(super) fn build_logs_script(
    container: &str,
    tail: u32,
    timestamps: bool,

) -> Result<String, String> {
    let container = validate_target("container", container)?;
    let tail = validate_tail(tail)?;
    let ts = if timestamps { " --timestamps" } else { "" };
    Ok(format!(
        r#"{prelude}out=$("$DOCKER" logs --tail {tail}{ts} {name_q} 2>&1)
rc=$?
if [ $rc -ne 0 ]; then
  case "$out" in
    *[Pp]ermission*denied*) echo '{denied}' ;;
    *) echo '{failed}'; printf '%s\n' "$out" ;;
  esac
  exit 5
fi
printf '{marker}%s\n' "${{#out}}"
printf '%s' "$out"
"#,
        prelude = prelude(),
        tail = tail,
        ts = ts,
        name_q = sq(&container),
        denied = DOCKER_PERMISSION_DENIED,
        failed = ACTION_FAILED,
        marker = LOGS_MARKER,
    ))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogsResult {
    pub output: String,
    /// 超出回传上限，只保留了末尾
    pub truncated: bool,
}

pub(super) fn parse_logs(output: &ExecOutput) -> Result<LogsResult, String> {
    if output.timed_out {
        return Err("读取容器日志超时（可减小行数后重试）".into());
    }
    if output.output.contains(DOCKER_PERMISSION_DENIED) {
        return Err(DOCKER_PERMISSION_DENIED.into());
    }
    if output.output.contains(DOCKER_MISSING) {
        return Err("远端未安装 docker CLI".into());
    }
    let Some((_, rest)) = output.output.split_once(LOGS_MARKER) else {
        if let Some((_, detail)) = output.output.split_once(ACTION_FAILED) {
            return Err(format!("读取容器日志失败：{}", one_line(detail)));
        }
        return Err(format!(
            "读取容器日志失败（退出码 {:?}）：{}",
            output.exit_code.unwrap_or(-1),
            one_line(&output.output)
        ));
    };
    let (len_line, body) = rest.split_once('\n').unwrap_or((rest, ""));
    let reported: usize = len_line.trim().parse().unwrap_or(0);
    let total = body.chars().count();
    let mut truncated = reported > MAX_LOG_CHARS;
    let mut text = body.to_string();
    if total > MAX_LOG_CHARS {
        text = body.chars().skip(total - MAX_LOG_CHARS).collect();
        truncated = true;
    }
    Ok(LogsResult {
        output: text,
        truncated,
    })
}

/* =========================================================
 *  镜像导入 / 导出
 * ========================================================= */

/// 远端文件路径：绝对路径、无控制字符、不含 `..`，允许空格（写入时整体加引号）
pub(super) fn validate_remote_path(input: &str) -> Result<String, String> {
    let path = input.trim();
    if path.is_empty() {
        return Err("请填写远端文件路径".into());
    }
    if path.len() > 512 {
        return Err("远端文件路径过长".into());
    }
    if !path.starts_with('/') {
        return Err("请填写绝对路径，例如 /tmp/image.tar".into());
    }
    if path.chars().any(|c| c.is_control()) {
        return Err("路径包含不允许的控制字符".into());
    }
    if path.split('/').any(|seg| seg == "..") {
        return Err("路径不能包含 ..".into());
    }
    if path.ends_with('/') {
        return Err("请填写文件名，而不是目录".into());
    }
    Ok(path.to_string())
}

/// 本地镜像文件路径：非空、绝对路径、无控制字符；允许空格与中文，不允许以分隔符结尾
pub(super) fn validate_local_path(input: &str) -> Result<String, String> {
    let path = input.trim();
    if path.is_empty() {
        return Err("请选择本地文件路径".into());
    }
    if path.len() > 4096 {
        return Err("本地文件路径过长".into());
    }
    if path.chars().any(|c| c.is_control()) {
        return Err("路径包含不允许的控制字符".into());
    }
    if !std::path::Path::new(path).is_absolute() {
        return Err("请选择绝对路径".into());
    }
    if path.ends_with('/') || path.ends_with('\\') {
        return Err("请选择文件名，而不是目录".into());
    }
    Ok(path.to_string())
}

/// 远端临时 tar 路径：`/tmp/rhost-<标签>-<纳秒时间戳>-<序号>.tar`。
/// 导入/导出到本地时中转文件，时间戳 + 进程内序号保证并发与残留场景下不撞名。
pub(super) fn temp_tar_path(label: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let safe: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    let safe = safe.trim_matches('_');
    let label = if safe.is_empty() { "image" } else { safe };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("/tmp/rhost-{label}-{nanos}-{seq:x}.tar")
}

pub(super) fn build_export_script(
    image: &str,
    path: &str,
    overwrite: bool,

) -> Result<String, String> {
    let image = validate_target("image", image)?;
    let path = validate_remote_path(path)?;
    Ok(format!(
        r#"{prelude}if [ -e {path_q} ] && [ {overwrite} -ne 1 ]; then
  echo '{exists}'
  exit 6
fi
out=$("$DOCKER" save -o {path_q} {image_q} 2>&1)
rc=$?
if [ $rc -ne 0 ]; then
  case "$out" in
    *[Pp]ermission*denied*) echo '{denied}' ;;
    *) echo '{failed}'; printf '%s\n' "$out" ;;
  esac
  exit 5
fi
size=$(wc -c < {path_q} 2>/dev/null | tr -d ' ')
printf '%s\n' '{ok}'
printf 'SIZE=%s\n' "$size"
"#,
        prelude = prelude(),
        path_q = sq(&path),
        overwrite = if overwrite { 1 } else { 0 },
        exists = EXPORT_EXISTS,
        image_q = sq(&image),
        denied = DOCKER_PERMISSION_DENIED,
        failed = EXPORT_FAILED,
        ok = EXPORT_OK,
    ))
}

/// 导出结果：返回人读大小；文件已存在用独立错误码交给前端确认覆盖
pub(super) fn export_result(output: &ExecOutput) -> Result<String, String> {
    if output.timed_out {
        return Err("导出超时，镜像可能过大或磁盘较慢".into());
    }
    if output.output.contains(DOCKER_PERMISSION_DENIED) {
        return Err(DOCKER_PERMISSION_DENIED.into());
    }
    if output.output.contains(EXPORT_EXISTS) {
        return Err(EXPORT_EXISTS.into());
    }
    if output.output.contains(EXPORT_OK) {
        let size = output
            .output
            .lines()
            .find_map(|l| l.strip_prefix("SIZE="))
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        return Ok(human_size(size));
    }
    if output.output.contains(DOCKER_MISSING) {
        return Err("远端未安装 docker CLI".into());
    }
    if let Some((_, rest)) = output.output.split_once(EXPORT_FAILED) {
        let detail = one_line(rest);
        return Err(if detail.is_empty() {
            "导出失败".into()
        } else {
            format!("导出失败：{detail}")
        });
    }
    Err(format!(
        "导出失败（退出码 {:?}）：{}",
        output.exit_code.unwrap_or(-1),
        one_line(&output.output)
    ))
}

pub(super) fn build_import_script(
    path: &str,
    tag: Option<&str>,

) -> Result<String, String> {
    let path = validate_remote_path(path)?;
    let tag_block = match tag {
        Some(t) => format!(
            r#"ref=$(printf '%s\n' "$out" | sed -n 's/^Loaded image: //p' | head -n 1)
if [ -z "$ref" ]; then
  ref=$(printf '%s\n' "$out" | sed -n 's/^Loaded image ID: //p' | head -n 1)
fi
if [ -z "$ref" ]; then
  echo '{tag_failed}'
  echo '无法从导入输出中识别镜像，未打标签'
  exit 7
fi
tag_out=$("$DOCKER" tag "$ref" {tag_q} 2>&1)
if [ $? -ne 0 ]; then
  echo '{tag_failed}'
  printf '%s\n' "$tag_out"
  exit 7
fi
printf 'TAGGED=%s\n' {tag_q}
"#,
            tag_failed = TAG_FAILED,
            tag_q = sq(t),
        ),
        None => String::new(),
    };
    Ok(format!(
        r#"{prelude}if [ ! -f {path_q} ]; then
  echo '{missing}'
  exit 6
fi
out=$("$DOCKER" load -i {path_q} 2>&1)
rc=$?
if [ $rc -ne 0 ]; then
  case "$out" in
    *[Pp]ermission*denied*) echo '{denied}' ;;
    *) echo '{failed}'; printf '%s\n' "$out" ;;
  esac
  exit 5
fi
{tag_block}printf '%s\n' '{ok}'
printf '%s\n' "$out"
"#,
        prelude = prelude(),
        path_q = sq(&path),
        missing = IMPORT_MISSING,
        denied = DOCKER_PERMISSION_DENIED,
        failed = IMPORT_FAILED,
        tag_block = tag_block,
        ok = IMPORT_OK,
    ))
}

/// 自定义标签：完整引用（含 `/`、`:` 或 `@`）按原样校验；
/// 只给标签名时沿用被拉取镜像的仓库名，例如仓库 `nginx` + `v2` → `nginx:v2`。
pub(super) fn validate_custom_tag(
    pulled: &str,
    raw: Option<&str>,
) -> Result<Option<String>, String> {
    let raw = match raw.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => v,
        None => return Ok(None),
    };
    if raw.contains('/') || raw.contains(':') || raw.contains('@') {
        return Ok(Some(validate_target("image", raw)?));
    }
    let tag_ok = raw.len() <= 128
        && raw
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !tag_ok {
        return Err("标签只能包含字母、数字、_ . -，且不能以符号开头".into());
    }
    // 去掉 digest 与末尾 tag，仅保留仓库部分；`host:5000/repo` 的冒号不属于 tag
    let base = pulled.split('@').next().unwrap_or(pulled);
    let repo = match base.rfind(':') {
        Some(idx) if !base[idx + 1..].contains('/') => &base[..idx],
        _ => base,
    };
    Ok(Some(validate_target("image", &format!("{repo}:{raw}"))?))
}

/// 导入时的自定义标签：无法从 tar 推断仓库名，必须给完整引用（如 `nginx:my-tag`）。
pub(super) fn validate_import_tag(raw: Option<&str>) -> Result<Option<String>, String> {
    let raw = match raw.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => v,
        None => return Ok(None),
    };
    if !raw.contains(':') && !raw.contains('/') && !raw.contains('@') {
        return Err("导入时自定义标签需填写完整镜像引用，例如 nginx:my-tag".into());
    }
    Ok(Some(validate_target("image", raw)?))
}

/// 导入结果：提取 `Loaded image:` 行作为摘要；打了标签时附加目标引用。
pub(super) fn import_result(output: &ExecOutput) -> Result<String, String> {
    if output.timed_out {
        return Err("导入超时，镜像包可能过大或磁盘较慢".into());
    }
    if output.output.contains(DOCKER_PERMISSION_DENIED) {
        return Err(DOCKER_PERMISSION_DENIED.into());
    }
    if output.output.contains(IMPORT_MISSING) {
        return Err("远端文件不存在，请确认路径".into());
    }
    if let Some((_, rest)) = output.output.split_once(TAG_FAILED) {
        let detail = one_line(rest);
        return Err(if detail.is_empty() {
            "镜像已导入，但打标签失败".into()
        } else {
            format!("镜像已导入，但打标签失败：{detail}")
        });
    }
    if output.output.contains(IMPORT_OK) {
        let loaded: Vec<&str> = output
            .output
            .lines()
            .filter(|l| l.trim_start().starts_with("Loaded image"))
            .collect();
        let tagged = output
            .output
            .lines()
            .find_map(|l| l.strip_prefix("TAGGED="))
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let mut msg = if loaded.is_empty() {
            "镜像已导入".to_string()
        } else {
            loaded.join("；")
        };
        if let Some(tag) = tagged {
            msg.push_str("，已打标签 ");
            msg.push_str(tag);
        }
        return Ok(msg);
    }
    if output.output.contains(DOCKER_MISSING) {
        return Err("远端未安装 docker CLI".into());
    }
    if let Some((_, rest)) = output.output.split_once(IMPORT_FAILED) {
        let detail = one_line(rest);
        return Err(if detail.is_empty() {
            "导入失败".into()
        } else {
            format!("导入失败：{detail}")
        });
    }
    Err(format!(
        "导入失败（退出码 {:?}）：{}",
        output.exit_code.unwrap_or(-1),
        one_line(&output.output)
    ))
}

/* =========================================================
 *  镜像拉取（自定义 URL + 可选标签）
 * ========================================================= */

pub(super) fn build_pull_script(
    image: &str,
    tag: Option<&str>,

) -> Result<String, String> {
    let image = validate_target("image", image)?;
    let tag_block = match tag {
        Some(t) => format!(
            r#"if [ -n "$ref" ]; then
  tag_out=$("$DOCKER" tag "$ref" {tag_q} 2>&1)
  if [ $? -ne 0 ]; then
    echo '{tag_failed}'
    printf '%s\n' "$tag_out"
    exit 7
  fi
  printf 'TAGGED=%s\n' {tag_q}
fi
"#,
            tag_failed = TAG_FAILED,
            tag_q = sq(t),
        ),
        None => String::new(),
    };
    Ok(format!(
        r#"{prelude}out=$("$DOCKER" pull {image_q} 2>&1)
rc=$?
if [ $rc -ne 0 ]; then
  case "$out" in
    *[Pp]ermission*denied*) echo '{denied}' ;;
    *) echo '{failed}'; printf '%s\n' "$out" ;;
  esac
  exit 5
fi
ref={image_q}
printf 'PULLED=%s\n' "$ref"
{tag_block}printf '%s\n' '{ok}'
"#,
        prelude = prelude(),
        image_q = sq(&image),
        denied = DOCKER_PERMISSION_DENIED,
        failed = PULL_FAILED,
        tag_block = tag_block,
        ok = PULL_OK,
    ))
}

/// 拉取结果：成功给出镜像引用，打标签失败单独提示（镜像已在本地）。
pub(super) fn pull_result(output: &ExecOutput) -> Result<String, String> {
    if output.timed_out {
        return Err("拉取超时，镜像可能过大或网络较慢".into());
    }
    if output.output.contains(DOCKER_PERMISSION_DENIED) {
        return Err(DOCKER_PERMISSION_DENIED.into());
    }
    if let Some((_, rest)) = output.output.split_once(TAG_FAILED) {
        let detail = one_line(rest);
        return Err(if detail.is_empty() {
            "镜像已拉取，但打标签失败".into()
        } else {
            format!("镜像已拉取，但打标签失败：{detail}")
        });
    }
    if output.output.contains(PULL_OK) {
        let pulled = output
            .output
            .lines()
            .find_map(|l| l.strip_prefix("PULLED="))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or("镜像");
        let tagged = output
            .output
            .lines()
            .find_map(|l| l.strip_prefix("TAGGED="))
            .map(str::trim)
            .filter(|v| !v.is_empty());
        return Ok(match tagged {
            Some(tag) => format!("镜像 {pulled} 已拉取，并打标签 {tag}"),
            None => format!("镜像 {pulled} 已拉取"),
        });
    }
    if output.output.contains(DOCKER_MISSING) {
        return Err("远端未安装 docker CLI".into());
    }
    if let Some((_, rest)) = output.output.split_once(PULL_FAILED) {
        let detail = one_line(rest);
        return Err(if detail.is_empty() {
            "拉取失败".into()
        } else {
            format!("拉取失败：{detail}")
        });
    }
    Err(format!(
        "拉取失败（退出码 {:?}）：{}",
        output.exit_code.unwrap_or(-1),
        one_line(&output.output)
    ))
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/* =========================================================
 *  Docker 配置（daemon.json 可视化：数据目录 + 镜像源）
 * ========================================================= */

/// 终端 / 配置读取前的探活脚本：只验证 CLI 与 socket 权限
pub(super) fn build_probe_script() -> String {
    format!(
        r#"{prelude}err=$("$DOCKER" info 2>&1 >/dev/null)
if [ $? -ne 0 ]; then
  {case}
  exit 5
fi
echo '{ready}'
"#,
        prelude = prelude(),
        case = permission_case(),
        ready = TERMINAL_READY,
    )
}

pub(super) fn probe_result(output: &ExecOutput) -> Result<(), String> {
    if output.timed_out {
        return Err("Docker 检测超时".into());
    }
    if output.output.contains(DOCKER_PERMISSION_DENIED) {
        return Err(DOCKER_PERMISSION_DENIED.into());
    }
    if output.output.contains(TERMINAL_READY) {
        return Ok(());
    }
    if output.output.contains(DOCKER_MISSING) {
        return Err("远端未安装 docker CLI".into());
    }
    if output.output.contains(DOCKER_DAEMON_DOWN) {
        return Err("无法连接 Docker 守护进程，请确认 docker 服务已启动".into());
    }
    if let Some((_, rest)) = output.output.split_once(DOCKER_ERROR) {
        return Err(format!("Docker 检测失败：{}", one_line(rest)));
    }
    Err(format!(
        "Docker 检测失败（退出码 {:?}）",
        output.exit_code.unwrap_or(-1)
    ))
}

/// 终端提权方式：免密 sudo / 需要密码（先 `sudo -v` 预热票据）/ 不需要
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SudoMode {
    None,
    Passwordless,
    Password,
}

/// 交互式 exec 命令：优先容器内 bash，回退 sh；sudo 密码由 PTY 首行输入，
/// 用 `sudo -v` 预热本 TTY 的票据，随后的 `sudo -n` 不再读 stdin。
pub(super) fn build_terminal_command(
    container: &str,
    sudo: SudoMode,
) -> Result<String, String> {
    let container = validate_target("container", container)?;
    let prefix = match sudo {
        SudoMode::None => "",
        SudoMode::Passwordless | SudoMode::Password => "sudo -n ",
    };
    let warmup = match sudo {
        SudoMode::Password => "sudo -S -p '' -v || exit 1\n",
        _ => "",
    };
    let name_q = sq(&container);
    Ok(format!(
        "{warmup}exec {prefix}'docker' exec -it {name_q} sh -c 'if command -v bash >/dev/null 2>&1; then exec bash; else exec sh; fi'\n"
    ))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DockerConfig {
    pub version: String,
    /// 原始 daemon.json；None 表示文件不存在
    pub daemon_json: Option<String>,
    pub config_path: String,
    pub registry_mirrors: Vec<String>,
    /// daemon.json 里的 `data-root`；None 表示使用 Docker 默认目录
    pub data_root: Option<String>,
}

pub(super) fn build_config_get_script() -> String {
    format!(
        r#"{prelude}printf '@@VER@@\n'
"$DOCKER" --version 2>&1
printf '@@DAEMON@@\n'
if [ -f {daemon} ]; then
  if ! cat {daemon} 2>/dev/null; then echo '{unreadable}'; fi
else
  echo '@@NOFILE@@'
fi
printf '\n@@END@@\n'
"#,
        prelude = prelude(),
        daemon = DAEMON_JSON_PATH,
        unreadable = DAEMON_UNREADABLE,
    )
}

pub(super) fn parse_config(output: &str) -> Result<DockerConfig, String> {
    if output.contains(DOCKER_MISSING) {
        return Err("远端未安装 docker CLI".into());
    }
    if output.contains(DAEMON_UNREADABLE) {
        return Err("无法读取 /etc/docker/daemon.json（权限不足）".into());
    }
    let Some((_, after_ver)) = output.split_once("@@VER@@") else {
        return Err(format!("读取 Docker 配置失败：{}", one_line(output)));
    };
    let Some((ver_part, after_daemon)) = after_ver.split_once("@@DAEMON@@") else {
        return Err("读取 Docker 配置失败：daemon.json 段缺失".into());
    };
    let daemon_part = after_daemon
        .split_once("@@END@@")
        .map(|(v, _)| v)
        .unwrap_or(after_daemon);
    let daemon_trim = daemon_part.trim();
    let (daemon_json, registry_mirrors, data_root) =
        if daemon_trim.is_empty() || daemon_trim == "@@NOFILE@@" {
            (None, Vec::new(), None)
        } else {
            let value: serde_json::Value = serde_json::from_str(daemon_trim)
                .map_err(|e| format!("远端 daemon.json 不是合法 JSON，暂不能在此编辑：{e}"))?;
            let mirrors = value
                .get("registry-mirrors")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let data_root = value
                .get("data-root")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            (Some(daemon_trim.to_string()), mirrors, data_root)
        };
    Ok(DockerConfig {
        version: ver_part.trim().to_string(),
        daemon_json,
        config_path: DAEMON_JSON_PATH.to_string(),
        registry_mirrors,
        data_root,
    })
}

/// 镜像源地址校验：必须是 http(s) URL，禁止明显的手误与超长输入
pub(super) fn validate_mirrors(input: &[String]) -> Result<Vec<String>, String> {
    if input.len() > MAX_MIRRORS {
        return Err(format!("镜像源最多 {MAX_MIRRORS} 个"));
    }
    let mut out: Vec<String> = Vec::new();
    for raw in input {
        let m = raw.trim();
        if m.is_empty() {
            continue;
        }
        if m.len() > 255 {
            return Err("镜像源地址过长".into());
        }
        if !(m.starts_with("https://") || m.starts_with("http://")) {
            return Err(format!("镜像源需以 http:// 或 https:// 开头：{m}"));
        }
        if m.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '"' | '\'' | '\\' | '`' | '$' | '<' | '>' | '|')
        }) {
            return Err(format!("镜像源包含不允许的字符：{m}"));
        }
        let rest = m.split_once("://").map(|(_, r)| r).unwrap_or("");
        if rest.is_empty() || rest.starts_with('/') {
            return Err(format!("镜像源缺少主机名：{m}"));
        }
        if !out.iter().any(|v| v == m) {
            out.push(m.to_string());
        }
    }
    Ok(out)
}

/// 数据目录校验：空值表示使用 Docker 默认目录（保存时移除 `data-root`）。
pub(super) fn validate_data_root(raw: &str) -> Result<Option<String>, String> {
    let p = raw.trim();
    if p.is_empty() {
        return Ok(None);
    }
    if !p.starts_with('/') {
        return Err("数据目录需为绝对路径，例如 /home/docker1".into());
    }
    if p == "/" {
        return Err("数据目录不能是根目录".into());
    }
    if p.len() > MAX_DATA_ROOT {
        return Err("数据目录路径过长".into());
    }
    if p.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("数据目录不能包含空白或控制字符".into());
    }
    if p.split('/').any(|seg| seg == "." || seg == "..") {
        return Err("数据目录不能包含 . 或 .. 路径段".into());
    }
    Ok(Some(p.trim_end_matches('/').to_string()))
}

/// 校验前端保存的 daemon.json 全文：顶层必须是 JSON 对象；
/// `data-root` / `registry-mirrors` 若存在则按表单规则校验并规范化。
pub(super) fn validate_daemon_json(raw: &str) -> Result<String, String> {
    if raw.len() > MAX_DAEMON_JSON {
        return Err(format!("daemon.json 内容过长（上限 {MAX_DAEMON_JSON} 字节）"));
    }
    let text = raw.trim();
    let text = if text.is_empty() { "{}" } else { text };
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| format!("daemon.json 不是合法 JSON，未做任何修改：{e}"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| "daemon.json 顶层必须是 JSON 对象，未做任何修改".to_string())?;
    let mut out = obj.clone();
    if let Some(entry) = out.get("registry-mirrors").cloned() {
        let arr = entry
            .as_array()
            .ok_or_else(|| "registry-mirrors 必须是字符串数组".to_string())?;
        let items: Vec<String> = arr
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "registry-mirrors 必须是字符串数组".to_string())
            })
            .collect::<Result<_, _>>()?;
        let mirrors = validate_mirrors(&items)?;
        if mirrors.is_empty() {
            out.remove("registry-mirrors");
        } else {
            out.insert("registry-mirrors".into(), json!(mirrors));
        }
    }
    if let Some(entry) = out.get("data-root").cloned() {
        let raw_path = entry
            .as_str()
            .ok_or_else(|| "data-root 必须是字符串".to_string())?;
        match validate_data_root(raw_path)? {
            Some(p) => {
                out.insert("data-root".into(), json!(p));
            }
            None => {
                out.remove("data-root");
            }
        }
    }
    serde_json::to_string_pretty(&serde_json::Value::Object(out))
        .map_err(|e| format!("生成 daemon.json 失败：{e}"))
}

pub(super) fn build_config_save_script(daemon_json: &str) -> String {
    format!(
        r#"{prelude}DAEMON={daemon}
mkdir -p /etc/docker 2>/dev/null
if [ -f "$DAEMON" ]; then
  cp -a "$DAEMON" "$DAEMON.bak.$(date +%Y%m%d%H%M%S)" || {{ echo '{failed}'; exit 5; }}
fi
printf '%s' {json_q} > "$DAEMON.tmp" || {{ echo '{failed}'; exit 5; }}
mv "$DAEMON.tmp" "$DAEMON" || {{ echo '{failed}'; exit 5; }}
if command -v systemctl >/dev/null 2>&1; then
  restart_out=$(systemctl restart docker 2>&1)
else
  restart_out=$(service docker restart 2>&1)
fi
rc=$?
if [ $rc -ne 0 ]; then
  latest=$(ls -t "$DAEMON".bak.* 2>/dev/null | head -n 1)
  if [ -n "$latest" ]; then cp -a "$latest" "$DAEMON"; fi
  echo '{restart_failed}'
  printf '%s\n' "$restart_out"
  exit 5
fi
echo '{ok}'
"#,
        prelude = prelude(),
        daemon = DAEMON_JSON_PATH,
        failed = CONFIG_FAILED,
        json_q = sq(daemon_json),
        restart_failed = CONFIG_RESTART_FAILED,
        ok = CONFIG_OK,
    )
}

pub(super) fn config_save_result(output: &ExecOutput) -> Result<(), String> {
    if output.timed_out {
        return Err("重启 Docker 服务超时，请稍后在远端确认服务状态".into());
    }
    if output.output.contains(DOCKER_PERMISSION_DENIED) {
        return Err(DOCKER_PERMISSION_DENIED.into());
    }
    if output.output.contains(CONFIG_OK) {
        return Ok(());
    }
    if output.output.contains(DOCKER_MISSING) {
        return Err("远端未安装 docker CLI".into());
    }
    if let Some((_, rest)) = output.output.split_once(CONFIG_RESTART_FAILED) {
        let detail = one_line(rest);
        return Err(if detail.is_empty() {
            "Docker 服务重启失败，已还原备份".into()
        } else {
            format!("Docker 服务重启失败，已还原备份：{detail}")
        });
    }
    if let Some((_, rest)) = output.output.split_once(CONFIG_FAILED) {
        let detail = one_line(rest);
        return Err(if detail.is_empty() {
            "写入 daemon.json 失败".into()
        } else {
            format!("写入 daemon.json 失败：{detail}")
        });
    }
    Err(format!(
        "Docker 配置保存失败（退出码 {:?}）：{}",
        output.exit_code.unwrap_or(-1),
        one_line(&output.output)
    ))
}

fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > MAX_DETAIL {
        let truncated: String = joined.chars().take(MAX_DETAIL).collect();
        format!("{truncated}…")
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(text: &str, exit: i32) -> ExecOutput {
        ExecOutput {
            output: text.to_string(),
            exit_code: Some(exit),
            timed_out: false,
        }
    }

    const SAMPLE: &str = "\
@@DIAG@@
UID=0
DOCKER_VERSION=19.03.13
COMPOSE_VERSION=1.25.0
@@CONTAINERS@@
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\tweb-1\tnginx:latest\tUp 3 days\t0.0.0.0:8080->80/tcp, [::]:8080->80/tcp\t2026-11-01 09:00:00 +0800 CST
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\tpg\tpostgres:16\tExited (0) 2 hours ago\t\t2026-10-30 12:00:00 +0800 CST
@@IMAGES@@
sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\tnginx\tlatest\t2 weeks ago\t187MB\t2026-10-20 08:00:00 +0800 CST
sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd\t<none>\t<none>\t3 days ago\t120MB\t2026-11-02 08:00:00 +0800 CST
@@NETWORKS@@
[{\"Name\":\"bridge\",\"Id\":\"f1f1f1f1f1f1\",\"Scope\":\"local\",\"Driver\":\"bridge\",\"EnableIPv6\":false,\"Internal\":false,\"IPAM\":{\"Config\":[{\"Subnet\":\"172.17.0.0/16\",\"Gateway\":\"172.17.0.1\"}]},\"Containers\":{\"c1ffffff\":{\"Name\":\"web-1\",\"IPv4Address\":\"172.17.0.2/16\"}}}]
@@VOLUMES@@
[{\"CreatedAt\":\"2026-10-01T08:00:00+08:00\",\"Driver\":\"local\",\"Labels\":null,\"Mountpoint\":\"/var/lib/docker/volumes/app-data/_data\",\"Name\":\"app-data\",\"Options\":null,\"Scope\":\"local\"},{\"CreatedAt\":\"2026-10-02T09:30:00+08:00\",\"Driver\":\"local\",\"Mountpoint\":\"/var/lib/docker/volumes/pgdata/_data\",\"Name\":\"pgdata\",\"Scope\":\"local\"}]
@@END@@
";

    #[test]
    fn parses_containers_and_images() {
        let list = parse_list(SAMPLE).unwrap();
        assert_eq!(list.uid, Some(0));
        assert_eq!(list.docker_version.as_deref(), Some("19.03.13"));
        assert_eq!(list.compose_version.as_deref(), Some("1.25.0"));
        assert_eq!(list.containers.len(), 2);
        assert_eq!(list.containers[0].name, "web-1");
        assert_eq!(list.containers[0].state, "running");
        assert_eq!(list.containers[0].ports, "0.0.0.0:8080->80/tcp, [::]:8080->80/tcp");
        assert_eq!(list.containers[1].status, "Exited (0) 2 hours ago");
        assert_eq!(list.containers[1].ports, "");
        assert_eq!(list.images.len(), 2);
        assert_eq!(list.images[0].repository, "nginx");
        assert_eq!(list.images[0].size, "187MB");
        assert_eq!(list.images[0].created_at, "2026-10-20 08:00:00 +0800 CST");
        assert_eq!(list.images[1].repository, "<none>");
    }

    #[test]
    fn normalizes_version_output() {
        assert_eq!(clean_version("19.03.13"), Some("19.03.13".into()));
        assert_eq!(clean_version("2.24.5\n"), Some("2.24.5".into()));
        assert_eq!(
            clean_version("docker-compose version 1.25.0, build 0a186604"),
            Some("1.25.0".into())
        );
        assert_eq!(
            clean_version("Docker version 24.0.7, build afdd53b"),
            Some("24.0.7".into())
        );
        assert_eq!(clean_version(""), None);
        assert_eq!(clean_version("  \n"), None);
    }

    #[test]
    fn derives_container_state_from_status() {
        assert_eq!(container_state("Up 3 days"), "running");
        assert_eq!(container_state("Up 2 hours (healthy)"), "running");
        assert_eq!(container_state("Exited (0) 2 hours ago"), "exited");
        assert_eq!(container_state("Paused"), "paused");
        assert_eq!(container_state("Restarting (1) 5 seconds ago"), "restarting");
        assert_eq!(container_state("Created"), "created");
        assert_eq!(container_state("Dead"), "dead");
        assert_eq!(container_state("Removal In Progress"), "removing");
        assert_eq!(container_state(""), "unknown");
    }

    #[test]
    fn parses_empty_lists() {
        let list = parse_list("@@DIAG@@\nUID=1000\n@@CONTAINERS@@\n@@IMAGES@@\n@@NETWORKS@@\n@@END@@\n").unwrap();
        assert!(list.containers.is_empty());
        assert!(list.images.is_empty());
        assert!(list.networks.is_empty());
        assert!(list.volumes.is_empty());
    }

    #[test]
    fn parses_volumes_from_inspect_json() {
        let list = parse_list(SAMPLE).unwrap();
        assert_eq!(list.volumes.len(), 2);
        assert_eq!(list.volumes[0].name, "app-data");
        assert_eq!(list.volumes[0].driver, "local");
        assert_eq!(list.volumes[0].scope, "local");
        assert_eq!(
            list.volumes[0].mountpoint,
            "/var/lib/docker/volumes/app-data/_data"
        );
        assert_eq!(list.volumes[0].created_at, "2026-10-01T08:00:00+08:00");
        assert_eq!(list.volumes[1].name, "pgdata");

        // 未按名称排序、缺少 CreatedAt 的旧版输出
        let multi = r#"@@DIAG@@
@@VOLUMES@@
[{"Name":"zeta","Driver":"nfs","Scope":"global","Mountpoint":"/mnt/z"},{"Name":"alpha","Driver":"local","Scope":"local"}]
@@END@@
"#;
        let list = parse_list(multi).unwrap();
        assert_eq!(list.volumes.len(), 2);
        assert_eq!(list.volumes[0].name, "alpha");
        assert_eq!(list.volumes[0].driver, "local");
        assert_eq!(list.volumes[0].mountpoint, "");
        assert_eq!(list.volumes[0].created_at, "");
        assert_eq!(list.volumes[1].scope, "global");

        let bad = "@@DIAG@@\n@@VOLUMES@@\nError response from daemon: oops\n@@END@@\n";
        assert!(parse_list(bad)
            .unwrap_err()
            .contains("无法解析 docker 数据卷"));
    }

    #[test]
    fn parses_networks_from_inspect_json() {
        let list = parse_list(SAMPLE).unwrap();
        assert_eq!(list.networks.len(), 1);
        let bridge = &list.networks[0];
        assert_eq!(bridge.name, "bridge");
        assert_eq!(bridge.driver, "bridge");
        assert_eq!(bridge.scope, "local");
        assert_eq!(bridge.subnet, "172.17.0.0/16");
        assert_eq!(bridge.gateway, "172.17.0.1");
        assert!(!bridge.internal);
        assert!(!bridge.ipv6);
        assert_eq!(bridge.containers, vec!["web-1 (172.17.0.2/16)"]);

        let multi = r#"@@DIAG@@
@@NETWORKS@@
[{"Name":"web-net","Id":"e2","Scope":"local","Driver":"bridge","EnableIPv6":true,"Internal":true,"IPAM":{"Config":[{"Subnet":"172.20.0.0/16","Gateway":"172.20.0.1"},{"Subnet":"fd00::/64","Gateway":"fd00::1"}]},"Containers":{"z9":{"Name":"b-1","IPv4Addresses":["172.20.0.3/16"]},"a1":{"Name":"a-1","IPv4Address":"172.20.0.2/16"}}},
 {"Name":"alpha","Id":"e1","Driver":"overlay","Scope":"swarm","Internal":false,"EnableIPv6":false,"IPAM":{"Config":[]},"Containers":{}}]
@@END@@
"#;
        let list = parse_list(multi).unwrap();
        assert_eq!(list.networks.len(), 2);
        assert_eq!(list.networks[0].name, "alpha");
        assert_eq!(list.networks[0].driver, "overlay");
        assert_eq!(list.networks[0].scope, "swarm");
        assert!(list.networks[0].subnet.is_empty());
        assert!(list.networks[0].containers.is_empty());
        let web = &list.networks[1];
        assert_eq!(web.subnet, "172.20.0.0/16, fd00::/64");
        assert_eq!(web.gateway, "172.20.0.1, fd00::1");
        assert!(web.internal);
        assert!(web.ipv6);
        assert_eq!(web.containers, vec!["a-1 (172.20.0.2/16)", "b-1 (172.20.0.3/16)"]);

        let bad = "@@DIAG@@\n@@NETWORKS@@\nError response from daemon: oops\n@@END@@\n";
        assert!(parse_list(bad).unwrap_err().contains("无法解析 docker 网络"));
    }

    #[test]
    fn validates_network_specs_and_builds_create_script() {
        assert_eq!(validate_network_name(" app-net_1.0 ").unwrap(), "app-net_1.0");
        assert!(validate_network_name("").is_err());
        assert!(validate_network_name("-net").is_err());
        assert!(validate_network_name("net;id").is_err());

        let spec = validate_network_spec(
            "app-net",
            "Bridge",
            Some("172.20.0.0/16"),
            Some("172.20.0.1"),
        )
        .unwrap();
        assert_eq!(spec.driver, "bridge");
        assert_eq!(spec.subnet.as_deref(), Some("172.20.0.0/16"));
        let script = build_network_create_script(&spec);
        assert!(script.contains(
            "network create --driver 'bridge' --subnet '172.20.0.0/16' --gateway '172.20.0.1' 'app-net'"
        ));
        assert!(script.contains(ACTION_OK));

        let bare = validate_network_spec("net2", "ipvlan", None, None).unwrap();
        assert!(build_network_create_script(&bare)
            .contains("network create --driver 'ipvlan' 'net2'"));

        assert!(validate_network_spec("n", "host", None, None).is_err());
        assert!(validate_network_spec("n", "bridge", Some("172.20.0.0"), None).is_err());
        assert!(validate_network_spec("n", "bridge", Some("172.20.0.0/40"), None).is_err());
        assert!(validate_network_spec("n", "bridge", None, Some("172.20.0.1")).is_err());
        assert!(validate_network_spec("n", "bridge", Some("bad/16"), None).is_err());
        assert!(validate_network_spec("n", "bridge", Some("fd00::/64"), Some("fd00::1")).is_ok());
    }

    #[test]
    fn builds_network_action_script() {
        let script = build_action_script("network", "remove", "app-net").unwrap();
        assert!(script.contains("network rm 'app-net'"));
        assert!(build_action_script("network", "remove", "bridge").is_err());
        assert!(build_action_script("network", "remove", "host").is_err());
        assert!(build_action_script("network", "remove", "bad;id").is_err());
        assert!(build_action_script("network", "start", "app-net").is_err());
    }

    #[test]
    fn maps_docker_failures() {
        assert!(parse_list("@@DIAG@@\nDOCKER_MISSING\n")
            .unwrap_err()
            .contains("未安装 docker"));
        assert_eq!(
            parse_list("@@DIAG@@\nDOCKER_PERMISSION_DENIED\n").unwrap_err(),
            DOCKER_PERMISSION_DENIED
        );
        assert!(parse_list("@@DIAG@@\nDOCKER_DAEMON_DOWN\n")
            .unwrap_err()
            .contains("守护进程"));
        assert!(parse_list("@@DIAG@@\nDOCKER_ERROR\nCannot connect to the Docker daemon\n")
            .unwrap_err()
            .contains("Cannot connect"));
    }

    #[test]
    fn rejects_unparsable_rows() {
        let text = "@@DIAG@@\nUID=0\n@@CONTAINERS@@\ntemplate: :1: unexpected\n@@IMAGES@@\n@@END@@\n";
        assert!(parse_list(text).unwrap_err().contains("无法解析"));
    }

    #[test]
    fn validates_actions() {
        assert!(validate_action("container", "start").is_ok());
        assert!(validate_action("container", "restart").is_ok());
        assert!(validate_action("container", "remove").is_ok());
        assert!(validate_action("container", "rmi").is_err());
        assert!(validate_action("image", "remove").is_ok());
        assert!(validate_action("image", "stop").is_err());
        assert!(validate_action("network", "remove").is_ok());
        assert!(validate_action("network", "create").is_err());
        assert!(validate_action("volume", "remove").is_ok());
        assert!(validate_action("volume", "create").is_err());
    }

    #[test]
    fn validates_targets() {
        assert_eq!(
            validate_target("container", "web-1").unwrap(),
            "web-1"
        );
        assert_eq!(
            validate_target("image", "registry.local:5000/team/app:1.2").unwrap(),
            "registry.local:5000/team/app:1.2"
        );
        assert_eq!(
            validate_target("image", "sha256:abcd1234").unwrap(),
            "sha256:abcd1234"
        );
        assert_eq!(validate_target("network", "app-net_1").unwrap(), "app-net_1");
        assert!(validate_target("container", "").is_err());
        assert!(validate_target("container", "-rf").is_err());
        assert!(validate_target("container", "web;rm -rf /").is_err());
        assert!(validate_target("image", "nginx latest").is_err());
        assert!(validate_target("image", "nginx:latest$(id)").is_err());
        assert_eq!(
            validate_target("volume", "app-data_1").unwrap(),
            "app-data_1"
        );
        assert!(validate_target("volume", "-data").is_err());
        assert!(validate_target("volume", "data;id").is_err());
    }

    #[test]
    fn builds_action_script() {
        let s = build_action_script("container", "stop", "web-1").unwrap();
        assert!(s.contains("DOCKER='docker'"));
        assert!(s.contains("err=$(\"$DOCKER\" stop 'web-1' 2>&1)"));
        assert!(!s.contains("out=$(\"$DOCKER\""));
        assert!(s.contains(ACTION_OK));
        assert!(s.contains(DOCKER_PERMISSION_DENIED));
        assert!(build_action_script("image", "remove", "nginx:latest")
            .unwrap()
            .contains("\"$DOCKER\" rmi 'nginx:latest'"));
        assert!(build_action_script("volume", "remove", "app-data")
            .unwrap()
            .contains("\"$DOCKER\" volume rm 'app-data'"));
        assert!(build_action_script("container", "explode", "web").is_err());
    }

    #[test]
    fn validates_volume_specs_and_builds_create_script() {
        assert_eq!(validate_volume_name(" app-data_1 ").unwrap(), "app-data_1");
        assert!(validate_volume_name("").is_err());
        assert!(validate_volume_name("-data").is_err());
        assert!(validate_volume_name("data;id").is_err());
        assert!(validate_volume_name(&"v".repeat(256)).is_err());

        let spec = validate_volume_spec("app-data", "Local").unwrap();
        assert_eq!(spec.driver, "local");
        let script = build_volume_create_script(&spec);
        assert!(script.contains("volume create --driver 'local' 'app-data'"));
        assert!(script.contains(ACTION_OK));

        let bare = validate_volume_spec("pgdata", "   ").unwrap();
        assert_eq!(bare.driver, "local");
        assert_eq!(validate_volume_spec("v", "nfs").unwrap().driver, "nfs");
        assert!(validate_volume_spec("v", "bad driver").is_err());
        assert!(validate_volume_spec("v", "-x").is_err());
    }

    #[test]
    fn parses_action_results() {
        assert!(action_result(&out("DOCKER_ACTION_OK\n", 0)).is_ok());
        let err = action_result(&out("DOCKER_ACTION_FAILED\nError: container is running\n", 5))
            .unwrap_err();
        assert!(err.contains("container is running"));
        let err = action_result(&out(
            "DOCKER_ERROR\nError response from daemon: No such container: web\n",
            5,
        ))
        .unwrap_err();
        assert!(err.contains("No such container"));
        assert_eq!(
            action_result(&out(DOCKER_PERMISSION_DENIED, 5)).unwrap_err(),
            DOCKER_PERMISSION_DENIED
        );
        assert!(action_result(&out(DOCKER_MISSING, 4))
            .unwrap_err()
            .contains("未安装 docker"));
        let timeout = ExecOutput {
            output: String::new(),
            exit_code: None,
            timed_out: true,
        };
        assert!(action_result(&timeout).unwrap_err().contains("超时"));
    }

    #[test]
    fn truncates_long_details() {
        let long = "x".repeat(MAX_DETAIL + 50);
        let err = action_result(&out(&format!("DOCKER_ACTION_FAILED\n{long}"), 5)).unwrap_err();
        assert!(err.ends_with('…'));
        assert!(err.chars().count() < MAX_DETAIL + 30);
    }

    #[test]
    fn builds_logs_script_and_parses() {
        let s = build_logs_script("web-1", 200, true).unwrap();
        assert!(s.contains("\"$DOCKER\" logs --tail 200 --timestamps 'web-1'"));
        assert!(build_logs_script("web-1", 5, false).is_err());
        let parsed = parse_logs(&out("@@LOGS@@12\nhello\nworld\n", 0)).unwrap();
        assert_eq!(parsed.output, "hello\nworld\n");
        assert!(!parsed.truncated);
        assert!(parse_logs(&out("DOCKER_PERMISSION_DENIED", 5)).is_err());
        let long_text = "y".repeat(MAX_LOG_CHARS + 10);
        let parsed = parse_logs(&out(
            &format!("@@LOGS@@{}\n{}", long_text.len(), long_text),
            0,
        ))
        .unwrap();
        assert!(parsed.truncated);
        assert_eq!(parsed.output.chars().count(), MAX_LOG_CHARS);
    }

    #[test]
    fn validates_remote_paths() {
        assert_eq!(validate_remote_path(" /tmp/a.tar ").unwrap(), "/tmp/a.tar");
        assert_eq!(
            validate_remote_path("/tmp/my image.tar").unwrap(),
            "/tmp/my image.tar"
        );
        assert!(validate_remote_path("tmp/a.tar").is_err());
        assert!(validate_remote_path("/tmp/").is_err());
        assert!(validate_remote_path("/tmp/../etc/passwd").is_err());
        assert!(validate_remote_path("/tmp/a\nb").is_err());
    }

    #[test]
    fn validates_local_paths() {
        assert_eq!(
            validate_local_path(" /tmp/a.tar ").unwrap(),
            "/tmp/a.tar"
        );
        assert_eq!(
            validate_local_path("/tmp/my image.tar").unwrap(),
            "/tmp/my image.tar"
        );
        assert!(validate_local_path("").is_err());
        assert!(validate_local_path("relative/a.tar").is_err());
        assert!(validate_local_path("/tmp/").is_err());
        assert!(validate_local_path("/tmp/a\nb.tar").is_err());
    }

    #[test]
    fn builds_temp_tar_path() {
        let p = temp_tar_path("nginx:latest");
        assert!(p.starts_with("/tmp/rhost-nginx_latest-"), "{p}");
        assert!(p.ends_with(".tar"), "{p}");
        let p = temp_tar_path("///");
        assert!(p.starts_with("/tmp/rhost-image-"), "{p}");
        assert_ne!(temp_tar_path("x"), temp_tar_path("x"));
    }

    #[test]
    fn builds_export_and_parses() {
        let s = build_export_script("nginx:latest", "/tmp/nginx.tar", false).unwrap();
        assert!(s.contains("\"$DOCKER\" save -o '/tmp/nginx.tar' 'nginx:latest'"));
        assert!(s.contains("[ 0 -ne 1 ]"));
        assert_eq!(
            export_result(&out("DOCKER_EXPORT_EXISTS\n", 5)).unwrap_err(),
            EXPORT_EXISTS
        );
        assert_eq!(
            export_result(&out("DOCKER_EXPORT_OK\nSIZE=2097152\n", 0)).unwrap(),
            "2.0 MB"
        );
        assert!(export_result(&out("DOCKER_EXPORT_FAILED\nno space left\n", 5))
            .unwrap_err()
            .contains("no space left"));
    }

    #[test]
    fn builds_import_and_parses() {
        let s = build_import_script("/tmp/nginx.tar", None).unwrap();
        assert!(s.contains("\"$DOCKER\" load -i '/tmp/nginx.tar'"));
        assert!(!s.contains(" tag "));
        assert_eq!(
            import_result(&out("DOCKER_IMPORT_OK\nLoaded image: nginx:latest\n", 0)).unwrap(),
            "Loaded image: nginx:latest"
        );
        assert!(import_result(&out("DOCKER_IMPORT_MISSING\n", 5))
            .unwrap_err()
            .contains("不存在"));
    }

    #[test]
    fn imports_with_custom_tag() {
        let s = build_import_script("/tmp/nginx.tar", Some("nginx:my-tag")).unwrap();
        assert!(s.contains("\"$DOCKER\" tag \"$ref\" 'nginx:my-tag'"));
        assert!(s.contains(TAG_FAILED));
        assert_eq!(
            import_result(&out(
                "TAGGED=nginx:my-tag\nDOCKER_IMPORT_OK\nLoaded image: nginx:latest\n",
                0
            ))
            .unwrap(),
            "Loaded image: nginx:latest，已打标签 nginx:my-tag"
        );
        assert!(import_result(&out(
            "DOCKER_TAG_FAILED\nconflict: unable to delete\n",
            7
        ))
        .unwrap_err()
        .contains("镜像已导入，但打标签失败"));

        assert_eq!(
            validate_import_tag(Some(" nginx:my-tag ")).unwrap(),
            Some("nginx:my-tag".into())
        );
        assert!(validate_import_tag(Some("my-tag")).is_err());
        assert_eq!(validate_import_tag(None).unwrap(), None);
    }

    #[test]
    fn resolves_custom_tags() {
        assert_eq!(validate_custom_tag("nginx", None).unwrap(), None);
        assert_eq!(
            validate_custom_tag("nginx:latest", Some("v2")).unwrap(),
            Some("nginx:v2".into())
        );
        assert_eq!(
            validate_custom_tag("registry.example.com:5000/team/app:1.0", Some("v2")).unwrap(),
            Some("registry.example.com:5000/team/app:v2".into())
        );
        assert_eq!(
            validate_custom_tag("nginx", Some("myrepo/nginx:custom")).unwrap(),
            Some("myrepo/nginx:custom".into())
        );
        assert!(validate_custom_tag("nginx", Some("-bad")).is_err());
        assert!(validate_custom_tag("nginx", Some("bad tag")).is_err());
    }

    #[test]
    fn builds_pull_and_parses() {
        let s = build_pull_script("ghcr.io/team/app:1.0", Some("app:local")).unwrap();
        assert!(s.contains("\"$DOCKER\" pull 'ghcr.io/team/app:1.0'"));
        assert!(s.contains("\"$DOCKER\" tag \"$ref\" 'app:local'"));
        assert_eq!(
            pull_result(&out(
                "PULLED=ghcr.io/team/app:1.0\nTAGGED=app:local\nDOCKER_PULL_OK\n",
                0
            ))
            .unwrap(),
            "镜像 ghcr.io/team/app:1.0 已拉取，并打标签 app:local"
        );
        assert_eq!(
            pull_result(&out("PULLED=nginx\nDOCKER_PULL_OK\n", 0)).unwrap(),
            "镜像 nginx 已拉取"
        );
        assert!(pull_result(&out("DOCKER_TAG_FAILED\ninvalid reference\n", 7))
            .unwrap_err()
            .contains("镜像已拉取，但打标签失败"));
        assert!(pull_result(&out("DOCKER_PULL_FAILED\nmanifest unknown\n", 5))
            .unwrap_err()
            .contains("manifest unknown"));
    }

    #[test]
    fn validates_mirrors_and_daemon_json() {
        assert_eq!(
            validate_mirrors(&[
                " https://docker.mirrors.ustc.edu.cn ".into(),
                "https://docker.mirrors.ustc.edu.cn".into(),
                "".into(),
            ])
            .unwrap(),
            vec!["https://docker.mirrors.ustc.edu.cn".to_string()]
        );
        assert!(validate_mirrors(&["docker.io".into()]).is_err());
        assert!(validate_mirrors(&["https://a b".into()]).is_err());
        assert!(validate_mirrors(&["https:///path".into()]).is_err());

        let edited = validate_daemon_json(
            r#"{"data-root":"/home/docker1/","registry-mirrors":["https://new"],"log-driver":"json-file","log-opts":{"max-size":"10m"}}"#,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&edited).unwrap();
        assert_eq!(v["data-root"], "/home/docker1");
        assert_eq!(v["registry-mirrors"][0], "https://new");
        assert_eq!(v["log-driver"], "json-file");
        assert_eq!(v["log-opts"]["max-size"], "10m");

        // 空镜像源与空字符串 data-root 保存时移除
        let removed = validate_daemon_json(
            r#"{"data-root":"","registry-mirrors":[],"log-level":"warn"}"#,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&removed).unwrap();
        assert!(v.get("data-root").is_none());
        assert!(v.get("registry-mirrors").is_none());
        assert_eq!(v["log-level"], "warn");

        assert_eq!(validate_daemon_json("  ").unwrap(), "{}");
        assert!(validate_daemon_json("not json").is_err());
        assert!(validate_daemon_json("[1,2]").unwrap_err().contains("顶层"));
        assert!(validate_daemon_json(r#"{"registry-mirrors":"https://a"}"#)
            .unwrap_err()
            .contains("字符串数组"));
        assert!(validate_daemon_json(r#"{"registry-mirrors":[1]}"#).is_err());
        assert!(validate_daemon_json(r#"{"data-root":5}"#)
            .unwrap_err()
            .contains("字符串"));
        assert!(validate_daemon_json(r#"{"data-root":"/home/../etc"}"#).is_err());
        assert!(validate_daemon_json(&format!(
            r#"{{"log-driver":"{}"}}"#,
            "x".repeat(MAX_DAEMON_JSON)
        ))
        .unwrap_err()
        .contains("过长"));
    }

    #[test]
    fn validates_data_root() {
        assert_eq!(validate_data_root("").unwrap(), None);
        assert_eq!(validate_data_root("   ").unwrap(), None);
        assert_eq!(
            validate_data_root(" /home/docker1/ ").unwrap(),
            Some("/home/docker1".into())
        );
        assert!(validate_data_root("home/docker1").is_err());
        assert!(validate_data_root("/").is_err());
        assert!(validate_data_root("/home/../etc").is_err());
        assert!(validate_data_root("/home/my docker").is_err());
        assert!(validate_data_root("/home/a\nb").is_err());
        assert!(validate_data_root(&format!("/{}", "x".repeat(MAX_DATA_ROOT))).is_err());
    }

    #[test]
    fn parses_config_snapshot() {
        let text = "@@VER@@\nDocker version 27.1.1, build abc\n@@DAEMON@@\n{\"data-root\":\"/home/docker1\",\"registry-mirrors\":[\"https://m\"],\"log-driver\":\"json-file\"}\n@@END@@\n";
        let cfg = parse_config(text).unwrap();
        assert!(cfg.version.contains("27.1.1"));
        assert_eq!(cfg.registry_mirrors, vec!["https://m".to_string()]);
        assert_eq!(cfg.data_root.as_deref(), Some("/home/docker1"));
        assert!(cfg.daemon_json.unwrap().contains("log-driver"));

        let cfg = parse_config("@@VER@@\nv\n@@DAEMON@@\n@@NOFILE@@\n@@END@@\n").unwrap();
        assert!(cfg.daemon_json.is_none());
        assert!(cfg.registry_mirrors.is_empty());
        assert!(cfg.data_root.is_none());

        let bad = "@@VER@@\nv\n@@DAEMON@@\n{bad json\n@@END@@\n";
        assert!(parse_config(bad).unwrap_err().contains("合法 JSON"));
    }

    #[test]
    fn builds_config_save_and_parses() {
        let s = build_config_save_script("{\"registry-mirrors\":[]}");
        assert!(s.contains("cp -a \"$DAEMON\" \"$DAEMON.bak."));
        assert!(s.contains("systemctl restart docker"));
        assert!(config_save_result(&out("DOCKER_CONFIG_OK\n", 0)).is_ok());
        let err = config_save_result(&out(
            "DOCKER_CONFIG_RESTART_FAILED\nJob for docker.service failed\n",
            5,
        ))
        .unwrap_err();
        assert!(err.contains("已还原备份"));
        assert!(err.contains("Job for docker.service"));
    }

    #[test]
    fn builds_terminal_command_and_probe() {
        let cmd = build_terminal_command("web-1", SudoMode::None).unwrap();
        assert!(cmd.starts_with("exec 'docker' exec -it 'web-1' sh -c"));
        assert!(!cmd.contains("sudo"));
        let cmd = build_terminal_command("web-1", SudoMode::Passwordless).unwrap();
        assert!(cmd.starts_with("exec sudo -n 'docker' exec -it 'web-1'"));
        assert!(!cmd.contains("sudo -S"));
        let cmd = build_terminal_command("web-1", SudoMode::Password).unwrap();
        assert!(cmd.starts_with("sudo -S -p '' -v || exit 1\n"));
        assert!(cmd.contains("exec sudo -n 'docker' exec -it 'web-1'"));
        assert!(build_terminal_command("web;id", SudoMode::None).is_err());

        assert!(probe_result(&out("DOCKER_TERMINAL_READY\n", 0)).is_ok());
        assert_eq!(
            probe_result(&out("DOCKER_PERMISSION_DENIED\n", 5)).unwrap_err(),
            DOCKER_PERMISSION_DENIED
        );
    }
}
