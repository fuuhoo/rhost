//! 软件源（镜像站）管理：识别远端 apt / yum(dnf) 软件源，按预设镜像站替换主机名，
//! 修改前自动备份到 `/etc/rhost/mirror-backup/<时间戳>/`，支持一键还原。
//!
//! 与 timesync 一样走独立 exec 通道。只读探测用普通用户执行；写入与还原由前端
//! 显式触发并按需提权。文件内容由后端解析后重写、以 base64 落到远端，前端不能
//! 传入任意文本；改写只动仓库地址（apt 的 deb/URIs、yum 的 baseurl 与特定
//! mirrorlist/metalink），gpgkey 等其余字段保持原样。

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Serialize;

use super::sq;

/// 状态探测脚本：marker 分段，任意一段缺失不影响其他段。
pub(super) const PROBE_SCRIPT: &str = r##"echo '@@OS@@'
uname -s 2>/dev/null
echo '@@ARCH@@'
uname -m 2>/dev/null
echo '@@OSREL@@'
cat /etc/os-release 2>/dev/null
echo '@@PKG@@'
for c in apt-get dnf yum zypper apk; do
  command -v "$c" >/dev/null 2>&1 && { echo "$c"; break; }
done
echo '@@FILES@@'
for f in /etc/apt/sources.list /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources /etc/yum.repos.d/*.repo; do
  [ -f "$f" ] || continue
  echo "FILE $f"
  cat "$f" 2>/dev/null
  echo
done
echo '@@BACKUPS@@'
for d in /etc/rhost/mirror-backup/*/; do
  [ -d "$d" ] || continue
  echo "BACKUP $(basename "$d")"
  sed -n '1,4p' "$d/meta" 2>/dev/null
  echo "FILES $(wc -l < "$d/manifest" 2>/dev/null | tr -d ' ')"
done
echo '@@END@@'
"##;

/// 预设镜像站（host 用于替换仓库地址里的主机名）
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorPreset {
    pub id: String,
    pub label: String,
    pub host: String,
}

pub(super) fn presets() -> Vec<MirrorPreset> {
    [
        ("aliyun", "阿里云", "mirrors.aliyun.com"),
        ("tencent", "腾讯云", "mirrors.cloud.tencent.com"),
        ("huawei", "华为云", "mirrors.huaweicloud.com"),
        ("tuna", "清华大学", "mirrors.tuna.tsinghua.edu.cn"),
        ("ustc", "中科大", "mirrors.ustc.edu.cn"),
        ("netease", "网易", "mirrors.163.com"),
    ]
    .into_iter()
    .map(|(id, label, host)| MirrorPreset {
        id: id.to_string(),
        label: label.to_string(),
        host: host.to_string(),
    })
    .collect()
}

pub(super) fn preset_by_id(id: &str) -> Option<MirrorPreset> {
    presets().into_iter().find(|p| p.id == id)
}

/// 一个软件源文件（内容仅供后端重写，不返回前端）
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RepoFile {
    pub path: String,
    /// apt / yum
    pub kind: &'static str,
    pub content: String,
}

/// 前端可见的软件源文件摘要
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorFile {
    pub path: String,
    pub kind: String,
    pub urls: Vec<String>,
    /// 含 mirrorlist/metalink 且未被改写的仓库（前端提示）
    pub mirrorlist: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorBackup {
    pub id: String,
    pub created: String,
    pub preset: String,
    pub files: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorStatus {
    pub os: String,
    pub arch: String,
    /// PRETTY_NAME（读不到时退回 uname）
    pub distro: String,
    /// /etc/os-release 的 ID（小写）
    pub distro_id: String,
    /// VERSION_ID 主版本（如 CentOS 7 → 7）
    pub distro_major: String,
    /// apt / dnf / yum / zypper / apk / none
    pub package_manager: String,
    pub managed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// 当前命中的预设镜像站 id
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// 软件源里出现的 HTTP(S) 主机名
    pub hosts: Vec<String>,
    pub files: Vec<MirrorFile>,
    pub backups: Vec<MirrorBackup>,
    pub presets: Vec<MirrorPreset>,
}

fn split_sections(output: &str) -> std::collections::HashMap<String, Vec<String>> {
    let mut sections: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    let mut current: Option<String> = None;
    for line in output.lines() {
        let t = line.trim_end_matches('\r');
        if let Some(name) = t.strip_prefix("@@").and_then(|x| x.strip_suffix("@@")) {
            if name == "END" {
                current = None;
            } else {
                sections.entry(name.to_string()).or_default();
                current = Some(name.to_string());
            }
            continue;
        }
        if let Some(key) = &current {
            sections.entry(key.clone()).or_default().push(t.to_string());
        }
    }
    sections
}

fn section(sections: &std::collections::HashMap<String, Vec<String>>, name: &str) -> String {
    sections
        .get(name)
        .map(|lines| lines.join("\n"))
        .unwrap_or_default()
}

fn os_release_map(osrel: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for line in osrel.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let key = k.trim();
        if key.is_empty() || key.starts_with('#') {
            continue;
        }
        let value = v.trim().trim_matches('"').trim_matches('\'').to_string();
        map.insert(key.to_string(), value);
    }
    map
}

/// 解析探测输出，返回前端状态与原始文件内容。
pub(super) fn parse_probe(output: &str) -> Result<(MirrorStatus, Vec<RepoFile>), String> {
    let sections = split_sections(output);
    let os = section(&sections, "OS").trim().to_string();
    let arch = section(&sections, "ARCH").trim().to_string();
    let osrel = section(&sections, "OSREL");
    let rel = os_release_map(&osrel);
    let distro = rel
        .get("PRETTY_NAME")
        .cloned()
        .or_else(|| match (rel.get("NAME"), rel.get("VERSION")) {
            (Some(n), Some(v)) => Some(format!("{n} {v}")),
            (Some(n), None) => Some(n.clone()),
            _ => None,
        })
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if os.is_empty() {
                "未知系统".to_string()
            } else {
                os.clone()
            }
        });
    let distro_id = rel.get("ID").map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    let distro_major = rel
        .get("VERSION_ID")
        .map(|v| v.split('.').next().unwrap_or("").to_string())
        .unwrap_or_default();

    let package_manager = section(&sections, "PKG")
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("none")
        .to_string();

    let files = parse_repo_files(&section(&sections, "FILES"));
    let mirrored = files.iter().map(describe_file).collect::<Vec<_>>();
    let mut hosts: Vec<String> = Vec::new();
    for f in &mirrored {
        for url in &f.urls {
            if let Some(h) = url_host(url) {
                let h = h.to_ascii_lowercase();
                if !hosts.contains(&h) {
                    hosts.push(h);
                }
            }
        }
    }
    hosts.sort();
    let current = if hosts.is_empty() {
        None
    } else {
        presets()
            .into_iter()
            .find(|p| hosts.iter().all(|h| h == &p.host.to_ascii_lowercase()))
            .map(|p| p.id)
    };

    let backups = parse_backups(&section(&sections, "BACKUPS"));

    let (managed, note) = if os == "Darwin" {
        (
            false,
            Some("macOS 不使用 apt/yum 软件源；请在系统内管理软件更新".to_string()),
        )
    } else if !matches!(package_manager.as_str(), "apt-get" | "dnf" | "yum") {
        (
            false,
            Some(format!(
                "当前包管理器为 {package_manager}，暂只支持 apt / yum / dnf 自动换源"
            )),
        )
    } else if files.is_empty() {
        (
            false,
            Some("未在标准路径找到软件源文件（/etc/apt 或 /etc/yum.repos.d）".to_string()),
        )
    } else if mirrored.iter().all(|f| f.urls.is_empty()) {
        (
            false,
            Some("软件源未使用 HTTP(S) 地址（可能是本地光盘源），无法换源".to_string()),
        )
    } else {
        (true, None)
    };

    let pm = match package_manager.as_str() {
        "apt-get" => "apt",
        other => other,
    };

    Ok((
        MirrorStatus {
            os,
            arch,
            distro,
            distro_id,
            distro_major,
            package_manager: pm.to_string(),
            managed,
            note,
            current,
            hosts,
            files: mirrored,
            backups,
            presets: presets(),
        },
        files,
    ))
}

fn parse_repo_files(section: &str) -> Vec<RepoFile> {
    let mut out: Vec<RepoFile> = Vec::new();
    let mut cur: Option<(String, String)> = None;
    for raw in section.lines() {
        if let Some(path) = raw.strip_prefix("FILE ") {
            if let Some((p, c)) = cur.take() {
                out.push(build_repo_file(p, c));
            }
            let path = path.trim().to_string();
            let allowed = (path.starts_with("/etc/apt/") || path.starts_with("/etc/yum.repos.d/"))
                && !path.contains("..");
            cur = if allowed {
                Some((path, String::new()))
            } else {
                None
            };
            continue;
        }
        if let Some((_, c)) = cur.as_mut() {
            c.push_str(raw);
            c.push('\n');
        }
    }
    if let Some((p, c)) = cur.take() {
        out.push(build_repo_file(p, c));
    }
    out.retain(|f| !f.content.trim().is_empty());
    out
}

fn build_repo_file(path: String, content: String) -> RepoFile {
    let kind = if path.starts_with("/etc/apt/") {
        "apt"
    } else {
        "yum"
    };
    RepoFile { path, kind, content }
}

fn describe_file(f: &RepoFile) -> MirrorFile {
    let mut urls: Vec<String> = Vec::new();
    let mut mirrorlist = false;
    for line in f.content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
            continue;
        }
        if f.kind == "apt" {
            if t.starts_with("URIs:") {
                for tok in t["URIs:".len()..].split_whitespace() {
                    if is_http_url(tok) && !urls.iter().any(|u| u == tok) {
                        urls.push(tok.to_string());
                    }
                }
                continue;
            }
            let mut it = t.split_whitespace();
            if !matches!(it.next(), Some("deb") | Some("deb-src")) {
                continue;
            }
            if let Some(u) = it.find(|tok| is_http_url(tok)) {
                if !urls.iter().any(|x| x == u) {
                    urls.push(u.to_string());
                }
            }
        } else if let Some((k, v)) = t.split_once('=') {
            match k.trim().to_ascii_lowercase().as_str() {
                "baseurl" => {
                    for u in v.split([',', ' ', '\t']).filter(|x| is_http_url(x)) {
                        if !urls.iter().any(|x| x == u) {
                            urls.push(u.to_string());
                        }
                    }
                }
                "mirrorlist" | "metalink" => {
                    mirrorlist = true;
                    for u in v.split([',', ' ', '\t']).filter(|x| is_http_url(x)) {
                        if !urls.iter().any(|x| x == u) {
                            urls.push(u.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    urls.retain(|u| url_host(u).is_some());
    MirrorFile {
        path: f.path.clone(),
        kind: f.kind.to_string(),
        urls,
        mirrorlist,
    }
}

fn parse_backups(section: &str) -> Vec<MirrorBackup> {
    let mut out: Vec<MirrorBackup> = Vec::new();
    for line in section.lines() {
        let t = line.trim();
        if let Some(id) = t.strip_prefix("BACKUP ") {
            out.push(MirrorBackup {
                id: id.trim().to_string(),
                created: String::new(),
                preset: String::new(),
                files: 0,
            });
            continue;
        }
        let Some(cur) = out.last_mut() else { continue };
        if let Some(v) = t.strip_prefix("created=") {
            cur.created = v.trim().to_string();
        } else if let Some(v) = t.strip_prefix("preset=") {
            cur.preset = v.trim().to_string();
        } else if let Some(v) = t.strip_prefix("FILES ") {
            cur.files = v.trim().parse().unwrap_or(0);
        }
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    out
}

fn is_http_url(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..end];
    if host.is_empty() || host.contains('@') {
        None
    } else {
        Some(host)
    }
}

/// 把 URL 的主机名换成镜像站主机，保留 scheme、路径与查询串。
fn replace_host(url: &str, new_host: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let lower = scheme.to_ascii_lowercase();
    if lower != "http" && lower != "https" {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let tail = &rest[end..];
    if tail.is_empty() {
        Some(format!("{scheme}://{new_host}/"))
    } else {
        Some(format!("{scheme}://{new_host}{tail}"))
    }
}

fn split_nl(line: &str) -> (&str, &str) {
    if let Some(s) = line.strip_suffix("\r\n") {
        (s, "\r\n")
    } else if let Some(s) = line.strip_suffix('\n') {
        (s, "\n")
    } else {
        (line, "")
    }
}

/// 重写一个软件源文件；返回 (新内容, 改动行数, 跳过的 mirrorlist/metalink 行数)
pub(super) fn rewrite_file(
    kind: &str,
    content: &str,
    host: &str,
    distro_id: &str,
    distro_major: &str,
) -> (String, usize, usize) {
    if kind == "apt" {
        let (out, changed) = rewrite_apt(content, host);
        (out, changed, 0)
    } else {
        rewrite_yum(content, host, distro_id, distro_major)
    }
}

fn rewrite_apt(content: &str, host: &str) -> (String, usize) {
    let mut out = String::with_capacity(content.len());
    let mut changed = 0usize;
    for line in content.split_inclusive('\n') {
        let (body, nl) = split_nl(line);
        let trimmed = body.trim_start();
        let indent = &body[..body.len() - trimmed.len()];
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push_str(line);
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("URIs:") {
            let mut hits = 0usize;
            let tokens: Vec<String> = rest
                .split_whitespace()
                .map(|tok| {
                    if let Some(next) = replace_host(tok, host) {
                        if next != tok {
                            hits += 1;
                        }
                        next
                    } else {
                        tok.to_string()
                    }
                })
                .collect();
            if hits > 0 {
                changed += 1;
                out.push_str(indent);
                out.push_str("URIs: ");
                out.push_str(&tokens.join(" "));
                out.push_str(nl);
            } else {
                out.push_str(line);
            }
            continue;
        }
        let mut parts: Vec<String> = trimmed.split_whitespace().map(str::to_string).collect();
        if matches!(parts.first().map(String::as_str), Some("deb") | Some("deb-src")) {
            let mut hits = 0usize;
            for token in parts.iter_mut().skip(1) {
                if is_http_url(token) {
                    if let Some(next) = replace_host(token, host) {
                        if &next != token {
                            *token = next;
                            hits += 1;
                        }
                    }
                    break;
                }
            }
            if hits > 0 {
                changed += 1;
                out.push_str(indent);
                out.push_str(&parts.join(" "));
                out.push_str(nl);
                continue;
            }
        }
        out.push_str(line);
    }
    (out, changed)
}

/// yum 重写：baseurl 换主机；CentOS 7 的 mirrorlist.centos.org 与 EPEL metalink
/// 改为对应镜像站的 baseurl（这两类地址没有镜像站提供等价的 mirrorlist 服务）。
fn rewrite_yum(
    content: &str,
    host: &str,
    distro_id: &str,
    distro_major: &str,
) -> (String, usize, usize) {
    let mut out = String::with_capacity(content.len());
    let mut changed = 0usize;
    let mut skipped = 0usize;
    let mut stanza = String::new();
    for line in content.split_inclusive('\n') {
        let (body, nl) = split_nl(line);
        let trimmed = body.trim_start();
        let indent = &body[..body.len() - trimmed.len()];
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            stanza = trimmed[1..trimmed.len() - 1].trim().to_ascii_lowercase();
            out.push_str(line);
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            out.push_str(line);
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            out.push_str(line);
            continue;
        };
        let key_lower = key.trim().to_ascii_lowercase();
        match key_lower.as_str() {
            "baseurl" => {
                let mut hits = 0usize;
                let urls: Vec<String> = value
                    .split_whitespace()
                    .map(|tok| {
                        if let Some(next) = replace_host(tok.trim_end_matches(','), host) {
                            if next != tok {
                                hits += 1;
                            }
                            if tok.ends_with(',') {
                                format!("{next},")
                            } else {
                                next
                            }
                        } else {
                            tok.to_string()
                        }
                    })
                    .collect();
                if hits > 0 {
                    changed += 1;
                    out.push_str(indent);
                    out.push_str(key.trim());
                    out.push('=');
                    out.push_str(&urls.join(" "));
                    out.push_str(nl);
                } else {
                    out.push_str(line);
                }
            }
            "mirrorlist" | "metalink" => {
                let url = value.trim();
                let vault = centos7_vault_base(url, distro_id, distro_major, host, &stanza);
                let epel = epel_base(url, host);
                if let Some(base) = vault.or(epel) {
                    changed += 1;
                    out.push_str(indent);
                    out.push_str("baseurl=");
                    out.push_str(&base);
                    out.push_str(nl);
                } else {
                    skipped += 1;
                    out.push_str(line);
                }
            }
            _ => out.push_str(line),
        }
    }
    (out, changed, skipped)
}

fn centos7_vault_base(
    url: &str,
    distro_id: &str,
    distro_major: &str,
    host: &str,
    stanza: &str,
) -> Option<String> {
    if distro_id != "centos" || distro_major != "7" || !url.contains("mirrorlist.centos.org") {
        return None;
    }
    let repo = match stanza {
        "base" | "os" => "os",
        "updates" => "updates",
        "extras" => "extras",
        "centosplus" => "centosplus",
        "fasttrack" => "fasttrack",
        _ => return None,
    };
    Some(format!("https://{host}/centos-vault/7.9.2009/{repo}/$basearch/"))
}

fn epel_base(url: &str, host: &str) -> Option<String> {
    if !url.contains("mirrors.fedoraproject.org") && !url.contains("mirrorlist") {
        return None;
    }
    let (_, query) = url.split_once("repo=")?;
    let repo = query.split('&').next()?.trim();
    let n = repo.strip_prefix("epel-")?;
    if n.is_empty() || !n.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("https://{host}/epel/{n}/$basearch/"))
}

pub(super) fn validate_backup_id(id: &str) -> Result<String, String> {
    let ok = id.len() == 15
        && id.as_bytes()[8] == b'-'
        && id
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 8 || b.is_ascii_digit());
    if ok {
        Ok(id.to_string())
    } else {
        Err("备份标识不合法".to_string())
    }
}

/// 把镜像站 URL 里的变量展开成可直接探测的地址（最多 4 条）。
pub(super) fn concrete_probe_urls(
    contents: &[(String, String)],
    arch: &str,
    releasever: &str,
) -> Vec<String> {
    let arch = if arch.is_empty() { "$basearch" } else { arch };
    let mut out: Vec<String> = Vec::new();
    for (_, content) in contents {
        for line in content.lines() {
            let t = line.trim();
            let candidates: Vec<&str> = if let Some(rest) = t.strip_prefix("URIs:") {
                rest.split_whitespace().collect()
            } else if let Some((k, v)) = t.split_once('=') {
                if k.trim().eq_ignore_ascii_case("baseurl") {
                    v.split_whitespace().collect()
                } else {
                    Vec::new()
                }
            } else {
                t.split_whitespace()
                    .filter(|tok| !matches!(*tok, "deb" | "deb-src"))
                    .collect()
            };
            for tok in candidates {
                if !is_http_url(tok) {
                    continue;
                }
                let url = tok
                    .replace("$basearch", arch)
                    .replace("$releasever", releasever)
                    .replace("$infra", "");
                if url.contains('$') {
                    continue;
                }
                if !out.iter().any(|u| u == &url) {
                    out.push(url);
                }
                if out.len() >= 4 {
                    return out;
                }
            }
        }
    }
    out
}

/// 应用换源：先（可选）探测目标镜像可达，再备份原文件、写入新内容。
pub(super) fn apply_script(
    preset: &str,
    host: &str,
    package_manager: &str,
    files: &[(String, String)],
    probe: bool,
    probe_urls: &[String],
) -> String {
    let mut script = String::from("set -e\n");
    if probe && !probe_urls.is_empty() {
        script.push_str("PROBE_FAIL=0\n");
        script.push_str("if command -v curl >/dev/null 2>&1; then\n");
        for url in probe_urls {
            script.push_str(&format!(
                "  curl -fsSL -o /dev/null --max-time 8 --range 0-0 {url} || {{ echo \"镜像不可达：{url}\" >&2; PROBE_FAIL=1; }}\n",
                url = sq(url)
            ));
        }
        script.push_str("elif command -v wget >/dev/null 2>&1; then\n");
        for url in probe_urls {
            script.push_str(&format!(
                "  wget -q --spider --timeout=8 {url} || {{ echo \"镜像不可达：{url}\" >&2; PROBE_FAIL=1; }}\n",
                url = sq(url)
            ));
        }
        script.push_str("fi\n");
        script.push_str("[ \"$PROBE_FAIL\" = 0 ] || { echo '目标镜像不可达，未做任何修改' >&2; exit 3; }\n");
    }

    script.push_str("TS=$(date +%Y%m%d-%H%M%S)\n");
    script.push_str("DIR=/etc/rhost/mirror-backup/$TS\n");
    script.push_str("mkdir -p \"$DIR/files\"\n");
    script.push_str(": > \"$DIR/manifest\"\n");
    script.push_str("for f in");
    for (path, _) in files {
        script.push(' ');
        script.push_str(&sq(path));
    }
    script.push_str("; do\n");
    script.push_str("  [ -f \"$f\" ] || continue\n");
    script.push_str("  rel=${f#/}\n");
    script.push_str("  mkdir -p \"$DIR/files/$(dirname \"$rel\")\"\n");
    script.push_str("  cp -a \"$f\" \"$DIR/files/$rel\"\n");
    script.push_str("  printf '%s\\n' \"$rel\" >> \"$DIR/manifest\"\n");
    script.push_str("done\n");
    script.push_str(&format!(
        "printf 'preset=%s\\npackage_manager=%s\\ncreated=%s\\n' {} {} \"$(date '+%Y-%m-%d %H:%M:%S')\" > \"$DIR/meta\"\n",
        sq(preset),
        sq(package_manager)
    ));
    for (path, content) in files {
        script.push_str(&format!(
            "base64 -d > {} <<'RHOST_MIRROR_B64'\n{}\nRHOST_MIRROR_B64\n",
            sq(path),
            B64.encode(content.as_bytes())
        ));
    }
    script.push_str("echo \"BACKUP_ID=$TS\"\n");
    script.push_str(&format!("echo '软件源已切换到 {}'\n", host));
    script
}

/// 还原：先把当前文件另存一份（还原也可回退），再覆盖回备份内容。
pub(super) fn restore_script(backup: &str) -> Result<String, String> {
    let id = validate_backup_id(backup)?;
    let dir = format!("/etc/rhost/mirror-backup/{id}");
    let mut script = String::new();
    script.push_str("set -e\n");
    script.push_str(&format!("B={}\n", sq(&dir)));
    script.push_str("[ -d \"$B\" ] || { echo '备份不存在' >&2; exit 1; }\n");
    script.push_str("[ -f \"$B/manifest\" ] || { echo '备份缺少清单' >&2; exit 1; }\n");
    script.push_str("TS=$(date +%Y%m%d-%H%M%S)\n");
    script.push_str("NEW=/etc/rhost/mirror-backup/$TS\n");
    script.push_str("mkdir -p \"$NEW/files\"\n");
    script.push_str(": > \"$NEW/manifest\"\n");
    script.push_str("while IFS= read -r rel; do\n");
    script.push_str("  [ -n \"$rel\" ] || continue\n");
    script.push_str("  case \"$rel\" in /*|*..*) echo '备份清单不合法' >&2; exit 1;; esac\n");
    script.push_str("  [ -f \"$B/files/$rel\" ] || { echo \"备份缺少文件：$rel\" >&2; exit 1; }\n");
    script.push_str("  if [ -f \"/$rel\" ]; then\n");
    script.push_str("    mkdir -p \"$NEW/files/$(dirname \"$rel\")\"\n");
    script.push_str("    cp -a \"/$rel\" \"$NEW/files/$rel\"\n");
    script.push_str("    printf '%s\\n' \"$rel\" >> \"$NEW/manifest\"\n");
    script.push_str("  fi\n");
    script.push_str("  mkdir -p \"$(dirname \"/$rel\")\"\n");
    script.push_str("  cp -a \"$B/files/$rel\" \"/$rel\"\n");
    script.push_str("done < \"$B/manifest\"\n");
    script.push_str(&format!(
        "printf 'preset=%s\\npackage_manager=%s\\ncreated=%s\\n' 'restore' '' \"$(date '+%Y-%m-%d %H:%M:%S')\" > \"$NEW/meta\"\n"
    ));
    script.push_str("echo \"BACKUP_ID=$TS\"\n");
    script.push_str("echo '软件源已还原'\n");
    Ok(script)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"@@OS@@
Linux
@@ARCH@@
x86_64
@@OSREL@@
NAME="CentOS Linux"
VERSION="7 (Core)"
ID="centos"
VERSION_ID="7"
PRETTY_NAME="CentOS Linux 7 (Core)"
@@PKG@@
yum
@@FILES@@
FILE /etc/apt/sources.list
deb http://archive.ubuntu.com/ubuntu focal main restricted
# deb http://archive.ubuntu.com/ubuntu focal universe
deb [arch=amd64] https://mirrors.aliyun.com/ubuntu focal-updates main
FILE /etc/apt/sources.list.d/vendor.sources
Types: deb
URIs: https://deb.debian.org/debian bookworm main
Suites: bookworm
FILE /etc/yum.repos.d/CentOS-Base.repo
[base]
name=CentOS-$releasever - Base
mirrorlist=http://mirrorlist.centos.org/?release=$releasever&arch=$basearch&repo=os&infra=$infra
enabled=1

[updates]
mirrorlist=http://mirrorlist.centos.org/?release=$releasever&arch=$basearch&repo=updates&infra=$infra
baseurl=https://mirrors.aliyun.com/centos-vault/7.8.2003/updates/$basearch/
FILE /etc/yum.repos.d/epel.repo
[epel]
metalink=https://mirrors.fedoraproject.org/metalink?repo=epel-7&arch=$basearch
enabled=1
@@BACKUPS@@
BACKUP 20261116-150000
preset=aliyun
package_manager=yum
created=2026-11-16 15:00:00
FILES 2
@@END@@
"#;

    #[test]
    fn parses_probe_output() {
        let (status, files) = parse_probe(SAMPLE).unwrap();
        assert_eq!(status.os, "Linux");
        assert_eq!(status.distro, "CentOS Linux 7 (Core)");
        assert_eq!(status.distro_id, "centos");
        assert_eq!(status.distro_major, "7");
        assert_eq!(status.package_manager, "yum");
        assert!(status.managed);
        assert_eq!(files.len(), 4);
        assert_eq!(status.hosts.len(), 5);
        assert_eq!(status.backups.len(), 1);
        assert_eq!(status.backups[0].files, 2);
        assert_eq!(status.backups[0].preset, "aliyun");
        assert_eq!(status.presets.len(), 6);
    }

    #[test]
    fn describes_apt_and_yum_entries() {
        let (_, files) = parse_probe(SAMPLE).unwrap();
        let apt = describe_file(&files[0]);
        assert_eq!(apt.kind, "apt");
        assert_eq!(apt.urls.len(), 2);
        let deb822 = describe_file(&files[1]);
        assert_eq!(deb822.urls, vec!["https://deb.debian.org/debian"]);
        let yum = describe_file(&files[2]);
        assert!(yum.mirrorlist);
        assert!(yum.urls.iter().any(|u| u.contains("mirrors.aliyun.com")));
        let epel = describe_file(&files[3]);
        assert!(epel.mirrorlist);
    }

    #[test]
    fn rewrites_apt_sources() {
        let (out, changed) = rewrite_apt(
            "deb http://archive.ubuntu.com/ubuntu focal main\ndeb-src https://mirrors.aliyun.com/ubuntu focal main\n# deb http://archive.ubuntu.com/ubuntu focal universe\n",
            "mirrors.tuna.tsinghua.edu.cn",
        );
        assert_eq!(changed, 2);
        assert!(out.contains("https://mirrors.tuna.tsinghua.edu.cn/ubuntu focal main"));
        assert!(out.contains("http://archive.ubuntu.com/ubuntu focal universe"));
    }

    #[test]
    fn rewrites_deb822_uris() {
        let (out, changed) = rewrite_apt(
            "Types: deb\nURIs: https://deb.debian.org/debian https://security.debian.org/debian-security\nSuites: bookworm\n",
            "mirrors.aliyun.com",
        );
        assert_eq!(changed, 1);
        assert!(out.contains("URIs: https://mirrors.aliyun.com/debian https://mirrors.aliyun.com/debian-security"));
    }

    #[test]
    fn rewrites_yum_baseurl_and_centos7_mirrorlist() {
        let content = "[base]\nname=CentOS\nmirrorlist=http://mirrorlist.centos.org/?release=$releasever&arch=$basearch&repo=os&infra=$infra\n\n[updates]\nbaseurl=https://mirrors.aliyun.com/centos-vault/7.8.2003/updates/$basearch/\n";
        let (out, changed, skipped) =
            rewrite_file("yum", content, "mirrors.tuna.tsinghua.edu.cn", "centos", "7");
        assert_eq!(changed, 2);
        assert_eq!(skipped, 0);
        assert!(out.contains("baseurl=https://mirrors.tuna.tsinghua.edu.cn/centos-vault/7.9.2009/os/$basearch/"));
        assert!(out.contains("baseurl=https://mirrors.tuna.tsinghua.edu.cn/centos-vault/7.8.2003/updates/$basearch/"));
    }

    #[test]
    fn rewrites_epel_metalink() {
        let content = "[epel]\nmetalink=https://mirrors.fedoraproject.org/metalink?repo=epel-7&arch=$basearch\n";
        let (out, changed, skipped) = rewrite_file("yum", content, "mirrors.aliyun.com", "centos", "7");
        assert_eq!(changed, 1);
        assert_eq!(skipped, 0);
        assert!(out.contains("baseurl=https://mirrors.aliyun.com/epel/7/$basearch/"));
    }

    #[test]
    fn leaves_unknown_metalink_untouched() {
        let content = "[baseos]\nmetalink=https://mirrors.rockylinux.org/metalink?repo=rocky-9-baseos&arch=$basearch\n";
        let (out, changed, skipped) = rewrite_file("yum", content, "mirrors.aliyun.com", "rocky", "9");
        assert_eq!(changed, 0);
        assert_eq!(skipped, 1);
        assert!(out.contains("metalink="));
    }

    #[test]
    fn validates_backup_ids() {
        assert!(validate_backup_id("20261116-150000").is_ok());
        assert!(validate_backup_id("../../etc").is_err());
        assert!(validate_backup_id("20261116_150000").is_err());
        assert!(validate_backup_id("2026111-150000").is_err());
    }

    #[test]
    fn builds_apply_script_with_backup_and_base64() {
        let files = vec![(
            "/etc/apt/sources.list".to_string(),
            "deb https://mirrors.aliyun.com/ubuntu jammy main\n".to_string(),
        )];
        let script = apply_script(
            "aliyun",
            "mirrors.aliyun.com",
            "apt",
            &files,
            true,
            &["https://mirrors.aliyun.com/ubuntu/dists/jammy/Release".to_string()],
        );
        assert!(script.contains("/etc/rhost/mirror-backup/$TS"));
        assert!(script.contains("cp -a \"$f\""));
        assert!(script.contains("base64 -d > '/etc/apt/sources.list'"));
        assert!(script.contains("<<'RHOST_MIRROR_B64'"));
        assert!(script.contains("--range 0-0 'https://mirrors.aliyun.com/ubuntu/dists/jammy/Release'"));
        assert!(script.contains("preset=%s"));
    }

    #[test]
    fn builds_restore_script() {
        let script = restore_script("20261116-150000").unwrap();
        assert!(script.contains("B='/etc/rhost/mirror-backup/20261116-150000'"));
        assert!(script.contains("while IFS= read -r rel"));
        assert!(restore_script("../x").is_err());
    }

    #[test]
    fn expands_probe_url_variables() {
        let files = vec![(
            "/etc/yum.repos.d/epel.repo".to_string(),
            "[epel]\nbaseurl=https://mirrors.aliyun.com/epel/7/$basearch/\n".to_string(),
        )];
        let urls = concrete_probe_urls(&files, "x86_64", "7");
        assert_eq!(urls, vec!["https://mirrors.aliyun.com/epel/7/x86_64/"]);
    }
}
