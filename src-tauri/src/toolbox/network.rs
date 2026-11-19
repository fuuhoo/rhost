//! 网络信息：网卡、地址、路由与 DNS，一次只读采集。
//!
//! 优先用 iproute2（`ip -o link/addr`、`ip route`）；没有 `ip` 时回退
//! `ifconfig -a` + `route -n`（老发行版与 macOS 远端）。远端只跑只读命令，
//! 解析全部在 Rust 侧完成，不需要提权。

use serde::Serialize;

/// 采集脚本：marker 分段，缺失段按空处理。
///
/// `ifconfig` / `route` 只在没有 `ip` 时执行，避免两套数据混在一起。
pub(super) const NETWORK_SCRIPT: &str = r#"echo '@@HOST@@'
hostname 2>/dev/null || cat /proc/sys/kernel/hostname 2>/dev/null
echo '@@DIAG@@'
uname -srm 2>/dev/null
if command -v ip >/dev/null 2>&1; then echo 'TOOL=ip'; else echo 'TOOL=legacy'; fi
echo '@@LINK@@'
ip -o link show 2>/dev/null
echo '@@ADDR@@'
ip -o addr show 2>/dev/null
echo '@@ROUTE@@'
ip route show 2>/dev/null
echo '@@ROUTE6@@'
ip -6 route show 2>/dev/null
echo '@@IFCONFIG@@'
if ! command -v ip >/dev/null 2>&1; then ifconfig -a 2>/dev/null; fi
echo '@@NETSTAT@@'
if ! command -v ip >/dev/null 2>&1; then route -n 2>/dev/null || netstat -rn 2>/dev/null; fi
echo '@@DNS@@'
cat /etc/resolv.conf 2>/dev/null
echo '@@END@@'
"#;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetAddr {
    /// ipv4 / ipv6
    pub family: String,
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetIface {
    pub name: String,
    /// UP / DOWN / UNKNOWN
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    pub addrs: Vec<NetAddr>,
    /// 承载默认路由（v4 或 v6）
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetRoute {
    /// ipv4 / ipv6
    pub family: String,
    /// `default` 或 CIDR（旧式 route 输出为点分地址）
    pub destination: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
    pub default: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetDns {
    pub servers: Vec<String>,
    pub search: Vec<String>,
    pub options: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// ip / legacy / none
    pub tool: String,
    pub interfaces: Vec<NetIface>,
    pub routes: Vec<NetRoute>,
    pub dns: NetDns,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn opt(v: &str) -> Option<String> {
    let v = v.trim();
    if v.is_empty() || v == "-" || v == "0.0.0.0" || v == "*" {
        None
    } else {
        Some(v.to_string())
    }
}

/// 去掉 `@if3` 之类的对端后缀
fn iface_name(raw: &str) -> String {
    raw.trim_end_matches(':')
        .split('@')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 网络掩码（点分十进制或 0x 十六进制）转前缀长度
fn mask_prefix(mask: &str) -> Option<u8> {
    if let Some(hex) = mask.strip_prefix("0x") {
        let v = u32::from_str_radix(hex, 16).ok()?;
        return Some(v.count_ones() as u8);
    }
    let v: u32 = mask
        .parse::<std::net::Ipv4Addr>()
        .ok()
        .map(u32::from)?;
    Some(v.count_ones() as u8)
}

fn parse_link_line(line: &str, ifaces: &mut Vec<NetIface>) {
    let mut it = line.split_whitespace();
    let _idx = it.next();
    let name = iface_name(it.next().unwrap_or(""));
    if name.is_empty() {
        return;
    }
    let toks: Vec<&str> = it.collect();
    let mut state = "UNKNOWN".to_string();
    let mut mtu = None;
    let mut mac = None;
    let mut i = 0;
    while i < toks.len() {
        match toks[i] {
            "state" => {
                if let Some(v) = toks.get(i + 1) {
                    state = v.to_string();
                }
                i += 2;
            }
            "mtu" => {
                mtu = toks.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            "link/ether" | "link/loopback" => {
                mac = toks.get(i + 1).map(|v| v.to_string());
                i += 2;
            }
            _ => i += 1,
        }
    }
    if let Some(existing) = ifaces.iter_mut().find(|x| x.name == name) {
        existing.state = state;
        existing.mtu = mtu;
        existing.mac = mac;
        return;
    }
    ifaces.push(NetIface {
        name,
        state,
        mtu,
        mac,
        addrs: Vec::new(),
        default: false,
    });
}

fn parse_addr_line(line: &str, ifaces: &mut Vec<NetIface>) {
    let mut it = line.split_whitespace();
    let _idx = it.next();
    let name = iface_name(it.next().unwrap_or(""));
    let kind = match it.next() {
        Some("inet") => "ipv4",
        Some("inet6") => "ipv6",
        _ => return,
    };
    let addr = match it.next() {
        Some(v) => v,
        None => return,
    };
    if name.is_empty() {
        return;
    }
    let rest: Vec<&str> = it.collect();
    let (address, prefix) = match addr.split_once('/') {
        Some((a, p)) => (a.to_string(), p.parse::<u8>().ok()),
        None => (addr.to_string(), None),
    };
    let scope = rest
        .iter()
        .position(|t| *t == "scope")
        .and_then(|p| rest.get(p + 1))
        .map(|v| v.to_string());
    let entry = ifaces.iter_mut().find(|x| x.name == name);
    let entry = match entry {
        Some(v) => v,
        None => {
            ifaces.push(NetIface {
                name: name.clone(),
                state: "UNKNOWN".into(),
                mtu: None,
                mac: None,
                addrs: Vec::new(),
                default: false,
            });
            ifaces.last_mut().unwrap()
        }
    };
    entry.addrs.push(NetAddr {
        family: kind.to_string(),
        address,
        prefix,
        scope,
    });
}

fn parse_route_line(line: &str, family: &str, routes: &mut Vec<NetRoute>) {
    let toks: Vec<&str> = line.split_whitespace().collect();
    let first = match toks.first() {
        Some(v) => *v,
        None => return,
    };
    if matches!(
        first,
        "blackhole" | "unreachable" | "prohibit" | "throw" | "local" | "broadcast"
    ) {
        return;
    }
    let mut route = NetRoute {
        family: family.to_string(),
        destination: first.to_string(),
        gateway: None,
        device: None,
        protocol: None,
        scope: None,
        metric: None,
        src: None,
        default: first == "default",
    };
    let mut i = 1;
    while i < toks.len() {
        match toks[i] {
            "via" => route.gateway = toks.get(i + 1).map(|v| v.to_string()),
            "dev" => route.device = toks.get(i + 1).map(|v| v.to_string()),
            "proto" => route.protocol = toks.get(i + 1).map(|v| v.to_string()),
            "scope" => route.scope = toks.get(i + 1).map(|v| v.to_string()),
            "metric" => route.metric = toks.get(i + 1).and_then(|v| v.parse().ok()),
            "src" => route.src = toks.get(i + 1).map(|v| v.to_string()),
            _ => {
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    routes.push(route);
}

/// 旧式 `ifconfig -a`：按接口分块解析 flags / mtu / ether / inet / inet6。
fn parse_ifconfig(text: &str, ifaces: &mut Vec<NetIface>) {
    let mut cur: Option<usize> = None;
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            cur = None;
            continue;
        }
        let indented = line.starts_with(|c: char| c == ' ' || c == '\t');
        if !indented {
            // 头部：`eno1: flags=...` 或旧版 `eth0  Link encap:Ethernet  HWaddr ...`
            let (name, rest) = if let Some((n, r)) = line.split_once(':').filter(|(n, _)| {
                !n.contains(char::is_whitespace)
            }) {
                (n.to_string(), r)
            } else {
                let mut it = line.splitn(2, char::is_whitespace);
                (it.next().unwrap_or("").to_string(), it.next().unwrap_or(""))
            };
            let name = iface_name(&name);
            if name.is_empty() {
                continue;
            }
            let mut state = "UNKNOWN".to_string();
            if let Some(flags) = rest
                .split(|c| c == '<' || c == '>')
                .nth(1)
            {
                state = if flags.split(',').any(|f| f == "UP") {
                    "UP".into()
                } else {
                    "DOWN".into()
                };
            }
            let toks: Vec<&str> = rest.split_whitespace().collect();
            let mtu = toks
                .iter()
                .position(|t| *t == "mtu")
                .and_then(|p| toks.get(p + 1))
                .and_then(|v| v.parse().ok());
            let mac = rest
                .split_whitespace()
                .position(|t| t == "HWaddr")
                .and_then(|p| rest.split_whitespace().nth(p + 1))
                .map(|v| v.to_string());
            ifaces.push(NetIface {
                name,
                state,
                mtu,
                mac,
                addrs: Vec::new(),
                default: false,
            });
            cur = Some(ifaces.len() - 1);
            continue;
        }
        let idx = match cur {
            Some(v) => v,
            None => continue,
        };
        let toks: Vec<&str> = line.split_whitespace().collect();
        let head = toks.first().copied().unwrap_or("");
        let entry = &mut ifaces[idx];
        if head == "ether" || head == "HWaddr" {
            if let Some(v) = toks.get(1) {
                entry.mac = Some(v.to_string());
            }
            continue;
        }
        let is4 = head == "inet";
        let is6 = head == "inet6";
        if !is4 && !is6 {
            continue;
        }
        let mut address: Option<String> = None;
        let mut prefix: Option<u8> = None;
        let mut scope: Option<String> = None;
        let mut i = 1;
        while i < toks.len() {
            let t = toks[i];
            if let Some(addr) = t.strip_prefix("addr:") {
                address = Some(addr.to_string());
                i += 1;
                continue;
            }
            if let Some(mask) = t.strip_prefix("Mask:") {
                prefix = mask_prefix(mask);
                i += 1;
                continue;
            }
            if t == "netmask" {
                if let Some(v) = toks.get(i + 1) {
                    prefix = mask_prefix(v);
                }
                i += 2;
                continue;
            }
            if t == "prefixlen" {
                prefix = toks.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
                continue;
            }
            if t == "scopeid" {
                if let Some(v) = toks.get(i + 1) {
                    scope = v.split('<').nth(1).map(|s| s.trim_end_matches('>').to_string());
                }
                i += 2;
                continue;
            }
            if t == "scope" {
                scope = toks.get(i + 1).map(|v| v.to_string());
                i += 2;
                continue;
            }
            if i == 1 && address.is_none() && v_is_addr(t) {
                let (a, p) = match t.split_once('/') {
                    Some((a, p)) => (a.to_string(), p.parse().ok()),
                    None => (t.split('%').next().unwrap_or("").to_string(), None),
                };
                address = Some(a);
                if p.is_some() {
                    prefix = p;
                }
                i += 1;
                continue;
            }
            i += 1;
        }
        if let Some(address) = address {
            entry.addrs.push(NetAddr {
                family: if is4 { "ipv4" } else { "ipv6" }.to_string(),
                address,
                prefix,
                scope,
            });
        }
    }
}

fn v_is_addr(t: &str) -> bool {
    let a = t.split('/').next().unwrap_or("");
    a.parse::<std::net::Ipv4Addr>().is_ok() || a.parse::<std::net::Ipv6Addr>().is_ok()
}

/// 旧式路由：Linux `route -n`（8 列）与 macOS `netstat -rn`（目标/网关/标志/网卡）。
fn parse_legacy_routes(text: &str, routes: &mut Vec<NetRoute>) {
    for line in text.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.is_empty()
            || matches!(
                toks[0],
                "Destination" | "Kernel" | "Routing" | "Internet:" | "Internet6:" | "Netif"
            )
        {
            continue;
        }
        if toks.len() >= 8 {
            let dest = toks[0];
            let gw = toks[1];
            let mask = toks[2];
            let metric = toks[4].parse().ok();
            let iface = toks[7];
            let is_default = dest == "0.0.0.0" && mask == "0.0.0.0";
            let destination = if is_default {
                "default".to_string()
            } else {
                match mask_prefix(mask) {
                    Some(p) if p < 32 => format!("{dest}/{p}"),
                    _ => dest.to_string(),
                }
            };
            routes.push(NetRoute {
                family: "ipv4".into(),
                destination,
                gateway: opt(gw),
                device: opt(iface),
                protocol: None,
                scope: None,
                metric,
                src: None,
                default: is_default,
            });
            continue;
        }
        if toks.len() >= 4 && (toks[0] == "default" || v_is_addr(toks[0])) {
            let dest = toks[0];
            let gateway = toks.get(1).and_then(|g| opt(g));
            let device = toks
                .get(3)
                .filter(|t| looks_iface(t))
                .map(|v| v.to_string());
            routes.push(NetRoute {
                family: "ipv4".into(),
                destination: dest.to_string(),
                gateway,
                device,
                protocol: None,
                scope: None,
                metric: None,
                src: None,
                default: dest == "default",
            });
        }
    }
}

/// 形如 en0 / eth0 / lo0 的网卡名（macOS 与 Linux 通用形态）
fn looks_iface(t: &str) -> bool {
    let mut chars = t.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    t.len() <= 16
        && !t.contains(|c: char| matches!(c, '.' | ':' | '#' | '['))
        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn parse_dns(text: &str, dns: &mut NetDns) {
    for line in text.lines() {
        let line = match line.split('#').next() {
            Some(v) => v.trim(),
            None => continue,
        };
        let mut it = line.split_whitespace();
        match it.next() {
            Some("nameserver") => {
                if let Some(v) = it.next() {
                    dns.servers.push(v.to_string());
                }
            }
            Some("search") | Some("domain") => {
                dns.search.extend(it.map(|v| v.to_string()));
            }
            Some("options") => {
                dns.options.extend(it.map(|v| v.to_string()));
            }
            _ => {}
        }
    }
}

/// 解析整段输出（脚本任何一段缺失都按空处理，不报错）。
pub(super) fn parse(output: &str) -> NetworkInfo {
    let mut host = String::new();
    let mut diag = String::new();
    let mut link = String::new();
    let mut addr = String::new();
    let mut route = String::new();
    let mut route6 = String::new();
    let mut ifconfig = String::new();
    let mut netstat = String::new();
    let mut dns_text = String::new();
    let mut section = "";

    for line in output.lines() {
        let trimmed = line.trim_end();
        if let Some(name) = trimmed.strip_prefix("@@").and_then(|v| v.strip_suffix("@@")) {
            section = match name {
                "HOST" => "host",
                "DIAG" => "diag",
                "LINK" => "link",
                "ADDR" => "addr",
                "ROUTE" => "route",
                "ROUTE6" => "route6",
                "IFCONFIG" => "ifconfig",
                "NETSTAT" => "netstat",
                "DNS" => "dns",
                _ => "",
            };
            continue;
        }
        let target = match section {
            "host" => &mut host,
            "diag" => &mut diag,
            "link" => &mut link,
            "addr" => &mut addr,
            "route" => &mut route,
            "route6" => &mut route6,
            "ifconfig" => &mut ifconfig,
            "netstat" => &mut netstat,
            "dns" => &mut dns_text,
            _ => continue,
        };
        target.push_str(line);
        target.push('\n');
    }

    let mut tool = "none".to_string();
    let mut os = None;
    for line in diag.lines() {
        if let Some(v) = line.strip_prefix("TOOL=") {
            tool = v.trim().to_string();
        } else if !line.trim().is_empty() {
            os = Some(line.trim().to_string());
        }
    }

    let mut interfaces: Vec<NetIface> = Vec::new();
    let mut routes: Vec<NetRoute> = Vec::new();
    if tool == "ip" {
        for line in link.lines() {
            parse_link_line(line, &mut interfaces);
        }
        for line in addr.lines() {
            parse_addr_line(line, &mut interfaces);
        }
        for line in route.lines() {
            parse_route_line(line, "ipv4", &mut routes);
        }
        for line in route6.lines() {
            parse_route_line(line, "ipv6", &mut routes);
        }
    } else {
        parse_ifconfig(&ifconfig, &mut interfaces);
        parse_legacy_routes(&netstat, &mut routes);
        if !ifconfig.trim().is_empty() {
            tool = "legacy".into();
        }
    }

    let default_devs: Vec<String> = routes
        .iter()
        .filter(|r| r.default)
        .filter_map(|r| r.device.clone())
        .collect();
    for iface in &mut interfaces {
        iface.default = default_devs.iter().any(|d| *d == iface.name);
    }
    // 容器主机的 veth / br- 动辄上百个：默认路由网卡、有地址的网卡排前面，
    // 其余按名字排，避免物理网卡被虚拟网卡挤出首屏。
    interfaces.sort_by(|a, b| {
        b.default
            .cmp(&a.default)
            .then_with(|| (!b.addrs.is_empty()).cmp(&(!a.addrs.is_empty())))
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut dns = NetDns::default();
    parse_dns(&dns_text, &mut dns);

    let note = match tool.as_str() {
        "legacy" => Some(
            "远端没有 ip 命令，已按 ifconfig / route 解析；IPv6 路由可能不完整。".to_string(),
        ),
        "none" => Some("未检测到 ip 或 ifconfig，无法读取网卡与路由。".to_string()),
        _ => None,
    };

    NetworkInfo {
        hostname: opt(host.lines().next().unwrap_or("")),
        os,
        tool,
        interfaces,
        routes,
        dns,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"@@HOST@@
web01
@@DIAG@@
Linux 5.15.0-91-generic x86_64
TOOL=ip
@@LINK@@
1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536 qdisc noqueue state UNKNOWN mode DEFAULT group default qlen 1000\    link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00
2: eno1: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc mq state UP mode DEFAULT group default qlen 1000\    link/ether 78:e5:ef:12:34:56 brd ff:ff:ff:ff:ff:ff
3: eno2: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 9000 qdisc mq state UP mode DEFAULT group default qlen 1000\    link/ether 78:e5:ef:12:34:57 brd ff:ff:ff:ff:ff:ff
@@ADDR@@
1: lo    inet 127.0.0.1/8 scope host lo\       valid_lft forever preferred_lft forever
1: lo    inet6 ::1/128 scope host \       valid_lft forever preferred_lft forever
2: eno1    inet 172.17.4.2/22 brd 172.17.7.255 scope global eno1\       valid_lft forever preferred_lft forever
2: eno1    inet6 fe80::76e5:efff:fe12:3456/64 scope link \       valid_lft forever preferred_lft forever
3: eno2    inet 10.20.30.1/24 scope global secondary eno2\       valid_lft forever preferred_lft forever
@@ROUTE@@
default via 172.17.4.1 dev eno1 proto dhcp src 172.17.4.2 metric 100
172.17.4.0/22 dev eno1 proto kernel scope link src 172.17.4.2
10.20.30.0/24 dev eno2 proto kernel scope link src 10.20.30.1
@@ROUTE6@@
fe80::/64 dev eno1 proto kernel metric 256 pref medium
@@DNS@@
# generated by dhclient
nameserver 172.17.4.1
nameserver 2001:db8::53
search corp.example.com lab.example.com
options timeout:2 attempts:3
@@END@@
"#;

    #[test]
    fn parses_iproute2_output() {
        let info = parse(SAMPLE);
        assert_eq!(info.hostname.as_deref(), Some("web01"));
        assert_eq!(info.tool, "ip");
        assert_eq!(info.interfaces.len(), 3);
        // 默认路由网卡排最前，其次有地址的按名字
        let eno1 = &info.interfaces[0];
        assert_eq!(eno1.name, "eno1");
        assert_eq!(eno1.state, "UP");
        assert_eq!(eno1.mtu, Some(1500));
        assert_eq!(eno1.mac.as_deref(), Some("78:e5:ef:12:34:56"));
        assert!(eno1.default);
        assert_eq!(eno1.addrs.len(), 2);
        assert_eq!(eno1.addrs[0].address, "172.17.4.2");
        assert_eq!(eno1.addrs[0].prefix, Some(22));
        assert_eq!(eno1.addrs[0].scope.as_deref(), Some("global"));
        assert_eq!(eno1.addrs[1].family, "ipv6");
        assert_eq!(info.interfaces[2].name, "lo");
        let eno2 = &info.interfaces[1];
        assert_eq!(eno2.mtu, Some(9000));
        assert!(!eno2.default);
        assert_eq!(eno2.addrs[0].address, "10.20.30.1");

        assert_eq!(info.routes.len(), 4);
        assert!(info.routes[0].default);
        assert_eq!(info.routes[0].gateway.as_deref(), Some("172.17.4.1"));
        assert_eq!(info.routes[0].device.as_deref(), Some("eno1"));
        assert_eq!(info.routes[0].protocol.as_deref(), Some("dhcp"));
        assert_eq!(info.routes[0].metric, Some(100));
        assert_eq!(info.routes[0].src.as_deref(), Some("172.17.4.2"));
        assert_eq!(info.routes[3].family, "ipv6");

        assert_eq!(info.dns.servers, vec!["172.17.4.1", "2001:db8::53"]);
        assert_eq!(info.dns.search.len(), 2);
        assert_eq!(info.dns.options, vec!["timeout:2", "attempts:3"]);
        assert!(info.note.is_none());
    }

    const LEGACY: &str = r#"@@HOST@@
old01
@@DIAG@@
Linux 3.10.0-1160.el7.x86_64 x86_64
TOOL=legacy
@@IFCONFIG@@
eth0: flags=4163<UP,BROADCAST,RUNNING,MULTICAST>  mtu 1500
        inet 10.1.2.3  netmask 255.255.255.0  broadcast 10.1.2.255
        inet6 fe80::5054:ff:fe12:3456  prefixlen 64  scopeid 0x20<link>
        ether 52:54:00:12:34:56  txqueuelen 1000  (Ethernet)
eth1: flags=4098<BROADCAST,MULTICAST>  mtu 1500
        ether 52:54:00:12:34:57  txqueuelen 1000  (Ethernet)
@@NETSTAT@@
Kernel IP routing table
Destination     Gateway         Genmask         Flags Metric Ref    Use Iface
0.0.0.0         10.1.2.1        0.0.0.0         UG    100    0        0 eth0
10.1.2.0        0.0.0.0         255.255.255.0   U     100    0        0 eth0
@@DNS@@
nameserver 10.1.2.1
@@END@@
"#;

    #[test]
    fn parses_legacy_ifconfig_and_route() {
        let info = parse(LEGACY);
        assert_eq!(info.tool, "legacy");
        assert!(info.note.unwrap().contains("ifconfig"));
        assert_eq!(info.interfaces.len(), 2);
        let eth0 = &info.interfaces[0];
        assert_eq!(eth0.state, "UP");
        assert!(eth0.default);
        assert_eq!(eth0.addrs[0].prefix, Some(24));
        assert_eq!(eth0.addrs[0].address, "10.1.2.3");
        assert_eq!(eth0.addrs[1].family, "ipv6");
        assert_eq!(eth0.addrs[1].prefix, Some(64));
        assert_eq!(eth0.addrs[1].scope.as_deref(), Some("link"));
        assert_eq!(eth0.mac.as_deref(), Some("52:54:00:12:34:56"));
        assert_eq!(info.interfaces[1].state, "DOWN");

        assert_eq!(info.routes.len(), 2);
        assert!(info.routes[0].default);
        assert_eq!(info.routes[0].gateway.as_deref(), Some("10.1.2.1"));
        assert_eq!(info.routes[1].destination, "10.1.2.0/24");
        assert_eq!(info.routes[1].gateway, None);
    }

    #[test]
    fn mask_prefix_handles_hex_and_dotted() {
        assert_eq!(mask_prefix("255.255.255.0"), Some(24));
        assert_eq!(mask_prefix("0xffffff00"), Some(24));
        assert_eq!(mask_prefix("255.255.0.0"), Some(16));
        assert_eq!(mask_prefix("junk"), None);
    }

    #[test]
    fn empty_output_yields_none_tool() {
        let info = parse("");
        assert_eq!(info.tool, "none");
        assert!(info.interfaces.is_empty());
        assert!(info.note.is_some());
    }
}
