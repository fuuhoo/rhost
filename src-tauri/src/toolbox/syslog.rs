//! 系统日志查看：按发行版探测可用日志来源，读取末尾 N 行。
//!
//! 不同系统的日志路径差异很大（Debian/Ubuntu 用 `/var/log/syslog`、`auth.log`，
//! RHEL/CentOS/Fedora 用 `/var/log/messages`、`secure`，现代 systemd 发行版统一走
//! journald，macOS 走统一日志 `log`）。这里先探测远端实际存在哪些来源，优先
//! journald（与发行版无关），回退到检测到的文本日志文件，macOS 另给 `log show`。
//!
//! 读取走独立 exec 通道（不经 PTY、不污染终端），远端只跑只读的 `tail` /
//! `journalctl` / `log show`。受保护日志普通用户读不了时返回
//! [`SYSLOG_PERMISSION_DENIED`]，由调用方按需提权重试。来源 id 与文件路径均经
//! 后端校验后重建命令，前端文本不直接拼进 shell。

use serde::Serialize;

use super::sq;
use crate::ssh::session::ExecOutput;

/// 普通用户读不了受保护日志：前端据此弹 sudo 后重试
pub(super) const SYSLOG_PERMISSION_DENIED: &str = "SYSLOG_PERMISSION_DENIED";

const READ_FAILED: &str = "SYSLOG_READ_FAILED";
const READ_MARKER: &str = "@@SYSLOG@@";

/// 日志回传上限：超出只保留末尾，前端明确提示已截断
const MAX_LOG_CHARS: usize = 512 * 1024;
/// 远端报错正文截断上限（错误横幅只展示摘要）
const MAX_DETAIL: usize = 400;

/// 探测的常见文本日志文件（不含 wtmp/btmp/lastlog 等二进制登录记录）
const LOG_FILES: &[&str] = &[
    "/var/log/syslog",
    "/var/log/messages",
    "/var/log/auth.log",
    "/var/log/secure",
    "/var/log/kern.log",
    "/var/log/dmesg",
    "/var/log/cron",
    "/var/log/daemon.log",
    "/var/log/user.log",
    "/var/log/maillog",
    "/var/log/mail.log",
    "/var/log/boot.log",
    "/var/log/system.log",
];

/// 来源探测脚本：以 marker 分段，任意一段缺失不影响其他段。
/// 文件段对每个候选路径输出 `R:`（存在且当前用户可读）或 `U:`（存在但不可读）。
pub(super) fn sources_script() -> String {
    let files = LOG_FILES.join(" ");
    format!(
        r#"echo '@@OS@@'
uname -s 2>/dev/null
echo '@@OSREL@@'
cat /etc/os-release 2>/dev/null
echo '@@JOURNAL@@'
if command -v journalctl >/dev/null 2>&1; then echo 1; else echo 0; fi
echo '@@MACLOG@@'
if command -v log >/dev/null 2>&1; then echo 1; else echo 0; fi
echo '@@FILES@@'
for f in {files}; do
  if [ -f "$f" ]; then
    if [ -r "$f" ]; then echo "R:$f"; else echo "U:$f"; fi
  fi
done
echo '@@END@@'
"#
    )
}

/// 一个可选的日志来源
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogSource {
    /// 稳定 id：journal / journal-kernel / macos / file:<绝对路径>
    pub id: String,
    pub label: String,
    /// journal / file / macos
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 人读说明（发行版归属 / 内容范围）
    pub detail: String,
    /// 当前用户是否可直接读取（file 来源；false 时读取会按需提权）
    pub readable: bool,
}

/// 来源探测结果
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyslogInfo {
    /// 系统名：Linux / Darwin / ...
    pub os: String,
    /// 发行版友好名（PRETTY_NAME），缺失回退 NAME / ID / os
    pub distro: String,
    pub sources: Vec<LogSource>,
}

/// 解析探测输出，按「journald 优先 → 文本文件 → macOS 统一日志」组装来源。
pub(super) fn parse_sources(output: &str) -> Result<SyslogInfo, String> {
    let mut os = String::new();
    let mut osrel = String::new();
    let mut journal = false;
    let mut maclog = false;
    let mut files: Vec<(String, bool)> = Vec::new();
    let mut section = "";

    for line in output.lines() {
        match line.trim_end() {
            "@@OS@@" => {
                section = "os";
                continue;
            }
            "@@OSREL@@" => {
                section = "osrel";
                continue;
            }
            "@@JOURNAL@@" => {
                section = "journal";
                continue;
            }
            "@@MACLOG@@" => {
                section = "maclog";
                continue;
            }
            "@@FILES@@" => {
                section = "files";
                continue;
            }
            "@@END@@" => {
                section = "end";
                continue;
            }
            _ => {}
        }
        match section {
            "os" => {
                if os.is_empty() {
                    os = line.trim().to_string();
                }
            }
            "osrel" => {
                osrel.push_str(line);
                osrel.push('\n');
            }
            "journal" => journal = line.trim() == "1",
            "maclog" => maclog = line.trim() == "1",
            "files" => {
                if let Some(p) = line.strip_prefix("R:") {
                    files.push((p.trim().to_string(), true));
                } else if let Some(p) = line.strip_prefix("U:") {
                    files.push((p.trim().to_string(), false));
                }
            }
            _ => {}
        }
    }

    if os.is_empty() {
        os = "Unknown".into();
    }
    let distro = osrel_value(&osrel, "PRETTY_NAME")
        .or_else(|| osrel_value(&osrel, "NAME"))
        .or_else(|| osrel_value(&osrel, "ID"))
        .unwrap_or_else(|| os.clone());

    let mut sources = Vec::new();
    if journal {
        sources.push(LogSource {
            id: "journal".into(),
            label: "系统日志（journald）".into(),
            kind: "journal".into(),
            path: None,
            detail: "systemd journal，汇总所有服务与内核".into(),
            readable: true,
        });
        sources.push(LogSource {
            id: "journal-kernel".into(),
            label: "内核日志（journald）".into(),
            kind: "journal".into(),
            path: None,
            detail: "journalctl -k，仅内核消息".into(),
            readable: true,
        });
    }
    for (path, readable) in &files {
        sources.push(LogSource {
            id: format!("file:{path}"),
            label: path.clone(),
            kind: "file".into(),
            path: Some(path.clone()),
            detail: file_detail(path).into(),
            readable: *readable,
        });
    }
    if os == "Darwin" && maclog {
        sources.push(LogSource {
            id: "macos".into(),
            label: "统一日志（log show）".into(),
            kind: "macos".into(),
            path: None,
            detail: "macOS 统一日志，最近 1 小时".into(),
            readable: true,
        });
    }

    Ok(SyslogInfo {
        os,
        distro,
        sources,
    })
}

/// 从 os-release 文本取某个键的值（兼容 `KEY="value"` 与 `KEY=value`）
fn osrel_value(text: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix(&prefix) else {
            continue;
        };
        let v = rest.trim().trim_matches('"').trim_matches('\'').to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    None
}

/// 按文件名给出人读说明
fn file_detail(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name {
        "syslog" => "综合系统日志（Debian/Ubuntu）",
        "messages" => "综合系统日志（RHEL/CentOS/Fedora）",
        "auth.log" => "认证与授权日志（Debian/Ubuntu）",
        "secure" => "认证与授权日志（RHEL/CentOS/Fedora）",
        "kern.log" => "内核日志",
        "dmesg" => "内核环形缓冲",
        "cron" => "计划任务日志",
        "daemon.log" => "守护进程日志",
        "user.log" => "用户进程日志",
        "maillog" | "mail.log" => "邮件日志",
        "boot.log" => "启动日志",
        "system.log" => "系统日志（macOS）",
        _ => "日志文件",
    }
}

/// 日志文件路径白名单：仅限 `/var/log/` 下、绝对路径、无 `..` 与控制字符。
/// 防止把任意文件（如 /etc/shadow）当日志读出来。
pub(super) fn validate_log_path(input: &str) -> Result<String, String> {
    let path = input.trim();
    if path.is_empty() {
        return Err("日志路径为空".into());
    }
    if path.len() > 512 {
        return Err("日志路径过长".into());
    }
    if !path.starts_with("/var/log/") {
        return Err("仅允许读取 /var/log/ 下的日志".into());
    }
    if path.chars().any(|c| c.is_control()) {
        return Err("路径包含不允许的控制字符".into());
    }
    if path.split('/').any(|seg| seg == "..") {
        return Err("路径不能包含 ..".into());
    }
    if path.ends_with('/') {
        return Err("请填写日志文件，而不是目录".into());
    }
    Ok(path.to_string())
}

pub(super) fn validate_tail(tail: u32) -> Result<u32, String> {
    if (10..=5000).contains(&tail) {
        Ok(tail)
    } else {
        Err("日志行数需在 10~5000 之间".into())
    }
}

/// 校验并规范化时间参数（journald `--since`/`--until` 用）。
/// 接受 `YYYY-MM-DD`、`YYYY-MM-DD HH:MM[:SS]`、`YYYY-MM-DDTHH:MM[:SS]`，
/// 统一输出 `YYYY-MM-DD HH:MM:SS`。结果只含数字与 `-`、`:`、空格，重建命令时再经
/// [`sq`] 包裹，前端文本不直接拼进 shell。
pub(super) fn validate_time(input: &str) -> Result<String, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("时间为空".into());
    }
    if s.len() > 32 {
        return Err("时间格式过长".into());
    }
    let (date, time) = match s.split_once(|c| c == 'T' || c == ' ') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let dparts: Vec<&str> = date.split('-').collect();
    if dparts.len() != 3 {
        return Err("时间格式应为 YYYY-MM-DD[ HH:MM[:SS]]".into());
    }
    let year = parse_time_num(dparts[0], 4)?;
    let month = parse_time_num(dparts[1], 2)?;
    let day = parse_time_num(dparts[2], 2)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err("月份或日期超出范围".into());
    }
    let (mut hh, mut mm, mut ss) = (0u32, 0u32, 0u32);
    if let Some(t) = time {
        let tparts: Vec<&str> = t.split(':').collect();
        if tparts.len() < 2 || tparts.len() > 3 {
            return Err("时间格式应为 HH:MM[:SS]".into());
        }
        hh = parse_time_num(tparts[0], 2)?;
        mm = parse_time_num(tparts[1], 2)?;
        if tparts.len() == 3 {
            ss = parse_time_num(tparts[2], 2)?;
        }
        if hh > 23 || mm > 59 || ss > 59 {
            return Err("时分秒超出范围".into());
        }
    }
    Ok(format!("{year:04}-{month:02}-{day:02} {hh:02}:{mm:02}:{ss:02}"))
}

fn parse_time_num(s: &str, width: usize) -> Result<u32, String> {
    if s.len() != width || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err("时间格式应为 YYYY-MM-DD[ HH:MM[:SS]]".into());
    }
    s.parse::<u32>().map_err(|_| "时间格式错误".into())
}

/// 按来源 id 重建读取脚本：只跑只读命令，输出经 marker + 字节长度回传。
/// `since`/`until` 仅对 journald 源生效（服务端 `--since`/`--until`）；文件源与
/// macOS 由前端在已加载行内按时间戳过滤，这里忽略。
pub(super) fn build_read_script(
    source: &str,
    tail: u32,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<String, String> {
    let tail = validate_tail(tail)?;
    let since = since.map(validate_time).transpose()?;
    let until = until.map(validate_time).transpose()?;
    let producer = match source {
        "journal" | "journal-kernel" => {
            let kernel = if source == "journal-kernel" { " -k" } else { "" };
            let mut cmd = format!("journalctl --no-pager{kernel}");
            if let Some(s) = &since {
                cmd.push_str(&format!(" --since {}", sq(s)));
            }
            if let Some(u) = &until {
                cmd.push_str(&format!(" --until {}", sq(u)));
            }
            cmd.push_str(&format!(" -n {tail} 2>&1"));
            cmd
        }
        "macos" => format!("log show --last 1h --style compact 2>&1 | tail -n {tail}"),
        _ => {
            let path = source
                .strip_prefix("file:")
                .ok_or_else(|| "未知的日志来源".to_string())?;
            let path = validate_log_path(path)?;
            format!("tail -n {tail} -- {} 2>&1", sq(&path))
        }
    };
    Ok(format!(
        r#"out=$({producer})
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
        producer = producer,
        denied = SYSLOG_PERMISSION_DENIED,
        failed = READ_FAILED,
        marker = READ_MARKER,
    ))
}

/// 日志读取结果
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyslogReadResult {
    pub output: String,
    /// 超出回传上限，只保留了末尾
    pub truncated: bool,
}

pub(super) fn parse_read(out: &ExecOutput) -> Result<SyslogReadResult, String> {
    if out.timed_out {
        return Err("读取系统日志超时（可减小行数后重试）".into());
    }
    if out.output.contains(SYSLOG_PERMISSION_DENIED) {
        return Err(SYSLOG_PERMISSION_DENIED.into());
    }
    let Some((_, rest)) = out.output.split_once(READ_MARKER) else {
        if let Some((_, detail)) = out.output.split_once(READ_FAILED) {
            return Err(format!("读取系统日志失败：{}", one_line(detail)));
        }
        return Err(format!(
            "读取系统日志失败（退出码 {:?}）：{}",
            out.exit_code.unwrap_or(-1),
            one_line(&out.output)
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
    Ok(SyslogReadResult {
        output: text,
        truncated,
    })
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

    fn out(text: &str, code: i32) -> ExecOutput {
        ExecOutput {
            output: text.to_string(),
            exit_code: Some(code),
            timed_out: false,
        }
    }

    #[test]
    fn parses_journald_and_files() {
        let raw = "\
@@OS@@
Linux
@@OSREL@@
PRETTY_NAME=\"Ubuntu 22.04.3 LTS\"
ID=ubuntu
@@JOURNAL@@
1
@@MACLOG@@
0
@@FILES@@
R:/var/log/syslog
U:/var/log/auth.log
R:/var/log/kern.log
@@END@@
";
        let info = parse_sources(raw).unwrap();
        assert_eq!(info.os, "Linux");
        assert_eq!(info.distro, "Ubuntu 22.04.3 LTS");
        assert_eq!(info.sources.len(), 5);
        assert_eq!(info.sources[0].id, "journal");
        assert_eq!(info.sources[1].id, "journal-kernel");
        assert_eq!(info.sources[2].id, "file:/var/log/syslog");
        assert!(info.sources[2].readable);
        assert_eq!(info.sources[2].detail, "综合系统日志（Debian/Ubuntu）");
        assert!(!info.sources[3].readable);
        assert_eq!(info.sources[3].path.as_deref(), Some("/var/log/auth.log"));
    }

    #[test]
    fn parses_rhel_file_only() {
        let raw = "\
@@OS@@
Linux
@@OSREL@@
ID=\"centos\"
@@JOURNAL@@
0
@@MACLOG@@
0
@@FILES@@
R:/var/log/messages
U:/var/log/secure
@@END@@
";
        let info = parse_sources(raw).unwrap();
        assert_eq!(info.distro, "centos");
        assert_eq!(info.sources.len(), 2);
        assert_eq!(info.sources[0].id, "file:/var/log/messages");
        assert_eq!(info.sources[0].detail, "综合系统日志（RHEL/CentOS/Fedora）");
        assert!(!info.sources[1].readable);
    }

    #[test]
    fn parses_macos_unified_log() {
        let raw = "\
@@OS@@
Darwin
@@OSREL@@
@@JOURNAL@@
0
@@MACLOG@@
1
@@FILES@@
R:/var/log/system.log
@@END@@
";
        let info = parse_sources(raw).unwrap();
        assert_eq!(info.os, "Darwin");
        assert_eq!(info.sources.len(), 2);
        assert_eq!(info.sources[0].id, "file:/var/log/system.log");
        assert_eq!(info.sources[1].id, "macos");
        assert_eq!(info.sources[1].kind, "macos");
    }

    #[test]
    fn macos_source_requires_darwin() {
        // Linux 上即便存在名为 log 的命令也不当作 macOS 统一日志
        let raw = "\
@@OS@@
Linux
@@JOURNAL@@
0
@@MACLOG@@
1
@@FILES@@
@@END@@
";
        let info = parse_sources(raw).unwrap();
        assert!(info.sources.is_empty());
    }

    #[test]
    fn empty_sources_when_nothing_found() {
        let raw = "\
@@OS@@
Linux
@@OSREL@@
@@JOURNAL@@
0
@@MACLOG@@
0
@@FILES@@
@@END@@
";
        let info = parse_sources(raw).unwrap();
        assert!(info.sources.is_empty());
        assert_eq!(info.distro, "Linux");
    }

    #[test]
    fn osrel_value_handles_quotes_and_prefix_collisions() {
        let text = "NAME=\"CentOS Linux\"\nVERSION_ID=\"7\"\nID=centos\nID_LIKE=\"rhel fedora\"\n";
        assert_eq!(osrel_value(text, "ID").as_deref(), Some("centos"));
        assert_eq!(osrel_value(text, "NAME").as_deref(), Some("CentOS Linux"));
        assert_eq!(osrel_value(text, "PRETTY_NAME"), None);
    }

    #[test]
    fn validates_log_path_whitelist() {
        assert_eq!(
            validate_log_path("/var/log/syslog").unwrap(),
            "/var/log/syslog"
        );
        assert_eq!(
            validate_log_path("/var/log/nginx/error.log").unwrap(),
            "/var/log/nginx/error.log"
        );
        assert!(validate_log_path("/etc/shadow").is_err());
        assert!(validate_log_path("/var/log/../etc/shadow").is_err());
        assert!(validate_log_path("/var/log/").is_err());
        assert!(validate_log_path("relative.log").is_err());
        assert!(validate_log_path("/var/log/a\nb").is_err());
        assert!(validate_log_path("").is_err());
    }

    #[test]
    fn builds_read_scripts_per_kind() {
        let j = build_read_script("journal", 200, None, None).unwrap();
        assert!(j.contains("journalctl --no-pager -n 200"));
        let k = build_read_script("journal-kernel", 100, None, None).unwrap();
        assert!(k.contains("journalctl --no-pager -k -n 100"));
        let f = build_read_script("file:/var/log/secure", 500, None, None).unwrap();
        assert!(f.contains("tail -n 500 -- '/var/log/secure'"));
        let m = build_read_script("macos", 200, None, None).unwrap();
        assert!(m.contains("log show --last 1h"));
        assert!(build_read_script("file:/etc/shadow", 200, None, None).is_err());
        assert!(build_read_script("bogus", 200, None, None).is_err());
        assert!(build_read_script("journal", 5, None, None).is_err());
        assert!(build_read_script("journal", 99999, None, None).is_err());
    }

    #[test]
    fn read_script_quotes_path_with_space() {
        let f = build_read_script("file:/var/log/my log.log", 100, None, None).unwrap();
        assert!(f.contains("tail -n 100 -- '/var/log/my log.log'"));
    }

    #[test]
    fn validates_time_formats() {
        assert_eq!(validate_time("2026-11-11").unwrap(), "2026-11-11 00:00:00");
        assert_eq!(
            validate_time("2026-11-11 10:30").unwrap(),
            "2026-11-11 10:30:00"
        );
        assert_eq!(
            validate_time("2026-11-11T10:30:45").unwrap(),
            "2026-11-11 10:30:45"
        );
        assert!(validate_time("").is_err());
        assert!(validate_time("2026-13-01").is_err());
        assert!(validate_time("2026-11-32").is_err());
        assert!(validate_time("2026-11-11 25:00").is_err());
        assert!(validate_time("2026-11-11 10:61").is_err());
        assert!(validate_time("2026-1-1").is_err());
        assert!(validate_time("yesterday").is_err());
        // 注入尝试：引号 / 命令分隔 / 控制字符一律拒绝
        assert!(validate_time("2026-11-11'; rm -rf /; '").is_err());
        assert!(validate_time("2026-11-11 10:00; reboot").is_err());
        assert!(validate_time("2026-11-11\n10:00").is_err());
    }

    #[test]
    fn journal_read_script_applies_time_range() {
        let j = build_read_script(
            "journal",
            200,
            Some("2026-11-11T10:00"),
            Some("2026-11-11 11:30:00"),
        )
        .unwrap();
        assert!(j.contains("--since '2026-11-11 10:00:00'"));
        assert!(j.contains("--until '2026-11-11 11:30:00'"));
        assert!(j.contains("-n 200"));
        let k = build_read_script("journal-kernel", 100, Some("2026-11-11"), None).unwrap();
        assert!(k.contains("journalctl --no-pager -k --since '2026-11-11 00:00:00'"));
        assert!(!k.contains("--until"));
    }

    #[test]
    fn file_and_macos_ignore_time_range() {
        // 文件源 / macOS 不做服务端时间过滤（交给前端），脚本里不应出现 --since
        let f = build_read_script(
            "file:/var/log/syslog",
            200,
            Some("2026-11-11 10:00"),
            Some("2026-11-11 11:00"),
        )
        .unwrap();
        assert!(f.contains("tail -n 200 -- '/var/log/syslog'"));
        assert!(!f.contains("--since"));
        let m = build_read_script("macos", 200, Some("2026-11-11 10:00"), None).unwrap();
        assert!(!m.contains("--since"));
        // 即便来源忽略时间，非法时间仍提前拒绝
        assert!(build_read_script("file:/var/log/syslog", 200, Some("bad"), None).is_err());
    }

    #[test]
    fn parses_read_output() {
        let parsed = parse_read(&out("@@SYSLOG@@12\nhello\nworld\n", 0)).unwrap();
        assert_eq!(parsed.output, "hello\nworld\n");
        assert!(!parsed.truncated);
    }

    #[test]
    fn read_permission_denied_is_error() {
        assert!(parse_read(&out(SYSLOG_PERMISSION_DENIED, 5)).is_err());
    }

    #[test]
    fn read_failure_reports_detail() {
        let e = parse_read(&out("SYSLOG_READ_FAILED\ntail: cannot open\n", 5)).unwrap_err();
        assert!(e.contains("读取系统日志失败"));
        assert!(e.contains("tail: cannot open"));
    }

    #[test]
    fn read_timeout_is_error() {
        let mut o = out("@@SYSLOG@@3\nabc", 0);
        o.timed_out = true;
        assert!(parse_read(&o).is_err());
    }

    #[test]
    fn read_truncates_to_tail() {
        let long = "y".repeat(MAX_LOG_CHARS + 10);
        let parsed = parse_read(&out(
            &format!("@@SYSLOG@@{}\n{long}", long.len()),
            0,
        ))
        .unwrap();
        assert!(parsed.truncated);
        assert_eq!(parsed.output.chars().count(), MAX_LOG_CHARS);
    }
}