mod applog;
mod ai;
mod config_crypto;
mod config_io;
mod config_migrate;
mod external;
mod fonts;
mod ipc;
mod keygen;
mod known_hosts;
mod localfs;
mod metrics;
mod motd;
pub mod ssh;
mod store;
mod sysmon;
mod toolbox;

use applog::events as ev;
use ssh::manager::SessionManager;
use tauri::{Manager, RunEvent, WebviewUrl, webview::WebviewWindowBuilder};

/// 应用启动时刻（退出时计算 uptime_s）
static BOOT_INSTANT: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// 构造 prevent-default 插件：关闭 WebView 原生右键菜单（Reload/Inspect Element 等），
/// 防止其叠加在自定义 ContextMenu 之上；同时禁用浏览器默认快捷键（F3/Ctrl+J 等）。
/// - dev 构建：保留 DEV_TOOLS（F12）与 RELOAD（Ctrl+R）便于调试
/// - release 构建：禁用全部默认快捷键
#[cfg(debug_assertions)]
fn prevent_default_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    use tauri_plugin_prevent_default::Flags;
    tauri_plugin_prevent_default::Builder::new()
        .with_flags(Flags::all().difference(Flags::DEV_TOOLS | Flags::RELOAD))
        .build()
}
#[cfg(not(debug_assertions))]
fn prevent_default_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri_plugin_prevent_default::init()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = BOOT_INSTANT.set(std::time::Instant::now());

    tauri::Builder::default()
        // 单实例守卫必须是第一个注册的插件（官方要求）：堡垒机反复点开链接时，
        // 第二个进程交出 argv 即退出，链接由运行中的实例打开
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            external::handle_command_line(app, &args, external::Source::SecondInstance);
        }))
        // ssh:// / sftp:// 协议声明与系统 URL 事件（协议清单见 tauri.conf.json
        // plugins.deep-link.desktop.schemes 与 src-tauri/Info.plist）
        .plugin(tauri_plugin_deep_link::init())
        // 终端内 http(s) 链接点击后交系统默认浏览器打开（WebView 内 window.open 不可靠）
        .plugin(tauri_plugin_opener::init())
        // 终端复制/粘贴的剪贴板读写（WKWebView 对 Web Clipboard API 读取有限制）
        .plugin(tauri_plugin_clipboard_manager::init())
        // 配置导入导出：系统打开/保存文件对话框
        .plugin(tauri_plugin_dialog::init())
        // 配置导入后提示「立即重启」：relaunch()
        .plugin(tauri_plugin_process::init())
        // 关闭 WebView 原生右键菜单与浏览器默认快捷键（详见 prevent_default_plugin）
        .plugin(prevent_default_plugin())
        // 全局配置写锁：串行化 app_config.json + connections.json 的所有写操作
        // （节写穿 / 连接增删 / 导入 / 重置），必须在 setup 与命令注册前挂载
        .manage(applog::persisted::ConfigWriteLock::new())
        // 堡垒机外部链接队列：冷启动链接在前端监听器就绪前先入队，挂载后领取
        .manage(external::PendingLinks::default())
        .setup(|app| {
            // 后端整体配置文件（前端 localStorage 已下线，此文件为唯一真相源）：
            // logStoragePath 重启生效的唯一载体，同时为整体配置导入导出与首帧注入提供数据；
            // 缺失/损坏时 load_full 回退默认值
            let app_config_file = app
                .path()
                .app_config_dir()
                .map(|d| d.join(applog::persisted::APP_CONFIG_FILE_NAME))
                .unwrap_or_else(|_| {
                    std::env::temp_dir()
                        .join("com.rhost.app")
                        .join(applog::persisted::APP_CONFIG_FILE_NAME)
                });
            let app_cfg = applog::persisted::load_full(&app_config_file);

            // 首帧注入：splash 背景模式必须在页面任何脚本执行前同步可知（异步 IPC 会导致
            // 透明/深色底闪烁）。窗口改由 Rust 创建（tauri.conf.json windows 留空），经
            // initialization_script 写入 window.__RHOST_BOOT__；settings 节缺失/非法回退默认透明
            let splash_transparent = app_cfg
                .settings
                .get("splashTransparent")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            // 界面主题（dark/light/auto）：index.html 头部脚本据此在首帧前挂
            // html[data-theme]，避免浅色用户看到深色底闪一下；auto 由前端
            // matchMedia 解析。此处传原始字符串，缺失时不注入（前端视为 auto）
            let ui_theme = app_cfg
                .settings
                .get("uiTheme")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            // 背景不透明度（60–100）：index.html 头部脚本据此在首帧前挂 --glass-a，
            // 避免透明窗口先实心再变玻璃的跳变；缺失/非法由前端回退默认 96
            let opacity = app_cfg
                .settings
                .get("opacity")
                .and_then(|v| v.as_u64())
                .map(|v| v.min(100) as u32);
            let boot_script = format!(
                "window.__RHOST_BOOT__={};",
                serde_json::json!({
                    "splashTransparent": splash_transparent,
                    // 同步注入运行平台（前端据此切换标题栏安全区与窗口控件渲染），
                    // 取 std::env::consts::OS：macos / windows / linux
                    "platform": std::env::consts::OS,
                    // 同步注入应用版本与是否开发版：侧栏/关于卡片据此显示，
                    // 走首帧注入而非异步 IPC，避免版本号先空后现的闪跳。
                    // 版本取 Cargo.toml（与 tauri.conf.json、frontend/package.json 同步）
                    "appVersion": env!("CARGO_PKG_VERSION"),
                    "appDevBuild": cfg!(debug_assertions),
                    "uiTheme": ui_theme,
                    "opacity": opacity,
                })
            );
            // 属性与原静态窗口定义保持一致；label 固定 "main"（capabilities/default.json 按此授权）
            // 跨平台标题栏策略：
            // - macOS：decorations(true) + Overlay 标题栏样式 + hidden_title，原生红绿灯浮于
            //   前端标题栏左侧；hidden_title 隐藏系统标题文字，避免与前端 logo 的 "Rhost"
            //   重影。需 macOSPrivateApi(true)（tauri.conf.json）+ transparent 才能让标题栏
            //   区域与内容融合（backdrop-filter 毛玻璃生效）。前端预留左侧安全区。
            // - Windows/Linux：decorations(false) 无边框，由前端在标题栏右侧自绘窗口控制按钮。
            //   title_bar_style / hidden_title / traffic_light_position 均为 macOS 专属 API，
            //   须 #[cfg] 隔离以免他平台编译失败。
            // macOS 专属 cfg 块内会对 builder 重新赋值，仅该平台需要 mut；
            // 非 macOS 平台不重赋值，须压制 unused_mut 警告
            #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
            let mut window_builder = WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::App("index.html".into()),
            )
            .title("Rhost")
            .inner_size(1200.0, 760.0)
            .min_inner_size(940.0, 600.0)
            .resizable(true)
            .decorations(cfg!(target_os = "macos"))
            .transparent(cfg!(target_os = "macos"))
            // 保持 Tauri 的拖放处理器开启：SFTP 拖拽上传必须要它——HTML5 的 drop 事件
            // 在 webview 里拿不到被拖入项的真实路径，只有这里的 Tauri 事件给出 paths。
            // 代价是 webview 收不到携带 File 的 drop，AI 附件区改为同样订阅 Tauri 事件、
            // 按路径读回字节重组 File（见 AiChatPanel 的 addPaths）
            .initialization_script(&boot_script);

            // 界面主题同步原生窗口外观：显式 dark/light 锁定 NSAppearance（红绿灯
            // 配色/毛玻璃底色联动）；auto/缺失不设置——原生默认即跟随系统，
            // 系统切换由 WebView 侧 matchMedia 监听同步 CSS
            match ui_theme.as_deref() {
                Some("dark") => window_builder = window_builder.theme(Some(tauri::Theme::Dark)),
                Some("light") => window_builder = window_builder.theme(Some(tauri::Theme::Light)),
                _ => {}
            }

            #[cfg(target_os = "macos")]
            {
                window_builder = window_builder
                    .title_bar_style(tauri::TitleBarStyle::Overlay)
                    .hidden_title(true)
                    // 红绿灯位置：x 左边距，y 顶部偏移（逻辑像素）；y=0 紧贴顶部，越大越往下
                    .traffic_light_position(tauri::LogicalPosition::new(12.0, 24.0));
            }

            // macOS 默认菜单栏（Tauri 自动安装）含「关闭窗口 ⌘W」，而原生 accelerator
            // 早于 WebView 的 keydown 消费按键，会让设置里的「关闭标签页 ⌘W」直接关掉整个
            // 窗口。这里用同构菜单替换、仅去掉 close_window：⌘Q 退出、⌘H 隐藏、编辑板报贴、
            // 最小化/全屏等标准项与快捷键全部保持一致。
            #[cfg(target_os = "macos")]
            {
                use tauri::menu::{AboutMetadata, Menu, PredefinedMenuItem, Submenu};
                let pkg_name = app.package_info().name.clone();
                let about = AboutMetadata {
                    name: Some(pkg_name.clone()),
                    version: Some(app.package_info().version.to_string()),
                    copyright: app.config().bundle.copyright.clone(),
                    authors: app.config().bundle.publisher.clone().map(|p| vec![p]),
                    ..Default::default()
                };
                let h = app.handle();
                let menu = Menu::with_items(
                    h,
                    &[
                        &Submenu::with_items(
                            h,
                            pkg_name,
                            true,
                            &[
                                &PredefinedMenuItem::about(h, None, Some(about))?,
                                &PredefinedMenuItem::separator(h)?,
                                &PredefinedMenuItem::services(h, None)?,
                                &PredefinedMenuItem::separator(h)?,
                                &PredefinedMenuItem::hide(h, None)?,
                                &PredefinedMenuItem::hide_others(h, None)?,
                                &PredefinedMenuItem::separator(h)?,
                                &PredefinedMenuItem::quit(h, None)?,
                            ],
                        )?,
                        // Edit 子菜单保留 ⌘C/⌘V/⌘A/⌘X 标准文本操作快捷键
                        &Submenu::with_items(
                            h,
                            "Edit",
                            true,
                            &[
                                &PredefinedMenuItem::undo(h, None)?,
                                &PredefinedMenuItem::redo(h, None)?,
                                &PredefinedMenuItem::separator(h)?,
                                &PredefinedMenuItem::cut(h, None)?,
                                &PredefinedMenuItem::copy(h, None)?,
                                &PredefinedMenuItem::paste(h, None)?,
                                &PredefinedMenuItem::select_all(h, None)?,
                            ],
                        )?,
                        &Submenu::with_items(
                            h,
                            "View",
                            true,
                            &[&PredefinedMenuItem::fullscreen(h, None)?],
                        )?,
                        // 与 Tauri 默认同名同 id：macOS 会把窗口列表挂进 Window 菜单
                        &Submenu::with_id_and_items(
                            h,
                            tauri::menu::WINDOW_SUBMENU_ID,
                            "Window",
                            true,
                            &[
                                &PredefinedMenuItem::minimize(h, None)?,
                                &PredefinedMenuItem::maximize(h, None)?,
                            ],
                        )?,
                        &Submenu::with_id_and_items(
                            h,
                            tauri::menu::HELP_SUBMENU_ID,
                            "Help",
                            true,
                            &[],
                        )?,
                    ],
                )?;
                app.set_menu(menu)?;
            }

            window_builder.build()?;

            // Hub 在窗口创建之后初始化：建窗不产生 applog，boot.start 仍是全局第一条日志
            let log_dir = match applog::init(app_cfg.logs_config(), app_config_file.clone()) {
                Ok(dir) => Some(dir),
                Err(e) => {
                    // Hub 初始化失败降级为无日志运行（emit 全静默），不阻断应用启动
                    eprintln!("[FATAL] applog::init 失败: {e}");
                    None
                }
            };

            // 启动埋点序列（§4.1 时序约束）：
            // boot.start → boot.config_loaded → boot.log_dir_ready → log.cleanup（可选）
            // → boot.keychain_ready → boot.ready
            applog::emit(
                log::Level::Info,
                "app",
                ev::APP_BOOT_START,
                None,
                "Rhost 启动中",
                Some(serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                })),
            );

            // 记录后端配置文件加载完成及其路径（kv 平铺，避免嵌套对象在面板
            // 显示为 [object Object]；生效中的具体配置可用 jq 查 app_config.json）
            applog::emit(
                log::Level::Info,
                "app",
                ev::APP_BOOT_CONFIG_LOADED,
                None,
                "配置文件已加载",
                Some(serde_json::json!({
                    "elapsed_ms": BOOT_INSTANT.get().map(|t| t.elapsed().as_millis() as u64).unwrap_or(0),
                    "path": app_config_file.display().to_string(),
                })),
            );

            if let Some(dir) = &log_dir {
                applog::emit(
                    log::Level::Info,
                    "app",
                    ev::APP_BOOT_LOG_DIR_READY,
                    None,
                    "日志目录已就绪",
                    Some(serde_json::json!({ "path": dir.to_string_lossy() })),
                );
                // 启动时过期日志清理（§6.4）
                if let Some(hub) = applog::try_hub() {
                    hub.cleanup_logs();
                }
            }

            applog::emit(
                log::Level::Info,
                "app",
                ev::APP_BOOT_KEYCHAIN_READY,
                None,
                "密钥存储已连接",
                Some(serde_json::json!({
                    "backend": if cfg!(target_os = "macos") {
                        "macOS Keychain"
                    } else if cfg!(target_os = "windows") {
                        "Windows Credential Manager"
                    } else {
                        "libsecret"
                    }
                })),
            );

            applog::emit(
                log::Level::Info,
                "app",
                ev::APP_BOOT_READY,
                None,
                "Rhost 启动完成",
                Some(serde_json::json!({
                    "elapsed_ms": BOOT_INSTANT.get().map(|t| t.elapsed().as_millis() as u64).unwrap_or(0),
                })),
            );

            // 堡垒机外部连接：命令行里的 ssh:// / sftp:// 链接先入队（前端监听器尚未
            // 挂载，由挂载后 take_external_links 领取），再订阅系统 URL 事件
            external::handle_command_line(
                app.handle(),
                &std::env::args().collect::<Vec<String>>(),
                external::Source::Cli,
            );
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                // macOS 由打包时写入 Info.plist 的 CFBundleURLTypes 声明协议，链接经
                // RunEvent::Opened 送达；Windows/Linux 需在运行时注册协议（仅写当前用户
                // 作用域的注册表 / xdg-mime 默认值），注册结果只落 debug
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                if let Err(e) = app.deep_link().register_all() {
                    log::debug!("ssh:// / sftp:// 协议注册失败（不影响堡垒机命令行拉起）: {e}");
                }
                let handle = app.handle().clone();
                app.deep_link().on_open_url(move |event| {
                    external::handle_urls(
                        &handle,
                        event.urls().iter().map(ToString::to_string).collect(),
                    );
                });
            }

            Ok(())
        })
        // 会话池注入全局状态，供命令按 session_id 查找
        .manage(SessionManager::new())
        // AI 对话进行中请求的取消令牌表（按 scope 索引）
        .manage(ai::AiState::default())
        .invoke_handler(tauri::generate_handler![
            ipc::connect_ssh,
            external::take_external_links,
            ipc::write_terminal,
            ipc::resize_terminal,
            ipc::tunnel_start,
            ipc::tunnel_stop,
            ipc::disconnect_session,
            ipc::sftp_list_dir,
            ipc::sftp_mkdir,
            ipc::sftp_mkdir_all,
            ipc::sftp_sync_cwd,
            ipc::sftp_set_idle_timeout,
            ipc::sftp_stat,
            ipc::sftp_walk,
            ipc::sftp_fetch,
            ipc::sftp_upload,
            ipc::sftp_download,
            ipc::sftp_transfer_pause,
            ipc::sftp_transfer_cancel,
            ipc::sftp_remove,
            ipc::sftp_rename,
            ipc::sftp_create_file,
            ipc::sftp_copy,
            ipc::sftp_readlink,
            ipc::sftp_set_perm,
            ipc::start_metrics,
            ipc::stop_metrics,
            ipc::metrics_heartbeat,
            ipc::metrics_procs_full,
            ipc::test_ssh_connection,
            ipc::load_connections,
            ipc::save_connection,
            ipc::delete_connection,
            ipc::save_connection_password,
            ipc::get_connection_password,
            keygen::ssh_keygen,
            keygen::ssh_key_import,
            ipc::get_app_memory,
            ipc::subscribe_app_logs,
            ipc::report_app_log,
            ipc::clear_app_log_buffer,
            ipc::reveal_log_dir,
            ipc::reveal_log_storage_dir,
            ipc::set_window_theme,
            ipc::set_window_glass,
            ipc::set_log_config,
            ipc::load_app_config,
            ipc::set_app_config_section,
            ipc::export_config,
            ipc::read_import_file,
            ipc::import_config,
            ipc::import_hosts,
            ipc::reset_settings_config,
            fonts::check_fonts,
            localfs::list_local_dir,
            localfs::mkdir,
            localfs::local_mkdir_all,
            localfs::local_remove,
            localfs::local_rename,
            localfs::local_create_file,
            localfs::local_copy,
            localfs::local_open_terminal,
            localfs::local_walk,
            localfs::local_files_meta,
            localfs::read_local_file_base64,
            localfs::open_in_editor,
            localfs::list_text_editors,
            ai::ai_chat_stream,
            ai::ai_stop_chat,
            ai::ai_test_connection,
            ai::ai_set_api_key,
            ai::ai_get_api_key,
            ai::ai_save_chat,
            ai::ai_load_chat,
            ai::ai_clear_chat,
            ai::ai_summarize,
            ai::ai_list_prompt_assets,
            ai::ai_reveal_prompt_dir,
            ai::ai_list_memories,
            ai::ai_save_memories,
            ai::ai_clear_memories,
            ai::ai_extract_memories,
            ai::ai_exec_tool,
            ai::ai_cancel_execs,
            toolbox::toolbox_list_ports,
            toolbox::toolbox_firewall_status,
            toolbox::toolbox_firewall_add,
            toolbox::toolbox_firewall_remove,
            toolbox::toolbox_firewall_toggle,
            toolbox::toolbox_deploy_pubkey,
            toolbox::toolbox_docker_list,
            toolbox::toolbox_docker_action,
            toolbox::toolbox_docker_logs,
            toolbox::toolbox_docker_logs_save,
            toolbox::toolbox_docker_export,
            toolbox::toolbox_docker_export_local,
            toolbox::toolbox_docker_import,
            toolbox::toolbox_docker_import_local,
            toolbox::toolbox_docker_pull,
            toolbox::toolbox_docker_config_get,
            toolbox::toolbox_docker_config_save,
            toolbox::toolbox_docker_terminal_open,
            toolbox::toolbox_docker_terminal_write,
            toolbox::toolbox_docker_terminal_resize,
            toolbox::toolbox_docker_terminal_close,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // 退出序列（§4.1/§6.2）：shutdown.start → sessions_closed →
            // log_flushed → app.exit（最后一条）；随后 Hub 关闭并 join 落盘线程，
            // 保证上述 4 条完整落盘、无半截行。
            if let RunEvent::ExitRequested { .. } = event {
                let shutdown_instant = std::time::Instant::now();

                let manager = app_handle.state::<SessionManager>();
                let active = tauri::async_runtime::block_on(manager.session_count());
                applog::emit(
                    log::Level::Info,
                    "app",
                    ev::APP_SHUTDOWN_START,
                    None,
                    "正在退出",
                    Some(serde_json::json!({ "active_sessions": active })),
                );

                let closed = tauri::async_runtime::block_on(manager.shutdown_all());
                applog::emit(
                    log::Level::Info,
                    "app",
                    ev::APP_SHUTDOWN_SESSIONS_CLOSED,
                    None,
                    "会话已全部关闭",
                    Some(serde_json::json!({
                        "count": closed,
                        "elapsed_ms": shutdown_instant.elapsed().as_millis() as u64,
                    })),
                );

                applog::emit(
                    log::Level::Info,
                    "app",
                    ev::APP_SHUTDOWN_LOG_FLUSHED,
                    None,
                    "日志缓冲区已落盘",
                    Some(serde_json::json!({
                        "pending": applog::try_hub().map(|h| h.pending_count()).unwrap_or(0),
                    })),
                );

                applog::emit(
                    log::Level::Info,
                    "app",
                    ev::APP_EXIT,
                    None,
                    "Rhost 退出",
                    Some(serde_json::json!({
                        "uptime_s": BOOT_INSTANT.get().map(|t| t.elapsed().as_secs()).unwrap_or(0),
                        "active_sessions": 0,
                    })),
                );

                if let Some(hub) = applog::try_hub() {
                    hub.shutdown();
                }
            }
        });
}
