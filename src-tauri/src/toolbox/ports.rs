//! 监听端口采集：ss → netstat → lsof 回退，补进程用户与 docker 容器归属。
//!
//! 一次 exec 往返分五段：端口表 + `ps` 用户表 + 全进程 cgroup 容器 ID +
//! docker 容器名/发布端口 + 环境诊断。容器归属有两条路径：监听进程本身在
//! 容器 cgroup 里（host 网络模式等），或端口由 `docker-proxy` 转发（默认
//! userland-proxy），后者按「宿主机地址+协议+端口」反查发布映射。解析全部
//! 在 Rust 侧完成，远端只跑只读命令。

use std::collections::HashMap;

use serde::Serialize;

/// 采集脚本：以 marker 分段，任意一段失败不影响其他段（解析端按段缺失降级）。
///
/// cgroup 段遍历全进程只为拿到「PID → 容器 ID」，输出行数约为进程数；
/// docker 段优先 CLI，同时输出容器名与发布端口（默认 userland-proxy 下监听
/// 端口属于宿主机的 docker-proxy，cgroup 反查不到容器）；CLI 缺席才回退
/// `<data-root>/containers/*/config.v2.json`（与指标采集同口径，无发布端口）。
/// diag 段带上 euid 与 docker CLI 可用性，供日志诊断与 root 视图判定。
pub(super) const PORTS_SCRIPT: &str = r#"echo '@@PORTS@@'
if command -v ss >/dev/null 2>&1; then
  echo 'TOOL=ss'
  ss -tulnp 2>/dev/null
elif command -v netstat >/dev/null 2>&1; then
  echo 'TOOL=netstat'
  netstat -tulnp 2>/dev/null
elif command -v lsof >/dev/null 2>&1; then
  echo 'TOOL=lsof'
  lsof -i -P -n 2>/dev/null
else
  echo 'TOOL=none'
fi
echo '@@PS@@'
# 不能带 `=` 空表头：procps-ng 3.3.10 上 `pid=,user=,comm=` 只输出 PID 列
ps -eo pid,user,comm 2>/dev/null
echo '@@CGROUP@@'
# 一次性批量 grep + awk 提取 PID:容器ID；逐进程 spawn grep 在千级进程主机会拖到超时
grep -aHoE '[a-f0-9]{12,64}' /proc/[0-9]*/cgroup 2>/dev/null | awk -F'[:/]' '!seen[$3]++ {print $3 ":" $NF}'
echo '@@DOCKER@@'
if command -v docker >/dev/null 2>&1; then
  docker ps --no-trunc --format '{{.ID}} {{.Names}} {{.Ports}}' 2>/dev/null | while IFS=' ' read -r id nm ports; do
    [ -n "$id" ] && [ -n "$nm" ] && echo "N:$id:$nm"
    [ -n "$ports" ] && echo "P:$ports|$nm"
  done
else
  dr=''
  if [ -f /etc/docker/daemon.json ]; then
    dr=$(sed -n 's/.*"data-root"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' /etc/docker/daemon.json | head -1)
  fi
  for root in "$dr" /var/lib/docker /home/docker "$HOME/.local/share/docker"; do
    [ -n "$root" ] && [ -d "$root/containers" ] || continue
    for f in "$root"/containers/*/config.v2.json; do
      [ -f "$f" ] || continue
      id=${f#*/containers/}; id=${id%%/*}
      name=$(grep -o '"Name":"[^"]*"' "$f" 2>/dev/null | head -1 | cut -d'"' -f4)
      [ -n "$name" ] && echo "N:$id:${name#/}"
    done
  done
fi
echo '@@DIAG@@'
echo "UID=$(id -u 2>/dev/null)"
if command -v docker >/dev/null 2>&1; then echo 'DOCKER=1'; else echo 'DOCKER=0'; fi
echo '@@END@@'
"#;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortRow {
    pub port: u16,
    pub protocol: String,
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

#[derive(Debug, Default)]
struct RawPort {
    port: u16,
    protocol: String,
    address: String,
    pid: Option<u32>,
    process: Option<String>,
    user: Option<String>,
}

/// 解析结果：端口行 + 供日志诊断的环境信息
#[derive(Debug)]
pub(super) struct ParsedPorts {
    pub tool: String,
    /// 远端 exec 通道的 euid（root 登录也是 0，与是否走 sudo 无关）
    pub uid: Option<u32>,
    pub docker_cli: bool,
    /// cgroup 扫到的「进程 → 容器 ID」条数
    pub cgroup_entries: usize,
    /// docker 容器名条数
    pub container_names: usize,
    /// docker 发布端口条数（含 v4/v6 双向）
    pub published_ports: usize,
    pub rows: Vec<PortRow>,
}

/// 解析整段输出
pub(super) fn parse(output: &str) -> Result<ParsedPorts, String> {
    let mut tool = "none".to_string();
    let mut ports_out = String::new();
    let mut ps_out = String::new();
    let mut cgroup_out = String::new();
    let mut docker_out = String::new();
    let mut uid: Option<u32> = None;
    let mut docker_cli = false;
    let mut section = "";

    for line in output.lines() {
        match line.trim_end() {
            "@@PORTS@@" => {
                section = "ports";
                continue;
            }
            "@@PS@@" => {
                section = "ps";
                continue;
            }
            "@@CGROUP@@" => {
                section = "cgroup";
                continue;
            }
            "@@DOCKER@@" => {
                section = "docker";
                continue;
            }
            "@@DIAG@@" => {
                section = "diag";
                continue;
            }
            "@@END@@" => {
                section = "end";
                continue;
            }
            _ => {}
        }
        match section {
            "ports" => {
                if let Some(t) = line.strip_prefix("TOOL=") {
                    tool = t.trim().to_string();
                } else {
                    ports_out.push_str(line);
                    ports_out.push('\n');
                }
            }
            "ps" => {
                ps_out.push_str(line);
                ps_out.push('\n');
            }
            "cgroup" => {
                cgroup_out.push_str(line);
                cgroup_out.push('\n');
            }
            "docker" => {
                docker_out.push_str(line);
                docker_out.push('\n');
            }
            "diag" => {
                if let Some(v) = line.strip_prefix("UID=") {
                    uid = v.trim().parse().ok();
                } else if let Some(v) = line.strip_prefix("DOCKER=") {
                    docker_cli = v.trim() == "1";
                }
            }
            _ => {}
        }
    }

    if tool == "none" {
        return Err("远端未找到 ss / netstat / lsof，无法读取监听端口".into());
    }
    let mut raws = match tool.as_str() {
        "ss" => parse_ss(&ports_out),
        "netstat" => parse_netstat(&ports_out),
        "lsof" => parse_lsof(&ports_out),
        _ => Vec::new(),
    };

    let ps = parse_ps_map(&ps_out);
    let cgroups = parse_pid_container_map(&cgroup_out);
    let docker = parse_docker_names(&docker_out);
    let published = parse_published_ports(&docker_out);

    for raw in &mut raws {
        if let Some(pid) = raw.pid {
            if let Some((user, comm)) = ps.get(&pid) {
                if raw.process.as_deref().unwrap_or("").is_empty() {
                    raw.process = Some(comm.clone());
                }
                if raw.user.is_none() {
                    raw.user = Some(user.clone());
                }
            }
        }
    }

    let mut rows: Vec<PortRow> = raws
        .into_iter()
        .map(|raw| {
            let container = raw
                .pid
                .and_then(|pid| cgroups.get(&pid))
                .and_then(|cid| find_container_name(&docker, cid))
                .or_else(|| published_container(&published, &raw));
            PortRow {
                port: raw.port,
                protocol: raw.protocol,
                address: raw.address,
                pid: raw.pid,
                process: raw.process.filter(|s| !s.is_empty()),
                user: raw.user,
                container,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        a.port
            .cmp(&b.port)
            .then_with(|| a.protocol.cmp(&b.protocol))
            .then_with(|| a.address.cmp(&b.address))
    });
    Ok(ParsedPorts {
        tool,
        uid,
        docker_cli,
        cgroup_entries: cgroups.len(),
        container_names: docker.len(),
        published_ports: published.len(),
        rows,
    })
}

fn parse_ss(stdout: &str) -> Vec<RawPort> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 5 {
            continue;
        }
        // Netid State Recv-Q Send-Q Local:Port Peer:Port [Process]
        let protocol = normalize_proto(f[0]);
        let Some(protocol) = protocol else { continue };
        if f[1] != "LISTEN" && f[1] != "UNCONN" {
            continue;
        }
        let Some((address, port)) = split_addr_port(f[4]) else {
            continue;
        };
        let (pid, process) = f
            .get(6)
            .and_then(|s| parse_ss_process(s))
            .map(|(p, n)| (Some(p), n))
            .unwrap_or((None, None));
        out.push(RawPort {
            port,
            protocol: protocol.into(),
            address,
            pid,
            process,
            user: None,
        });
    }
    out
}

/// `users:(("sshd",pid=1234,fd=3),("sshd",pid=1235,fd=3))` → (1234, "sshd")
fn parse_ss_process(s: &str) -> Option<(u32, Option<String>)> {
    if !s.starts_with("users:") {
        return None;
    }
    let name = s.find('"').and_then(|i| {
        let rest = &s[i + 1..];
        rest.find('"').map(|j| rest[..j].to_string())
    });
    let pid = s.find("pid=").and_then(|i| {
        s[i + 4..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u32>()
            .ok()
    });
    pid.map(|p| (p, name.filter(|n| !n.is_empty())))
}

fn parse_netstat(stdout: &str) -> Vec<RawPort> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 6 {
            continue;
        }
        let Some(protocol) = normalize_proto(f[0]) else {
            continue;
        };
        if protocol == "tcp" && f[5] != "LISTEN" {
            continue;
        }
        let Some((address, port)) = split_addr_port(f[3]) else {
            continue;
        };
        let (pid, process) = f
            .get(6)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && *s != "-")
            .and_then(|s| match s.split_once('/') {
                Some((p, name)) => p.parse::<u32>().ok().map(|pid| {
                    (Some(pid), Some(name.to_string()).filter(|n| !n.is_empty()))
                }),
                None => s.parse::<u32>().ok().map(|pid| (Some(pid), None)),
            })
            .unwrap_or((None, None));
        out.push(RawPort {
            port,
            protocol: protocol.into(),
            address,
            pid,
            process,
            user: None,
        });
    }
    out
}

fn parse_lsof(stdout: &str) -> Vec<RawPort> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 9 || f[0] == "COMMAND" {
            continue;
        }
        // COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
        let Some(protocol) = normalize_proto(f[7]) else {
            continue;
        };
        let name = f[8];
        if name.contains("->") {
            continue; // 已建立连接的条目，不是监听口
        }
        let Some((address, port)) = split_addr_port(name) else {
            continue;
        };
        if protocol == "tcp" && !f[8..].contains(&"(LISTEN)") {
            continue;
        }
        out.push(RawPort {
            port,
            protocol: protocol.into(),
            address,
            pid: f[1].parse::<u32>().ok(),
            process: Some(f[0].to_string()),
            user: Some(f[2].to_string()),
        });
    }
    out
}

fn normalize_proto(raw: &str) -> Option<&'static str> {
    let r = raw.to_ascii_lowercase();
    if r.starts_with("tcp") {
        Some("tcp")
    } else if r.starts_with("udp") {
        Some("udp")
    } else {
        None
    }
}

/// `0.0.0.0:22` / `*:80` / `[::]:443` / `:::22` → (地址, 端口)
fn split_addr_port(s: &str) -> Option<(String, u16)> {
    let s = s.trim();
    let (addr, port_s) = s.rsplit_once(':')?;
    let port: u16 = port_s.parse().ok()?;
    if port == 0 {
        return None;
    }
    let addr = if addr.is_empty() || addr == "*" {
        "0.0.0.0"
    } else {
        addr
    };
    Some((addr.to_string(), port))
}

fn parse_ps_map(stdout: &str) -> HashMap<u32, (String, String)> {
    let mut map = HashMap::new();
    for line in stdout.lines() {
        let mut it = line.split_whitespace();
        let pid = it.next().and_then(|s| s.parse::<u32>().ok());
        let user = it.next().map(str::to_string);
        // comm 可能含空格（如 "Web Content"），余下整体作为进程名
        let comm = it.collect::<Vec<_>>().join(" ");
        if let (Some(pid), Some(user), false) = (pid, user, comm.is_empty()) {
            map.insert(pid, (user, comm));
        }
    }
    map
}

/// `PID:容器ID` 行 → pid 映射
fn parse_pid_container_map(stdout: &str) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    for line in stdout.lines() {
        if let Some((pid, cid)) = line.split_once(':') {
            if let (Ok(pid), true) = (pid.trim().parse::<u32>(), cid.len() >= 12) {
                map.insert(pid, cid.trim().to_string());
            }
        }
    }
    map
}

/// `N:容器ID:名称` 行 → 名称映射
fn parse_docker_names(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("N:"))
        .filter_map(|line| line.split_once(':'))
        .filter(|(id, name)| id.trim().len() >= 12 && !name.trim().is_empty())
        .map(|(id, name)| (id.trim().to_string(), name.trim().trim_start_matches('/').to_string()))
        .collect()
}

/// `P:<docker ps 的 Ports 串>|<名称>` 行 → (宿主机地址, 协议, 端口) 到容器名
///
/// Ports 串形如 `0.0.0.0:8080->8080/tcp, [::]:8080->8080/tcp`；
/// 只有 `容器端口/tcp` 的暴露口没有宿主机监听，直接忽略。
fn parse_published_ports(stdout: &str) -> Vec<((String, String, u16), String)> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("P:") else { continue };
        let Some((ports, name)) = rest.rsplit_once('|') else { continue };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        for item in ports.split(',') {
            let Some((left, right)) = item.trim().split_once("->") else { continue };
            let Some((_, proto)) = right.rsplit_once('/') else { continue };
            let proto = proto.trim().to_ascii_lowercase();
            if proto != "tcp" && proto != "udp" {
                continue;
            }
            let Some((addr, port)) = left.rsplit_once(':') else { continue };
            let Ok(port) = port.parse::<u16>() else { continue };
            out.push(((norm_addr(addr), proto, port), name.to_string()));
        }
    }
    out
}

/// `[::]` / `::` / `*` / `0.0.0.0` 归一化，便于 ss 与 docker 两侧比较
fn norm_addr(addr: &str) -> String {
    let a = addr.trim();
    let a = a.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(a);
    if a.is_empty() || a == "*" {
        "0.0.0.0".into()
    } else {
        a.to_string()
    }
}

/// docker-proxy 行反查发布映射：先按「地址+协议+端口」，地址对不上再按
/// 「协议+端口」且仅当该端口只映射到一个容器时兜底，避免误标宿主机同名端口。
fn published_container(
    published: &[((String, String, u16), String)],
    raw: &RawPort,
) -> Option<String> {
    let is_proxy = raw
        .process
        .as_deref()
        .map(|p| p.contains("docker-proxy"))
        .unwrap_or(false);
    if !is_proxy {
        return None;
    }
    let addr = norm_addr(&raw.address);
    if let Some((_, name)) = published
        .iter()
        .find(|((a, p, port), _)| *port == raw.port && p == &raw.protocol && a == &addr)
    {
        return Some(name.clone());
    }
    let mut names = published
        .iter()
        .filter(|((_, p, port), _)| *port == raw.port && p == &raw.protocol)
        .map(|(_, name)| name);
    let first = names.next()?;
    if names.all(|n| n == first) {
        Some(first.clone())
    } else {
        None
    }
}

/// cgroup 里的 ID 与 docker ID 可能一侧是完整 64 位、一侧是 12 位短 ID
fn find_container_name(docker: &[(String, String)], cid: &str) -> Option<String> {
    docker
        .iter()
        .find(|(id, _)| id == cid || id.starts_with(cid) || cid.starts_with(id))
        .map(|(_, name)| name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ps_with_leading_spaces() {
        let map = parse_ps_map("   42 tomcat java\n  812    root sshd\n");
        assert_eq!(map.get(&42), Some(&("tomcat".to_string(), "java".to_string())));
        assert_eq!(map.get(&812), Some(&("root".to_string(), "sshd".to_string())));
    }

    #[test]
    fn skips_ps_header_line() {
        let map = parse_ps_map("PID USER COMMAND\n      1 root     systemd\n");
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&1), Some(&("root".to_string(), "systemd".to_string())));
    }

    #[test]
    fn parses_ss_with_users() {
        let out = r#"Netid State  Recv-Q Send-Q Local Address:Port Peer Address:Port Process
tcp   LISTEN 0      128    0.0.0.0:22         0.0.0.0:*         users:(("sshd",pid=812,fd=3))
tcp   LISTEN 0      511    [::]:443           [::]:*            users:(("nginx",pid=1200,fd=6))
udp   UNCONN 0      0      127.0.0.1:53        0.0.0.0:*         users:(("dnsmasq",pid=701,fd=4))
tcp   ESTAB  0      0      10.0.0.2:22         10.0.0.9:52144
"#;
        let rows = parse_ss(out);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].port, 22);
        assert_eq!(rows[0].protocol, "tcp");
        assert_eq!(rows[0].pid, Some(812));
        assert_eq!(rows[0].process.as_deref(), Some("sshd"));
        assert_eq!(rows[1].address, "[::]");
        assert_eq!(rows[2].protocol, "udp");
    }

    #[test]
    fn parses_netstat_tcp_only_listen() {
        let out = r#"Proto Recv-Q Send-Q Local Address           Foreign Address         State       PID/Program name
tcp        0      0 0.0.0.0:22              0.0.0.0:*               LISTEN      900/sshd
tcp        0      0 10.0.0.2:22             10.0.0.9:5000           ESTABLISHED 901/sshd
udp        0      0 0.0.0.0:68              0.0.0.0:*                           1200/dhclient
"#;
        let rows = parse_netstat(out);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].port, 22);
        assert_eq!(rows[1].port, 68);
        assert_eq!(rows[1].protocol, "udp");
    }

    #[test]
    fn parses_lsof_listen_rows() {
        let out = r#"COMMAND   PID USER   FD   TYPE DEVICE SIZE/OFF NODE NAME
sshd      812 root    3u  IPv4  12345      0t0  TCP *:22 (LISTEN)
nginx    1200 root    6u  IPv6  23456      0t0  TCP *:443 (LISTEN)
sshd      812 root    4u  IPv4  34567      0t0  TCP 10.0.0.2:22->10.0.0.9:52144 (ESTABLISHED)
"#;
        let rows = parse_lsof(out);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].process.as_deref(), Some("sshd"));
        assert_eq!(rows[1].port, 443);
    }

    #[test]
    fn maps_docker_container_by_cgroup_prefix() {
        let docker = parse_docker_names("N:3f2a1b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a:web-1\n");
        let cid = "3f2a1b9c8d7e";
        assert_eq!(
            find_container_name(&docker, cid).as_deref(),
            Some("web-1")
        );
        assert_eq!(find_container_name(&docker, "ffffffffffff"), None);
    }

    #[test]
    fn parses_sections_and_enriches_rows() {
        let out = "\
@@PORTS@@
TOOL=ss
tcp LISTEN 0 128 0.0.0.0:8080 0.0.0.0:* users:((\\\"java\\\",pid=42,fd=5))
@@PS@@
42 tomcat root
@@CGROUP@@
42:aaaaaaaaaaaa
@@DOCKER@@
N:aaaaaaaaaaaabbbbccccddddeeeeffff11112222333344445555666677778888:app
@@DIAG@@
UID=0
DOCKER=1
@@END@@
";
        let p = parse(out).unwrap();
        assert_eq!(p.tool, "ss");
        assert_eq!(p.uid, Some(0));
        assert!(p.docker_cli);
        assert_eq!(p.cgroup_entries, 1);
        assert_eq!(p.container_names, 1);
        assert_eq!(p.rows.len(), 1);
        assert_eq!(p.rows[0].port, 8080);
        assert_eq!(p.rows[0].user.as_deref(), Some("tomcat"));
        assert_eq!(p.rows[0].container.as_deref(), Some("app"));
    }

    #[test]
    fn maps_docker_proxy_rows_by_published_port() {
        let out = "\
@@PORTS@@
TOOL=ss
tcp LISTEN 0 128 0.0.0.0:8080 0.0.0.0:* users:((\\\"docker-proxy\\\",pid=99,fd=4))
tcp LISTEN 0 128 [::]:8080 [::]:* users:((\\\"docker-proxy\\\",pid=100,fd=4))
tcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\\\"sshd\\\",pid=5,fd=3))
@@PS@@
99 root docker-proxy
100 root docker-proxy
5 root sshd
@@CGROUP@@
@@DOCKER@@
N:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:web
P:0.0.0.0:8080->8080/tcp, [::]:8080->8080/tcp|web
@@DIAG@@
UID=0
DOCKER=1
@@END@@
";
        let p = parse(out).unwrap();
        assert_eq!(p.published_ports, 2);
        let web_rows: Vec<_> = p
            .rows
            .iter()
            .filter(|r| r.container.as_deref() == Some("web"))
            .collect();
        assert_eq!(web_rows.len(), 2);
        assert!(p.rows.iter().find(|r| r.port == 22).unwrap().container.is_none());
    }

    #[test]
    fn does_not_label_host_process_as_container() {
        let out = "\
@@PORTS@@
TOOL=ss
tcp LISTEN 0 128 0.0.0.0:8080 0.0.0.0:* users:((\\\"nginx\\\",pid=7,fd=3))
@@PS@@
7 nginx nginx
@@DOCKER@@
N:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:web
P:0.0.0.0:8080->8080/tcp|web
@@END@@
";
        let p = parse(out).unwrap();
        assert!(p.rows[0].container.is_none());
    }

    #[test]
    fn no_tool_is_error() {
        let out = "@@PORTS@@\nTOOL=none\n@@END@@\n";
        assert!(parse(out).is_err());
    }

    #[test]
    fn splits_address_forms() {
        assert_eq!(
            split_addr_port("[::]:443"),
            Some(("[::]".into(), 443))
        );
        assert_eq!(split_addr_port("*:80"), Some(("0.0.0.0".into(), 80)));
        assert_eq!(split_addr_port(":::22"), Some(("::".into(), 22)));
        assert_eq!(split_addr_port("127.0.0.1%lo:53"), Some(("127.0.0.1%lo".into(), 53)));
        assert_eq!(split_addr_port("0.0.0.0:0"), None);
    }
}
