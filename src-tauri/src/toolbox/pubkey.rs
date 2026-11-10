//! 公钥下发：把本地公钥追加到远端用户 `~/.ssh/authorized_keys`。
//!
//! 公钥文本先经过严格校验（算法白名单、base64 字符集、单行、长度上限），
//! 再由后端按字段重建脚本，前端原文绝不直接拼进 shell。目标用户必须是
//! POSIX 风格用户名，home 目录通过 `getent passwd` / `/etc/passwd` 解析。
//!
//! 写入当前登录用户不需要提权；写入其他用户时非 root 会返回
//! [`SUDO_REQUIRED`] 交给前端弹窗，以 root 登录则直接写入并修正属主。

use crate::ssh::session::ExecOutput;

use super::{sq, SUDO_REQUIRED};

/// 支持的公钥算法（与 OpenSSH `authorized_keys` 常用类型一致）
const KEY_TYPES: [&str; 7] = [
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// authorized_keys 单行上限；正常密钥远小于此值（RSA 4096 约 700 字符）
const MAX_KEY_LEN: usize = 8192;

/// 校验并规范化公钥，返回（整行, 算法, base64 主体）。
pub(super) fn normalize_key(input: &str) -> Result<(String, String, String), String> {
    let text = input.trim();
    if text.is_empty() {
        return Err("公钥内容为空".into());
    }
    if text.len() > MAX_KEY_LEN {
        return Err("公钥内容过长".into());
    }
    if text.contains(['\n', '\r', '\0']) {
        return Err("公钥必须是单行文本".into());
    }
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() < 2 {
        return Err("公钥格式不合法（应形如 ssh-ed25519 AAAA...）".into());
    }
    let key_type = parts[0];
    if !KEY_TYPES.contains(&key_type) {
        return Err(format!("不支持的密钥类型：{key_type}"));
    }
    let body = parts[1];
    if body.len() < 16
        || !body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        || body.starts_with('=')
    {
        return Err("公钥 base64 内容不合法".into());
    }
    let comment = parts[2..].join(" ");
    if comment.chars().any(|c| c.is_control()) {
        return Err("公钥注释包含不可见控制字符".into());
    }
    let line = if comment.is_empty() {
        format!("{key_type} {body}")
    } else {
        format!("{key_type} {body} {comment}")
    };
    Ok((line, key_type.to_string(), body.to_string()))
}

/// 目标用户名校验：POSIX 风格，禁止前导 `-` 与 shell 特殊字符。
pub(super) fn validate_user(input: &str) -> Result<String, String> {
    let user = input.trim();
    if user.is_empty() {
        return Err("请填写目标用户".into());
    }
    if user.len() > 32 {
        return Err("目标用户名过长".into());
    }
    let mut chars = user.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err("目标用户不合法（应以字母或下划线开头）".into());
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
        return Err("目标用户不合法（仅支持字母、数字、下划线、连字符和点）".into());
    }
    Ok(user.to_string())
}

/// 生成远端写入脚本。脚本自行判断是否需要提权：
/// - 当前 root：直接写入并修正目标用户属主；
/// - 非 root 且目标用户不是自己：输出 [`SUDO_REQUIRED`]，由前端问密码后重试。
pub(super) fn build_script(user: &str, key_type: &str, body: &str, line: &str) -> String {
    format!(
        r#"u={u}
kt={kt}
kb={kb}
kl={kl}
home=$(getent passwd "$u" 2>/dev/null | cut -d: -f6)
if [ -z "$home" ]; then
  home=$(awk -F: -v u="$u" '$1==u {{print $6}}' /etc/passwd 2>/dev/null)
fi
if [ -z "$home" ] || [ "$home" = "/" ] || [ ! -d "$home" ]; then
  echo 'PUBKEY_USER_NOT_FOUND' >&2
  exit 3
fi
if [ "$(id -u)" != "0" ] && [ "$(id -un 2>/dev/null)" != "$u" ]; then
  echo '{sentinel}'
  exit 9
fi
ssh_dir="$home/.ssh"
ak="$ssh_dir/authorized_keys"
mkdir -p "$ssh_dir" 2>/dev/null || {{ echo 'PUBKEY_PERMISSION_DENIED' >&2; exit 4; }}
chmod 700 "$ssh_dir" 2>/dev/null
if [ -f "$ak" ] && awk -v kt="$kt" -v kb="$kb" '($1==kt && $2==kb) {{found=1}} END {{exit !found}}' "$ak"; then
  echo "PUBKEY_EXISTS $ak"
  exit 0
fi
if [ -f "$ak" ]; then
  if [ -s "$ak" ] && [ "$(tail -c 1 "$ak" | od -An -tx1 | tr -d ' \n')" != "0a" ]; then
    printf '\n' >> "$ak" 2>/dev/null || {{ echo 'PUBKEY_PERMISSION_DENIED' >&2; exit 4; }}
  fi
else
  : > "$ak" 2>/dev/null || {{ echo 'PUBKEY_PERMISSION_DENIED' >&2; exit 4; }}
fi
printf '%s\n' "$kl" >> "$ak" 2>/dev/null || {{ echo 'PUBKEY_PERMISSION_DENIED' >&2; exit 4; }}
chmod 600 "$ak" 2>/dev/null
if [ "$(id -u)" = "0" ] && [ "$u" != "root" ]; then
  group=$(getent passwd "$u" 2>/dev/null | cut -d: -f4)
  if [ -z "$group" ]; then
    group=$(awk -F: -v u="$u" '$1==u {{print $4}}' /etc/passwd 2>/dev/null)
  fi
  if [ -n "$group" ]; then
    chown -R "$u:$group" "$ssh_dir" 2>/dev/null || chown -R "$u" "$ssh_dir" 2>/dev/null || true
  else
    chown -R "$u" "$ssh_dir" 2>/dev/null || true
  fi
fi
command -v restorecon >/dev/null 2>&1 && restorecon -R "$ssh_dir" >/dev/null 2>&1 || true
echo "PUBKEY_ADDED $ak"
"#,
        u = sq(user),
        kt = sq(key_type),
        kb = sq(body),
        kl = sq(line),
        sentinel = SUDO_REQUIRED,
    )
}

/// 写入结果
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PubkeyDeploy {
    /// 实际写入的 authorized_keys 绝对路径
    pub path: String,
    /// false 表示该公钥已存在，未重复追加
    pub added: bool,
}

/// 解析脚本输出；失败信息对用户可读，且不会带出公钥内容。
pub(super) fn deploy_result(out: &ExecOutput) -> Result<PubkeyDeploy, String> {
    if out.output.contains(SUDO_REQUIRED) {
        return Err(SUDO_REQUIRED.into());
    }
    if out.timed_out {
        return Err("写入公钥超时，请稍后重试".into());
    }
    if let Some(path) = out
        .output
        .lines()
        .find_map(|l| l.trim().strip_prefix("PUBKEY_ADDED "))
    {
        return Ok(PubkeyDeploy {
            path: path.trim().to_string(),
            added: true,
        });
    }
    if let Some(path) = out
        .output
        .lines()
        .find_map(|l| l.trim().strip_prefix("PUBKEY_EXISTS "))
    {
        return Ok(PubkeyDeploy {
            path: path.trim().to_string(),
            added: false,
        });
    }
    if out.output.contains("PUBKEY_USER_NOT_FOUND") {
        return Err("远端不存在该用户或无法解析其 home 目录".into());
    }
    if out.output.contains("PUBKEY_PERMISSION_DENIED") {
        return Err("没有权限写入目标用户的 .ssh 目录".into());
    }
    let detail = out.output.trim();
    if detail.is_empty() {
        Err(format!(
            "写入公钥失败（退出码 {:?}）",
            out.exit_code.unwrap_or(-1)
        ))
    } else {
        Err(format!("写入公钥失败：{detail}"))
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

    #[test]
    fn normalizes_key_with_comment() {
        let (line, kt, body) =
            normalize_key("  ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILkQ alice@host \n").unwrap();
        assert_eq!(kt, "ssh-ed25519");
        assert_eq!(body, "AAAAC3NzaC1lZDI1NTE5AAAAILkQ");
        assert_eq!(line, "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILkQ alice@host");
    }

    #[test]
    fn normalizes_key_without_comment() {
        let (line, _, _) =
            normalize_key("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQ").unwrap();
        assert_eq!(line, "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQ");
    }

    #[test]
    fn rejects_bad_keys() {
        assert!(normalize_key("").is_err());
        assert!(normalize_key("ssh-ed25519").is_err());
        assert!(normalize_key("ssh-dss AAAAB3NzaC1kc3M").is_err());
        assert!(normalize_key("ssh-ed25519 not_base64!!").is_err());
        assert!(normalize_key("ssh-ed25519 AAAAAAAA\nssh-rsa BBBB").is_err());
        assert!(normalize_key(&format!("ssh-ed25519 {}", "A".repeat(MAX_KEY_LEN))).is_err());
    }

    #[test]
    fn validates_users() {
        assert_eq!(validate_user("root").unwrap(), "root");
        assert_eq!(validate_user("deploy").unwrap(), "deploy");
        assert_eq!(validate_user("_svc-1.x").unwrap(), "_svc-1.x");
        assert!(validate_user("").is_err());
        assert!(validate_user("-root").is_err());
        assert!(validate_user("a b").is_err());
        assert!(validate_user("a;rm").is_err());
        assert!(validate_user("1abc").is_err());
    }

    #[test]
    fn script_quotes_user_and_key() {
        let s = build_script("o'brien", "ssh-ed25519", "AAAA", "ssh-ed25519 AAAA o'b");
        assert!(s.contains("u='o'\\''brien'"));
        assert!(s.contains("kl='ssh-ed25519 AAAA o'\\''b'"));
        assert!(s.contains(SUDO_REQUIRED));
    }

    #[test]
    fn parses_added_and_exists() {
        let r = deploy_result(&out("PUBKEY_ADDED /home/alice/.ssh/authorized_keys\n", 0)).unwrap();
        assert!(r.added);
        assert_eq!(r.path, "/home/alice/.ssh/authorized_keys");

        let r = deploy_result(&out("PUBKEY_EXISTS /root/.ssh/authorized_keys\n", 0)).unwrap();
        assert!(!r.added);
        assert_eq!(r.path, "/root/.ssh/authorized_keys");
    }

    #[test]
    fn maps_errors() {
        assert_eq!(
            deploy_result(&out(SUDO_REQUIRED, 9)).unwrap_err(),
            SUDO_REQUIRED
        );
        assert!(deploy_result(&out("PUBKEY_USER_NOT_FOUND", 3))
            .unwrap_err()
            .contains("不存在该用户"));
        assert!(deploy_result(&out("PUBKEY_PERMISSION_DENIED", 4))
            .unwrap_err()
            .contains("没有权限"));
        let timeout = ExecOutput {
            output: String::new(),
            exit_code: None,
            timed_out: true,
        };
        assert!(deploy_result(&timeout).unwrap_err().contains("超时"));
    }
}
