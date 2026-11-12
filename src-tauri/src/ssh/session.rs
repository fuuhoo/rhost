//! SSH 交互式 PTY 会话：russh 纯异步实现，禁止 spawn_blocking。
//!
//! 数据流：
//! ```text
//! russh ChannelReadHalf.wait()            前端键盘输入
//!   │  (PTY 原始字节 / 控制消息)                │ Vec<u8>
//!   ▼                                          ▼
//! PtyEvent 通道 (mpsc) ──► merge_task    write_task ──► ChannelWriteHalf.data_bytes()
//!   (4KB / 5ms 攒包)                        (直接透传)
//!   │
//!   ▼ 二进制帧 (1B 类型 + 4B 长度 + payload)
//! frame_rx ──► ipc 层转发到 Tauri Channel<Vec<u8>>
//! ```

use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::BytesMut;
use log::{debug, warn};
use russh::client;
use russh::{ChannelMsg, Disconnect};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::frame::{FrameType, encode_frame};
use super::sftp::SftpState;
use super::tunnel::{REMOTE_CONNECT_TIMEOUT, RemoteRegistry, TunnelManager, run_relay_conn};
use super::{AuthMethod, HostKeyCheck, SessionConfig, SshError};
use crate::applog;
use crate::applog::events as ev;

/// 小包合并阈值：攒够 4KB 立即发帧
const MERGE_BYTES: usize = 4096;
/// 小包合并窗口：距本批首字节 5ms 仍未攒满也发帧
const MERGE_WINDOW: Duration = Duration::from_millis(5);
/// PTY 事件通道容量（背压：满了则 read_task 暂停读取，由 TCP 窗口自然限流）
const EVENT_QUEUE: usize = 128;
/// 帧输出通道容量
const FRAME_QUEUE: usize = 128;
/// 前端输入通道容量（键盘输入量小）
const WRITE_QUEUE: usize = 64;

/// RTT 探测周期：30s。独立于传输层 keepalive（后者按主机配置），
/// 仅用于状态栏延迟显示；RTT 变化慢，更高频无收益
const RTT_INTERVAL: Duration = Duration::from_secs(30);
/// 单次 ping 等待上限：不响应 global request 的 sshd（罕见）静默跳过本轮
const RTT_TIMEOUT: Duration = Duration::from_secs(5);

/// PTY TERM 终端类型：申请 PTY 与上报状态栏共用此常量，保证两处一致
const TERM_TYPE: &str = "xterm-256color";
/// 终端字节流编码：xterm.js 仅按 UTF-8 解码，全链路透传不转码
const TERM_ENCODING: &str = "UTF-8";

/// SSH 握手真实协商出的算法（`kex_done` 回调填入，连接建立后读取上报）。
#[derive(Clone, Debug, Default)]
pub(crate) struct NegotiatedAlgo {
    /// 服务器主机密钥算法标准名（如 `ssh-ed25519`、`ecdsa-sha2-nistp256`）
    host_key: String,
    /// 对称加密算法标准名（如 `aes256-gcm@openssh.com`、`chacha20-poly1305@openssh.com`）
    cipher: String,
}

/// 主机密钥校验结论：`check_server_key` 写入共享槽，
/// `connect_and_auth` 连接失败时读出做错误映射（Mismatch/Unknown → 协议前缀错误）。
#[derive(Clone, Debug)]
pub(crate) struct HostKeyVerdict {
    pub kind: VerdictKind,
    /// 主机密钥算法标准名（如 `ssh-ed25519`）
    pub algo: String,
    /// SHA256 指纹（`SHA256:…`）
    pub fingerprint: String,
    /// OpenSSH 格式公钥（`algorithm base64`，前端「查看完整公钥」展开区用）
    pub pubkey: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerdictKind {
    /// trust 重试命中（用户确认指纹 = 本次握手指纹）：通过并落盘
    Trusted,
    /// 磁盘记录比对一致：通过
    Match,
    /// 密钥变更（确认指纹或已存指纹 ≠ 本次指纹）：拒绝
    Mismatch,
    /// 首连未知（无记录且未信任）：拒绝
    Unknown,
}

/// 主机密钥校验决策表（hostkey-verification-design.md §6.4）：
/// 自上而下命中即止，**trust 覆盖分支必须先于 stored 比对**——mismatch 的
/// 「更新指纹并重连」以 `trust=true, trust_fp=新指纹` 重试时 `stored=Some(旧)`
/// 仍与当前不等，若 stored 比对在前会再次判 Mismatch、落盘永不可达。
/// 纯函数（不触 IO），Trusted 时的落盘由调用方执行；单测覆盖全分支。
fn evaluate_host_key(
    stored: Option<&(String, String)>,
    trust: bool,
    trust_fp: Option<&str>,
    current_fp: &str,
) -> (bool, VerdictKind) {
    if trust {
        match trust_fp {
            // 第 1 行：确认指纹 = 本次指纹 → 通过并落盘（对 stored 不作区分，
            // 首连信任与变更更新统一由此行落盘覆盖）
            Some(fp) if fp == current_fp => return (true, VerdictKind::Trusted),
            // 第 2 行：确认指纹 ≠ 本次指纹 → 二次握手密钥已变（TOFU 竞态防护），拒绝
            Some(_) => return (false, VerdictKind::Mismatch),
            // 第 3 行：trust=true 但未携带确认指纹 → 视同未确认，落回 stored 比对，
            // 绝不落盘（杜绝伪造 IPC 携带 trust=true 直接落盘的旁路）
            None => {}
        }
    }
    match stored {
        // 第 4 行：磁盘记录一致
        Some((_, fp)) if fp == current_fp => (true, VerdictKind::Match),
        // 第 5 行：密钥变更
        Some(_) => (false, VerdictKind::Mismatch),
        // 第 6 行：首连未知
        None => (false, VerdictKind::Unknown),
    }
}

/// russh 客户端回调。
pub(crate) struct ClientHandler {
    /// 协商算法出口：russh 在 KEX 完成时经 `kex_done` 填入，Arc 共享给连接主流程
    algo: Arc<StdMutex<Option<NegotiatedAlgo>>>,
    /// 主机密钥校验结论出口：`check_server_key` 写入，连接主流程读出做错误映射
    ///（`connect_and_auth` 内创建共享，复用 `algo` 槽手法）
    hk: Arc<StdMutex<Option<HostKeyVerdict>>>,
    /// 主机密钥校验输入（`HostKeyPolicy::SkipForTests` 时为 None：无条件放行）
    hk_check: Option<HostKeyCheck>,
    /// 会话标签（主机密钥指纹等握手期日志事件用）
    sid: String,
    /// -R 回调查表：远端来连时按 (绑定地址, 绑定端口) 查出转发目标与规则闸门。
    /// 与 `TunnelManager` 共享同一实例（`SshSession::connect` 创建后 Arc clone）。
    remote: RemoteRegistry,
    /// 会话级转发连接总闸（MAX_CONN_GLOBAL）：-R 回调与 TunnelManager 共用
    global_conns: Arc<Semaphore>,
}

/// 会话初始化脚本（写入远端临时文件，由前端 source）。
///
/// 始终注入 CWD 上报钩子（保证 Shell→SFTP 目录同步与彩色提示符开关无关）；
/// 仅当 `color_prompt=true` 时才覆盖 PS1/PROMPT 并开启 ls/grep 着色，
/// 否则保留用户原有提示符配置。
///
/// 生成 init 脚本中的 export 块：值以单引号包裹（内部 `'` 按 POSIX 规则转义）。
/// 变量名必须匹配 `[A-Za-z_][A-Za-z0-9_]*`、值不得含换行（保持脚本单行结构），
/// 非法项静默跳过——该函数输出直接拼进远端执行的脚本，必须从严过滤注入面。
fn env_export_lines(env: &[(String, String)]) -> String {
    let mut out = String::new();
    for (k, v) in env {
        let valid = k.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && k.chars()
                .skip(1)
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid || v.contains(['\n', '\r']) {
            continue;
        }
        out.push_str(&format!("export {k}='{}'\n", v.replace('\'', r"'\''")));
    }
    out
}

/// POSIX sh 片段：为「远端非 UTF-8」挑选一个**实测可用**的 UTF-8 locale 名。
/// bash/dash/ash/busybox sh 通用。候选名在片段内按
/// `C.UTF-8 C.utf8 en_US.UTF-8 en_US.utf8` 顺序探测：
/// `C.UTF-8`/`C.utf8` 为 glibc≥2.35 与 musl 内置、不绑定语言，优先；
/// `en_US.UTF-8` 兜底 CentOS 7 / RHEL 7-8 / Amazon Linux 2（glibc 较旧、
/// 没有 C.UTF-8，但最小安装通常生成 en_US）；两种拼写都试（Debian 是 C.utf8）。
/// 运行后结果在 `_rh_l`：空串表示当前已是 UTF-8（无需改动，尊重用户
/// locale 偏好），或有 locale 命令且 charmap 正常但候选皆不可用（典型
/// CentOS 极瘦镜像，此时不设以免无效 LC_ALL 触发 glibc 警告）；非空则
/// 可直接 `export LC_ALL="$_rh_l"` 或作为 `LC_ALL="$_rh_l" exec …` 前缀。
/// 调用方负责 `unset _rh_l _rh_c`（exec 路径进程被替换可省略）。
///
/// 为什么必须运行时探测、不能写死 `UTF-8` 或单个 `C.UTF-8`：
/// 1. `UTF-8` 是字符集名，不是 locale 名——`LC_ALL=UTF-8` 系统查无此项，
///    直接报 `No such file or directory` 并退回 C，vim 仍 latin1（实测）；
/// 2. 没有全平台通用的 locale 名：`C.UTF-8` 在 glibc<2.35（CentOS/RHEL/AL2）
///    不存在，写死它会在这些系统静默落空；
/// 3. 判据是 `locale charmap` 实测输出，而非 LANG 字符串含不含 UTF-8
///    （LANG=zh_CN.UTF-8 但未 locale-gen 时变量好看、实际是 ASCII）。
///
/// 另外两点兼容：charmap 匹配放宽 `UTF-?8`（忽略连字符，兼容个别实现输出
/// `UTF8`）；有 `locale` 命令但 `locale charmap` 无输出（busybox 残损 applet）
/// 或根本无该命令（极简 musl）时直接保底 C.UTF-8——musl 按 codeset 宽松
/// 接受并按 UTF-8 工作，glibc 真不支持也只是静默回退 C，不会更差。
const UTF8_LOCALE_PICK_SH: &str = "\
_rh_l=''
if ! printf '%s' \"$(locale charmap 2>/dev/null)\" | grep -qiE '^UTF-?8$'; then
  for _rh_c in C.UTF-8 C.utf8 en_US.UTF-8 en_US.utf8; do
    if LC_ALL=\"$_rh_c\" locale charmap 2>/dev/null | grep -qiE '^UTF-?8$'; then
      _rh_l=$_rh_c
      break
    fi
  done
  if [ -z \"$_rh_l\" ] && { ! command -v locale >/dev/null 2>&1 || [ -z \"$(locale charmap 2>/dev/null)\" ]; }; then
    _rh_l=C.UTF-8
  fi
fi";

/// 生成远端 init 脚本内容。整体设计约束见下：
/// - 脚本经 exec 通道写临时文件后 source，绝不作为键盘输入发送——长命令经
///   PTY 回显既难看又会把 ESC 字节喂给 readline/终端导致命令被毁；
/// - 提示符仍完全由远端 shell 的 PS1/PROMPT 生成（透传架构不变）；
/// - 远端 PS1/PROMPT 已含颜色标记或用户名（`\\u`/`%n`）时跳过，不覆盖用户配置；
/// - 否则注入含用户名（`\\u@\\h`/`%n@%m`）的 PS1：`color_prompt=true` 时带颜色，
///   `color_prompt=false` 时为纯文本，避免远端默认提示符（如 `\\h:\\w\\$`）缺用户名；
/// - PS1 内嵌 ESC 字节（远端 printf 现场生成），\[ \] 标记非打印区间，
///   保证 readline 行宽计算正确；
/// - 脚本刻意**不**发清行序列抹回显：回显行由后端在放行 hold 前按内容剥掉
///   （见 [`late_gate`]）。按游标相对位置清行在 profile 输出与注入交错时
///   会吃错行——要么回显留在屏上，要么把服务器自己的告警行一起抹掉；
/// - 末尾 rm -f 自删临时文件（fd 已打开，删路径不影响 source 读完）。
fn init_script(path: &str, color_prompt: bool, env: &[(String, String)]) -> String {
    // path 由后端生成（/tmp/.ri-<uuid8>），不含单引号/空格，无注入面
    let color_block = if color_prompt {
        "\
_rh_e=$(printf '\\033')
case \"$PS1$PROMPT\" in *'\\\\e['*|*'\\\\033['*|*\"$_rh_e\"*|*'%F'*|*'fg['*|*'\\\\u'*|*'%n'*) ;; *)
  if [ -n \"$ZSH_VERSION\" ]; then PROMPT='%F{green}%n@%m%f:%F{blue}%~%f%# '
  else PS1=\"\\[${_rh_e}[1;32m\\]\\u@\\h\\[${_rh_e}[0m\\]:\\[${_rh_e}[1;34m\\]\\w\\[${_rh_e}[0m\\]\\\\\\$ \"
  fi ;;
esac
unset _rh_e
export CLICOLOR=1
# 用户已定义同名 alias 时绝不覆盖（保留其自定义参数）
alias ls >/dev/null 2>&1 || { ls --color=auto -d . >/dev/null 2>&1 && alias ls='ls --color=auto'; }
alias grep >/dev/null 2>&1 || { echo . | grep --color=auto . >/dev/null 2>&1 && alias grep='grep --color=auto'; }
"
    } else {
        // 无色模式：仍注入含用户名的纯文本 PS1，避免远端默认提示符缺用户名
        "\
case \"$PS1$PROMPT\" in *'\\\\e['*|*'\\\\033['*|*'%F'*|*'fg['*|*'\\\\u'*|*'%n'*) ;; *)
  if [ -n \"$ZSH_VERSION\" ]; then PROMPT='%n@%m:%~%# '
  else PS1='\\u@\\h:\\w\\$ '
  fi ;;
esac
"
    };

    let env_block = env_export_lines(env);

    // UTF-8 字符集保底（init 脚本通道，不依赖 sshd AcceptEnv——任何启动
    // 路径都会 source 本脚本）。容器/精简系统常无 LANG 或 LANG 指向未生成
    // 的 locale（如前端默认注入 zh_CN.UTF-8 但服务器未 locale-gen），此时
    // vim 的 setlocale(LC_ALL,"") 因非 CTYPE 类别回退到无效 LANG 而整体
    // 失败，默认 encoding=latin1，UTF-8 中文显示为 `~x` 乱码；用最高优先级
    // 的 LC_ALL 统一压制（LC_CTYPE 压不住无效 LANG，实测）。探测逻辑与
    // 候选列表见 UTF8_LOCALE_PICK_SH。位于 env_block 之前：LC_ALL 优先级恒
    // 高于其后注入的 LANG，且仅在当前 locale 已失效时 `_rh_l` 才非空，
    // 有效系统零改动、保留用户语言/地区偏好。
    let locale_block = format!(
        "\
{pick}
if [ -n \"$_rh_l\" ]; then
  export LC_ALL=\"$_rh_l\"
fi
unset _rh_l _rh_c
",
        pick = UTF8_LOCALE_PICK_SH,
    );

    format!(
        "\
{locale_block}{env_block}{color_block}# CWD 上报钩子：每次提示符刷新前输出不可见 OSC 6667 序列（内含 $PWD），
# 后端 merge_task 解析后同步 SFTP 文件树到同一目录。
# bash 用 PROMPT_COMMAND，zsh 用 precmd_functions；前置注入不覆盖用户已有钩子。
_rh_cwd_hook() {{ printf '\\033]6667;%s\\007' \"$PWD\"; }}
if [ -n \"$ZSH_VERSION\" ]; then
  precmd_functions=( _rh_cwd_hook \"${{precmd_functions[@]}}\" )
else
  case \";$PROMPT_COMMAND;\" in *\";_rh_cwd_hook;\"*) ;; *)
    PROMPT_COMMAND=\"_rh_cwd_hook${{PROMPT_COMMAND:+;$PROMPT_COMMAND}}\" ;;
  esac
fi
rm -f '{path}'
printf '\\033]6666;rhinit\\007'
"
    )
}

/// 经独立 exec 通道把初始化脚本写入远端 `path`（尽力而为，失败不影响连接）。
/// 以远端退出状态确认落盘成功（cat 写失败/chmod 失败都会反映为非零），
/// 保证前端 source 时文件已完整。成功返回 `Ok(())`，失败返回带原因的 `Err`，
/// 由调用方决定如何记录（失败会连带跳过 CWD 同步、彩色提示符、UTF-8 locale
/// 自动配置，值得在会话日志里以 warn 暴露，而非静默）。
async fn try_upload_script(
    handle: &mut client::Handle<ClientHandler>,
    path: &str,
    color_prompt: bool,
    env: &[(String, String)],
) -> Result<(), String> {
    let mut ch = handle
        .channel_open_session()
        .await
        .map_err(|e| format!("打开 exec 通道失败: {e}"))?;
    ch.exec(true, format!("cat > {path} && chmod 600 {path}"))
        .await
        .map_err(|e| format!("exec 上传命令被拒: {e}"))?;
    let script = init_script(path, color_prompt, env);
    ch.data(script.as_bytes())
        .await
        .map_err(|e| format!("写入脚本数据失败: {e}"))?;
    ch.eof().await.map_err(|e| format!("发送 EOF 失败: {e}"))?;
    // 等 cat 收尾并读取退出状态（ExitStatus 通常先于 Close 到达）
    let mut status: Option<u32> = None;
    while let Some(msg) = ch.wait().await {
        match msg {
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    let _ = ch.close().await;
    match status {
        Some(0) => Ok(()),
        Some(code) => Err(format!(
            "远端写入命令退出码 {code}（{path}，可能 /tmp 不可写或磁盘满）"
        )),
        None => Err(format!("未收到远端退出状态（{path}）")),
    }
}

/// 前端要发送的 source 命令：行首空格配合常见 HISTCONTROL=ignorespace 不进 history。
/// 命令刻意压到最短（删除临时文件由脚本末尾自删完成）：
/// 「PS1 + 本命令」典型宽度 ≈ 21+31 = 52 列，50 列以下终端才可能折行，
/// 折了行的回显剥不干净（见 [`late_gate`]）。该命令由后端在 shell 就绪后
/// 注入（tty 缓冲暂存），前端不参与时序。
fn source_cmd(path: &str) -> String {
    format!(" . \"{path}\" 2>/dev/null")
}

/// 注入脚本执行完成标记：不可见 OSC 序列，xterm 解析后静默丢弃（不上屏）。
/// merge_task 以它为「初始化完成」信号：检测到才放行 hold 住的 PTY 输出，
/// 前端首帧即脚本生效后的最终画面（无原始 PS1、无回显行的中间态）。
const INIT_DONE_MARKER: &[u8] = b"\x1b]6666;rhinit\x07";

/// Shell CWD 上报 OSC 序列前缀：`\x1b]6667;<path>\x07`。
/// 由 init_script 注入的 PROMPT_COMMAND / precmd 钩子在每次提示符刷新前输出，
/// merge_task 扫描到即提取 <path> 并发出 Cwd 帧同步 SFTP 文件树。
const CWD_MARKER_PREFIX: &[u8] = b"\x1b]6667;";
const OSC_TERMINATOR: u8 = 0x07;

/// 判定「shell 就绪」的输出静默阈值：PS1 刷完后 shell 阻塞读输入、不再输出，
/// 此刻注入的回显行才完整。略大于攒包窗口（5ms），保证 PS1 批次完整刷完。
const SHELL_QUIET: Duration = Duration::from_millis(120);

/// 回显剥离窗口的存活时长：hold 放行时注入命令的回显字节未必已到（3s 超时兜底
/// 更是必然未到），故放行后仍按内容继续剥。窗口**不因命中而提前关闭**——同一条
/// 注入命令会被回显两次（内核一次、readline 一次，见 [`late_gate`]）。窗口只在会话
/// 开头存在，且 probe 是本次会话独有的随机路径，不会误伤正常输出。
const LATE_ECHO_TTL: Duration = Duration::from_secs(10);

/// SFTP→Shell cd 同步的回显剥离窗口时长。回显在注入后一个 RTT 内到达，2s 足够
/// 覆盖慢链路；窗口越短，误剥用户手动敲出的同内容 cd 行的概率越低。
const CD_ECHO_TTL: Duration = Duration::from_secs(2);

/// cd 回显行剥除后原位写入的序列：回车 + 清至行尾。旧提示符已上屏，清行后
/// 紧随的新目录提示符正好落在原行——终端只看到目录变化，无 cd 命令、无空行。
const CD_CLEAR_LINE: &[u8] = b"\r\x1b[K";

/// 初始化 hold 状态：PTY 输出先攒在 buf 不上屏；cmd 为 Some 表示等静默注入，
/// None 表示已注入、等脚本末尾 marker。`probe` 是注入命令里的临时文件路径，
/// 用于在攒下的字节里定位并剥掉那一行回显（见 [`late_gate`]）。
struct InitHold {
    buf: BytesMut,
    cmd: Option<Vec<u8>>,
    probe: Vec<u8>,
    tx: mpsc::Sender<WriteReq>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        use russh::keys::HashAlg;
        use russh::keys::PublicKeyBase64;
        // 统一取内层公钥：PublicKeyOrCertificate::public_key() 对普通密钥与证书
        // 均返回 ssh_key::PublicKey，算法名/指纹/OpenSSH 公钥串由此算出（设计 §6.4）
        let pk = key.public_key();
        let algo = pk.algorithm().to_string();
        let fingerprint = pk.fingerprint(HashAlg::Sha256).to_string();
        let pubkey = format!("{algo} {}", pk.public_key_base64());
        // SkipForTests：仅 e2e 可达（doc(hidden)），记日志后放行
        let Some(check) = self.hk_check.clone() else {
            applog::emit(
                log::Level::Debug,
                "ssh",
                ev::SSH_HOSTKEY_FINGERPRINT,
                Some(&self.sid),
                "主机密钥指纹（校验已跳过）",
                Some(serde_json::json!({ "fingerprint": fingerprint })),
            );
            return Ok(true);
        };

        // 决策表评估（§6.4）并写入共享槽，供连接失败时错误映射
        let (pass, kind) = evaluate_host_key(
            check.stored.as_ref(),
            check.trust,
            check.trust_fp.as_deref(),
            &fingerprint,
        );
        *self.hk.lock().unwrap() = Some(HostKeyVerdict {
            kind,
            algo: algo.clone(),
            fingerprint: fingerprint.clone(),
            pubkey: pubkey.clone(),
        });
        // 指纹/算法属公开信息可记录（设计 §7）；密码/PTY 字节流永不入日志
        applog::emit(
            log::Level::Debug,
            "ssh",
            ev::SSH_HOSTKEY_FINGERPRINT,
            Some(&self.sid),
            "主机密钥校验",
            Some(serde_json::json!({
                "algo": algo,
                "fingerprint": fingerprint,
                "verdict": format!("{kind:?}"),
            })),
        );

        if !pass {
            // 拒绝：russh 随即中断握手，错误映射在 connect_and_auth 读槽完成
            return Ok(false);
        }
        if kind == VerdictKind::Trusted && check.persist {
            // TOFU 落盘：「仅本次连接」（persist=false）跳过——内存信任不跨会话，
            // 下次新建会话重新首连确认。与握手完成同步收敛（同步小文件 I/O，
            // 几 KB 可接受），禁止 tokio::spawn 异步写（设计 §6.2：写未完成时断开
            // 重连会读到旧文件，产生「连接成功但马上重弹确认」竞态）。
            // upsert by host+port 覆盖旧条目。
            // 写失败绝不断开已建立的会话（§7 行为契约 1）：本次连接继续可信，
            // 下次连接重新确认。
            let entry = crate::known_hosts::KnownHostEntry {
                host: check.host,
                port: check.port,
                algo: algo.clone(),
                fingerprint: fingerprint.clone(),
                added_at: crate::known_hosts::now_ms(),
            };
            match crate::known_hosts::record(&check.path, entry) {
                Ok(()) => applog::emit(
                    log::Level::Info,
                    "ssh",
                    ev::SSH_HOSTKEY_FINGERPRINT,
                    Some(&self.sid),
                    format!("主机密钥已信任并保存（{algo} {fingerprint}）"),
                    None,
                ),
                Err(e) => applog::emit(
                    log::Level::Error,
                    "ssh",
                    ev::SSH_HOSTKEY_RECORD_FAILED,
                    Some(&self.sid),
                    format!("无法保存主机密钥，下次连接将重新确认指纹: {e}"),
                    Some(serde_json::json!({ "path": check.path.display().to_string() })),
                ),
            }
        }
        Ok(true)
    }

    /// KEX 完成：记录本次真实协商出的主机密钥算法与加密算法。
    /// 在返回的 Future 被 poll 前同步写入（不把 `&mut self` 带入 async 块，
    /// 规避生命周期问题）；此时认证尚未开始，连接主流程读到时值已就绪。
    fn kex_done(
        &mut self,
        _shared_secret: Option<&[u8]>,
        names: &russh::Names,
        _session: &mut client::Session,
    ) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        *self.algo.lock().unwrap() = Some(NegotiatedAlgo {
            host_key: names.key.to_string(),
            cipher: names.cipher.as_ref().to_string(),
        });
        async { Ok(()) }
    }

    /// 远程转发（-R）接入回调：远端 sshd 有新连接进入本地注册的监听端口时调用。
    /// 流程（tunnel-design.md §6.5）：查表 → 并发闸门 → 连本地目标 → accept →
    /// spawn 双向 copy。同步段先查表/clone 出 owned 数据，async 块不持
    /// `&mut self` 跨 await（沿用 `kex_done` 既有手法）。
    /// 未命中/闸门已满/本地目标连接失败一律 reject，绝不影响主连接与其他规则。
    fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        // 同步段：锁内 clone 出 binding（内含 Arc，clone 廉价）后立即放锁
        let binding = self.remote.lookup(connected_address, connected_port as u16);
        let global_conns = self.global_conns.clone();
        let bound = format!("{connected_address}:{connected_port}");
        async move {
            let Some(binding) = binding else {
                // 规则已停止或从未注册：拒绝接入（cancel-tcpip-forward 存在竞态窗口：
                // 撤监听与回调投递并发，属正常时序，debug 即可）
                debug!("远程转发 {bound} 回调未命中（规则已停止），拒绝接入");
                reply
                    .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                    .await;
                return Ok(());
            };
            // 并发闸门（与本地 accept 同一套防线）：try 语义失败立即拒绝，绝不排队
            let permits = match (
                binding.entry.rule_conns.clone().try_acquire_owned(),
                global_conns.clone().try_acquire_owned(),
            ) {
                (Ok(a), Ok(b)) => (a, b),
                (a, b) => {
                    drop((a, b));
                    debug!("远程转发 {bound} 并发闸门已满，拒绝接入");
                    reply
                        .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                        .await;
                    return Ok(());
                }
            };
            // 先连本地目标再 accept：REMOTE_CONNECT_TIMEOUT 内连不上即拒绝，
            // 不为注定失败的转发占用远端 channel
            let target = (binding.target.host.as_str(), binding.target.port);
            let tcp = match tokio::time::timeout(
                REMOTE_CONNECT_TIMEOUT,
                tokio::net::TcpStream::connect(target),
            )
            .await
            {
                Ok(Ok(tcp)) => tcp,
                Ok(Err(e)) => {
                    debug!(
                        "远程转发 {bound} 连本地目标 {}:{} 失败: {e}",
                        binding.target.host, binding.target.port
                    );
                    reply.reject(russh::ChannelOpenFailure::ConnectFailed).await;
                    return Ok(());
                }
                Err(_) => {
                    debug!("远程转发 {bound} 连本地目标超时，拒绝接入");
                    reply.reject(russh::ChannelOpenFailure::ConnectFailed).await;
                    return Ok(());
                }
            };
            reply.accept().await;
            // 双向 copy：活动计数/流量统计/许可收尾全在 run_relay_conn 内 RAII 管理；
            // owned channel 可安全 move 进独立任务（russh 0.63.3 已验证）
            tokio::spawn(run_relay_conn(channel, tcp, binding.entry, permits));
            Ok(())
        }
    }
}

/// 打开 session 通道并申请 PTY（不启动 shell：调用方需要先把 MOTD 首帧排好序）。
/// 失败统一记 `ssh.session.failed`（stage=channel/pty），成功产出 channel_open + pty 事件。
async fn open_pty(
    handle: &mut client::Handle<ClientHandler>,
    cfg: &SessionConfig,
    sid: &str,
) -> Result<russh::Channel<client::Msg>, SshError> {
    let channel = handle.channel_open_session().await.map_err(|e| {
        applog::emit(
            log::Level::Error,
            "ssh",
            ev::SSH_SESSION_FAILED,
            Some(sid),
            "会话创建失败",
            Some(serde_json::json!({ "stage": "channel", "err": e.to_string() })),
        );
        SshError::Channel(e.to_string())
    })?;
    applog::emit(
        log::Level::Debug,
        "ssh",
        ev::SSH_SESSION_CHANNEL_OPEN,
        Some(sid),
        "会话通道已打开",
        Some(serde_json::json!({ "channel_id": format!("{:?}", channel.id()) })),
    );
    channel
        .request_pty(true, TERM_TYPE, cfg.cols.max(1), cfg.rows.max(1), 0, 0, &[])
        .await
        .map_err(|e| {
            applog::emit(
                log::Level::Error,
                "ssh",
                ev::SSH_SESSION_FAILED,
                Some(sid),
                "会话创建失败",
                Some(serde_json::json!({ "stage": "pty", "err": e.to_string() })),
            );
            SshError::Channel(e.to_string())
        })?;
    applog::emit(
        log::Level::Debug,
        "ssh",
        ev::SSH_SESSION_PTY,
        Some(sid),
        "PTY 已分配",
        Some(serde_json::json!({
            "term": TERM_TYPE,
            "cols": cfg.cols,
            "rows": cfg.rows,
        })),
    );
    Ok(channel)
}

/// 构造以登录 shell 替换自身的启动命令；远端 UTF-8 locale 缺失/失效时
/// （容器内常见两种：无 LANG 等同 C locale；或 LANG 指向未生成的 locale，
/// 如 `zh_CN.UTF-8` 但未 locale-gen——两种情况下 readline 都把粘贴的
/// UTF-8 字节转义成 `\nnn` 八进制文本，vim 默认 encoding=latin1 把
/// UTF-8 中文渲染成 `~x` 乱码）保底注入 `LC_ALL=<实测可用的 UTF-8 locale>`。
///
/// 为什么写在命令串而非启动后脚本：readline 仅在初始化时读取一次 locale
/// （环境变量 LC_ALL/LC_CTYPE/LANG）并缓存；变量赋值前缀 + `exec`（POSIX
/// 特殊内建）会把 LC_ALL 带进替换后进程的环境，新 shell 启动时
/// `setlocale(LC_ALL, "")` 即读到 UTF-8。
///
/// 具体探测 / 候选顺序 / busybox 兜底见 [`UTF8_LOCALE_PICK_SH`]（与 init
/// 脚本同一份逻辑）。要点：判据是 `locale charmap` **实测**而非 LANG 字符串；
/// 已是 UTF-8 时零改动（尊重用户 LANG/LC_* 偏好）；用 LC_ALL 而非 LC_CTYPE
/// （init 脚本随后可能 export 指向未生成 locale 的 LANG，实测该无效 LANG
/// 会令 vim 的 setlocale(LC_ALL,"") 整体失败回退 latin1，LC_CTYPE 压不住，
/// 只有最高优先级的 LC_ALL 能让 vim encoding=utf-8）；候选含 en_US.UTF-8
/// 以覆盖无 C.UTF-8 的 CentOS/RHEL/Amazon Linux 旧版。
fn locale_login_cmd(shell: &str) -> String {
    let q = shell.replace('\'', "'\\''");
    // 复用 init 脚本同一套候选探测（UTF8_LOCALE_PICK_SH）：命中则带
    // LC_ALL=<实测可用名> exec；`_rh_l` 为空（已是 UTF-8，或无可用候选的
    // 极瘦镜像）则裸 exec，保持与改动前一致、绝不塞无效 locale。
    //
    // shell 路径先经 `[ -x ]` 校验再 exec：探测值来自远端 passwd，可能被
    // shell 启动告警污染或指向容器内不存在的解释器，直接 exec 会得到 127
    // 且会话在数十毫秒内退出（用户侧表现为「连上就断」）。回退顺序：
    // 探测值 → $SHELL → 常见 shell → 裸 request_shell 语义交给 sshd。
    format!(
        "\
_rh_s='{q}'
if [ ! -x \"$_rh_s\" ]; then
  _rh_s=\"${{SHELL:-}}\"
fi
if [ ! -x \"$_rh_s\" ]; then
  for _rh_sc in /bin/bash /bin/sh /usr/bin/bash /usr/bin/sh /bin/zsh; do
    if [ -x \"$_rh_sc\" ]; then _rh_s=\"$_rh_sc\"; break; fi
  done
fi
{pick}
if [ -n \"$_rh_l\" ]; then
  LC_ALL=\"$_rh_l\" exec \"$_rh_s\" -l
else
  exec \"$_rh_s\" -l
fi",
        pick = UTF8_LOCALE_PICK_SH,
        q = q,
    )
}

/// 打开 session 通道、申请 PTY 并启动交互式 shell（含抑制原生横幅逻辑）。
///
/// - `suppress=false`：标准 `request_shell`，sshd 打印原生 MOTD / Last login；
/// - `suppress=true`：以 exec 执行 `exec <login shell> -l`。sshd 的横幅
///   （do_motd / lastlog）只在 shell 请求路径输出，exec 路径天然不打印。
///   注意用「shell 启动参数 -l」而非 `exec -l`：后者是 shell 内建选项，
///   busybox ash 不支持（会报 illegal option 并退出）；而 `-l` 作为启动参数
///   bash/zsh/dash/ash/fish/tcsh 全部支持，且 stdin 是 PTY 故交互语义不变，
///   仍会加载 /etc/profile 等登录配置；外层 `exec` 是 POSIX 通用内建，
///   保证中间 shell 自我替换、不额外残留一层进程。
/// - exec 请求被拒（极少）或未探测到 shell 路径时，重开 PTY 回退标准 shell 请求
///   （同一 session 通道在一次会话子请求失败后不能再发 shell 请求）。
///
/// 日志：env/shell 子步骤成功各产出一个 DEBUG 事件；最终失败记
/// `ssh.session.failed`（stage=shell）。`init_injected` 由调用方提供
/// （CWD hook 脚本是否已落盘，决定后续 SFTP 目录同步是否可用）。
async fn open_interactive(
    handle: &mut client::Handle<ClientHandler>,
    cfg: &SessionConfig,
    suppress: bool,
    login_shell: Option<String>,
    sid: &str,
    init_injected: bool,
) -> Result<russh::Channel<client::Msg>, SshError> {
    let mut channel = open_pty(handle, cfg, sid).await?;

    if suppress {
        if let Some(shell) = login_shell {
            // locale 保底必须在 shell 进程启动前注入（见 locale_login_cmd）：
            // readline 初始化时一次性读取并缓存 locale，shell 启动后再 export
            // 无法改变当前行编辑对非 ASCII 字节的处理。
            let cmd = locale_login_cmd(&shell);
            // locale 注入内联在 exec 命令串中，先于 shell 启动事件记录
            applog::emit(
                log::Level::Debug,
                "ssh",
                ev::SSH_SESSION_ENV,
                Some(sid),
                "环境变量已设置",
                Some(serde_json::json!({
                    "locale": "C.UTF-8",
                    "fallback": true,
                    "via": "exec_cmd",
                })),
            );
            match channel.exec(true, cmd).await {
                Ok(()) => {
                    debug!("已以 exec(login shell -l) 启动，抑制服务端原生 MOTD: {shell}");
                    applog::emit(
                        log::Level::Info,
                        "ssh",
                        ev::SSH_SESSION_SHELL,
                        Some(sid),
                        "shell 已启动",
                        Some(serde_json::json!({
                            "shell": shell,
                            "via": "exec_login",
                            "init_injected": init_injected,
                        })),
                    );
                    return Ok(channel);
                }
                Err(e) => {
                    warn!("exec 登录 shell 被拒，重开 PTY 回退标准 shell: {e}");
                    applog::emit(
                        log::Level::Warn,
                        "ssh",
                        ev::SSH_SESSION_SHELL,
                        Some(sid),
                        "exec 登录 shell 被拒，回退标准 shell 请求",
                        Some(serde_json::json!({ "via": "exec_rejected", "err": e.to_string() })),
                    );
                }
            }
        } else {
            debug!("未探测到登录 shell 路径，抑制原生 MOTD 回退为标准 shell 请求");
            applog::emit(
                log::Level::Warn,
                "ssh",
                ev::SSH_SESSION_SHELL,
                Some(sid),
                "未探测到登录 shell，回退标准 shell 请求",
                Some(serde_json::json!({ "via": "shell_undetected" })),
            );
        }
        // 回退路径：丢弃当前通道，重新申请 PTY + 标准 shell
        let _ = channel.close().await;
        channel = open_pty(handle, cfg, sid).await?;
    }

    // 标准 shell 请求路径：尽力在启动前注入 UTF-8 字符集（接受与否取决于
    // sshd 的 AcceptEnv；want_reply=false 不等待回复、不会悬挂）。exec
    // 抑制路径已在命令串中解决，不会走到这里。被拒不影响后续 shell 请求。
    //
    // 这里用 LC_CTYPE 而非 LC_ALL：此为 shell 启动前的「盲发」（无法先探测
    // 远端 locale），LC_ALL 会无条件覆盖有效系统上用户 LANG 的界面语言/地区
    // 格式；此刻前端 env 的 LANG 尚未注入，LC_CTYPE 足以覆盖 shell/readline
    // 的早期窗口。针对「无效 LANG 拖垮 vim」的决定性兜底在 init 脚本里
    // （有条件、实测后用 LC_ALL），所有路径 shell 启动后都会 source。
    let _ = channel.set_env(false, "LC_CTYPE", "C.UTF-8").await;
    applog::emit(
        log::Level::Debug,
        "ssh",
        ev::SSH_SESSION_ENV,
        Some(sid),
        "环境变量已设置",
        Some(serde_json::json!({
            "locale": "C.UTF-8",
            "fallback": true,
            "via": "set_env",
        })),
    );

    channel.request_shell(true).await.map_err(|e| {
        applog::emit(
            log::Level::Error,
            "ssh",
            ev::SSH_SESSION_FAILED,
            Some(sid),
            "会话创建失败",
            Some(serde_json::json!({ "stage": "shell", "err": e.to_string() })),
        );
        SshError::Channel(e.to_string())
    })?;
    applog::emit(
        log::Level::Info,
        "ssh",
        ev::SSH_SESSION_SHELL,
        Some(sid),
        "shell 已启动",
        Some(serde_json::json!({
            // 标准路径由 sshd 决定登录 shell，本地未探测
            "shell": "default",
            "via": "request_shell",
            "init_injected": init_injected,
        })),
    );
    Ok(channel)
}

/// PTY 事件：数据与结束信号走同一通道，保证帧顺序
enum PtyEvent {
    /// PTY 原始输出字节（stdout 与 stderr 合并）
    Data(Vec<u8>),
    /// 会话结束。
    /// - `reason`：可选展示文本（退出状态码/信号描述）；
    /// - `lost=true`：未收到远端退出状态连接即关闭（网络中断/sshd 断连），
    ///   前端据此触发断线自动重连；
    /// - `lost=false`：远端进程正常退出或被信号终止（用户主动 exit 等），不重连。
    Eof { reason: Option<String>, lost: bool },
}

/// 前端 → 写任务的请求：输入数据与窗口尺寸复用同一通道，保持单写者
enum WriteReq {
    /// 键盘原始字节
    Data(Vec<u8>),
    /// PTY 尺寸变化（SSH window-change）
    Resize { cols: u32, rows: u32 },
}

/// 容器交互终端（`docker exec -it`）的写入口。
///
/// 独立于主终端 PTY：输入/尺寸变更各自排队，输出经独立帧通道交给调用方
/// 转发到前端专用 Channel；关闭时只取消本终端的子令牌，不影响会话本身。
pub struct DockerTerm {
    write_tx: mpsc::Sender<WriteReq>,
    cancel: CancellationToken,
}

impl DockerTerm {
    pub async fn write(&self, data: Vec<u8>) -> Result<(), SshError> {
        self.write_tx
            .send(WriteReq::Data(data))
            .await
            .map_err(|_| SshError::Closed)
    }

    pub async fn resize(&self, cols: u32, rows: u32) -> Result<(), SshError> {
        if cols == 0 || rows == 0 {
            return Ok(());
        }
        self.write_tx
            .send(WriteReq::Resize { cols, rows })
            .await
            .map_err(|_| SshError::Closed)
    }

    pub fn close(&self) {
        self.cancel.cancel();
    }
}

/// 会话断开日志状态：read_task（异常断开）与 shutdown（主动关闭）竞争，
/// 原子 swap 保证仅首次产出 `ssh.disconnect` 事件。
struct DisconnectState {
    sid: String,
    connected_at: Instant,
    /// 已记录 disconnect（true=已产日志，不再重复）
    logged: AtomicBool,
}

/// 一条存活中的 SSH PTY 会话（由 SessionManager 持有）
pub struct SshSession {
    /// 取消令牌：关闭/异常时取消全部后台任务，防止任务泄漏
    cancel: CancellationToken,
    /// 前端请求入口：键盘输入与窗口尺寸变更
    write_tx: mpsc::Sender<WriteReq>,
    /// 已认证 SSH handle（russh Handle 内部为 Arc，clone 廉价）。
    /// 供 PTY 之外的独立 exec 子通道做主机指标采集（0x05/0x06 帧），
    /// 与终端通道物理隔离、互不影响；Mutex 串行化同一会话的 exec 通道操作。
    /// 同时供 sftp 模块懒加载 SFTP 子系统通道。
    pub(super) handle: Arc<Mutex<client::Handle<ClientHandler>>>,
    /// 帧发送口的 clone：动态指标（0x06）与 PTY 帧共用同一条 FIFO 队列，
    /// 经 ipc 层同一个 Channel 转发前端
    frame_tx: mpsc::Sender<Vec<u8>>,
    /// 动态指标采集任务句柄（启动后存在）。同步锁仅做瞬时取放，
    /// 不在持锁期间 await；stop 只取消令牌，任务异步退出
    metrics: StdMutex<Option<crate::metrics::collector::CollectorHandle>>,
    /// CPU 快照与上次利用率：跨采集任务保留，切换会话/暂停后重启 collector
    /// 首帧仍能算出真实差值，不会因「没有上一次」而显示 0%
    metrics_cpu: StdMutex<crate::metrics::CpuSampleState>,
    /// SFTP 子系统长驻会话（懒加载；远端文件面板列目录使用）
    pub(super) sftp: SftpState,
    /// Shell 当前工作目录：由 merge_task 解析 PTY 中的 OSC 6667 序列更新，
    /// sftp_list_dir 无参数时以此为默认目录，保持 Shell 与 SFTP CWD 一致。
    pub(super) cwd: Arc<Mutex<Option<String>>>,
    /// SFTP→Shell cd 同步的回显剥离窗口（probe=注入的 cd 行字节，until=到期时刻）。
    /// 写侧 sync_shell_cd 武装，读侧 merge_task/flush 消费（见 [`cd_gate`]）。
    /// 用 Vec 而非单槽：连续快速切目录时多条 cd 的回显都要剥。
    pub(super) cd_strip: Arc<Mutex<Vec<(Vec<u8>, Instant)>>>,
    /// 端口转发管理器：与 ClientHandler 共享 RemoteRegistry / 全局闸门，
    /// 与 SFTP 子系统、metrics 采集器平级，共用同一条已认证主连接
    tunnel: TunnelManager,
    /// 断开日志状态：共享给 read_task，防止主动关闭与异常断开重复记录
    dc_state: Arc<DisconnectState>,
    /// 初始化脚本远端路径（/tmp/.ri-<uuid8>）：shutdown 时主动清理，
    /// 避免会话中断导致残留
    init_script_path: Option<String>,
    /// 单通道会话（堡垒机外部连接）：指标采集与 AI 只读工具的 exec 通道一律拒绝，
    /// 这些主机在同一条连接上开第二条通道就会断开整个会话
    single_channel: bool,
    /// `sftp://` 外部连接：本会话没有 PTY，那条唯一通道就是 SFTP 子系统。
    /// 据此放行 SFTP（否则会被 single_channel 误拦）、并让 PTY 相关调用静默空转
    sftp_only: bool,
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

/// 仅建立 TCP+SSH 握手并完成认证（密码或公钥），不申请 PTY/shell。
///
/// 由「正式连接」与「测试连接」共用，保证两者认证行为完全一致：
/// 测试通过 = 正式连接的认证阶段也必然通过。成功返回已认证的 handle，
/// 调用方负责断开；测试连接用完即断，不注册会话池。
///
/// 日志按 §4.2 分阶段产出：connect.start → connect.tcp → handshake.start →
/// handshake.complete → auth.start → auth.success；任一阶段失败产出对应
/// ERROR 事件后返回，不再产出后续阶段事件。
pub(crate) async fn connect_and_auth(
    cfg: &SessionConfig,
    algo: Arc<StdMutex<Option<NegotiatedAlgo>>>,
    sid: &str,
    // -R 回调所需的共享件：正式连接传入与 TunnelManager 共享的实例；
    // 测试连接无转发，传入默认值即可（registry 为空表，闸门不消费）
    remote: RemoteRegistry,
    global_conns: Arc<Semaphore>,
) -> Result<client::Handle<ClientHandler>, SshError> {
    let auth_method = match &cfg.auth {
        AuthMethod::Password(_) => "password",
        AuthMethod::Key { .. } => "publickey",
    };
    applog::emit(
        log::Level::Info,
        "ssh",
        ev::SSH_CONNECT_START,
        Some(sid),
        "正在连接",
        Some(serde_json::json!({
            "host": cfg.host,
            "port": cfg.port,
            "user": cfg.username,
            "auth": auth_method,
        })),
    );

    // ---- TCP 阶段：自建 TcpStream 再交 connect_stream，与握手阶段精确区分 ----
    let tcp_start = Instant::now();
    let socket = match tokio::net::TcpStream::connect((cfg.host.as_str(), cfg.port)).await {
        Ok(s) => {
            applog::emit(
                log::Level::Debug,
                "ssh",
                ev::SSH_CONNECT_TCP,
                Some(sid),
                "TCP 已连接",
                Some(serde_json::json!({
                    "rtt_ms": tcp_start.elapsed().as_millis() as u64,
                })),
            );
            s
        }
        Err(e) => {
            applog::emit(
                log::Level::Error,
                "ssh",
                ev::SSH_CONNECT_TCP_FAILED,
                Some(sid),
                "TCP 连接失败",
                Some(serde_json::json!({
                    "host": cfg.host,
                    "port": cfg.port,
                    "err": e.to_string(),
                })),
            );
            return Err(SshError::Connect(e.to_string()));
        }
    };
    let _ = socket.set_nodelay(true);

    // ---- SSH 握手阶段 ----
    applog::emit(
        log::Level::Debug,
        "ssh",
        ev::SSH_HANDSHAKE_START,
        Some(sid),
        "SSH 握手开始",
        None,
    );
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(300)),
        keepalive_interval: cfg.keepalive,
        nodelay: true,
        ..Default::default()
    });
    // 主机密钥校验：verdict 共享槽在连接主流程创建（复用 algo 槽手法），
    // `check_server_key` 写入、下方握手失败分支读出做错误映射（§6.4）
    let hk = Arc::new(StdMutex::new(None));
    let hk_check = match &cfg.host_key {
        super::HostKeyPolicy::Verify(check) => Some(check.clone()),
        super::HostKeyPolicy::SkipForTests => None,
    };
    let mut handle = match client::connect_stream(
        config,
        socket,
        ClientHandler {
            algo: algo.clone(),
            hk: hk.clone(),
            hk_check,
            sid: sid.to_string(),
            remote,
            global_conns,
        },
    )
    .await
    {
        Ok(h) => {
            // kex_done 已在握手完成时写入协商算法
            let negotiated = algo.lock().unwrap().clone().unwrap_or_default();
            applog::emit(
                log::Level::Info,
                "ssh",
                ev::SSH_HANDSHAKE_COMPLETE,
                Some(sid),
                "SSH 握手完成",
                Some(serde_json::json!({
                    "host_key": negotiated.host_key,
                    "cipher": negotiated.cipher,
                })),
            );
            h
        }
        Err(e) => {
            applog::emit(
                log::Level::Error,
                "ssh",
                ev::SSH_HANDSHAKE_FAILED,
                Some(sid),
                "SSH 握手失败",
                Some(serde_json::json!({ "err": e.to_string() })),
            );
            // 主机密钥拒绝（check_server_key 返回 false 由 russh 中断握手）：
            // 按 verdict 槽细分映射为协议前缀错误，否则维持普通连接失败（§6.4）
            let verdict = hk.lock().unwrap().clone();
            if let Some(v) =
                verdict.filter(|v| !matches!(v.kind, VerdictKind::Trusted | VerdictKind::Match))
            {
                // payload 三段：`algo|fingerprint|pubkey`（pubkey 不含 `|`，可按 `|` 切）
                let payload = format!("{}|{}|{}", v.algo, v.fingerprint, v.pubkey);
                applog::emit(
                    log::Level::Warn,
                    "ssh",
                    ev::SSH_HOSTKEY_FINGERPRINT,
                    Some(sid),
                    format!("主机密钥校验未通过（{:?}）", v.kind),
                    Some(
                        serde_json::json!({ "algo": v.algo, "fingerprint": v.fingerprint, "pubkey": v.pubkey }),
                    ),
                );
                return Err(match v.kind {
                    VerdictKind::Mismatch => SshError::HostKeyMismatch(payload),
                    _ => SshError::HostKeyUnknown(payload),
                });
            }
            return Err(SshError::Connect(e.to_string()));
        }
    };

    // ---- 认证阶段 ----
    applog::emit(
        log::Level::Debug,
        "ssh",
        ev::SSH_AUTH_START,
        Some(sid),
        "开始认证",
        Some(serde_json::json!({
            "method": auth_method,
            "user": cfg.username,
        })),
    );
    let auth = match &cfg.auth {
        AuthMethod::Password(password) => handle
            .authenticate_password(cfg.username.clone(), password.clone())
            .await
            .map_err(|e| {
                applog::emit(
                    log::Level::Error,
                    "ssh",
                    ev::SSH_AUTH_FAILED,
                    Some(sid),
                    "认证失败",
                    Some(serde_json::json!({
                        "method": auth_method,
                        "reason": e.to_string(),
                    })),
                );
                SshError::Auth(e.to_string())
            })?,
        AuthMethod::Key { path, passphrase } => {
            let key_path = expand_tilde(path);
            let key = russh::keys::load_secret_key(
                &key_path,
                if passphrase.is_empty() {
                    None
                } else {
                    Some(passphrase.as_str())
                },
            )
            .map_err(|e| {
                let (reason, err) = match e {
                    // 无口令加载加密私钥 / 口令错误都报 KeyIsEncrypted，
                    // 统一转成约定标记，前端弹口令框后带口令重试
                    russh::keys::Error::KeyIsEncrypted => {
                        ("私钥已加密，需要口令".to_string(), SshError::KeyEncrypted)
                    }
                    other => (
                        format!("私钥加载失败（{key_path}）: {other}"),
                        SshError::Auth(format!("私钥加载失败（{key_path}）: {other}")),
                    ),
                };
                applog::emit(
                    log::Level::Error,
                    "ssh",
                    ev::SSH_AUTH_FAILED,
                    Some(sid),
                    "认证失败",
                    Some(serde_json::json!({
                        "method": auth_method,
                        "reason": reason,
                    })),
                );
                err
            })?;
            handle
                .authenticate_publickey(
                    cfg.username.clone(),
                    russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), None),
                )
                .await
                .map_err(|e| {
                    applog::emit(
                        log::Level::Error,
                        "ssh",
                        ev::SSH_AUTH_FAILED,
                        Some(sid),
                        "认证失败",
                        Some(serde_json::json!({
                            "method": auth_method,
                            "reason": e.to_string(),
                        })),
                    );
                    SshError::Auth(e.to_string())
                })?
        }
    };
    if !auth.success() {
        let _ = handle
            .disconnect(Disconnect::ByApplication, "auth failed", "")
            .await;
        let reason = match &cfg.auth {
            AuthMethod::Password(_) => "用户名或密码错误",
            AuthMethod::Key { .. } => "服务器拒绝了该密钥",
        };
        applog::emit(
            log::Level::Error,
            "ssh",
            ev::SSH_AUTH_FAILED,
            Some(sid),
            "认证失败",
            Some(serde_json::json!({
                "method": auth_method,
                "reason": reason,
            })),
        );
        return Err(SshError::Auth(reason.into()));
    }
    applog::emit(
        log::Level::Info,
        "ssh",
        ev::SSH_AUTH_SUCCESS,
        Some(sid),
        "认证成功",
        Some(serde_json::json!({
            "method": auth_method,
            "user": cfg.username,
        })),
    );
    Ok(handle)
}

/// 全局同时在飞的指标 exec 通道上限（进程级保险丝）。
/// 正常仅活动会话采集（前端门控），远触不到该上限；批量会话齐启动/重连风暴等
/// 极端情况下削峰，避免瞬时打开大量 SSH channel 耗尽本机 fd 或撞远端 sshd MaxSessions。
const MAX_CONCURRENT_METRIC_EXEC: usize = 24;
static METRIC_EXEC_GATE: Semaphore = Semaphore::const_new(MAX_CONCURRENT_METRIC_EXEC);

/// 当前在飞的指标 exec 数（诊断/压测观测；宽松序足够）
static METRIC_EXEC_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
/// 进程生命周期内在飞 exec 的历史峰值（压测验收「启动峰值 ≤ 24」实测读数）
static METRIC_EXEC_PEAK: AtomicUsize = AtomicUsize::new(0);

/// 指标 exec 并发观测值：`(当前在飞, 历史峰值)`。仅用于诊断与压测断言。
pub fn metric_exec_concurrency() -> (usize, usize) {
    (
        METRIC_EXEC_INFLIGHT.load(Ordering::Relaxed),
        METRIC_EXEC_PEAK.load(Ordering::Relaxed),
    )
}

/// exec channel 执行结果（stdout/stderr 合并文本 + 退出码 + 超时标记）。
///
/// 退出码语义：`Some(0)` 成功；`Some(n>0)` 命令业务失败（文件不存在/权限拒绝等，
/// 错误信息在 `output` 中）；`None` 通道未给出 ExitStatus（含超时）。
/// `timed_out=true` 时 `output` 为已收到的部分片段（可能非空）。
pub struct ExecOutput {
    /// stdout + stderr 合并文本（超时时为部分片段）
    pub output: String,
    /// 远端命令退出码
    pub exit_code: Option<i32>,
    /// true 表示执行超时，output 为部分输出
    pub timed_out: bool,
}

/// 在飞计数 RAII 守卫：无论 exec 成功/失败/提前返回，离开作用域即递减。
struct InflightGuard;
impl InflightGuard {
    fn enter() -> Self {
        let n = METRIC_EXEC_INFLIGHT.fetch_add(1, Ordering::Relaxed) + 1;
        METRIC_EXEC_PEAK.fetch_max(n, Ordering::Relaxed);
        Self
    }
}
impl Drop for InflightGuard {
    fn drop(&mut self) {
        METRIC_EXEC_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

impl SshSession {
    /// 建立连接并完成密码认证；严格按「认证 → exec 采集 → MOTD 首帧 → 开启 PTY」
    /// 的顺序编排，保证 MOTD 状态面板始终先于远端 PS1 提示符出现。
    ///
    /// 返回 (会话句柄, 帧输出接收端)；帧接收端交给 ipc 层转发给前端 Channel。
    /// 初始化脚本（始终含 CWD 钩子，cfg.color_prompt 开启时含彩色提示符）已在
    /// PTY 开启后经写队列自动注入，merge_task 以 marker 为信号 hold 初始化输出，
    /// 前端无需参与时序编排。
    /// 后台任务（读/合并/写）均挂在 `cancel` 上，`shutdown()` 即全部取消。
    ///
    /// `sid` 为会话标签（uuid 前 6 位），贯穿连接全生命周期日志事件。
    pub async fn connect(
        cfg: SessionConfig,
        sid: &str,
    ) -> Result<(Self, mpsc::Receiver<Vec<u8>>), SshError> {
        let connect_start = Instant::now();
        let cancel = CancellationToken::new();

        // 1. TCP + SSH 握手 + 认证（与测试连接共用，保证认证行为一致）。
        //    russh Handle 不支持 Clone，认证后即包进 Arc<Mutex>：
        //    连接阶段（MOTD/落盘/开 PTY）与连接后的指标 exec 通道共用同一个 handle，
        //    Mutex 串行化通道操作，Arc 保证 PTY 后台任务存活期间连接不被释放。
        //    algo 是 KEX 协商算法的出口（kex_done 回调已在认证前填入）。
        //    remote/global_conns 先创建、后与 TunnelManager 共享：
        //    ClientHandler 的 -R 回调与转发引擎走同一张注册表、同一套闸门。
        let algo = Arc::new(StdMutex::new(None));
        let remote = RemoteRegistry::default();
        let global_conns = Arc::new(Semaphore::new(super::tunnel::MAX_CONN_GLOBAL));
        // 连接建立总超时：TCP 建连 + 握手 + 认证全程共享一个 deadline
        // （规避不可达主机让 DNS/TCP 把用户晾住数分钟）；`None` = 不限制。
        let handle = Arc::new(Mutex::new(
            match cfg.connect_timeout {
                Some(d) => tokio::time::timeout(
                    d,
                    connect_and_auth(&cfg, algo.clone(), sid, remote.clone(), global_conns.clone()),
                )
                .await
                .map_err(|_| {
                    applog::emit(
                        log::Level::Error,
                        "ssh",
                        ev::SSH_CONNECT_TCP_FAILED,
                        Some(sid),
                        "连接超时",
                        Some(serde_json::json!({
                            "host": cfg.host,
                            "port": cfg.port,
                            "timeout_secs": d.as_secs(),
                        })),
                    );
                    SshError::Connect(format!(
                        "连接超时（{} 秒内未完成 TCP 建连与认证）",
                        d.as_secs()
                    ))
                })??,
                None => connect_and_auth(&cfg, algo.clone(), sid, remote.clone(), global_conns.clone()).await?,
            },
        ));

        // 2.【阶段 A/B】PTY 尚未打开：独立 exec 子通道并行采集服务器状态，
        //    本地组装 MOTD 渲染指令数组（不碰 PTY、不经键盘、不解析 shell 输出）。
        //    3s 超时/失败/取消均静默降级为 None，绝不阻塞主连接。
        //    单通道会话跳过：这条 exec 通道正是堡垒机会掐断的第二条通道。
        let motd_outcome = if cfg.motd && !cfg.single_channel {
            let mut h = handle.lock().await;
            crate::motd::collect_and_build(&mut h, &cancel, &cfg.motd_logo).await
        } else {
            None
        };

        // 初始化脚本（CWD 钩子 + 可选彩色提示符 + UTF-8 locale 保底）在 PTY
        // 开启前经 exec 通道静默落盘，终端无感知。CWD 钩子始终注入以保证
        // Shell→SFTP 目录同步；locale 保底覆盖 vim 等子进程；彩色部分仅在
        // cfg.color_prompt 开启时生效。落盘失败（/tmp 不可写、exec 受限等）
        // 以 warn 记录后降级——init_cmd 为 None，这些自动配置将缺席。
        // 单通道会话跳过：脚本落盘本身就要开一条 exec 通道。
        let (init_cmd, init_script_path) = if cfg.single_channel {
            (None, None)
        } else {
            let tag = &Uuid::new_v4().simple().to_string()[..8];
            let tmp_path = format!("/tmp/.ri-{tag}");
            let cmd = {
                let mut h = handle.lock().await;
                match try_upload_script(&mut h, &tmp_path, cfg.color_prompt, &cfg.env).await {
                    Ok(()) => Some(source_cmd(&tmp_path)),
                    Err(e) => {
                        applog::emit(
                            log::Level::Warn,
                            "ssh",
                            ev::SSH_SESSION_INIT_SCRIPT_FAILED,
                            Some(sid),
                            format!(
                                "会话初始化脚本上传失败，CWD 同步/提示符/UTF-8 locale 自动配置跳过: {e}"
                            ),
                            None,
                        );
                        None
                    }
                }
            };
            // 记录成功上传的脚本路径，供 shutdown 时主动清理（source 自删仅在
            // 脚本成功执行后生效；会话在 source 前断开会导致残留）
            let path = if cmd.is_some() { Some(tmp_path) } else { None };
            (cmd, path)
        };

        // 3. 建立帧 / 事件 / 输入通道；合并任务先待命（此时没有任何 PTY 事件）
        let (event_tx, event_rx) = mpsc::channel::<PtyEvent>(EVENT_QUEUE);
        let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE);
        let (write_tx, write_rx) = mpsc::channel::<WriteReq>(WRITE_QUEUE);

        // 4.【阶段 C】MOTD 指令数组作为整个二进制通道的**首帧**发出，
        //    紧接着发主机静态信息（0x05）。mpsc FIFO + 此刻尚无 PTY 数据帧，
        //    严格保证帧序：MOTD → HostInfo → 一切 PTY 输出。
        if let Some(outcome) = &motd_outcome {
            let json = serde_json::to_vec(&outcome.cmds).unwrap_or_default();
            if !json.is_empty()
                && frame_tx
                    .send(encode_frame(FrameType::Motd, &json))
                    .await
                    .is_err()
            {
                warn!("帧通道已关闭，放弃 MOTD 首帧");
            }

            // HostInfo：复用同一批采集 raw 解析，不额外开 exec 通道
            let info = crate::metrics::build_host_info(&outcome.raw);
            let json = serde_json::to_vec(&info).unwrap_or_default();
            if frame_tx
                .send(encode_frame(FrameType::HostInfo, &json))
                .await
                .is_err()
            {
                warn!("帧通道已关闭，放弃 HostInfo 帧");
            }
        }

        // Algo：SSH 握手真实协商算法，无条件发送（与 MOTD 开关无关）。
        // kex_done 早在认证前已写入；排在 HostInfo/MOTD 之后、PTY 首字节之前。
        let negotiated = algo.lock().unwrap().clone().unwrap_or_default();
        let algo_json = serde_json::json!({
            "host_key": negotiated.host_key,
            "cipher": negotiated.cipher,
            "term": TERM_TYPE,
            "enc": TERM_ENCODING,
        });
        let algo_bytes = serde_json::to_vec(&algo_json).unwrap_or_default();
        if frame_tx
            .send(encode_frame(FrameType::Algo, &algo_bytes))
            .await
            .is_err()
        {
            warn!("帧通道已关闭，放弃 Algo 帧");
        }

        // 5~7.【阶段 D】交互式 PTY：打开会话通道、启动登录 shell，并挂上读/合并/写
        //    三个后台任务。MOTD 首帧已在帧队列中，PTY 输出无论多快都排在其后。
        //    sftp-only 会话（`sftp://` 链接）整段跳过：不申请 PTY，整条连接只保留
        //    SFTP 子系统这一条通道；write_rx 随分支释放，误发的键盘输入直接失败。
        //
        //    断开日志状态：read_task 的 EOF 分支（异常断开）与 shutdown（主动关闭）
        //    竞争记录 `ssh.disconnect`，原子 swap 保证只记一条
        let dc_state = Arc::new(DisconnectState {
            sid: sid.to_string(),
            connected_at: connect_start,
            logged: AtomicBool::new(false),
        });
        let cwd = Arc::new(Mutex::new(None));
        let cd_strip = Arc::new(Mutex::new(Vec::new()));

        let metrics_frame_tx = if cfg.sftp_only {
            drop(event_tx);
            drop(event_rx);
            drop(write_rx);
            frame_tx
        } else {
            applog::emit(
                log::Level::Debug,
                "ssh",
                ev::SSH_SESSION_CREATE,
                Some(sid),
                "创建会话",
                Some(serde_json::json!({
                    "term": TERM_TYPE,
                    "cols": cfg.cols,
                    "rows": cfg.rows,
                    "enc": TERM_ENCODING,
                })),
            );
            let login_shell = motd_outcome.as_ref().and_then(|o| o.login_shell.clone());
            let channel = {
                let mut h = handle.lock().await;
                // motd 开启即走 exec 路径抑制 sshd 原生横幅，关闭走标准 shell
                let suppress_native = cfg.motd && !cfg.single_channel;
                open_interactive(&mut h, &cfg, suppress_native, login_shell, sid, init_cmd.is_some())
                    .await?
            };

            // 7. 拆分读写半通道，启动双向转发；PTY 输出从此刻起进入帧队列（恒在 MOTD 之后）
            let (read_half, write_half) = channel.split();
            // 留一个帧发送口给动态指标采集器（0x06），与 PTY 帧共用 FIFO 保序
            let metrics_frame_tx = frame_tx.clone();

            // 提示符注入交给 merge_task 的静默门：等 shell 启动输出静默（PS1 刷完、
            // readline 阻塞）后才经写队列注入；注入全过程的 PTY 输出被 hold 至脚本
            // 末尾 marker，放行前按内容剥掉回显行，前端零感知。probe 用临时脚本路径
            // ——它在本次会话内唯一，只可能出现在回显里。
            let init_inject = init_cmd.map(|cmd| {
                let mut bytes = cmd.into_bytes();
                bytes.push(b'\r');
                let probe = init_script_path.clone().unwrap_or_default().into_bytes();
                (bytes, probe, write_tx.clone())
            });

            tokio::spawn(read_task(
                read_half,
                event_tx,
                cancel.clone(),
                dc_state.clone(),
            ));
            tokio::spawn(merge_task(
                event_rx,
                frame_tx,
                cancel.clone(),
                init_inject,
                cwd.clone(),
                cd_strip.clone(),
            ));
            tokio::spawn(write_task(write_half, write_rx, cancel.clone()));
            metrics_frame_tx
        };

        // 7.1 RTT 探测任务（0x07 帧）：独立于 MetricsCollector 运行——
        //     状态栏常显，不能随监控面板隐藏而停更；挂在会话 cancel 上随断连退出。
        //     sftp-only 会话不发：堡垒机对全局请求同样可能拒绝，且没有终端可显示
        if !cfg.sftp_only {
            tokio::spawn(rtt_task(
                handle.clone(),
                metrics_frame_tx.clone(),
                cancel.clone(),
            ));
        }

        // 8. handle（Arc<Mutex>）随 SshSession 保留：供连接建立后的独立 exec
        //    指标采集通道使用（主机指标 0x05/0x06 帧），与 PTY 半通道共同维持连接。
        applog::emit(
            log::Level::Info,
            "ssh",
            ev::SSH_SESSION_READY,
            Some(sid),
            "会话已建立",
            Some(serde_json::json!({
                "term": TERM_TYPE,
                "cols": cfg.cols,
                "rows": cfg.rows,
                "enc": TERM_ENCODING,
                "elapsed_ms": connect_start.elapsed().as_millis() as u64,
            })),
        );

        let tunnel = TunnelManager::new(
            handle.clone(),
            metrics_frame_tx.clone(),
            cancel.clone(),
            remote,
            global_conns,
        );
        // SFTP 子系统状态：空闲看门狗挂在本会话取消令牌上（断连即退出）；
        // 全局限速键含端口——同主机不同端口算不同链路，各自独立限速
        let sftp = SftpState::new(
            sid,
            &format!("{}:{}", cfg.host, cfg.port),
            &cfg.username,
            &cancel,
        );
        Ok((
            Self {
                cancel,
                write_tx,
                handle,
                frame_tx: metrics_frame_tx,
                metrics: StdMutex::new(None),
                metrics_cpu: StdMutex::new(crate::metrics::CpuSampleState::default()),
                sftp,
                cwd,
                cd_strip,
                tunnel,
                dc_state,
                init_script_path,
                single_channel: cfg.single_channel,
                sftp_only: cfg.sftp_only,
            },
            frame_rx,
        ))
    }

    /// 将前端原始输入写入 PTY（异步排队，不阻塞调用方）
    pub async fn write(&self, data: Vec<u8>) -> Result<(), SshError> {
        // sftp:// 外部连接没有 PTY：输入无处可去，静默空转而不是报错
        if self.sftp_only {
            return Ok(());
        }
        self.write_tx
            .send(WriteReq::Data(data))
            .await
            .map_err(|_| SshError::Closed)
    }

    /// 同步 PTY 窗口尺寸：发送 SSH window-change（像素尺寸传 0，由对端忽略）
    pub async fn resize(&self, cols: u32, rows: u32) -> Result<(), SshError> {
        if cols == 0 || rows == 0 {
            // 容器不可见时 fit 可能得出 0，不能把远端 PTY 缩成 0x0
            return Ok(());
        }
        if self.sftp_only {
            return Ok(());
        }
        self.write_tx
            .send(WriteReq::Resize { cols, rows })
            .await
            .map_err(|_| SshError::Closed)
    }

    /// 在独立通道上启动 `docker exec -it` 交互终端，返回写入口与帧接收端。
    ///
    /// 与主终端 PTY 完全隔离：自申请 PTY、自建读写半通道，输出走独立
    /// `mpsc<Vec<u8>>`（帧编码由调用方完成）。子令牌挂在会话取消上，
    /// 会话断开时终端任务随之退出；`close` 只影响本终端。
    pub async fn open_docker_terminal(
        &self,
        command: String,
        cols: u32,
        rows: u32,
    ) -> Result<(DockerTerm, mpsc::Receiver<Vec<u8>>), SshError> {
        if self.single_channel {
            return Err(SshError::Channel(
                "EXTERNAL_SINGLE_CHANNEL: 外部连接不开放额外通道".into(),
            ));
        }
        if self.sftp_only {
            return Err(SshError::Channel("该连接没有终端会话".into()));
        }

        let channel = {
            let handle = self.handle.lock().await;
            let channel = handle
                .channel_open_session()
                .await
                .map_err(|e| SshError::Channel(e.to_string()))?;
            channel
                .request_pty(true, TERM_TYPE, cols.max(1), rows.max(1), 0, 0, &[])
                .await
                .map_err(|e| SshError::Channel(e.to_string()))?;
            channel
                .exec(true, command)
                .await
                .map_err(|e| SshError::Channel(e.to_string()))?;
            channel
        };

        let cancel = self.cancel.child_token();
        let (read_half, write_half) = channel.split();
        let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE);
        let (write_tx, write_rx) = mpsc::channel::<WriteReq>(WRITE_QUEUE);
        tokio::spawn(docker_term_read_task(
            read_half,
            frame_tx,
            cancel.clone(),
        ));
        tokio::spawn(write_task(write_half, write_rx, cancel.clone()));
        Ok((DockerTerm { write_tx, cancel }, frame_rx))
    }

    /// SFTP → Shell 方向同步：把远端交互式 shell 的工作目录切到 `path`。
    ///
    /// 经 PTY 写入通道发送 `cd '<quoted>'\\n`，shell 执行后提示符即反映新目录，
    /// 随后 init_script 注入的 CWD 钩子会上报新路径，形成闭环。路径含单引号
    /// 时按 shell 规则转义（`'` → `'\''`），避免注入。
    ///
    /// 发送前武装回显剥离窗口（[`cd_gate`]）：终端不显示这条 cd，旧提示符行
    /// 被新目录提示符原位替换。窗口按内容匹配，用户手动敲的同内容 cd 在窗口
    /// 期内也会被剥（2s，纯观感、概率极低）；shell 正忙时注入排队、窗口过期
    /// 后回显照显——与未剥离前的行为一致，不会更糟。
    pub(crate) async fn sync_shell_cd(&self, path: &str) -> Result<(), SshError> {
        // 没有 PTY 就没有 shell 可同步（SFTP 面板会忽略本返回值）
        if self.sftp_only {
            return Ok(());
        }
        let quoted = format!("'{}'", path.replace('\'', "'\\''"));
        let cmd = format!("cd {quoted}\n");
        {
            let mut probes = self.cd_strip.lock().await;
            probes.push((format!("cd {quoted}").into_bytes(), Instant::now() + CD_ECHO_TTL));
            // 只保留最近 4 条：更早的回显要么已剥、要么已过期
            let overflow = probes.len().saturating_sub(4);
            if overflow > 0 {
                probes.drain(..overflow);
            }
        }
        self.write_tx
            .send(WriteReq::Data(cmd.into_bytes()))
            .await
            .map_err(|_| SshError::Closed)
    }

    /// 外部连接是否允许打开 SFTP 子系统通道：
    /// 常规会话允许；`sftp://` 链接允许（SFTP 正是它唯一的那条通道）；
    /// `ssh://` 链接的唯一通道是 PTY，再开 SFTP 就是第二条通道，
    /// 不允许多开通道的堡垒机会因此断开整个会话——由 sftp 层拒绝并给出做法
    pub(super) fn sftp_allowed(&self) -> bool {
        !self.single_channel || self.sftp_only
    }

    /// 取当前跟踪的 Shell CWD（由 OSC 6667 钩子上报，merge_task 更新）
    pub(super) async fn get_cwd(&self) -> Option<String> {
        self.cwd.lock().await.clone()
    }

    /// 在独立 exec 子通道执行采集脚本，返回 stdout/stderr 合并文本、退出码与超时标记。
    ///
    /// 与 PTY 交互通道完全隔离：不申请 PTY、不经键盘输入、不解析终端流，
    /// 用于 MOTD 之后的主机指标采集（0x05 静态信息 / 0x06 周期指标），
    /// 以及 AI ReAct 只读工具（read_file / list_directory / get_system_info）。
    ///
    /// - `script_body` 为 `/bin/sh` 命令体（POSIX sh，兼容 fish/tcsh 用户 shell），
    ///   单引号按标准 shell 规则转义，禁止拼接任何会话参数字段；
    /// - 整次执行受 `timeout` 约束；超时采用 deadline-based 循环内超时：
    ///   `channel_open_session`/`ch.exec`/`ch.wait` 均受同一 deadline 约束，
    ///   超时后 break 并返回已收到的部分 output（标记 `timed_out=true`），
    ///   不丢弃已到达的 stdout/stderr 片段；通道随作用域 drop 关闭，
    ///   远端 sh 收 SIGHUP/SIGPIPE 退出，无残留进程；
    /// - 同一会话的 exec 通道由 Mutex 串行（collector 侧另有单飞保证，不会竞争）。
    pub async fn exec_collect(
        &self,
        script_body: &str,
        timeout: Duration,
    ) -> Result<ExecOutput, SshError> {
        self.exec_collect_impl(script_body, None, timeout).await
    }

    /// 同 [`Self::exec_collect`]，但在 exec 后把 `stdin` 写入通道并立即 EOF。
    /// 仅用于把 sudo 密码经管道交给 `sudo -S`：密码绝不拼进命令行，
    /// 也不出现在任何日志字段里。
    pub async fn exec_collect_with_stdin(
        &self,
        script_body: &str,
        stdin: &[u8],
        timeout: Duration,
    ) -> Result<ExecOutput, SshError> {
        self.exec_collect_impl(script_body, Some(stdin), timeout).await
    }

    async fn exec_collect_impl(
        &self,
        script_body: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecOutput, SshError> {
        // 单通道会话（堡垒机外部连接）：只允许业务真正需要的那条通道，
        // 采集类 exec 通道一律拒绝——前端已按外部会话屏蔽监控与 AI 工具，
        // 这里兜底防止任何调用方误开第二条通道导致堡垒机掐断整个会话
        if self.single_channel {
            return Err(SshError::Channel(
                "EXTERNAL_SINGLE_CHANNEL: 外部连接不开放额外采集通道".into(),
            ));
        }
        let wrapped = format!("/bin/sh -c '{}'", script_body.replace('\'', "'\\''"));
        let deadline = Instant::now() + timeout;

        // 全局并发闸门：极端并发（批量会话齐启动/重连风暴/前端门控失效）下限制同时
        // 在飞的 exec 通道，保护本机 fd 与远端 sshd MaxSessions。正常负载下立即取牌。
        // permit 在整个 channel 读写期间持有，随本函数结束 drop 归还；
        // 排队取牌也被 deadline 覆盖，慢主机上不会无限挂起。
        let _permit = METRIC_EXEC_GATE
            .acquire()
            .await
            .map_err(|e| SshError::Channel(format!("exec 闸门已关闭: {e}")))?;
        // 取到牌才计入在飞（不计排队），守卫随本块结束 drop 递减，成功失败都不漏
        let _inflight = InflightGuard::enter();
        let handle = self.handle.lock().await;

        // channel_open_session + ch.exec 受 deadline 约束：超时返回部分输出为空
        let open_fut = async {
            let ch = handle
                .channel_open_session()
                .await
                .map_err(|e| SshError::Channel(e.to_string()))?;
            ch.exec(true, wrapped)
                .await
                .map_err(|e| SshError::Channel(e.to_string()))?;
            Ok::<_, SshError>(ch)
        };
        let mut ch = match tokio::time::timeout_at(deadline, open_fut).await {
            // deadline 内成功打开并 exec
            Ok(Ok(ch)) => ch,
            // deadline 内打开/exec 失败
            Ok(Err(e)) => return Err(e),
            // deadline 超时
            Err(_) => {
                return Ok(ExecOutput {
                    output: String::new(),
                    exit_code: None,
                    timed_out: true,
                });
            }
        };

        let mut out = Vec::new();
        let mut exit_code = None;
        let mut timed_out = false;
        if let Some(bytes) = stdin {
            if !bytes.is_empty() {
                ch.data(bytes)
                    .await
                    .map_err(|e| SshError::Channel(e.to_string()))?;
            }
            ch.eof()
                .await
                .map_err(|e| SshError::Channel(e.to_string()))?;
        }
        // deadline-based 循环内超时：逐次 ch.wait 竞速剩余时间，
        // 超时时 break 并保留 out 中已收到的部分片段
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                timed_out = true;
                break;
            }
            match tokio::time::timeout(remaining, ch.wait()).await {
                Err(_) => {
                    timed_out = true;
                    break;
                }
                Ok(None) => break, // 通道关闭
                Ok(Some(msg)) => match msg {
                    ChannelMsg::Data { data } => out.extend_from_slice(&data),
                    ChannelMsg::ExtendedData { data, .. } => out.extend_from_slice(&data),
                    ChannelMsg::ExitStatus { exit_status } => {
                        exit_code = Some(exit_status as i32);
                    }
                    ChannelMsg::Close => break,
                    _ => {}
                },
            }
        }
        let _ = ch.close().await;
        // 远端命令输出均为 UTF-8 文本（/proc、df 等）；非法字节不致命，
        // 与 MOTD 采集一致用 from_utf8_lossy 语义交给解析层降级
        Ok(ExecOutput {
            output: String::from_utf8_lossy(&out).into_owned(),
            exit_code,
            timed_out,
        })
    }

    /// 启动动态指标采集（0x06）。幂等：已在采集则先停旧任务再以新间隔启动。
    /// `interval_ms` 夹在 1s~10s，防异常参数打爆远端；
    /// `iface` 为默认路由网卡名，网络计数优先取它，None/缺失时汇总非 lo 网卡。
    ///
    /// 单通道会话（堡垒机外部连接）拒绝启动：采集每轮都要开一条 exec 通道。
    pub fn start_metrics(
        self: Arc<Self>,
        interval_ms: u64,
        iface: Option<String>,
    ) -> Result<(), SshError> {
        if self.single_channel {
            return Err(SshError::Channel(
                "EXTERNAL_SINGLE_CHANNEL: 外部连接不开放指标采集通道".into(),
            ));
        }
        let interval = Duration::from_millis(interval_ms.clamp(1000, 10_000));
        let mut guard = self.metrics.lock().unwrap();
        if let Some(old) = guard.take() {
            old.stop();
        }
        let handle = crate::metrics::collector::spawn(
            self.clone(),
            interval,
            self.cancel.clone(),
            self.frame_tx.clone(),
            iface,
        );
        *guard = Some(handle);
        Ok(())
    }

    /// 停止动态指标采集（心跳超时由任务自行退出，此处为前端显式停/切换会话）
    pub fn stop_metrics(&self) {
        if let Ok(mut guard) = self.metrics.lock()
            && let Some(handle) = guard.take()
        {
            handle.stop();
        }
    }

    /// 进程弹窗订阅开关：采集未启动时静默忽略（下次启动默认关）
    pub fn set_procs_full(&self, on: bool) {
        if let Ok(guard) = self.metrics.lock()
            && let Some(handle) = guard.as_ref()
        {
            handle.set_procs_full(on);
        }
    }

    /// 前端心跳续约：刷新采集任务的 TTL
    pub fn metrics_heartbeat(&self) {
        if let Ok(guard) = self.metrics.lock()
            && let Some(handle) = guard.as_ref()
        {
            handle.heartbeat();
        }
    }

    /// 读取跨采集任务的 CPU 快照状态（collector 启动时恢复）
    pub(crate) fn metrics_cpu_state(&self) -> crate::metrics::CpuSampleState {
        *self.metrics_cpu.lock().unwrap()
    }

    /// 写回 CPU 快照状态（每轮采集成功后调用）
    pub(crate) fn set_metrics_cpu_state(&self, state: crate::metrics::CpuSampleState) {
        *self.metrics_cpu.lock().unwrap() = state;
    }

    /// 关闭会话：取消所有后台任务，释放 russh 连接。
    /// 幂等：重复调用仅首次产出 `ssh.disconnect` 日志事件。
    pub fn shutdown(&self) {
        // 已取消说明是重复 shutdown（disconnect/shutdown_all 不重复记日志）
        if self.cancel.is_cancelled() {
            return;
        }
        // 端口转发收尾：撤规则任务、清 -R 注册表、置 Stopped（同步实现，无 await）。
        // 必须在 cancel 之前做：engine 内部还要读各 entry 的状态字段
        self.tunnel.shutdown();
        // 采集任务挂在会话 cancel 的 child token 上，根取消即连带停止
        if let Ok(mut guard) = self.metrics.lock() {
            *guard = None;
        }
        self.cancel.cancel();
        // read_task 的 EOF 分支可能已先记录（异常断开）；原子 swap 保证只记一条
        if !self.dc_state.logged.swap(true, Ordering::Relaxed) {
            applog::emit(
                log::Level::Info,
                "ssh",
                ev::SSH_DISCONNECT,
                Some(&self.dc_state.sid),
                "连接已断开",
                Some(serde_json::json!({
                    "reason": "closed",
                    "uptime_s": self.dc_state.connected_at.elapsed().as_secs(),
                })),
            );
        }
        // 清理初始化脚本：source 自删仅在脚本成功执行后生效，会话在 source
        // 前断开会导致残留。spawn 异步任务，不阻塞 shutdown（同步方法）。
        // 复用 exec_collect 的 exec 通道（有闸门保护），包裹 rm -f 命令。
        if let Some(ref path) = self.init_script_path {
            let handle = self.handle.clone();
            let sid = self.dc_state.sid.clone();
            let path = path.clone();
            tokio::spawn(async move {
                let h = handle.lock().await;
                match h.channel_open_session().await {
                    Ok(ch) => {
                        let cmd = format!("rm -f '{path}'");
                        if let Err(e) = ch.exec(true, cmd.as_str()).await {
                            applog::emit(
                                log::Level::Warn,
                                "ssh",
                                ev::SSH_SESSION_INIT_SCRIPT_CLEANUP_FAILED,
                                Some(&sid),
                                format!("初始化脚本清理失败（{path}）: {e}"),
                                None,
                            );
                        }
                        // 不等待 channel EOF：shutdown 场景连接即将断开，
                        // 尽力而为发送即可，ch 随作用域结束自动关闭
                    }
                    Err(e) => {
                        applog::emit(
                            log::Level::Warn,
                            "ssh",
                            ev::SSH_SESSION_INIT_SCRIPT_CLEANUP_FAILED,
                            Some(&sid),
                            format!("初始化脚本清理通道打开失败（{path}）: {e}"),
                            None,
                        );
                    }
                }
            });
        }
    }

    /// 派生会话级取消令牌：SFTP 传输等独立任务挂在其上，
    /// 会话 shutdown 时连带取消（供同 crate 的 sftp 模块使用）
    pub(super) fn child_cancel(&self) -> CancellationToken {
        self.cancel.child_token()
    }

    /// 会话标签（uuid 前 6 位）：供 metrics 采集器等跨模块组件的日志事件使用
    pub(crate) fn sid(&self) -> &str {
        &self.dc_state.sid
    }

    /// 端口转发管理器引用（供 manager/ipc 层转发 tunnel_start/tunnel_stop、e2e 直连调用）
    pub fn tunnel(&self) -> &TunnelManager {
        &self.tunnel
    }
}

/// Docker exec 终端读任务：输出编码为 Data 帧，结束时发一帧 Exit
/// （payload 为 JSON `{"code": <u32|null>, "reason": <文本>}`）。
/// 取消（前端关闭浮层）与远端进程退出都会走到 Exit，由前端决定是否提示。
async fn docker_term_read_task(
    mut read_half: russh::ChannelReadHalf,
    frame_tx: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
) {
    let mut exit_code: Option<u32> = None;
    let mut reason: Option<String> = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let payload = serde_json::json!({ "code": exit_code, "reason": "终端已关闭" });
                let bytes = serde_json::to_vec(&payload).unwrap_or_default();
                let _ = frame_tx.send(encode_frame(FrameType::Exit, &bytes)).await;
                break;
            }
            msg = read_half.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        if frame_tx
                            .send(encode_frame(FrameType::Data, &data))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        if frame_tx
                            .send(encode_frame(FrameType::Data, &data))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        exit_code = Some(exit_status);
                        reason = Some(format!("退出码 {exit_status}"));
                    }
                    Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                        reason = Some(format!("被信号 {signal_name:?} 终止"));
                    }
                    Some(ChannelMsg::Eof) if reason.is_some() => {
                        let payload = serde_json::json!({ "code": exit_code, "reason": reason });
                        let bytes = serde_json::to_vec(&payload).unwrap_or_default();
                        let _ = frame_tx.send(encode_frame(FrameType::Exit, &bytes)).await;
                        break;
                    }
                    Some(ChannelMsg::Eof) => {}
                    Some(ChannelMsg::Close) | None => {
                        let payload = serde_json::json!({ "code": exit_code, "reason": reason });
                        let bytes = serde_json::to_vec(&payload).unwrap_or_default();
                        let _ = frame_tx.send(encode_frame(FrameType::Exit, &bytes)).await;
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// 读任务：独占读半通道，把服务端消息转成 PtyEvent。
/// 结束时 event_tx 随作用域 drop，merge_task 收到 None 后 flush 并退出。
async fn read_task(
    mut read_half: russh::ChannelReadHalf,
    event_tx: mpsc::Sender<PtyEvent>,
    cancel: CancellationToken,
    dc_state: Arc<DisconnectState>,
) {
    fn try_log_disconnect(dc_state: &DisconnectState, reason: &str, exit_code: Option<u32>) {
        if !dc_state.logged.swap(true, Ordering::Relaxed) {
            applog::emit(
                log::Level::Warn,
                "ssh",
                ev::SSH_DISCONNECT,
                Some(&dc_state.sid),
                "连接已断开",
                Some(serde_json::json!({
                    "reason": reason,
                    "exit_code": exit_code,
                    "uptime_s": dc_state.connected_at.elapsed().as_secs(),
                })),
            );
        }
    }
    // 远端退出信息（exit-status/exit-signal）。必须以「是否收到过退出状态」判定
    // 正常退出 vs 连接丢失：部分 sshd/时序下 Eof/Close 会先于 ExitStatus 投递，
    // 见 Eof 即收尾会把正常 exit 误判为掉线（触发前端无谓自动重连）。
    // 因此退出状态只记录、不收尾，统一在通道关闭处按记录是否存在决定 lost。
    let mut exit_reason: Option<String> = None;
    let mut exit_log_reason = "eof";
    let mut exit_code: Option<u32> = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                // 取消令牌触发（shutdown）：发送 EOF 通知前端连接已断开
                try_log_disconnect(&dc_state, "cancelled", None);
                let _ = event_tx
                    .send(PtyEvent::Eof {
                        reason: None,
                        lost: true,
                    })
                    .await;
                break;
            }
            msg = read_half.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        if event_tx.send(PtyEvent::Data(data.to_vec())).await.is_err() {
                            break;
                        }
                    }
                    // stderr（扩展数据流）并入主流，交给 xterm 渲染
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        if event_tx.send(PtyEvent::Data(data.to_vec())).await.is_err() {
                            break;
                        }
                    }
                    // 记录退出状态，继续排空到 Eof/Close/None（消息可能乱序先至）
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        exit_log_reason = "exit_status";
                        exit_code = Some(exit_status);
                        exit_reason = Some(format!("进程退出，状态码 {exit_status}"));
                    }
                    Some(ChannelMsg::ExitSignal { signal_name, error_message, .. }) => {
                        exit_log_reason = "exit_signal";
                        exit_reason =
                            Some(format!("进程被信号 {signal_name:?} 终止: {error_message}"));
                    }
                    // 半关闭：已拿到退出状态即可收尾（正常路径 ExitStatus→Eof）；
                    // 无退出状态时继续等 Close/None——异常断开紧接着就会到，
                    // 若退出状态在乱序队列后面也在此兜住
                    Some(ChannelMsg::Eof) if exit_reason.is_some() => {
                        try_log_disconnect(&dc_state, exit_log_reason, exit_code);
                        let _ = event_tx
                            .send(PtyEvent::Eof { reason: exit_reason, lost: false })
                            .await;
                        break;
                    }
                    Some(ChannelMsg::Eof) => {}
                    // 通道完全关闭：有退出状态=正常退出；无=连接意外丢失（自动重连依据）
                    Some(ChannelMsg::Close) | None => {
                        try_log_disconnect(&dc_state, exit_log_reason, exit_code);
                        let _ = event_tx
                            .send(PtyEvent::Eof {
                                reason: exit_reason,
                                lost: exit_log_reason == "eof",
                            })
                            .await;
                        break;
                    }
                    // Success / Failure / WindowAdjusted 等控制消息暂不处理
                    _ => {}
                }
            }
        }
    }
}

/// 扫描字节流中的 CWD 上报 OSC 序列（`\x1b]6667;<path>\x07`），返回提取到的路径。
/// OSC 字节保留在原流中由 xterm.js 静默丢弃（未知 OSC 码不上屏），故此处不做移除，
/// 仅提取路径供后端同步 SFTP CWD。若前缀已到但终止符未到（跨包），返回 None 等待。
fn scan_cwd(buf: &[u8]) -> Option<String> {
    let mut i = 0;
    while i + CWD_MARKER_PREFIX.len() <= buf.len() {
        if &buf[i..i + CWD_MARKER_PREFIX.len()] == CWD_MARKER_PREFIX {
            let tail = &buf[i + CWD_MARKER_PREFIX.len()..];
            if let Some(rel) = tail.iter().position(|&b| b == OSC_TERMINATOR) {
                return Some(String::from_utf8_lossy(&tail[..rel]).into_owned());
            }
            // 前缀命中但无终止符：路径尚未收完，等下一批数据
            return None;
        }
        i += 1;
    }
    None
}

/// 合并任务：把高频小数据包按「4KB 或 5ms」攒成一帧，降低 IPC 次数。
/// 控制事件（Eof）到达时先冲刷缓存数据再发 Exit 帧，保证前端看到的顺序。
///
/// 提示符注入（init_inject 存在时）三阶段状态机，全程输出不上屏：
/// 1. AwaitQuiet：hold 全部 PTY 输出，120ms 静默判定 PS1 已刷完（readline 阻塞）
/// 2. 注入 source 命令
/// 3. AwaitMarker：检测到脚本末尾 OSC marker 后，先按内容剥掉注入命令的回显行
///    （[`late_gate`]），再转入主攒包窗口与紧随的着色 PS1 同帧放行——
///    前端单帧绘制最终画面，屏上既无原始 PS1，也无回显中间态
/// 3s 超时兜底：尽力注入、同样剥回显后直通放行（退化但不阻塞）。
///
/// 另外扫描 PTY 输出中的 CWD 上报 OSC 序列，命中即更新会话 cwd 并向前端发 Cwd 帧。
async fn merge_task(
    mut event_rx: mpsc::Receiver<PtyEvent>,
    frame_tx: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
    init_inject: Option<(Vec<u8>, Vec<u8>, mpsc::Sender<WriteReq>)>,
    cwd: Arc<Mutex<Option<String>>>,
    cd_strip: Arc<Mutex<Vec<(Vec<u8>, Instant)>>>,
) {
    let mut buf = BytesMut::with_capacity(MERGE_BYTES * 2);
    // 合并窗口定时器：仅在窗口开启时参与 select
    let window = time::sleep(MERGE_WINDOW);
    tokio::pin!(window);
    let mut window_open = false;

    let mut hold: Option<InitHold> = init_inject.map(|(cmd, probe, tx)| InitHold {
        buf: BytesMut::with_capacity(8192),
        cmd: Some(cmd),
        probe,
        tx,
    });
    // 放行后仍在生效的回显剥离窗口（见 LATE_ECHO_TTL）；无注入则为 None，零开销
    let mut late: Option<(Vec<u8>, Instant)> = None;
    // cd 同步窗口扣留的半行/半个 probe（见 cd_gate），下一轮 flush 拼回再裁决
    let mut cd_hold = BytesMut::new();
    // 静默计时（等 PS1 刷完）与 hold 硬超时，仅在对应阶段参与 select
    let quiet = time::sleep(SHELL_QUIET);
    tokio::pin!(quiet);
    let mut quiet_open = false;
    let hold_deadline = time::sleep(Duration::from_secs(3));
    tokio::pin!(hold_deadline);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            ev = event_rx.recv() => {
                match ev {
                    Some(PtyEvent::Data(data)) => {
                        if let Some(h) = &mut hold {
                            let scan = h.buf.len().saturating_sub(INIT_DONE_MARKER.len() - 1);
                            h.buf.extend_from_slice(&data);
                            if h.cmd.is_some() {
                                // 阶段 1：启动输出仍在刷新，重置静默计时
                                quiet.as_mut().reset(Instant::now() + SHELL_QUIET);
                                quiet_open = true;
                            } else if h.buf[scan..]
                                .windows(INIT_DONE_MARKER.len())
                                .any(|w| w == INIT_DONE_MARKER)
                            {
                                // 阶段 3：marker 到——hold 内容转入主攒包缓冲走
                                // 正常窗口，紧随 marker 的着色 PS1 并入同帧；
                                // 注入命令的回显行由 flush 在发出前按内容剥掉
                                let h = hold.take().unwrap();
                                late = Some((h.probe, Instant::now() + LATE_ECHO_TTL));
                                buf.extend_from_slice(&h.buf[..]);
                                if !window_open {
                                    window.as_mut().reset(Instant::now() + MERGE_WINDOW);
                                    window_open = true;
                                }
                            }
                            continue;
                        }
                        buf.extend_from_slice(&data);
                        // 扫描 Shell CWD 上报序列，命中即更新会话 cwd 并向前端发 Cwd 帧。
                        // OSC 字节仍留在 buf 中随 PTY 流下发，xterm.js 对未知 OSC 码静默丢弃。
                        if let Some(path) = scan_cwd(&buf) {
                            *cwd.lock().await = Some(path.clone());
                            let _ = frame_tx
                                .send(encode_frame(FrameType::Cwd, path.as_bytes()))
                                .await;
                        }
                        if !window_open {
                            window.as_mut().reset(Instant::now() + MERGE_WINDOW);
                            window_open = true;
                        }
                        if buf.len() >= MERGE_BYTES {
                            flush(&mut buf, &frame_tx, &mut late, &cd_strip, &mut cd_hold).await;
                            window_open = false;
                        }
                    }
                    Some(PtyEvent::Eof { reason, lost }) => {
                        if let Some(h) = hold.take() {
                            buf.extend_from_slice(&h.buf[..]);
                        }
                        flush(&mut buf, &frame_tx, &mut late, &cd_strip, &mut cd_hold).await;
                        // 结构化 payload：reason 展示用，lost 指示前端是否可自动重连
                        let payload = serde_json::json!({
                            "reason": reason.unwrap_or_default(),
                            "lost": lost,
                        })
                        .to_string();
                        let _ = frame_tx
                            .send(encode_frame(FrameType::Exit, payload.as_bytes()))
                            .await;
                        break;
                    }
                    // 上游全部关闭：冲刷残包后退出
                    None => {
                        if let Some(h) = hold.take() {
                            buf.extend_from_slice(&h.buf[..]);
                        }
                        flush(&mut buf, &frame_tx, &mut late, &cd_strip, &mut cd_hold).await;
                        break;
                    }
                }
            }
            _ = &mut window, if window_open => {
                flush(&mut buf, &frame_tx, &mut late, &cd_strip, &mut cd_hold).await;
                window_open = false;
            }
            // 阶段 1→2：静默确认 shell 就绪，注入 source 命令
            _ = &mut quiet, if quiet_open => {
                quiet_open = false;
                if let Some(h) = &mut hold {
                    if let Some(cmd) = h.cmd.take() {
                        let _ = h.tx.send(WriteReq::Data(cmd)).await;
                    }
                }
            }
            // hold 超时：尽力注入（若还未），已攒内容直通放行；回显多半还在路上，
            // 交给 late 窗口在后续 flush 里剥
            _ = &mut hold_deadline, if hold.is_some() => {
                if let Some(mut h) = hold.take() {
                    if let Some(cmd) = h.cmd.take() {
                        let _ = h.tx.send(WriteReq::Data(cmd)).await;
                    }
                    late = Some((h.probe, Instant::now() + LATE_ECHO_TTL));
                    buf.extend_from_slice(&h.buf[..]);
                    flush(&mut buf, &frame_tx, &mut late, &cd_strip, &mut cd_hold).await;
                    window_open = false;
                }
            }
        }
    }
}

/// 回显剥离窗口内对缓冲的放行裁决（[`flush`] 每次发送前调用）。
///
/// 为什么在本地按内容剥而不是让远端脚本发清行序列：清行只能按游标相对位置猜行，
/// 而注入时机靠输出静默判定——远端 profile 慢一拍（如 `/etc/profile.d/lang.sh`
/// 里 `/sbin/consoletype` 报 ENOENT 才继续）时，回显、告警、PS1 三者会错位，
/// 相对清行要么留下回显，要么把服务器自己的告警行吃掉（两种形态都实测过）。
/// 本地知道注入的确切字节（probe 为本次会话独有的随机路径），按内容定位无猜测。
///
/// 三种裁决（每次都把缓冲里**所有**完整回显行剥掉，见下「回显会出现两次」）：
/// - 完整命中（probe 与行尾换行同在）：整行剥除，继续找下一处；
/// - probe 已现、行尾换行未到（flush 边界正好切在回显行中间）：从 probe 所在行
///   的行首起整段扣留不发，留待下一批字节补全后再剥——照发半行即用户可见的
///   「`[root@x ~]# . "/tmp/.ri-xx" 2>/dev/null` 泄漏」（3s 兜底放行后回显才
///   到达的会话必经此窗口，PTY 分包/5ms 攒包边界落点不定，故偶发）；
/// - probe 未现：检查缓冲末尾是否恰为 probe 的真前缀（probe 被切一半的形态），
///   是则只扣留该后缀待拼接，否则全部放行。
///
/// 回显会出现两次（172.17.4.2 实测）：注入的字节在 shell 还在跑 profile 时到达
/// tty，此时行规程是规范模式 + ECHO，内核立刻回显一份（**不带 PS1**）；profile
/// 刷完、readline 接管后才从缓冲里取出这行逐字符再回显一份（**带原始 PS1 前缀**）。
/// 只剥第一份就关窗，第二份原样上屏。故窗口不因命中而关闭，只按 [`LATE_ECHO_TTL`]
/// 到期关闭；probe 是本次会话独有的随机路径，10s 内误伤正常输出的概率为零。
///
/// 返回 true 表示窗口继续开启（除空 probe 外恒为 true，TTL 到点由 [`flush`] 收窗）。
///
/// 扣留的字节最坏滞留到窗口过期（10s）才直通，但仅会话开头存在窗口且
/// 扣留量 ≤ probe 长度（33 字节内），终端侧无感知。
fn late_gate(buf: &mut BytesMut, probe: &[u8]) -> bool {
    if probe.is_empty() {
        return true;
    }
    while let Some(hit) = buf.windows(probe.len()).position(|w| w == probe) {
        let row_start = buf[..hit]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |p| p + 1);
        // 行尾换行未到：半行（扣留待补全）；已到：完整回显行，整行剥除后继续找下一处
        let Some(off) = buf[hit..].iter().position(|&b| b == b'\n') else {
            if row_start > 0 {
                let tail = buf.split_off(row_start);
                *buf = tail;
            }
            return true;
        };
        let row_end = hit + off + 1;
        buf.copy_within(row_end.., row_start);
        buf.truncate(buf.len() - (row_end - row_start));
    }
    // probe 未现：末尾真前缀（被切一半的 probe 开头）扣留待拼接
    let keep = longest_suffix_prefix(buf, probe);
    if keep > 0 {
        let tail = buf.split_off(buf.len() - keep);
        *buf = tail;
    }
    true
}

/// buf 末尾与 probe 前缀的最长公共部分长度（0 = 不可能有被切开的 probe）。
fn longest_suffix_prefix(buf: &[u8], probe: &[u8]) -> usize {
    let max = probe.len().saturating_sub(1).min(buf.len());
    for n in (1..=max).rev() {
        if &buf[buf.len() - n..] == &probe[..n] {
            return n;
        }
    }
    0
}

/// 把累积缓冲区封装为 Data 帧发出（空缓冲不发）。
/// 发出前若回显剥离窗口仍开着，先经 [`late_gate`] 裁决：命中则剥整行（可多处）；
/// 半行/半个 probe 则扣留待拼接（其余照发），窗口到期后全部直通。
/// cd 同步窗口（[`cd_gate`]）独立裁决：剥行后原位清行，扣留字节存 cd_hold。
async fn flush(
    buf: &mut BytesMut,
    frame_tx: &mpsc::Sender<Vec<u8>>,
    late: &mut Option<(Vec<u8>, Instant)>,
    cd_strip: &Mutex<Vec<(Vec<u8>, Instant)>>,
    cd_hold: &mut BytesMut,
) {
    if buf.is_empty() && cd_hold.is_empty() {
        return;
    }
    // 拼回上轮扣留的半行，保证 cd 回显跨 flush 边界也能完整命中
    if !cd_hold.is_empty() {
        let mut combined = std::mem::take(cd_hold);
        combined.extend_from_slice(&buf[..]);
        *buf = combined;
    }
    if let Some((probe, until)) = late.take() {
        if Instant::now() < until {
            if late_gate(buf, &probe) {
                *late = Some((probe, until));
            }
        }
    }
    {
        let mut probes = cd_strip.lock().await;
        probes.retain(|(_, until)| Instant::now() < *until);
        if !probes.is_empty() {
            let owned: Vec<Vec<u8>> = probes.iter().map(|(p, _)| p.clone()).collect();
            let sendable = cd_gate(buf, &owned);
            *cd_hold = buf.split_off(sendable);
        }
    }
    if buf.is_empty() {
        return;
    }
    let frame = encode_frame(FrameType::Data, &buf[..]);
    buf.clear();
    if frame_tx.send(frame).await.is_err() {
        warn!("帧输出通道已关闭，丢弃 PTY 数据");
    }
}

/// SFTP cd 同步的回显裁决：返回本轮可放行的前缀长度，其余为扣留的半行/半个
/// probe（调用方存入 cd_hold 待下一轮拼接）。与 [`late_gate`] 的纯剥除不同，
/// 完整回显行原位替换为 [`CD_CLEAR_LINE`]——旧提示符行被清掉，紧随的新目录
/// 提示符落在原行，终端无 cd 命令、无空行。
///
/// 一次性处理全部 probe（连续快速切目录时多条 cd 并存）：在原始快照上按 `\n`
/// 定位行边界、重建输出。不能边剥边扫——替换序列不含 `\n`，剥掉第一行后
/// 后续行的行首边界就丢了，会把前一行的清行序列也吃掉。
///
/// 三种形态与 late_gate 一致：完整行（probe + 行尾换行）替换；probe 已现但
/// 换行未到，从行首起扣留；probe 未现则只扣留末尾的 probe 真前缀。
/// 替换而非追加到缓冲末尾：回显行与新提示符常在同一批字节里，追加会把
/// 新提示符一起清掉。
fn cd_gate(buf: &mut BytesMut, probes: &[Vec<u8>]) -> usize {
    if probes.is_empty() {
        return buf.len();
    }
    let src = buf.clone();
    let mut out = BytesMut::with_capacity(src.len());
    let mut pos = 0;
    while pos < src.len() {
        // 取位置最靠前的 probe 命中（多条 cd 按到达顺序处理）
        let mut best: Option<usize> = None;
        for p in probes {
            if p.is_empty() {
                continue;
            }
            if let Some(rel) = src[pos..].windows(p.len()).position(|w| w == &p[..]) {
                let hit = pos + rel;
                if best.map_or(true, |h| hit < h) {
                    best = Some(hit);
                }
            }
        }
        let Some(hit) = best else {
            out.extend_from_slice(&src[pos..]);
            let keep = probes
                .iter()
                .map(|p| longest_suffix_prefix(&out, p))
                .max()
                .unwrap_or(0);
            *buf = out;
            return buf.len() - keep;
        };
        let row_start = src[..hit]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |p| p + 1);
        out.extend_from_slice(&src[pos..row_start]);
        // 行尾换行未到：半行，放行行首之前的字节，其余扣留待拼接
        let Some(off) = src[hit..].iter().position(|&b| b == b'\n') else {
            let sendable = out.len();
            out.extend_from_slice(&src[row_start..]);
            *buf = out;
            return sendable;
        };
        out.extend_from_slice(CD_CLEAR_LINE);
        pos = hit + off + 1;
    }
    *buf = out;
    buf.len()
}

/// 写任务：串行处理前端输入与尺寸变更（均通过写半通道发出）。
/// 退出时尽力通知服务端关闭通道；Handle 随任务 drop 后底层连接释放。
async fn write_task(
    write_half: russh::ChannelWriteHalf<client::Msg>,
    mut write_rx: mpsc::Receiver<WriteReq>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            req = write_rx.recv() => {
                match req {
                    Some(WriteReq::Data(bytes)) => {
                        if write_half.data_bytes(bytes).await.is_err() {
                            break;
                        }
                    }
                    Some(WriteReq::Resize { cols, rows }) => {
                        // window-change 不要求对端回复；失败说明通道已断
                        if write_half.window_change(cols, rows, 0, 0).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    let _ = write_half.close().await;
}

/// RTT 探测任务（0x07 帧）：SSH global request ping/pong 计时。
///
/// - 建连后立即测一次让首屏有值，之后每 30s 一轮；
/// - `send_ping` 等待 sshd 的 request 响应（RFC 4254 要求对 want_reply 的
///   global request 必回 success/failure，故任何标准 sshd 都能计时）；
/// - 单轮 5s 超时/协议错误静默跳过——断连判定交给 inactivity_timeout 与
///   PTY 通道错误，RTT 任务不做连接健康裁决；
/// - ping 期间持有 handle 锁，会短暂阻塞同会话 metrics exec 开通道：
///   正常 RTT 亚秒级，最坏 5s 超时导致 metrics 单轮跳过（有界影响，可接受）；
/// - 帧发送失败即前端通道已释放（窗口关闭），任务退出。
async fn rtt_task(
    handle: Arc<Mutex<client::Handle<ClientHandler>>>,
    frame_tx: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
) {
    loop {
        let fut = async {
            let h = handle.lock().await;
            let start = Instant::now();
            h.send_ping().await.map(|_| start.elapsed())
        };
        if let Ok(Ok(d)) = time::timeout(RTT_TIMEOUT, fut).await {
            let payload = serde_json::to_vec(&serde_json::json!({ "ms": d.as_millis() as u64 }))
                .unwrap_or_default();
            if frame_tx
                .send(encode_frame(FrameType::Rtt, &payload))
                .await
                .is_err()
            {
                break;
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = time::sleep(RTT_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
mod hostkey_decision_tests {
    use super::{VerdictKind, evaluate_host_key};

    const FP: &str = "SHA256:CUR";
    const ALGO: &str = "ssh-ed25519";

    fn stored(fp: &str) -> Option<(String, String)> {
        Some((ALGO.into(), fp.into()))
    }

    /// 第 1 行：trust 重试命中 → Trusted（stored=None 首连 与 stored=Some(旧) 变更更新
    /// 两态都必须落盘——该行对 stored 不作区分，mismatch「更新指纹并重连」依赖此序）
    #[test]
    fn trust_hit_passes_and_persists_for_both_stored_states() {
        for stored in [None, stored("SHA256:OLD")] {
            let (pass, kind) = evaluate_host_key(stored.as_ref(), true, Some(FP), FP);
            assert!(pass, "stored={stored:?}");
            assert_eq!(kind, VerdictKind::Trusted, "stored={stored:?}");
        }
    }

    /// 第 2 行：确认指纹 ≠ 本次指纹（TOFU 竞态：二次握手密钥已变）→ Mismatch 拒绝
    #[test]
    fn trust_stale_fingerprint_is_mismatch() {
        let (pass, kind) = evaluate_host_key(None, true, Some("SHA256:OTHER"), FP);
        assert!(!pass);
        assert_eq!(kind, VerdictKind::Mismatch);
    }

    /// 第 3 行：trust=true 但缺 trust_fp → 视同未确认，按 stored 比对走，绝不落盘
    #[test]
    fn trust_without_fingerprint_falls_back_to_stored_compare() {
        // 无记录：落回第 6 行 Unknown
        let (pass, kind) = evaluate_host_key(None, true, None, FP);
        assert!(!pass);
        assert_eq!(kind, VerdictKind::Unknown);
        // 有记录且一致：落回第 4 行 Match
        let (pass, kind) = evaluate_host_key(stored(FP).as_ref(), true, None, FP);
        assert!(pass);
        assert_eq!(kind, VerdictKind::Match);
        // 有记录但不一致：落回第 5 行 Mismatch
        let (pass, kind) = evaluate_host_key(stored("SHA256:OLD").as_ref(), true, None, FP);
        assert!(!pass);
        assert_eq!(kind, VerdictKind::Mismatch);
    }

    /// 第 4 行：磁盘记录一致 → Match
    #[test]
    fn stored_match_passes() {
        let (pass, kind) = evaluate_host_key(stored(FP).as_ref(), false, None, FP);
        assert!(pass);
        assert_eq!(kind, VerdictKind::Match);
    }

    /// 第 5 行：密钥变更 → Mismatch
    #[test]
    fn stored_changed_is_mismatch() {
        let (pass, kind) = evaluate_host_key(stored("SHA256:OLD").as_ref(), false, None, FP);
        assert!(!pass);
        assert_eq!(kind, VerdictKind::Mismatch);
    }

    /// 第 6 行：首连（无记录、未信任）→ Unknown
    #[test]
    fn first_connect_is_unknown() {
        let (pass, kind) = evaluate_host_key(None, false, None, FP);
        assert!(!pass);
        assert_eq!(kind, VerdictKind::Unknown);
    }
}

#[cfg(test)]
mod gate_tests {
    use super::METRIC_EXEC_GATE;

    /// 全局闸门容量固定 24：占满后第 25 个取牌失败，permit 释放后立即可再取。
    /// lib 单测二进制内仅本测试接触该全局信号量（真实 exec 在独立的 e2e 二进制）。
    #[test]
    fn exec_gate_caps_at_24() {
        let permits: Vec<_> = (0..24)
            .map(|i| {
                METRIC_EXEC_GATE
                    .try_acquire()
                    .unwrap_or_else(|_| panic!("第 {i} 个 permit 应可获取"))
            })
            .collect();
        assert!(
            METRIC_EXEC_GATE.try_acquire().is_err(),
            "占满 24 个后不应再能取牌"
        );
        drop(permits);
        assert!(
            METRIC_EXEC_GATE.try_acquire().is_ok(),
            "permit 全部释放后应恢复可取"
        );
    }
}

#[cfg(test)]
mod env_export_tests {
    use super::{
        cd_gate, env_export_lines, init_script, late_gate, locale_login_cmd, INIT_DONE_MARKER,
    };
    use bytes::BytesMut;

    #[test]
    fn normal_pairs_get_quoted_exports() {
        let out = env_export_lines(&[
            ("EDITOR".into(), "nvim".into()),
            ("PROXY_URL".into(), "http://127.0.0.1:7890".into()),
        ]);
        assert_eq!(
            out,
            "export EDITOR='nvim'\nexport PROXY_URL='http://127.0.0.1:7890'\n"
        );
    }

    #[test]
    fn single_quote_in_value_is_posix_escaped() {
        let out = env_export_lines(&[("MSG".into(), "it's ok".into())]);
        // '\'' 关闭引号+转义引号+重开引号，shell 里还原为字面单引号
        assert_eq!(out, "export MSG='it'\\''s ok'\n");
    }

    #[test]
    fn illegal_keys_and_multiline_values_are_skipped() {
        let out = env_export_lines(&[
            ("1BAD".into(), "v".into()),    // 数字开头
            ("BAD KEY".into(), "v".into()), // 含空格
            ("BAD;KEY".into(), "v".into()), // 注入面：分号
            ("BAD-KEY".into(), "v".into()), // 连字符
            ("".into(), "v".into()),        // 空键
            ("OK".into(), "a\nb".into()),   // 值含换行破坏脚本单行结构
            ("OK".into(), "a\rb".into()),   // 值含回车
            ("GOOD_1".into(), "v".into()),  // 合法：下划线+数字
            ("_X".into(), "v".into()),      // 合法：下划线开头
        ]);
        assert_eq!(out, "export GOOD_1='v'\nexport _X='v'\n");
    }

    #[test]
    fn script_carries_no_remote_clear_line_and_orders_env_before_hook() {
        let script = init_script("/tmp/.ri-test", false, &[("EDITOR".into(), "nvim".into())]);
        // 回显由后端本地剥离，脚本发清行会在 profile 输出与注入交错时吃错行
        assert!(
            !script.contains("\\033[1A") && !script.contains("\\033[2K"),
            "脚本不应再含相对清行序列"
        );
        let first = script.lines().next().unwrap_or_default();
        assert!(
            first.starts_with("_rh_l="),
            "脚本首行应是 locale 探测起始，不该是清行 printf：{first}"
        );
        let env_line = script
            .find("export EDITOR='nvim'")
            .expect("export 块应在脚本内");
        let hook = script.find("_rh_cwd_hook").expect("CWD 钩子应在脚本内");
        assert!(
            env_line < hook,
            "顺序应为 export → CWD 钩子"
        );
        // color_prompt=false 时 export 之后不应有颜色块
        assert!(!script.contains("_rh_e"), "未开启提示符着色时不应有颜色块");
    }

    /// 启动输出的典型形态：profile 输出 → PS1 → 注入命令回显 → 回车 → marker → 新 PS1
    fn startup_stream(warn_before_echo: bool) -> BytesMut {
        let warn = "/etc/profile.d/lang.sh: line 45: /sbin/consoletype: No such file or directory\r\n";
        let echo = " \"/tmp/.ri-baf1d674\" 2>/dev/null\r\n";
        let mut b = BytesMut::new();
        if warn_before_echo {
            b.extend_from_slice(warn.as_bytes());
            b.extend_from_slice("root@localhost:~# ".as_bytes());
            b.extend_from_slice(echo.as_bytes());
        } else {
            // 交错形态（实测截图）：回显先上屏，profile 告警晚于它才打印出来
            b.extend_from_slice(echo.as_bytes());
            b.extend_from_slice(warn.as_bytes());
        }
        b.extend_from_slice(INIT_DONE_MARKER);
        b.extend_from_slice("root@localhost:~# ".as_bytes());
        b
    }

    #[test]
    fn late_gate_strips_only_the_echoed_command_row() {
        let probe = b"/tmp/.ri-baf1d674";
        let mut buf = startup_stream(true);
        let open = late_gate(&mut buf, probe);
        let text = String::from_utf8_lossy(&buf);
        assert!(open, "窗口只按 TTL 收，命中不提前关窗（同一条命令回显两次）");
        assert!(!text.contains(".ri-baf1d674"), "回显必须整行消失：{text}");
        assert!(
            text.contains("/sbin/consoletype"),
            "服务器自己的告警行不能跟着被吃掉：{text}"
        );
        assert!(
            text.ends_with("root@localhost:~# "),
            "脚本生效后的新 PS1 必须保留：{text}"
        );
    }

    #[test]
    fn late_gate_strips_both_echo_copies() {
        // 172.17.4.2 实测泄漏形态：注入字节在 profile 还在跑时到达 tty，内核按
        // 规范模式先回显一份（不带 PS1）；profile 刷完、readline 接管后从缓冲里
        // 取出同一行逐字符再回显一份（带原始 PS1 前缀）。旧实现命中一次即关窗，
        // 第二份原样上屏。
        let probe = b"/tmp/.ri-baf1d674";
        let echo = " . \"/tmp/.ri-baf1d674\" 2>/dev/null\r\n";
        let mut buf = BytesMut::from("/etc/profile.d/lang.sh: line 45: /sbin/consoletype: No such file or directory\r\n");
        buf.extend_from_slice(echo.as_bytes()); // 内核回显（无 PS1）
        buf.extend_from_slice("[root@localhost ~]# ".as_bytes());
        buf.extend_from_slice(echo.as_bytes()); // readline 再回显（带 PS1）
        buf.extend_from_slice(INIT_DONE_MARKER);
        buf.extend_from_slice("root@localhost:~# ".as_bytes());

        let mut open = late_gate(&mut buf, probe);
        assert!(
            !String::from_utf8_lossy(&buf).contains(".ri-baf1d674"),
            "两份回显都要消失（屏上只剩告警行与新 PS1）：{}",
            String::from_utf8_lossy(&buf)
        );
        // 第二份回显落在下一个 flush 批次：窗口仍开着，照样剥
        let mut next = BytesMut::from(echo.as_bytes());
        next.extend_from_slice("[root@localhost ~]# ls\r\n".as_bytes());
        open = late_gate(&mut next, probe) && open;
        assert!(
            !String::from_utf8_lossy(&next).contains(".ri-baf1d674"),
            "跨批次的第二份回显也必须消失：{}",
            String::from_utf8_lossy(&next)
        );
        let text = String::from_utf8_lossy(&buf);
        assert!(open, "窗口保持开启直到 TTL");
        assert!(
            !text.contains(".ri-baf1d674"),
            "两份回显都要消失（屏上只剩告警行与新 PS1）：{text}"
        );
        assert_eq!(
            text.matches("/sbin/consoletype").count(),
            1,
            "告警行不能被吃掉或重复：{text}"
        );
        assert!(
            text.ends_with("root@localhost:~# "),
            "新 PS1 保留：{text}"
        );
        assert!(
            !text.contains("[root@localhost ~]#"),
            "原始 PS1 与第二份回显同一行，一起剥掉才对（屏上只留新 PS1）：{text}"
        );
    }

    #[test]
    fn late_gate_survives_interleaved_profile_output() {
        // 回显与 profile 告警先后颠倒（正是清行序列会吃错行的场景）
        let probe = b"/tmp/.ri-baf1d674";
        let mut buf = startup_stream(false);
        let open = late_gate(&mut buf, probe);
        let text = String::from_utf8_lossy(&buf);
        assert!(open);
        assert!(!text.contains(".ri-baf1d674"), "回显必须消失：{text}");
        assert!(text.contains("/sbin/consoletype"), "告警行必须保留：{text}");
    }

    #[test]
    fn late_gate_passthrough_without_probe_and_empty_probe() {
        let probe = b"/tmp/.ri-baf1d674";
        let mut plain: BytesMut = BytesMut::from("Last login: Mon\r\nroot@localhost:~# ".as_bytes());
        let snapshot = plain.clone();
        // 不含 probe：字节级原样放行，窗口保持开启
        assert!(late_gate(&mut plain, probe));
        assert_eq!(&plain[..], &snapshot[..]);
        // 空 probe（脚本未落盘的降级路径）绝不错删
        assert!(late_gate(&mut plain, b""));
        assert_eq!(&plain[..], &snapshot[..]);
    }

    #[test]
    fn late_gate_withholds_partial_row_until_line_completes() {
        // 泄漏回归守卫（实测截图形态）：flush 边界切在回显行中间，行尾换行未到。
        // 旧行为把半行原样发出，回显 `[root@x ~]# . "/tmp/.ri-xx" ...` 直接上屏。
        let probe = b"/tmp/.ri-baf1d674";
        let head = "Last login: Mon\r\nroot@localhost:~#  \"/tmp/.ri-baf1d674\" 2>/dev/null";
        let mut buf = BytesMut::from(head.as_bytes());
        assert!(late_gate(&mut buf, probe), "半行未命中应保持窗口开启");
        assert_eq!(
            &buf[..],
            &BytesMut::from(
                "root@localhost:~#  \"/tmp/.ri-baf1d674\" 2>/dev/null".as_bytes()
            )[..],
            "半行回显扣留不发（行首前的正常输出由本轮发出）"
        );
        // 下一批字节补上行尾换行 + marker + 新 PS1：此时整行剥得干净
        buf.extend_from_slice("\r\n".as_bytes());
        buf.extend_from_slice(INIT_DONE_MARKER);
        buf.extend_from_slice("root@localhost:~# ".as_bytes());
        let open = late_gate(&mut buf, probe);
        let text = String::from_utf8_lossy(&buf);
        assert!(open, "窗口保持开启直到 TTL");
        assert!(!text.contains(".ri-baf1d674"), "回显整行消失：{text}");
        assert!(
            text.ends_with("root@localhost:~# "),
            "新 PS1 原样保留：{text}"
        );
    }

    #[test]
    fn late_gate_reassembles_probe_split_across_flush_boundary() {
        // probe 本身被 flush 边界切成两半：扣留真前缀待拼接，拼接后剥整行
        let probe = b"/tmp/.ri-baf1d674";
        let mut buf = BytesMut::from("root@localhost:~#  \"/tmp/.ri-baf1".as_bytes());
        assert!(late_gate(&mut buf, probe));
        assert_eq!(
            &buf[..],
            &BytesMut::from("/tmp/.ri-baf1".as_bytes())[..],
            "末尾 probe 真前缀扣留，之前的输出放行"
        );
        // 末尾不是 probe 前缀时不扣留、全部放行
        let mut plain = BytesMut::from("hello world\r\n".as_bytes());
        assert!(late_gate(&mut plain, probe));
        assert_eq!(&plain[..], &BytesMut::from("hello world\r\n".as_bytes())[..]);
        // 补上另一半：拼成完整回显行（还差行尾），再补行尾后剥除
        buf.extend_from_slice("d674\" 2>/dev/null\r\n".as_bytes());
        assert!(late_gate(&mut buf, probe), "拼接后完整命中，窗口仍按 TTL 开启");
        assert!(
            !String::from_utf8_lossy(&buf).contains(".ri-baf1d674"),
            "切开的回显拼接后同样剥得干净"
        );
    }

    /// 测试辅助：把字节串列表转成 cd_gate 需要的 probe 向量
    fn cd_probes(list: &[&[u8]]) -> Vec<Vec<u8>> {
        list.iter().map(|p| p.to_vec()).collect()
    }

    #[test]
    fn cd_gate_replaces_echo_row_with_clear_line_keeps_prompt() {
        // 回显行与新提示符同批到达：原位替换（不是追加到末尾），新提示符不能被清掉
        let probes = cd_probes(&[b"cd '/root'"]);
        let mut buf = BytesMut::from("cd '/root'\r\nroot@localhost:/root# ");
        let sendable = cd_gate(&mut buf, &probes);
        assert_eq!(sendable, buf.len(), "完整行替换后全部放行");
        assert_eq!(
            &buf[..],
            &BytesMut::from("\r\x1b[Kroot@localhost:/root# ")[..],
            "回显行原位换成清行序列，新提示符紧随其后落在原行"
        );
    }

    #[test]
    fn cd_gate_keeps_preceding_output_row() {
        let probes = cd_probes(&[b"cd '/root'"]);
        let mut buf = BytesMut::from("file1\r\ncd '/root'\r\nPS1");
        let sendable = cd_gate(&mut buf, &probes);
        assert_eq!(sendable, buf.len());
        assert_eq!(
            &buf[..],
            &BytesMut::from("file1\r\n\r\x1b[KPS1")[..],
            "回显行之前的正常输出不能被吃掉"
        );
    }

    #[test]
    fn cd_gate_withholds_partial_row_until_line_completes() {
        let probes = cd_probes(&[b"cd '/root'"]);
        // probe 本身被切一半：末尾真前缀扣留，之前的输出放行
        let mut buf = BytesMut::from("hello\r\ncd '/ro");
        let sendable = cd_gate(&mut buf, &probes);
        assert_eq!(sendable, 7, "hello\\r\\n 放行，半个 probe 扣留");
        let mut hold = buf.split_off(sendable);
        assert_eq!(&buf[..], &BytesMut::from("hello\r\n")[..]);
        // 下一轮 flush：拼回扣留 + 新字节，整行替换
        let mut next = std::mem::take(&mut hold);
        next.extend_from_slice("ot'\r\nPS1".as_bytes());
        let sendable = cd_gate(&mut next, &probes);
        assert_eq!(sendable, next.len());
        assert_eq!(&next[..], &BytesMut::from("\r\x1b[KPS1")[..]);
    }

    #[test]
    fn cd_gate_withholds_complete_probe_without_newline() {
        let probes = cd_probes(&[b"cd '/root'"]);
        let mut buf = BytesMut::from("cd '/root'");
        assert_eq!(cd_gate(&mut buf, &probes), 0, "行尾换行未到，整行扣留");
        buf.extend_from_slice("\r\nPS1".as_bytes());
        let sendable = cd_gate(&mut buf, &probes);
        assert_eq!(sendable, buf.len());
        assert_eq!(&buf[..], &BytesMut::from("\r\x1b[KPS1")[..]);
    }

    #[test]
    fn cd_gate_strips_both_echo_copies() {
        // 同一条 cd 被回显两次（内核 + readline）：两行都替换，清行序列不能互相吃掉
        let probes = cd_probes(&[b"cd '/root'"]);
        let mut buf = BytesMut::from("cd '/root'\r\ncd '/root'\r\nPS1");
        let sendable = cd_gate(&mut buf, &probes);
        assert_eq!(sendable, buf.len());
        assert_eq!(&buf[..], &BytesMut::from("\r\x1b[K\r\x1b[KPS1")[..]);
    }

    #[test]
    fn cd_gate_multiple_probes_strip_their_own_rows() {
        // 连续快速切目录：两条 probe 一次裁决各自剥行（原始快照定边界）
        let probes = cd_probes(&[b"cd '/a'", b"cd '/b'"]);
        let mut buf = BytesMut::from("cd '/a'\r\ncd '/b'\r\nPS1");
        let sendable = cd_gate(&mut buf, &probes);
        assert_eq!(sendable, buf.len());
        assert_eq!(&buf[..], &BytesMut::from("\r\x1b[K\r\x1b[KPS1")[..]);
    }

    #[test]
    fn cd_gate_passthrough_without_probe_and_empty_probe() {
        let probes = cd_probes(&[b"cd '/root'"]);
        let mut buf = BytesMut::from("ls\r\nfile1\r\n");
        let snapshot = buf.clone();
        assert_eq!(cd_gate(&mut buf, &probes), buf.len());
        assert_eq!(&buf[..], &snapshot[..], "不含 probe 原样放行");
        assert_eq!(cd_gate(&mut buf, &[]), buf.len());
        assert_eq!(&buf[..], &snapshot[..], "空 probe 表绝不错删");
        let empty = cd_probes(&[b""]);
        assert_eq!(cd_gate(&mut buf, &empty), buf.len());
        assert_eq!(&buf[..], &snapshot[..], "空 probe 绝不错删");
    }

    #[test]
    fn utf8_locale_fallback_block_present_before_user_env() {
        // vim 乱码回归守卫：init 脚本必须带 UTF-8 locale 候选探测 + LC_ALL
        // 保底，且整段位于用户自定义 env 之前（LC_ALL 优先级恒压其后注入的 LANG）
        let script = init_script("/tmp/.ri-test", false, &[("EDITOR".into(), "nvim".into())]);

        // 候选列表：C.UTF-8 系列优先（内置/语言中性），en_US 兜底旧版
        // CentOS/RHEL/Amazon Linux；两种拼写都试。顺序必须固定。
        let c_utf8 = script.find("C.UTF-8").expect("应有 C.UTF-8 候选");
        let c_utf8_lower = script.find("C.utf8").expect("应有 C.utf8 候选");
        let en_us = script
            .find("en_US.UTF-8")
            .expect("应有 en_US.UTF-8 候选（CentOS 兜底）");
        let en_us_lower = script.find("en_US.utf8").expect("应有 en_US.utf8 候选");
        assert!(
            c_utf8 < c_utf8_lower && c_utf8_lower < en_us && en_us < en_us_lower,
            "候选探测顺序应为 C.UTF-8 C.utf8 en_US.UTF-8 en_US.utf8"
        );

        // charmap 判定放宽 UTF-?8（兼容输出 UTF8 的实现），且为实测 grep
        assert!(
            script.contains("grep -qiE '^UTF-?8$'"),
            "charmap 应按 UTF-?8 实测匹配"
        );

        // 无 locale 命令 / 残损 busybox locale（charmap 无输出）两路兜底
        assert!(
            script.contains("command -v locale"),
            "应覆盖无 locale 命令的系统"
        );
        assert!(
            script.contains("[ -z \"$(locale charmap 2>/dev/null)\" ]"),
            "应覆盖有 locale 命令但 charmap 无输出的 busybox 残损实现"
        );

        // 保底经 _rh_l 变量 export LC_ALL，而非写死某个名字（候选可能落空）
        let export_lc = script
            .find("export LC_ALL=\"$_rh_l\"")
            .expect("应只在探测到可用 locale 后按 _rh_l export LC_ALL");
        let env_line = script.find("export EDITOR='nvim'").unwrap();
        assert!(export_lc < env_line, "locale 保底必须在用户 env 之前");

        // 探测整段只在「当前不是 UTF-8」时执行（外层 if ! ... grep UTF-?8）
        assert!(
            script.contains(
                "if ! printf '%s' \"$(locale charmap 2>/dev/null)\" | grep -qiE '^UTF-?8$'"
            ),
            "外层必须是「当前非 UTF-8 才探测」，已是 UTF-8 时零改动"
        );
        // 绝不硬写 LANG（只统一字符类别，语言/地区偏好留给用户）
        assert!(!script.contains("export LANG="), "不应硬编码 LANG");
        // 不能出现把字符集名当 locale 名的错误用法
        assert!(
            !script.contains("LC_ALL=UTF-8 "),
            "UTF-8 不是合法 locale 名，不能直接设"
        );
        // 临时变量用完即清
        assert!(script.contains("unset _rh_l _rh_c"), "应清理临时变量");
    }

    #[test]
    fn locale_login_cmd_uses_candidate_pick_and_safe_fallback() {
        let cmd = locale_login_cmd("/bin/bash");
        // exec 路径复用同一候选探测（含 CentOS 兜底候选与放宽匹配）
        assert!(cmd.contains("en_US.UTF-8"), "exec 路径也应探测 en_US.UTF-8");
        assert!(
            cmd.contains("grep -qiE '^UTF-?8$'"),
            "exec 路径应实测 charmap"
        );
        // 命中：带 LC_ALL 前缀 exec；未命中：else 分支行首裸 exec（不塞无效 locale）
        assert!(
            cmd.contains("LC_ALL=\"$_rh_l\" exec \"$_rh_s\" -l"),
            "命中候选时应带 LC_ALL 前缀 exec"
        );
        assert!(
            cmd.contains("\n  exec \"$_rh_s\" -l"),
            "无候选时 else 分支应行首裸 exec"
        );
        // 探测到的 shell 路径先校验可执行再 exec（passwd 值可能被启动告警污染，
        // 直接 exec 得 127 秒断），不可执行时回退 $SHELL 与常见 shell
        assert!(
            cmd.contains("_rh_s='/bin/bash'"),
            "shell 路径应先赋给受校验的变量"
        );
        assert!(
            cmd.contains("[ ! -x \"$_rh_s\" ]"),
            "exec 前应校验 shell 可执行"
        );
        assert!(cmd.contains("/bin/sh"), "不可执行时应回退到 /bin/sh");
        // shell 路径经单引号转义后安全内插
        let evil = locale_login_cmd("/tmp/x'bash");
        assert!(
            evil.contains("_rh_s='/tmp/x'\\''bash'"),
            "shell 单引号应被转义"
        );
        assert!(!evil.contains("LC_ALL=UTF-8 "), "不得把 UTF-8 当 locale 名");
    }
}
