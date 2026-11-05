//! 本地文件系统列举：供 SFTP 面板"本地"栏展示真实目录内容。

use serde::Serialize;

/// 单个目录项（字段与前端 FsEntry 对应）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalFsEntry {
    pub name: String,
    pub is_dir: bool,
    /// 是否为符号链接（file_type 不跟随；is_dir 经 metadata 跟随判断）
    pub is_symlink: bool,
    /// 文件字节数；目录恒为 0（前端显示 —）
    pub size: u64,
    /// 本地时间 "YYYY-MM-DD HH:MM"；取不到为空串
    pub mtime: String,
    /// 权限字符串 "rwxr-xr-x"（不输出八进制）
    pub perm: String,
    /// 属主名（「用户」列）；Windows 下为空串
    pub owner_user: String,
    /// 属组名（「用户组」列）；Windows 下为空串
    pub owner_group: String,
    /// 类型位，与远端 `ls -l` 首字符同口径：`d`/`l`/`-`/`c`/`b`/`p`/`s`
    pub kind: String,
}

/// 一次目录列举结果：带回实际路径（path 缺省时解析为 home）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalDirListing {
    pub path: String,
    pub entries: Vec<LocalFsEntry>,
}

/// 解析 home 目录（不引入 dirs crate：Unix 读 HOME，Windows 读 USERPROFILE）
fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .filter(|s| !s.is_empty())
}

fn fmt_mtime(md: &std::fs::Metadata) -> String {
    match md.modified() {
        Ok(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        }
        Err(_) => String::new(),
    }
}

/// 权限位 → "rwxr-xr-x"（含 setuid/setgid/sticky 的 s/S/t/T 惯例）
#[cfg(unix)]
fn fmt_perm(md: &std::fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    let mode = md.permissions().mode();
    let mut s = String::with_capacity(9);
    for (bit, chr) in [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ] {
        s.push(if mode & bit != 0 { chr } else { '-' });
    }
    if mode & 0o4000 != 0 {
        let i = 2; // user x 位
        s.replace_range(i..=i, if mode & 0o100 != 0 { "s" } else { "S" });
    }
    if mode & 0o2000 != 0 {
        let i = 5; // group x 位
        s.replace_range(i..=i, if mode & 0o010 != 0 { "s" } else { "S" });
    }
    if mode & 0o1000 != 0 {
        let i = 8; // other x 位
        s.replace_range(i..=i, if mode & 0o001 != 0 { "t" } else { "T" });
    }
    s
}

#[cfg(not(unix))]
fn fmt_perm(_md: &std::fs::Metadata) -> String {
    "---------".to_string()
}

/// uid → 用户名。libc 反查，同目录大量条目属主重复，用缓存避免重复查询。
#[cfg(unix)]
fn fmt_user(
    md: &std::fs::Metadata,
    uid_cache: &mut std::collections::HashMap<u32, String>,
) -> String {
    use std::os::unix::fs::MetadataExt;
    let uid = md.uid();
    uid_cache
        .entry(uid)
        .or_insert_with(|| {
            let p = unsafe { libc::getpwuid(uid) };
            if p.is_null() {
                uid.to_string()
            } else {
                unsafe { std::ffi::CStr::from_ptr((*p).pw_name) }
                    .to_string_lossy()
                    .into_owned()
            }
        })
        .clone()
}

/// gid → 组名（同上，独立缓存）
#[cfg(unix)]
fn fmt_group(
    md: &std::fs::Metadata,
    gid_cache: &mut std::collections::HashMap<u32, String>,
) -> String {
    use std::os::unix::fs::MetadataExt;
    let gid = md.gid();
    gid_cache
        .entry(gid)
        .or_insert_with(|| {
            let p = unsafe { libc::getgrgid(gid) };
            if p.is_null() {
                gid.to_string()
            } else {
                unsafe { std::ffi::CStr::from_ptr((*p).gr_name) }
                    .to_string_lossy()
                    .into_owned()
            }
        })
        .clone()
}

#[cfg(not(unix))]
fn fmt_user(
    _md: &std::fs::Metadata,
    _u: &mut std::collections::HashMap<u32, String>,
) -> String {
    String::new()
}

#[cfg(not(unix))]
fn fmt_group(
    _md: &std::fs::Metadata,
    _g: &mut std::collections::HashMap<u32, String>,
) -> String {
    String::new()
}

/// 与远端 `ls -l` 首字符对齐的类型位；判断顺序遵循「不跟随链接」优先，
/// 使指向目录的软链显示为 `l`（前端另按 isDir 决定是否可进入）。
#[cfg(unix)]
fn kind_of(ft: &std::fs::FileType) -> String {
    use std::os::unix::fs::FileTypeExt;
    let c = if ft.is_symlink() {
        'l'
    } else if ft.is_dir() {
        'd'
    } else if ft.is_char_device() {
        'c'
    } else if ft.is_block_device() {
        'b'
    } else if ft.is_fifo() {
        'p'
    } else if ft.is_socket() {
        's'
    } else {
        '-'
    };
    c.to_string()
}

/// Windows 无设备/管道/套接字类型位可辨，只区分链接、目录与其余
#[cfg(not(unix))]
fn kind_of(ft: &std::fs::FileType) -> String {
    let c = if ft.is_symlink() {
        'l'
    } else if ft.is_dir() {
        'd'
    } else {
        '-'
    };
    c.to_string()
}

/// 列举本地目录。`path` 为 None/空串时用 home 目录（前端首次打开定位到用户主目录）。
/// 读目录可能耗时（网络盘/大目录），放阻塞线程池避免卡住异步运行时。
#[tauri::command]
pub async fn list_local_dir(path: Option<String>) -> Result<LocalDirListing, String> {
    crate::slow_span!("list_local_dir");
    let dir = match path {
        Some(p) if !p.is_empty() => p,
        _ => home_dir().ok_or_else(|| "无法确定用户主目录".to_string())?,
    };

    tauri::async_runtime::spawn_blocking(move || {
        let rd = std::fs::read_dir(&dir).map_err(|e| format!("无法读取目录 {dir}：{e}"))?;
        let mut entries = Vec::new();
        let mut uid_cache = std::collections::HashMap::new();
        let mut gid_cache = std::collections::HashMap::new();
        for item in rd {
            let Ok(ent) = item else { continue };
            let name = ent.file_name().to_string_lossy().into_owned();
            // file_type() 不跟随符号链接，用于识别软链；
            // metadata() 跟随链接，指向目录的快捷方式按目录处理
            let is_symlink = ent.file_type().map(|t| t.is_symlink()).unwrap_or(false);
            let kind = ent
                .file_type()
                .map(|t| kind_of(&t))
                .unwrap_or_else(|_| "-".to_string());
            let Ok(md) = ent.metadata() else { continue };
            let is_dir = md.is_dir();
            entries.push(LocalFsEntry {
                name,
                is_dir,
                is_symlink,
                size: if is_dir { 0 } else { md.len() },
                mtime: fmt_mtime(&md),
                perm: fmt_perm(&md),
                owner_user: fmt_user(&md, &mut uid_cache),
                owner_group: fmt_group(&md, &mut gid_cache),
                kind,
            });
        }
        Ok(LocalDirListing { path: dir, entries })
    })
    .await
    .map_err(|e| format!("目录列举任务失败：{e}"))?
}

/// 本地创建目录（单层，不递归）。
#[tauri::command]
pub async fn mkdir(path: String) -> Result<(), String> {
    crate::slow_span!("mkdir");
    tokio::fs::create_dir(&path)
        .await
        .map_err(|e| format!("创建目录 {path} 失败：{e}"))
}

/// 批量创建本地目录（mkdir -p 语义，已存在视为成功）。
/// 下载整棵远端目录树时前端一次传入骨架，空子目录也能复刻出来。
#[tauri::command]
pub async fn local_mkdir_all(paths: Vec<String>) -> Result<(), String> {
    crate::slow_span!("local_mkdir_all");
    for p in &paths {
        tokio::fs::create_dir_all(p)
            .await
            .map_err(|e| format!("创建目录 {p} 失败：{e}"))?;
    }
    Ok(())
}

/// 本地删除：文件/符号链接 → remove_file（软链只删链接本身，不触碰目标）；
/// 目录 → remove_dir_all 递归删除。用 symlink_metadata 判类型，避免跟随软链。
#[tauri::command]
pub async fn local_remove(path: String) -> Result<(), String> {
    crate::slow_span!("local_remove");
    let md = tokio::fs::symlink_metadata(&path)
        .await
        .map_err(|e| format!("获取 {path} 属性失败：{e}"))?;
    if md.is_dir() {
        tokio::fs::remove_dir_all(&path)
            .await
            .map_err(|e| format!("删除目录 {path} 失败：{e}"))
    } else {
        // 普通文件与符号链接均走 remove_file
        tokio::fs::remove_file(&path)
            .await
            .map_err(|e| format!("删除 {path} 失败：{e}"))
    }
}

/// 本地重命名/移动（同盘原子；跨卷由系统执行复制+删除）。
#[tauri::command]
pub async fn local_rename(old_path: String, new_path: String) -> Result<(), String> {
    crate::slow_span!("local_rename");
    tokio::fs::rename(&old_path, &new_path)
        .await
        .map_err(|e| format!("重命名 {old_path} → {new_path} 失败：{e}"))
}

/// 本地创建零字节空文件（目标已存在时报错，不截断）。
#[tauri::command]
pub async fn local_create_file(path: String) -> Result<(), String> {
    crate::slow_span!("local_create_file");
    tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .await
        .map(|_| ())
        .map_err(|e| format!("创建文件 {path} 失败：{e}"))
}

/// 本地递归复制：符号链接复制为同指向软链（不跟随）；目录递归；
/// 文件走 fs::copy。dst 的父目录需已存在（前端按目录树拼接）。
#[tauri::command]
pub async fn local_copy(src: String, dst: String) -> Result<(), String> {
    crate::slow_span!("local_copy");
    async fn rec(src: &str, dst: &str) -> Result<(), String> {
        let md = tokio::fs::symlink_metadata(src)
            .await
            .map_err(|e| format!("获取 {src} 属性失败：{e}"))?;
        let ftype = md.file_type();
        if ftype.is_symlink() {
            let target = tokio::fs::read_link(src)
                .await
                .map_err(|e| format!("读取链接 {src} 失败：{e}"))?;
            #[cfg(unix)]
            tokio::fs::symlink(&target, dst)
                .await
                .map_err(|e| format!("创建链接 {dst} 失败：{e}"))?;
            #[cfg(not(unix))]
            let _ = target;
        } else if md.is_dir() {
            tokio::fs::create_dir(dst)
                .await
                .map_err(|e| format!("创建目录 {dst} 失败：{e}"))?;
            let mut rd = tokio::fs::read_dir(src)
                .await
                .map_err(|e| format!("读取目录 {src} 失败：{e}"))?;
            while let Some(ent) = rd
                .next_entry()
                .await
                .map_err(|e| format!("遍历目录 {src} 失败：{e}"))?
            {
                let name = ent.file_name().to_string_lossy().into_owned();
                let s = if src == "/" {
                    format!("/{name}")
                } else {
                    format!("{src}/{name}")
                };
                let d = if dst == "/" {
                    format!("/{name}")
                } else {
                    format!("{dst}/{name}")
                };
                Box::pin(rec(&s, &d)).await?;
            }
        } else {
            tokio::fs::copy(src, dst)
                .await
                .map_err(|e| format!("复制 {src} → {dst} 失败：{e}"))?;
        }
        Ok(())
    }
    rec(&src, &dst).await
}

/// 在系统终端中打开指定目录（本地栏"在终端打开"，与远程栏菜单对等）。
/// 传入文件时打开其所在目录。进程 spawn 放阻塞线程池。
#[tauri::command]
pub async fn local_open_terminal(path: String) -> Result<(), String> {
    crate::slow_span!("local_open_terminal");
    let p = std::path::Path::new(&path);
    let dir = if p.is_dir() {
        p.to_path_buf()
    } else {
        p.parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf()
    };
    tauri::async_runtime::spawn_blocking(move || open_terminal(&dir))
        .await
        .map_err(|e| format!("终端任务失败：{e}"))?
}

/// macOS：`open -a Terminal <dir>`
#[cfg(target_os = "macos")]
fn open_terminal(dir: &std::path::Path) -> Result<(), String> {
    std::process::Command::new("open")
        .arg("-a")
        .arg("Terminal")
        .arg(dir)
        .spawn()
        .map_err(|e| format!("打开终端失败：{e}"))?;
    Ok(())
}

/// Linux：依次尝试常见终端模拟器；不支持工作目录参数的用进程 CWD 定位
#[cfg(target_os = "linux")]
fn open_terminal(dir: &std::path::Path) -> Result<(), String> {
    let attempts: &[(&str, &[&str])] = &[
        ("x-terminal-emulator", &[]),
        ("gnome-terminal", &["--working-directory"]),
        ("konsole", &["--workdir"]),
        ("xfce4-terminal", &["--working-directory"]),
        ("xterm", &[]),
    ];
    let mut last_err = String::new();
    for (bin, args) in attempts {
        let mut cmd = std::process::Command::new(bin);
        if args.is_empty() {
            cmd.current_dir(dir);
        } else {
            cmd.args(args.iter().copied()).arg(dir);
        }
        match cmd.spawn() {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = format!("{bin}: {e}");
                continue;
            }
        }
    }
    Err(format!("未找到可用的终端模拟器（{last_err}）"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn open_terminal(_dir: &std::path::Path) -> Result<(), String> {
    Err("当前平台暂不支持在系统终端打开".to_string())
}

/* ---- 拖拽上传：递归枚举被拖入的本地路径 ---- */

/// 被拖入路径下的一个待上传文件
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalWalkFile {
    /// 相对被拖入项的路径，恒用 `/` 连接，可直接拼到远端目录之后；
    /// 拖入单个文件时就是它自己的文件名
    pub rel: String,
    /// 本地绝对路径
    pub abs: String,
    pub size: u64,
}

/// 一次递归枚举结果
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalWalkResult {
    pub root: String,
    /// 被拖入项的名字（远端据此决定是否多建一层目录）
    pub name: String,
    pub is_dir: bool,
    pub files: Vec<LocalWalkFile>,
    /// 根之下各级目录的相对路径（含空目录，`/` 连接，不含根自身）。
    /// 单靠 files 推不出空目录，拖入 git 工程之类会丢掉 `.github/`、`logs/` 等占位目录。
    pub dirs: Vec<String>,
}

/// 枚举被拖入的本地路径：目录 → 其下全部普通文件（带相对路径）与各级子目录，
/// 文件 → 单条。
/// 符号链接与非普通文件（socket/fifo…）一律跳过：既不跟随链接（防环、防把链接目标
/// 所在目录整棵传上去），也不会在远端留下语义不对的替代品。
/// `max_files` 缺省 5000（封顶 20000），文件与目录条目合并计数，超限直接报错而非
/// 静默截断——半棵树悄悄传完比传失败更难发现。
#[tauri::command]
pub async fn local_walk(
    root: String,
    max_files: Option<usize>,
) -> Result<LocalWalkResult, String> {
    crate::slow_span!("local_walk");
    let cap = max_files.unwrap_or(5000).clamp(1, 20000);
    tauri::async_runtime::spawn_blocking(move || walk_sync(&root, cap))
        .await
        .map_err(|e| format!("枚举任务失败：{e}"))?
}

/// 层级上限：正常工程目录远不到这个深度，触到说明拖入了异常结构
const WALK_MAX_DEPTH: u32 = 64;

fn walk_sync(root: &str, cap: usize) -> Result<LocalWalkResult, String> {
    let p = std::path::Path::new(root);
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string());
    let md = std::fs::symlink_metadata(p).map_err(|e| format!("读取 {root} 失败：{e}"))?;
    if md.is_symlink() || !md.is_dir() {
        if !md.is_file() {
            return Err(format!("{name} 不是普通文件（链接或特殊文件不支持上传）"));
        }
        return Ok(LocalWalkResult {
            root: root.to_string(),
            name: name.clone(),
            is_dir: false,
            files: vec![LocalWalkFile {
                rel: name,
                abs: root.to_string(),
                size: md.len(),
            }],
            dirs: Vec::new(),
        });
    }
    // 显式栈遍历：递归版本在深树上有爆栈风险
    let mut files: Vec<LocalWalkFile> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, String, u32)> =
        vec![(p.to_path_buf(), String::new(), 0)];
    while let Some((dir, prefix, depth)) = stack.pop() {
        if depth > WALK_MAX_DEPTH {
            return Err(format!(
                "目录层级超过 {WALK_MAX_DEPTH} 层：{}",
                dir.display()
            ));
        }
        let rd = std::fs::read_dir(&dir)
            .map_err(|e| format!("读取目录 {} 失败：{e}", dir.display()))?;
        for item in rd {
            let Ok(ent) = item else { continue };
            let child = ent.path();
            let Ok(cmd) = std::fs::symlink_metadata(&child) else {
                continue
            };
            let fname = match child.file_name() {
                Some(s) => s.to_string_lossy().into_owned(),
                None => continue,
            };
            let rel = if prefix.is_empty() {
                fname
            } else {
                format!("{prefix}/{fname}")
            };
            if cmd.is_symlink() {
                continue;
            }
            if files.len() + dirs.len() >= cap {
                return Err(format!(
                    "目录内条目超过 {cap} 个，已取消本次拖拽上传（请缩小范围）"
                ));
            }
            if cmd.is_dir() {
                dirs.push(rel.clone());
                stack.push((child, rel, depth + 1));
                continue;
            }
            if !cmd.is_file() {
                continue;
            }
            files.push(LocalWalkFile {
                rel,
                abs: child.to_string_lossy().into_owned(),
                size: cmd.len(),
            });
        }
    }
    Ok(LocalWalkResult {
        root: root.to_string(),
        name,
        is_dir: true,
        files,
        dirs,
    })
}

/* ---- 外部编辑器集成：用本机编辑器打开远端文件的编辑副本 + 保存监听用的批量元数据 ---- */

/// 一个本地文件的元数据（前端编辑器保存监听轮询；按下标与请求路径对齐）
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalFileMeta {
    /// 文件已不存在或读不到（编辑器换名另存、用户删除）
    pub exists: bool,
    pub size: u64,
    /// 修改时间（epoch 毫秒）；取不到为 0
    pub mtime_ms: i64,
}

/// 批量取文件元数据：一次 IPC 覆盖整批「编辑中」的文件。
/// 单个路径读不到不算失败，回 `exists:false`——调用方要的是「哪些变了」，不是「哪个报错」。
#[tauri::command]
pub async fn local_files_meta(paths: Vec<String>) -> Result<Vec<LocalFileMeta>, String> {
    crate::slow_span!("local_files_meta");
    tauri::async_runtime::spawn_blocking(move || files_meta_sync(&paths))
        .await
        .map_err(|e| format!("元数据任务失败：{e}"))
}

fn files_meta_sync(paths: &[String]) -> Vec<LocalFileMeta> {
    paths
        .iter()
        .map(|p| match std::fs::metadata(p) {
            Ok(md) => LocalFileMeta {
                exists: true,
                size: md.len(),
                mtime_ms: md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
            },
            Err(_) => LocalFileMeta::default(),
        })
        .collect()
}

/// 内联给前端的文件原始字节上限（base64 前）：背景图与 AI 附件都走这条通道，超限直接拒。
pub const MAX_INLINE_FILE_BYTES: u64 = 20 * 1024 * 1024;

/// 读一个本地文件，返回标准 base64（无 dataURL 前缀）。
/// webview 里两条入口都拿不到字节：原生对话框只回路径，系统拖放（Tauri 事件）也只回路径，
/// 故由本命令按路径读回——背景图在前端缩放重编码为 dataURL，AI 附件重组成 File 走既有校验。
#[tauri::command]
pub async fn read_local_file_base64(path: String) -> Result<String, String> {
    crate::slow_span!("read_local_file_base64");
    tauri::async_runtime::spawn_blocking(move || read_inline_file_sync(&path))
        .await
        .map_err(|e| format!("读取任务失败：{e}"))?
}

fn read_inline_file_sync(path: &str) -> Result<String, String> {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    let p = std::path::Path::new(path);
    let md = std::fs::metadata(p).map_err(|e| format!("读取文件信息失败：{e}"))?;
    if !md.is_file() {
        return Err("所选不是普通文件".to_string());
    }
    if md.len() > MAX_INLINE_FILE_BYTES {
        return Err(format!(
            "文件过大（{} 字节，上限 {MAX_INLINE_FILE_BYTES} 字节）",
            md.len()
        ));
    }
    let bytes = std::fs::read(p).map_err(|e| format!("读取文件失败：{e}"))?;
    Ok(B64.encode(bytes))
}

/// 已知 GUI 编辑器：PATH 命令名 → macOS 下的 .app 名（VS Code 没装命令行工具时
/// 依然能用 `open -a` 拉起来）。
const GUI_EDITORS: &[(&str, &str)] = &[("code", "Visual Studio Code"), ("codium", "VSCodium"), ("cursor", "Cursor")];

/// 一个候选外部编辑器（全局设置「编辑器命令」的可选来源）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEditorCandidate {
    /// 展示名
    pub label: String,
    /// 可直接写进 `sftpEditorCommand` 的值，与手填自由文本同格式；
    /// 选中后启动路径仍走 [`open_with_editor`]，行为与手填完全一致
    pub command: String,
    /// 命令的程序部分（已去掉参数、已去引号），前端用它标注当前值是否命中候选
    pub program: String,
    /// 来源：cli（PATH 命令）/ app（.app 包或 .desktop 入口）/ reg（Windows App Paths）/ system（系统默认程序）
    pub source: &'static str,
}

/// 已知能接收文件路径参数的 GUI 编辑器命令名 → 展示名；顺序即候选优先级。
/// 刻意不含 vim / nano / micro 等终端编辑器：GUI 进程被拉起时没有 TTY，
/// 全屏编辑器只会「点了没反应」（同 [`open_with_editor`] 不读 `EDITOR` 的理由）。
const CLI_EDITORS_COMMON: &[(&str, &str)] = &[
    ("code", "Visual Studio Code"),
    ("codium", "VSCodium"),
    ("cursor", "Cursor"),
    ("subl", "Sublime Text"),
    ("idea", "IntelliJ IDEA"),
    ("pycharm", "PyCharm"),
    ("goland", "GoLand"),
    ("webstorm", "WebStorm"),
    ("rustrover", "RustRover"),
    ("trae", "Trae"),
    ("qoder", "Qoder"),
];

/// 只在 Linux 存在的图形编辑器命令（macOS 上同名命令往往是别的东西，
/// 例如 `/usr/bin/xed` 是「在 Xcode 里打开」的脚本，不能当候选）
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const CLI_EDITORS_LINUX: &[(&str, &str)] = &[
    ("geany", "Geany"),
    ("kate", "Kate"),
    ("gedit", "GNOME Text Editor"),
    ("org.gnome.TextEditor", "GNOME Text Editor"),
    ("mousepad", "Mousepad"),
    ("featherpad", "FeatherPad"),
    ("leafpad", "Leafpad"),
    ("xed", "Xed"),
    ("pluma", "Pluma"),
    ("bluefish", "Bluefish"),
    ("nedit", "NEdit"),
];

/// Windows 自带的记事本系
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const CLI_EDITORS_WINDOWS: &[(&str, &str)] = &[
    ("notepad", "Notepad"),
    ("notepad++", "Notepad++"),
    ("code", "Visual Studio Code"),
];

/// 浏览器和播放器也声明能打开 text/plain，但它们不是「编辑器」，不该混进候选
const NOT_EDITORS: &[&str] = &[
    "chrome",
    "chromium",
    "google chrome",
    "microsoft edge",
    "edge",
    "firefox",
    "brave browser",
    "opera",
    "vivaldi",
    "safari",
    "arc",
    "epic",
    "iina",
    "vlc",
    "spotify",
    "steam",
];

/// 候选上限：再多就是扫描跑偏（比如把整个 /Applications 都收进来了）
const MAX_EDITOR_CANDIDATES: usize = 48;

/// 枚举系统里可打开文本文件的程序，供设置面板选择。
/// 各平台来源不同（macOS .app 包 / Linux .desktop / Windows App Paths），
/// 探测失败只让候选变少，不返回错误——用户始终可以手填或按路径选择。
#[tauri::command]
pub async fn list_text_editors() -> Result<Vec<TextEditorCandidate>, String> {
    crate::slow_span!("list_text_editors");
    tauri::async_runtime::spawn_blocking(list_editors_sync)
        .await
        .map_err(|e| format!("编辑器探测任务失败：{e}"))
}

/// 候选收集器：按展示名去重（`Visual Studio Code` 的 CLI 与 .app 只留一条），
/// 先到先得，所以调用顺序就是优先级。
struct EditorCandidates {
    items: Vec<TextEditorCandidate>,
    seen: std::collections::HashSet<String>,
}

impl EditorCandidates {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            seen: std::collections::HashSet::new(),
        }
    }

    fn add(&mut self, label: String, program: &str, args: &[&str], source: &'static str) {
        if self.items.len() >= MAX_EDITOR_CANDIDATES {
            return;
        }
        let key: String = label
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        if key.is_empty() || is_not_editor(&key) {
            return;
        }
        if !self.seen.insert(key) {
            return;
        }
        let mut command = quote_program_path(program);
        for a in args {
            command.push(' ');
            command.push_str(&quote_program_path(a));
        }
        self.items.push(TextEditorCandidate {
            label,
            command,
            program: program.to_string(),
            source,
        });
    }
}

fn is_not_editor(dedup_key: &str) -> bool {
    NOT_EDITORS
        .iter()
        .any(|n| dedup_key == n.replace(' ', "").as_str())
}

/// 可能含空格的程序路径写成设置里能被 [`split_command_line`] 还原的形态：
/// 双引号成组，内部的 `"` 与 `\` 反斜杠转义。
fn quote_program_path(p: &str) -> String {
    if !p.chars().any(|c| c.is_whitespace() || c == '"' || c == '\\') {
        return p.to_string();
    }
    let mut s = String::with_capacity(p.len() + 2);
    s.push('"');
    for c in p.chars() {
        if c == '"' || c == '\\' {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('"');
    s
}

/// 本平台值得在 PATH 里找的编辑器命令表。
/// 跨平台同名命令含义不同（macOS 的 `xed` 是「在 Xcode 里打开」的脚本），所以按平台分表。
#[cfg(target_os = "linux")]
fn editor_cli_lists() -> Vec<&'static [(&'static str, &'static str)]> {
    vec![CLI_EDITORS_COMMON, CLI_EDITORS_LINUX]
}

#[cfg(target_os = "windows")]
fn editor_cli_lists() -> Vec<&'static [(&'static str, &'static str)]> {
    vec![CLI_EDITORS_COMMON, CLI_EDITORS_WINDOWS]
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn editor_cli_lists() -> Vec<&'static [(&'static str, &'static str)]> {
    vec![CLI_EDITORS_COMMON]
}

fn list_editors_sync() -> Vec<TextEditorCandidate> {
    let mut c = EditorCandidates::new();

    // 1) PATH 上的已知命令：最接近用户会手写的形式，也支持 --wait 一类参数
    for list in editor_cli_lists() {
        for (bin, label) in list {
            if which_in_path(bin).is_some() {
                c.add((*label).to_string(), bin, &[], "cli");
            }
        }
    }

    // 2) 平台专属来源
    #[cfg(target_os = "macos")]
    collect_macos_apps(&mut c);
    #[cfg(target_os = "linux")]
    collect_desktop_apps(&mut c);
    #[cfg(target_os = "windows")]
    collect_app_paths(&mut c);

    // 3) 系统默认程序（按扩展名交给 LaunchServices / mime 关联）。
    //    Windows 的 `cmd /c start` 会把第一个带引号参数当窗口标题，路径不可靠，故不提供。
    #[cfg(target_os = "macos")]
    c.add("系统默认程序（按文件类型）".into(), "open", &[], "system");
    #[cfg(target_os = "linux")]
    c.add("系统默认程序（按文件类型）".into(), "xdg-open", &[], "system");

    c.items
}

/// macOS：扫描应用程序目录里的 `.app` 包。CLI 命令没装（VS Code 右键菜单装了但没加
/// PATH、或用的是 Insiders/别名）时，`open -a <包路径>` 仍能拉起来。
#[cfg(target_os = "macos")]
fn collect_macos_apps(c: &mut EditorCandidates) {
    let mut roots = vec![std::path::PathBuf::from("/Applications")];
    if let Some(home) = home_dir() {
        roots.push(std::path::Path::new(&home).join("Applications"));
    }
    roots.push(std::path::PathBuf::from("/System/Applications"));
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue
        };
        let mut bundles: Vec<std::path::PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.extension().map(|x| x == "app").unwrap_or(false))
            .filter(|p| bundle_edits_text(p))
            .collect();
        // 同目录返回顺序不稳定，排序保证候选列表每次打开一致
        bundles.sort();
        for b in bundles {
            let Some(stem) = b.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue
            };
            let path = b.to_string_lossy().into_owned();
            c.add(stem, "open", &["-a", &path], "app");
        }
    }
}

/// 该 `.app` 是不是文本/代码编辑器。
/// Info.plist 无论 XML 还是二进制（bplist00）都把字符串按 UTF-8 原样存放，
/// 这里只做「有没有声明相应文档类型」的存在性判断，不解析结构。
///
/// 判据刻意收窄。`public.plain-text` 会被播放器、笔记、传输工具一并声明（Music、TV、
/// LocalSend、WPS 都有），`public.app-category.developer-tools` 也一样（Clash Verge、
/// MQTTX、Ollama 都是）。真正稳定的信号是代码编辑器特有的 `public.source-code`
/// （Xcode）与 `editorconfig` 文档类型（VS Code 及其派生系：Cursor、Trae、Qoder…）。
/// Apple 自带的纯文本编辑器两个都不声明，按名放行。
#[cfg(target_os = "macos")]
fn bundle_edits_text(app: &std::path::Path) -> bool {
    let Some(stem) = app.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
        return false;
    };
    if TEXT_EDITOR_ALLOWLIST.contains(&stem.as_str()) {
        return true;
    }
    let Ok(raw) = std::fs::read(app.join("Contents/Info.plist")) else {
        return false;
    };
    if raw.len() > 8 << 20 {
        return false;
    }
    let text = String::from_utf8_lossy(&raw);
    text.contains("public.source-code") || text.contains("editorconfig")
}

/// 只声明 `public.plain-text` 但确实是文本编辑器的应用（按 .app 目录名）
#[cfg(target_os = "macos")]
const TEXT_EDITOR_ALLOWLIST: &[&str] = &["TextEdit"];

/// Linux：`*.desktop` 入口里声明了 TextEditor 分类或 text/plain 的项。
/// Exec 行的 `%U %F` 之类字段码要剥掉，它们不是参数。
#[cfg(target_os = "linux")]
fn collect_desktop_apps(c: &mut EditorCandidates) {
    let mut dirs = vec![
        std::path::PathBuf::from("/usr/share/applications"),
        std::path::PathBuf::from("/usr/local/share/applications"),
    ];
    if let Some(home) = home_dir() {
        dirs.push(std::path::Path::new(&home).join(".local/share/applications"));
    }
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue
        };
        let mut files: Vec<std::path::PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "desktop").unwrap_or(false))
            .collect();
        files.sort();
        for f in files {
            let Some(raw) = std::fs::read_to_string(&f).ok() else {
                continue
            };
            if let Some((name, exec)) = desktop_editor_entry(&raw) {
                let Some((program, args)) = split_desktop_exec(&exec) else {
                    continue
                };
                // 入口指向的程序真在才算候选（卸载残留的 .desktop 不少）
                if !program.contains('/') && which_in_path(&program).is_none() {
                    continue;
                }
                if program.contains('/') && !std::path::Path::new(&program).exists() {
                    continue;
                }
                let owned: Vec<&str> = args.iter().map(String::as_str).collect();
                c.add(name, &program, &owned, "app");
            }
        }
    }
}

/// 解析 `.desktop`，命中「文本编辑器」时返回 (Name, Exec)。
/// 解析本身与平台无关，`test` 也纳入编译，这样在 macOS 上跑单测同样能回归。
#[cfg(any(target_os = "linux", test))]
fn desktop_editor_entry(raw: &str) -> Option<(String, String)> {
    let mut name = String::new();
    let mut exec = String::new();
    let mut editorish = false;
    let mut skip = false;
    let mut in_entry = false;
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with('[') {
            // 只认 [Desktop Entry] 主段，[Desktop Action xxx] 是右键动作
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue
        };
        match k.trim() {
            "NoDisplay" | "Hidden" if v.trim() == "true" => skip = true,
            "Terminal" if v.trim() == "true" => skip = true,
            "Name" if name.is_empty() => name = v.trim().to_string(),
            "Exec" if exec.is_empty() => exec = v.trim().to_string(),
            "Categories" => {
                if v.split(';').any(|x| x.eq_ignore_ascii_case("TextEditor")) {
                    editorish = true;
                }
            }
            "MimeType" => {
                if v.split(';').any(|x| x.ends_with("/plain") || x.ends_with("/text")) {
                    editorish = true;
                }
            }
            _ => {}
        }
    }
    if skip || !editorish || name.is_empty() || exec.is_empty() {
        return None;
    }
    Some((name, exec))
}

/// Exec 行 → (程序, 参数)：去掉桌面入口字段码，保留原有引号语义。
#[cfg(any(target_os = "linux", test))]
fn split_desktop_exec(exec: &str) -> Option<(String, Vec<String>)> {
    const FIELD_CODES: &[&str] = &[
        "%U", "%u", "%F", "%f", "%D", "%d", "%n", "%N", "%v", "%m", "%i", "%c", "%k",
    ];
    let mut keep: Vec<String> = split_command_line(exec)
        .into_iter()
        .filter(|t| !FIELD_CODES.contains(&t.as_str()))
        .collect();
    // `env VAR=x prog …` 只是环境包装：程序名在 env 与其后的赋值之后
    if keep.first().map(|t| t == "env").unwrap_or(false) {
        keep.remove(0);
        while keep
            .first()
            .map(|t| !t.starts_with('-') && t.contains('='))
            .unwrap_or(false)
        {
            keep.remove(0);
        }
    }
    if keep.is_empty() {
        return None;
    }
    let program = keep.remove(0);
    Some((program, keep))
}

/// Windows：`App Paths` 注册表下的程序名可被 CreateProcess 直接解析（即使不在 PATH），
/// 用它补齐安装器没勾「Add to PATH」的编辑器。
#[cfg(target_os = "windows")]
fn collect_app_paths(c: &mut EditorCandidates) {
    const ROOTS: &[&str] = &[
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\App Paths",
        r"HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths",
    ];
    for root in ROOTS {
        let Some(out) = reg_query(root) else {
            continue
        };
        for line in out.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix(root) else {
                continue;
            };
            let name = rest.trim_start_matches('\\').trim();
            // 只看一级子键（形如 …\App Paths\Code.exe），更深的值是键内的值名
            if name.is_empty() || name.contains('\\') || !name.ends_with(".exe") {
                continue;
            }
            let stem = name.trim_end_matches(".exe").to_string();
            c.add(stem.clone(), name, &[], "reg");
        }
    }
}

#[cfg(target_os = "windows")]
fn reg_query(key: &str) -> Option<String> {
    // 键名作为独立参数交给 CreateProcess，绝不拼 shell 命令
    let out = std::process::Command::new("reg")
        .args(["query", key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).into_owned().into()
}

/// 用本地编辑器打开一批文件（一次多份即批量编辑）。返回实际使用的编辑器名，
/// 供 toast 与日志展示。
///
/// `command` 是全局设置里的覆盖命令；留空则按 [`GUI_EDITORS`] 自动探测，都没命中
/// 才交给系统默认程序。**不采用 `EDITOR` 环境变量**：它多半指向 nvim/vim 这类终端
/// 编辑器，从 GUI 进程 detached 拉起没有 TTY，只会「点了没反应」，比报错更难查。
#[tauri::command]
pub async fn open_in_editor(
    paths: Vec<String>,
    command: Option<String>,
) -> Result<String, String> {
    crate::slow_span!("open_in_editor");
    tauri::async_runtime::spawn_blocking(move || open_with_editor(&paths, command.as_deref()))
        .await
        .map_err(|e| format!("编辑器任务失败：{e}"))?
}

fn open_with_editor(paths: &[String], command: Option<&str>) -> Result<String, String> {
    if paths.is_empty() {
        return Err("没有要打开的文件".to_string());
    }
    let custom = command.map(str::trim).filter(|s| !s.is_empty());
    let (program, pre_args, label) = match custom {
        Some(raw) => {
            let mut toks = split_command_line(raw);
            if toks.is_empty() {
                return Err(format!("编辑器命令「{raw}」无法解析"));
            }
            let label = toks.join(" ");
            (toks.remove(0), toks, label)
        }
        None => detect_editor()?,
    };
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&pre_args);
    // 文件路径逐个作为独立参数交给 execve，绝不拼进 shell 命令行：远端文件名可以是
    // 任何东西（空格、引号、反引号、`$(…)`），拼串等于把命令执行权交给服务器
    cmd.args(paths);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn()
        .map_err(|e| format!("启动编辑器 {program} 失败：{e}"))?;
    Ok(label)
}

/// macOS：优先 PATH 上的命令，其次 `/Applications/*.app`（CLI 没装也能用），
/// 最后 `open`（按文件类型走系统默认程序）。
#[cfg(target_os = "macos")]
fn detect_editor() -> Result<(String, Vec<String>, String), String> {
    for (bin, app) in GUI_EDITORS {
        if which_in_path(bin).is_some() {
            return Ok(((*bin).to_string(), vec![], (*bin).to_string()));
        }
        if std::path::Path::new(&format!("/Applications/{app}.app")).exists() {
            return Ok((
                "open".to_string(),
                vec!["-a".to_string(), (*app).to_string()],
                (*app).to_string(),
            ));
        }
    }
    Ok((
        "open".to_string(),
        vec![],
        "系统默认程序".to_string(),
    ))
}

/// Linux：只认 PATH 上的命令，都没有则 `xdg-open`
#[cfg(target_os = "linux")]
fn detect_editor() -> Result<(String, Vec<String>, String), String> {
    for (bin, _) in GUI_EDITORS {
        if which_in_path(bin).is_some() {
            return Ok(((*bin).to_string(), vec![], (*bin).to_string()));
        }
    }
    Ok((
        "xdg-open".to_string(),
        vec![],
        "系统默认程序".to_string(),
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn detect_editor() -> Result<(String, Vec<String>, String), String> {
    Err("未能自动探测到编辑器，请在全局设置的「编辑器命令」里填写可执行文件路径".to_string())
}

/// 在 PATH 里找可执行文件。不 spawn `which`：那会为每次探测额外起一个进程。
#[cfg(unix)]
fn which_in_path(bin: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir_iter = std::env::var("PATH").ok()?;
    dir_iter.split(':').find_map(|d| {
        let p = std::path::Path::new(d).join(bin);
        std::fs::metadata(&p)
            .ok()
            .filter(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)?;
        Some(p)
    })
}

#[cfg(not(unix))]
fn which_in_path(bin: &str) -> Option<std::path::PathBuf> {
    let dir_iter = std::env::var("PATH").ok()?;
    dir_iter.split(';').find_map(|d| {
        let p = std::path::Path::new(d).join(format!("{bin}.exe"));
        p.is_file().then_some(p)
    })
}

/// 把设置里的编辑器命令切成「程序 + 前置参数」：按空白分割，支持单/双引号成组
/// 与反斜杠转义（路径含空格的编辑器可写 `"…/My Editor" -n`）。
/// 只作用于命令串本身——被打开的文件路径不经过它，见 [`open_with_editor`]。
fn split_command_line(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            Some('"') => {
                if c == '"' {
                    quote = None;
                } else if c == '\\' {
                    // 双引号内只有 \\ \" \$ \` \ 有意义，其余保留反斜杠原样
                    match chars.next() {
                        Some(n @ ('\\' | '"' | '$' | '`')) => cur.push(n),
                        Some(n) => {
                            cur.push('\\');
                            cur.push(n);
                        }
                        None => cur.push('\\'),
                    }
                } else {
                    cur.push(c);
                }
            }
            _ => {
                if c == '\\' {
                    match chars.next() {
                        Some(n) => cur.push(n),
                        None => cur.push('\\'),
                    }
                    started = true;
                } else if c == '\'' || c == '"' {
                    quote = Some(c);
                    started = true;
                } else if c.is_whitespace() {
                    if started {
                        out.push(std::mem::take(&mut cur));
                        started = false;
                    }
                } else {
                    cur.push(c);
                    started = true;
                }
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_case(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rhost-walk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn walk_returns_single_file_with_basename_rel() {
        let dir = tmp_case("file");
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        let r = walk_sync(dir.join("a.txt").to_str().unwrap(), 10).unwrap();
        assert!(!r.is_dir);
        assert_eq!(r.name, "a.txt");
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.files[0].rel, "a.txt");
        assert_eq!(r.files[0].size, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_skips_symlinks_and_keeps_relative_paths() {
        let dir = tmp_case("tree");
        std::fs::create_dir_all(dir.join("sub/deep")).unwrap();
        std::fs::write(dir.join("sub/deep/x.log"), b"12").unwrap();
        std::fs::write(dir.join("top.txt"), b"1").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("sub"), dir.join("link")).unwrap();

        let r = walk_sync(dir.to_str().unwrap(), 10).unwrap();
        assert!(r.is_dir);
        let mut rels: Vec<&str> = r.files.iter().map(|f| f.rel.as_str()).collect();
        rels.sort();
        // link → 指向 sub 的软链目录整体跳过，绝不重复传一份
        assert_eq!(rels, vec!["sub/deep/x.log", "top.txt"]);
        let mut d: Vec<&str> = r.dirs.iter().map(|s| s.as_str()).collect();
        d.sort();
        assert_eq!(d, vec!["sub", "sub/deep"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_refuses_over_file_cap() {
        let dir = tmp_case("cap");
        for i in 0..4 {
            std::fs::write(dir.join(format!("f{i}")), b"x").unwrap();
        }
        let err = walk_sync(dir.to_str().unwrap(), 3).unwrap_err();
        assert!(err.contains("超过 3 个"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一次拖入「多个文件 + 多个文件夹」：每项各自枚举，rel 路径互不串台
    #[test]
    fn walk_handles_batch_of_mixed_items() {
        let dir = tmp_case("batch");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.log"), b"bb").unwrap();
        std::fs::create_dir_all(dir.join("proj/src")).unwrap();
        std::fs::write(dir.join("proj/src/main.rs"), b"fn main(){}").unwrap();
        std::fs::create_dir_all(dir.join("docs/assets")).unwrap();
        std::fs::write(dir.join("docs/assets/1.png"), b"png").unwrap();
        // 空子目录也要出现在 dirs 里（否则远端复刻出的树会丢掉占位目录）
        std::fs::create_dir_all(dir.join("proj/vendor/empty")).unwrap();

        // 模拟前端：按拖入顺序逐项枚举，拼出「根名/rel」即远端相对路径
        let mut remote_rel: Vec<String> = Vec::new();
        let mut remote_dirs: Vec<String> = Vec::new();
        for it in ["a.txt", "b.log", "proj", "docs"] {
            let r = walk_sync(dir.join(it).to_str().unwrap(), 100).unwrap();
            for d in &r.dirs {
                remote_dirs.push(format!("{}/{}", r.name, d));
                assert!(
                    dir.join(it).join(d).is_dir(),
                    "dirs 里的 {} 不是本地目录",
                    d
                );
            }
            for f in &r.files {
                let joined = if r.is_dir {
                    format!("{}/{}", r.name, f.rel)
                } else {
                    f.rel.clone()
                };
                remote_rel.push(joined);
                // abs 必须真的指向该文件；目录项按 rel 还原层级，文件项就是被拖入的路径本身
                let expected = if r.is_dir {
                    dir.join(it).join(&f.rel)
                } else {
                    dir.join(it)
                };
                assert_eq!(
                    std::path::Path::new(&f.abs),
                    expected.as_path(),
                    "rel={}",
                    f.rel
                );
                assert!(std::fs::metadata(&f.abs).unwrap().is_file(), "{:?}", f.abs);
            }
        }
        remote_rel.sort();
        assert_eq!(
            remote_rel,
            vec![
                "a.txt",
                "b.log",
                "docs/assets/1.png",
                "proj/src/main.rs",
            ]
        );
        remote_dirs.sort();
        assert_eq!(
            remote_dirs,
            vec![
                "docs/assets",
                "proj/src",
                "proj/vendor",
                "proj/vendor/empty",
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_meta_aligns_by_index_and_survives_missing_paths() {
        let dir = tmp_case("meta");
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        let missing = dir.join("gone.txt").to_string_lossy().into_owned();
        let got = files_meta_sync(&[
            dir.join("a.txt").to_string_lossy().into_owned(),
            missing,
            dir.to_string_lossy().into_owned(),
        ]);
        assert_eq!(got.len(), 3);
        assert!(got[0].exists);
        assert_eq!(got[0].size, 5);
        // mtime 必须是真实时间戳（拿不到会是 0，前端会把它当「没变」而漏掉回写）
        assert!(got[0].mtime_ms > 0, "mtime_ms={}", got[0].mtime_ms);
        // 读不到的路径只报「不存在」，不中断整批轮询
        assert!(!got[1].exists);
        assert_eq!(got[1].size, 0);
        // 目录也是「存在」的：编辑器另存成目录这类怪事不该被当成删除
        assert!(got[2].exists);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidate_command_round_trips_through_split() {
        let mut c = EditorCandidates::new();
        c.add(
            "Visual Studio Code".into(),
            "/Applications/Visual Studio Code.app/Contents/MacOS/Electron",
            &["--new-window"],
            "app",
        );
        c.add(
            "系统默认".into(),
            "open",
            &["-a", "/Applications/TextEdit.app"],
            "system",
        );
        let got = c.items;
        assert_eq!(
            got[0].command,
            "\"/Applications/Visual Studio Code.app/Contents/MacOS/Electron\" --new-window"
        );
        assert_eq!(
            got[0].program,
            "/Applications/Visual Studio Code.app/Contents/MacOS/Electron"
        );
        // 无空格的路径不加引号（更贴近用户会手写的形式），带空格的才成组
        assert_eq!(got[1].command, "open -a /Applications/TextEdit.app");
        // 候选值必须能被现有解析器原样还原：设置里存的仍是同一套命令语法
        let toks = split_command_line(&got[0].command);
        assert_eq!(toks[0], "/Applications/Visual Studio Code.app/Contents/MacOS/Electron");
        assert_eq!(toks[1], "--new-window");
    }

    #[test]
    fn candidate_list_dedupes_and_drops_browsers() {
        let mut c = EditorCandidates::new();
        c.add("Visual Studio Code".into(), "code", &[], "cli");
        c.add(
            "Visual Studio Code".into(),
            "open",
            &["-a", "/Applications/Visual Studio Code.app"],
            "app",
        );
        c.add(
            "Google Chrome".into(),
            "open",
            &["-a", "/Applications/Google Chrome.app"],
            "app",
        );
        c.add(String::new(), "code", &[], "cli");
        let got = c.items;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].command, "code");
    }

    /// .desktop 只收真正的图形文本编辑器：终端编辑器、隐藏项都要跳过
    #[test]
    fn desktop_entry_parsing_keeps_text_editors() {
        let ok = "\
[Desktop Entry]
Name=Kate
Exec=kate -u %U
Icon=kate
MimeType=text/plain;
Categories=Qt;KDE;TextEditor;
";
        let (name, exec) = desktop_editor_entry(ok).expect("Kate 应被识别为编辑器");
        assert_eq!(name, "Kate");
        let (program, args) = split_desktop_exec(&exec).unwrap();
        assert_eq!(program, "kate");
        assert_eq!(args, ["-u"]);

        let terminal =
            "[Desktop Entry]\nName=vim\nExec=vim %F\nCategories=TextEditor;\nTerminal=true\n";
        assert!(desktop_editor_entry(terminal).is_none());

        let hidden =
            "[Desktop Entry]\nName=Helper\nExec=helper %U\nCategories=TextEditor;\nNoDisplay=true\n";
        assert!(desktop_editor_entry(hidden).is_none());

        // 动作段里的 Name/Exec 不能污染主段解析
        let with_actions = "\
[Desktop Entry]
Name=Geany
Exec=geany --config-path=/dev/null %F
Categories=Development;TextEditor;

[Desktop Action new-window]
Name=New Window
Exec=geany --new-window
";
        let (name, exec) = desktop_editor_entry(with_actions).unwrap();
        assert_eq!(name, "Geany");
        let (program, args) = split_desktop_exec(&exec).unwrap();
        assert_eq!(program, "geany");
        assert_eq!(args, ["--config-path=/dev/null"]);

        // env 包装取真正的程序名，带空格的路径保持成一个 token
        let (program, args) = split_desktop_exec("env DISPLAY=:0 \"/opt/my editor/ed\" %U").unwrap();
        assert_eq!(program, "/opt/my editor/ed");
        assert!(args.is_empty());
    }

    #[test]
    fn editor_command_splits_quotes_and_escapes() {
        assert_eq!(split_command_line("code --wait"), ["code", "--wait"]);
        assert_eq!(split_command_line("   "), Vec::<String>::new());
        assert_eq!(
            split_command_line("\"/Applications/My Editor.app/Contents/MacOS/My Editor\" -n"),
            ["/Applications/My Editor.app/Contents/MacOS/My Editor", "-n"]
        );
        assert_eq!(split_command_line("sh -c 'echo hi'"), ["sh", "-c", "echo hi"]);
        assert_eq!(split_command_line("my\\ editor -w"), ["my editor", "-w"]);
        // 空引号串是一个真实的位置参数，不能丢
        assert_eq!(split_command_line("ed \"\" x"), ["ed", "", "x"]);
        // 单引号内不认反斜杠转义（POSIX 规则）：'\n' 就是两个字符
        assert_eq!(split_command_line("'a\\b'"), ["a\\b"]);
    }

    #[test]
    fn read_inline_file_encodes_rejects_dir_and_oversize() {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine;
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("rhost-img-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let png = dir.join("bg.png");
        let bytes = [0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        std::fs::write(&png, bytes).unwrap();
        assert_eq!(
            read_inline_file_sync(png.to_str().unwrap()).unwrap(),
            B64.encode(bytes),
            "文件要按标准 base64 原样读回"
        );

        // 目录不是普通文件：前端拿到的是明确错误，不是一段解不开的 base64
        assert!(read_inline_file_sync(dir.to_str().unwrap()).is_err());

        // 超上限先按 metadata 长度拒绝：稀疏文件不落盘真实内容，创建几乎零成本
        let huge = dir.join("huge.png");
        let mut f = std::fs::File::create(&huge).unwrap();
        f.write_all(&bytes).unwrap();
        f.set_len(MAX_INLINE_FILE_BYTES + 1).unwrap();
        let err = read_inline_file_sync(huge.to_str().unwrap()).unwrap_err();
        assert!(err.contains("文件过大"), "超限应报体积错误而非读取失败：{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

