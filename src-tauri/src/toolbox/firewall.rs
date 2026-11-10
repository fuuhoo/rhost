//! 防火墙管理：探测 ufw → firewalld → iptables，解析规则并生成增删/启停脚本。
//!
//! 所有远端命令都由后端按结构化字段重建，前端传来的端口/协议/来源先经过
//! 严格校验（端口范围、协议枚举、IP/CIDR），任何字段都不直接拼进 shell。
//! iptables 只支持可还原的端口规则；DOCKER 链、日志规则等只读展示、不可删。

use serde::Serialize;

use crate::ssh::session::ExecOutput;

pub(super) const STATUS_SCRIPT: &str = r#"if command -v ufw >/dev/null 2>&1; then
  echo 'FW=ufw'
  ufw status verbose 2>/dev/null
elif command -v firewall-cmd >/dev/null 2>&1; then
  echo 'FW=firewalld'
  echo "STATE=$(firewall-cmd --state 2>/dev/null)"
  echo 'PORTS:'
  firewall-cmd --list-ports 2>/dev/null
  echo 'RICH:'
  firewall-cmd --list-rich-rules 2>/dev/null
elif command -v iptables >/dev/null 2>&1; then
  echo 'FW=iptables'
  iptables -S INPUT 2>/dev/null
else
  echo 'FW=none'
fi
"#;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FirewallRule {
    /// allow / deny / other
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// 规则来源：ufw / port / rich / iptables（删除时据此选择命令，避免把
    /// firewalld 的 deny rich 规则误当成简单端口规则去 remove-port）
    pub origin: String,
    pub raw: String,
    pub deletable: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FirewallStatus {
    /// ufw / firewalld / iptables / none
    pub backend: String,
    pub enabled: bool,
    pub rules: Vec<FirewallRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub(super) fn parse_status(output: &str) -> Result<FirewallStatus, String> {
    let mut lines = output.lines();
    let backend = lines
        .next()
        .and_then(|l| l.strip_prefix("FW="))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "none".into());
    let rest: Vec<&str> = lines.collect();
    Ok(match backend.as_str() {
        "ufw" => parse_ufw(&rest),
        "firewalld" => parse_firewalld(&rest),
        "iptables" => parse_iptables(&rest),
        _ => FirewallStatus {
            backend: "none".into(),
            enabled: false,
            rules: Vec::new(),
            note: Some("远端未检测到 ufw / firewalld / iptables".into()),
        },
    })
}

fn parse_ufw(lines: &[&str]) -> FirewallStatus {
    let mut enabled = false;
    let mut in_rules = false;
    let mut rules = Vec::new();
    for line in lines {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("Status:") {
            enabled = v.trim().starts_with("active");
            continue;
        }
        if t.starts_with("To ") {
            in_rules = true;
            continue;
        }
        if !in_rules || t.is_empty() {
            continue;
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        let Some(action_idx) = f
            .iter()
            .position(|w| matches!(*w, "ALLOW" | "DENY" | "REJECT" | "LIMIT"))
        else {
            continue;
        };
        let action = match f[action_idx] {
            "ALLOW" => "allow",
            "DENY" | "REJECT" => "deny",
            _ => "other",
        };
        // 端口列：动作前的第一个非 (v6) token；应用名（如 Apache Full）不是端口
        let port_token = f[..action_idx].iter().find(|w| **w != "(v6)").copied();
        let (port, protocol) = port_token
            .and_then(parse_port_proto)
            .map(|(p, pr)| (Some(p), Some(pr)))
            .unwrap_or((None, None));
        let mut source_tokens = Vec::new();
        let mut direction = "IN";
        let mut has_iface = false;
        for w in &f[action_idx + 1..] {
            match *w {
                "(v6)" => {}
                "IN" | "OUT" => direction = w,
                "on" => has_iface = true,
                _ => source_tokens.push(*w),
            }
        }
        let source = source_tokens.join(" ");
        let source = if source.is_empty() || source == "Anywhere" {
            None
        } else {
            Some(source)
        };
        let deletable = port.is_some()
            && direction == "IN"
            && !has_iface
            && matches!(action, "allow" | "deny")
            && source.as_deref().is_none_or(|s| validate_source(s).is_ok());
        rules.push(FirewallRule {
            action: action.into(),
            port,
            protocol,
            source,
            origin: "ufw".into(),
            raw: t.to_string(),
            deletable,
        });
    }
    FirewallStatus {
        backend: "ufw".into(),
        enabled,
        rules,
        note: None,
    }
}

fn parse_firewalld(lines: &[&str]) -> FirewallStatus {
    let mut state = String::new();
    let mut section = "";
    let mut ports = Vec::new();
    let mut rich_lines = Vec::new();
    for line in lines {
        let t = line.trim();
        if let Some(s) = t.strip_prefix("STATE=") {
            state = s.trim().to_string();
            continue;
        }
        if t == "PORTS:" {
            section = "ports";
            continue;
        }
        if t == "RICH:" {
            section = "rich";
            continue;
        }
        if t.is_empty() {
            continue;
        }
        match section {
            "ports" => ports.extend(t.split_whitespace().map(|s| s.to_string())),
            "rich" => rich_lines.push(t.to_string()),
            _ => {}
        }
    }
    let mut rules = Vec::new();
    for p in ports {
        if let Some((port, protocol)) = parse_port_proto(&p) {
            rules.push(FirewallRule {
                action: "allow".into(),
                port: Some(port),
                protocol: Some(protocol),
                source: None,
                origin: "port".into(),
                raw: p,
                deletable: true,
            });
        }
    }
    for line in rich_lines {
        rules.push(parse_rich_rule(&line));
    }
    FirewallStatus {
        backend: "firewalld".into(),
        enabled: state == "running",
        rules,
        note: Some("仅显示默认区域的端口与富规则；区域/服务等其它配置不在本工具范围".into()),
    }
}

/// 富规则解析：只识别 `rule family source address port port protocol accept/drop/reject`
/// 这一种可还原形态，其余（destination/log/limit/ipset…）只读展示。
fn parse_rich_rule(line: &str) -> FirewallRule {
    let words = split_words(line);
    let mut supported = true;
    for w in &words {
        let ok = w == "rule"
            || w == "source"
            || w == "port"
            || matches!(w.as_str(), "accept" | "drop" | "reject")
            || w.starts_with("family=")
            || w.starts_with("address=")
            || w.starts_with("port=")
            || w.starts_with("protocol=");
        if !ok {
            supported = false;
            break;
        }
    }
    let has_source = words.iter().any(|w| w == "source");
    let source = if has_source {
        attr(&words, "address")
    } else {
        None
    };
    let port = attr(&words, "port").and_then(|p| {
        let p = p.replace('-', ":");
        parse_port_spec(&p).ok().map(|_| p)
    });
    let protocol = attr(&words, "protocol").filter(|p| p == "tcp" || p == "udp");
    let action = match words.last().map(|s| s.as_str()) {
        Some("accept") => "allow",
        Some("drop") | Some("reject") => "deny",
        _ => "other",
    };
    let deletable = supported
        && has_source == source.is_some()
        && port.is_some()
        && protocol.is_some()
        && matches!(action, "allow" | "deny")
        && source.as_deref().is_none_or(|s| validate_source(s).is_ok());
    FirewallRule {
        action: action.into(),
        port,
        protocol,
        source,
        origin: "rich".into(),
        raw: line.to_string(),
        deletable,
    }
}

fn parse_iptables(lines: &[&str]) -> FirewallStatus {
    let mut policy = "ACCEPT".to_string();
    let mut rules = Vec::new();
    for line in lines {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("-P INPUT ") {
            policy = v.trim().to_string();
            continue;
        }
        if !t.starts_with("-A INPUT") {
            continue;
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        let mut port = None;
        let mut protocol = None;
        let mut source = None;
        let mut action = "other".to_string();
        let mut supported = true;
        let mut i = 2; // 跳过 -A INPUT
        while i < f.len() {
            match f[i] {
                "-p" => {
                    protocol = f.get(i + 1).copied().and_then(normalize_proto);
                    i += 2;
                }
                "-s" => {
                    source = f.get(i + 1).map(|s| (*s).to_string());
                    i += 2;
                }
                "--dport" => {
                    port = f.get(i + 1).map(|s| (*s).to_string());
                    i += 2;
                }
                "-m" => {
                    // -m tcp/udp 是配套的隐式 match，删除时按 proto 重建
                    i += 2;
                }
                "-j" => {
                    action = match f.get(i + 1).copied() {
                        Some("ACCEPT") => "allow",
                        Some("DROP") | Some("REJECT") => "deny",
                        _ => "other",
                    }
                    .into();
                    i += 2;
                }
                other => {
                    if other.starts_with('-') {
                        supported = false;
                        // 未知长选项通常带一个参数，尽量跳过避免错位
                        if other.starts_with("--") {
                            i += 2;
                        } else {
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
        }
        let deletable = supported
            && port.is_some()
            && protocol.is_some()
            && matches!(action.as_str(), "allow" | "deny")
            && source.as_deref().is_none_or(|s| validate_source(s).is_ok());
        rules.push(FirewallRule {
            action,
            port,
            protocol,
            source,
            origin: "iptables".into(),
            raw: t.to_string(),
            deletable,
        });
    }
    FirewallStatus {
        backend: "iptables".into(),
        enabled: policy != "ACCEPT" || !rules.is_empty(),
        rules,
        note: Some(
            "iptables 规则即时生效；重启后是否保留取决于系统是否保存规则；本工具不管理启停"
                .into(),
        ),
    }
}

fn normalize_proto(raw: &str) -> Option<String> {
    let r = raw.to_ascii_lowercase();
    if r.starts_with("tcp") {
        Some("tcp".into())
    } else if r.starts_with("udp") {
        Some("udp".into())
    } else {
        None
    }
}

fn split_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn attr(words: &[String], key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    words
        .iter()
        .find(|w| w.starts_with(&prefix))
        .map(|w| w[prefix.len()..].trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
}

/* =========================================================
 *  参数校验
 * ========================================================= */

pub(super) fn validate_port_spec(s: &str) -> Result<String, String> {
    let t = s.trim();
    let t = t.replace('-', ":");
    parse_port_spec(&t).map(|_| t)
}

fn parse_port_spec(s: &str) -> Result<(), String> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.is_empty() || parts.len() > 2 {
        return Err(format!("端口格式不合法：{s}（示例 80 或 8080:8090）"));
    }
    let mut nums = Vec::new();
    for p in parts {
        let n: u16 = p
            .parse()
            .map_err(|_| format!("端口格式不合法：{s}（示例 80 或 8080:8090）"))?;
        if n == 0 {
            return Err("端口必须在 1~65535 之间".into());
        }
        nums.push(n);
    }
    if nums.len() == 2 && nums[1] < nums[0] {
        return Err("端口范围起始值不能大于结束值".into());
    }
    Ok(())
}

/// `80/tcp` / `8080:8090/tcp` → ("80", "tcp")；无协议视为 any
fn parse_port_proto(s: &str) -> Option<(String, String)> {
    let (port, proto) = match s.split_once('/') {
        Some((p, pr)) => (p, pr),
        None => (s, "any"),
    };
    let port = port.replace('-', ":");
    parse_port_spec(&port).ok()?;
    let proto = match proto.to_ascii_lowercase().as_str() {
        "tcp" => "tcp",
        "udp" => "udp",
        "any" | "both" | "" => "any",
        _ => return None,
    };
    Some((port, proto.into()))
}

pub(super) fn validate_protocol(s: &str) -> Result<&'static str, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "tcp" => Ok("tcp"),
        "udp" => Ok("udp"),
        "both" | "any" => Ok("both"),
        _ => Err(format!("协议仅支持 tcp / udp / both：{s}")),
    }
}

pub(super) fn validate_action(s: &str) -> Result<&'static str, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "allow" => Ok("allow"),
        "deny" => Ok("deny"),
        _ => Err(format!("动作仅支持 allow / deny：{s}")),
    }
}

pub(super) fn validate_source(s: &str) -> Result<Option<String>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let (ip_part, prefix) = match s.split_once('/') {
        Some((ip, p)) => (ip, Some(p)),
        None => (s, None),
    };
    let ip: std::net::IpAddr = ip_part
        .parse()
        .map_err(|_| format!("来源地址不合法（仅支持 IP / CIDR）：{s}"))?;
    if let Some(p) = prefix {
        let n: u8 = p
            .parse()
            .map_err(|_| format!("CIDR 前缀不合法：{s}"))?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        if n == 0 || n > max {
            return Err(format!("CIDR 前缀超出范围：{s}"));
        }
    }
    Ok(Some(s.to_string()))
}

/* =========================================================
 *  命令构造
 * ========================================================= */

fn protos(protocol: &str) -> &'static [&'static str] {
    match protocol {
        "tcp" => &["tcp"],
        "udp" => &["udp"],
        _ => &["tcp", "udp"],
    }
}

fn firewalld_port(port: &str) -> String {
    port.replace(':', "-")
}

fn iptables_target(action: &str) -> &'static str {
    if action == "allow" {
        "ACCEPT"
    } else {
        "DROP"
    }
}

fn firewalld_action(action: &str) -> &'static str {
    if action == "allow" {
        "accept"
    } else {
        "drop"
    }
}

pub(super) fn build_add_script(
    port: &str,
    protocol: &str,
    action: &str,
    source: Option<&str>,
) -> Result<String, String> {
    let mut ufw_cmds = Vec::new();
    let mut fw_cmds = Vec::new();
    let mut ipt_cmds = Vec::new();
    for proto in protos(protocol) {
        match (action, source) {
            ("allow", None) => ufw_cmds.push(format!("ufw allow {port}/{proto}")),
            ("deny", None) => ufw_cmds.push(format!("ufw deny {port}/{proto}")),
            ("allow", Some(src)) => {
                ufw_cmds.push(format!("ufw allow from {src} to any port {port} proto {proto}"))
            }
            ("deny", Some(src)) => {
                ufw_cmds.push(format!("ufw deny from {src} to any port {port} proto {proto}"))
            }
            _ => return Err("不支持的动作".into()),
        }

        let fw_port = firewalld_port(port);
        match source {
            None if action == "allow" => {
                fw_cmds.push(format!("firewall-cmd --permanent --add-port={fw_port}/{proto}"));
            }
            _ => {
                let family = if source.is_some_and(|s| s.contains(':')) {
                    "ipv6"
                } else {
                    "ipv4"
                };
                let src = source
                    .map(|s| format!(" source address=\"{s}\""))
                    .unwrap_or_default();
                fw_cmds.push(format!(
                    "firewall-cmd --permanent --add-rich-rule='rule family=\"{family}\"{src} port port=\"{fw_port}\" protocol=\"{proto}\" {}'",
                    firewalld_action(action)
                ));
            }
        }
        fw_cmds.push("firewall-cmd --reload".into());

        let src = source.map(|s| format!(" -s {s}")).unwrap_or_default();
        ipt_cmds.push(format!(
            "iptables -I INPUT{src} -p {proto} -m {proto} --dport {port} -j {}",
            iptables_target(action)
        ));
    }
    Ok(backend_script(
        &ufw_cmds.join(" && "),
        &fw_cmds.join(" && "),
        &ipt_cmds.join(" && "),
    ))
}

pub(super) fn build_remove_script(
    port: &str,
    protocol: &str,
    action: &str,
    source: Option<&str>,
    origin: &str,
) -> Result<String, String> {
    let mut cmds = Vec::new();
    match origin {
        "ufw" => {
            for proto in protos(protocol) {
                let spec = if protocol == "both" {
                    format!("{port}/{proto}")
                } else if protocol == "tcp" || protocol == "udp" {
                    format!("{port}/{proto}")
                } else {
                    port.to_string()
                };
                match source {
                    Some(src) => cmds.push(format!(
                        "ufw --force delete {action} from {src} to any port {port} proto {proto}"
                    )),
                    None => cmds.push(format!("ufw --force delete {action} {spec}")),
                }
            }
            Ok(format!(
                "command -v ufw >/dev/null 2>&1 || {{ echo '远端已无 ufw，请刷新后重试' >&2; exit 1; }}; {}",
                cmds.join(" && ")
            ))
        }
        "port" => {
            for proto in protos(protocol) {
                let p = if *proto == "tcp" { "tcp" } else { "udp" };
                cmds.push(format!(
                    "firewall-cmd --permanent --remove-port={}/{}",
                    firewalld_port(port),
                    p
                ));
            }
            cmds.push("firewall-cmd --reload".into());
            Ok(format!(
                "command -v firewall-cmd >/dev/null 2>&1 || {{ echo '远端已无 firewalld，请刷新后重试' >&2; exit 1; }}; {}",
                cmds.join(" && ")
            ))
        }
        "rich" => {
            for proto in protos(protocol) {
                let family = if source.is_some_and(|s| s.contains(':')) {
                    "ipv6"
                } else {
                    "ipv4"
                };
                let src = source
                    .map(|s| format!(" source address=\"{s}\""))
                    .unwrap_or_default();
                cmds.push(format!(
                    "firewall-cmd --permanent --remove-rich-rule='rule family=\"{family}\"{src} port port=\"{}\" protocol=\"{proto}\" {}'",
                    firewalld_port(port),
                    firewalld_action(action)
                ));
            }
            cmds.push("firewall-cmd --reload".into());
            Ok(format!(
                "command -v firewall-cmd >/dev/null 2>&1 || {{ echo '远端已无 firewalld，请刷新后重试' >&2; exit 1; }}; {}",
                cmds.join(" && ")
            ))
        }
        "iptables" => {
            for proto in protos(protocol) {
                let src = source.map(|s| format!(" -s {s}")).unwrap_or_default();
                cmds.push(format!(
                    "iptables -D INPUT{src} -p {proto} -m {proto} --dport {port} -j {}",
                    iptables_target(action)
                ));
            }
            Ok(format!(
                "command -v iptables >/dev/null 2>&1 || {{ echo '远端已无 iptables，请刷新后重试' >&2; exit 1; }}; {}",
                cmds.join(" && ")
            ))
        }
        _ => Err(format!("未知规则来源：{origin}")),
    }
}

fn backend_script(ufw: &str, firewalld: &str, iptables: &str) -> String {
    format!(
        "if command -v ufw >/dev/null 2>&1; then {ufw}; \
         elif command -v firewall-cmd >/dev/null 2>&1; then {firewalld}; \
         elif command -v iptables >/dev/null 2>&1; then {iptables}; \
         else echo '未检测到受支持的防火墙后端（ufw / firewalld / iptables）' >&2; exit 1; fi"
    )
}

/// 启用/停用脚本；iptables 没有可启停的服务，直接报错。
pub(super) fn build_toggle_script(
    status: &FirewallStatus,
    enable: bool,
    ssh_port: u16,
) -> Result<String, String> {
    match status.backend.as_str() {
        "ufw" => {
            if !enable {
                return Ok("ufw --force disable".into());
            }
            if ssh_rule_missing(status, ssh_port) {
                Ok(format!(
                    "ufw allow {ssh_port}/tcp && ufw --force enable"
                ))
            } else {
                Ok("ufw --force enable".into())
            }
        }
        "firewalld" => {
            if !enable {
                return Ok("systemctl stop firewalld; systemctl disable firewalld".into());
            }
            let mut cmds = vec!["systemctl start firewalld".to_string()];
            if ssh_rule_missing(status, ssh_port) {
                cmds.push(format!(
                    "firewall-cmd --permanent --add-port={ssh_port}/tcp"
                ));
            }
            cmds.push("firewall-cmd --reload".into());
            cmds.push("systemctl enable firewalld".into());
            Ok(cmds.join(" && "))
        }
        "iptables" => Err(
            "当前后端为 iptables，没有可启停的服务；规则即时生效，请直接增删规则".into(),
        ),
        _ => Err("未检测到可管理的防火墙后端（ufw / firewalld）".into()),
    }
}

/// SSH 端口是否已有放行规则（用于启用前自动补放行）
pub(super) fn ssh_rule_missing(status: &FirewallStatus, ssh_port: u16) -> bool {
    let port = ssh_port.to_string();
    !status.rules.iter().any(|r| {
        r.action == "allow"
            && r.port.as_deref() == Some(port.as_str())
            && matches!(r.protocol.as_deref(), Some("tcp") | Some("any"))
    })
}

/// 变更命令的判定：超时/非零退出都算失败，输出非空则附在错误里。
pub(super) fn change_result(out: &ExecOutput, op: &str) -> Result<String, String> {
    if out.timed_out {
        return Err(format!("{op}超时，远端状态可能已变化，请刷新确认"));
    }
    if out.exit_code == Some(0) {
        return Ok(format!("{op}成功"));
    }
    let detail = out.output.trim();
    if detail.is_empty() {
        Err(format!(
            "{op}失败（退出码 {:?}）",
            out.exit_code.unwrap_or(-1)
        ))
    } else {
        Err(format!("{op}失败：{detail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ufw_status() {
        let out = "\
FW=ufw
Status: active
Logging: on (low)
Default: deny (incoming), allow (outgoing), disabled (routed)
New profiles: skip

To                         Action      From
--                         ------      ----
22/tcp                     ALLOW IN    Anywhere
80,443/tcp                 ALLOW IN    Anywhere
3306/tcp                   DENY IN     10.20.3.0/24
8080:8090/tcp              ALLOW IN    Anywhere
22/tcp (v6)                ALLOW IN    Anywhere (v6)
";
        let s = parse_status(out).unwrap();
        assert_eq!(s.backend, "ufw");
        assert!(s.enabled);
        assert_eq!(s.rules.len(), 5);
        assert_eq!(s.rules[0].port.as_deref(), Some("22"));
        assert_eq!(s.rules[2].action, "deny");
        assert_eq!(s.rules[2].source.as_deref(), Some("10.20.3.0/24"));
        assert_eq!(s.rules[3].port.as_deref(), Some("8080:8090"));
        assert!(s.rules[3].deletable);
    }

    #[test]
    fn parses_firewalld_ports_and_rich_rules() {
        let out = "\
FW=firewalld
STATE=running
PORTS:
80/tcp 443/tcp
RICH:
rule family=\"ipv4\" source address=\"10.0.0.0/8\" port port=\"3306\" protocol=\"tcp\" drop
";
        let s = parse_status(out).unwrap();
        assert_eq!(s.backend, "firewalld");
        assert!(s.enabled);
        assert_eq!(s.rules.len(), 3);
        assert_eq!(s.rules[0].origin, "port");
        assert_eq!(s.rules[2].action, "deny");
        assert_eq!(s.rules[2].origin, "rich");
        assert!(s.rules[2].deletable);
    }

    #[test]
    fn parses_iptables_chain() {
        let out = "\
FW=iptables
-P INPUT ACCEPT
-A INPUT -p tcp -m tcp --dport 22 -j ACCEPT
-A INPUT -s 10.0.0.0/8 -p tcp -m tcp --dport 3306 -j DROP
-A INPUT -j DOCKER-USER
-A INPUT -i lo -j ACCEPT
";
        let s = parse_status(out).unwrap();
        assert_eq!(s.backend, "iptables");
        assert!(s.enabled);
        assert_eq!(s.rules.len(), 4);
        assert!(s.rules[0].deletable);
        assert_eq!(s.rules[1].source.as_deref(), Some("10.0.0.0/8"));
        assert!(!s.rules[2].deletable);
        assert!(!s.rules[3].deletable);
    }

    #[test]
    fn validates_ports_protocols_actions_sources() {
        assert!(validate_port_spec("80").is_ok());
        assert!(validate_port_spec("8080:8090").is_ok());
        assert!(validate_port_spec("8080-8090").is_ok());
        assert!(validate_port_spec("0").is_err());
        assert!(validate_port_spec("70000").is_err());
        assert!(validate_port_spec("80; rm -rf /").is_err());
        assert!(validate_port_spec("90:80").is_err());
        assert!(validate_protocol("both").is_ok());
        assert!(validate_protocol("icmp").is_err());
        assert!(validate_action("ALLOW").is_ok());
        assert!(validate_action("ACCEPT").is_err());
        assert!(validate_source("10.0.0.0/8").unwrap().is_some());
        assert!(validate_source("2001:db8::/32").unwrap().is_some());
        assert!(validate_source("").unwrap().is_none());
        assert!(validate_source("1.2.3.4; reboot").is_err());
        assert!(validate_source("10.0.0.0/33").is_err());
    }

    #[test]
    fn add_script_never_interpolates_unvalidated_input() {
        let script = build_add_script("8080:8090", "both", "allow", Some("10.0.0.0/8")).unwrap();
        assert!(script.contains("ufw allow from 10.0.0.0/8 to any port 8080:8090 proto tcp"));
        assert!(script.contains("--add-rich-rule='rule family=\"ipv4\" source address=\"10.0.0.0/8\" port port=\"8080-8090\" protocol=\"tcp\" accept'"));
        assert!(script.contains("iptables -I INPUT -s 10.0.0.0/8 -p udp -m udp --dport 8080:8090 -j ACCEPT"));
    }

    #[test]
    fn remove_script_uses_rule_origin() {
        let ufw = build_remove_script("80", "tcp", "allow", None, "ufw").unwrap();
        assert!(ufw.contains("ufw --force delete allow 80/tcp"));
        let rich = build_remove_script("3306", "tcp", "deny", Some("10.0.0.0/8"), "rich").unwrap();
        assert!(rich.contains("--remove-rich-rule='rule family=\"ipv4\" source address=\"10.0.0.0/8\" port port=\"3306\" protocol=\"tcp\" drop'"));
        let ipt = build_remove_script("22", "tcp", "allow", None, "iptables").unwrap();
        assert!(ipt.contains("iptables -D INPUT -p tcp -m tcp --dport 22 -j ACCEPT"));
    }

    #[test]
    fn toggle_auto_allows_ssh_port() {
        let status = FirewallStatus {
            backend: "ufw".into(),
            enabled: false,
            rules: Vec::new(),
            note: None,
        };
        let script = build_toggle_script(&status, true, 2222).unwrap();
        assert!(script.contains("ufw allow 2222/tcp"));
        assert!(script.contains("ufw --force enable"));

        let with_rule = FirewallStatus {
            backend: "ufw".into(),
            enabled: false,
            rules: vec![FirewallRule {
                action: "allow".into(),
                port: Some("2222".into()),
                protocol: Some("tcp".into()),
                source: None,
                origin: "ufw".into(),
                raw: "2222/tcp ALLOW Anywhere".into(),
                deletable: true,
            }],
            note: None,
        };
        assert!(!ssh_rule_missing(&with_rule, 2222));
        assert!(build_toggle_script(&with_rule, true, 2222)
            .unwrap()
            .contains("ufw --force enable"));
        assert!(build_toggle_script(&status, false, 2222)
            .unwrap()
            .contains("disable"));
    }
}
