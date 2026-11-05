//! SSH 密钥的落盘生成与导入（密钥面板的「生成密钥」「导入密钥」）。
//!
//! 走系统 `ssh-keygen` 而不是引入纯 Rust 密钥库：产物与用户在终端里手敲
//! `ssh-keygen` 完全一致（OpenSSH 私钥格式、口令加密、`.pub` 伴生文件、
//! SHA256 指纹），连接侧 `russh::keys::load_secret_key` 直接可用；macOS/Linux
//! 自带，Windows 10+ 默认安装 OpenSSH 客户端。缺失时返回明确错误，不做静默降级。
//!
//! 口令只经进程参数传递，绝不写入日志与返回值；两个入参结构因此手工实现
//! `Debug`（脱敏），防止后续有人补 `#[derive(Debug)]` 后不小心打印。

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// 密钥面板支持的算法（与前端 `KeyType` 联合类型同名同值）
const SUPPORTED_TYPES: [&str; 3] = ["ED25519", "RSA", "ECDSA"];

/// 生成/导入的返回：前端 `SshKey` 所需的全部真实字段
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyInfo {
    /// `ssh-ed25519 AAAA...` 一行（不含 comment）
    pub public_key: String,
    /// `SHA256:...`（与终端 `ssh-keygen -lf` 一致）
    pub fingerprint: String,
    /// 'ED25519' | 'RSA' | 'ECDSA'
    pub key_type: String,
    /// 十进制字符串，如 '4096' / '521' / '256'
    pub bits: String,
    pub comment: String,
    /// 私钥绝对路径；仅导入公钥时为 None
    pub private_path: Option<String>,
    /// 私钥的 `~/.ssh/...` 展示形式（列表按此显示，连接时按此展开）
    pub private_tilde: Option<String>,
    /// 公钥文件绝对路径
    pub public_path: String,
}

/// 生成入参。`bits` 缺省按算法取安全默认（RSA 4096 / ECDSA 521 / ED25519 固定 256）
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeygenInput {
    name: String,
    key_type: String,
    bits: Option<String>,
    comment: Option<String>,
    passphrase: Option<String>,
}

impl std::fmt::Debug for KeygenInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeygenInput")
            .field("name", &self.name)
            .field("key_type", &self.key_type)
            .field("bits", &self.bits)
            .field("passphrase", &"[已隐藏]")
            .finish()
    }
}

/// 导入入参：粘贴/读取到的密钥文本（私钥 PEM 或公钥单行）
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyImportInput {
    name: String,
    text: String,
    passphrase: Option<String>,
}

impl std::fmt::Debug for KeyImportInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyImportInput")
            .field("name", &self.name)
            .field("text_len", &self.text.len())
            .field("passphrase", &"[已隐藏]")
            .finish()
    }
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "无法确定用户主目录".to_string())
}

/// 把绝对路径转成 `~/.ssh/xxx` 展示形式（与既有密钥数据保持一致）
fn to_tilde(path: &PathBuf) -> String {
    match home_dir() {
        Ok(home) => match path.strip_prefix(&home) {
            Ok(rel) => format!("~/{}", rel.display()),
            Err(_) => path.display().to_string(),
        },
        Err(_) => path.display().to_string(),
    }
}

/// 文件名 slug：只留字母数字/下划线/连字符/汉字，其余转下划线，
/// 避免路径穿越与 shell 元字符进入文件名
fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let out = out.trim_matches('_');
    if out.is_empty() {
        "key".to_string()
    } else if out.chars().count() > 48 {
        out.chars().take(48).collect()
    } else {
        out.to_string()
    }
}

/// 在 `~/.ssh` 下取一个尚未占用的基名（已存在则追加 `-2`、`-3`…）。
/// 私钥绝不覆盖：同名多半意味着用户想保留旧文件。
fn free_path(dir: &PathBuf, base: &str) -> PathBuf {
    let mut candidate = dir.join(base);
    let mut n = 2;
    while candidate.exists() || pub_side(&candidate).exists() {
        candidate = dir.join(format!("{base}-{n}"));
        n += 1;
    }
    candidate
}

fn pub_side(path: &PathBuf) -> PathBuf {
    let s = path.display().to_string();
    if s.ends_with(".pub") {
        PathBuf::from(s)
    } else {
        PathBuf::from(format!("{s}.pub"))
    }
}

/// 运行 ssh-keygen：stdin 挂空（文件已存在等场景下它会转入交互询问，
/// 无 tty 时永久挂起），输出捕获，非零退出码转成 stderr 首行文本
fn run_ssh_keygen(args: &[&str]) -> Result<String, String> {
    let out = Command::new("ssh-keygen")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("调用 ssh-keygen 失败（请确认系统已安装 OpenSSH 客户端）: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // 只取首行：ssh-keygen 失败时会把口令提示等后续行一起打到 stderr
        let brief = err.lines().next().unwrap_or("未知错误").trim();
        return Err(format!("ssh-keygen: {brief}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 解析 `ssh-keygen -lf x.pub` 输出：`BITS SHA256:... comment (TYPE)`
fn parse_lf(out: &str) -> Result<(String, String, String, String), String> {
    let mut it = out.split_whitespace();
    let bits = it.next().ok_or_else(|| "指纹输出为空".to_string())?;
    let fp = it
        .next()
        .ok_or_else(|| format!("无法解析指纹输出: {out}"))?;
    if !fp.starts_with("SHA256:") {
        return Err(format!("无法解析指纹输出: {out}"));
    }
    // 尾部 `(RSA)` / `(ED25519)` / `(ECDSA-SHA2-NISTP384)`
    let tail = out
        .rsplit_once('(')
        .map(|(_, rest)| rest.trim_end_matches(')'))
        .unwrap_or("");
    let key_type = if tail.starts_with("ED25519") {
        "ED25519"
    } else if tail.starts_with("ECDSA") {
        "ECDSA"
    } else if tail.starts_with("RSA") {
        "RSA"
    } else {
        return Err(format!("不支持的密钥类型: {tail}"));
    };
    // comment = 去掉首两个字段与尾部 (TYPE) 后的中间部分
    let comment = out
        .strip_prefix(&format!("{bits} {fp} "))
        .and_then(|s| s.split_once(format!(" ({tail})").as_str()))
        .map(|(c, _)| c.trim().to_string())
        .unwrap_or_default();
    Ok((bits.to_string(), fp.to_string(), key_type.to_string(), comment))
}

#[cfg(unix)]
fn set_mode(path: &PathBuf, mode: u32) -> Result<(), String> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("设置 {} 权限失败: {e}", path.display()))
}

/// Windows 无 POSIX 权限位；OpenSSH for Windows 按 ACL 处理，此处不额外设置
#[cfg(not(unix))]
fn set_mode(_path: &PathBuf, _mode: u32) -> Result<(), String> {
    Ok(())
}

/// 新建文件写入（已存在则失败，绝不覆盖）并设权限（私钥 0600、公钥 0644）
fn write_key_file(path: &PathBuf, content: &str, mode: u32) -> Result<(), String> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
    f.write_all(content.as_bytes())
        .map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
    f.flush().ok();
    set_mode(path, mode)
}

fn ssh_dir() -> Result<PathBuf, String> {
    let dir = home_dir()?.join(".ssh");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;
    // 0700：权限过宽的目录会被 sshd 拒绝（authorized_keys 同目录）
    set_mode(&dir, 0o700)?;
    Ok(dir)
}

/// 写入公钥文件并解析出（位数, 指纹, 类型, comment）
fn write_and_parse_pub(
    pub_path: &PathBuf,
    pub_line: &str,
) -> Result<(String, String, String, String), String> {
    write_key_file(pub_path, &format!("{pub_line}\n"), 0o644)?;
    let lf = run_ssh_keygen(&["-E", "sha256", "-lf", &pub_path.display().to_string()])?;
    parse_lf(&lf)
}

/// 公钥行的 comment 段（`ssh-ed25519 <base64> <comment>`）
fn pub_comment(pub_line: &str) -> String {
    pub_line
        .split_whitespace()
        .nth(2)
        .unwrap_or("imported")
        .to_string()
}

/// 生成密钥对：`ssh-keygen -t <alg> -f ~/.ssh/<slug> -C <comment> -N <口令> [-b <bits>]`
#[tauri::command]
pub async fn ssh_keygen(input: KeygenInput) -> Result<KeyInfo, String> {
    // 外部进程放到阻塞线程池，避免占住异步执行器
    tokio::task::spawn_blocking(move || ssh_keygen_sync(input))
        .await
        .map_err(|e| format!("密钥生成任务失败: {e}"))?
}

fn ssh_keygen_sync(input: KeygenInput) -> Result<KeyInfo, String> {
    let alg = input.key_type.trim().to_ascii_uppercase();
    if !SUPPORTED_TYPES.contains(&alg.as_str()) {
        return Err(format!("不支持的密钥类型: {}", input.key_type));
    }
    let dir = ssh_dir()?;
    let base = format!("id_{}", slug(&input.name));
    let priv_path = free_path(&dir, &base);
    let pub_path = pub_side(&priv_path);
    let priv_str = priv_path.display().to_string();
    let user = std::env::var("USER").unwrap_or_else(|_| "rhost".into());
    let default_comment = format!("{user}@localhost");
    let comment = match input.comment.map(|c| c.trim().to_string()) {
        Some(c) if !c.is_empty() => c,
        _ => default_comment,
    };
    let passphrase = input.passphrase.unwrap_or_default();
    // RSA/ECDSA 位数缺省取各自的安全上限默认值（ED25519 位数固定，不传 -b）
    let bits_arg = if alg == "ED25519" {
        None
    } else {
        Some(match input.bits.as_deref().map(str::trim) {
            Some(b) if b.parse::<u32>().is_ok_and(|v| v > 0) => b.to_string(),
            _ => match alg.as_str() {
                "RSA" => "4096".to_string(),
                _ => "521".to_string(),
            },
        })
    };

    let mut args: Vec<&str> =
        vec!["-q", "-t", &alg, "-f", &priv_str, "-C", &comment];
    if let Some(b) = bits_arg.as_deref() {
        args.extend(["-b", b]);
    }
    // 空口令 = -N ""：生成明文私钥；非空由 ssh-keygen 用 bcrypt+AES 加密
    args.extend(["-N", &passphrase]);

    run_ssh_keygen(&args)?;
    // ssh-keygen 已按 umask 设权限，这里强制 0600 保证一致
    set_mode(&priv_path, 0o600)?;

    let pub_line = read_pub_line(&pub_path)?;
    let pub_str = pub_path.display().to_string();
    let lf = run_ssh_keygen(&["-E", "sha256", "-lf", &pub_str])?;
    let (bits, fp, key_type, lf_comment) = parse_lf(&lf)?;
    Ok(KeyInfo {
        public_key: pub_line,
        fingerprint: fp,
        key_type,
        bits,
        comment: if lf_comment.is_empty() { comment } else { lf_comment },
        private_path: Some(priv_str),
        private_tilde: Some(to_tilde(&priv_path)),
        public_path: pub_path.display().to_string(),
    })
}

fn read_pub_line(pub_path: &PathBuf) -> Result<String, String> {
    let text = std::fs::read_to_string(pub_path)
        .map_err(|e| format!("读取 {} 失败: {e}", pub_path.display()))?;
    text.lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .ok_or_else(|| "公钥文件为空".to_string())
}

/// 导入密钥：私钥 PEM（可选口令）或公钥单行。校验通过后写入 `~/.ssh/`，
/// 返回真实指纹与公钥——连接时按私钥路径加载即可用。
#[tauri::command]
pub async fn ssh_key_import(input: KeyImportInput) -> Result<KeyInfo, String> {
    tokio::task::spawn_blocking(move || ssh_key_import_sync(input))
        .await
        .map_err(|e| format!("密钥导入任务失败: {e}"))?
}

fn ssh_key_import_sync(input: KeyImportInput) -> Result<KeyInfo, String> {
    let text = input.text.trim();
    if text.is_empty() {
        return Err("密钥内容为空".to_string());
    }
    let dir = ssh_dir()?;
    let is_public =
        !text.contains("BEGIN") && (text.starts_with("ssh-") || text.starts_with("ecdsa-sha2-"));

    if is_public {
        let pub_line = text
            .lines()
            .find(|l| l.starts_with("ssh-") || l.starts_with("ecdsa-sha2-"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let pub_path = free_path(&dir, &format!("{}.pub", slug(&input.name)));
        let (bits, fingerprint, key_type, lf_comment) =
            write_and_parse_pub(&pub_path, &pub_line)?;
        return Ok(KeyInfo {
            comment: if lf_comment.is_empty() { pub_comment(&pub_line) } else { lf_comment },
            public_key: pub_line,
            fingerprint,
            key_type,
            bits,
            private_path: None,
            private_tilde: None,
            public_path: pub_path.display().to_string(),
        });
    }

    // 私钥：先落盘（0600）→ ssh-keygen -y 提取公钥（顺带校验格式与口令）→ 写 .pub → 取指纹
    let priv_path = free_path(&dir, &slug(&input.name));
    let pub_path = pub_side(&priv_path);
    let priv_str = priv_path.display().to_string();
    let mut pem = text.to_string();
    if !pem.ends_with('\n') {
        pem.push('\n');
    }
    write_key_file(&priv_path, &pem, 0o600)?;

    let passphrase = input.passphrase.unwrap_or_default();
    let pub_line = match run_ssh_keygen(&["-y", "-P", &passphrase, "-f", &priv_str]) {
        Ok(line) => line,
        Err(e) => {
            // 校验失败要回收刚写入的文件，避免 ~/.ssh 留下不可用的私钥
            let _ = std::fs::remove_file(&priv_path);
            let hint = if e.to_lowercase().contains("passphrase") {
                "（私钥已加密，请填写口令后重试）"
            } else {
                ""
            };
            return Err(format!("{e}{hint}"));
        }
    };
    let (bits, fingerprint, key_type, lf_comment) = write_and_parse_pub(&pub_path, &pub_line)?;
    Ok(KeyInfo {
        comment: if lf_comment.is_empty() { pub_comment(&pub_line) } else { lf_comment },
        public_key: pub_line,
        fingerprint,
        key_type,
        bits,
        private_path: Some(priv_str),
        private_tilde: Some(to_tilde(&priv_path)),
        public_path: pub_path.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_blocks_traversal_and_spaces() {
        assert_eq!(slug("../../etc/passwd"), "etc_passwd");
        assert_eq!(slug("my key; rm -rf"), "my_key__rm_-rf");
        assert_eq!(slug(""), "key");
        assert_eq!(slug("***"), "key");
    }

    #[test]
    fn slug_keeps_cjk_and_caps_length() {
        assert_eq!(slug("生产跳板"), "生产跳板");
        assert_eq!(slug(&"a".repeat(60)).chars().count(), 48);
    }

    #[test]
    fn parse_lf_reads_keygen_output() {
        let (bits, fp, t, c) = parse_lf(
            "3072 SHA256:AbCdEfGhIjKlMnOpQrStUvWxYz0123456789abcdw user@host (RSA)",
        )
        .expect("应解析成功");
        assert_eq!(bits, "3072");
        assert_eq!(fp, "SHA256:AbCdEfGhIjKlMnOpQrStUvWxYz0123456789abcdw");
        assert_eq!(t, "RSA");
        assert_eq!(c, "user@host");

        let (_, _, t2, c2) = parse_lf("256 SHA256:xyz (ED25519)").unwrap();
        assert_eq!(t2, "ED25519");
        assert_eq!(c2, "");
        let (_, _, t3, c3) =
            parse_lf("521 SHA256:abc no comment here (ECDSA-SHA2-NISTP521)").unwrap();
        assert_eq!(t3, "ECDSA");
        assert_eq!(c3, "no comment here");

        assert!(parse_lf("garbage").is_err());
        assert!(parse_lf("2048 MD5:zzz x (RSA)").is_err());
    }

    #[test]
    fn pub_side_avoids_double_suffix() {
        assert_eq!(pub_side(&PathBuf::from("/x/id_rsa")).to_str(), Some("/x/id_rsa.pub"));
        assert_eq!(pub_side(&PathBuf::from("/x/k.pub")).to_str(), Some("/x/k.pub"));
    }

    #[test]
    fn input_debug_does_not_leak_passphrase() {
        let i = KeygenInput {
            name: "k".into(),
            key_type: "ED25519".into(),
            bits: None,
            comment: None,
            passphrase: Some("s3cr3t".into()),
        };
        assert!(!format!("{i:?}").contains("s3cr3t"));
        let j = KeyImportInput {
            name: "k".into(),
            text: "-----BEGIN OPENSSH PRIVATE KEY-----".into(),
            passphrase: Some("s3cr3t".into()),
        };
        assert!(!format!("{j:?}").contains("s3cr3t"));
    }

    #[test]
    fn to_tilde_maps_home_prefix() {
        let home = home_dir().expect("测试环境应有 HOME");
        assert_eq!(to_tilde(&home.join(".ssh/id_demo")), "~/.ssh/id_demo");
    }

    #[test]
    fn pub_comment_falls_back() {
        assert_eq!(pub_comment("ssh-ed25519 AAAA alice@laptop"), "alice@laptop");
        assert_eq!(pub_comment("ssh-ed25519 AAAA"), "imported");
    }
}
