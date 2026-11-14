//! 服务器时间与 NTP 时间同步：读取远端时间、探测 chrony / ntpd / systemd-timesyncd，
//! 安装 chrony、替换时间源、立即校时与启停服务。
//!
//! 读取走独立 exec 通道（不经 PTY、不污染终端），全部为只读命令；安装与配置属于
//! 系统变更，只由用户显式点击触发，且命令由后端按结构化字段重建——服务器地址先
//! 经过严格校验，前端文本不会拼进 shell。配置文件改动前保留 `.rhost.bak` 备份。

use serde::Serialize;

use super::sq;

/// 状态探测脚本：以 marker 分段，任意一段缺失不影响其他段。
pub(super) const STATUS_SCRIPT: &str = r##"echo '@@OS@@'
uname -s 2>/dev/null
echo '@@OSREL@@'
cat /etc/os-release 2>/dev/null
echo '@@NOW@@'
date '+%s|%z|%Z|%Y-%m-%d %H:%M:%S' 2>/dev/null
echo '@@TOOLS@@'
for c in chronyc chronyd ntpd ntpq timedatectl systemctl; do
  if command -v "$c" >/dev/null 2>&1; then echo "$c=1"; else echo "$c=0"; fi
done
echo '@@SERVICES@@'
if command -v systemctl >/dev/null 2>&1; then
  for s in chronyd chrony ntp ntpd systemd-timesyncd; do
    st=$(systemctl is-active "$s" 2>/dev/null)
    en=$(systemctl is-enabled "$s" 2>/dev/null)
    echo "$s|${st:-unknown}|${en:-unknown}"
  done
fi
echo '@@OPENRC@@'
if command -v rc-service >/dev/null 2>&1; then
  for s in chronyd ntp; do
    echo "$s|$(rc-service "$s" status 2>&1 | tail -1)"
  done
fi
echo '@@TIMEDATECTL@@'
timedatectl show -p NTPSynchronized --value 2>/dev/null
echo '@@CHRONY_TRACK@@'
chronyc tracking 2>/dev/null
echo '@@NTPQ@@'
ntpq -pn 2>/dev/null
echo '@@CHRONY_CONF@@'
for f in /etc/chrony.conf /etc/chrony/chrony.conf; do
  [ -f "$f" ] && { echo "FILE $f"; cat "$f" 2>/dev/null; }
done
echo '@@NTP_CONF@@'
for f in /etc/ntp.conf /etc/ntp/ntp.conf; do
  [ -f "$f" ] && { echo "FILE $f"; cat "$f" 2>/dev/null; }
done
echo '@@TIMESYNCD_CONF@@'
[ -f /etc/systemd/timesyncd.conf ] && { echo "FILE /etc/systemd/timesyncd.conf"; cat /etc/systemd/timesyncd.conf 2>/dev/null; }
echo '@@PKG@@'
for c in apt-get dnf yum zypper pacman apk; do
  command -v "$c" >/dev/null 2>&1 && { echo "$c"; break; }
done
echo '@@END@@'
"##;

/// 单次服务器时间读取（状态栏分钟级校时用）
pub(super) const SERVER_TIME_SCRIPT: &str = "date '+%s|%z|%Z|%Y-%m-%d %H:%M:%S' 2>/dev/null";

/// 服务器时间与时区
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerTime {
    /// Unix 毫秒
    pub epoch_ms: i64,
    /// 时区偏移（分钟，东八区为 480）
    pub tz_offset_minutes: i32,
    /// 时区缩写（CST / UTC）
    pub tz_name: String,
    /// 远端本地时间 `YYYY-MM-DD HH:MM:SS`
    pub formatted: String,
}

/// NTP 状态
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NtpStatus {
    pub os: String,
    pub distro: String,
    /// chrony / ntpd / timesyncd / none
    pub service: String,
    pub installed: bool,
    pub active: bool,
    pub enabled: bool,
    /// 已与上游同步；读不到时为 None
    pub synchronized: Option<bool>,
    /// 当前偏差（毫秒，正数表示本机快于上游）
    pub offset_ms: Option<f64>,
    /// 配置里的时间源
    pub servers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    /// apt-get / dnf / yum / zypper / pacman / apk / none
    pub package_manager: String,
    /// 是否支持安装/配置（macOS 与未知平台为 false）
    pub managed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub now: ServerTime,
}

/// 解析 `date '+%s|%z|%Z|%Y-%m-%d %H:%M:%S'` 的输出
///
/// exec 通道可能先带上登录 shell 的启动噪声（如 profile 报错），因此不取首行，
/// 而是挑第一条「4 段且首段是整数」的行。
pub(super) fn parse_server_time(output: &str) -> Result<ServerTime, String> {
    let line = output
        .lines()
        .map(str::trim)
        .find(|l| {
            let mut it = l.splitn(4, '|');
            let epoch = it.next().map_or("", str::trim);
            epoch.parse::<i64>().is_ok() && it.count() == 3
        })
        .ok_or_else(|| "读取服务器时间失败".to_string())?;
    let mut parts = line.splitn(4, '|');
    let epoch = parts.next().unwrap_or("").trim();
    let tz = parts.next().unwrap_or("").trim();
    let tz_name = parts.next().unwrap_or("").trim();
    let formatted = parts.next().unwrap_or("").trim();
    let secs: i64 = epoch
        .parse()
        .map_err(|_| format!("无法解析服务器时间：{line}"))?;
    Ok(ServerTime {
        epoch_ms: secs * 1000,
        tz_offset_minutes: parse_tz_offset(tz),
        tz_name: if tz_name.is_empty() {
            tz.to_string()
        } else {
            tz_name.to_string()
        },
        formatted: formatted.to_string(),
    })
}

/// `+0800` / `-0530` → 分钟
fn parse_tz_offset(tz: &str) -> i32 {
    let bytes = tz.as_bytes();
    if bytes.len() < 5 {
        return 0;
    }
    let sign = if bytes[0] == b'-' { -1 } else { 1 };
    let hh: i32 = tz[1..3].parse().unwrap_or(0);
    let mm: i32 = tz[3..5].parse().unwrap_or(0);
    sign * (hh * 60 + mm)
}

/// 解析状态探测输出
pub(super) fn parse_status(output: &str) -> Result<NtpStatus, String> {
    let mut sections: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    let mut current: Option<&str> = None;
    for line in output.lines() {
        let t = line.trim_end_matches('\r');
        if let Some(name) = t.strip_prefix("@@").and_then(|x| x.strip_suffix("@@")) {
            if name == "END" {
                current = None;
            } else {
                current = Some(name);
                sections.entry(name).or_default();
            }
            continue;
        }
        if let Some(key) = current {
            sections.entry(key).or_default().push(t);
        }
    }

    let section = |name: &str| -> String {
        sections
            .get(name)
            .map(|lines| lines.join("\n"))
            .unwrap_or_default()
    };

    let os = section("OS").trim().to_string();
    let now = parse_server_time(&section("NOW"))?;

    let distro = parse_distro(&section("OSREL"), &os);

    let mut has = std::collections::HashMap::new();
    for line in section("TOOLS").lines() {
        if let Some((k, v)) = line.split_once('=') {
            has.insert(k.trim().to_string(), v.trim() == "1");
        }
    }
    let tool = |name: &str| -> bool { has.get(name).copied().unwrap_or(false) };

    let mut unit_state: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    for line in section("SERVICES").lines() {
        let mut it = line.split('|');
        let name = it.next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let active = it.next().unwrap_or("unknown").trim().to_string();
        let enabled = it.next().unwrap_or("unknown").trim().to_string();
        unit_state.insert(name.to_string(), (active, enabled));
    }
    let mut openrc_state: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in section("OPENRC").lines() {
        if let Some((name, state)) = line.split_once('|') {
            let s = state.to_ascii_lowercase();
            let running = if s.contains("started") {
                "active"
            } else if s.contains("stopped") {
                "inactive"
            } else {
                "unknown"
            };
            openrc_state.insert(name.trim().to_string(), running.to_string());
        }
    }
    let state = |name: &str| -> (bool, bool, bool) {
        let (active, enabled) = unit_state
            .get(name)
            .cloned()
            .unwrap_or_else(|| ("unknown".into(), "unknown".into()));
        let active = if active == "active" {
            true
        } else if active == "unknown" {
            openrc_state
                .get(name)
                .map(|s| s == "active")
                .unwrap_or(false)
        } else {
            false
        };
        let present = active
            || openrc_state.contains_key(name)
            || !matches!(enabled.as_str(), "unknown" | "not-found");
        let enabled = enabled == "enabled" || openrc_state.contains_key(name);
        (active, enabled, present)
    };

    let (chrony_active, chrony_enabled, chrony_unit) = state("chronyd");
    let (chrony2_active, chrony2_enabled, chrony2_unit) = state("chrony");
    let (ntp_active, ntp_enabled, ntp_unit) = state("ntp");
    let (ntpd_active, ntpd_enabled, ntpd_unit) = state("ntpd");
    let (ts_active, ts_enabled, ts_unit) = state("systemd-timesyncd");

    let chrony_installed = tool("chronyc") || tool("chronyd") || chrony_unit || chrony2_unit;
    let ntpd_installed = tool("ntpd") || tool("ntpq") || ntp_unit || ntpd_unit;
    let ts_installed = ts_unit;

    let chrony_active = chrony_active || chrony2_active;
    let chrony_enabled = chrony_enabled || chrony2_enabled;
    let ntpd_active = ntp_active || ntpd_active;
    let ntpd_enabled = ntp_enabled || ntpd_enabled;

    let service = if chrony_active {
        "chrony"
    } else if ntpd_active {
        "ntpd"
    } else if ts_active {
        "timesyncd"
    } else if chrony_installed {
        "chrony"
    } else if ntpd_installed {
        "ntpd"
    } else if ts_installed {
        "timesyncd"
    } else {
        "none"
    };

    let (active, enabled, installed) = match service {
        "chrony" => (chrony_active, chrony_enabled, chrony_installed),
        "ntpd" => (ntpd_active, ntpd_enabled, ntpd_installed),
        "timesyncd" => (ts_active, ts_enabled, ts_installed),
        _ => (false, false, false),
    };

    let package_manager = section("PKG")
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("none")
        .to_string();

    let mut synchronized = None;
    let mut offset_ms = None;
    if service == "chrony" {
        let track = section("CHRONY_TRACK");
        if !track.trim().is_empty() {
            synchronized = Some(
                field_value(&track, "Leap status")
                    .map(|v| v.eq_ignore_ascii_case("normal"))
                    .unwrap_or(false)
                    && field_value(&track, "Reference ID")
                        .map(|v| !v.starts_with("00000000"))
                        .unwrap_or(true),
            );
            offset_ms = parse_chrony_offset(&track);
        }
    } else if service == "ntpd" {
        let ntpq = section("NTPQ");
        if !ntpq.trim().is_empty() {
            synchronized = Some(ntpq.lines().any(|l| l.starts_with('*')));
            offset_ms = parse_ntpq_offset(&ntpq);
        }
    }
    if synchronized.is_none() {
        match section("TIMEDATECTL").trim() {
            "yes" => synchronized = Some(true),
            "no" => synchronized = Some(false),
            _ => {}
        }
    }

    let (servers, config_path) = match service {
        "chrony" => (parse_servers(&section("CHRONY_CONF")), first_conf_path(&section("CHRONY_CONF"))),
        "ntpd" => (parse_servers(&section("NTP_CONF")), first_conf_path(&section("NTP_CONF"))),
        "timesyncd" => (
            parse_servers(&section("TIMESYNCD_CONF")),
            first_conf_path(&section("TIMESYNCD_CONF")),
        ),
        _ => (Vec::new(), None),
    };

    let (managed, note) = if os == "Darwin" {
        (
            false,
            Some("macOS 请在「系统设置 → 通用 → 日期与时间」中管理网络时间；工具箱不修改系统配置".to_string()),
        )
    } else if service == "none" && package_manager == "none" {
        (false, Some("未识别远端包管理器，无法自动安装 chrony".to_string()))
    } else {
        (true, None)
    };

    Ok(NtpStatus {
        os,
        distro,
        service: service.to_string(),
        installed,
        active,
        enabled,
        synchronized,
        offset_ms,
        servers,
        config_path,
        package_manager,
        managed,
        note,
        now,
    })
}

fn parse_distro(osrel: &str, os: &str) -> String {
    let mut name = String::new();
    let mut id = String::new();
    for line in osrel.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
            name = v.trim_matches('"').to_string();
        } else if let Some(v) = line.strip_prefix("NAME=") {
            if name.is_empty() {
                name = v.trim_matches('"').to_string();
            }
        } else if let Some(v) = line.strip_prefix("ID=") {
            id = v.trim_matches('"').to_string();
        }
    }
    if name.is_empty() {
        name = if id.is_empty() { os.to_string() } else { id };
    }
    name
}

/// chrony tracking 的 `Last offset : +0.000123456 seconds` / `System time : ... fast`
fn parse_chrony_offset(track: &str) -> Option<f64> {
    if let Some(v) = field_value(track, "Last offset") {
        return parse_seconds_ms(v);
    }
    field_value(track, "System time").and_then(parse_seconds_ms)
}

fn parse_seconds_ms(value: &str) -> Option<f64> {
    let token = value.split_whitespace().next()?;
    let num: f64 = token.parse().ok()?;
    let ms = num * 1000.0;
    Some(if value.contains("slow") { -ms } else { ms })
}

/// `field : value` 形式的值
fn field_value<'a>(block: &'a str, key: &str) -> Option<&'a str> {
    block.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        if k.trim().eq_ignore_ascii_case(key) {
            Some(v.trim())
        } else {
            None
        }
    })
}

/// ntpq -pn 输出里带 `*` 的同步源 offset 列（毫秒）
fn parse_ntpq_offset(ntpq: &str) -> Option<f64> {
    for line in ntpq.lines() {
        if !line.starts_with('*') {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 9 {
            if let Ok(v) = cols[8].parse::<f64>() {
                return Some(v);
            }
        }
    }
    None
}

/// 从配置段取第一行 `FILE <路径>` 作为配置文件路径
fn first_conf_path(conf: &str) -> Option<String> {
    conf.lines()
        .find_map(|line| line.trim().strip_prefix("FILE ").map(str::to_string))
}

/// 配置段中的时间源：chrony/ntpd 的 server/pool/peer 与 timesyncd 的 NTP=
fn parse_servers(conf: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in conf.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with('!') {
            continue;
        }
        if let Some(rest) = t.strip_prefix("NTP=") {
            for host in rest.split_whitespace() {
                push_unique(&mut out, host);
            }
            continue;
        }
        let mut it = t.split_whitespace();
        match it.next() {
            Some("server" | "pool" | "peer") => {
                if let Some(host) = it.next() {
                    push_unique(&mut out, host);
                }
            }
            _ => {}
        }
    }
    out
}

fn push_unique(out: &mut Vec<String>, host: &str) {
    if !out.iter().any(|h| h.eq_ignore_ascii_case(host)) {
        out.push(host.to_string());
    }
}

/// 校验并去重时间源（最多 6 个）
pub(super) fn validate_servers(input: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for raw in input {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        if !is_valid_host(s) {
            return Err(format!("NTP 服务器地址不合法：{s}"));
        }
        push_unique(&mut out, s);
    }
    if out.is_empty() {
        return Err("至少填写一个 NTP 服务器".into());
    }
    if out.len() > 6 {
        return Err("NTP 服务器最多 6 个".into());
    }
    Ok(out)
}

fn is_valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if bare.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    bare.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// 安装 chrony 并启用服务
pub(super) fn install_script(package_manager: &str) -> Result<String, String> {
    let install = match package_manager {
        "apt-get" => "DEBIAN_FRONTEND=noninteractive apt-get update -qq || true\n\
                      DEBIAN_FRONTEND=noninteractive apt-get install -y chrony",
        "dnf" => "dnf install -y chrony",
        "yum" => "yum install -y chrony",
        "zypper" => "zypper --non-interactive install chrony",
        "pacman" => "pacman -Sy --noconfirm chrony",
        "apk" => "apk add chrony",
        other => return Err(format!("未识别的包管理器：{other}，请手动安装 chrony")),
    };
    Ok(format!(
        "set -e\n{install}\n\
         if command -v systemctl >/dev/null 2>&1; then\n\
           systemctl disable --now systemd-timesyncd 2>/dev/null || true\n\
           systemctl enable --now chronyd 2>/dev/null || systemctl enable --now chrony\n\
         elif command -v rc-update >/dev/null 2>&1; then\n\
           rc-update add chronyd default 2>/dev/null || true\n\
           rc-service chronyd restart 2>/dev/null || rc-service chronyd start\n\
         fi\n"
    ))
}

/// 替换时间源并重启服务
pub(super) fn configure_script(service: &str, servers: &[String]) -> Result<String, String> {
    let servers = validate_servers(servers)?;
    match service {
        "chrony" => {
            let mut lines = String::new();
            for s in &servers {
                lines.push_str(&sq(&format!("server {s} iburst")));
                lines.push(' ');
            }
            Ok(format!(
                "set -e\n\
                 CONF=/etc/chrony.conf\n\
                 [ -f \"$CONF\" ] || CONF=/etc/chrony/chrony.conf\n\
                 [ -f \"$CONF\" ] || {{ echo '未找到 chrony 配置文件' >&2; exit 1; }}\n\
                 cp -a \"$CONF\" \"$CONF.rhost.bak\"\n\
                 sed -i -E 's/^[[:space:]]*(server|pool|peer)[[:space:]]/# &/' \"$CONF\"\n\
                 printf '%s\\n' {lines}>> \"$CONF\"\n\
                 if command -v systemctl >/dev/null 2>&1; then\n\
                   systemctl restart chronyd 2>/dev/null || systemctl restart chrony\n\
                 else\n\
                   rc-service chronyd restart\n\
                 fi\n"
            ))
        }
        "ntpd" => {
            let mut lines = String::new();
            for s in &servers {
                lines.push_str(&sq(&format!("server {s} iburst")));
                lines.push(' ');
            }
            Ok(format!(
                "set -e\n\
                 CONF=/etc/ntp.conf\n\
                 [ -f \"$CONF\" ] || CONF=/etc/ntp/ntp.conf\n\
                 [ -f \"$CONF\" ] || {{ echo '未找到 ntpd 配置文件' >&2; exit 1; }}\n\
                 cp -a \"$CONF\" \"$CONF.rhost.bak\"\n\
                 sed -i -E 's/^[[:space:]]*(server|pool|peer)[[:space:]]/# &/' \"$CONF\"\n\
                 printf '%s\\n' {lines}>> \"$CONF\"\n\
                 if command -v systemctl >/dev/null 2>&1; then\n\
                   systemctl restart ntp 2>/dev/null || systemctl restart ntpd\n\
                 else\n\
                   rc-service ntp restart\n\
                 fi\n"
            ))
        }
        "timesyncd" => {
            let list = servers.join(" ");
            Ok(format!(
                "set -e\n\
                 mkdir -p /etc/systemd/timesyncd.conf.d\n\
                 printf '%s\\n' '[Time]' {ntp} > /etc/systemd/timesyncd.conf.d/rhost.conf\n\
                 systemctl restart systemd-timesyncd\n",
                ntp = sq(&format!("NTP={list}"))
            ))
        }
        other => Err(format!(
            "当前主机使用 {other}，请先安装 chrony 后再配置时间源"
        )),
    }
}

/// 立即校时：chrony 强制步进，其他服务重启触发一次同步
pub(super) fn sync_script(service: &str) -> Result<String, String> {
    let script = match service {
        "chrony" => {
            "if command -v chronyc >/dev/null 2>&1 && chronyc makestep 1 -1 >/dev/null 2>&1; then\n\
             \x20 echo '已立即校准系统时间'\n\
             else\n\
             \x20 if command -v systemctl >/dev/null 2>&1; then systemctl restart chronyd 2>/dev/null || systemctl restart chrony; else rc-service chronyd restart; fi\n\
             \x20 echo '已重启时间服务，等待下一次同步'\n\
             fi\n"
        }
        "ntpd" => {
            "if command -v systemctl >/dev/null 2>&1; then systemctl restart ntp 2>/dev/null || systemctl restart ntpd; else rc-service ntp restart; fi\n\
             echo '已重启时间服务，等待下一次同步'\n"
        }
        "timesyncd" => {
            "systemctl restart systemd-timesyncd\n\
             echo '已重启时间服务，等待下一次同步'\n"
        }
        other => {
            return Err(format!(
                "当前主机未运行受支持的时间服务（{other}），请先安装 chrony"
            ))
        }
    };
    Ok(script.to_string())
}

/// 启停 / 开机自启
pub(super) fn service_script(service: &str, action: &str) -> Result<String, String> {
    let units: &[&str] = match service {
        "chrony" => &["chronyd", "chrony"],
        "ntpd" => &["ntp", "ntpd"],
        "timesyncd" => &["systemd-timesyncd"],
        other => {
            return Err(format!(
                "当前主机未安装受支持的时间服务（{other}），请先安装 chrony"
            ))
        }
    };
    let (systemd_verb, openrc_verb) = match action {
        "enable" => ("enable --now", "add"),
        "disable" => ("disable", "del"),
        "start" => ("start", "start"),
        "stop" => ("stop", "stop"),
        other => return Err(format!("不支持的服务操作：{other}")),
    };
    let mut systemd = String::new();
    for (i, unit) in units.iter().enumerate() {
        if i > 0 {
            systemd.push_str(" || ");
        }
        systemd.push_str(&format!("systemctl {systemd_verb} {unit}"));
    }
    let rc = match action {
        "enable" | "disable" => {
            format!("rc-update {openrc_verb} {} default", units[0])
        }
        _ => format!("rc-service {} {openrc_verb}", units[0]),
    };
    Ok(format!(
        "set -e\n\
         if command -v systemctl >/dev/null 2>&1; then\n\
         \x20 {systemd}\n\
         else\n\
         \x20 {rc}\n\
         fi\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CHRONY: &str = "\
@@OS@@
Linux
@@OSREL@@
NAME=\"CentOS Linux\"
VERSION=\"7 (Core)\"
ID=\"centos\"
PRETTY_NAME=\"CentOS Linux 7 (Core)\"
@@NOW@@
1762900000|+0800|CST|2026-11-12 12:26:40
@@TOOLS@@
chronyc=1
chronyd=1
ntpd=0
ntpq=0
timedatectl=1
systemctl=1
@@SERVICES@@
chronyd|active|enabled
chrony|not-found|not-found
ntp|inactive|disabled
ntpd|not-found|not-found
systemd-timesyncd|not-found|not-found
@@OPENRC@@
@@TIMEDATECTL@@
yes
@@CHRONY_TRACK@@
Reference ID    : 0A0A0A0A (10.10.10.10)
Stratum         : 2
System time     : 0.000123456 seconds fast of NTP time
Last offset     : +0.000234567 seconds
Leap status     : Normal
@@NTPQ@@
@@CHRONY_CONF@@
FILE /etc/chrony.conf
# Use public servers from the pool.ntp.org project.
server 0.centos.pool.ntp.org iburst
server 1.centos.pool.ntp.org iburst
@@NTP_CONF@@
@@TIMESYNCD_CONF@@
@@PKG@@
yum
@@END@@
";

    #[test]
    fn parses_chrony_status() {
        let st = parse_status(SAMPLE_CHRONY).unwrap();
        assert_eq!(st.os, "Linux");
        assert_eq!(st.distro, "CentOS Linux 7 (Core)");
        assert_eq!(st.service, "chrony");
        assert!(st.installed && st.active && st.enabled);
        assert_eq!(st.synchronized, Some(true));
        assert!((st.offset_ms.unwrap() - 0.234567).abs() < 1e-6);
        assert_eq!(
            st.servers,
            vec![
                "0.centos.pool.ntp.org".to_string(),
                "1.centos.pool.ntp.org".to_string()
            ]
        );
        assert_eq!(st.config_path.as_deref(), Some("/etc/chrony.conf"));
        assert_eq!(st.package_manager, "yum");
        assert!(st.managed);
        assert_eq!(st.now.epoch_ms, 1_762_900_000_000);
        assert_eq!(st.now.tz_offset_minutes, 480);
        assert_eq!(st.now.tz_name, "CST");
    }

    #[test]
    fn parses_ntpd_and_timesyncd_states() {
        let ntpd = SAMPLE_CHRONY
            .replace("chronyd|active|enabled", "chronyd|not-found|not-found")
            .replace("ntp|inactive|disabled", "ntp|active|enabled")
            .replace("chronyc=1", "chronyc=0")
            .replace("chronyd=1", "chronyd=0")
            .replace("ntpd=0", "ntpd=1")
            .replace("ntpq=0", "ntpq=1");
        let st = parse_status(&ntpd).unwrap();
        assert_eq!(st.service, "ntpd");
        assert!(st.installed && st.active);

        let ts = SAMPLE_CHRONY
            .replace("chronyd|active|enabled", "chronyd|not-found|not-found")
            .replace("systemd-timesyncd|not-found|not-found", "systemd-timesyncd|active|enabled")
            .replace("chronyc=1", "chronyc=0")
            .replace("chronyd=1", "chronyd=0")
            .replace("@@TIMEDATECTL@@\nyes", "@@TIMEDATECTL@@\nyes");
        let st = parse_status(&ts).unwrap();
        assert_eq!(st.service, "timesyncd");
        assert_eq!(st.synchronized, Some(true));
    }

    #[test]
    fn parses_empty_service() {
        let none = SAMPLE_CHRONY
            .replace("chronyd|active|enabled", "chronyd|not-found|not-found")
            .replace("ntp|inactive|disabled", "ntp|not-found|not-found")
            .replace("chronyc=1", "chronyc=0")
            .replace("chronyd=1", "chronyd=0")
            .replace("@@PKG@@\nyum", "@@PKG@@\napk");
        let st = parse_status(&none).unwrap();
        assert_eq!(st.service, "none");
        assert!(!st.installed);
        assert_eq!(st.package_manager, "apk");
        assert!(st.managed);
    }

    #[test]
    fn macos_is_not_managed() {
        let mac = SAMPLE_CHRONY
            .replace("Linux\n@@OSREL@@", "Darwin\n@@OSREL@@")
            .replace("chronyd|active|enabled", "chronyd|not-found|not-found")
            .replace("chronyc=1", "chronyc=0")
            .replace("chronyd=1", "chronyd=0")
            .replace("@@PKG@@\nyum", "@@PKG@@\n");
        let st = parse_status(&mac).unwrap();
        assert_eq!(st.os, "Darwin");
        assert!(!st.managed);
        assert!(st.note.unwrap().contains("macOS"));
    }

    #[test]
    fn parses_server_time() {
        let t = parse_server_time("1762900000|+0800|CST|2026-11-12 12:26:40\n").unwrap();
        assert_eq!(t.epoch_ms, 1_762_900_000_000);
        assert_eq!(t.tz_offset_minutes, 480);
        assert_eq!(t.formatted, "2026-11-12 12:26:40");
        assert_eq!(parse_server_time("1762900000|-0530|UTC|2026-11-12 00:00:00").unwrap().tz_offset_minutes, -330);
        assert!(parse_server_time("").is_err());
        // 登录 shell 的 profile 噪声在 date 输出之前（真实主机上出现过）
        let noisy = "/etc/profile.d/lang.sh: line 45: /sbin/consoletype: No such file or directory\n\
                     1762900000|+0800|CST|2026-11-12 12:26:40\n";
        assert_eq!(parse_server_time(noisy).unwrap().formatted, "2026-11-12 12:26:40");
        assert!(parse_server_time("random | noise\nmore text").is_err());
    }

    #[test]
    fn validates_servers() {
        assert_eq!(
            validate_servers(&["pool.ntp.org".into(), "10.0.0.1".into()]).unwrap(),
            vec!["pool.ntp.org".to_string(), "10.0.0.1".to_string()]
        );
        assert!(validate_servers(&["[2001:db8::1]".into()]).is_ok());
        assert!(validate_servers(&["ntp1".into()]).is_ok());
        assert!(validate_servers(&["bad server".into()]).is_err());
        assert!(validate_servers(&["a;rm -rf /".into()]).is_err());
        assert!(validate_servers(&["-bad.example.com".into()]).is_err());
        assert!(validate_servers(&[]).is_err());
        let many: Vec<String> = (0..7).map(|i| format!("ntp{i}.example.com")).collect();
        assert!(validate_servers(&many).is_err());
    }

    #[test]
    fn builds_install_and_configure_scripts() {
        let s = install_script("apt-get").unwrap();
        assert!(s.contains("apt-get install -y chrony"));
        assert!(s.contains("systemctl enable --now chronyd"));
        assert!(install_script("unknown").is_err());

        let s = configure_script("chrony", &["pool.ntp.org".into(), "10.0.0.1".into()]).unwrap();
        assert!(s.contains("cp -a \"$CONF\" \"$CONF.rhost.bak\""));
        assert!(s.contains("server pool.ntp.org iburst"));
        assert!(s.contains("server 10.0.0.1 iburst"));
        assert!(s.contains("systemctl restart chronyd"));
        assert!(configure_script("none", &["pool.ntp.org".into()]).is_err());

        let s = configure_script("timesyncd", &["ntp.aliyun.com".into()]).unwrap();
        assert!(s.contains("NTP=ntp.aliyun.com"));
        assert!(s.contains("timesyncd.conf.d/rhost.conf"));
    }

    #[test]
    fn builds_service_and_sync_scripts() {
        assert!(service_script("chrony", "enable").unwrap().contains("systemctl enable --now chronyd"));
        assert!(service_script("chrony", "disable").unwrap().contains("systemctl disable chronyd"));
        assert!(!service_script("chrony", "disable").unwrap().contains("disable --now"));
        assert!(service_script("chrony", "bogus").is_err());
        assert!(service_script("none", "enable").is_err());
        assert!(sync_script("chrony").unwrap().contains("chronyc makestep 1 -1"));
        assert!(sync_script("none").is_err());
    }

    #[test]
    fn parses_servers_from_sections() {
        assert_eq!(
            parse_servers("server a.example.com iburst\npool pool.ntp.org iburst\n# server ignored\n"),
            vec!["a.example.com".to_string(), "pool.ntp.org".to_string()]
        );
        assert_eq!(
            parse_servers("[Time]\nNTP=ntp1.example.com ntp2.example.com\nFallbackNTP=x\n"),
            vec!["ntp1.example.com".to_string(), "ntp2.example.com".to_string()]
        );
    }
}
