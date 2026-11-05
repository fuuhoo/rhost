//! 堡垒机「外部连接」入口：解析 ssh:// / sftp:// 链接与 Xshell / WinSCP 命令行，
//! 交给前端打开成一条不保存的单通道会话。
//!
//! 三条唤醒路径共用同一套解析与投递逻辑：
//! - 冷启动 argv：堡垒机客户端直接拉起可执行文件并带链接参数
//! - 单实例回调：应用已在运行时再次被拉起，第二个进程交出 argv 后立即退出
//! - 系统 URL 事件：LaunchServices 把 ssh:// / sftp:// 链接派发给运行中的实例
//!
//! 投递采用「入队 + 事件」双轨：冷启动时前端监听器尚未挂载，事件会丢，
//! 由前端挂载后调用 [`take_external_links`] 取空队列补齐；两条路径都以队列
//! 为唯一真相源，因此重复触发不会重复开会话。

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::applog;
use crate::applog::events as ev;

/// 一条待打开的外部连接
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalLink {
    /// sftp:// 链接：只开 SFTP 子系统通道，不开 PTY
    pub sftp: bool,
    pub user: String,
    /// 链接内嵌密码：仅存在于内存与队列，绝不落盘、绝不写钥匙串
    pub password: Option<String>,
    pub host: String,
    pub port: u16,
    /// 标签名（Xshell `-newtab` / WinSCP `/sessionname=`）；缺省时前端回落 user@host:port
    pub name: Option<String>,
}

/// 交给前端的一项：链接，或被拒链接的原因文本
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<ExternalLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 一次命令行解析的产出
#[derive(Debug, Default)]
pub struct Launch {
    pub links: Vec<ExternalLink>,
    pub errors: Vec<String>,
}

impl Launch {
    fn push_link(&mut self, raw: &str) {
        match parse_link(raw) {
            Ok(link) => self.links.push(link),
            Err(reason) => self.errors.push(reason),
        }
    }

    fn is_empty(&self) -> bool {
        self.links.is_empty() && self.errors.is_empty()
    }
}

/// 待前端领取的外部连接队列
#[derive(Default)]
pub struct PendingLinks {
    items: Mutex<Vec<OpenItem>>,
}

impl PendingLinks {
    fn push(&self, items: Vec<OpenItem>) {
        self.items.lock().unwrap().extend(items);
    }

    /// 取空并返回全部待处理项（取出即清空，保证前端只处理一次）
    pub fn take(&self) -> Vec<OpenItem> {
        std::mem::take(&mut *self.items.lock().unwrap())
    }
}

/* =========================================================
 *  链接与命令行解析
 * ========================================================= */

/// percent 解码（RFC 3986  userinfo 段）：非法转义按字面保留，
/// 堡垒机链接里的密码常含 `@`、`%` 等需要转义的字符
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Ok(h), Ok(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(()),
    }
}

/// 解析 `ssh://user[:password]@host[:port]` / `sftp://…`。
///
/// 用户名本身含 `@` 时按 URL 规则只有最后一个 `@` 之后是地址，故用 `rsplit_once`；
/// 端口缺省 22；路径段（WinSCP 用它表达起始目录）忽略，远端按惯例打开家目录。
pub fn parse_link(raw: &str) -> Result<ExternalLink, String> {
    // 有的堡垒机客户端把整串命令行塞进一个参数（`-url "ssh://h -newtab"`）：
    // URL 里不会出现裸空格（要空格得写成 %20），所以截到第一个空白之前，后面当杂散参数丢掉
    let raw = raw.split_whitespace().next().unwrap_or(raw);
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or_else(|| format!("链接「{raw}」不是 ssh:// 或 sftp:// 地址"))?;
    let sftp = match scheme.to_ascii_lowercase().as_str() {
        "ssh" => false,
        "sftp" => true,
        other => return Err(format!("链接协议「{other}://」暂不支持，仅支持 ssh:// 与 sftp://")),
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let (creds, addr) = authority
        .rsplit_once('@')
        .ok_or_else(|| format!("链接「{raw}」缺少用户名（形如 ssh://user@host）"))?;

    let (user, password) = match creds.split_once(':') {
        Some((u, p)) => (
            percent_decode(u),
            (!p.is_empty()).then(|| percent_decode(p)),
        ),
        None => (percent_decode(creds), None),
    };
    if user.is_empty() {
        return Err(format!("链接「{raw}」缺少用户名"));
    }

    // IPv6 字面量带方括号：`[fe80::1]:2222`
    let (host, port_text) = match addr.strip_prefix('[') {
        Some(inner) => {
            let (v6, rest) = inner
                .split_once(']')
                .ok_or_else(|| format!("链接「{raw}」IPv6 地址缺少右方括号"))?;
            (v6.to_string(), rest.strip_prefix(':'))
        }
        None => match addr.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p)),
            None => (addr.to_string(), None),
        },
    };
    if host.is_empty() {
        return Err(format!("链接「{raw}」缺少地址"));
    }
    let port = match port_text {
        None | Some("") => 22,
        Some(p) => p
            .parse::<u16>()
            .map_err(|_| format!("链接「{raw}」端口「{p}」非法"))?,
    };
    if port == 0 {
        return Err(format!("链接「{raw}」端口不能为 0"));
    }

    Ok(ExternalLink {
        sftp,
        user,
        password,
        host,
        port,
        name: None,
    })
}

/// `-opt=value` / `/opt=value` 拆分；返回小写选项名与原始值
fn split_option(arg: &str) -> Option<(&str, &str)> {
    let body = arg.strip_prefix(['-', '/'])?;
    let (flag, value) = body.split_once('=')?;
    Some((flag, value))
}

/// 是否为选项 token（`-url`、`/sessionname=`、Windows 的 `/ini` 等）
fn is_option(arg: &str) -> bool {
    arg.starts_with(['-', '/'])
}

/// 解析堡垒机传入的命令行（等价 Xshell / WinSCP 的调用形式，大小写不敏感）：
///
/// ```sh
/// rhost ssh://user:pass@host:2222
/// rhost -url ssh://user:pass@host:2222 -newtab 标签名
/// rhost /sessionname=标签名 sftp://user:pass@host:2222
/// ```
///
/// `-url` 的值可写作下一个 token 或 `-url=链接`；`-newtab`、`-sessionname`、
/// `/sessionname=` 同义；`-newwin`（本应用只有一扇窗）与 `/ini=nul`、
/// `/newinstance` 等未知选项一律忽略。
pub fn parse_command_line(args: &[String]) -> Launch {
    let mut launch = Launch::default();
    // argv[0] 为可执行文件路径
    let mut name: Option<String> = None;
    let mut expect: Expect = Expect::None;

    for arg in args.iter().skip(1) {
        let token = arg.trim();
        if token.is_empty() {
            continue;
        }
        // 上一个 token 是「值待填」的选项（-url / -newtab）
        match expect {
            Expect::Url => {
                launch.push_link(token);
                expect = Expect::None;
                continue;
            }
            Expect::Name => {
                name = Some(token.to_string());
                expect = Expect::None;
                continue;
            }
            Expect::None => {}
        }

        if let Some((flag, value)) = split_option(token) {
            match flag.to_ascii_lowercase().as_str() {
                "url" => launch.push_link(value),
                "newtab" | "sessionname" | "tabname" => name = Some(value.to_string()),
                // 未知带值选项（/ini=nul、/password=x 等）连同其值一起忽略
                _ => {}
            }
            continue;
        }

        let lowered = token.to_ascii_lowercase();
        if is_option(token) {
            match lowered.as_str() {
                "-url" | "/url" => expect = Expect::Url,
                "-newtab" | "/newtab" | "-sessionname" | "/sessionname" | "-tabname"
                | "/tabname" => expect = Expect::Name,
                // -newwin 及其它未知开关：忽略
                _ => {}
            }
            continue;
        }

        // 位置参数按链接处理；不含 :// 的自由 token（dev 参数等）忽略
        if token.contains("://") {
            launch.push_link(token);
        }
    }

    // 标签名统一回填给未命名的链接（一次调用通常只有一条链接）
    if let Some(n) = name {
        for link in &mut launch.links {
            if link.name.is_none() {
                link.name = Some(n.clone());
            }
        }
    }
    launch
}

enum Expect {
    None,
    Url,
    Name,
}

/* =========================================================
 *  投递
 * ========================================================= */

/// 来源标记（落盘日志字段，便于区分堡垒机三种拉起方式）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// 冷启动命令行
    Cli,
    /// 已在运行时被再次拉起，由单实例插件转交 argv
    SecondInstance,
    /// 系统把 ssh:// / sftp:// 链接交给运行中的实例
    UrlEvent,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Cli => "cli",
            Source::SecondInstance => "second_instance",
            Source::UrlEvent => "url_event",
        }
    }
}

/// 解析结果入队并通知前端；同时把窗口带到前台（堡垒机点开链接后不应留在后台）
pub fn dispatch(app: &AppHandle, launch: Launch, source: Source) {
    if launch.is_empty() {
        return;
    }
    let Launch { links, errors } = launch;
    applog::emit(
        log::Level::Info,
        "external",
        ev::EXTERNAL_LINK_OPEN,
        None,
        "收到堡垒机外部连接链接",
        Some(serde_json::json!({
            "source": source.as_str(),
            "count": links.len(),
            "rejected": errors.len(),
            // 只记地址与协议，绝不记密码
            "targets": links
                .iter()
                .map(|l| {
                    let scheme = if l.sftp { " (sftp)" } else { "" };
                    format!("{}@{}:{}{scheme}", l.user, l.host, l.port)
                })
                .collect::<Vec<_>>(),
        })),
    );
    for reason in &errors {
        applog::emit(
            log::Level::Warn,
            "external",
            ev::EXTERNAL_LINK_REJECTED,
            None,
            "链接无法解析，已忽略",
            Some(serde_json::json!({ "reason": reason, "source": source.as_str() })),
        );
    }

    let mut items = Vec::with_capacity(links.len() + errors.len());
    items.extend(links.into_iter().map(|link| OpenItem {
        link: Some(link),
        error: None,
    }));
    items.extend(errors.into_iter().map(|error| OpenItem {
        link: None,
        error: Some(error),
    }));

    app.state::<PendingLinks>().push(items);
    let _ = app.emit("external-open", ());
    bring_window_forward(app);
}

/// 解析命令行并投递（冷启动与单实例回调共用）
pub fn handle_command_line(app: &AppHandle, args: &[String], source: Source) {
    dispatch(app, parse_command_line(args), source);
}

/// 系统 URL 事件投递：仅处理本应用声明的 ssh:// / sftp://，其余静默忽略
/// （浏览器里点开的 http(s) 等链接不该打扰用户）
pub fn handle_urls(app: &AppHandle, urls: Vec<String>) {
    let mut launch = Launch::default();
    for url in urls {
        let scheme = url.split_once("://").map(|(s, _)| s).unwrap_or_default();
        if matches!(scheme.to_ascii_lowercase().as_str(), "ssh" | "sftp") {
            launch.push_link(&url);
        }
    }
    dispatch(app, launch, Source::UrlEvent);
}

fn bring_window_forward(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// 前端领取并清空外部连接队列（冷启动链接由此补送；事件到达后同样调它）
#[tauri::command]
pub fn take_external_links(state: tauri::State<'_, PendingLinks>) -> Vec<OpenItem> {
    state.take()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn link_basic_and_defaults() {
        let l = parse_link("ssh://root@10.0.0.1").unwrap();
        assert_eq!((l.user.as_str(), l.host.as_str(), l.port, l.sftp), ("root", "10.0.0.1", 22, false));
        assert_eq!(l.password, None);
        assert!(parse_link("sftp://u@h:2222").unwrap().sftp);
    }

    #[test]
    fn link_password_with_escaped_at() {
        let l = parse_link("ssh://u:p%40ss@host:2222").unwrap();
        assert_eq!(l.password.as_deref(), Some("p@ss"));
    }

    /// 用户名含 @ 时，只有最后一个 @ 之后是地址
    /// 用户名含 `@`（堡垒机常见 domain\user@corp 形式）时，只有最后一个 @ 之后是地址
    #[test]
    fn link_user_containing_at() {
        let l = parse_link("ssh://domain%2Fuser@corp@example.com").unwrap();
        assert_eq!(l.user, "domain/user@corp");
        assert_eq!(l.host, "example.com");
    }

    #[test]
    fn link_ignores_path_and_query() {
        assert_eq!(parse_link("sftp://u@h/srv/data?x=1").unwrap().host, "h");
    }

    /// 客户端把整串参数塞进一个值里（`-url "ssh://h -newtab"`）：截到空白前，后面的丢掉
    #[test]
    fn link_stops_at_the_first_space() {
        let l = parse_link("ssh://u@h:2222 -newtab 名字").unwrap();
        assert_eq!((l.user.as_str(), l.host.as_str(), l.port), ("u", "h", 2222));
        assert_eq!(parse_link("  ssh://u@h  ").unwrap().host, "h");
    }

    #[test]
    fn link_ipv6_literal() {
        let l = parse_link("ssh://u@[fe80::1]:2222").unwrap();
        assert_eq!(l.host, "fe80::1");
        assert_eq!(l.port, 2222);
    }

    #[test]
    fn link_rejects_bad_forms() {
        assert!(parse_link("telnet://u@h").is_err());
        assert!(parse_link("ssh://h").is_err());
        assert!(parse_link("ssh://u@").is_err());
        assert!(parse_link("ssh://u@h:99999").is_err());
        assert!(parse_link("ssh://u@h:0").is_err());
        assert!(parse_link("just-a-host").is_err());
    }

    #[test]
    fn cli_positional_url() {
        let p = parse_command_line(&args(&["rhost", "ssh://u@h:22"]));
        assert_eq!(p.links.len(), 1);
        assert!(p.errors.is_empty());
    }

    #[test]
    fn cli_xshell_url_and_newtab() {
        let p = parse_command_line(&args(&[
            "rhost", "-url", "ssh://u@h:2222", "-newtab", "生产堡垒",
        ]));
        assert_eq!(p.links[0].name.as_deref(), Some("生产堡垒"));
        assert_eq!(p.links[0].port, 2222);
    }

    #[test]
    fn cli_winscp_sessionname_and_unknown_options() {
        let p = parse_command_line(&args(&[
            "rhost", "/sessionname=台账", "/ini=nul", "/newinstance", "sftp://u@h",
        ]));
        assert_eq!(p.links.len(), 1);
        assert!(p.links[0].sftp);
        assert_eq!(p.links[0].name.as_deref(), Some("台账"));
    }

    /// 选项大小写混写与 `=` 形式
    #[test]
    fn cli_is_case_insensitive() {
        let p = parse_command_line(&args(&["rhost", "-URL=SFTP://U@H", "-NewWin"]));
        assert_eq!(p.links.len(), 1);
        assert!(p.links[0].sftp);
        assert!(p.errors.is_empty());
    }

    #[test]
    fn cli_reports_reason_for_bad_link() {
        let p = parse_command_line(&args(&["rhost", "http://u@h"]));
        assert!(p.links.is_empty());
        assert_eq!(p.errors.len(), 1);
    }

    /// 不含 `://` 的自由 token（dev 参数、残留值）静默忽略，不产生报错 toast
    #[test]
    fn cli_ignores_non_url_positionals() {
        let p = parse_command_line(&args(&["target/debug/rhost", "--hot-reload", "dev"]));
        assert!(p.links.is_empty() && p.errors.is_empty());
    }

    /// 队列取出即清空：事件与挂载后补领两条路径不会重复开会话
    #[test]
    fn pending_queue_drains_once() {
        let q = PendingLinks::default();
        q.push(vec![OpenItem {
            link: Some(ExternalLink {
                sftp: false,
                user: "u".into(),
                password: None,
                host: "h".into(),
                port: 22,
                name: None,
            }),
            error: None,
        }]);
        assert_eq!(q.take().len(), 1);
        assert!(q.take().is_empty());
    }
}
